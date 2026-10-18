// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Bounded match evidence over the text held by the current graph authority.
//! Code matches belong to the innermost recorded entity span. Unparsed text
//! matches retain the artifact UUID and namespace; neither path returns a
//! source body or invents a semantic binding. Incomplete code spans are gaps.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;

use kin_model::entity::Entity;
use kin_model::graph::{EntityFilter, GraphStore};
use kin_model::ids::FilePathId;
use kin_model::{Hash256, ResolvedTree};
use serde_json::{json, Value};

use super::text_match::{self, Budget, Matcher, Options};
use crate::artifact_entity::{self, ArtifactEntity, Held, Level};
use crate::error::{McpError, Result};

pub(crate) type TextOf<'a> = &'a dyn Fn(Hash256) -> Option<Arc<Vec<u8>>>;

/// Unlike the fixture reader, a live authority may refuse a source allowance.
/// That refusal aborts the whole search instead of becoming an unread unit.
pub(super) type SearchTextOf<'a> = &'a dyn Fn(Hash256) -> Result<Option<Arc<Vec<u8>>>>;

/// No process-wide byte cache: every read must pass this request's authority.
/// HeldSourceAuthority already memoizes admitted content within that request.
fn held(digest: Hash256, text_of: SearchTextOf<'_>) -> Result<Option<Arc<Vec<u8>>>> {
    text_of(digest)
}

pub(crate) enum Subject {
    Code(Entity),
    Text {
        entity: ArtifactEntity,
        kind: String,
    },
}

pub(crate) struct TextRow {
    subject: Subject,
    pub(crate) line_in_entity: u32,
    pub(crate) line: String,
    pub(crate) more_lines: usize,
}

impl TextRow {
    pub(crate) fn entity(&self) -> Option<&Entity> {
        match &self.subject {
            Subject::Code(entity) => Some(entity),
            _ => None,
        }
    }

    /// The module an unparsed entity's row is placed in, `Some(None)` when it
    /// has none, and `None` for a code row.
    pub(crate) fn unparsed_module(&self) -> Option<Option<&str>> {
        match &self.subject {
            Subject::Text { entity, .. } => Some(entity.module.as_deref()),
            Subject::Code(_) => None,
        }
    }

    pub(crate) fn hit<G: GraphStore>(&self, store: &G) -> Value {
        let mut hit = match &self.subject {
            Subject::Code(entity) => {
                let mut hit = json!({"entity_id": entity.id, "name": entity.name,
                    "kind": entity.kind, "language": entity.language});
                if entity.role == kin_model::EntityRole::Test {
                    hit["role"] = json!("test");
                }
                crate::semantic_owners::RecordedOwners::observe(store, entity).attach(&mut hit);
                hit
            }
            Subject::Text { entity, kind } => {
                let mut hit = json!({"entity_id": entity.id().0, "name": entity.name,
                    "kind": kind, "held_as": "text"});
                if let Some(module) = &entity.module {
                    hit["module"] = json!(module);
                }
                hit
            }
        };
        hit["line_in_entity"] = json!(self.line_in_entity);
        hit["line_confidence"] = json!("exact");
        hit["matched_field"] = json!("text");
        hit["excerpt"] = json!(self.line);
        if self.more_lines > 0 {
            hit["more_lines"] = json!(self.more_lines);
        }
        hit
    }
}

#[derive(Default, serde::Serialize)]
pub(crate) struct Coverage {
    pub(crate) modules: usize,
    pub(crate) unread: usize,
    pub(crate) outside_entities: usize,
    pub(crate) modules_without_complete_spans: usize,
    pub(crate) modules_with_unverified_parse: usize,
    pub(crate) artifacts_text: usize,
    pub(crate) artifacts_unread: usize,
    pub(crate) artifacts_bytes_skipped: usize,
    pub(crate) unaddressable: usize,
}

/// Search a single held tree, without changing code reads or offering a file
/// fallback. Limits refuse the entire lookup, never certify a partial scan.
pub(crate) fn search<G: GraphStore>(
    store: &G,
    tree: &ResolvedTree,
    needle: &str,
    options: Options,
    text_of: SearchTextOf<'_>,
) -> Result<(Vec<TextRow>, Coverage, Budget)> {
    let mut budget = Budget::new();
    let matcher = Matcher::new(needle, options, &mut budget)?;
    if tree.artifacts_by_path().count() > 10_000 {
        return Err(McpError::InvalidParams(
            "text search refused: tree exceeds 10000 held units".into(),
        ));
    }
    // Namespace preparation shares the same reader and deadline as matches.
    // In particular, a root manifest cannot consume an authority allowance and
    // hide its refusal by converting the typed failure to a missing name.
    let root = match artifact_entity::namespace::root_manifest(tree) {
        Some((manifest, hash)) => {
            budget.check()?;
            let bytes = held(hash, text_of)?;
            budget.check()?;
            bytes.and_then(|bytes| {
                std::str::from_utf8(&bytes).ok().and_then(|text| {
                    artifact_entity::namespace::declared_project_name(&manifest, text)
                })
            })
        }
        None => None,
    };
    let placement = Held::load(store, tree, root).map_err(McpError::Context)?;
    let mut coverage = Coverage::default();
    let mut rows = Vec::new();
    for artifact in tree.artifacts_by_path() {
        budget.check()?;
        let code = artifact_entity::is_code(&artifact.path);
        let kin_model::TreeEntry::Blob { hash, .. } = artifact.entry else {
            coverage.artifacts_bytes_skipped += 1;
            continue;
        };
        if code {
            coverage.modules += 1;
        }
        let Some(bytes) = held(hash, text_of)? else {
            if code {
                coverage.unread += 1;
            } else {
                coverage.artifacts_unread += 1;
            }
            continue;
        };
        // All content reads count toward the same budget, including bytes
        // later excluded from text matching.
        if !code && Level::of(&bytes) == Level::Bytes {
            budget.charge_unit(bytes.len())?;
            coverage.artifacts_bytes_skipped += 1;
            continue;
        }
        let ranges = matcher.ranges(&bytes, &mut budget)?;
        if code {
            let Some(path) = artifact.path.as_utf8() else {
                coverage.unaddressable += 1;
                continue;
            };
            let entities = store
                .query_entities(&EntityFilter {
                    file_path: Some(FilePathId::new(path)),
                    ..Default::default()
                })
                .map_err(McpError::graph)?;
            let original_count = entities.len();
            let entities: Vec<_> = entities
                .into_iter()
                .filter(|entity| {
                    matches!(
                        super::common::span_source_coherence(entity, &hash, path),
                        Ok(super::common::SpanCoherence::DigestVerified)
                    )
                })
                .collect();
            let spans_verified = entities.len() == original_count;
            coverage.modules_with_unverified_parse +=
                usize::from(!parse_recorded_whole(store, artifact, hash));
            let (unit_rows, outside, complete) = attribute(&bytes, &ranges, entities, &budget)?;
            coverage.outside_entities += outside;
            coverage.modules_without_complete_spans += usize::from(!complete || !spans_verified);
            rows.extend(unit_rows);
        } else {
            coverage.artifacts_text += 1;
            if artifact.path.as_utf8().is_none() {
                coverage.unaddressable += 1;
                continue;
            }
            if let Some(first) = ranges.first() {
                let entity = placement.placed(artifact);
                let kind = artifact_entity::kind::describe(
                    &entity.place(),
                    &entity.name,
                    true,
                    &bytes[..bytes.len().min(4096)],
                )
                .kind;
                let mut lines = HashSet::new();
                let (mut scanned, mut line) = (0, 0);
                for range in &ranges {
                    budget.check()?;
                    advance_line(&bytes, &mut scanned, &mut line, range.start);
                    lines.insert(line);
                }
                rows.push(TextRow {
                    subject: Subject::Text { entity, kind },
                    line_in_entity: bytes[..first.start].iter().filter(|&&b| b == b'\n').count()
                        as u32
                        + 1,
                    line: text_match::excerpt(&bytes, first.start),
                    more_lines: lines.len() - 1,
                });
            }
        }
    }
    budget.check()?;
    Ok((rows, coverage, budget))
}

/// Own each match by its complete byte range, never by the pattern's length.
fn attribute(
    bytes: &[u8],
    ranges: &[Range<usize>],
    entities: Vec<Entity>,
    budget: &Budget,
) -> Result<(Vec<TextRow>, usize, bool)> {
    let mut spanned: Vec<_> = entities
        .into_iter()
        .filter_map(|entity| {
            let span = entity.span.as_ref()?;
            let (start, end) = (span.start_byte as usize, span.end_byte as usize);
            (start < end && end <= bytes.len()).then_some((start, end, entity))
        })
        .collect();
    spanned.sort_by_key(|(start, end, entity)| (*start, *end, entity.id));
    // A missing span cannot turn a match-free scan into proof of semantic
    // coverage. Whitespace between adjacent entity spans is harmless.
    let mut covered = 0;
    let mut complete = true;
    for (start, end, _) in &spanned {
        if *start > covered
            && bytes[covered..*start]
                .iter()
                .any(|b| !b.is_ascii_whitespace())
        {
            complete = false;
        }
        covered = covered.max(*end);
    }
    if bytes[covered..].iter().any(|b| !b.is_ascii_whitespace()) {
        complete = false;
    }
    let mut outside = 0;
    let mut matches: HashMap<usize, (usize, HashSet<usize>)> = HashMap::new();
    let (mut scanned, mut line) = (0, 0);
    for range in ranges {
        budget.check()?;
        advance_line(bytes, &mut scanned, &mut line, range.start);
        let owner = spanned
            .iter()
            .enumerate()
            .filter(|(_, (start, end, _))| *start <= range.start && range.end <= *end)
            .min_by_key(|(_, (start, end, _))| end - start)
            .map(|(index, _)| index);
        if let Some(index) = owner {
            let (_, lines) = matches
                .entry(index)
                .or_insert_with(|| (range.start, HashSet::new()));
            lines.insert(line);
        } else {
            outside += 1;
        }
    }
    let mut matches: Vec<_> = matches.into_iter().collect();
    matches.sort_by_key(|(index, _)| *index);
    let rows = matches
        .into_iter()
        .map(|(index, (first, lines))| {
            budget.check()?;
            let (start, end, entity) = &spanned[index];
            Ok(TextRow {
                subject: Subject::Code(entity.clone()),
                line_in_entity: bytes[*start..first].iter().filter(|&&b| b == b'\n').count() as u32
                    + 1,
                // Crop to the entity first: a long line shared by two entities
                // must not expose the neighbor's body as match context.
                line: text_match::excerpt(&bytes[*start..*end], first - start),
                more_lines: lines.len() - 1,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok((rows, outside, complete))
}

// Both matcher modes yield ascending starts. Advancing only across newly
// visited bytes avoids quadratic rescans on one long, heavily matched line.
fn advance_line(bytes: &[u8], scanned: &mut usize, line: &mut usize, at: usize) {
    if let Some(newline) = bytes[*scanned..at].iter().rposition(|&b| b == b'\n') {
        *line = *scanned + newline + 1;
    }
    *scanned = at;
}

fn parse_recorded_whole<G: GraphStore>(
    store: &G,
    artifact: &kin_model::ResolvedArtifact,
    hash: Hash256,
) -> bool {
    use kin_model::graph::RelationLookup;
    let Ok(RelationLookup::Present(relation)) =
        store.lookup_relation_by_id(&kin_index::parse_coverage_relation_id(artifact.artifact_id))
    else {
        return false;
    };
    let node = kin_model::GraphNodeId::Artifact(artifact.artifact_id);
    relation.src == node
        && relation.dst == node
        && relation.kind == kin_model::RelationKind::DependsOn
        && kin_index::parse_coverage_source_digest(&relation) == Some(hash)
        && kin_index::coverage_certificate::CertificateDetail::from_certificate(&relation)
            .and_then(|detail| detail.parse)
            .is_some_and(|parse| parse.bucket() == "full")
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use kin_model::graph::EntityStore;
    use serde_json::{json, Value};

    /// Flask's application class, shaped as it is: a docstring long enough
    /// that the class's held body preview stops before its class-level code,
    /// then `default_config`, then a method.
    fn flask_app() -> String {
        let doc = "The flask object implements a WSGI application. ".repeat(80);
        format!(
            "class Flask:\n    \"\"\"{doc}\"\"\"\n\n    default_config = {{\n        \
             \"DEBUG\": None,\n        \"SESSION_REFRESH_EACH_REQUEST\": True,\n    }}\n\n    \
             def run(self):\n        return 1\n"
        )
    }

    const SESSIONS: &str = "class SecureCookieSessionInterface:\n    \
        def should_set_cookie(self, app, session):\n        \
        return session.permanent and app.config[\"SESSION_REFRESH_EACH_REQUEST\"]\n\n\n\
        def make_null_session():\n    marker = \"held-only-in-a-function-body\"\n    return marker\n";

    /// A text search as an agent reads it when a daemon answered: the
    /// handler's answer under the daemon's envelope, then the reply the stdio
    /// server writes, plain and in text. An in-memory graph keeps no content,
    /// so the search is handed the bytes the fixtures admitted.
    fn text_search_as_delivered(
        graph: &kin_db::InMemoryGraph,
        text: &str,
        held: &[&str],
    ) -> String {
        use crate::server::{present_for_client, McpServerConfig};
        let content: HashMap<Hash256, Arc<Vec<u8>>> = held
            .iter()
            .map(|source| {
                let bytes = source.as_bytes().to_vec();
                (
                    Hash256::from_bytes(kin_blobs::digest(&bytes).0),
                    Arc::new(bytes),
                )
            })
            .collect();
        let args: HashMap<String, Value> = HashMap::from([
            ("literal".to_string(), json!(text)),
            (
                super::super::lexical::HELD_TEXT_KEY.to_string(),
                json!(true),
            ),
        ]);
        let result = super::super::lexical::lookup(
            &args,
            graph,
            None,
            Some(&|digest| content.get(&digest).cloned()),
        )
        .unwrap();
        let envelope = crate::envelope::Envelope::daemon().with_health(&json!({
            "graph_loaded": true,
            "initialized": true,
            "graph_entity_count": kin_model::graph::EntityStore::list_all_entities(graph).unwrap().len(),
            "graph_generation": 1,
        }));
        let finalized = crate::envelope::finalize_bounded(
            result,
            envelope,
            super::super::lexical::TOOL_NAME,
            &crate::budget::ResponseBudget::default(),
        );
        let crate::types::ContentBlock::Text { text: machine } =
            finalized.content.first().expect("one block");
        let config = McpServerConfig {
            plain_replies: true,
            agent_belt: true,
            reply_format: crate::text_reply::ReplyFormat::Text,
            session_handles: Some(Arc::new(crate::session_handles::SessionHandles::default())),
            ..Default::default()
        };
        let message = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "locate", "arguments": {"query": text, "mode": "text"}}})
        .to_string();
        let response = crate::types::JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: Some(json!(1)),
            result: Some(json!({"content": [{"type": "text", "text": machine}]})),
            error: None,
        };
        let result = present_for_client(response, &message, &config)
            .result
            .unwrap();
        result["content"][0]["text"].as_str().unwrap().to_string()
    }

    /// A text search confined to a module path keeps the rows inside it and
    /// counts the rows left out: `SESSION_REFRESH_EACH_REQUEST` inside the
    /// session module is the method that reads it, not the class that sets
    /// its default.
    #[test]
    fn a_text_search_keeps_the_rows_inside_its_scope() {
        let (graph, app) = admitted();
        let content: HashMap<Hash256, Arc<Vec<u8>>> = [app.as_str(), SESSIONS]
            .iter()
            .map(|source| {
                let bytes = source.as_bytes().to_vec();
                (
                    Hash256::from_bytes(kin_blobs::digest(&bytes).0),
                    Arc::new(bytes),
                )
            })
            .collect();
        let search = |scope: Option<&str>| {
            let mut args: HashMap<String, Value> = HashMap::from([
                ("literal".to_string(), json!("SESSION_REFRESH_EACH_REQUEST")),
                (
                    super::super::lexical::HELD_TEXT_KEY.to_string(),
                    json!(true),
                ),
            ]);
            if let Some(scope) = scope {
                args.insert("scope".to_string(), json!(scope));
            }
            let result = super::super::lexical::lookup(
                &args,
                &graph,
                None,
                Some(&|digest| content.get(&digest).cloned()),
            )
            .unwrap();
            let crate::types::ContentBlock::Text { text } = result.content.first().unwrap();
            serde_json::from_str::<Value>(text).unwrap()
        };
        assert_eq!(search(None)["total_matching"], json!(2));
        let scoped = search(Some("sessions"));
        assert_eq!(scoped["total_matching"], json!(1), "{scoped}");
        assert!(scoped["hits"][0]["name"]
            .as_str()
            .unwrap()
            .ends_with("should_set_cookie"));
        assert_eq!(scoped["row_scope"]["outside"], json!(1), "{scoped}");
    }

    fn admitted() -> (kin_db::InMemoryGraph, String) {
        let graph = kin_db::InMemoryGraph::default();
        let app = flask_app();
        crate::declaration_coverage::fixtures::admit(&graph, "src/flask/app.py", &app, false);
        crate::declaration_coverage::fixtures::admit(
            &graph,
            "src/flask/sessions.py",
            SESSIONS,
            false,
        );
        (graph, app)
    }

    /// Measured in a real task: a model spent ten turns looking for where
    /// `SESSION_REFRESH_EACH_REQUEST` gets its default, a key in the
    /// class-level `default_config` of Flask's application class, past the
    /// 8,000 characters of the class any stored field holds. The text search
    /// answers with the independently held config-key field and the line
    /// numbered within that field, alongside the method that reads the key.
    #[test]
    fn a_config_key_in_class_level_code_is_found_in_its_own_field_with_its_line() {
        let (graph, app) = admitted();
        let text =
            text_search_as_delivered(&graph, "SESSION_REFRESH_EACH_REQUEST", &[&app, SESSIONS]);
        println!("{text}");
        assert!(
            text.starts_with(
                "Answer (complete): \"SESSION_REFRESH_EACH_REQUEST\" is in 2 entities Kin holds."
            ),
            "{text}"
        );
        let rows: Vec<&str> = text.lines().filter(|line| line.starts_with("  ")).collect();
        assert!(
            rows.iter().any(|row| row
                .contains("Flask.default_config.SESSION_REFRESH_EACH_REQUEST")
                && row.contains("(field)")
                && row.contains("\"SESSION_REFRESH_EACH_REQUEST\": True")
                && row.contains("line_in_entity=1")),
            "{text}"
        );
        assert!(
            rows.iter().any(
                |row| row.contains("SecureCookieSessionInterface.should_set_cookie")
                    && row.contains("line_in_entity=2")
            ),
            "{text}"
        );
        assert!(
            !text.contains("src/flask"),
            "no path reaches the reader: {text}"
        );
    }

    /// Text held only inside a function's body belongs to that function.
    #[test]
    fn text_inside_a_function_body_is_that_functions() {
        let (graph, app) = admitted();
        let text =
            text_search_as_delivered(&graph, "held-only-in-a-function-body", &[&app, SESSIONS]);
        assert!(
            text.starts_with(
                "Answer (complete): \"held-only-in-a-function-body\" is in 1 entity Kin holds."
            ),
            "{text}"
        );
        let rows: Vec<&str> = text.lines().filter(|line| line.starts_with("  ")).collect();
        assert_eq!(rows.len(), 1, "{text}");
        assert!(
            rows[0].contains("make_null_session") && rows[0].contains("line_in_entity=2"),
            "{text}"
        );
    }

    /// Text Kin holds nowhere is said to be in nothing it holds, with what it
    /// read; with a module it could not read back, it says the text could be
    /// there.
    #[test]
    fn text_held_nowhere_is_said_to_be_in_nothing_kin_holds_with_what_was_read() {
        let (graph, app) = admitted();
        let text = text_search_as_delivered(&graph, "NOT_A_KEY_ANYWHERE", &[&app, SESSIONS]);
        assert!(
            text.starts_with(
                "Answer (complete): \"NOT_A_KEY_ANYWHERE\" is in nothing Kin holds as code or text. Kin read every one of its 2 parsed modules, and none holds it."
            ),
            "{text}"
        );
        assert!(
            text.contains("Searched the text Kin holds of all 2 parsed modules"),
            "{text}"
        );

        // A module whose content no search in this process was handed, so
        // nothing kept it either.
        crate::declaration_coverage::fixtures::admit(
            &graph,
            "src/flask/cli.py",
            "def main_never_read_back():\n    return 0\n",
            false,
        );
        let text = text_search_as_delivered(&graph, "NOT_A_KEY_ANYWHERE", &[&app, SESSIONS]);
        assert!(
            text.starts_with(
                "Answer (partial): \"NOT_A_KEY_ANYWHERE\" is in none of the 2 parsed modules Kin read. Kin could not read back 1 parsed module, so the text could also be there."
            ),
            "{text}"
        );
    }

    #[test]
    fn locate_mode_text_routes_to_the_held_text_search() {
        let mut name = "locate".to_string();
        let mut arguments: HashMap<String, Value> = serde_json::from_value(
            json!({"query": "SESSION_REFRESH_EACH_REQUEST", "mode": "text"}),
        )
        .unwrap();
        crate::verbs::resolve(&mut name, &mut arguments).unwrap();
        assert_eq!(name, super::super::lexical::TOOL_NAME);
        assert_eq!(arguments["literal"], json!("SESSION_REFRESH_EACH_REQUEST"));
        assert_eq!(arguments[super::super::lexical::HELD_TEXT_KEY], json!(true));
    }

    fn occurrences(bytes: &[u8], query: &[u8]) -> Vec<usize> {
        if query.is_empty() {
            return vec![];
        }
        let mut budget = Budget::new();
        Matcher::new(
            std::str::from_utf8(query).unwrap(),
            Options::default(),
            &mut budget,
        )
        .unwrap()
        .ranges(bytes, &mut budget)
        .unwrap()
        .into_iter()
        .map(|r| r.start)
        .collect()
    }

    #[test]
    fn occurrences_are_every_exact_position() {
        assert_eq!(occurrences(b"abcabcab", b"ab"), vec![0, 3, 6]);
        assert_eq!(occurrences(b"aaaa", b"aa"), vec![0, 1, 2]);
        assert!(occurrences(b"ABC", b"abc").is_empty());
        assert!(occurrences(b"ab", b"abc").is_empty());
        assert!(occurrences(b"ab", b"").is_empty());
    }
    #[test]
    fn regex_ownership_uses_match_range_and_excerpts_stop_at_entity_boundary() {
        use kin_model::graph::EntityStore;
        let graph = kin_db::InMemoryGraph::default();
        let source = "def first():\n    return 'Marker LONG VALUE'\n\ndef second():\n    return 'neighbor secret'\n";
        crate::declaration_coverage::fixtures::admit(&graph, "owned.py", source, false);
        let mut entities = graph.list_all_entities().unwrap();
        entities.retain(|e| e.name == "first");
        assert_eq!(entities.len(), 1);
        let budget = Budget::new();
        let start = source.find("Marker").unwrap();
        let (_, outside, _) = attribute(
            source.as_bytes(),
            std::slice::from_ref(&(start..source.len())),
            entities.clone(),
            &budget,
        )
        .unwrap();
        assert_eq!(
            outside, 1,
            "a regex spanning two bodies must not belong to first"
        );
        let (rows, outside, _) = attribute(
            source.as_bytes(),
            std::slice::from_ref(&(start..start + "Marker LONG VALUE".len())),
            entities,
            &budget,
        )
        .unwrap();
        assert_eq!(outside, 0);
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].line.contains("neighbor"));
    }

    fn raw_search(
        graph: &kin_db::InMemoryGraph,
        args: Value,
        content: &[&str],
    ) -> crate::error::Result<Value> {
        let content: HashMap<_, _> = content
            .iter()
            .map(|text| {
                let bytes = text.as_bytes().to_vec();
                (
                    Hash256::from_bytes(kin_blobs::digest(&bytes).0),
                    Arc::new(bytes),
                )
            })
            .collect();
        let args = serde_json::from_value(args).unwrap();
        let result = super::super::lexical::lookup(
            &args,
            graph,
            None,
            Some(&|digest| content.get(&digest).cloned()),
        )?;
        let crate::types::ContentBlock::Text { text, .. } = &result.content[0];
        Ok(serde_json::from_str(text).unwrap())
    }

    fn add_artifact(
        graph: &kin_db::InMemoryGraph,
        path: &str,
        content: &str,
    ) -> kin_model::ArtifactId {
        let id = kin_model::ArtifactId::new();
        graph
            .apply_transaction_delta(&kin_model::TransactionDelta {
                tree_deltas: vec![kin_model::TreeDelta::Added {
                    artifact_id: id,
                    new: kin_model::LocatedEntry::new(
                        kin_model::RepoPath::from_utf8(path).unwrap(),
                        kin_model::TreeEntry::blob(
                            Hash256::from_bytes(kin_blobs::digest(content.as_bytes()).0),
                            false,
                        ),
                    ),
                }],
                ..Default::default()
            })
            .unwrap();
        id
    }

    #[test]
    fn held_artifact_match_is_bounded_semantic_evidence_not_a_body_read() {
        let graph = kin_db::InMemoryGraph::default();
        let text = format!(
            "heading\n{}needle{}\nneedle again\n",
            "a".repeat(400),
            "b".repeat(400)
        );
        let id = add_artifact(&graph, "docs/guide.md", &text);
        add_artifact(&graph, "image.dat", "\0needle");
        let result = raw_search(
            &graph,
            json!({"literal":"needle","held_text":true}),
            &[&text, "\0needle"],
        )
        .unwrap();
        assert_eq!(result["total_matching"], 1);
        let row = &result["hits"][0];
        assert_eq!(row["entity_id"], json!(id.0));
        assert_eq!(row["name"], "guide.md");
        assert_eq!(row["held_as"], "text");
        assert_eq!(row["line_in_entity"], 2);
        assert_eq!(row["more_lines"], 1);
        assert!(row["excerpt"].as_str().unwrap().chars().count() <= 160);
        for key in [
            "file",
            "file_path",
            "projected_at",
            "source",
            "body",
            "source_base",
        ] {
            assert!(row.get(key).is_none(), "{key}: {row}");
        }
        assert_eq!(result["held_text_coverage"]["artifacts_text"], 1);
        assert_eq!(result["held_text_coverage"]["artifacts_bytes_skipped"], 1);
    }

    #[test]
    fn primed_content_never_supplies_a_request_without_authority() {
        let graph = kin_db::InMemoryGraph::default();
        let text = "def held_authority():\n    return 'needle'\n";
        crate::declaration_coverage::fixtures::admit(&graph, "authority.py", text, false);
        assert_eq!(
            raw_search(
                &graph,
                json!({"literal":"needle","held_text":true}),
                &[text]
            )
            .unwrap()["total_matching"],
            1
        );
        let args = serde_json::from_value(json!({"literal":"needle","held_text":true})).unwrap();
        let result = super::super::lexical::handle_lexical_lookup(&args, &graph, None).unwrap();
        let crate::types::ContentBlock::Text { text, .. } = &result.content[0];
        let value: Value = serde_json::from_str(text).unwrap();
        assert_eq!(value["total_matching"], 0);
        assert_eq!(value["held_text_coverage"]["modules_unread"], 1);
        assert_eq!(value["held_text_coverage"]["authority_unavailable"], true);
    }

    #[test]
    fn recognized_code_without_entities_remains_a_gap_not_an_artifact_row() {
        let graph = kin_db::InMemoryGraph::default();
        let text = "def unparsed():\n    return 'needle'\n";
        add_artifact(&graph, "broken.py", text);
        let value = raw_search(
            &graph,
            json!({"literal":"needle","held_text":true}),
            &[text],
        )
        .unwrap();
        assert_eq!(value["total_matching"], 0);
        assert_eq!(
            value["held_text_coverage"]["modules_without_complete_spans"],
            1
        );
        assert_eq!(
            value["held_text_coverage"]["occurrences_outside_entities"],
            1
        );
        assert_eq!(value["held_text_coverage"]["artifacts_text"], 0);
        let absent = raw_search(
            &graph,
            json!({"literal":"absent","held_text":true}),
            &[text],
        )
        .unwrap();
        assert_eq!(
            absent["held_text_coverage"]["modules_without_complete_spans"],
            1
        );
    }

    #[test]
    fn text_cursors_bind_flags_evidence_and_held_tree() {
        let graph = kin_db::InMemoryGraph::default();
        add_artifact(&graph, "first.md", "NEEDLE one");
        add_artifact(&graph, "second.md", "needle two");
        let content = &["NEEDLE one", "needle two"];
        let value = raw_search(
            &graph,
            json!({"literal":"n[e]+dle","held_text":true,
            "pattern":true,"ignore_case":true,"limit":1}),
            content,
        )
        .unwrap();
        assert_eq!(value["total_matching"], 2);
        let cursor = value["next_cursor"].as_str().unwrap();
        assert!(cursor.starts_with("txt2."));
        let second = raw_search(&graph, json!({"cursor":cursor}), content).unwrap();
        assert_eq!(second["pattern"], true);
        assert_eq!(second["ignore_case"], true);
        assert_ne!(
            value["hits"][0]["entity_id"],
            second["hits"][0]["entity_id"]
        );
        for flag in ["pattern", "ignore_case"] {
            let mut args = json!({"cursor":cursor});
            args[flag] = json!(false);
            assert!(raw_search(&graph, args, content)
                .unwrap_err()
                .to_string()
                .contains(flag));
        }
        let shortened = super::super::lexical::cursor_after_withheld(&value, 1).unwrap();
        assert_eq!(
            raw_search(&graph, json!({"cursor":shortened}), content).unwrap()["pattern"],
            true
        );
        add_artifact(&graph, "unrelated.md", "unrelated content");
        assert!(raw_search(&graph, json!({"cursor":cursor}), content)
            .unwrap_err()
            .to_string()
            .contains("workspace authority"));
    }

    #[test]
    fn text_includes_tests_by_default_and_regex_uses_the_actual_match_span() {
        use kin_model::graph::EntityStore;
        let graph = kin_db::InMemoryGraph::default();
        let text = "def check():\n    return 'Needle_123456'\n";
        crate::declaration_coverage::fixtures::admit(&graph, "scope.py", text, false);
        for mut entity in graph.query_entities(&EntityFilter::default()).unwrap() {
            entity.role = kin_model::EntityRole::Test;
            graph.upsert_entity(&entity).unwrap();
        }
        let args =
            json!({"literal":"needle_[0-9]+","held_text":true,"pattern":true,"ignore_case":true});
        let result = raw_search(&graph, args.clone(), &[text]).unwrap();
        assert_eq!(result["total_matching"], 1);
        assert_eq!(result["include_tests"], true);
        assert!(result["hits"][0]["excerpt"]
            .as_str()
            .unwrap()
            .contains("Needle_123456"));
        let mut exclude = args;
        exclude["include_tests"] = json!(false);
        assert_eq!(
            raw_search(&graph, exclude, &[text]).unwrap()["total_matching"],
            0
        );
    }

    #[test]
    fn a_pattern_crossing_two_entity_spans_has_no_fabricated_owner() {
        let graph = kin_db::InMemoryGraph::default();
        let text = "def left():\n    return 1\ndef right():\n    return 2\n";
        crate::declaration_coverage::fixtures::admit(&graph, "parts.py", text, false);
        let entities: Vec<_> = graph
            .query_entities(&EntityFilter::default())
            .unwrap()
            .into_iter()
            .filter(|entity| entity.kind == kin_model::EntityKind::Function)
            .collect();
        let split = text.find("def right").unwrap();
        let (rows, outside, _) = attribute(
            text.as_bytes(),
            std::slice::from_ref(&(split - 2..split + 4)),
            entities,
            &Budget::new(),
        )
        .unwrap();
        assert!(rows.is_empty());
        assert_eq!(outside, 1);
    }

    #[test]
    fn exact_and_regex_queries_preserve_spaces_and_language_is_not_ignored() {
        let graph = kin_db::InMemoryGraph::default();
        add_artifact(&graph, "a.txt", "marker");
        add_artifact(&graph, "b.txt", " marker ");
        let content = &["marker", " marker "];
        for pattern in [false, true] {
            let reply = raw_search(
                &graph,
                json!({"literal":" marker ","held_text":true,"pattern":pattern}),
                content,
            )
            .unwrap();
            assert_eq!(reply["total_matching"], 1);
            assert_eq!(reply["literal"], " marker ");
        }
        let error = raw_search(
            &graph,
            json!({"literal":"marker","held_text":true,"language":"python"}),
            content,
        )
        .unwrap_err();
        assert!(error.to_string().contains("language filtering"));
    }

    #[test]
    fn stale_span_provenance_and_partial_parse_certificate_remain_gaps() {
        let graph = kin_db::InMemoryGraph::default();
        let text = "def current():\n    return 'needle'\n";
        crate::declaration_coverage::fixtures::admit(&graph, "stale.py", text, true);
        let args = json!({"literal":"needle","held_text":true});
        let current = raw_search(&graph, args.clone(), &[text]).unwrap();
        assert_eq!(
            current["held_text_coverage"]["modules_with_unverified_parse"],
            1
        );
        for mut entity in graph.query_entities(&EntityFilter::default()).unwrap() {
            entity
                .metadata
                .extra
                .insert("blob_hash".into(), json!("11".repeat(32)));
            graph.upsert_entity(&entity).unwrap();
        }
        let stale = raw_search(&graph, args, &[text]).unwrap();
        assert_eq!(stale["total_matching"], 0);
        assert_eq!(
            stale["held_text_coverage"]["modules_without_complete_spans"],
            1
        );
        assert_eq!(
            stale["held_text_coverage"]["occurrences_outside_entities"],
            1
        );
    }

    /// Persist and reopen actual repository authority. No injected text reader
    /// participates in these regressions, so backend pre-allocation and hosted
    /// request allowances are exercised by the same route as a live lookup.
    pub(in crate::handlers) struct DurableTextFixture {
        _directory: tempfile::TempDir,
        pub(in crate::handlers) graph: kin_db::InMemoryGraph,
        backend: Arc<kin_db::LocalFileBackend>,
        repository_id: kin_model::RepositoryId,
        workspace_id: kin_model::WorkspaceId,
        change_id: kin_model::SemanticChangeId,
    }

    impl DurableTextFixture {
        fn new(units: &[(&str, Vec<u8>)]) -> Self {
            Self::with_entities(units, Vec::new())
        }

        pub(in crate::handlers) fn with_entities(
            units: &[(&str, Vec<u8>)],
            entities: Vec<Entity>,
        ) -> Self {
            use kin_model::graph::ChangeStore;
            let directory = tempfile::tempdir().unwrap();
            let repository_id = kin_model::RepositoryId::new("bounded-text-source-budget").unwrap();
            let workspace_id = kin_model::WorkspaceId::new();
            let backend = Arc::new(kin_db::LocalFileBackend::new(directory.path()));
            let manager = kin_db::RepositoryAuthorityManager::open(
                repository_id.clone(),
                Arc::clone(&backend),
            )
            .unwrap();
            let tree_deltas = units
                .iter()
                .map(|(path, bytes)| {
                    let hash = Hash256::from_bytes(kin_blobs::digest_bytes(bytes));
                    manager.save_source_blob(hash, bytes).unwrap();
                    kin_model::TreeDelta::Added {
                        artifact_id: kin_model::ArtifactId::new(),
                        new: kin_model::LocatedEntry::new(
                            kin_model::RepoPath::from_utf8(*path).unwrap(),
                            kin_model::TreeEntry::blob(hash, false),
                        ),
                    }
                })
                .collect::<Vec<_>>();
            let policy = kin_model::SharedAdmissionPolicy::empty(0);
            let mut change = kin_model::SemanticChange {
                id: kin_model::SemanticChangeId::from_hash(Hash256::from_bytes([0; 32])),
                parents: vec![],
                timestamp: kin_model::Timestamp::now(),
                author: kin_model::AuthorId::new("text-budget-test"),
                message: "admit held text allowance fixture".into(),
                entity_deltas: entities
                    .into_iter()
                    .map(|new| kin_model::EntityDelta::Added { new })
                    .collect(),
                relation_deltas: vec![],
                tree_deltas,
                admission_policy_delta: Some(kin_model::AdmissionPolicyDelta::initialize(
                    policy.clone(),
                )),
                projected_files: vec![],
                spec_link: None,
                evidence: vec![],
                risk_summary: None,
                origin: kin_model::ChangeOrigin::Native,
                external_reference_deltas: vec![],
                resolution_record_deltas: vec![],
            };
            change.id = kin_model::compute_semantic_change_id(&change).unwrap();
            let graph = kin_db::InMemoryGraph::new();
            graph
                .apply_transaction_delta(&kin_model::TransactionDelta {
                    entity_deltas: change.entity_deltas.clone(),
                    tree_deltas: change.tree_deltas.clone(),
                    ..Default::default()
                })
                .unwrap();
            graph.create_change(&change).unwrap();
            kin_core::initialize_repository_authority(
                &manager,
                repository_id.clone(),
                workspace_id,
                kin_model::AdmissionCase::Sensitive,
                kin_model::RefName::branch(b"main").unwrap(),
                policy,
                Some(change.clone()),
            )
            .unwrap();
            // The reader below must reopen durable state, not borrow this writer.
            drop(manager);
            Self {
                _directory: directory,
                graph,
                backend,
                repository_id,
                workspace_id,
                change_id: change.id,
            }
        }

        pub(in crate::handlers) fn local(
            &self,
        ) -> super::super::repository_authority::RequestRepositoryAuthority {
            super::super::repository_authority::RequestRepositoryAuthority::pinned(
                kin_core::LocalRepositoryAuthorityBinding::from_parts(
                    self.repository_id.clone(),
                    self.workspace_id,
                    Arc::clone(&self.backend),
                ),
            )
        }

        fn hosted(
            &self,
            max_blob_bytes: u64,
            total_bytes: u64,
            reads: usize,
        ) -> super::super::repository_authority::RequestRepositoryAuthority {
            use super::super::repository_authority::{
                ActiveRepositoryAuthority, RequestRepositoryAuthority,
            };
            use kin_db::VersionedAuthorityState;
            let backend: Arc<dyn kin_db::StorageBackend> = self.backend.clone();
            let manager = kin_db::RepositoryAuthorityManager::open(
                self.repository_id.clone(),
                Arc::clone(&backend),
            )
            .unwrap();
            let generation = manager.read_authority().generation();
            let tree = self.graph.resolved_tree_snapshot().unwrap().unwrap();
            let active = ActiveRepositoryAuthority::hosted(
                manager,
                backend,
                self.repository_id.clone(),
                self.change_id,
                tree,
                generation,
            )
            .unwrap();
            RequestRepositoryAuthority::hosted(Arc::new(active))
                .with_hosted_source_projection_budget(max_blob_bytes, total_bytes, reads)
        }

        fn search(
            &self,
            authority: &super::super::repository_authority::RequestRepositoryAuthority,
        ) -> crate::error::Result<crate::types::ToolCallResult> {
            let args = HashMap::from([
                ("literal".into(), json!("needle")),
                (super::super::lexical::HELD_TEXT_KEY.into(), json!(true)),
            ]);
            super::super::lexical::handle_lexical_lookup(&args, &self.graph, Some(authority))
        }
    }

    #[tokio::test]
    async fn unparsed_text_source_refuses_whole_body_and_edit_base() {
        for (name, content) in [
            ("070_profiles.sql", "CREATE TABLE secret_marker (id text);"),
            ("NOTES.md", "# Whole artifact\n\nsecret_marker\n"),
        ] {
            let fixture = DurableTextFixture::new(&[(name, content.as_bytes().to_vec())]);
            let authority = fixture.local();
            let config = crate::server::McpServerConfig {
                session_authority_mode: crate::server::SessionAuthorityMode::OfflineFallback,
                repository_authority: Some(fixture.local()),
                ..Default::default()
            };
            let sessions = crate::session::SessionRegistry::empty_for_test();
            // Pin the advertised initialize promise to the real persisted
            // source-serving path below, not only a constant-string test.
            let initialized = crate::server::process_message(
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
                &fixture.graph,
                &config,
                &sessions,
            )
            .await
            .unwrap()
            .result
            .unwrap();
            let contract =
                &initialized["capabilities"]["experimental"]["kin"]["semanticSourceContract"];
            assert_eq!(contract["schema"], "kin.semantic-source.v1");
            assert_eq!(contract["sourceUnit"], "parsed_entity");
            assert_eq!(contract["wholeFileReads"], false);
            assert_eq!(contract["unparsedTextSource"], "refused");
            for full_body in [false, true] {
                let args = HashMap::from([
                    ("entity_id".into(), json!(name)),
                    ("full_body".into(), json!(full_body)),
                ]);
                let reply = super::super::entities::handle_get_entity_source(
                    &args,
                    &fixture.graph,
                    Some(&authority),
                )
                .unwrap();
                assert_eq!(reply.is_error, Some(true));
                let delivered = serde_json::to_string(&reply).unwrap();
                assert!(
                    delivered.contains("unparsed_source_requires_semantic_coverage"),
                    "{delivered}"
                );
                assert!(!delivered.contains("secret_marker"), "{delivered}");
                assert!(!delivered.contains("source_base"), "{delivered}");
                // Exercise the actual JSON-RPC tool dispatch as well as the
                // shared source resolver, including the batched read root.
                for tool in ["get_entity_source", "get_entity_sources"] {
                    let arguments = if tool == "get_entity_source" {
                        json!({"entity_id": name, "full_body": full_body})
                    } else {
                        json!({"entity_ids": [name], "full_body": full_body})
                    };
                    let request = json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":tool,"arguments":arguments}});
                    let reply = crate::server::process_message(
                        &request.to_string(),
                        &fixture.graph,
                        &config,
                        &sessions,
                    )
                    .await
                    .unwrap();
                    let delivered = serde_json::to_string(&reply).unwrap();
                    let refused = delivered.contains("unparsed_source_requires_semantic_coverage")
                        || (tool == "get_entity_sources" && delivered.contains("not_found"));
                    assert!(refused, "{delivered}");
                    assert!(!delivered.contains("secret_marker"), "{delivered}");
                    assert!(!delivered.contains("source_base"), "{delivered}");
                }
            }
        }
    }

    #[test]
    fn durable_local_source_size_refusal_discards_earlier_text_matches() {
        let cap = super::super::repository_authority::HOSTED_SEMANTIC_SOURCE_BLOB_MAX_BYTES;
        let fixture = DurableTextFixture::new(&[
            ("a.txt", b"needle already matched".to_vec()),
            ("z.txt", vec![b'x'; cap as usize + 1]),
        ]);
        let error = fixture
            .search(&fixture.local())
            .expect_err("no earlier hits may survive the backend cap");
        assert!(
            matches!(error, McpError::SourceBudgetExhausted(_)),
            "{error}"
        );
        assert!(error.to_string().contains("per-blob allowance"));
    }

    #[test]
    fn durable_hosted_read_and_byte_allowances_refuse_the_whole_text_search() {
        let first = b"needle first".to_vec();
        let second = b"needle second".to_vec();
        let first_len = first.len() as u64;
        let total_len = first_len + second.len() as u64;
        let fixture = DurableTextFixture::new(&[("a.txt", first), ("z.txt", second)]);
        let control = fixture.search(&fixture.hosted(1024, total_len, 2)).unwrap();
        let crate::types::ContentBlock::Text { text } = &control.content[0];
        let payload: Value = serde_json::from_str(text).unwrap();
        assert_eq!(payload["total_matching"], 2);
        for authority in [
            fixture.hosted(1024, total_len, 1),
            fixture.hosted(1024, first_len, 2),
        ] {
            let error = fixture
                .search(&authority)
                .expect_err("an earlier match cannot hide spent source allowance");
            assert!(
                matches!(error, McpError::SourceBudgetExhausted(_)),
                "{error}"
            );
        }
    }

    #[test]
    fn durable_root_manifest_budget_failure_is_not_a_missing_namespace() {
        let fixture = DurableTextFixture::new(&[
            ("a.txt", b"needle".to_vec()),
            ("package.json", br#"{"name":"needle-project"}"#.to_vec()),
        ]);
        let error = fixture
            .search(&fixture.hosted(8, 1024, 4))
            .expect_err("namespace preparation must propagate the source budget refusal");
        assert!(
            matches!(error, McpError::SourceBudgetExhausted(_)),
            "{error}"
        );
        let control = fixture.search(&fixture.hosted(1024, 1024, 4)).unwrap();
        let crate::types::ContentBlock::Text { text } = &control.content[0];
        let payload: Value = serde_json::from_str(text).unwrap();
        assert_eq!(payload["total_matching"], 2);
        assert_eq!(payload["hits"][0]["module"], "needle-project");
    }
}
