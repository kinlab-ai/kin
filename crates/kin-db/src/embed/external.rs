// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The HTTP-only embedding adapter. Its dependency graph includes no tokenizer,
//! weight reader or local inference engine.

use super::*;

pub struct CodeEmbedder {
    backend: OpenAiCompatEmbedder,
    dimensions: usize,
    cache: Option<EmbeddingCache>,
    runtime: EmbeddingRuntimeConfig,
}

impl std::fmt::Debug for CodeEmbedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodeEmbedder")
            .field("dimensions", &self.dimensions)
            .finish()
    }
}

impl CodeEmbedder {
    pub fn new() -> Result<Self, KinDbError> {
        let profile = match configured_embedding_provider() {
            EmbeddingProvider::OpenAiCompat { profile } => profile,
            EmbeddingProvider::None => {
                return Err(KinDbError::IndexError(
                    "the embeddings model slot is none; graph retrieval remains available".into(),
                ))
            }
            EmbeddingProvider::Local => return Err(disabled_error()),
            EmbeddingProvider::Invalid(value) => {
                return Err(KinDbError::IndexError(format!(
                    "unknown embeddings model provider '{value}'"
                )))
            }
        };
        Self::with_config(OpenAiCompatConfig::from_profile(profile)?)
    }

    fn with_config(config: OpenAiCompatConfig) -> Result<Self, KinDbError> {
        let mut runtime = EmbeddingRuntimeConfig {
            provider: config.provider_identity(),
            model_id: config.model_id.clone(),
            revision: config.runtime_revision(),
            dimensions: config.dimensions,
            pipeline_epoch: EMBEDDING_CACHE_PIPELINE_EPOCH.into(),
        };
        let backend = OpenAiCompatEmbedder::new(config)?;
        let dimensions = backend.dimensions;
        runtime.dimensions = Some(dimensions);
        let cache = EmbeddingCache::new(backend.cache_namespace(), dimensions);
        Ok(Self {
            backend,
            dimensions,
            cache,
            runtime,
        })
    }

    pub fn with_model(_model: &str, _revision: &str) -> Result<Self, KinDbError> {
        Err(disabled_error())
    }
    pub fn dimensions(&self) -> usize {
        self.dimensions
    }
    pub fn uses_local_accelerator(&self) -> bool {
        false
    }
    pub(crate) fn runtime_identity(&self) -> Option<&EmbeddingRuntimeConfig> {
        Some(&self.runtime)
    }

    pub fn embed_entity(
        &self,
        name: &str,
        signature: &str,
        body: &str,
    ) -> Result<Vec<f32>, KinDbError> {
        let text = format_entity_text(name, signature, body);
        self.embed_batch(&[text])?
            .pop()
            .ok_or_else(|| KinDbError::IndexError("embedding returned no vector".into()))
    }

    pub fn embed_text(&self, text: &str) -> Result<Vec<f32>, KinDbError> {
        self.embed_query_batch(&[text.into()])?
            .pop()
            .ok_or_else(|| KinDbError::IndexError("embedding returned no vector".into()))
    }

    pub fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, KinDbError> {
        Ok(self.embed_batch_with_producers(texts)?.vectors)
    }
    pub fn embed_batch_with_producers(
        &self,
        texts: &[String],
    ) -> Result<ProducedEmbeddingBatch, KinDbError> {
        self.embed_batch_for_role(texts, EmbeddingInputRole::Document, &())
    }
    pub fn embed_batch_with_producers_observed(
        &self,
        texts: &[String],
        work: &dyn EmbeddingWork,
    ) -> Result<ProducedEmbeddingBatch, KinDbError> {
        self.embed_batch_for_role(texts, EmbeddingInputRole::Document, work)
    }
    pub fn embed_query_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, KinDbError> {
        Ok(self.embed_query_batch_with_producers(texts)?.vectors)
    }
    pub fn embed_query_batch_with_producers(
        &self,
        texts: &[String],
    ) -> Result<ProducedEmbeddingBatch, KinDbError> {
        self.embed_batch_for_role(texts, EmbeddingInputRole::Query, &())
    }

    fn embed_batch_for_role(
        &self,
        texts: &[String],
        role: EmbeddingInputRole,
        work: &dyn EmbeddingWork,
    ) -> Result<ProducedEmbeddingBatch, KinDbError> {
        work.checkpoint()?;
        if texts.is_empty() {
            return Ok(ProducedEmbeddingBatch::empty());
        }

        let prepared_texts = self.prepare_inputs(texts, role);
        let mut cached_results: Vec<Option<AttributedEmbedding>> = vec![None; texts.len()];
        // Cache-missing inputs are borrowed from `prepared_texts`, never copied —
        // the inference backend only needs to read them.
        let mut missing_texts: Vec<&str> = Vec::new();
        let mut missing_slots: Vec<Vec<usize>> = Vec::new();
        let mut missing_keys = Vec::new();

        if let Some(cache) = self.cache.as_ref() {
            let mut missing_by_key: HashMap<String, usize> = HashMap::new();
            for (idx, text) in prepared_texts.iter().enumerate() {
                let key = cache.key_for_text(text);
                if let Some(vector) = cache.get_by_key(&key) {
                    cached_results[idx] = Some(vector);
                    continue;
                }

                match missing_by_key.get(&key).copied() {
                    Some(miss_idx) => missing_slots[miss_idx].push(idx),
                    None => {
                        missing_by_key.insert(key.clone(), missing_texts.len());
                        missing_keys.push(key);
                        missing_texts.push(text.as_str());
                        missing_slots.push(vec![idx]);
                    }
                }
            }
        } else {
            for (idx, text) in prepared_texts.iter().enumerate() {
                missing_texts.push(text.as_str());
                missing_slots.push(vec![idx]);
            }
        }

        if missing_texts.is_empty() {
            let attributed = cached_results
                .into_iter()
                .map(|embedding| {
                    embedding.ok_or_else(|| {
                        KinDbError::IndexError("embedding cache returned incomplete batch".into())
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            work.checkpoint()?;
            return Ok(produced_batch_from_attributed(attributed));
        }

        let missing_results = self.embed_uncached_batch(&missing_texts, work)?;
        if missing_results.len() != missing_texts.len() {
            return Err(KinDbError::IndexError(format!(
                "embedding endpoint returned {} vectors for {} inputs",
                missing_results.len(),
                missing_texts.len()
            )));
        }

        for (miss_idx, attributed) in missing_results.into_iter().enumerate() {
            let embedding = sanitize_attributed_embedding(attributed, self.dimensions, || {
                missing_texts.get(miss_idx).copied().unwrap_or("")
            });
            if let Some(cache) = self.cache.as_ref() {
                if let Some(key) = missing_keys.get(miss_idx) {
                    cache.put_by_key(key, &embedding.vector, &embedding.producers);
                }
            }

            fanout_attributed_embedding(&mut cached_results, &missing_slots[miss_idx], &embedding)?;
        }

        let attributed = cached_results
            .into_iter()
            .map(|embedding| {
                embedding.ok_or_else(|| {
                    KinDbError::IndexError("embedding batch result order mismatch".into())
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        work.checkpoint()?;
        Ok(produced_batch_from_attributed(attributed))
    }

    fn prepare_inputs<'a>(
        &self,
        texts: &'a [String],
        role: EmbeddingInputRole,
    ) -> Cow<'a, [String]> {
        self.backend.prepare_inputs(texts, role)
    }
    fn embed_uncached_batch(
        &self,
        texts: &[&str],
        work: &dyn EmbeddingWork,
    ) -> Result<Vec<AttributedEmbedding>, KinDbError> {
        Ok(observe_completed_chunk(
            work,
            texts.len(),
            || self.backend.embed_batch(texts),
            |vector| vector.iter().all(|value| value.is_finite()),
        )?
        .into_iter()
        .map(AttributedEmbedding::remote)
        .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_only_response_validation_keeps_input_order_and_rejects_malformed_rows() {
        let vectors = parse_openai_embedding_response(
            r#"{"data":[{"index":1,"embedding":[2.0,1.0]},{"index":0,"embedding":[1.0,2.0]}]}"#,
            2,
        )
        .unwrap();
        assert_eq!(vectors, vec![vec![1.0, 2.0], vec![2.0, 1.0]]);
        assert!(
            parse_openai_embedding_response(r#"{"data":[{"index":2,"embedding":[1.0]}]}"#, 1)
                .is_err()
        );
        assert!(parse_openai_embedding_response(
            r#"{"data":[{"index":0,"embedding":[1.0]},{"index":0,"embedding":[1.0]}]}"#,
            2
        )
        .is_err());
    }

    #[test]
    fn model_slots_external_embedding_request_is_remote_and_preserves_role_prefix() {
        use std::io::{BufRead, Read};
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
            assert!(line.starts_with("POST /v1/embeddings "));
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
            assert_eq!(request["model"], "mock-embedding");
            assert_eq!(
                request["input"],
                serde_json::json!(["query: graph question"])
            );
            let body = r#"{"data":[{"index":0,"embedding":[0.2,0.8]}]}"#;
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
        });
        let mut embedder = CodeEmbedder::with_config(OpenAiCompatConfig {
            provider_label: "openai-compatible".into(),
            base_url: format!("http://{address}/v1"),
            model_id: "mock-embedding".into(),
            api_key: None,
            dimensions: Some(2),
            send_dimensions: false,
            timeout_secs: 5,
            request_overrides: serde_json::Map::new(),
            query_prefix: "query: ".into(),
            document_prefix: "document: ".into(),
        })
        .unwrap();
        embedder.cache = None;
        let batch = embedder
            .embed_query_batch_with_producers(&["graph question".into()])
            .unwrap();
        assert_eq!(batch.vectors, vec![vec![0.2, 0.8]]);
        assert_eq!(
            batch.producers,
            EmbeddingProducerSet::singleton(EmbeddingProducer::Remote)
        );
        assert_eq!(
            embedder.runtime_identity().unwrap().model_id,
            "mock-embedding"
        );
        assert!(!embedder.uses_local_accelerator());
        server.join().unwrap();
    }
}
