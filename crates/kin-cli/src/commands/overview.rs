// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

use anyhow::{Context, Result};
use kin_mcp::handlers::common::entity_presentation_start_line;
use kin_model::EntityStore;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverviewRequest {
    #[serde(default)]
    pub json: bool,
    #[serde(default)]
    pub compact: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverviewResponse {
    #[serde(default)]
    pub lines: Vec<String>,
}

pub async fn run_json() -> Result<()> {
    let layout = crate::commands::require_repository_layout()?;
    print_overview_response(
        run_daemon_overview(
            &layout,
            &OverviewRequest {
                json: true,
                compact: true,
            },
        )
        .await?,
    )
}

pub async fn run(compact: bool) -> Result<()> {
    let layout = crate::commands::require_repository_layout()?;
    print_overview_response(
        run_daemon_overview(
            &layout,
            &OverviewRequest {
                json: false,
                compact,
            },
        )
        .await?,
    )
}

async fn run_daemon_overview(
    layout: &kin_core::KinLayout,
    request: &OverviewRequest,
) -> Result<OverviewResponse> {
    let daemon_url = std::env::var("KIN_DAEMON_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(Some)
        .unwrap_or(crate::daemon_client::resolve_daemon_url(layout).await?);
    let base_url = daemon_url
        .ok_or_else(|| crate::daemon_client::daemon_required_error("overview", layout))?;
    let client = crate::daemon_client::DaemonClient::from_base_url(base_url)?;
    client
        .overview(request)
        .await
        .context("daemon overview failed")
}

fn print_overview_response(response: OverviewResponse) -> Result<()> {
    for line in response.lines {
        println!("{line}");
    }
    Ok(())
}

pub fn build_overview_response(
    repo_name: &str,
    graph: &kin_db::InMemoryGraph,
    request: &OverviewRequest,
) -> Result<OverviewResponse> {
    let entities = graph.list_all_entities()?;
    if request.json {
        let unique_files: std::collections::HashSet<_> = entities
            .iter()
            .filter_map(|e| e.file_origin.as_ref().map(|f| f.0.clone()))
            .collect();
        let mut relation_ids = std::collections::HashSet::new();
        let mut kinds = HashMap::<String, usize>::new();
        let mut languages = HashMap::<String, usize>::new();

        for entity in &entities {
            *kinds.entry(format!("{:?}", entity.kind)).or_default() += 1;
            *languages.entry(entity.language.to_string()).or_default() += 1;
            for rel in graph.get_all_relations_for_entity(&entity.id)? {
                relation_ids.insert(rel.id);
            }
        }

        return Ok(OverviewResponse {
            lines: vec![serde_json::to_string(&serde_json::json!({
                "entities": entities.len(),
                "edges": relation_ids.len(),
                "files": unique_files.len(),
                "kinds": kinds,
                "languages": languages,
            }))?],
        });
    }

    // The parsed units these entities were parsed from. Counted by origin, and
    // said as units: Kin holds entities, and the reader of this page is asking
    // how much of the repository was parsed into them, not how many files the
    // checkout has.
    let parsed_units: std::collections::HashSet<_> = entities
        .iter()
        .filter_map(|e| e.file_origin.as_ref().map(|f| f.0.clone()))
        .collect();

    let mut lines = Vec::new();
    lines.push("=== Kin Overview ===".to_string());
    lines.push(format!(
        "Repository: {}  |  Entities: {}  |  Parsed units: {}",
        repo_name,
        entities.len(),
        parsed_units.len()
    ));
    lines.push(String::new());

    // Group by language
    let mut by_lang: HashMap<String, (usize, std::collections::HashSet<String>)> = HashMap::new();
    for e in &entities {
        let lang_str = e.language.to_string();
        let entry = by_lang
            .entry(lang_str)
            .or_insert((0, std::collections::HashSet::new()));
        entry.0 += 1;
        if let Some(ref fo) = e.file_origin {
            entry.1.insert(fo.0.clone());
        }
    }
    lines.push("--- Languages ---".to_string());
    let mut langs: Vec<_> = by_lang.iter().collect();
    langs.sort_by_key(|b| std::cmp::Reverse(b.1 .0));
    for (lang, (count, units)) in &langs {
        lines.push(format!(
            "  {}: {} entities in {} parsed units",
            lang,
            count,
            units.len()
        ));
    }
    lines.push(String::new());

    // Group by kind
    let mut by_kind: HashMap<String, Vec<&kin_model::Entity>> = HashMap::new();
    for e in &entities {
        by_kind.entry(format!("{:?}", e.kind)).or_default().push(e);
    }

    if request.compact {
        // Compact mode: just counts per kind, no entity listings
        lines.push("--- Entity Kinds ---".to_string());
        let mut kinds: Vec<_> = by_kind.iter().collect();
        kinds.sort_by_key(|b| std::cmp::Reverse(b.1.len()));
        for (kind, ents) in &kinds {
            lines.push(format!("  {}: {}", kind, ents.len()));
        }
    } else {
        // Full mode: show top entities per kind
        let top_n = 5;
        lines.push("--- Top Entities by Kind ---".to_string());
        let mut kinds: Vec<_> = by_kind.iter().collect();
        kinds.sort_by_key(|b| std::cmp::Reverse(b.1.len()));
        for (kind, ents) in &kinds {
            lines.push(format!("{} ({}):", kind, ents.len()));
            for e in ents.iter().take(top_n) {
                // The graph stores tree-sitter rows, which are 0-based; a
                // `file:line` a reader pastes into an editor is 1-based. An entity
                // the graph carries no span for names its origin alone rather than
                // the `:0` that used to appear here, and one with no origin at all
                // is named alone rather than beside a `?`.
                let location =
                    e.file_origin
                        .as_ref()
                        .map(|origin| match entity_presentation_start_line(e) {
                            Some(line) => format!("{}:{line}", origin.0),
                            None => origin.0.clone(),
                        });
                match location {
                    Some(location) => lines.push(format!("  {}  {}", e.name, location)),
                    None => lines.push(format!("  {}", e.name)),
                }
            }
            if ents.len() > top_n {
                lines.push(format!("  ... and {} more", ents.len() - top_n));
            }
            lines.push(String::new());
        }
    }

    Ok(OverviewResponse { lines })
}

#[cfg(test)]
mod tests {
    use super::{build_overview_response, OverviewRequest};
    use kin_db::InMemoryGraph;
    use kin_model::{
        Entity, EntityId, EntityKind, EntityMetadata, EntityRole, EntityStore, FilePathId,
        FingerprintAlgorithm, Hash256, LanguageId, SemanticFingerprint, SourceSpan, Visibility,
    };

    fn overview_entity(name: &str, path: &str, start_line: Option<u32>) -> Entity {
        let file = FilePathId::new(path);
        Entity {
            id: EntityId::new(),
            kind: EntityKind::Function,
            name: name.to_string(),
            language: LanguageId::Python,
            fingerprint: SemanticFingerprint {
                algorithm: FingerprintAlgorithm::V1TreeSitter,
                ast_hash: Hash256::from_bytes([0; 32]),
                signature_hash: Hash256::from_bytes([1; 32]),
                behavior_hash: Hash256::from_bytes([2; 32]),
                equivalence_hash: Hash256::from_bytes([0; 32]),
                stability_score: 1.0,
            },
            span: start_line.map(|start_line| SourceSpan {
                file: file.clone(),
                start_byte: 0,
                end_byte: 1,
                start_line,
                start_col: 0,
                end_line: start_line + 1,
                end_col: 1,
            }),
            file_origin: Some(file),
            signature: format!("def {name}()"),
            visibility: Visibility::Public,
            role: EntityRole::Source,
            doc_summary: None,
            metadata: EntityMetadata::default(),
            lineage_parent: None,
            created_in: None,
            superseded_by: None,
        }
    }

    /// `kin overview`'s top-entity listing reports the line a human editor shows.
    ///
    /// The graph stores tree-sitter rows, which are 0-based, and every
    /// agent-facing `file:line` is 1-based. This listing printed the raw row, so
    /// it disagreed with `kin refs` and every MCP read surface about the same
    /// entity, and a reader acting on it opened the line above the definition.
    #[test]
    fn the_top_entity_listing_reports_the_line_a_human_editor_shows() {
        const SOURCE: &str = "# header\n\n\ndef probe_overview():\n    return 1\n";
        // What a reader counting lines in an editor would say, derived from the
        // fixture rather than written down.
        let human_line = SOURCE
            .lines()
            .position(|line| line.contains("def probe_overview"))
            .map(|index| index + 1)
            .expect("the fixture declares the function") as u32;
        let graph_row = human_line - 1;

        let graph = InMemoryGraph::new();
        graph
            .upsert_entity(&overview_entity(
                "probe_overview",
                "src/probe.py",
                Some(graph_row),
            ))
            .expect("upsert spanned entity");
        // An entity the graph carries no span for has no line to report.
        graph
            .upsert_entity(&overview_entity("probe_spanless", "src/spanless.py", None))
            .expect("upsert spanless entity");

        let response = build_overview_response(
            "probe-repo",
            &graph,
            &OverviewRequest {
                json: false,
                compact: false,
            },
        )
        .expect("build overview");
        let joined = response.lines.join("\n");

        assert!(
            joined.contains(&format!("src/probe.py:{human_line}")),
            "the listing must name line {human_line}: {joined}"
        );
        assert!(
            !joined.contains(&format!("src/probe.py:{graph_row}")),
            "the raw 0-based row must not reach the reader: {joined}"
        );
        assert!(
            joined.contains("src/spanless.py") && !joined.contains("src/spanless.py:0"),
            "a spanless entity names its file without a fabricated line: {joined}"
        );
    }

    /// The page speaks in entities and parsed units, never in files: Kin holds
    /// entities, and a first conversion's overview that said `Files: 83` told a
    /// reader the opposite. An entity with no origin is named alone rather
    /// than beside a `?`.
    #[test]
    fn the_overview_counts_parsed_units_and_never_says_files() {
        let graph = InMemoryGraph::new();
        for (name, path) in [
            ("first", "src/a.py"),
            ("second", "src/a.py"),
            ("third", "src/b.py"),
        ] {
            graph
                .upsert_entity(&overview_entity(name, path, Some(0)))
                .expect("upsert entity");
        }
        let mut originless = overview_entity("originless", "src/c.py", None);
        originless.file_origin = None;
        graph.upsert_entity(&originless).expect("upsert entity");
        for compact in [true, false] {
            let response = build_overview_response(
                "probe-repo",
                &graph,
                &OverviewRequest {
                    json: false,
                    compact,
                },
            )
            .expect("build overview");
            let joined = response.lines.join("\n");
            assert!(
                joined.contains("Entities: 4  |  Parsed units: 2"),
                "{joined}"
            );
            assert!(
                joined.contains(&format!(
                    "{}: 4 entities in 2 parsed units",
                    LanguageId::Python
                )),
                "{joined}"
            );
            assert!(!joined.to_lowercase().contains("file"), "{joined}");
            assert!(!joined.contains('?'), "{joined}");
        }
    }

    /// The machine answer counts entities by language as well as by kind, so
    /// a reader that renders the repository's languages reads a number rather
    /// than parsing the terminal listing.
    #[test]
    fn the_json_answer_counts_entities_by_language() {
        let graph = InMemoryGraph::new();
        for name in ["first", "second"] {
            graph
                .upsert_entity(&overview_entity(name, "src/probe.py", Some(0)))
                .expect("upsert entity");
        }
        let response = build_overview_response(
            "probe-repo",
            &graph,
            &OverviewRequest {
                json: true,
                compact: true,
            },
        )
        .expect("build overview");
        let answer: serde_json::Value =
            serde_json::from_str(&response.lines[0]).expect("one JSON line");
        assert_eq!(answer["entities"], 2);
        assert_eq!(
            answer["languages"][LanguageId::Python.to_string()],
            2,
            "{answer}"
        );
    }
}
