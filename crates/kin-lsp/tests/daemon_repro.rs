// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Reproduces the daemon LSP worker flow: start RA, didOpen, prepareCallHierarchy.
//! This tests whether the background reader + oneshot + timeout work correctly
//! when called in the same sequence as the daemon worker.

use std::time::Duration;

use kin_lsp::adapters::rust_analyzer::{cargo_layout, launch_for, RustAnalyzerAdapter};
use kin_lsp::adapters::LspAdapter;
use kin_lsp::lifecycle::LspServer;
use kin_lsp::protocol;

/// How long rust-analyzer is given to load a three-line crate, and then to
/// answer about the document it was handed. Loading takes about three seconds,
/// most of it the standard library; the bound is for a loaded host.
const READY_BUDGET: Duration = Duration::from_secs(60);

fn has_rust_analyzer() -> bool {
    which::which("rust-analyzer").is_ok()
        && std::process::Command::new("rust-analyzer")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
}

/// Reproduces the daemon worker flow:
/// 1. Start RA the way the daemon does (`launch_settled`, which waits for RA
///    to report its project loaded)
/// 2. Wait until it reports its workspace indexed (the daemon's indexing wait)
/// 3. didOpen a file (like daemon worker)
/// 4. Wait until RA answers about that file
/// 5. prepareCallHierarchy with 5s timeout (like daemon enrichment)
///
/// Steps 2 and 4 were fixed 25 s and 10 s sleeps. The waits now end on RA's
/// own signals, so the answers below are asserted rather than printed: a
/// timeout or an empty answer after RA said it was ready is the daemon bug
/// this test exists to catch.
#[tokio::test]
async fn daemon_flow_reproduction() {
    if !has_rust_analyzer() {
        eprintln!("SKIP: rust-analyzer not available");
        return;
    }

    // Use a tiny project — same as daemon test
    let workspace = std::env::temp_dir().join(format!("lsp-repro-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&workspace);
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    std::fs::write(
        workspace.join("Cargo.toml"),
        b"[package]\nname = \"repro\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(
        workspace.join("src/main.rs"),
        b"fn helper() -> i32 { 42 }\nfn caller() -> i32 { helper() }\nfn main() { println!(\"{}\", caller()); }\n",
    ).unwrap();

    let adapter = RustAnalyzerAdapter;

    // Step 1: Start RA
    eprintln!("Step 1: Starting rust-analyzer...");
    let started = tokio::time::Instant::now();
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
    .expect("rust-analyzer reports a three-line crate loaded within the budget")
    .expect("server start failed");
    eprintln!(
        "  RA started and loaded in {:?}. Capabilities: call_hierarchy={}",
        started.elapsed(),
        server.has_call_hierarchy()
    );

    // Step 2: Wait for indexing, by RA's own status.
    eprintln!("Step 2: Waiting for indexing...");
    let indexed = server.wait_until_indexed(READY_BUDGET).await;
    eprintln!("  Indexing wait complete: {indexed:?}.");
    assert_eq!(
        indexed,
        kin_lsp::lifecycle::Indexed::Quiescent,
        "rust-analyzer says when it has finished loading, and the wait ends on that"
    );

    // Step 3: didOpen
    let file_path = workspace.join("src/main.rs");
    let file_content = std::fs::read_to_string(&file_path).unwrap();
    let uri = protocol::path_to_uri(&file_path);
    eprintln!("Step 3: Sending didOpen for {}", uri);
    server
        .client
        .notify(
            "textDocument/didOpen",
            serde_json::json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": "rust",
                    "version": 1,
                    "text": file_content,
                }
            }),
        )
        .await
        .expect("didOpen failed");
    eprintln!("  didOpen sent.");

    // Step 4: Wait for RA to take the document in.
    eprintln!("Step 4: Waiting for RA to answer about the document...");
    let waited = server
        .wait_for_document(&uri, READY_BUDGET)
        .await
        .expect("rust-analyzer answers about a document it was just handed");
    eprintln!("  RA answered about the document after {waited:?}.");

    // Step 5: prepareCallHierarchy with 5s timeout (daemon pattern)
    eprintln!("Step 5: prepareCallHierarchy with 5s timeout...");
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        server.client.request(
            "textDocument/prepareCallHierarchy",
            protocol::CallHierarchyPrepareParams {
                text_document: protocol::TextDocumentIdentifier { uri: uri.clone() },
                position: protocol::Position {
                    line: 1,
                    character: 4,
                }, // caller function
            },
        ),
    )
    .await;

    match result {
        Ok(Ok(ref value)) => {
            eprintln!(
                "  SUCCESS: Got response: {}",
                serde_json::to_string_pretty(value).unwrap_or_default()
            );
        }
        Ok(Err(ref e)) => {
            eprintln!("  LSP error: {}", e);
        }
        Err(_) => {
            eprintln!("  TIMEOUT: 5s timeout fired (this is the daemon bug!)");
        }
    }

    // Step 6: If prepareCallHierarchy worked, try outgoingCalls
    let mut outgoing = Vec::new();
    if let Ok(Ok(value)) = &result {
        let items: Vec<protocol::CallHierarchyItem> =
            serde_json::from_value(value.clone()).unwrap_or_default();
        if let Some(item) = items.first() {
            eprintln!("Step 6: querying outgoingCalls for '{}'...", item.name);
            let outgoing_result = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                server.client.request(
                    "callHierarchy/outgoingCalls",
                    protocol::CallHierarchyOutgoingCallsParams { item: item.clone() },
                ),
            )
            .await;

            match outgoing_result {
                Ok(Ok(calls)) => {
                    let parsed: Vec<protocol::CallHierarchyOutgoingCall> =
                        serde_json::from_value(calls).unwrap_or_default();
                    eprintln!("  SUCCESS: {} outgoing calls", parsed.len());
                    for call in &parsed {
                        eprintln!("    -> {}", call.to.name);
                    }
                    outgoing = parsed.into_iter().map(|call| call.to.name).collect();
                }
                Ok(Err(e)) => eprintln!("  outgoingCalls error: {}", e),
                Err(_) => eprintln!("  outgoingCalls TIMEOUT"),
            }
        }
    }

    server.shutdown().await.ok();
    let _ = std::fs::remove_dir_all(&workspace);

    let prepared: Vec<protocol::CallHierarchyItem> = match &result {
        Ok(Ok(value)) => serde_json::from_value(value.clone()).unwrap_or_default(),
        Ok(Err(error)) => panic!("prepareCallHierarchy failed after RA was ready: {error}"),
        Err(_) => panic!("prepareCallHierarchy ran out its 5 s after RA was ready"),
    };
    assert_eq!(
        prepared.first().map(|item| item.name.as_str()),
        Some("caller"),
        "a ready rust-analyzer names the function under the cursor: {prepared:?}"
    );
    assert_eq!(
        outgoing,
        ["helper"],
        "`caller` calls `helper` and nothing else"
    );
    eprintln!("Test complete.");
}
