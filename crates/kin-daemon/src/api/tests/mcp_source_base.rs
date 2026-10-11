// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

const SOURCE_BASE_ORIGINAL: &str = "pub fn value() -> u8 { 1 }\npub fn sibling() -> u8 { 8 }\n";
const SOURCE_BASE_EDIT: &str = "pub fn value() -> u8 { 2 }";

#[tokio::test]
async fn mcp_module_inventory_withholds_aggregate_bodies_and_rejects_forged_write_bases() {
    let (dir, state) = mcp_lifecycle_fixture();
    let body = "pub mod config { pub fn inner() -> u8 { 1 } }\n";
    source_tree_conversion_fixture(
        &state,
        serde_json::json!({
            "verb":"create", "target":"src/config.rs", "body":body,
            "description":"module boundary fixture"
        }),
    )
    .await;
    let module = state
        .graph
        .query_entities(&Default::default())
        .unwrap()
        .into_iter()
        .find(|entity| {
            entity.kind == kin_model::EntityKind::Module
                && !kin_model::is_file_module_surface(entity)
        })
        .unwrap();
    for tool in ["get_entity_source", "get_entity_body"] {
        let result = mcp_call(
            router(Arc::clone(&state)),
            tool,
            serde_json::json!({"entity_id":module.id,"full_body":true}),
        )
        .await;
        assert_ne!(result.is_error, Some(true), "{}", mcp_result_text(&result));
        let value = tool_result_payload(&result);
        assert!(value["body"].is_null(), "{value}");
        assert!(value["source_base"].is_null(), "{value}");
        // This adapter currently records no module Contains edges. The graph
        // miss must remain a disclosed gap, never a raw body fallback.
        assert_eq!(value["member_coverage"], "unavailable", "{value}");
        assert_eq!(
            value["semantic_gap"],
            kin_mcp::container_shape::MODULE_MEMBER_GAP
        );
        assert!(value["members"].as_array().unwrap().is_empty());
        assert!(!value.to_string().contains("pub fn inner() -> u8 { 1 }"));
    }
    let batch = mcp_call(
        router(Arc::clone(&state)),
        "get_entity_sources",
        serde_json::json!({"entity_ids":[module.id]}),
    )
    .await;
    assert_ne!(batch.is_error, Some(true), "{}", mcp_result_text(&batch));
    let batch = tool_result_payload(&batch);
    assert!(batch["results"][0]["body"].is_null(), "{batch}");
    assert_eq!(
        batch["results"][0]["semantic_gap"],
        kin_mcp::container_shape::MODULE_MEMBER_GAP
    );
    let inner = source_base_read_named(&state, "inner").await;
    let mut forged = inner["source_base"].clone();
    let span = module.span.as_ref().unwrap();
    forged["entity_id"] = serde_json::json!(module.id);
    forged["start_byte"] = serde_json::json!(span.start_byte);
    forged["end_byte"] = serde_json::json!(span.end_byte);
    forged["body_hash"] = serde_json::json!(kin_blobs::digest(
        &body.as_bytes()[span.start_byte..span.end_byte]
    )
    .to_string());
    for payload in [
        serde_json::json!({"EntitySourceBase":forged}),
        serde_json::json!({"EntitySourcePatch":{"source_base":forged,"edits":[{"old_text":"{ 1 }","new_text":"{ 2 }"}]}}),
    ] {
        let session = mcp_test_session(&state);
        let tx = mcp_lifecycle_begin(&state, &session).await;
        let operation = if payload.get("EntitySourceBase").is_some() {
            serde_json::json!({"verb":"update","target":module.id,"body":body.replace("{ 1 }","{ 2 }"),"payload":payload,"description":"forged module replacement"})
        } else {
            serde_json::json!({"verb":"patch","target":module.id,"payload":payload,"description":"forged module patch"})
        };
        let result = source_base_private_call(
            &state,
            "kin_transaction_commit",
            serde_json::json!({"transaction_id":tx,"operations":[operation]}),
        )
        .await;
        assert_eq!(result.is_error, Some(true), "{}", mcp_result_text(&result));
        assert!(
            mcp_result_text(&result).contains("module_body_unavailable"),
            "{}",
            mcp_result_text(&result)
        );
    }
    assert_eq!(
        source_base_read_named(&state, "inner").await["body"],
        "pub fn inner() -> u8 { 1 }"
    );
    // The same source span covering a whole artifact is valid for callable code.
    source_tree_conversion_fixture(
        &state,
        serde_json::json!({
            "verb":"create", "target":"src/solo.rs", "body":"pub fn solo() -> u8 { 7 }",
            "description":"whole artifact callable control"
        }),
    )
    .await;
    let solo = source_base_read_named(&state, "solo").await;
    assert_eq!(solo["body"], "pub fn solo() -> u8 { 7 }");
    assert!(!solo["source_base"].is_null());
    drop(dir);
}

/// Public request preparation retains the complete fixture witness separately.
fn ticketed_source_operation(mut operation: serde_json::Value) -> serde_json::Value {
    if let Some(payload) = operation
        .get_mut("payload")
        .and_then(serde_json::Value::as_object_mut)
    {
        for (kind, value) in payload {
            let base = if kind == "EntitySourceBase" {
                Some(value)
            } else {
                value.get_mut("source_base")
            };
            if let Some(base) = base {
                if let Some(ticket) = kin_mcp::source_ticket::compact(base) {
                    *base = ticket;
                }
            }
        }
    }
    operation
}

/// Exercise private expanded-base validation, including malformed or forged
/// bases which public MCP never accepts. Public ticket lifecycle tests use the
/// HTTP route explicitly and do not call this helper.
async fn source_base_private_call(
    state: &Arc<DaemonState>,
    name: &str,
    arguments: serde_json::Value,
) -> kin_mcp::ToolCallResult {
    mcp_tools_call_inner(
        axum::http::HeaderMap::new(),
        State(Arc::clone(state)),
        Json(McpToolCallRequest {
            name: name.into(),
            arguments: serde_json::from_value(arguments).unwrap(),
        }),
    )
    .await
    .unwrap()
    .0
}

async fn source_base_private_stage(
    state: &Arc<DaemonState>,
    operation: serde_json::Value,
) -> String {
    let session = mcp_test_session(state);
    let tx = mcp_lifecycle_begin(state, &session).await;
    let result = source_base_private_call(
        state,
        "kin_transaction_stage",
        serde_json::json!({"transaction_id":tx,"operations":[operation]}),
    )
    .await;
    assert_ne!(result.is_error, Some(true), "{}", mcp_result_text(&result));
    tx
}

async fn source_base_commit_operation(
    state: &Arc<DaemonState>,
    operation: serde_json::Value,
) -> serde_json::Value {
    let operation = ticketed_source_operation(operation);
    let session = mcp_test_session(state);
    let tx = mcp_lifecycle_begin(state, &session).await;
    let result = mcp_call(
        router(Arc::clone(state)),
        "kin_transaction_commit",
        serde_json::json!({ "transaction_id": tx, "operations": [operation] }),
    )
    .await;
    assert_ne!(result.is_error, Some(true), "{}", mcp_result_text(&result));
    tool_result_payload(&result)
}

/// Explicit legacy conversion fixture boundary, compiled only in daemon tests.
/// The production MCP route never accepts these source-tree operations.
async fn source_tree_conversion_fixture(
    state: &Arc<DaemonState>,
    operation: serde_json::Value,
) -> serde_json::Value {
    let session = mcp_test_session(state);
    let tx = mcp_lifecycle_begin(state, &session).await;
    let _coordination = state.coordination_gate.lock().await;
    let _authority = state.begin_graph_authority_mutation();
    let sessions = mcp_session_registry_snapshot(state).unwrap();
    let args = serde_json::from_value(serde_json::json!({
        "transaction_id": tx, "operations": [operation]
    }))
    .unwrap();
    let result = crate::mcp_commit::tests::commit_conversion_fixture(state, &sessions, &args, None);
    persist_mcp_lifecycle_transactions(state, &sessions).unwrap();
    assert_ne!(result.is_error, Some(true), "{}", mcp_result_text(&result));
    // Mirror the served API's terminal eviction after fixture publication.
    crate::api::forget_mcp_transaction(state, &tx);
    tool_result_payload(&result)
}

/// A fresh copy of the source-base store, opened afresh, with its original
/// private complete source witness.
async fn source_base_fixture() -> (tempfile::TempDir, Arc<DaemonState>, serde_json::Value) {
    source_base_fixture_from(&source_base_template().await).await
}

/// The lifecycle store with `src/value.rs` converted into it, before any read.
async fn source_base_seeded() -> (tempfile::TempDir, Arc<DaemonState>) {
    let (dir, state) = mcp_lifecycle_fixture();
    source_tree_conversion_fixture(&state, serde_json::json!({
        "verb": "create", "target": "src/value.rs", "body": SOURCE_BASE_ORIGINAL, "description": "original source"
    })).await;
    (dir, state)
}

/// [`source_base_seeded`]'s store as a template, seeded once per test binary
/// and run.
async fn source_base_template() -> SeededStore {
    SeededStore::cached("source-base", source_base_seeded).await
}

/// [`source_base_fixture`] over a fresh copy of `template`: the same store,
/// opened afresh, with its original private complete source witness.
async fn source_base_fixture_from(
    template: &SeededStore,
) -> (tempfile::TempDir, Arc<DaemonState>, serde_json::Value) {
    let (dir, state) = template.open_copy();
    let source = source_base_original_read(&state).await;
    (dir, state, source)
}

/// Read `value` with the private complete base used by authority tests.
async fn source_base_original_read(state: &Arc<DaemonState>) -> serde_json::Value {
    let entity = state
        .graph
        .query_entities(&kin_model::EntityFilter {
            name_pattern: Some("value".into()),
            ..Default::default()
        })
        .unwrap()
        .into_iter()
        .find(|entity| entity.name == "value" && entity.kind != kin_model::EntityKind::Module)
        .unwrap();
    let source = source_base_read(state, &serde_json::json!(entity.id)).await;
    assert_eq!(source["body"], "pub fn value() -> u8 { 1 }");
    assert_eq!(
        source["source_base"]["schema"], "kin.entity.source_base.v1",
        "{source}"
    );
    source
}

fn source_base_operation(source: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "verb": "update", "target": source["id"], "body": SOURCE_BASE_EDIT,
        "payload": { "EntitySourceBase": source["source_base"] }, "description": "guarded editor change"
    })
}

/// Read a private complete witness for tests of exact base validation and
/// persisted custody. Public HTTP controls separately assert compact tickets;
/// this helper is not the agent-facing source contract.
async fn source_base_read(
    state: &Arc<DaemonState>,
    entity_id: &serde_json::Value,
) -> serde_json::Value {
    let (source, _) = mcp_entity_source_attempt(
        state,
        None,
        &state.graph,
        RequestGraphAuthority::Head,
        entity_id.as_str().expect("entity UUID"),
        false,
    )
    .await;
    assert_ne!(source.is_error, Some(true), "{}", mcp_result_text(&source));
    let source = tool_result_payload(&source);
    assert_eq!(
        source["source_base"]["schema"], "kin.entity.source_base.v1",
        "{source}"
    );
    source
}

/// Resolve the one declaration named exactly `name` to its entity, then read
/// its current source. Resolving is the caller's step, as it is for an agent:
/// a whole-entity replacement names its entity by id and carries the base the
/// read returned.
async fn source_base_read_named(state: &Arc<DaemonState>, name: &str) -> serde_json::Value {
    let named = state
        .graph
        .query_entities(&kin_model::EntityFilter {
            name_pattern: Some(name.into()),
            ..Default::default()
        })
        .unwrap()
        .into_iter()
        .filter(|entity| entity.name == name && entity.kind != kin_model::EntityKind::Module)
        .collect::<Vec<_>>();
    assert_eq!(
        named.len(),
        1,
        "exactly one declaration is named {name}: {named:?}"
    );
    source_base_read(state, &serde_json::json!(named[0].id)).await
}

/// A guarded whole-entity replacement: `body` becomes the complete source of
/// the entity `source` was read from, admitted only while that read is current.
fn source_base_replacement(
    source: &serde_json::Value,
    body: &str,
    description: &str,
) -> serde_json::Value {
    serde_json::json!({
        "verb": "update", "target": source["id"], "body": body,
        "payload": { "EntitySourceBase": source["source_base"] }, "description": description
    })
}

/// Commit a guarded replacement of `entity_id` against a fresh read, the way an
/// agent lands an edit after other work has moved the workspace.
async fn source_base_commit_fresh(
    state: &Arc<DaemonState>,
    entity_id: &serde_json::Value,
    body: &str,
    description: &str,
) -> serde_json::Value {
    let current = source_base_read(state, entity_id).await;
    source_base_commit_operation(state, source_base_replacement(&current, body, description)).await
}

async fn source_base_stage(state: &Arc<DaemonState>, operation: serde_json::Value) -> String {
    let operation = ticketed_source_operation(operation);
    let session = mcp_test_session(state);
    let tx = mcp_lifecycle_begin(state, &session).await;
    let result = mcp_call(
        router(Arc::clone(state)),
        "kin_transaction_stage",
        serde_json::json!({ "transaction_id": tx, "operations": [operation] }),
    )
    .await;
    assert_ne!(result.is_error, Some(true), "{}", mcp_result_text(&result));
    tx
}

fn source_base_roots(state: &DaemonState) -> kin_model::RootBundle {
    crate::local_repository_authority::ActiveLocalRepositoryAuthority::open(state)
        .unwrap()
        .manager
        .read_authority()
        .roots()
        .clone()
}

async fn source_base_assert_conflict(
    state: &Arc<DaemonState>,
    tx: &str,
    operation: &serde_json::Value,
) {
    let before = source_base_roots(state);
    let result = mcp_call(
        router(Arc::clone(state)),
        "kin_transaction_commit",
        serde_json::json!({ "transaction_id": tx }),
    )
    .await;
    assert_eq!(result.is_error, Some(true), "{}", mcp_result_text(&result));
    assert!(
        kin_mcp::source_base::is_source_base_conflict(&mcp_result_text(&result)),
        "{}",
        mcp_result_text(&result)
    );
    assert_eq!(
        source_base_roots(state),
        before,
        "conflict must not publish"
    );
    let retained = state.mcp_transactions.lock().unwrap();
    assert_eq!(retained[tx].state, "active");
    assert!(
        retained[tx].commit_payload_hash.is_none(),
        "conflict precedes the publication fence"
    );
    assert_eq!(
        retained[tx].staged_operations[0].body.as_deref(),
        Some(SOURCE_BASE_EDIT)
    );
    assert_eq!(
        serde_json::to_value(&retained[tx].staged_operations[0]).unwrap()["payload"],
        operation["payload"]
    );
    drop(retained);
    let reopened = DaemonState::open(state.layout.clone()).unwrap();
    let persisted = reopened.mcp_transactions.lock().unwrap();
    assert_eq!(
        persisted[tx].staged_operations[0].body.as_deref(),
        Some(SOURCE_BASE_EDIT)
    );
    assert_eq!(
        serde_json::to_value(&persisted[tx].staged_operations[0]).unwrap()["payload"],
        operation["payload"]
    );
    assert_eq!(source_base_roots(&reopened), before);
}

#[tokio::test]
async fn mcp_source_base_exact_read_stage_restart_commit_and_receipt_replay() {
    let (dir, state, source) = source_base_fixture().await;
    let tx = source_base_stage(&state, source_base_operation(&source)).await;
    let owner = retained_transaction_value(&state, &tx)["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let layout = state.layout.clone();
    drop(state);
    let state = Arc::new(DaemonState::open(layout.clone()).unwrap());
    state
        .is_initialized
        .store(true, std::sync::atomic::Ordering::Relaxed);
    reregister_transaction_owner(&state, &owner).await;
    let result = mcp_call(
        router(Arc::clone(&state)),
        "kin_transaction_commit",
        serde_json::json!({"transaction_id": tx}),
    )
    .await;
    assert_ne!(result.is_error, Some(true), "{}", mcp_result_text(&result));
    let receipt = tool_result_payload(&result);
    let current = mcp_call(
        router(Arc::clone(&state)),
        "get_entity_source",
        serde_json::json!({"entity_id":source["id"]}),
    )
    .await;
    assert_eq!(tool_result_payload(&current)["body"], SOURCE_BASE_EDIT);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("src/value.rs")).unwrap(),
        SOURCE_BASE_ORIGINAL.replacen("{ 1 }", "{ 2 }", 1)
    );
    source_base_commit_fresh(
        &state,
        &source["id"],
        "pub fn value() -> u8 { 3 }",
        "later work",
    )
    .await;
    let later = source_base_roots(&state);
    drop(state);
    let reopened = Arc::new(DaemonState::open(layout).unwrap());
    reopened
        .is_initialized
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let replay = mcp_call(
        router(Arc::clone(&reopened)),
        "kin_transaction_commit",
        serde_json::json!({"transaction_id":tx}),
    )
    .await;
    assert_ne!(replay.is_error, Some(true), "{}", mcp_result_text(&replay));
    assert_eq!(
        tool_result_payload(&replay)["change_id"],
        receipt["change_id"]
    );
    assert_eq!(tool_result_payload(&replay)["already_applied"], true);
    assert_eq!(
        source_base_roots(&reopened),
        later,
        "receipt replay bypasses the stale guard without publishing again"
    );
    let current = mcp_call(
        router(reopened),
        "get_entity_source",
        serde_json::json!({"entity_id":source["id"]}),
    )
    .await;
    assert_eq!(
        tool_result_payload(&current)["body"],
        "pub fn value() -> u8 { 3 }"
    );
}

#[tokio::test]
async fn mcp_source_base_refuses_stale_body_branch_and_deleted_entity_without_losing_work() {
    let template = source_base_template().await;
    for change in ["body", "branch", "delete"] {
        let (_dir, state, source) = source_base_fixture_from(&template).await;
        let operation = source_base_operation(&source);
        let tx = source_base_stage(&state, operation.clone()).await;
        match change {
            "body" => {
                source_base_commit_fresh(
                    &state,
                    &source["id"],
                    "pub fn value() -> u8 { 9 }",
                    "concurrent edit",
                )
                .await;
            }
            "delete" => {
                source_tree_conversion_fixture(&state, serde_json::json!({"verb":"delete", "target":"src/value.rs", "description":"retire source"})).await;
            }
            _ => {
                let name = kin_model::RefName::branch(b"other").unwrap();
                for request in [
                    kin_cli::commands::branch::BranchRequest::Create {
                        name: name.clone(),
                        operation_id: kin_model::OperationId::new(),
                        actor: AuthorId::new("guard-test"),
                    },
                    kin_cli::commands::branch::BranchRequest::Switch {
                        name,
                        operation_id: kin_model::OperationId::new(),
                        actor: AuthorId::new("guard-test"),
                    },
                ] {
                    let (status, body) =
                        post_branch_request(Arc::clone(&state), &request, None).await;
                    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
                }
            }
        }
        source_base_assert_conflict(&state, &tx, &operation).await;
    }
}

#[tokio::test]
async fn mcp_source_base_replaced_repository_and_unknown_protocol_fail_closed() {
    let (dir, state, source) = source_base_fixture().await;
    let old_namespace = tempfile::tempdir().unwrap();
    let old_kin = dir.path().join(".kin");
    drop(state);
    std::fs::rename(&old_kin, old_namespace.path().join("retained.kin")).unwrap();
    let layout = kin_core::init(dir.path()).unwrap().layout;
    let state = Arc::new(DaemonState::open(layout).unwrap());
    state
        .is_initialized
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let operation = source_base_operation(&source);
    let tx = source_base_private_stage(&state, operation.clone()).await;
    source_base_assert_conflict(&state, &tx, &operation).await;
    let before = source_base_roots(&state);
    for unsupported in [
        serde_json::json!({"EntitySourceBaseV2": source["source_base"]}),
        {
            let mut value = operation["payload"].clone();
            value["EntitySourceBase"]["schema"] = serde_json::json!("kin.entity.source_base.v2");
            value
        },
        {
            let mut value = operation["payload"].clone();
            value["EntitySourceBase"]["expected_base"] = serde_json::json!("must not be ignored");
            value
        },
    ] {
        let mut unknown = operation.clone();
        unknown["payload"] = unsupported;
        let result = source_base_private_call(
            &state,
            "kin_transaction_stage",
            serde_json::json!({"transaction_id":tx,"operations":[unknown]}),
        )
        .await;
        assert_eq!(result.is_error, Some(true), "{}", mcp_result_text(&result));
        assert!(
            mcp_result_text(&result).contains("unknown"),
            "{}",
            mcp_result_text(&result)
        );
        assert_eq!(
            state.mcp_transactions.lock().unwrap()[&tx]
                .staged_operations
                .len(),
            1
        );
        assert_eq!(source_base_roots(&state), before);
    }
}

#[tokio::test]
async fn mcp_source_base_inline_conflict_durably_retains_the_attempted_body() {
    let (dir, state, source) = source_base_fixture().await;
    let operation = source_base_operation(&source);
    source_base_commit_fresh(
        &state,
        &source["id"],
        "pub fn value() -> u8 { 9 }",
        "concurrent edit before inline attempt",
    )
    .await;
    let tx = mcp_lifecycle_begin(&state, &mcp_test_session(&state)).await;
    let owner = retained_transaction_value(&state, &tx)["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let before = source_base_roots(&state);
    let result = source_base_private_call(
        &state,
        "kin_transaction_commit",
        serde_json::json!({ "transaction_id": tx, "operations": [operation.clone()] }),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    assert!(kin_mcp::source_base::is_source_base_conflict(
        &mcp_result_text(&result)
    ));
    assert_eq!(source_base_roots(&state), before);
    let layout = state.layout.clone();
    drop(state);
    let reopened = Arc::new(DaemonState::open(layout).unwrap());
    reopened
        .is_initialized
        .store(true, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(
        reopened.mcp_transactions.lock().unwrap()[&tx]
            .staged_operations
            .len(),
        1
    );
    let retained = retained_transaction_value(&reopened, &tx);
    let mirror = crate::state::mcp_transactions_disk_path(&reopened.layout);
    let persisted = std::fs::read(&mirror).unwrap();
    let current_body = std::fs::read(dir.path().join("src/value.rs")).unwrap();
    let owner_id = SessionId(Uuid::parse_str(&owner).unwrap());
    assert!(reopened
        .coordinator
        .get_session(&owner_id)
        .unwrap()
        .is_none());
    let refused = mcp_call_as(
        router(Arc::clone(&reopened)),
        "kin_transaction_commit",
        serde_json::json!({"transaction_id":tx}),
        owner_id,
    )
    .await;
    assert_eq!(
        refused.is_error,
        Some(true),
        "{}",
        mcp_result_text(&refused)
    );
    assert!(mcp_result_text(&refused).contains("Session not found"));
    assert_eq!(retained_transaction_value(&reopened, &tx), retained);
    assert_eq!(std::fs::read(&mirror).unwrap(), persisted);
    assert_eq!(source_base_roots(&reopened), before);
    assert_eq!(
        std::fs::read(dir.path().join("src/value.rs")).unwrap(),
        current_body
    );
    reregister_transaction_owner(&reopened, &owner).await;
    assert_eq!(retained_transaction_value(&reopened, &tx), retained);
    assert_eq!(std::fs::read(&mirror).unwrap(), persisted);
    assert_eq!(source_base_roots(&reopened), before);
    // Fresh ownership does not authorize publishing the stale source base.
    source_base_assert_conflict(&reopened, &tx, &operation).await;
}

/// What a base binds is checked even when nothing has moved: the entity's
/// artifact, its body, and the body's length, which a shifted start changes.
/// The file's blob hash is no longer part of it: another entity's edit
/// changes that hash and none of this entity's bytes.
#[tokio::test]
async fn mcp_source_base_binds_artifact_and_body_even_at_the_same_workspace_revision() {
    let (_dir, state, source) = source_base_fixture().await;
    for field in ["artifact_id", "body_hash", "start_byte"] {
        let mut operation = source_base_operation(&source);
        operation["payload"]["EntitySourceBase"][field] = match field {
            "artifact_id" => serde_json::json!(kin_model::ArtifactId::new()),
            "start_byte" => serde_json::json!(1),
            _ => serde_json::json!("0".repeat(64)),
        };
        let tx = source_base_private_stage(&state, operation.clone()).await;
        source_base_assert_conflict(&state, &tx, &operation).await;
    }
    // Refused expectations did not consume or alter the original valid base.
    source_base_commit_operation(&state, source_base_operation(&source)).await;
}

/// Language-server enrichment publishes a finished file's completion mark as a
/// workspace mutation of its own, which advances the workspace generation over
/// the same head and tree. A guarded edit read before that publication still
/// names the exact bytes it replaces, so it lands.
#[tokio::test]
async fn mcp_source_base_lands_over_a_generation_that_advanced_with_no_source_change() {
    let (dir, state, source) = source_base_fixture().await;
    let workspace_context = |state: &DaemonState| {
        let workspace_id = state
            .local_repository_authority_binding()
            .unwrap()
            .workspace_id();
        let authority =
            crate::local_repository_authority::ActiveLocalRepositoryAuthority::open(state).unwrap();
        let lease = authority.manager.read_authority();
        let workspace = lease
            .metadata()
            .workspaces
            .iter()
            .find(|workspace| workspace.workspace_id == workspace_id)
            .unwrap()
            .clone();
        kin_mcp::source_base::SourceBaseContext::from_workspace(&workspace).unwrap()
    };
    let read = workspace_context(&state);
    assert_eq!(
        source["source_base"]["context"]["workspace_generation"],
        read.workspace_generation
    );

    let path = kin_model::RepoPath::from_utf8("src/value.rs".to_string()).unwrap();
    let blob = state
        .graph
        .resolved_tree()
        .artifact_at_path(&path)
        .and_then(|artifact| artifact.entry.blob_identity())
        .expect("the fixture's source file is a blob");
    assert!(crate::daemon::record_sweep_file_completed(
        &state,
        "src/value.rs",
        blob,
        crate::daemon::current_marker_epoch(&state),
    ));
    state.save_snapshot().unwrap();
    let advanced = workspace_context(&state);
    assert!(
        advanced.workspace_generation > read.workspace_generation,
        "the completion mark publishes as a workspace mutation: {read:?} -> {advanced:?}"
    );
    assert_eq!(advanced.workspace_head_hash, read.workspace_head_hash);
    assert_eq!(advanced.workspace_tree_hash, read.workspace_tree_hash);

    source_base_commit_operation(&state, source_base_operation(&source)).await;
    assert_eq!(
        std::fs::read_to_string(dir.path().join("src/value.rs")).unwrap(),
        SOURCE_BASE_ORIGINAL.replacen("{ 1 }", "{ 2 }", 1)
    );
}

/// A plain source read serves a ticket in place of its whole `source_base`.
/// A write that carries it resolves, through the daemon's own current read, to
/// the entity's current base while its body is the one the read served, even
/// after the workspace generation advanced, and lands like the whole base
/// would. Once the body changes, the same ticket is refused before anything
/// is staged.
#[tokio::test]
async fn mcp_source_base_ticket_resolves_to_the_read_base_and_refuses_once_it_changed() {
    let (dir, state, source) = source_base_fixture().await;
    let ticket = kin_mcp::source_ticket::compact(&source["source_base"]).expect("a ticket");
    assert!(kin_mcp::source_ticket::is_ticket(&ticket));

    // Publish a completion mark: the generation moves, the bytes do not.
    let path = kin_model::RepoPath::from_utf8("src/value.rs".to_string()).unwrap();
    let blob = state
        .graph
        .resolved_tree()
        .artifact_at_path(&path)
        .and_then(|artifact| artifact.entry.blob_identity())
        .expect("the fixture's source file is a blob");
    assert!(crate::daemon::record_sweep_file_completed(
        &state,
        "src/value.rs",
        blob,
        crate::daemon::current_marker_epoch(&state),
    ));
    state.save_snapshot().unwrap();
    let current = source_base_read(&state, &source["id"]).await;
    assert_ne!(
        current["source_base"]["context"]["workspace_generation"],
        source["source_base"]["context"]["workspace_generation"],
        "the generation advanced"
    );

    let read_source = |entity_id: String| {
        let state = Arc::clone(&state);
        async move { Ok(source_base_read(&state, &serde_json::json!(entity_id)).await) }
    };
    let ticketed = serde_json::json!({
        "verb": "update", "target": source["id"], "body": SOURCE_BASE_EDIT,
        "payload": { "EntitySourceBase": ticket }, "description": "guarded editor change"
    });
    let mut arguments = std::collections::HashMap::from([(
        "operations".to_string(),
        serde_json::json!([ticketed]),
    )]);
    kin_mcp::source_ticket::resolve_call("kin_transaction_commit", &mut arguments, &read_source)
        .await
        .expect("a current ticket resolves");
    let resolved = arguments["operations"][0].clone();
    assert_eq!(
        resolved["payload"]["EntitySourceBase"], current["source_base"],
        "the entity's current base: the same body, at the instant it is now read"
    );
    source_base_commit_operation(&state, resolved).await;
    assert_eq!(
        std::fs::read_to_string(dir.path().join("src/value.rs")).unwrap(),
        SOURCE_BASE_ORIGINAL.replacen("{ 1 }", "{ 2 }", 1)
    );

    let mut again = std::collections::HashMap::from([(
        "operations".to_string(),
        serde_json::json!([ticketed]),
    )]);
    let refusal =
        kin_mcp::source_ticket::resolve_call("kin_transaction_commit", &mut again, &read_source)
            .await
            .expect_err("a ticket for a body that changed is refused");
    assert!(refusal.starts_with("source_base_changed:"), "{refusal}");
    assert_eq!(
        again["operations"][0], ticketed,
        "a refused call is left as sent"
    );
}

#[tokio::test]
async fn mcp_source_base_historical_sessions_never_receive_a_current_write_expectation() {
    let (_dir, state, source) = source_base_fixture().await;
    let historical_head = kin_cli::commands::ref_lookup::resolve_ref(
        state.graph.as_ref(),
        &state.local_repository_authority_binding().unwrap(),
        None,
    )
    .unwrap();
    // The historical focal body and span still match HEAD byte-for-byte.
    source_tree_conversion_fixture(
        &state,
        serde_json::json!({
            "verb": "create", "target": "src/later.rs", "body": "pub fn later() {}\n",
            "description": "advance history without changing the focal source"
        }),
    )
    .await;
    let session = mcp_test_session(&state);
    let session_id = SessionId(uuid::Uuid::parse_str(&session).unwrap());
    let response = router(Arc::clone(&state))
        .oneshot(
            Request::post(format!("/session/{session}/scope"))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"ref_string": format!("kin:{historical_head}")}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert!(matches!(
        state
            .graph_for_request_with_authority(Some(&session_id))
            .await
            .1,
        RequestGraphAuthority::SessionScope
    ));
    let before = source_base_roots(&state);
    for name in ["get_entity_source", "get_entity_body"] {
        let historical = mcp_call_as(
            router(Arc::clone(&state)),
            name,
            serde_json::json!({"entity_id":source["id"]}),
            session_id,
        )
        .await;
        assert_ne!(
            historical.is_error,
            Some(true),
            "{}",
            mcp_result_text(&historical)
        );
        let historical = tool_result_payload(&historical);
        assert_eq!(historical["body"], source["body"]);
        assert!(
            historical["source_base"].is_null(),
            "historical body cannot authorize a current edit: {historical}"
        );
        let current = mcp_call(
            router(Arc::clone(&state)),
            name,
            serde_json::json!({"entity_id":source["id"]}),
        )
        .await;
        assert!(kin_mcp::source_ticket::is_ticket(
            &tool_result_payload(&current)["source_base"]
        ));
    }
    for scoped in [false, true] {
        let batch_args = serde_json::json!({"entity_ids":[source["id"]]});
        let batch = if scoped {
            mcp_call_as(
                router(Arc::clone(&state)),
                "get_entity_sources",
                batch_args,
                session_id,
            )
            .await
        } else {
            mcp_call(router(Arc::clone(&state)), "get_entity_sources", batch_args).await
        };
        assert_ne!(batch.is_error, Some(true), "{}", mcp_result_text(&batch));
        assert!(!mcp_result_text(&batch).contains("source_base"));
    }
    assert_eq!(source_base_roots(&state), before);

    // The fixture's base predates the history advanced above, so the current
    // edit reads the version it replaces again, as HEAD rather than as the
    // historical session.
    source_base_commit_fresh(
        &state,
        &source["id"],
        "pub fn value() -> u8 { 9 }",
        "make current source diverge from the selected historical revision",
    )
    .await;
    let before = source_base_roots(&state);
    for name in ["get_entity_source", "get_entity_body"] {
        let historical = mcp_call_as(
            router(Arc::clone(&state)),
            name,
            serde_json::json!({"entity_id":source["id"]}),
            session_id,
        )
        .await;
        // Historical reconstruction may honestly refuse unavailable provenance;
        // neither a refusal nor a supported historical body can mint a HEAD base.
        assert!(!mcp_result_text(&historical).contains("kin.entity.source_base.v1"));
        if historical.is_error != Some(true) {
            let historical = tool_result_payload(&historical);
            assert_eq!(historical["body"], source["body"]);
            assert!(historical["source_base"].is_null());
        }
    }
    assert_eq!(source_base_roots(&state), before);
}

#[tokio::test]
async fn semantic_retained_conversion_refuses_validation_and_inline_append_without_growth() {
    let (_dir, state, source) = source_base_fixture().await;
    let session = mcp_test_session(&state);
    let tx = mcp_lifecycle_begin(&state, &session).await;
    let sessions = mcp_session_registry_snapshot(&state).unwrap();
    let old = kin_mcp::session::parse_staged_operations(&serde_json::json!([{
        "verb":"create", "target":"src/legacy.rs", "body":"pub fn legacy() {}\n",
        "description":"retained work from an older client"
    }]))
    .unwrap();
    sessions.stage_transaction(&tx, old).unwrap();
    persist_mcp_lifecycle_transactions(&state, &sessions).unwrap();
    let retained = retained_transaction_value(&state, &tx);
    let mirror = crate::state::mcp_transactions_disk_path(&state.layout);
    let bytes = std::fs::read(&mirror).unwrap();
    let roots = source_base_roots(&state);
    for tool in [
        "kin_transaction_validate",
        "kin_transaction_commit",
        "kin_transaction_commit",
    ] {
        let mut args = serde_json::json!({"transaction_id":tx});
        if tool == "kin_transaction_commit" {
            args["operations"] =
                serde_json::json!([ticketed_source_operation(source_base_operation(&source))]);
        }
        let result = mcp_call(router(Arc::clone(&state)), tool, args).await;
        assert_eq!(result.is_error, Some(true));
        assert!(
            mcp_result_text(&result).contains("semantic_operation_required"),
            "{}",
            mcp_result_text(&result)
        );
        assert_eq!(retained_transaction_value(&state, &tx), retained);
        assert_eq!(std::fs::read(&mirror).unwrap(), bytes);
        assert_eq!(source_base_roots(&state), roots);
    }
    let reopened = DaemonState::open(state.layout.clone()).unwrap();
    assert_eq!(
        serde_json::to_value(&reopened.mcp_transactions.lock().unwrap()[&tx]).unwrap(),
        retained
    );
}

#[tokio::test]
async fn public_source_ticket_stage_and_inline_receipt_replay_preserve_authority() {
    let (_dir, state, witness) = source_base_fixture().await;
    let public = mcp_call(
        router(Arc::clone(&state)),
        "get_entity_source",
        serde_json::json!({"entity_id":witness["id"]}),
    )
    .await;
    let source = tool_result_payload(&public);
    assert!(
        kin_mcp::source_ticket::is_ticket(&source["source_base"]),
        "{source}"
    );
    let operation = source_base_operation(&source);
    let tx = source_base_stage(&state, operation.clone()).await;
    let retained = state.mcp_transactions.lock().unwrap()[&tx].clone();
    assert_eq!(
        serde_json::to_value(&retained.staged_operations[0]).unwrap()["payload"],
        source_base_operation(&witness)["payload"]
    );
    assert_eq!(
        retained.staged_operations[0].body.as_deref(),
        Some(SOURCE_BASE_EDIT)
    );
    let committed = mcp_call(
        router(Arc::clone(&state)),
        "kin_transaction_commit",
        serde_json::json!({"transaction_id":tx}),
    )
    .await;
    assert_ne!(
        committed.is_error,
        Some(true),
        "{}",
        mcp_result_text(&committed)
    );
    source_base_commit_fresh(
        &state,
        &source["id"],
        "pub fn value() -> u8 { 3 }",
        "later work",
    )
    .await;
    let roots = source_base_roots(&state);
    let replay = mcp_call(
        router(Arc::clone(&state)),
        "kin_transaction_commit",
        serde_json::json!({"transaction_id":tx,"operations":[operation.clone()]}),
    )
    .await;
    assert_ne!(replay.is_error, Some(true), "{}", mcp_result_text(&replay));
    assert_eq!(
        tool_result_payload(&replay)["change_id"],
        tool_result_payload(&committed)["change_id"]
    );
    assert_eq!(source_base_roots(&state), roots);
    for phase in ["committing", "committed"] {
        let mut fenced = retained.clone();
        fenced.state = phase.into();
        let mut arguments =
            HashMap::from([("operations".into(), serde_json::json!([operation.clone()]))]);
        resolve_fenced_mcp_source_tickets(&mut arguments, &fenced)
            .await
            .unwrap();
        assert_eq!(arguments["operations"][0], source_base_operation(&witness));
        let mut foreign = operation.clone();
        foreign["payload"]["EntitySourceBase"] = kin_mcp::source_ticket::compact(
            &source_base_read(&state, &source["id"]).await["source_base"],
        )
        .unwrap();
        let mut arguments = HashMap::from([("operations".into(), serde_json::json!([foreign]))]);
        let original_arguments = arguments.clone();
        assert!(resolve_fenced_mcp_source_tickets(&mut arguments, &fenced)
            .await
            .is_err());
        assert_eq!(
            arguments, original_arguments,
            "refusal must not partially replace bases"
        );
    }
}

#[tokio::test]
async fn public_mcp_write_refuses_expanded_source_authority_before_staging() {
    let (_dir, state, source) = source_base_fixture().await;
    let tx = mcp_lifecycle_begin(&state, &mcp_test_session(&state)).await;
    let roots = source_base_roots(&state);
    let refused = mcp_call(
        router(Arc::clone(&state)),
        "kin_transaction_stage",
        serde_json::json!({"transaction_id":tx,"operations":[source_base_operation(&source)]}),
    )
    .await;
    assert_eq!(refused.is_error, Some(true));
    assert!(
        mcp_result_text(&refused).contains("source_base ticket"),
        "{}",
        mcp_result_text(&refused)
    );
    assert!(state.mcp_transactions.lock().unwrap()[&tx]
        .staged_operations
        .is_empty());
    assert_eq!(source_base_roots(&state), roots);
    let accepted = mcp_call(router(Arc::clone(&state)), "kin_transaction_stage",
        serde_json::json!({"transaction_id":tx,"operations":[ticketed_source_operation(source_base_operation(&source))]})).await;
    assert_ne!(
        accepted.is_error,
        Some(true),
        "{}",
        mcp_result_text(&accepted)
    );
    assert_eq!(
        serde_json::to_value(&state.mcp_transactions.lock().unwrap()[&tx].staged_operations[0])
            .unwrap()["payload"],
        source_base_operation(&source)["payload"]
    );
}

const SOURCE_BASE_CLASS: &str = "class App:\n    def first(self):\n        return 1\n\n    def second(self):\n        return 2\n\n    def third(self):\n        return 3\n";

/// An agent reads three methods of one class, then changes them in three
/// separate writes, each carrying the ticket its first read returned. Every
/// write lands: a commit that left a method alone moved the tree, the file's
/// bytes and the method's place in the file, and none of the method's own
/// bytes. A fourth write with the ticket of a method already changed is
/// refused, in a sentence that says to read it again, and writes nothing.
#[tokio::test]
async fn mcp_source_base_tickets_survive_commits_that_left_their_entity_alone() {
    let (dir, state) = mcp_lifecycle_fixture();
    source_tree_conversion_fixture(
        &state,
        serde_json::json!({
            "verb": "create", "target": "pkg/app.py", "body": SOURCE_BASE_CLASS,
            "description": "a class with three methods"
        }),
    )
    .await;
    let disk = || std::fs::read_to_string(dir.path().join("pkg/app.py")).unwrap();
    // Every read comes first, through the public route, which serves tickets.
    let mut reads = Vec::new();
    for name in ["App.first", "App.second", "App.third"] {
        let witness = source_base_read_named(&state, name).await;
        let public = tool_result_payload(
            &mcp_call(
                router(Arc::clone(&state)),
                "get_entity_source",
                serde_json::json!({"entity_id": witness["id"]}),
            )
            .await,
        );
        assert!(
            kin_mcp::source_ticket::is_ticket(&public["source_base"]),
            "{public}"
        );
        reads.push(public);
    }
    let patch = |read: &serde_json::Value, from: &str, to: &str| {
        serde_json::json!({
            "verb": "patch", "target": read["id"],
            "payload": {"EntitySourcePatch": {"source_base": read["source_base"],
                "edits": [{"old_text": from, "new_text": to}]}},
            "description": "change one method"
        })
    };
    let write = |operation: serde_json::Value| {
        let state = Arc::clone(&state);
        async move {
            let session = mcp_test_session(&state);
            let tx = mcp_lifecycle_begin(&state, &session).await;
            mcp_call(
                router(Arc::clone(&state)),
                "kin_transaction_commit",
                serde_json::json!({"transaction_id": tx, "operations": [operation]}),
            )
            .await
        }
    };
    for (read, (from, to)) in reads.iter().zip([
        ("return 1", "return 10"),
        ("return 2", "return 20"),
        ("return 3", "return 30"),
    ]) {
        let result = write(patch(read, from, to)).await;
        assert_ne!(
            result.is_error,
            Some(true),
            "{from}: {}",
            mcp_result_text(&result)
        );
        // The receipt says which entity the write landed on.
        let receipt = tool_result_payload(&result);
        let changes = &receipt[kin_mcp::write_reply::ENTITY_CHANGES_KEY];
        assert_eq!(changes.as_array().map(Vec::len), Some(1), "{receipt}");
        assert_eq!(changes[0]["entity_id"], read["id"], "{receipt}");
        assert_eq!(changes[0]["change"], "changed", "{receipt}");
        assert_eq!(changes[0]["module"], "pkg.app", "{receipt}");
        assert_eq!(
            receipt[kin_mcp::write_reply::UNITS_WRITTEN_KEY], 1,
            "{receipt}"
        );
    }
    let changed = SOURCE_BASE_CLASS
        .replace("return 1", "return 10")
        .replace("return 2", "return 20")
        .replace("return 3", "return 30");
    assert_eq!(disk(), changed);

    // The first method's body is no longer the one its ticket was read from.
    let refused = write(patch(&reads[0], "return 10", "return 11")).await;
    assert_eq!(
        refused.is_error,
        Some(true),
        "{}",
        mcp_result_text(&refused)
    );
    let said = mcp_result_text(&refused);
    assert!(said.contains("source_base_changed"), "{said}");
    assert!(
        said.contains("read App.first again with get_entity_source"),
        "{said}"
    );
    assert!(said.contains("Nothing was written"), "{said}");
    assert_eq!(disk(), changed);

    // A fresh read of it is one step, and its ticket lands.
    let witness = source_base_read_named(&state, "App.first").await;
    let fresh = tool_result_payload(
        &mcp_call(
            router(Arc::clone(&state)),
            "get_entity_source",
            serde_json::json!({"entity_id": witness["id"]}),
        )
        .await,
    );
    let result = write(patch(&fresh, "return 10", "return 11")).await;
    assert_ne!(result.is_error, Some(true), "{}", mcp_result_text(&result));
    assert_eq!(disk(), changed.replace("return 10", "return 11"));
}
