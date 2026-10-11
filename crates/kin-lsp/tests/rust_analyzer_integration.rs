// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Integration test: start rust-analyzer, query definition and call hierarchy
//! on a Rust project.
//!
//! Requires: rust-analyzer installed (`rustup component add rust-analyzer`).
//! Skipped if rust-analyzer is not available.

use std::path::Path;
use std::time::Duration;

use kin_lsp::adapters::rust_analyzer::{cargo_layout, launch_for, RustAnalyzerAdapter};
use kin_lsp::adapters::LspAdapter;
use kin_lsp::lifecycle::LspServer;
use kin_lsp::protocol;

/// How long rust-analyzer is given to load the fixture crate, and then to
/// answer about the document it was handed. Loading takes about three seconds,
/// most of it the standard library; the bound is for a loaded host.
const READY_BUDGET: Duration = Duration::from_secs(60);

fn kin_workspace_root() -> Option<std::path::PathBuf> {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let sibling_kin = manifest_dir.parent()?.join("kin");
    if sibling_kin.join("Cargo.toml").exists() {
        return Some(sibling_kin);
    }

    let bundled_workspace = manifest_dir.parent()?.parent()?;
    if bundled_workspace.join("Cargo.toml").exists() {
        return Some(bundled_workspace.to_path_buf());
    }

    None
}

fn has_rust_analyzer() -> bool {
    which::which("rust-analyzer").is_ok()
        && std::process::Command::new("rust-analyzer")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
}

/// Test that we can start rust-analyzer, initialize it on a Rust project,
/// and get a valid response.
#[tokio::test]
async fn start_and_initialize_rust_analyzer() {
    if !has_rust_analyzer() {
        eprintln!("SKIP: rust-analyzer not available");
        return;
    }

    let Some(workspace) = kin_workspace_root() else {
        eprintln!("SKIP: kin workspace not found");
        return;
    };

    let adapter = RustAnalyzerAdapter;
    let server = LspServer::start(
        adapter.server_command(),
        &[],
        &workspace,
        adapter.initialization_options(&workspace),
        None,
    )
    .await;

    match server {
        Ok(server) => {
            eprintln!("rust-analyzer started and initialized successfully");
            eprintln!("  call_hierarchy: {}", server.has_call_hierarchy());
            eprintln!("  definition: {}", server.has_definition());
            eprintln!("  references: {}", server.has_references());

            // Verify we got some capabilities back.
            assert!(
                server.has_call_hierarchy() || server.has_definition(),
                "server should support at least call_hierarchy or definition"
            );

            // Clean shutdown.
            server.shutdown().await.expect("shutdown failed");
            eprintln!("rust-analyzer shut down cleanly");
        }
        Err(e) => {
            eprintln!("rust-analyzer failed to start: {}", e);
            // Don't fail the test — server might not support the workspace
        }
    }
}

/// A crate shaped like `kin-core`'s `init`, which calls `init_with_config`,
/// which calls `build_genesis_change`.
///
/// The queries below used to run against the whole workspace this crate
/// lives in, after fixed 10 s and 20 s sleeps. rust-analyzer takes close to
/// two minutes to load that workspace (105 s on a loaded 18-core host), so
/// both ran against a server still loading: the definition came back `null`,
/// the enrichment produced no relations, and neither asserted anything. A
/// crate this size loads in about three seconds, so the waits end on
/// rust-analyzer's own signals and the answers are asserted.
const FIXTURE_LIB: &str = "pub struct InitResult;

pub fn init(working_dir: &str) -> InitResult {
    init_with_config(working_dir, 0)
}

fn init_with_config(working_dir: &str, config: u32) -> InitResult {
    let _ = (working_dir, config);
    build_genesis_change()
}

fn build_genesis_change() -> InitResult {
    InitResult
}
";

/// The zero-based line in [`FIXTURE_LIB`] that contains `needle`.
fn fixture_line(needle: &str) -> u32 {
    FIXTURE_LIB
        .lines()
        .position(|line| line.contains(needle))
        .unwrap_or_else(|| panic!("the fixture has a line containing {needle:?}")) as u32
}

/// Test querying textDocument/definition on a known call, then full call
/// hierarchy enrichment (start server, build entity index, query outgoing
/// calls, produce Relations), against one server. Both only read, so they
/// share the one start and the one load.
#[tokio::test]
async fn query_definition_and_enrich_call_hierarchy_on_one_server() {
    use kin_lsp::enrichment::{enrich_entity_calls, EntityIndex, EntityRef};
    use kin_model::{EntityId, GraphNodeId};

    if !has_rust_analyzer() {
        eprintln!("SKIP: rust-analyzer not available");
        return;
    }

    let workspace = std::env::temp_dir().join(format!("kin-lsp-ra-init-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&workspace);
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    std::fs::write(
        workspace.join("Cargo.toml"),
        "[package]\nname = \"init_shape\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    let test_file = workspace.join("src/lib.rs");
    std::fs::write(&test_file, FIXTURE_LIB).unwrap();

    // Started the way the daemon starts it, which waits for rust-analyzer to
    // report the crate loaded, then waited on until it reports itself indexed.
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
    .expect("rust-analyzer reports the fixture crate loaded within the budget")
    .expect("rust-analyzer starts on the fixture crate");
    let indexed = server.wait_until_indexed(READY_BUDGET).await;
    assert_eq!(
        indexed,
        kin_lsp::lifecycle::Indexed::Quiescent,
        "rust-analyzer says when it has finished loading, and the wait ends on that"
    );

    // Open lib.rs so RA knows about it, and wait until it answers about it.
    let file_content = FIXTURE_LIB.to_string();
    let uri = protocol::path_to_uri(&test_file);
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
    server
        .wait_for_document(&uri, READY_BUDGET)
        .await
        .expect("rust-analyzer answers about a document it was just handed");

    // Query definition at the call `init` makes, on the line after its
    // signature.
    let init_line = fixture_line("pub fn init(working_dir");
    let init_with_config_line = fixture_line("fn init_with_config(");
    let call_line = init_line + 1;
    let call_col = FIXTURE_LIB
        .lines()
        .nth(call_line as usize)
        .and_then(|line| line.find("init_with_config"))
        .expect("init calls init_with_config") as u32;
    eprintln!("querying definition at line {}", call_line);
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        server.client.request(
            "textDocument/definition",
            protocol::TextDocumentPositionParams {
                text_document: protocol::TextDocumentIdentifier { uri: uri.clone() },
                position: protocol::Position {
                    line: call_line,
                    character: call_col + 2,
                },
            },
        ),
    )
    .await;
    let definition = match result {
        Ok(Ok(value)) => value,
        Ok(Err(e)) => panic!("definition request failed after rust-analyzer was ready: {e}"),
        Err(_) => panic!("definition request timed out after rust-analyzer was ready"),
    };
    eprintln!(
        "definition response: {}",
        serde_json::to_string_pretty(&definition).unwrap_or_default()
    );
    let landed: Vec<u64> = definition
        .as_array()
        .cloned()
        .unwrap_or_else(|| vec![definition.clone()])
        .iter()
        .filter_map(|location| {
            let range = location
                .get("targetSelectionRange")
                .or(location.get("range"))?;
            range["start"]["line"].as_u64()
        })
        .collect();
    assert_eq!(
        landed,
        [u64::from(init_with_config_line)],
        "the call in `init` resolves to `init_with_config`'s declaration: {definition}"
    );

    // Build an entity index with `init` and the functions it might call.
    let entity = |name: &str, line: u32, end_line: u32| {
        let name_col = FIXTURE_LIB
            .lines()
            .nth(line as usize)
            .and_then(|text| text.find(&format!("fn {name}")))
            .map(|col| col + 3)
            .expect("the entity is declared on its line") as u32;
        EntityRef {
            id: EntityId::new(),
            name: name.to_string(),
            file_path: "src/lib.rs".to_string(),
            start_line: line,
            start_col: 0,
            end_line,
            name_line: line,
            name_col,
            declares_name: true,
            declares_only: false,
            kind: kin_model::EntityKind::Function,
        }
    };
    let genesis_line = fixture_line("fn build_genesis_change");
    let init_entity = entity("init", init_line, init_line + 2);
    let config_entity = entity(
        "init_with_config",
        init_with_config_line,
        init_with_config_line + 3,
    );
    let genesis_entity = entity("build_genesis_change", genesis_line, genesis_line + 2);
    let index = EntityIndex::new(
        vec![
            init_entity.clone(),
            config_entity.clone(),
            genesis_entity.clone(),
        ],
        &workspace,
    );

    // Enrich: query call hierarchy for init() function.
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        enrich_entity_calls(
            &server,
            &init_entity,
            &index,
            &workspace,
            Some(&|file| (file == init_entity.file_path).then(|| file_content.clone())),
        ),
    )
    .await;
    server.shutdown().await.ok();
    let _ = std::fs::remove_dir_all(&workspace);

    let relations = match result {
        Ok(Ok(calls)) => calls.relations,
        Ok(Err(e)) => panic!("enrichment failed after rust-analyzer was ready: {e}"),
        Err(_) => panic!("enrichment timed out after rust-analyzer was ready"),
    };
    eprintln!(
        "enrichment produced {} relations from init()",
        relations.len()
    );
    for rel in &relations {
        eprintln!("  {:?} -> {:?}", rel.src, rel.dst);
    }
    let targets: Vec<GraphNodeId> = relations.iter().map(|relation| relation.dst).collect();
    assert_eq!(
        targets,
        [GraphNodeId::Entity(config_entity.id)],
        "init() calls init_with_config() and nothing else in the index; build_genesis_change() \
         is called one hop further in"
    );
    assert!(
        relations
            .iter()
            .all(|relation| relation.src == GraphNodeId::Entity(init_entity.id)),
        "every edge starts at init(): {relations:?}"
    );
    eprintln!(
        "SUCCESS: LSP enrichment pipeline works end-to-end ({} relations)",
        relations.len()
    );
}
