// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! External reranking over candidates already admitted by the graph.

use kin_core::models::{
    observe_graph_selection, ModelProvider, ModelSlot, ModelSlotConfig, ModelUse,
};
use serde::Deserialize;

use crate::commands::locate::scorer::{
    CandidateScorer, ScorerModel, ScorerUnavailable, ScoringCandidate,
};

pub struct ExternalReranker {
    config: ModelSlotConfig,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GraphRanking {
    /// Original candidate indices, in the endpoint's selected order.
    pub rows: Vec<(usize, f64)>,
    pub observed: ModelUse,
}

#[derive(Deserialize)]
struct Response {
    results: Vec<Row>,
}

#[derive(Deserialize)]
struct Row {
    index: usize,
    relevance_score: f64,
}

fn unavailable(detail: impl Into<String>) -> ScorerUnavailable {
    ScorerUnavailable {
        reason: "external_rerank_failed",
        detail: detail.into(),
        remediation: "verify the configured reranking endpoint, or select the none provider".into(),
    }
}

impl ExternalReranker {
    pub fn new(config: ModelSlotConfig) -> Result<Self, ScorerUnavailable> {
        config
            .validate()
            .map_err(|error| unavailable(error.to_string()))?;
        if config.provider != ModelProvider::External {
            return Err(unavailable(
                "external reranking requires the external provider",
            ));
        }
        Ok(Self { config })
    }

    pub fn model(&self) -> &str {
        self.config
            .model
            .as_deref()
            .expect("validated external identity")
    }

    pub fn rank(
        &self,
        query: &str,
        candidates: &[ScoringCandidate],
    ) -> Result<GraphRanking, ScorerUnavailable> {
        if candidates.is_empty() {
            return decode_response(&self.config, candidates, r#"{"results":[]}"#);
        }
        let base = self
            .config
            .base_url
            .as_deref()
            .expect("validated external endpoint")
            .trim_end_matches('/');
        let endpoint = if base.ends_with("/rerank") {
            base.to_string()
        } else {
            format!("{base}/rerank")
        };
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .map_err(|_| unavailable("the reranking HTTP client could not be created"))?;
        let documents: Vec<_> = candidates
            .iter()
            .map(|candidate| candidate.text.as_str())
            .collect();
        let mut request = client.post(endpoint).json(&serde_json::json!({
            "model": self.model(), "query": query, "documents": documents,
            "top_n": candidates.len(), "return_documents": false,
        }));
        if let Some(name) = &self.config.api_key_env {
            let key = std::env::var(name)
                .ok()
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| {
                    unavailable(format!("reranking credential variable {name} is unset"))
                })?;
            request = request.bearer_auth(key);
        }
        let response = request
            .send()
            .map_err(|_| unavailable("the reranking request failed"))?;
        if !response.status().is_success() {
            return Err(unavailable(format!(
                "the reranking endpoint returned HTTP {}",
                response.status()
            )));
        }
        let text = response
            .text()
            .map_err(|_| unavailable("the reranking response could not be read"))?;
        decode_response(&self.config, candidates, &text)
    }
}

/// A transport response can refer only to an index in the supplied candidate
/// list. Endpoint-supplied documents, entity ids and absence claims are ignored.
fn decode_response(
    config: &ModelSlotConfig,
    candidates: &[ScoringCandidate],
    text: &str,
) -> Result<GraphRanking, ScorerUnavailable> {
    let response: Response =
        serde_json::from_str(text).map_err(|_| unavailable("invalid reranking response"))?;
    let ids: Vec<_> = candidates
        .iter()
        .enumerate()
        .map(|(index, _)| index.to_string())
        .collect();
    let selected: Vec<_> = response
        .results
        .iter()
        .map(|row| (row.index.to_string(), row.relevance_score))
        .collect();
    if response
        .results
        .iter()
        .any(|row| !(0.0..=1.0).contains(&row.relevance_score))
    {
        return Err(unavailable(
            "reranking relevance scores must be finite probabilities in [0, 1]",
        ));
    }
    let observed =
        observe_graph_selection(config, ModelSlot::Reranking, &ids, &selected).map_err(|_| {
            unavailable("reranking returned duplicate or unknown graph candidate indices")
        })?;
    Ok(GraphRanking {
        rows: response
            .results
            .into_iter()
            .map(|row| (row.index, row.relevance_score))
            .collect(),
        observed,
    })
}

impl CandidateScorer for ExternalReranker {
    fn name(&self) -> &'static str {
        "external"
    }

    fn provider(&self) -> ModelProvider {
        ModelProvider::External
    }

    fn model(&self) -> Option<ScorerModel> {
        Some(ScorerModel {
            id: self.model().to_string(),
            revision: None,
            device: Some("external".into()),
        })
    }

    fn score(
        &self,
        query: &str,
        candidates: &[ScoringCandidate],
    ) -> Result<Vec<Option<f32>>, ScorerUnavailable> {
        let ranked = self.rank(query, candidates)?;
        // The current scorer stage retains every graph row. A partial endpoint
        // response cannot silently turn an unscored row into a selected answer.
        if ranked.observed.filtered_out > 0 {
            return Err(unavailable(format!("the endpoint withheld {} graph candidates; this scorer stage requires a score for every supplied row", ranked.observed.filtered_out)));
        }
        let mut scores = vec![None; candidates.len()];
        for (index, score) in ranked.rows {
            scores[index] = Some(score as f32);
        }
        Ok(scores)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> ModelSlotConfig {
        ModelSlotConfig {
            provider: ModelProvider::External,
            model: Some("test-reranker".into()),
            base_url: Some("http://localhost:1/v1".into()),
            api_key_env: None,
        }
    }
    fn candidates() -> Vec<ScoringCandidate> {
        ["graph-a", "graph-b"]
            .into_iter()
            .map(|id| ScoringCandidate {
                entity_id: id.into(),
                name: id.into(),
                kind: "function".into(),
                file: None,
                text: format!("graph body {id}"),
            })
            .collect()
    }
    #[test]
    fn rerank_uses_only_original_indices_and_discloses_filtering() {
        let ranked = decode_response(
            &settings(),
            &candidates(),
            r#"{"results":[{"index":1,"relevance_score":0.9,"document":{"text":"invented"}}]}"#,
        )
        .unwrap();
        assert_eq!(ranked.rows, vec![(1, 0.9)]);
        assert_eq!(
            (
                ranked.observed.candidates,
                ranked.observed.returned,
                ranked.observed.filtered_out
            ),
            (2, 1, 1)
        );
        assert_eq!(ranked.observed.model.as_deref(), Some("test-reranker"));
    }
    #[test]
    fn malformed_duplicate_out_of_range_or_unbounded_results_are_refused() {
        for text in [
            r#"{"results":[{"index":2,"relevance_score":0.9}]}"#,
            r#"{"results":[{"index":0,"relevance_score":0.9},{"index":0,"relevance_score":0.8}]}"#,
            r#"{"results":[{"index":0,"relevance_score":1.1}]}"#,
            r#"{"results":[{"index":0,"relevance_score":null}]}"#,
        ] {
            assert!(decode_response(&settings(), &candidates(), text).is_err());
        }
    }

    #[test]
    fn model_slots_external_rerank_posts_graph_candidates_and_restores_input_indices() {
        use std::io::{BufRead, Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert!(line.starts_with("POST /v1/rerank "));
            let mut length = 0;
            loop {
                line.clear();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap();
                }
            }
            let mut bytes = vec![0; length];
            reader.read_exact(&mut bytes).unwrap();
            let request: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(request["model"], "test-reranker");
            assert_eq!(
                request["documents"],
                serde_json::json!(["graph body graph-a", "graph body graph-b"])
            );
            assert_eq!(request["top_n"], 2);
            let body = r#"{"results":[{"index":1,"relevance_score":0.9},{"index":0,"relevance_score":0.3}]}"#;
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
        });
        let mut config = settings();
        config.base_url = Some(format!("http://{address}/v1"));
        let reranker = ExternalReranker::new(config).unwrap();
        assert_eq!(
            reranker.score("graph question", &candidates()).unwrap(),
            vec![Some(0.3), Some(0.9)]
        );
        server.join().unwrap();
    }
}
