// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Cold async routes must perform external embedding IO on a blocking worker.

use super::*;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener as FixtureListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

const MODEL: &str = "fixture-producer-endpoint";
const QUERY: &str = "Apply a percentage rebate to a customer's final payable amount";

struct EmbeddingEndpoint {
    base_url: String,
    requests: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl EmbeddingEndpoint {
    fn start() -> Self {
        let listener = FixtureListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&requests);
        let stopped = Arc::clone(&stop);
        let worker = std::thread::spawn(move || {
            while !stopped.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => serve_embeddings(stream, &observed).unwrap(),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("fixture endpoint failed: {error}"),
                }
            }
        });
        Self {
            base_url: format!("http://127.0.0.1:{port}/v1"),
            requests,
            stop,
            worker: Some(worker),
        }
    }
}

impl Drop for EmbeddingEndpoint {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}

fn serve_embeddings(mut stream: TcpStream, requests: &AtomicUsize) -> std::io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut content_length = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(());
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = Some(value.trim().parse::<usize>().unwrap());
        }
    }
    let mut body = vec![0; content_length.expect("the embedding request has a JSON body")];
    reader.read_exact(&mut body)?;
    let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(request["model"], MODEL);
    let inputs = request["input"].as_array().unwrap();
    assert!(!inputs.is_empty());
    let data: Vec<_> = inputs
        .iter()
        .enumerate()
        .map(|(index, _)| json!({"index": index, "embedding": [1.0, 0.0, 0.0, 0.0]}))
        .collect();
    let payload = json!({"data": data}).to_string();
    // Count before answering, so observing a response also observes its request.
    requests.fetch_add(1, Ordering::Relaxed);
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    )?;
    stream.flush()
}

fn cold_indexed_state() -> (Arc<DaemonState>, Entity) {
    let initial = test_state();
    let layout = initial.layout.clone();
    install_repository_file(
        &initial,
        "src/billing.py",
        b"def calculate_total(prices, discount):\n    return sum(prices) * (1 - discount)\n",
    );
    // Reopen from the real published authority, as committed_test_state does,
    // so HEAD and its semantic history are present in the active projection.
    drop(initial);
    let state = Arc::new(DaemonState::open(layout).unwrap());
    let entity = derived_entity(
        &state,
        "src/billing.py",
        "calculate_total",
        EntityKind::Function,
    );
    // Install persisted vectors directly. No embedder is constructed to seed
    // this index, so the first async request must initialize its HTTP client.
    let descriptor = kin_db::IndexDescriptor {
        model_id: Some(MODEL.to_string()),
        graph_root: Some(hex::encode(state.graph.compute_root_hash())),
    };
    let index = kin_db::VectorIndex::new(4).unwrap();
    index.set_descriptor(descriptor.clone());
    index
        .upsert_retrievable_with_producers(
            entity.id.into(),
            &[1.0, 0.0, 0.0, 0.0],
            &kin_db::EmbeddingProducerSet::singleton(kin_db::EmbeddingProducer::Remote),
        )
        .unwrap();
    let path = state.layout.kindb_vector_index_path();
    index.save(&path).unwrap();
    assert!(matches!(
        state.graph.load_vector_index_compatible(&path, &descriptor),
        kin_db::VectorIndexLoad::Loaded(1)
    ));
    assert_eq!(state.graph.embedding_status().indexed, 1);
    assert!(!state.graph.release_embedder(), "the fixture must be cold");
    state.is_initialized.store(true, Ordering::Relaxed);
    (state, entity)
}

async fn post_json(app: Router, path: &str, body: serde_json::Value) -> serde_json::Value {
    let response = app
        .oneshot(
            Request::post(path)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 256 * 1024)
        .await
        .unwrap();
    assert_eq!(status, StatusCode::OK, "{path}: {bytes:?}");
    serde_json::from_slice(&bytes).unwrap()
}

/// Synthetic vectors isolate HTTP transport and async dispatch from model
/// accuracy. Each route starts with its own cold embedder and persisted index.
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial]
async fn cold_external_provider_queries_answer_through_async_routes() {
    let _lock = env_test_lock();
    let endpoint = EmbeddingEndpoint::start();
    let _environment = kin_core::test_env::EnvVarGuard::new()
        .with("KIN_MODEL_EMBEDDINGS_PROVIDER", "external")
        .with("KIN_MODEL_EMBEDDINGS_BASE_URL", &endpoint.base_url)
        .with("KIN_MODEL_EMBEDDINGS_MODEL", MODEL)
        .with("KIN_EMBED_OPENAI_DIMENSIONS", "4")
        .with("KIN_EMBED_OPENAI_TIMEOUT_SECS", "5")
        .with("KIN_EMBED_CACHE", "0")
        .without("KIN_MODEL_EMBEDDINGS_API_KEY_ENV")
        .without("KIN_EMBED_OPENAI_API_KEY")
        .without("KIN_EMBED_OPENAI_REQUEST_JSON")
        .without("KIN_EMBED_OPENAI_QUERY_PREFIX")
        .without("KIN_EMBED_OPENAI_DOCUMENT_PREFIX")
        .without("KIN_REQUIRE_COMPLETE_EMBEDDINGS")
        .without("KIN_LOCATE_ARM")
        .without("KIN_LOCATE_SCORER")
        .without("KIN_LOCATE_CROSS_ENCODER_ENABLED")
        .without("KIN_DAEMON_AUTH_TOKEN")
        .without("KIN_DAEMON_REQUIRE_TOKEN");

    for surface in [
        "fused",
        "cosine",
        "search",
        "reference",
        "context",
        "context_pack",
    ] {
        let requests_before = endpoint.requests.load(Ordering::Relaxed);
        let (state, entity) = cold_indexed_state();
        assert_eq!(endpoint.requests.load(Ordering::Relaxed), requests_before);
        let app = router(Arc::clone(&state));
        let id = entity.id.to_string();
        match surface {
            "fused" => {
                let answer = call_semantic_locate(
                    app.clone(),
                    json!({"query": QUERY, "explain": true, "include_snippet": false,
                           "surface": "full", "limit": 5}),
                )
                .await;
                assert_eq!(answer["routing"], "fused-v1");
                assert_ne!(answer["retrieval_arm"], "cards");
                let row = answer["entities"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|row| row["entity_id"] == id)
                    .unwrap_or_else(|| panic!("the indexed entity must rank: {answer}"));
                assert!(row["provenance"]["cosine"].as_f64().unwrap() > 0.0);
                assert_ne!(answer["semantic_coverage"]["embedding_state"], "not_used");
            }
            "cosine" => {
                let answer = call_semantic_locate(
                    app.clone(),
                    json!({"query": QUERY, "pipeline": "cosine", "include_snippet": false,
                           "limit": 5}),
                )
                .await;
                assert_eq!(answer["routing"], "cosine-v0");
                let row = answer["results"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|row| row["entity_id"] == id)
                    .unwrap_or_else(|| panic!("the indexed entity must rank: {answer}"));
                assert_eq!(row["match_kind"], "semantic");
                assert!(row["score"].as_f64().unwrap() > 0.0);
            }
            "search" => {
                let answer = post_json(
                    app.clone(),
                    "/search",
                    json!({"query": QUERY, "semantic": true, "limit": 5}),
                )
                .await;
                assert_eq!(answer["semantic"], true);
                assert!(
                    answer["records"].as_array().unwrap().iter().any(|row| {
                        row["id"] == id && row["score"].as_f64().is_some_and(|score| score > 0.0)
                    }),
                    "{answer}"
                );
            }
            "context" => {
                let answer = post_json(
                    app.clone(),
                    "/context",
                    json!({"question": QUERY, "budget": "8k", "max_focals": 1}),
                )
                .await;
                assert!(answer["error"].is_null(), "{answer}");
                assert!(
                    answer["focals"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|row| row["id"] == id),
                    "{answer}"
                );
                assert!(
                    answer["pack"]["focal_entities"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|row| {
                            row["content"]
                                .as_str()
                                .is_some_and(|body| body.contains("calculate_total"))
                        }),
                    "{answer}"
                );
            }
            "reference" => {
                let answer = post_json(
                    app.clone(),
                    "/locate",
                    json!({"text": QUERY, "reference": "HEAD", "explain": true,
                           "max_files": 5, "max_files_explicit": true, "entity_surface": true}),
                )
                .await;
                let row = answer["entities"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|row| row["entity_id"] == id)
                    .unwrap_or_else(|| panic!("the indexed HEAD entity must rank: {answer}"));
                assert!(row["provenance"]["cosine"].as_f64().unwrap() > 0.0);
            }
            "context_pack" => {
                let answer = call_mcp_tool(
                    app.clone(),
                    "get_context_pack",
                    json!({"question": QUERY, "max_focals": 1, "token_budget": 2000}),
                )
                .await;
                assert!(
                    answer["focals"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|row| row["entity_id"] == id),
                    "{answer}"
                );
                assert!(
                    answer["entities"].as_array().unwrap().iter().any(|row| {
                        row["id"] == id
                            && row["section"] == "focal"
                            && row["body"]
                                .as_str()
                                .is_some_and(|body| body.contains("calculate_total"))
                    }),
                    "{answer}"
                );
            }
            _ => unreachable!(),
        }
        assert!(
            endpoint.requests.load(Ordering::Relaxed) > requests_before,
            "{surface} must call the provider"
        );
        let health = app
            .clone()
            .oneshot(Request::get("/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(health.status(), StatusCode::OK, "{surface}");
        drop(app);
        // The graph owns a blocking client after the query, so its final drop
        // belongs on the blocking worker too.
        tokio::task::spawn_blocking(move || drop(state))
            .await
            .unwrap();
    }
}
