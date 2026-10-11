// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Probe: what's the maximum relationship data RA can give us?

use std::time::Duration;

use kin_lsp::adapters::rust_analyzer::{cargo_layout, launch_for, RustAnalyzerAdapter};
use kin_lsp::adapters::LspAdapter;
use kin_lsp::lifecycle::LspServer;
use kin_lsp::protocol;

/// How long rust-analyzer is given to load the probe crate, and then to answer
/// about its one document. Loading takes about three seconds, most of it the
/// standard library; the bound is for a loaded host.
const READY_BUDGET: Duration = Duration::from_secs(60);

fn has_rust_analyzer() -> bool {
    which::which("rust-analyzer").is_ok()
        && std::process::Command::new("rust-analyzer")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
}

#[tokio::test]
async fn probe_all_lsp_endpoints() {
    if !has_rust_analyzer() {
        eprintln!("SKIP");
        return;
    }

    let workspace = std::env::temp_dir().join(format!("lsp-max-probe-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&workspace);
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    std::fs::write(
        workspace.join("Cargo.toml"),
        b"[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();

    let src = r#"struct Config { name: String }

impl Config {
    fn new(name: &str) -> Self { Config { name: name.to_string() } }
    fn name(&self) -> &str { &self.name }
}

fn process(config: &Config) -> String { config.name().to_uppercase() }
fn helper() -> Config { Config::new("test") }
fn main() {
    let cfg = helper();
    println!("{}", process(&cfg));
}
"#;
    std::fs::write(workspace.join("src/main.rs"), src).unwrap();

    // Started the way the daemon starts it, which waits for rust-analyzer to
    // report the crate loaded, and then waited on until it reports itself
    // indexed. This was a fixed 25 s sleep after a bare start.
    let adapter = RustAnalyzerAdapter;
    let server = tokio::time::timeout(
        READY_BUDGET,
        LspServer::launch_settled(
            adapter.server_command(),
            &[],
            &workspace,
            &launch_for(cargo_layout(&workspace)),
            None,
        ),
    )
    .await
    .expect("rust-analyzer reports the probe crate loaded within the budget")
    .unwrap();
    let indexed = server.wait_until_indexed(READY_BUDGET).await;
    assert_eq!(
        indexed,
        kin_lsp::lifecycle::Indexed::Quiescent,
        "rust-analyzer says when it has finished loading, and the wait ends on that"
    );

    let uri = protocol::path_to_uri(&workspace.join("src/main.rs"));
    server
        .client
        .notify(
            "textDocument/didOpen",
            serde_json::json!({
                "textDocument": { "uri": uri, "languageId": "rust", "version": 1, "text": src }
            }),
        )
        .await
        .unwrap();
    // Until rust-analyzer answers about the document it may not have read it.
    // This was a fixed 5 s sleep.
    server
        .wait_for_document(&uri, READY_BUDGET)
        .await
        .expect("rust-analyzer answers about a document it was just handed");

    // === 1. documentSymbol ===
    eprintln!("\n=== documentSymbol ===");
    let symbols = server
        .client
        .request(
            "textDocument/documentSymbol",
            serde_json::json!({"textDocument": {"uri": uri}}),
        )
        .await;
    let symbol_count = match &symbols {
        Ok(v) => {
            fn count_symbols(v: &serde_json::Value) -> usize {
                let mut n = 0;
                if let Some(arr) = v.as_array() {
                    for s in arr {
                        n += 1;
                        if let Some(children) = s.get("children") {
                            n += count_symbols(children);
                        }
                    }
                }
                n
            }
            let n = count_symbols(v);
            eprintln!("  {} total symbols (including nested)", n);
            n
        }
        Err(e) => {
            eprintln!("  error: {}", e);
            0
        }
    };

    // === 2. semanticTokens/full ===
    eprintln!("\n=== semanticTokens/full ===");
    let tokens = server
        .client
        .request(
            "textDocument/semanticTokens/full",
            serde_json::json!({"textDocument": {"uri": uri}}),
        )
        .await;
    let token_count = match &tokens {
        Ok(v) => {
            let data = v
                .get("data")
                .and_then(|d| d.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            let tc = data / 5; // 5 values per token
            eprintln!("  {} semantic tokens", tc);
            tc
        }
        Err(e) => {
            eprintln!("  error: {}", e);
            0
        }
    };

    // === 3. references for every symbol position ===
    eprintln!("\n=== textDocument/references (per symbol) ===");
    // Parse documentSymbol to get all positions
    let mut positions: Vec<(String, u32, u32)> = Vec::new();
    if let Ok(v) = &symbols {
        fn collect_positions(v: &serde_json::Value, positions: &mut Vec<(String, u32, u32)>) {
            if let Some(arr) = v.as_array() {
                for s in arr {
                    if let (Some(name), Some(range)) = (s["name"].as_str(), s.get("selectionRange"))
                    {
                        let line = range["start"]["line"].as_u64().unwrap_or(0) as u32;
                        let col = range["start"]["character"].as_u64().unwrap_or(0) as u32;
                        positions.push((name.to_string(), line, col));
                    }
                    if let Some(children) = s.get("children") {
                        collect_positions(children, positions);
                    }
                }
            }
        }
        collect_positions(v, &mut positions);
    }

    let mut total_refs = 0usize;
    for (name, line, col) in &positions {
        let refs = server
            .client
            .request(
                "textDocument/references",
                serde_json::json!({
                    "textDocument": {"uri": uri},
                    "position": {"line": line, "character": col},
                    "context": {"includeDeclaration": true}
                }),
            )
            .await;
        match refs {
            Ok(v) => {
                let arr: Vec<serde_json::Value> = serde_json::from_value(v).unwrap_or_default();
                if !arr.is_empty() {
                    eprintln!("  {} ({}:{}): {} references", name, line, col, arr.len());
                }
                total_refs += arr.len();
            }
            Err(e) => {
                eprintln!("  {} error: {}", name, e);
            }
        }
    }

    // === 4. definition for each token that could be a reference ===
    eprintln!("\n=== textDocument/definition (at reference positions) ===");
    // Query definition at each line to find cross-references
    let line_count = src.lines().count();
    let mut total_defs = 0usize;
    for line in 0..line_count as u32 {
        let line_text = src.lines().nth(line as usize).unwrap_or("");
        // Find identifiers in the line
        for (col, _) in line_text.match_indices(char::is_alphabetic) {
            let def = server
                .client
                .request(
                    "textDocument/definition",
                    serde_json::json!({
                        "textDocument": {"uri": uri},
                        "position": {"line": line, "character": col as u32}
                    }),
                )
                .await;
            if let Ok(v) = def {
                let locations: Vec<serde_json::Value> = serde_json::from_value::<
                    Vec<serde_json::Value>,
                >(v.clone())
                .unwrap_or_else(|_| {
                    if v.get("uri").is_some() {
                        vec![v.clone()]
                    } else {
                        vec![]
                    }
                });
                if !locations.is_empty() {
                    total_defs += locations.len();
                }
            }
        }
    }
    eprintln!(
        "  {} definition resolutions across all positions",
        total_defs
    );

    eprintln!("\n=== SUMMARY ===");
    eprintln!("Symbols:     {}", symbol_count);
    eprintln!("Sem tokens:  {}", token_count);
    eprintln!("References:  {}", total_refs);
    eprintln!("Definitions: {}", total_defs);
    eprintln!("TOTAL relationship signals: {}", total_refs + total_defs);
    eprintln!("\nFor comparison: tree-sitter found 4 relations for this file.");

    server.shutdown().await.ok();
    let _ = std::fs::remove_dir_all(&workspace);

    // What a ready server answers for this file, whatever the exact counts: a
    // server asked before it was ready answers every one of these with nothing.
    assert!(
        symbol_count > 0,
        "a ready rust-analyzer lists the file's symbols"
    );
    assert!(token_count > 0, "a ready rust-analyzer tokenizes the file");
    assert!(
        total_refs > 0,
        "a ready rust-analyzer finds the file's references"
    );
    assert!(
        total_defs > 0,
        "a ready rust-analyzer resolves the file's definitions"
    );
}
