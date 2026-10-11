// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Exact graph coordinates for adapter file-module surfaces. This is neither
//! filename ranking nor an inventory of every entity sharing a file.

use kin_model::entity::EntityKind;
use kin_model::graph::{EntityFilter, EntityPage, GraphStore};
use kin_model::{FilePathId, RepoPath};
use serde_json::{json, Value};

use crate::error::{McpError, Result};
use crate::handlers::common::{CompactSearchResult, SemanticSearchResult};
use crate::types::{ContentBlock, ToolCallResult};

pub const KEY: &str = "coordinate_lookup";
const SCOPE: &str = "exact_graph_file_module_surfaces";
const MAX_COORDINATE_CANDIDATES: usize = 256;

/// A whole path token or an explicit path prefix, not descriptive text that
/// merely mentions a path. A bare dotted name retains its name convention;
/// `./file.ext` pins a root coordinate.
/// Explicit description/literal modes remain authoritative at the verb router.
pub fn looks_like_coordinate(query: &str) -> bool {
    let query = query.trim();
    let separated = query.contains('/') || query.contains('\\');
    if !separated {
        return false;
    }
    if query.starts_with("./")
        || query.starts_with("../")
        || query.starts_with('/')
        || query.starts_with('\\')
        || query.starts_with("~/")
    {
        return true;
    }
    if query.chars().any(char::is_whitespace) {
        return false;
    }
    if query.chars().any(char::is_control) {
        return true;
    }
    if query.split(['/', '\\']).any(|part| {
        part.eq_ignore_ascii_case(".git")
            || part.eq_ignore_ascii_case(".kin")
            || part.eq_ignore_ascii_case(".kin-session")
    }) {
        return true;
    }
    let leaf = query.rsplit(['/', '\\']).next().unwrap_or_default();
    leaf.contains('.') || leaf.is_empty() || query.contains("://") || windows_drive(query)
}

fn windows_drive(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'/' | b'\\')
}

fn coordinate(raw: &str) -> Result<(String, RepoPath)> {
    if raw.chars().any(char::is_control)
        || raw.contains(':')
        || raw.chars().any(|ch| matches!(ch, '*' | '?' | '[' | ']'))
        || windows_drive(raw.trim())
    {
        return Err(McpError::InvalidParams(
            "file coordinate must be one repository-relative source path, without control characters, a URL, drive prefix, line suffix or wildcard".into(),
        ));
    }
    let swapped = raw.trim().replace('\\', "/");
    let mut path = swapped.as_str();
    while let Some(rest) = path.strip_prefix("./") {
        path = rest;
    }
    if path.is_empty() || path.ends_with('/') || path.starts_with("~/") {
        return Err(McpError::InvalidParams(
            "file coordinate must name one repository-relative source path, not a directory or home path".into(),
        ));
    }
    let repo_path = RepoPath::from_utf8(path.to_string()).map_err(|error| {
        McpError::InvalidParams(format!("invalid repository file coordinate: {error}"))
    })?;
    kin_core::validate_source_paths([&repo_path]).map_err(|error| {
        McpError::InvalidParams(format!("inadmissible repository file coordinate: {error}"))
    })?;
    Ok((path.to_string(), repo_path))
}

/// Recognize only this handler's checked machine shape. An arbitrary field or
/// malformed metadata cannot suppress declaration, package or absence gates.
pub fn recognizes_payload(payload: &Value) -> bool {
    let Some(block) = payload.get(KEY) else {
        return false;
    };
    let (Some(query), Some(file), Some(total), Some(rows)) = (
        payload.get("query").and_then(Value::as_str),
        block.get("file").and_then(Value::as_str),
        payload.get("total_matches").and_then(Value::as_u64),
        payload.get("results").and_then(Value::as_array),
    ) else {
        return false;
    };
    let held = block.get("recorded_surfaces").and_then(Value::as_u64);
    let tracked = block.get("tracked_in_graph").and_then(Value::as_bool);
    let state_valid = match (block.get("state").and_then(Value::as_str), held, tracked) {
        (Some("matched"), Some(held), Some(_)) => total > 0 && held >= total,
        (Some("filtered_out"), Some(held), Some(_)) => total == 0 && held > 0,
        (Some("module_unrecorded"), Some(0), Some(true)) => total == 0,
        (Some("coordinate_unrecorded"), Some(0), Some(false)) => total == 0,
        _ => false,
    };
    block.get("version").and_then(Value::as_u64) == Some(1)
        && looks_like_coordinate(query)
        && block.get("scope").and_then(Value::as_str) == Some(SCOPE)
        && block.get("matching_surfaces").and_then(Value::as_u64) == Some(total)
        && state_valid
        && total >= rows.len() as u64
        && coordinate(query).is_ok_and(|(normalized, _)| normalized == file)
        && rows.iter().all(|row| {
            row.get("file_path").and_then(Value::as_str) == Some(file)
                && row.get("id").is_some_and(Value::is_string)
                && matches!(
                    row.get("kind").and_then(Value::as_str),
                    Some("module" | "package" | "file")
                )
        })
}

pub(crate) fn recognizes(tool: &str, payload: &Value) -> bool {
    tool == "semantic_search" && recognizes_payload(payload)
}

pub fn recognizes_result(result: &ToolCallResult) -> bool {
    !result.is_error.unwrap_or(false)
        && result.content.len() == 1
        && result.content.iter().all(|content| {
            let ContentBlock::Text { text } = content;
            serde_json::from_str::<Value>(text).is_ok_and(|payload| recognizes_payload(&payload))
        })
}

pub(crate) fn lookup<G: GraphStore>(
    store: &G,
    query: &str,
    limit: usize,
    filter: &EntityFilter,
    compact: bool,
) -> Result<Option<Value>> {
    if !looks_like_coordinate(query) {
        return Ok(None);
    }
    let (file, repo_path) = coordinate(query)?;
    let page = store
        .query_entities_page(
            &EntityFilter {
                file_path: Some(FilePathId::new(&file)),
                kinds: Some(vec![
                    EntityKind::Module,
                    EntityKind::Package,
                    EntityKind::File,
                ]),
                ..Default::default()
            },
            &EntityPage::first(MAX_COORDINATE_CANDIDATES),
        )
        .map_err(McpError::graph)?;
    if page.total_matching > MAX_COORDINATE_CANDIDATES || page.next_offset.is_some() {
        return Err(McpError::Other("file-coordinate lookup exceeded its candidate bound; no complete coordinate match set is established".into()));
    }
    let mut surfaces: Vec<_> = page
        .entities
        .into_iter()
        .filter(|entity| {
            entity
                .file_origin
                .as_ref()
                .is_some_and(|origin| origin.0 == file)
                && kin_model::is_file_module_surface(entity)
        })
        .collect();
    // Never let index order choose a file container among duplicate records.
    surfaces.sort_by_key(|entity| (entity.name.clone(), entity.id.0));
    let recorded = surfaces.len();
    let surfaces: Vec<_> = surfaces
        .into_iter()
        .filter(|entity| {
            filter
                .kinds
                .as_ref()
                .is_none_or(|kinds| kinds.contains(&entity.kind))
                && filter
                    .languages
                    .as_ref()
                    .is_none_or(|languages| languages.contains(&entity.language))
                && filter
                    .roles
                    .as_ref()
                    .is_none_or(|roles| roles.contains(&entity.role))
        })
        .collect();
    let total = surfaces.len();
    let rows: Vec<Value> = surfaces
        .into_iter()
        .take(limit)
        .map(|entity| {
            if compact {
                serde_json::to_value(CompactSearchResult::from(entity))
            } else {
                serde_json::to_value(SemanticSearchResult::from(entity))
            }
        })
        .collect::<std::result::Result<_, _>>()
        .map_err(McpError::Json)?;
    let tracked = store.artifact_id_at_path(&repo_path).is_some();
    let state = if total > 0 {
        "matched"
    } else if recorded > 0 {
        "filtered_out"
    } else if tracked {
        "module_unrecorded"
    } else {
        "coordinate_unrecorded"
    };
    let mut block = json!({"version":1,"scope":SCOPE,"file":file,"state":state,
        "tracked_in_graph":tracked,"recorded_surfaces":recorded,"matching_surfaces":total});
    if filter.kinds.is_some() || filter.languages.is_some() || filter.roles.is_some() {
        block["filters"] =
            json!({"kinds":filter.kinds,"languages":filter.languages,"roles":filter.roles});
    }
    let mut payload = json!({"query":query,"limit":limit,"total_matches":total,
        "truncated":total > rows.len(),"results":rows});
    payload[KEY] = block;
    Ok(Some(payload))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kin_db::InMemoryGraph;
    use kin_model::graph::EntityStore;
    use kin_model::{
        ArtifactId, Entity, EntityId, EntityMetadata, EntityRole, FingerprintAlgorithm, Hash256,
        LanguageId, LocatedEntry, SemanticFingerprint, TransactionDelta, TreeDelta, TreeEntry,
        Visibility,
    };
    use std::collections::HashMap;

    fn surface(name: &str, file: &str) -> Entity {
        Entity {
            id: EntityId::new(),
            kind: EntityKind::Module,
            name: name.into(),
            language: LanguageId::Python,
            fingerprint: SemanticFingerprint {
                algorithm: FingerprintAlgorithm::V1TreeSitter,
                ast_hash: Hash256::from_bytes([0; 32]),
                signature_hash: Hash256::from_bytes([0; 32]),
                behavior_hash: Hash256::from_bytes([0; 32]),
                equivalence_hash: Hash256::from_bytes([0; 32]),
                stability_score: 1.0,
            },
            file_origin: Some(FilePathId::new(file)),
            span: None,
            signature: format!("module {file}"),
            visibility: Visibility::Public,
            role: EntityRole::Source,
            doc_summary: None,
            metadata: EntityMetadata::default(),
            lineage_parent: None,
            created_in: None,
            superseded_by: None,
        }
    }

    fn lookup_at(store: &InMemoryGraph, query: &str, limit: usize) -> Value {
        lookup(store, query, limit, &EntityFilter::default(), true)
            .unwrap()
            .expect("coordinate")
    }

    fn payload(result: &ToolCallResult) -> Value {
        let ContentBlock::Text { text } = &result.content[0];
        serde_json::from_str(text).unwrap()
    }

    fn ready() -> crate::envelope::Envelope {
        crate::envelope::Envelope::daemon().with_health(&json!({
            "graph_loaded":true,"initialized":true,"graph_entity_count":3,"graph_generation":1}))
    }

    #[test]
    fn exact_coordinate_never_selects_a_nested_module_or_same_basename_elsewhere() {
        let store = InMemoryGraph::new();
        let wanted = surface("engine", "one/engine.py");
        let other = surface("engine", "two/engine.py");
        let mut nested = surface("inner", "one/engine.py");
        nested.signature = "mod inner".into();
        let mut function = surface("engine", "one/engine.py");
        function.kind = EntityKind::Function;
        for entity in [&wanted, &other, &nested, &function] {
            store.upsert_entity(entity).unwrap();
        }
        let answer = lookup_at(&store, "one/engine.py", 20);
        assert_eq!(answer["total_matches"], 1);
        assert_eq!(answer["results"][0]["id"], json!(wanted.id));
        assert_eq!(answer[KEY]["recorded_surfaces"], 1);
        assert!(recognizes_payload(&answer));
        let miss = lookup_at(&store, "./engine.py", 20);
        assert_eq!(miss["results"], json!([]));
        assert_eq!(miss[KEY]["state"], "coordinate_unrecorded");
        assert!(
            lookup(&store, "engine.py", 20, &EntityFilter::default(), true)
                .unwrap()
                .is_none(),
            "a bare dotted name must retain name interpretation"
        );
    }

    #[test]
    fn coordinate_normalization_preserves_case_and_never_guesses_stems() {
        let store = InMemoryGraph::new();
        let wanted = surface("Engine", "src/Engine.py");
        store.upsert_entity(&wanted).unwrap();
        for query in ["./src/Engine.py", "src\\Engine.py", " ./src/Engine.py "] {
            let answer = lookup_at(&store, query, 20);
            assert_eq!(answer["results"][0]["id"], json!(wanted.id));
            assert_eq!(answer[KEY]["file"], "src/Engine.py");
            assert!(recognizes_payload(&answer));
        }
        for query in ["src/engine.py", "./Engine.py", "other/Engine.py"] {
            assert_eq!(
                lookup_at(&store, query, 20)["results"],
                json!([]),
                "{query}"
            );
        }
    }

    #[test]
    fn duplicate_file_surfaces_and_filters_are_explicit_without_first_match_preference() {
        let store = InMemoryGraph::new();
        let first = surface("first", "src/main.py");
        let second = surface("second", "src/main.py");
        for entity in [&second, &first] {
            store.upsert_entity(entity).unwrap();
        }
        let all = lookup_at(&store, "src/main.py", 20);
        assert_eq!(all["total_matches"], 2);
        assert_eq!(all["results"][0]["id"], json!(first.id));
        assert_eq!(all["results"][1]["id"], json!(second.id));
        let limited = lookup_at(&store, "src/main.py", 1);
        assert_eq!(limited["total_matches"], 2);
        assert_eq!(limited["results"].as_array().unwrap().len(), 1);
        assert_eq!(limited["truncated"], true);
        assert!(recognizes_payload(&limited));
        let filter = EntityFilter {
            kinds: Some(vec![EntityKind::Function]),
            ..Default::default()
        };
        let filtered = lookup(&store, "src/main.py", 20, &filter, true)
            .unwrap()
            .unwrap();
        assert_eq!(filtered[KEY]["state"], "filtered_out");
        assert_eq!(filtered[KEY]["recorded_surfaces"], 2);
        assert_eq!(filtered["results"], json!([]));
        assert!(recognizes_payload(&filtered));
    }

    #[test]
    fn tracked_source_without_a_file_surface_stays_a_distinct_graph_gap() {
        let store = InMemoryGraph::new();
        store
            .apply_transaction_delta(&TransactionDelta {
                tree_deltas: vec![TreeDelta::Added {
                    artifact_id: ArtifactId::new(),
                    new: LocatedEntry::new(
                        RepoPath::from_utf8("src/unparsed.py".to_string()).unwrap(),
                        TreeEntry::blob(Hash256::from_bytes([7; 32]), false),
                    ),
                }],
                entity_deltas: Vec::new(),
                relation_deltas: Vec::new(),
                admission_policy_delta: None,
                external_reference_deltas: Vec::new(),
                resolution_record_deltas: Vec::new(),
            })
            .unwrap();
        let tracked = lookup_at(&store, "src/unparsed.py", 20);
        let unknown = lookup_at(&store, "src/unknown.py", 20);
        assert_eq!(tracked[KEY]["state"], "module_unrecorded");
        assert_eq!(tracked[KEY]["tracked_in_graph"], true);
        assert_eq!(unknown[KEY]["state"], "coordinate_unrecorded");
        assert_eq!(unknown[KEY]["tracked_in_graph"], false);
        for answer in [tracked, unknown] {
            assert!(recognizes_payload(&answer));
        }
    }

    #[test]
    fn unsupported_coordinate_attempts_are_typed_errors_and_natural_text_keeps_ranking() {
        let store = InMemoryGraph::new();
        for query in [
            "/tmp/main.py",
            "../main.py",
            "src/../main.py",
            "C:\\src\\main.py",
            "https://example.test/main.py",
            "src/*.py",
            "src/main.py:20",
            ".git/config",
            ".kin/objects",
            "src/main.py\0",
            "src/",
        ] {
            assert!(
                looks_like_coordinate(query),
                "unsupported coordinate must not rank: {query:?}"
            );
            assert!(
                matches!(
                    lookup(&store, query, 20, &EntityFilter::default(), true),
                    Err(McpError::InvalidParams(_))
                ),
                "{query:?}"
            );
        }
        for query in [
            "read/write",
            "where retries in src/main.py happen",
            "where retries in src/main.py\n happen",
            "where src/.git/config is mentioned",
            "app.run",
            "engine.py",
        ] {
            assert!(!looks_like_coordinate(query), "{query}");
        }
        let route = |query: &str, mode: Option<&str>| {
            let mut name = "locate".to_string();
            let mut args = HashMap::from([("query".into(), json!(query))]);
            if let Some(mode) = mode {
                args.insert("mode".into(), json!(mode));
            }
            crate::verbs::resolve(&mut name, &mut args).unwrap();
            name
        };
        assert_eq!(route("src/main.py", None), "semantic_search");
        assert_eq!(route("src/main.py", Some("description")), "semantic_locate");
        assert_eq!(
            route("src/main.py", Some("literal")),
            crate::handlers::lexical::TOOL_NAME
        );
        assert_eq!(
            route("where retries in src/main.py happen", None),
            "semantic_locate"
        );
    }

    #[test]
    fn coordinate_trust_remains_qualified_and_malformed_metadata_cannot_bypass_name_gates() {
        let store = InMemoryGraph::new();
        store
            .upsert_entity(&surface("main", "src/main.py"))
            .unwrap();
        for query in ["src/main.py", "src/missing.py"] {
            let answer = lookup_at(&store, query, 20);
            let coordinate_result = ToolCallResult::text(answer.to_string());
            assert!(recognizes_result(&coordinate_result));
            let mut mixed = coordinate_result.clone();
            mixed.content.extend(ToolCallResult::text("{}").content);
            assert!(
                !recognizes_result(&mixed),
                "mixed content is not a scoped coordinate result"
            );
            assert!(!crate::negative::answer_claims_absence(
                "semantic_search",
                &answer
            ));
            assert!(
                crate::negative::absence_coverage_clauses("semantic_search", &answer).is_empty()
            );
            for envelope in [ready(), crate::envelope::Envelope::offline()] {
                let result = crate::envelope::finalize(
                    ToolCallResult::text(answer.to_string()),
                    envelope,
                    "semantic_search",
                );
                let result = payload(&result);
                assert_eq!(result["negative"]["interpretation"], "qualified_answer");
                assert_eq!(result["negative"]["safe_to_conclude_absent"], false);
                assert!(!result["negative"]["subject"]
                    .as_str()
                    .unwrap()
                    .contains("returned rows"));
                assert!(result["negative"]["subject"]
                    .as_str()
                    .unwrap()
                    .contains("exact graph coordinate"));
                assert_eq!(result["_kin"]["verdict"]["absence_claim"], "not_applicable");
                assert!(result.get("edge_coverage").is_none());
            }
            let offline = crate::negative::negative_for(
                "semantic_search",
                &answer,
                &crate::envelope::Envelope::offline(),
                &[],
            )
            .unwrap();
            assert_eq!(offline["trust"], "inconclusive");
            let mut blocked = ready();
            blocked.degraded.mass_deletion_blocked = Some(true);
            let blocked =
                crate::negative::negative_for("semantic_search", &answer, &blocked, &[]).unwrap();
            assert_eq!(blocked["trust"], "inconclusive");
            assert!(blocked["trust_reason"]
                .as_str()
                .unwrap()
                .contains("degraded"));
            for field in ["degraded_signals", "bounding_signals"] {
                assert!(
                    blocked[field]
                        .as_array()
                        .unwrap()
                        .contains(&json!("mass_deletion_blocked")),
                    "the actual substrate blocker must remain disclosed: {field}"
                );
            }
            assert_eq!(blocked["safe_to_conclude_absent"], false);
            let mut blocked_envelope = ready();
            blocked_envelope.degraded.mass_deletion_blocked = Some(true);
            let finalized = crate::envelope::finalize(
                ToolCallResult::text(answer.to_string()),
                blocked_envelope,
                "semantic_search",
            );
            let finalized = payload(&finalized);
            assert_eq!(finalized["_kin"]["verdict"]["state"], "inconclusive");
            assert_eq!(
                finalized["_kin"]["verdict"]["safe_to_conclude_absent"],
                false
            );
            assert_eq!(finalized["_kin"]["degraded"]["mass_deletion_blocked"], true);
            let unavailable = crate::envelope::Envelope::daemon().with_health(&json!({
                "graph_loaded":false,"initialized":false,"graph_entity_count":3}));
            let unavailable =
                crate::negative::negative_for("semantic_search", &answer, &unavailable, &[])
                    .unwrap();
            assert_eq!(unavailable["trust"], "inconclusive");

            for field in [
                "version",
                "scope",
                "file",
                "state",
                "tracked_in_graph",
                "recorded_surfaces",
                "matching_surfaces",
            ] {
                let mut malformed = answer.clone();
                malformed[KEY].as_object_mut().unwrap().remove(field);
                assert!(!recognizes_payload(&malformed), "{field}");
                assert!(
                    !crate::negative::absence_coverage_clauses("semantic_search", &malformed)
                        .is_empty(),
                    "{field}"
                );
            }
            let mut declaration = answer.clone();
            declaration["query"] = json!("app.run");
            declaration[KEY]["file"] = json!("app.run");
            assert!(
                !recognizes_payload(&declaration),
                "metadata cannot relabel a dotted name"
            );
        }
    }

    #[test]
    fn coordinates_observe_tracked_nested_control_content_and_refuse_own_control_namespace() {
        let store = InMemoryGraph::new();
        for file in [
            "fixtures/legacy/.kin/manifest.json",
            "fixtures/legacy/.git/config",
            "fixtures/nested/.KIN/config.toml",
        ] {
            let repo_path = RepoPath::from_utf8(file.to_string()).unwrap();
            kin_core::validate_source_paths([&repo_path]).expect(
                "a nested control directory can be deliberately admitted as source content",
            );
            store
                .apply_transaction_delta(&TransactionDelta {
                    tree_deltas: vec![TreeDelta::Added {
                        artifact_id: ArtifactId::new(),
                        new: LocatedEntry::new(
                            repo_path,
                            TreeEntry::blob(Hash256::from_bytes([8; 32]), false),
                        ),
                    }],
                    ..Default::default()
                })
                .unwrap();
            let entity = surface("recorded_fixture", file);
            store.upsert_entity(&entity).unwrap();
            let answer = lookup_at(&store, file, 20);
            assert_eq!(answer["results"][0]["id"], json!(entity.id));
            assert_eq!(answer[KEY]["file"], file);
            assert_eq!(answer[KEY]["tracked_in_graph"], true);
            assert_eq!(answer[KEY]["state"], "matched");
            assert!(recognizes_payload(&answer));
        }
        let missing = lookup_at(&store, "fixtures/absent/.git/config", 20);
        assert_eq!(missing[KEY]["state"], "coordinate_unrecorded");
        assert_eq!(missing["results"], json!([]));
        assert!(!crate::negative::answer_claims_absence(
            "semantic_search",
            &missing
        ));
        for file in [
            ".git/config",
            ".GIT/config",
            ".kin/manifest.json",
            ".KIN/config.toml",
            ".kin-session/state.json",
            "fixtures/nested/.kin-session/token",
        ] {
            assert!(
                looks_like_coordinate(file),
                "control attempt must not become fuzzy ranking: {file}"
            );
            assert!(
                matches!(
                    lookup(&store, file, 20, &EntityFilter::default(), true),
                    Err(McpError::InvalidParams(_))
                ),
                "{file}"
            );
        }
    }

    #[test]
    fn candidate_bound_refuses_instead_of_certifying_a_clipped_coordinate_set() {
        let store = InMemoryGraph::new();
        for index in 0..=MAX_COORDINATE_CANDIDATES {
            store
                .upsert_entity(&surface(&format!("module_{index}"), "src/main.py"))
                .unwrap();
        }
        let answer = lookup(&store, "src/main.py", 20, &EntityFilter::default(), true);
        assert!(
            matches!(answer,Err(McpError::Other(ref reason)) if reason.contains("candidate bound"))
        );
    }

    #[test]
    fn budget_cuts_disclose_retained_modules_without_turning_them_into_an_absence() {
        let store = InMemoryGraph::new();
        for index in 0..40 {
            let name = format!("module_{index}_{}", "x".repeat(300));
            store.upsert_entity(&surface(&name, "src/main.py")).unwrap();
        }
        let answer = lookup_at(&store, "src/main.py", 40);
        let mut name = "locate".to_string();
        let mut arguments = HashMap::from([
            ("query".into(), json!("src/main.py")),
            ("max_output_bytes".into(), json!(2000)),
        ]);
        crate::verbs::resolve(&mut name, &mut arguments).unwrap();
        assert_eq!(name, "semantic_search");
        let budget = crate::budget::ResponseBudget::from_arguments(&arguments);
        assert_eq!(budget.max_chars, 2000);
        let result = crate::envelope::finalize_bounded(
            ToolCallResult::text(answer.to_string()),
            ready(),
            "semantic_search",
            &budget,
        );
        let machine = payload(&result);
        assert!(
            recognizes_payload(&machine),
            "coordinate identity/scope survives cuts: {machine}"
        );
        assert_eq!(machine["total_matches"], 40);
        assert!(machine["results"].as_array().unwrap().len() < 40);
        assert_eq!(machine["negative"]["safe_to_conclude_absent"], false);
        assert_eq!(
            machine["_kin"]["verdict"]["absence_claim"],
            "not_applicable"
        );
        let mut result = serde_json::to_value(result).unwrap();
        crate::query_reply::QueryPresentation::default().present(
            &mut result,
            "semantic_search",
            &HashMap::new(),
        );
        let plain: Value =
            serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
        assert!(plain.get("coordinate_lookup").is_none());
        assert!(!plain.to_string().contains("src/main.py"));
        let rows = plain["results"].as_array().unwrap();
        assert_eq!(rows.len(), machine["results"].as_array().unwrap().len());
        for (row, original) in rows.iter().zip(machine["results"].as_array().unwrap()) {
            assert_eq!(row["entity_id"], original["id"]);
            assert_eq!(row["name"], original["name"]);
        }
        assert_eq!(plain["total"], 40);
        assert!(plain["_kin"]["delivered"]
            .as_str()
            .unwrap()
            .contains("of 40"));
        assert!(!plain["_kin"]["meaning"]
            .as_str()
            .unwrap()
            .contains("no recorded module"));
        assert!(plain["_kin"]["meaning"]
            .as_str()
            .unwrap()
            .contains("does not establish that every declaration was parsed"));
    }

    #[test]
    fn rendered_coordinate_scope_and_details_preserve_module_identity_and_delivery() {
        let store = InMemoryGraph::new();
        for name in ["one", "two"] {
            store.upsert_entity(&surface(name, "src/main.py")).unwrap();
        }
        let answer = lookup_at(&store, "src/main.py", 1);
        let final_result = crate::envelope::finalize(
            ToolCallResult::text(answer.to_string()),
            ready(),
            "semantic_search",
        );
        for details in [false, true] {
            let mut result = serde_json::to_value(&final_result).unwrap();
            crate::query_reply::QueryPresentation::default().present(
                &mut result,
                "semantic_search",
                &HashMap::from([("details".into(), json!(details))]),
            );
            let plain: Value =
                serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
            assert_eq!(plain["results"][0]["entity_id"], answer["results"][0]["id"]);
            assert_eq!(plain["results"][0]["name"], answer["results"][0]["name"]);
            assert!(plain["results"][0].get("file").is_none());
            assert!(plain.get("coordinate_lookup").is_none());
            assert!(!plain.to_string().contains("src/main.py"));
            assert_eq!(plain["total"], 2);
            assert!(plain["_kin"]["meaning"]
                .as_str()
                .unwrap()
                .contains("module entities recorded for this selection"));
            assert!(plain["_kin"]["delivered"]
                .as_str()
                .unwrap()
                .contains("Showing 1 of 2"));
            assert!(!plain["_kin"]["searched"]
                .as_str()
                .unwrap()
                .contains("every declaration"));
            // The stored header is exactly `module <origin>`, not a program
            // declaration. Neither detail mode may expose that coordinate.
            assert!(plain["results"][0].get("signature").is_none());
        }
        let missing = lookup_at(&store, "src/missing.py", 20);
        let result = crate::envelope::finalize(
            ToolCallResult::text(missing.to_string()),
            ready(),
            "semantic_search",
        );
        let mut result = serde_json::to_value(result).unwrap();
        crate::plain_reply::present(&mut result, "semantic_search");
        let plain: Value =
            serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(plain["_kin"]["verdict"], crate::plain_reply::PARTIAL);
        assert!(plain["_kin"]["meaning"]
            .as_str()
            .unwrap()
            .contains("no recorded module identity"));
        assert!(plain["_kin"]["meaning"]
            .as_str()
            .unwrap()
            .contains("does not establish that every declaration was parsed"));
        assert!(!plain.to_string().contains("src/missing.py"));
    }
}
