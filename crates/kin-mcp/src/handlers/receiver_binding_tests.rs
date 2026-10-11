// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! A reference row bound through a typed receiver carries the chain that
//! proved it, and a chain the linker could not finish names its missing link.
//!
//! The store is built the way a conversion builds it: each file parsed by its
//! real adapter, admitted at its path, linked by the batch linker with its
//! coverage certificate, and persisted.

use std::collections::HashMap;

use kin_db::InMemoryGraph;
use kin_index::receiver_origin::OriginStepKind;
use kin_index::FileParseData;
use kin_model::graph::EntityStore;
use kin_model::{
    ArtifactId, Entity, EntityKind, FilePathId, LocatedEntry, ParseCompleteness, RepoPath,
    TransactionDelta, TreeDelta, TreeEntry,
};

use super::entities::handle_find_references;
use crate::types::ContentBlock;

const STORE: &str = "\
export type Config = {name: string};

export default class Store {
  containsElement(id: number): boolean {
    return id > 0;
  }
}
";

const OTHER_STORE: &str = "\
export class Store {
  containsElement(id: number): boolean {
    return false;
  }
}
";

const CONTEXT: &str = "\
import {createContext} from 'react';
import Store from '../store';

export const StoreContext = createContext<Store>(null as any as Store);
export const LooseContext = createContext(null);
";

const PANEL: &str = "\
import {useContext} from 'react';
import {StoreContext, LooseContext} from './context';

export function Panel({id}: {id: number}) {
  const store = useContext(StoreContext);
  return store.containsElement(id);
}

export function Loose({id}: {id: number}) {
  const store = useContext(LooseContext);
  return store.containsElement(id);
}
";

const FILES: [(&str, &str); 4] = [
    ("src/devtools/store.ts", STORE),
    ("src/other/Store.ts", OTHER_STORE),
    ("src/devtools/views/context.ts", CONTEXT),
    ("src/devtools/views/Panel.tsx", PANEL),
];

/// Parse, link and persist `FILES`, and admit each at its path when `tree`
/// is set. The certificate lookup reads a file's artifact through the tree.
/// A reference answer reads its callers' bodies through a pinned repository
/// authority these fixtures do not build, so without `tree` the entities are
/// stored without spans, as the other reference-row fixtures store them, and
/// the answer serves every step it can address by id.
fn admitted(tree: bool) -> (InMemoryGraph, Vec<FileParseData>) {
    admitted_sources(&FILES, tree)
}

fn admitted_sources(sources: &[(&str, &str)], tree: bool) -> (InMemoryGraph, Vec<FileParseData>) {
    let graph = InMemoryGraph::new();
    let mut files = Vec::new();
    let mut ids = HashMap::new();
    for &(path, source) in sources {
        let bytes = source.as_bytes();
        let blob = kin_blobs::digest(bytes);
        let indexed = kin_index::IndexPipeline::new()
            .index_file_content_with_tests(&FilePathId::new(path), bytes, blob)
            .unwrap()
            .indexed_file;
        let artifact = ArtifactId::new();
        if tree {
            graph
                .apply_transaction_delta(&TransactionDelta {
                    tree_deltas: vec![TreeDelta::Added {
                        artifact_id: artifact,
                        new: LocatedEntry::new(
                            RepoPath::from_utf8(path).unwrap(),
                            TreeEntry::blob(kin_model::Hash256::from_bytes(blob.0), false),
                        ),
                    }],
                    ..Default::default()
                })
                .unwrap();
        }
        for entity in &indexed.entities {
            let mut stored = entity.clone();
            if !tree {
                stored.span = None;
            }
            graph.upsert_entity(&stored).unwrap();
        }
        ids.insert(path.to_string(), artifact);
        files.push(FileParseData {
            file_path: path.to_string(),
            entities: indexed.entities,
            relations: indexed.extracted_relations,
            imports: indexed.imports,
        });
    }
    let completeness = files
        .iter()
        .map(|file| (file.file_path.clone(), ParseCompleteness::Full))
        .collect();
    for relation in
        kin_index::link_cross_file_with_completeness(&files, &ids, &completeness).unwrap()
    {
        // An artifact's own relations (its certificate, its imports) need the
        // artifact admitted.
        if tree || (relation.src.as_entity().is_some() && relation.dst.as_entity().is_some()) {
            graph.upsert_relation(&relation).unwrap();
        }
    }
    (graph, files)
}

fn entity<'a>(files: &'a [FileParseData], file: &str, kind: EntityKind, name: &str) -> &'a Entity {
    files
        .iter()
        .find(|parsed| parsed.file_path == file)
        .and_then(|parsed| {
            parsed
                .entities
                .iter()
                .find(|entity| entity.kind == kind && entity.name == name)
        })
        .unwrap_or_else(|| panic!("no {kind:?} {name} in {file}"))
}

#[tokio::test]
async fn a_row_bound_through_a_context_serves_the_chain_that_proved_it() {
    let (graph, files) = admitted(false);
    let focal = entity(
        &files,
        "src/devtools/store.ts",
        EntityKind::Method,
        "Store.containsElement",
    );
    let panel = entity(
        &files,
        "src/devtools/views/Panel.tsx",
        EntityKind::Function,
        "Panel",
    );
    let context = entity(
        &files,
        "src/devtools/views/context.ts",
        EntityKind::Constant,
        "StoreContext",
    );
    let store = entity(&files, "src/devtools/store.ts", EntityKind::Class, "Store");
    let args = HashMap::from([(
        "entity_id".to_string(),
        serde_json::json!(focal.id.to_string()),
    )]);
    let result = handle_find_references(&args, &graph, None).await.unwrap();
    let ContentBlock::Text { text } = result.content.first().expect("a content block");
    let reply: serde_json::Value = serde_json::from_str(text).unwrap();
    let rows = reply["references"].as_array().expect("reference rows");
    let row = rows
        .iter()
        .find(|row| row["entity_id"] == panel.id.to_string())
        .unwrap_or_else(|| panic!("Panel is a counted caller: {reply:#}"));
    assert_eq!(row["resolution"], "type_resolved", "{row:#}");
    let steps = row[crate::receiver_binding::RECEIVER_BINDING_KEY]
        .as_array()
        .unwrap_or_else(|| panic!("the row carries its receiver chain: {row:#}"));
    let kinds: Vec<&str> = steps
        .iter()
        .map(|step| step["step"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["call", "binding", "provider", "type"], "{row:#}");
    // The call and the binding are inside the row's own caller: addressed by
    // their line there, never by the caller's id again.
    assert!(steps[0].get("entity_id").is_none(), "{:#}", steps[0]);
    assert_eq!(steps[1]["name"], "store");
    assert!(steps[1].get("entity_id").is_none(), "{:#}", steps[1]);
    assert_eq!(steps[1]["line_in_entity"], 1, "the binding's line in Panel");
    assert_eq!(steps[2]["name"], "StoreContext");
    assert_eq!(steps[2]["kind"], "react_context");
    assert_eq!(steps[2]["entity_id"], context.id.to_string());
    assert!(
        steps[2]["code"]
            .as_str()
            .is_some_and(|code| code.contains("StoreContext")),
        "{:#}",
        steps[2]
    );
    assert_eq!(steps[3]["resolves_to"], "Store");
    assert_eq!(steps[3]["via"], "declared");
    assert_eq!(steps[3]["entity_id"], store.id.to_string());
    assert!(steps[3].get("code").is_none(), "{:#}", steps[3]);
    // The caller the chain could not prove keeps no such field.
    let loose = entity(
        &files,
        "src/devtools/views/Panel.tsx",
        EntityKind::Function,
        "Loose",
    );
    assert!(rows
        .iter()
        .filter(|row| row["entity_id"] == loose.id.to_string())
        .all(|row| row
            .get(crate::receiver_binding::RECEIVER_BINDING_KEY)
            .is_none()));
}

#[tokio::test]
async fn plain_refs_alias_metadata_distinguishes_same_named_python_functions() {
    let sources = [
        ("rate_engine.py", "def normalize_rate(rate):\n    return rate\n"),
        ("other_engine.py", "def normalize_rate(rate):\n    return str(rate)\n"),
        ("publisher.py", "import rate_engine\nfrom rate_engine import normalize_rate as normalize\nfrom other_engine import normalize_rate as format_rate\n\ndef build_manifest(rate):\n    return normalize(rate)\n\ndef audit_manifest(rate):\n    return rate_engine.normalize_rate(rate)\n\ndef pretty_manifest(rate):\n    return format_rate(rate)\n"),
    ];
    let (graph, files) = admitted_sources(&sources, true);
    // Retain real parser spans and read their bodies through pinned repository
    // authority, just as runtime dispatch does, rather than a working file.
    let directory = tempfile::tempdir().unwrap();
    let repository_id = kin_model::RepositoryId::new("alias-identity").unwrap();
    let workspace_id = kin_model::WorkspaceId::new();
    let backend = std::sync::Arc::new(kin_db::LocalFileBackend::new(directory.path()));
    let manager = std::sync::Arc::new(
        kin_db::RepositoryAuthorityManager::open(repository_id.clone(), backend.clone()).unwrap(),
    );
    for (_, source) in &sources {
        manager
            .save_source_blob(
                kin_model::Hash256::from_bytes(kin_blobs::digest_bytes(source.as_bytes())),
                source.as_bytes(),
            )
            .unwrap();
    }
    let snapshot = graph.to_snapshot();
    let policy = kin_model::SharedAdmissionPolicy::empty(0);
    let mut change = kin_model::SemanticChange {
        id: kin_model::SemanticChangeId::from_hash(kin_model::Hash256::from_bytes([0; 32])),
        origin: kin_model::ChangeOrigin::Native,
        parents: Vec::new(),
        timestamp: kin_model::Timestamp::now(),
        author: kin_model::AuthorId::new("alias-identity-test"),
        message: "admit same-named Python functions and their distinct imports".into(),
        entity_deltas: snapshot
            .entities
            .into_values()
            .map(|new| kin_model::EntityDelta::Added { new })
            .collect(),
        relation_deltas: snapshot
            .relations
            .into_values()
            .map(|new| kin_model::RelationDelta::Added { new })
            .collect(),
        tree_deltas: snapshot
            .resolved_tree
            .artifacts()
            .map(|artifact| TreeDelta::Added {
                artifact_id: artifact.artifact_id,
                new: LocatedEntry::new(artifact.path.clone(), artifact.entry.clone()),
            })
            .collect(),
        admission_policy_delta: Some(kin_model::AdmissionPolicyDelta::initialize(policy.clone())),
        projected_files: Vec::new(),
        spec_link: None,
        evidence: Vec::new(),
        risk_summary: None,
        external_reference_deltas: Vec::new(),
        resolution_record_deltas: Vec::new(),
    };
    change.id = kin_model::compute_semantic_change_id(&change).unwrap();
    kin_core::initialize_repository_authority(
        &manager,
        repository_id.clone(),
        workspace_id,
        kin_model::AdmissionCase::Sensitive,
        kin_model::RefName::branch(b"main").unwrap(),
        policy,
        Some(change),
    )
    .unwrap();
    let binding = kin_core::LocalRepositoryAuthorityBinding::from_parts(
        repository_id.clone(),
        workspace_id,
        backend,
    );
    let authority = super::repository_authority::RequestRepositoryAuthority::already_open(
        binding,
        std::sync::Arc::new(
            super::repository_authority::ActiveRepositoryAuthority::from_shared(
                manager,
                repository_id,
                workspace_id,
            ),
        ),
    );
    for (file, alias, callers) in [
        (
            "rate_engine.py",
            "normalize",
            vec!["audit_manifest", "build_manifest"],
        ),
        ("other_engine.py", "format_rate", vec!["pretty_manifest"]),
    ] {
        let focal = entity(&files, file, EntityKind::Function, "normalize_rate");
        let args = HashMap::from([
            ("entity_id".into(), serde_json::json!(focal.id.to_string())),
            ("relation_kinds".into(), serde_json::json!(["calls"])),
        ]);
        let result = handle_find_references(&args, &graph, Some(&authority))
            .await
            .unwrap();
        // Agent-profile runtime finalizes the whole evidence and verdict before
        // projecting its default answer-only payload for plain presentation.
        let result = crate::envelope::finalize_bounded(
            result,
            crate::envelope::Envelope::offline(),
            "find_references",
            &crate::budget::ResponseBudget {
                answer_only: true,
                ..Default::default()
            },
        );
        let ContentBlock::Text { text } = &result.content[0];
        let mut presented = serde_json::json!({"content": [{"type": "text", "text": text}]});
        crate::plain_reply::present(&mut presented, "find_references");
        let plain: serde_json::Value =
            serde_json::from_str(presented["content"][0]["text"].as_str().unwrap()).unwrap();
        let sites = plain["other_names"]["sites"]
            .as_array()
            .unwrap_or_else(|| panic!("machine: {text}\nplain: {plain:#}"));
        assert_eq!(sites.len(), 1, "{plain:#}");
        assert!(sites[0].get("file").is_none(), "{plain:#}");
        assert!(sites[0].get("line").is_none(), "{plain:#}");
        assert_eq!(sites[0]["code"], alias, "{plain:#}");
        assert_eq!(plain["entity"]["entity_id"], focal.id.to_string());
        assert_eq!(plain["entity"]["name"], "normalize_rate");
        for row in plain["references"].as_array().unwrap() {
            let expected = entity(
                &files,
                "publisher.py",
                EntityKind::Function,
                row["name"].as_str().unwrap(),
            );
            assert_eq!(row["entity_id"], expected.id.to_string(), "{plain:#}");
        }
        let mut found: Vec<_> = plain["references"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["name"].as_str().unwrap())
            .collect();
        found.sort();
        assert_eq!(found, callers, "{plain:#}");
        let note = plain["other_names"]["note"].as_str().unwrap();
        assert!(note.contains("coverage verdict"), "{plain:#}");
        assert!(!note.contains("included in references"), "{plain:#}");
        assert!(!note.contains("nothing here is left to check"), "{plain:#}");
    }
}

#[tokio::test]
async fn plain_refs_keeps_the_persisted_factory_return_and_receiver_type() {
    let sources = [(
        "src/context.py",
        "class Context:\n    def pop(self):\n        pass\n\nclass Factory:\n    def make(self) -> Context:\n        return Context()\n",
    ), (
        "src/worker.py",
        "from .context import Factory\n\ndef run(factory: Factory):\n    ctx = factory.make()\n    ctx.pop()\n",
    )];
    let (graph, files) = admitted_sources(&sources, false);
    let focal = entity(&files, "src/context.py", EntityKind::Method, "Context.pop");
    let caller = entity(&files, "src/worker.py", EntityKind::Function, "run");
    let factory = entity(&files, "src/context.py", EntityKind::Method, "Factory.make");
    let context = entity(&files, "src/context.py", EntityKind::Class, "Context");
    let args = HashMap::from([(
        "entity_id".to_string(),
        serde_json::json!(focal.id.to_string()),
    )]);
    let result = handle_find_references(&args, &graph, None).await.unwrap();
    let ContentBlock::Text { text } = &result.content[0];
    let machine: serde_json::Value = serde_json::from_str(text).unwrap();
    let machine_row = machine["references"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["entity_id"] == caller.id.to_string())
        .unwrap_or_else(|| panic!("Factory.run is a caller: {machine:#}"));
    let key = crate::receiver_binding::RECEIVER_BINDING_KEY;
    let steps = machine_row[key]
        .as_array()
        .unwrap_or_else(|| panic!("the persisted edge has a receiver chain: {machine_row:#}"));
    assert!(
        steps.iter().any(|step| {
            step["step"] == "returns"
                && step["entity_id"] == factory.id.to_string()
                && step["code"] == "def make(self) -> Context"
        }),
        "{steps:#?}"
    );
    assert!(
        steps.iter().any(|step| {
            step["step"] == "type"
                && step["resolves_to"] == "Context"
                && step["entity_id"] == context.id.to_string()
                && step["via"] == "returned"
        }),
        "{steps:#?}"
    );
    let mut presented = serde_json::json!({"content": [{"type": "text", "text": text}]});
    crate::plain_reply::present(&mut presented, "find_references");
    let plain: serde_json::Value =
        serde_json::from_str(presented["content"][0]["text"].as_str().unwrap()).unwrap();
    let plain_row = plain["references"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["entity_id"] == caller.id.to_string())
        .unwrap_or_else(|| panic!("plain refs retains the caller: {plain:#}"));
    assert!(
        plain_row.get(key).is_none(),
        "default refs stays compact: {plain_row:#}"
    );
    assert_eq!(plain_row["receiver_binding_available"], true);
    let mut selected_args = args.clone();
    selected_args.insert(
        "proof_for_caller".into(),
        serde_json::json!(caller.id.to_string()),
    );
    let selected = handle_find_references(&selected_args, &graph, None)
        .await
        .unwrap();
    let ContentBlock::Text { text } = &selected.content[0];
    let mut presented = serde_json::json!({"content": [{"type": "text", "text": text}]});
    crate::plain_reply::present(&mut presented, "find_references");
    let selected: serde_json::Value =
        serde_json::from_str(presented["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(
        selected["receiver_proof"][key], machine_row[key],
        "{selected:#}"
    );
    assert_eq!(selected["receiver_proof"]["status"], "recorded");
    assert!(selected.get("references").is_none());
    assert!(plain_row
        .get(crate::receiver_binding::RECEIVER_BINDING_MISSING_KEY)
        .is_none());
}

#[tokio::test]
async fn selected_receiver_proof_without_a_chain_reports_not_recorded() {
    let (graph, files) = admitted(false);
    let focal = entity(
        &files,
        "src/devtools/store.ts",
        EntityKind::Method,
        "Store.containsElement",
    );
    let caller = entity(
        &files,
        "src/devtools/views/Panel.tsx",
        EntityKind::Function,
        "Panel",
    );
    for mut relation in graph.get_all_relations_for_entity(&caller.id).unwrap() {
        if relation.dst == kin_model::GraphNodeId::Entity(focal.id) {
            relation.evidence.retain(|record| {
                record.parser_rule.as_deref()
                    != Some(kin_index::receiver_origin::RECEIVER_ORIGIN_STEPS_RULE)
            });
            graph.upsert_relation(&relation).unwrap();
        }
    }
    let args = HashMap::from([
        ("entity_id".into(), serde_json::json!(focal.id.to_string())),
        (
            "proof_for_caller".into(),
            serde_json::json!(caller.id.to_string()),
        ),
    ]);
    let result = handle_find_references(&args, &graph, None).await.unwrap();
    let ContentBlock::Text { text } = &result.content[0];
    let reply: serde_json::Value = serde_json::from_str(text).unwrap();
    assert_eq!(
        reply["receiver_proof"]["status"], "not_recorded",
        "{reply:#}"
    );
    assert_eq!(reply["receiver_proof"]["resolution"], "type_resolved");
    assert!(reply["receiver_proof"]
        .get(crate::receiver_binding::RECEIVER_BINDING_KEY)
        .is_none());
}

#[tokio::test]
async fn selected_receiver_proof_refuses_bad_ids_and_cursor_combinations() {
    let (graph, files) = admitted(false);
    let focal = entity(
        &files,
        "src/devtools/store.ts",
        EntityKind::Method,
        "Store.containsElement",
    );
    let caller = entity(
        &files,
        "src/devtools/views/Panel.tsx",
        EntityKind::Function,
        "Panel",
    );
    let base = HashMap::from([
        (
            "entity_id".to_string(),
            serde_json::json!(focal.id.to_string()),
        ),
        (
            "proof_for_caller".to_string(),
            serde_json::json!(caller.id.to_string()),
        ),
    ]);
    for (key, value) in [
        ("proof_for_caller", serde_json::json!("not-a-uuid")),
        (
            "proof_for_caller",
            serde_json::json!(kin_model::EntityId::new().to_string()),
        ),
        ("entity_id", serde_json::json!("Store.containsElement")),
        ("cursor", serde_json::json!("a-prior-page")),
    ] {
        let mut args = base.clone();
        args.insert(key.into(), value);
        assert!(
            handle_find_references(&args, &graph, None).await.is_err(),
            "{args:?}"
        );
    }
    let mut args = base;
    args.remove("entity_id");
    args.insert("query".into(), serde_json::json!("Store.containsElement"));
    assert!(handle_find_references(&args, &graph, None).await.is_err());
}

#[test]
fn a_chain_the_linker_could_not_finish_names_its_missing_link() {
    let (graph, files) = admitted(true);
    let loose = entity(
        &files,
        "src/devtools/views/Panel.tsx",
        EntityKind::Function,
        "Loose",
    );
    let (steps, missing) =
        crate::receiver_binding::chain_gap(&graph, loose, "store", "containsElement")
            .expect("the certificate records the stopped chain");
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].step, OriginStepKind::Binding);
    assert_eq!(steps[0].entity, Some(loose.id));
    assert_eq!(missing.step, OriginStepKind::Provider);
    assert_eq!(missing.name, "LooseContext");
    assert_eq!(missing.reason, "provider_value_unknown");

    let served = crate::receiver_binding::served_steps(
        &graph,
        loose,
        Some(&PANEL[PANEL.find("export function Loose").unwrap()..]),
        crate::receiver_binding::CallAt {
            line_in_entity: Some(2),
            code: Some("store.containsElement(id)"),
        },
        &steps,
    );
    assert_eq!(served.len(), 2);
    assert_eq!(
        served[0],
        serde_json::json!({"step": "call", "line_in_entity": 2})
    );
    assert_eq!(served[1]["step"], "binding");
    assert_eq!(
        served[1]["code"], "const store = useContext(LooseContext);",
        "the binding is quoted from the caller's own body"
    );
    let missing = crate::receiver_binding::missing_json(&missing);
    assert_eq!(missing["step"], "provider");
    assert_eq!(missing["kind"], "react_context");

    // A receiver no fact binds lacks its binding, whatever the certificate
    // holds.
    let (steps, missing) =
        crate::receiver_binding::chain_gap(&graph, loose, "registry", "containsElement").unwrap();
    assert!(steps.is_empty());
    assert_eq!(missing.step, OriginStepKind::Binding);
    assert_eq!(missing.reason, "no_value_origin_fact");

    // A bound call records no gap.
    let panel = entity(
        &files,
        "src/devtools/views/Panel.tsx",
        EntityKind::Function,
        "Panel",
    );
    assert!(
        crate::receiver_binding::chain_gap(&graph, panel, "store", "containsElement").is_none()
    );
}

#[test]
fn a_list_quotes_each_declaration_once_and_within_its_budget() {
    let step = |id: &str, code: &str| serde_json::json!({"step": "provider", "entity_id": id, "code": code});
    let chain = |steps: Vec<serde_json::Value>| serde_json::json!({ crate::receiver_binding::RECEIVER_BINDING_KEY: steps });
    let mut rows = vec![
        chain(vec![step("a", "StoreContext = createContext<Store>(null)")]),
        chain(vec![step("a", "StoreContext = createContext<Store>(null)")]),
        chain(vec![step("b", &"x".repeat(80))]),
    ];
    let cut = crate::receiver_binding::cap_step_code(&mut rows, 60);
    assert_eq!(
        cut,
        crate::receiver_binding::CodeCut {
            quoted: 1,
            repeated: 1,
            elided: 1
        }
    );
    let key = crate::receiver_binding::RECEIVER_BINDING_KEY;
    assert!(rows[0][key][0].get("code").is_some());
    assert_eq!(
        rows[1][key][0],
        serde_json::json!({"step": "provider", "entity_id": "a"}),
        "described once per list, and named by its id after that"
    );
    assert_eq!(
        rows[2][key][0],
        serde_json::json!({"step": "provider", "entity_id": "b"}),
        "past the budget a step keeps its address and drops its code"
    );
    let disclosure = cut.disclosure(60).expect("a cut is disclosed");
    assert_eq!(disclosure["elided"], 1);
}
