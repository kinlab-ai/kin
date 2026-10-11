// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

#![cfg(feature = "embeddings")]

//! Kin's served vectors match each model card's own reference pipeline.
//!
//! The reference numbers below were produced on the CPU by
//! sentence-transformers 6.1.0 with transformers 5.17.0 and torch 2.14.0, from
//! the same published weights, exactly as each card says to serve its model:
//!
//! - nomic-embed-text-v1.5: `search_query: ` on queries, `search_document: ` on
//!   documents, mean pooling (`1_Pooling/config.json`), then
//!   `F.normalize(p=2)` at the full 768 dimensions, with no Matryoshka layer
//!   norm.
//! - CodeRankEmbed: its code instruction on queries, raw documents, CLS
//!   pooling, then L2 normalization.
//!
//! The negative control is the default model as Kin used to serve it, with CLS
//! pooling and CodeRankEmbed's query instruction. It sits well outside the
//! tolerance, so a return to that serving fails here.

use kin_db::CodeEmbedder;

const QUERIES: [&str; 3] = [
    "register a function to run before each request",
    "load configuration values from a python file",
    "render a template with the given context",
];

const DOCUMENTS: [&str; 3] = [
    "def before_request(self, f):\n    \"\"\"Register a function to run before each request.\"\"\"",
    "def from_pyfile(self, filename, silent=False):\n    \"\"\"Updates the values in the config from a Python file.\"\"\"",
    "def render_template(template_name_or_list, **context):\n    \"\"\"Render a template by name with the given context.\"\"\"",
];

struct Reference {
    model_id: &'static str,
    /// Cosine similarity of each query (row) to each document (column).
    similarities: [[f32; 3]; 3],
    first_query_head: [f32; 8],
    first_document_head: [f32; 8],
}

const NOMIC_EMBED_TEXT_V1_5: Reference = Reference {
    model_id: "nomic-ai/nomic-embed-text-v1.5",
    similarities: [
        [0.770975, 0.532297, 0.478734],
        [0.480656, 0.676932, 0.449496],
        [0.522851, 0.467994, 0.836646],
    ],
    first_query_head: [
        -0.032025, 0.032884, -0.203865, -0.02921, 0.060878, -0.049546, 0.057516, 0.030947,
    ],
    first_document_head: [
        0.012191, 0.052979, -0.170872, -0.02925, 0.059429, -0.017597, 0.055981, 0.002387,
    ],
};

/// nomic-embed-text-v1.5 served the old way: CLS pooling, and CodeRankEmbed's
/// query instruction on queries with raw documents.
const NOMIC_EMBED_TEXT_V1_5_OLD_SERVING: [[f32; 3]; 3] = [
    [0.730654, 0.550337, 0.612905],
    [0.60757, 0.662987, 0.590657],
    [0.624287, 0.532743, 0.789488],
];

const CODERANKEMBED: Reference = Reference {
    model_id: "nomic-ai/CodeRankEmbed",
    similarities: [
        [0.861921, -0.001189, 0.034653],
        [0.059526, 0.655594, -0.047336],
        [0.027325, -0.042246, 0.667309],
    ],
    first_query_head: [
        -0.036288, 0.042576, -0.005769, 0.00857, 0.029, -0.036124, -0.027755, -0.025136,
    ],
    first_document_head: [
        -0.022215, 0.023383, 0.008349, 0.016365, 0.02764, -0.010566, 0.021612, -0.016893,
    ],
};

/// CPU float order differs between the two runtimes; served the same way,
/// they agree to well inside this.
const TOLERANCE: f32 = 2e-3;

fn served_similarities(embedder: &CodeEmbedder) -> (Vec<Vec<f32>>, Vec<Vec<f32>>, [[f32; 3]; 3]) {
    let queries = embedder
        .embed_query_batch(&QUERIES.map(str::to_string))
        .unwrap();
    let documents = embedder
        .embed_batch(&DOCUMENTS.map(str::to_string))
        .unwrap();
    let mut similarities = [[0.0f32; 3]; 3];
    for (row, query) in queries.iter().enumerate() {
        for (column, document) in documents.iter().enumerate() {
            similarities[row][column] = query.iter().zip(document).map(|(a, b)| a * b).sum();
        }
    }
    (queries, documents, similarities)
}

fn assert_close(label: &str, served: &[f32], reference: &[f32]) {
    for (index, (served, reference)) in served.iter().zip(reference).enumerate() {
        assert!(
            (served - reference).abs() <= TOLERANCE,
            "{label}[{index}]: served {served}, card reference {reference}"
        );
    }
}

fn check(reference: &Reference) -> [[f32; 3]; 3] {
    let embedder = CodeEmbedder::with_model(reference.model_id, "main").unwrap();
    assert!(
        !embedder.uses_local_accelerator(),
        "the references are CPU vectors"
    );
    let (queries, documents, similarities) = served_similarities(&embedder);
    for (row, expected) in reference.similarities.iter().enumerate() {
        assert_close(
            &format!("{} similarities row {row}", reference.model_id),
            &similarities[row],
            expected,
        );
    }
    assert_close(
        &format!("{} first query", reference.model_id),
        &queries[0][..8],
        &reference.first_query_head,
    );
    assert_close(
        &format!("{} first document", reference.model_id),
        &documents[0][..8],
        &reference.first_document_head,
    );
    similarities
}

/// Whether the Hugging Face cache already holds a model, so a test of a model
/// Kin does not use by default never downloads one.
fn cached(repo: &str) -> bool {
    let Some(hub) = std::env::var_os("HF_HOME")
        .map(|home| std::path::PathBuf::from(home).join("hub"))
        .or_else(|| {
            std::env::var_os("HOME")
                .map(|home| std::path::PathBuf::from(home).join(".cache/huggingface/hub"))
        })
    else {
        return false;
    };
    let repo = hub.join(format!("models--{}", repo.replace('/', "--")));
    let Ok(revision) = std::fs::read_to_string(repo.join("refs/main")) else {
        return false;
    };
    let snapshot = repo.join("snapshots").join(revision.trim());
    ["config.json", "tokenizer.json", "model.safetensors"]
        .iter()
        .all(|name| snapshot.join(name).is_file())
}

/// One test, so the environment it sets is set before any embedder in this
/// binary reads it.
#[test]
fn served_vectors_match_each_models_card_reference() {
    // The CPU backend, which the references were produced on, and no shared
    // text cache, so every vector here is computed now.
    std::env::set_var("KIN_INFER_FORCE_CPU", "1");
    std::env::set_var("KIN_EMBED_BACKEND", "cpu");
    std::env::set_var("KIN_EMBED_CACHE", "0");

    let served = check(&NOMIC_EMBED_TEXT_V1_5);
    let largest_gap = served
        .iter()
        .flatten()
        .zip(NOMIC_EMBED_TEXT_V1_5_OLD_SERVING.iter().flatten())
        .map(|(served, old)| (served - old).abs())
        .fold(0.0f32, f32::max);
    assert!(
        largest_gap > 10.0 * TOLERANCE,
        "the old serving must be distinguishable here, largest gap {largest_gap}"
    );

    if cached(CODERANKEMBED.model_id) {
        check(&CODERANKEMBED);
    } else {
        eprintln!("skipped CodeRankEmbed: not in the Hugging Face cache");
    }
}
