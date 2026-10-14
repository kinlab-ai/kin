// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

// Keeping chosen entities of the uncommitted working copy, and undoing one
// entity, end to end through the daemon's routes: the working copy is never
// written by a keep, the change carries exactly the chosen entities, and the
// work left behind is exactly the work that was not chosen.

const ENTITY_KEEP_FILE: &str = "src/work.py";
const ENTITY_KEEP_HEAD: &str = "import os\n\n\ndef alpha():\n    return 1\n\n\ndef beta():\n    return 2\n\n\ndef gamma():\n    return 3\n";
const ENTITY_KEEP_NOTES: &str = "notes.txt";

/// The working copy every keep test starts from: alpha and beta edited, gamma
/// removed, delta added, a new import, and the unparsed notes rewritten.
const ENTITY_KEEP_WORK: &str = "import os\nimport sys\n\n\ndef alpha():\n    return 10\n\n\ndef beta():\n    return 20\n\n\ndef delta():\n    return 4\n";

async fn entity_keep_post(
    state: &Arc<DaemonState>,
    route: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let response = router(Arc::clone(state))
        .oneshot(
            Request::post(route)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 256 * 1024)
        .await
        .unwrap();
    let parsed = serde_json::from_slice(&body)
        .unwrap_or_else(|_| serde_json::json!({"raw": String::from_utf8_lossy(&body)}));
    (status, parsed)
}

fn entity_keep_commit_body(
    entities: &[String],
    operation_id: kin_model::OperationId,
) -> serde_json::Value {
    serde_json::json!({
        "operation_id": operation_id,
        "timestamp": kin_model::Timestamp::now(),
        "message": "keep the chosen entities",
        "author": "Test Author <test@example.invalid>",
        "entities": entities,
    })
}

fn entity_keep_restore_body(
    entity: &str,
    to: Option<&str>,
    expected: Option<&str>,
) -> serde_json::Value {
    serde_json::json!({
        "operation_id": kin_model::OperationId::new(),
        "entity": entity,
        "to": to,
        "expected": expected,
        "actor": "Test Author <test@example.invalid>",
    })
}

/// The live entity named `name` in the work content.
fn entity_keep_live(state: &Arc<DaemonState>, name: &str) -> Option<kin_model::Entity> {
    state
        .graph
        .query_entities(&kin_model::EntityFilter {
            file_path: Some(kin_model::FilePathId::new(ENTITY_KEEP_FILE)),
            ..Default::default()
        })
        .unwrap()
        .into_iter()
        .find(|entity| entity.name == name && entity.kind != kin_model::EntityKind::Module)
}

fn entity_keep_id(state: &Arc<DaemonState>, name: &str) -> String {
    entity_keep_live(state, name)
        .unwrap_or_else(|| panic!("{name} is held in the working copy"))
        .id
        .to_string()
}

/// What the head change holds at `path`.
fn entity_keep_head_bytes(state: &Arc<DaemonState>, path: &str) -> Option<Vec<u8>> {
    let authority = ActiveApiRepositoryAuthority::open(state).unwrap();
    let lease = authority.manager.read_authority();
    let workspace = lease
        .metadata()
        .workspaces
        .iter()
        .find(|workspace| workspace.workspace_id == authority.workspace_id)
        .unwrap()
        .clone();
    let head = lease
        .resolve_target_change_id(workspace.base_target.as_ref().unwrap())
        .unwrap();
    let resolved = crate::commit_deltas::resolve_authority_baseline(lease.snapshot(), &head)
        .unwrap()
        .into_owned();
    let artifact = resolved
        .tree
        .artifact_at_path(&RepoPath::from_utf8(path).unwrap())?;
    let hash = artifact.entry.blob_identity().unwrap();
    Some(authority.manager.load_source_blob(hash).unwrap().unwrap())
}

fn entity_keep_head_change(state: &Arc<DaemonState>) -> SemanticChangeId {
    let authority = ActiveApiRepositoryAuthority::open(state).unwrap();
    let lease = authority.manager.read_authority();
    let workspace = lease
        .metadata()
        .workspaces
        .iter()
        .find(|workspace| workspace.workspace_id == authority.workspace_id)
        .unwrap()
        .clone();
    lease
        .resolve_target_change_id(workspace.base_target.as_ref().unwrap())
        .unwrap()
}

/// The entities and unparsed contents the working copy holds uncommitted, by
/// name, as `kin diff` derives them.
fn entity_keep_uncommitted(
    state: &Arc<DaemonState>,
) -> (
    std::collections::BTreeSet<String>,
    std::collections::BTreeSet<String>,
) {
    let binding = kin_core::LocalRepositoryAuthorityBinding::from_layout(&state.layout).unwrap();
    let report =
        kin_cli::commands::diff::inspect(&binding, None, None, Some(state.graph.as_ref())).unwrap();
    let entities = report
        .entity_deltas
        .iter()
        .filter_map(|delta| match delta {
            EntityDelta::Added { new } => Some(new),
            EntityDelta::Removed { old } => Some(old),
            EntityDelta::Modified { old, new } => {
                kin_review::is_semantic_modification(old, new).then_some(new)
            }
        })
        .filter(|entity| entity.kind != kin_model::EntityKind::Module)
        .map(|entity| entity.name.clone())
        .collect();
    let contents = report
        .artifact_deltas
        .iter()
        .filter_map(|delta| delta.new_state().or(delta.old_state()))
        .map(|located| located.path.to_string())
        .collect();
    (entities, contents)
}

/// A store whose head holds the original work content and notes, with the
/// working copy then edited into [`ENTITY_KEEP_WORK`] and admitted.
async fn entity_keep_fixture() -> (Arc<DaemonState>, SemanticChangeId, String) {
    let state = test_state();
    let root = state.layout.working_dir().to_path_buf();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join(ENTITY_KEEP_FILE), ENTITY_KEEP_HEAD).unwrap();
    std::fs::write(root.join(ENTITY_KEEP_NOTES), "first draft\n").unwrap();
    post_command_commit(&state, "the original work").await;
    let base = entity_keep_head_change(&state);
    let gamma = entity_keep_id(&state, "gamma");
    std::fs::write(root.join(ENTITY_KEEP_FILE), ENTITY_KEEP_WORK).unwrap();
    std::fs::write(root.join(ENTITY_KEEP_NOTES), "second draft\n").unwrap();
    crate::loop_runner::sync_filesystem_with_graph_under_coordination(&state)
        .await
        .unwrap();
    (state, base, gamma)
}

fn entity_keep_notes_id(state: &Arc<DaemonState>) -> String {
    state
        .graph
        .resolved_tree()
        .artifact_at_path(&RepoPath::from_utf8(ENTITY_KEEP_NOTES).unwrap())
        .unwrap()
        .artifact_id
        .0
        .to_string()
}

fn entity_keep_disk(state: &Arc<DaemonState>, path: &str) -> String {
    std::fs::read_to_string(state.layout.working_dir().join(path)).unwrap()
}

/// The keep: alpha's edit, delta's addition and gamma's removal are committed,
/// and beta's edit, the new import and the notes stay uncommitted, the working
/// copy not touched at all.
#[tokio::test]
async fn entity_keep_commits_only_the_chosen_entities_and_leaves_the_rest_exactly() {
    let (state, _base, gamma) = entity_keep_fixture().await;
    let (before, _) = entity_keep_uncommitted(&state);
    assert_eq!(
        before,
        ["alpha", "beta", "delta", "gamma"]
            .into_iter()
            .map(String::from)
            .collect::<std::collections::BTreeSet<_>>(),
        "the fixture's uncommitted work"
    );
    let chosen = vec![
        entity_keep_id(&state, "alpha"),
        entity_keep_id(&state, "delta"),
        gamma,
    ];
    let operation_id = kin_model::OperationId::new();
    let (status, answer) = entity_keep_post(
        &state,
        "/commands/commit/entities",
        entity_keep_commit_body(&chosen, operation_id),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["replayed"], false, "{answer}");

    {
        let authority = ActiveApiRepositoryAuthority::open(&state).unwrap();
        let lease = authority.manager.read_authority();
        let persisted = lease
            .workspace_graph_snapshot(&authority.workspace_id)
            .unwrap()
            .unwrap();
        let live = state.graph.to_snapshot();
        assert_eq!(
            persisted.resolved_tree, live.resolved_tree,
            "a reopened authority keeps the whole working copy's tree"
        );
        assert_eq!(
            persisted.entities, live.entities,
            "a reopened authority keeps the uncommitted entities after its base advances"
        );
        assert_eq!(
            persisted.relations, live.relations,
            "a reopened authority keeps the working copy's relations"
        );
    }

    assert_eq!(
        entity_keep_disk(&state, ENTITY_KEEP_FILE),
        ENTITY_KEEP_WORK,
        "a keep never writes the working copy"
    );
    assert_eq!(
        entity_keep_disk(&state, ENTITY_KEEP_NOTES),
        "second draft\n"
    );
    assert_eq!(
        String::from_utf8(entity_keep_head_bytes(&state, ENTITY_KEEP_FILE).unwrap()).unwrap(),
        "import os\n\n\ndef alpha():\n    return 10\n\n\ndef beta():\n    return 2\n\n\ndef delta():\n    return 4\n",
        "the change holds alpha's edit, delta and gamma's removal, and nothing else"
    );
    assert_eq!(
        entity_keep_head_bytes(&state, ENTITY_KEEP_NOTES).unwrap(),
        b"first draft\n",
        "an unparsed entity that was not chosen stays at the head"
    );
    let (entities, contents) = entity_keep_uncommitted(&state);
    assert_eq!(
        entities,
        ["beta"]
            .into_iter()
            .map(String::from)
            .collect::<std::collections::BTreeSet<_>>(),
        "only beta's change is left uncommitted, and delta keeps the identity it had"
    );
    assert!(contents.contains(ENTITY_KEEP_NOTES), "{contents:?}");

    // Sent again, the same operation answers with the change it recorded and
    // records nothing new.
    let landed = entity_keep_head_change(&state);
    let (status, replay) = entity_keep_post(
        &state,
        "/commands/commit/entities",
        entity_keep_commit_body(&chosen, operation_id),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{replay}");
    assert_eq!(replay["replayed"], true, "{replay}");
    assert_eq!(replay["change_id"], answer["change_id"]);
    assert_eq!(entity_keep_head_change(&state), landed);

    // The unparsed entity is kept whole.
    let notes = entity_keep_notes_id(&state);
    let (status, answer) = entity_keep_post(
        &state,
        "/commands/commit/entities",
        entity_keep_commit_body(&[notes], kin_model::OperationId::new()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(
        entity_keep_head_bytes(&state, ENTITY_KEEP_NOTES).unwrap(),
        b"second draft\n"
    );
    let (entities, contents) = entity_keep_uncommitted(&state);
    assert_eq!(
        entities,
        ["beta"]
            .into_iter()
            .map(String::from)
            .collect::<std::collections::BTreeSet<_>>()
    );
    assert!(!contents.contains(ENTITY_KEEP_NOTES), "{contents:?}");

    // Keeping the last entity of the content keeps the content whole,
    // the import no entity holds included.
    let beta = entity_keep_id(&state, "beta");
    let (status, answer) = entity_keep_post(
        &state,
        "/commands/commit/entities",
        entity_keep_commit_body(&[beta], kin_model::OperationId::new()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(
        String::from_utf8(entity_keep_head_bytes(&state, ENTITY_KEEP_FILE).unwrap()).unwrap(),
        ENTITY_KEEP_WORK
    );
    let (entities, contents) = entity_keep_uncommitted(&state);
    assert!(entities.is_empty(), "{entities:?}");
    assert!(contents.is_empty(), "{contents:?}");
}

#[tokio::test]
async fn entity_keep_refuses_what_it_cannot_commit_and_records_nothing() {
    let (state, _base, _gamma) = entity_keep_fixture().await;
    let refuse = |answer: &serde_json::Value| {
        assert_eq!(answer["error"], "entity_refused", "{answer}");
        answer["message"].as_str().unwrap().to_string()
    };

    let (status, answer) = entity_keep_post(
        &state,
        "/commands/commit/entities",
        entity_keep_commit_body(&["not-an-entity".into()], kin_model::OperationId::new()),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(refuse(&answer), "not-an-entity does not name an entity.");

    let (status, answer) = entity_keep_post(
        &state,
        "/commands/commit/entities",
        entity_keep_commit_body(
            &[kin_model::EntityId::new().to_string()],
            kin_model::OperationId::new(),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(refuse(&answer).starts_with("No uncommitted work names entity"));

    let root = state.layout.working_dir().to_path_buf();
    std::fs::write(
        root.join("src/pair.js"),
        "function left() { return 1 } function right() { return 2 }\n",
    )
    .unwrap();
    post_command_commit(&state, "a pair on one line").await;
    std::fs::write(
        root.join("src/pair.js"),
        "function left() { return 10 } function right() { return 20 }\n",
    )
    .unwrap();
    crate::loop_runner::sync_filesystem_with_graph_under_coordination(&state)
        .await
        .unwrap();
    let left = state
        .graph
        .query_entities(&kin_model::EntityFilter {
            file_path: Some(kin_model::FilePathId::new("src/pair.js")),
            ..Default::default()
        })
        .unwrap()
        .into_iter()
        .find(|entity| entity.name == "left")
        .unwrap();
    let head = entity_keep_head_change(&state);
    let (status, answer) = entity_keep_post(
        &state,
        "/commands/commit/entities",
        entity_keep_commit_body(&[left.id.to_string()], kin_model::OperationId::new()),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{answer}");
    assert_eq!(
        refuse(&answer),
        "left and right were changed in the same place, so Kin cannot commit one without the \
         other."
    );
    assert_eq!(
        entity_keep_head_change(&state),
        head,
        "a refusal records nothing"
    );
    assert_eq!(
        entity_keep_disk(&state, "src/pair.js"),
        "function left() { return 10 } function right() { return 20 }\n"
    );
}

/// The sentence a restore guarded by a semantic fingerprint is refused with.
fn entity_keep_inexact(name: &str) -> String {
    format!(
        "Kin cannot tell whether {name} changed since it was read, because that fingerprint \
         stays the same when a comment or spacing changes, so nothing was restored. Guard the \
         restore with the exact source a review reports for {name}, source:<body_hash>."
    )
}

/// The exact source guard for `entity`, cut from the working copy the way a
/// review reports its `after.body_hash`.
fn entity_keep_exact_source(state: &Arc<DaemonState>, entity: &kin_model::Entity) -> String {
    let span = entity.span.as_ref().unwrap();
    let work = entity_keep_disk(state, &entity.file_origin.as_ref().unwrap().0);
    format!(
        "source:{}",
        kin_blobs::digest(&work.as_bytes()[span.start_byte..span.end_byte])
    )
}

/// The exact source guard for the live entity `name` in the work content.
fn entity_keep_exact(state: &Arc<DaemonState>, name: &str) -> String {
    entity_keep_exact_source(state, &entity_keep_live(state, name).unwrap())
}

/// No semantic fingerprint is a stale guard: an AST shape stays put when a
/// literal changes, and a behavior fingerprint stays put when a comment does.
#[tokio::test]
async fn entity_restore_refuses_a_later_token_edit_and_any_semantic_fingerprint_guard() {
    let (state, _, _) = entity_keep_fixture().await;
    let read = entity_keep_live(&state, "beta").unwrap();
    let later = ENTITY_KEEP_WORK.replace("return 20", "return 30");
    std::fs::write(state.layout.working_dir().join(ENTITY_KEEP_FILE), &later).unwrap();
    crate::loop_runner::sync_filesystem_with_graph_under_coordination(&state)
        .await
        .unwrap();
    let current = entity_keep_live(&state, "beta").unwrap();
    assert_eq!(read.id, current.id);
    assert_eq!(read.fingerprint.ast_hash, current.fingerprint.ast_hash);
    assert_ne!(
        read.fingerprint.behavior_hash,
        current.fingerprint.behavior_hash
    );
    let head = entity_keep_head_change(&state);
    for (expected, message) in [
        (
            read.fingerprint.behavior_hash,
            "beta changed since it was read, so nothing was restored.".to_string(),
        ),
        (read.fingerprint.ast_hash, entity_keep_inexact("beta")),
        (current.fingerprint.behavior_hash, entity_keep_inexact("beta")),
    ] {
        let (status, answer) = entity_keep_post(
            &state,
            "/commands/entity/restore",
            entity_keep_restore_body(&read.id.to_string(), None, Some(&expected.to_string())),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{answer}");
        assert_eq!(answer["message"], message);
        assert_eq!(entity_keep_disk(&state, ENTITY_KEEP_FILE), later);
        assert_eq!(entity_keep_head_change(&state), head);
    }
    let (status, answer) = entity_keep_post(
        &state,
        "/commands/entity/restore",
        entity_keep_restore_body(
            &current.id.to_string(),
            None,
            Some(&entity_keep_exact(&state, "beta")),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(
        entity_keep_disk(&state, ENTITY_KEEP_FILE),
        ENTITY_KEEP_WORK.replace("return 20", "return 2")
    );
}

/// The step a working review reports for `name`, as the local app reads it.
async fn entity_keep_read_step(state: &Arc<DaemonState>, name: &str) -> serde_json::Value {
    let guide = entity_keep_guide(state, None).await;
    guide["chapters"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|chapter| chapter["steps"].as_array().unwrap())
        .find(|step| step["name"] == name)
        .unwrap_or_else(|| panic!("the working review reports {name}: {guide}"))
        .clone()
}

/// A reviewer reads an entity, then its source changes in a way no semantic
/// fingerprint shows: a comment, or spacing inside it. Restoring from that
/// reading would discard what was never read, so it is refused and nothing
/// is written, whether the reading is named by its exact source or by its
/// behavior fingerprint. A literal edit is refused the same way. Read again,
/// the restore lands, and the same operation sent again replays.
#[tokio::test]
async fn entity_restore_refuses_comment_spacing_and_literal_edits_after_the_read_then_replays() {
    let (state, _, _) = entity_keep_fixture().await;
    let head = entity_keep_head_change(&state);
    let step = entity_keep_read_step(&state, "beta").await;
    let entity = step["entity_id"].as_str().unwrap().to_string();
    let body_hash = step["after"]["body_hash"].as_str().unwrap();
    assert_eq!(
        format!("source:{body_hash}"),
        entity_keep_exact(&state, "beta"),
        "the review reports the exact span the restore checks"
    );
    let exact = format!("source:{body_hash}");
    let behavior = step["evidence"]["fingerprint_after"]["behavior"]
        .as_str()
        .unwrap()
        .to_string();

    for (later, what) in [
        (
            ENTITY_KEEP_WORK.replace("def beta():\n", "def beta():\n    # explained later\n"),
            "a comment",
        ),
        (
            ENTITY_KEEP_WORK.replace("return 20", "return  20"),
            "spacing inside the entity",
        ),
        (
            ENTITY_KEEP_WORK.replace("return 20", "return 21"),
            "a literal",
        ),
    ] {
        std::fs::write(state.layout.working_dir().join(ENTITY_KEEP_FILE), &later).unwrap();
        crate::loop_runner::sync_filesystem_with_graph_under_coordination(&state)
            .await
            .unwrap();
        let semantic_only = what != "a literal";
        assert_eq!(
            entity_keep_live(&state, "beta")
                .unwrap()
                .fingerprint
                .behavior_hash
                .to_string()
                == behavior,
            semantic_only,
            "{what} leaves the behavior fingerprint as it was: {semantic_only}"
        );
        let (status, answer) = entity_keep_post(
            &state,
            "/commands/entity/restore",
            entity_keep_restore_body(&entity, None, Some(&exact)),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{what}: {answer}");
        assert_eq!(
            answer["message"], "beta changed since it was read, so nothing was restored.",
            "{what}"
        );
        assert_eq!(entity_keep_disk(&state, ENTITY_KEEP_FILE), later, "{what}");
        assert_eq!(entity_keep_head_change(&state), head, "{what}");

        let (status, answer) = entity_keep_post(
            &state,
            "/commands/entity/restore",
            entity_keep_restore_body(&entity, None, Some(&behavior)),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{what}: {answer}");
        let expected_sentence = if semantic_only {
            entity_keep_inexact("beta")
        } else {
            "beta changed since it was read, so nothing was restored.".to_string()
        };
        assert_eq!(answer["message"], expected_sentence, "{what}");
        assert_eq!(entity_keep_disk(&state, ENTITY_KEEP_FILE), later, "{what}");
        assert_eq!(entity_keep_head_change(&state), head, "{what}");
    }

    // Read again, the restore lands, and the same operation replays without
    // writing again, even though the reading it names is no longer current.
    let commented =
        ENTITY_KEEP_WORK.replace("def beta():\n", "def beta():\n    # explained later\n");
    std::fs::write(state.layout.working_dir().join(ENTITY_KEEP_FILE), &commented).unwrap();
    crate::loop_runner::sync_filesystem_with_graph_under_coordination(&state)
        .await
        .unwrap();
    let fresh = entity_keep_read_step(&state, "beta").await;
    let fresh = format!("source:{}", fresh["after"]["body_hash"].as_str().unwrap());
    assert_ne!(fresh, exact);
    let restore = entity_keep_restore_body(&entity, None, Some(&fresh));
    let (status, answer) =
        entity_keep_post(&state, "/commands/entity/restore", restore.clone()).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["replayed"], false, "{answer}");
    let restored = ENTITY_KEEP_WORK.replace("return 20", "return 2");
    assert_eq!(entity_keep_disk(&state, ENTITY_KEEP_FILE), restored);
    let (status, replay) = entity_keep_post(&state, "/commands/entity/restore", restore).await;
    assert_eq!(status, StatusCode::OK, "{replay}");
    assert_eq!(replay["replayed"], true, "{replay}");
    assert_eq!(entity_keep_disk(&state, ENTITY_KEEP_FILE), restored);
    assert_eq!(entity_keep_head_change(&state), head, "a restore records no change");
}

#[tokio::test]
async fn entity_restore_checks_exact_source_and_preserves_an_unrelated_later_edit() {
    let (state, _, _) = entity_keep_fixture().await;
    let read = entity_keep_live(&state, "beta").unwrap();
    let span = read.span.as_ref().unwrap();
    let expected = format!(
        "source:{}",
        kin_blobs::digest(&ENTITY_KEEP_WORK.as_bytes()[span.start_byte..span.end_byte])
    );
    let later = ENTITY_KEEP_WORK.replace("def beta():\n", "def beta():\n    # newly explained\n");
    std::fs::write(state.layout.working_dir().join(ENTITY_KEEP_FILE), &later).unwrap();
    crate::loop_runner::sync_filesystem_with_graph_under_coordination(&state)
        .await
        .unwrap();
    let current = entity_keep_live(&state, "beta").unwrap();
    assert_eq!(
        read.fingerprint.behavior_hash,
        current.fingerprint.behavior_hash
    );
    let (status, answer) = entity_keep_post(
        &state,
        "/commands/entity/restore",
        entity_keep_restore_body(&read.id.to_string(), None, Some(&expected)),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{answer}");
    assert_eq!(entity_keep_disk(&state, ENTITY_KEEP_FILE), later);

    // Only a sibling moves after the read. Its edit must not stale this read,
    // and the restore must preserve it exactly.
    let later = ENTITY_KEEP_WORK.replace("return 10", "return 99");
    std::fs::write(state.layout.working_dir().join(ENTITY_KEEP_FILE), &later).unwrap();
    let (status, answer) = entity_keep_post(
        &state,
        "/commands/entity/restore",
        entity_keep_restore_body(&read.id.to_string(), None, Some(&expected)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(
        entity_keep_disk(&state, ENTITY_KEEP_FILE),
        later.replace("return 20", "return 2")
    );
}

/// Undo one entity: its uncommitted change goes, as an uncommitted edit, and
/// nothing else in the working copy moves. A removed entity comes back, an
/// added one goes, and a stale read is refused.
#[tokio::test]
async fn entity_restore_undoes_one_entity_and_round_trips() {
    let (state, base, gamma) = entity_keep_fixture().await;
    let head = entity_keep_head_change(&state);
    let beta = entity_keep_live(&state, "beta").unwrap();

    let (status, answer) = entity_keep_post(
        &state,
        "/commands/entity/restore",
        entity_keep_restore_body(&beta.id.to_string(), None, Some("stale-fingerprint")),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{answer}");
    assert_eq!(
        answer["message"],
        "beta changed since it was read, so nothing was restored."
    );
    assert_eq!(entity_keep_disk(&state, ENTITY_KEEP_FILE), ENTITY_KEEP_WORK);

    let restore_beta = entity_keep_restore_body(
        &beta.id.to_string(),
        None,
        Some(&entity_keep_exact(&state, "beta")),
    );
    let (status, answer) =
        entity_keep_post(&state, "/commands/entity/restore", restore_beta.clone()).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["restored_to"], head.to_string());
    assert_eq!(answer["replayed"], false);
    // Sent again, the same operation answers that it landed and writes nothing.
    let (status, replay) = entity_keep_post(&state, "/commands/entity/restore", restore_beta).await;
    assert_eq!(status, StatusCode::OK, "{replay}");
    assert_eq!(replay["replayed"], true, "{replay}");
    let restored = ENTITY_KEEP_WORK.replace("return 20", "return 2");
    assert_eq!(
        entity_keep_disk(&state, ENTITY_KEEP_FILE),
        restored,
        "beta is back at the head's version and nothing else moved"
    );
    assert_eq!(
        entity_keep_head_change(&state),
        head,
        "a restore records no change"
    );
    let (entities, _) = entity_keep_uncommitted(&state);
    assert_eq!(
        entities,
        ["alpha", "delta", "gamma"]
            .into_iter()
            .map(String::from)
            .collect::<std::collections::BTreeSet<_>>()
    );
    assert_eq!(
        entity_keep_live(&state, "beta").unwrap().id,
        beta.id,
        "the restored entity keeps its identity"
    );

    // The removed entity comes back, after the entity written in its place.
    let (status, answer) = entity_keep_post(
        &state,
        "/commands/entity/restore",
        entity_keep_restore_body(&gamma, Some(&base.to_string()), Some("absent")),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    let returned = format!("{restored}\n\ndef gamma():\n    return 3\n");
    assert_eq!(entity_keep_disk(&state, ENTITY_KEEP_FILE), returned);
    assert_eq!(
        entity_keep_live(&state, "gamma").unwrap().id.to_string(),
        gamma
    );

    // The added entity goes.
    let delta = entity_keep_id(&state, "delta");
    let (status, answer) = entity_keep_post(
        &state,
        "/commands/entity/restore",
        entity_keep_restore_body(&delta, None, None),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert!(entity_keep_live(&state, "delta").is_none());
    let (entities, _) = entity_keep_uncommitted(&state);
    assert_eq!(
        entities,
        ["alpha"]
            .into_iter()
            .map(String::from)
            .collect::<std::collections::BTreeSet<_>>(),
        "after three restores only alpha's change is left"
    );

    // Restoring what already matches is refused, and writes nothing.
    let unchanged = entity_keep_disk(&state, ENTITY_KEEP_FILE);
    let (status, answer) = entity_keep_post(
        &state,
        "/commands/entity/restore",
        entity_keep_restore_body(&beta.id.to_string(), None, None),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{answer}");
    assert_eq!(
        answer["message"],
        "beta already matches that change, so there is nothing to restore."
    );
    assert_eq!(entity_keep_disk(&state, ENTITY_KEEP_FILE), unchanged);

    // The unparsed entity is restored whole.
    let notes = entity_keep_notes_id(&state);
    let (status, answer) = entity_keep_post(
        &state,
        "/commands/entity/restore",
        entity_keep_restore_body(&notes, None, None),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(entity_keep_disk(&state, ENTITY_KEEP_NOTES), "first draft\n");
}

#[tokio::test]
async fn entity_restore_replay_uses_its_recorded_base_after_the_head_advances() {
    let (state, original_head, _) = entity_keep_fixture().await;
    let beta = entity_keep_id(&state, "beta");
    let restore_beta = entity_keep_restore_body(&beta, None, None);
    let (status, answer) =
        entity_keep_post(&state, "/commands/entity/restore", restore_beta.clone()).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["restored_to"], original_head.to_string());

    post_command_commit(&state, "record the remaining work").await;
    let advanced_head = entity_keep_head_change(&state);
    assert_ne!(advanced_head, original_head);
    let later_work = entity_keep_disk(&state, ENTITY_KEEP_FILE)
        .replace("def beta():\n    return 2", "def beta():\n    return 200");
    std::fs::write(
        state.layout.working_dir().join(ENTITY_KEEP_FILE),
        &later_work,
    )
    .unwrap();
    let (status, replay) = entity_keep_post(&state, "/commands/entity/restore", restore_beta).await;
    assert_eq!(status, StatusCode::OK, "{replay}");
    assert_eq!(replay["replayed"], true, "{replay}");
    assert_eq!(
        replay["restored_to"],
        original_head.to_string(),
        "a replay reports the original implicit target rather than today's head"
    );
    assert_eq!(entity_keep_head_change(&state), advanced_head);
    assert_eq!(
        entity_keep_disk(&state, ENTITY_KEEP_FILE),
        later_work,
        "replaying an old restore leaves a later edit intact"
    );

    let alpha = entity_keep_id(&state, "alpha");
    let explicit = entity_keep_restore_body(&alpha, Some(&original_head.to_string()), None);
    let (status, answer) =
        entity_keep_post(&state, "/commands/entity/restore", explicit.clone()).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["restored_to"], original_head.to_string());
    let restored_work = entity_keep_disk(&state, ENTITY_KEEP_FILE);
    let (status, replay) = entity_keep_post(&state, "/commands/entity/restore", explicit).await;
    assert_eq!(status, StatusCode::OK, "{replay}");
    assert_eq!(replay["replayed"], true, "{replay}");
    assert!(
        replay["restored_to"].is_null(),
        "a receipt does not prove an explicit restore's target"
    );
    assert_eq!(entity_keep_disk(&state, ENTITY_KEEP_FILE), restored_work);
}

async fn entity_keep_guide(state: &Arc<DaemonState>, change: Option<String>) -> serde_json::Value {
    let response = router(Arc::clone(state))
        .oneshot(
            Request::post("/review")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"op":"guide", "json":true, "working":change.is_none(), "change":change})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    let answer: kin_cli::commands::review::ReviewResponse = serde_json::from_slice(&body).unwrap();
    serde_json::from_str(&answer.json.unwrap()).unwrap()
}

fn entity_keep_authored_steps(guide: &serde_json::Value) -> std::collections::BTreeSet<String> {
    guide["chapters"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|chapter| chapter["steps"].as_array().unwrap())
        .filter_map(|step| {
            let authors = step["authored_by"].as_array()?;
            assert_eq!(authors.len(), 1, "{step}");
            assert_eq!(authors[0]["name"], "Claude Code", "{step}");
            assert_eq!(authors[0]["session"], "client-session-1", "{step}");
            assert_eq!(authors[0]["through"], "client", "{step}");
            Some(step["name"].as_str().unwrap().to_string())
        })
        .collect()
}

async fn entity_keep_client_authorship_round_trip(delayed: bool) {
    let state = test_state();
    let root = state.layout.working_dir().to_path_buf();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join(ENTITY_KEEP_FILE), ENTITY_KEEP_HEAD).unwrap();
    post_command_commit(&state, "the original entities").await;
    let original_base = entity_keep_head_change(&state);
    activity_open_session(&state).await;
    let unit = root.join(ENTITY_KEEP_FILE);
    let prepared =
        activity_edit_hook(&state, &unit, ENTITY_KEEP_HEAD, ENTITY_KEEP_WORK, "before").await;
    assert_eq!(prepared["outcome"], "prepared", "{prepared}");
    std::fs::write(&unit, ENTITY_KEEP_WORK).unwrap();
    crate::loop_runner::sync_filesystem_with_graph(&state)
        .await
        .unwrap();
    if !delayed {
        let claimed = activity_report_edit(&state, &unit, ENTITY_KEEP_HEAD, ENTITY_KEEP_WORK).await;
        assert_eq!(claimed["outcome"], "attributed", "{claimed}");
        assert_eq!(claimed["entities"], 4, "{claimed}");
    }
    let alpha = entity_keep_id(&state, "alpha");
    let request = entity_keep_commit_body(&[alpha], kin_model::OperationId::new());
    let (status, kept) =
        entity_keep_post(&state, "/commands/commit/entities", request.clone()).await;
    assert_eq!(status, StatusCode::OK, "{kept}");
    let first_change = kept["change_id"].as_str().unwrap().to_string();
    if delayed {
        let claimed = activity_report_edit(&state, &unit, ENTITY_KEEP_HEAD, ENTITY_KEEP_WORK).await;
        assert_eq!(claimed["outcome"], "attributed", "{claimed}");
        assert_eq!(claimed["entities"], 4, "{claimed}");
    }
    let edits = state.agent_authorship.edits();
    assert_eq!(edits.len(), 2, "{edits:?}");
    let sealed = edits
        .iter()
        .find(|edit| edit.change_id.as_ref() == Some(&first_change))
        .unwrap();
    assert_eq!(sealed.base, Some(original_base.to_string()));
    assert_eq!(sealed.entities.len(), 1);
    let leftover = edits.iter().find(|edit| edit.change_id.is_none()).unwrap();
    assert_eq!(leftover.base.as_ref(), Some(&first_change));
    assert_eq!(leftover.entities.len(), 3);
    let (status, replayed) = entity_keep_post(&state, "/commands/commit/entities", request).await;
    assert_eq!(status, StatusCode::OK, "{replayed}");
    assert_eq!(
        state.agent_authorship.edits(),
        edits,
        "a replay duplicates no attribution"
    );
    let names = |items: &[&str]| {
        items
            .iter()
            .map(|name| name.to_string())
            .collect::<std::collections::BTreeSet<_>>()
    };
    assert_eq!(
        entity_keep_authored_steps(&entity_keep_guide(&state, Some(first_change.clone())).await),
        names(&["alpha"])
    );
    assert_eq!(
        entity_keep_authored_steps(&entity_keep_guide(&state, None).await),
        names(&["beta", "delta", "gamma"])
    );

    // Another chosen commit takes only beta and leaves the remaining two
    // claims uncommitted on the new base without changing their attribution.
    let beta = entity_keep_id(&state, "beta");
    let (status, kept) = entity_keep_post(
        &state,
        "/commands/commit/entities",
        entity_keep_commit_body(&[beta], kin_model::OperationId::new()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{kept}");
    let second_change = kept["change_id"].as_str().unwrap().to_string();
    assert_eq!(
        entity_keep_authored_steps(&entity_keep_guide(&state, Some(second_change.clone())).await),
        names(&["beta"])
    );
    assert_eq!(
        entity_keep_authored_steps(&entity_keep_guide(&state, None).await),
        names(&["delta", "gamma"])
    );
    assert_eq!(entity_keep_disk(&state, ENTITY_KEEP_FILE), ENTITY_KEEP_WORK);
    let recorded = state.agent_authorship.edits();
    // Attribution is repository authority once written, which a daemon does
    // as its agents go quiet and before it stops; a daemon that reopens the
    // store reads it back from there.
    crate::agent_authorship::settle(&state, true).await;
    let reopened = Arc::new(DaemonState::open(state.layout.clone()).unwrap());
    reopened
        .is_initialized
        .store(true, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(reopened.agent_authorship.edits(), recorded);
    assert_eq!(
        entity_keep_authored_steps(&entity_keep_guide(&reopened, Some(first_change)).await),
        names(&["alpha"])
    );
    assert_eq!(
        entity_keep_authored_steps(&entity_keep_guide(&reopened, Some(second_change)).await),
        names(&["beta"])
    );
    assert_eq!(
        entity_keep_authored_steps(&entity_keep_guide(&reopened, None).await),
        names(&["delta", "gamma"])
    );
}

#[tokio::test]
#[serial_test::serial]
async fn entity_keep_preserves_client_authorship_for_committed_and_leftover_entities_after_reopen()
{
    entity_keep_client_authorship_round_trip(false).await;
}

#[tokio::test]
#[serial_test::serial]
async fn entity_keep_attributes_a_delayed_client_hook_to_both_chosen_and_leftover_work() {
    entity_keep_client_authorship_round_trip(true).await;
}

const ENTITY_KEEP_CLASS_FILE: &str = "src/cart.py";
const ENTITY_KEEP_CLASS_HEAD: &str = "class Cart:\n    def first(self):\n        return 1\n\n    def second(self):\n        return 2\n";
const ENTITY_KEEP_CLASS_WORK: &str = "class Cart:\n    def first(self):\n        return 10\n\n    def second(self):\n        return 20\n\n    def third(self):\n        return 3\n";
const ENTITY_KEEP_FRESH_FILE: &str = "src/fresh.py";
const ENTITY_KEEP_FRESH_WORK: &str =
    "import os\n\n\ndef one():\n    return 1\n\n\ndef two():\n    return 2\n";

/// The live entity whose name is `name`, or ends in `.name`, in `path`.
fn entity_keep_member(state: &Arc<DaemonState>, path: &str, name: &str) -> kin_model::Entity {
    state
        .graph
        .query_entities(&kin_model::EntityFilter {
            file_path: Some(kin_model::FilePathId::new(path)),
            ..Default::default()
        })
        .unwrap()
        .into_iter()
        .find(|entity| entity.name == name || entity.name.ends_with(&format!(".{name}")))
        .unwrap_or_else(|| panic!("{name} is held in the working copy"))
}

/// One member of a container is kept while its sibling's edit stays. Content
/// no commit holds yet is kept whole or not at all.
#[tokio::test]
async fn entity_keep_keeps_one_member_of_a_container_and_new_content_whole() {
    let state = test_state();
    let root = state.layout.working_dir().to_path_buf();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join(ENTITY_KEEP_CLASS_FILE), ENTITY_KEEP_CLASS_HEAD).unwrap();
    post_command_commit(&state, "a container with two members").await;
    std::fs::write(root.join(ENTITY_KEEP_CLASS_FILE), ENTITY_KEEP_CLASS_WORK).unwrap();
    std::fs::write(root.join(ENTITY_KEEP_FRESH_FILE), ENTITY_KEEP_FRESH_WORK).unwrap();
    crate::loop_runner::sync_filesystem_with_graph_under_coordination(&state)
        .await
        .unwrap();
    let first = entity_keep_member(&state, ENTITY_KEEP_CLASS_FILE, "first");
    let third = entity_keep_member(&state, ENTITY_KEEP_CLASS_FILE, "third");
    let one = entity_keep_member(&state, ENTITY_KEEP_FRESH_FILE, "one");
    let two = entity_keep_member(&state, ENTITY_KEEP_FRESH_FILE, "two");
    let (status, answer) = entity_keep_post(
        &state,
        "/commands/commit/entities",
        entity_keep_commit_body(
            &[first.id.to_string(), third.id.to_string()],
            kin_model::OperationId::new(),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(
        String::from_utf8(entity_keep_head_bytes(&state, ENTITY_KEEP_CLASS_FILE).unwrap()).unwrap(),
        "class Cart:\n    def first(self):\n        return 10\n\n    def second(self):\n        return 2\n\n    def third(self):\n        return 3\n",
        "the container holds the kept members' changes and the other member as it was"
    );
    assert_eq!(
        entity_keep_disk(&state, ENTITY_KEEP_CLASS_FILE),
        ENTITY_KEEP_CLASS_WORK
    );
    assert!(entity_keep_head_bytes(&state, ENTITY_KEEP_FRESH_FILE).is_none());
    let leftover = |state: &Arc<DaemonState>| {
        entity_keep_uncommitted(state)
            .0
            .iter()
            .map(|name| name.rsplit('.').next().unwrap().to_string())
            .collect::<std::collections::BTreeSet<_>>()
    };
    assert_eq!(
        leftover(&state),
        ["Cart", "one", "second", "two"]
            .into_iter()
            .map(String::from)
            .collect::<std::collections::BTreeSet<_>>(),
        "what was not chosen is what is left, with the container that still holds it"
    );

    // One entity of content the head never held cannot be kept alone.
    let head = entity_keep_head_change(&state);
    let (status, answer) = entity_keep_post(
        &state,
        "/commands/commit/entities",
        entity_keep_commit_body(&[two.id.to_string()], kin_model::OperationId::new()),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{answer}");
    assert_eq!(
        answer["message"],
        "two and one are in content no commit holds yet, which Kin commits whole, so it cannot \
         commit one without the other."
    );
    assert_eq!(entity_keep_head_change(&state), head);

    // Both of its entities keep it whole, the import no entity holds included.
    let (status, answer) = entity_keep_post(
        &state,
        "/commands/commit/entities",
        entity_keep_commit_body(
            &[one.id.to_string(), two.id.to_string()],
            kin_model::OperationId::new(),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(
        String::from_utf8(entity_keep_head_bytes(&state, ENTITY_KEEP_FRESH_FILE).unwrap()).unwrap(),
        ENTITY_KEEP_FRESH_WORK
    );
    assert_eq!(
        leftover(&state),
        ["Cart", "second"]
            .into_iter()
            .map(String::from)
            .collect::<std::collections::BTreeSet<_>>()
    );
    assert_eq!(
        entity_keep_member(&state, ENTITY_KEEP_FRESH_FILE, "two").id,
        two.id,
        "a kept entity keeps the identity it had in the working copy"
    );

    // The sibling's edit is undone alone, and the kept members stay kept.
    let second = entity_keep_member(&state, ENTITY_KEEP_CLASS_FILE, "second");
    let (status, answer) = entity_keep_post(
        &state,
        "/commands/entity/restore",
        entity_keep_restore_body(
            &second.id.to_string(),
            None,
            Some(&entity_keep_exact_source(&state, &second)),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(
        entity_keep_disk(&state, ENTITY_KEEP_CLASS_FILE),
        ENTITY_KEEP_CLASS_WORK.replace("return 20", "return 2")
    );
    assert!(leftover(&state).is_empty());
}

const ENTITY_KEEP_AGENT_FILE: &str = "src/agent.py";
const ENTITY_KEEP_AGENT_HEAD: &str = "def tally():\n    return 1\n";

/// Each step of a guide, by entity name, with the names of whoever it says
/// wrote that entity.
fn entity_keep_step_authors(
    guide: &serde_json::Value,
) -> std::collections::BTreeMap<String, Vec<String>> {
    guide["chapters"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|chapter| chapter["steps"].as_array().unwrap())
        .map(|step| {
            let authors = step["authored_by"]
                .as_array()
                .map(|authors| {
                    authors
                        .iter()
                        .map(|author| author["name"].as_str().unwrap_or_default().to_string())
                        .collect()
                })
                .unwrap_or_default();
            (step["name"].as_str().unwrap().to_string(), authors)
        })
        .collect()
}

/// An agent's commit over MCP records what its operation wrote and nothing
/// else. A person's uncommitted entities in another unit, attributed to the
/// client session that made them, stay uncommitted against the agent's new
/// change, keep that attribution, and are still what the guide of uncommitted
/// work walks; the agent's change names only the entity the agent wrote.
#[tokio::test]
#[serial_test::serial]
async fn entity_keep_an_agent_commit_leaves_uncommitted_work_and_its_authorship_in_place() {
    let state = test_state();
    let root = state.layout.working_dir().to_path_buf();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join(ENTITY_KEEP_FILE), ENTITY_KEEP_HEAD).unwrap();
    std::fs::write(root.join(ENTITY_KEEP_AGENT_FILE), ENTITY_KEEP_AGENT_HEAD).unwrap();
    post_command_commit(&state, "the original work").await;
    let original_base = entity_keep_head_change(&state);

    // The person's edits, reported by the client that made them.
    activity_open_session(&state).await;
    let unit = root.join(ENTITY_KEEP_FILE);
    let prepared =
        activity_edit_hook(&state, &unit, ENTITY_KEEP_HEAD, ENTITY_KEEP_WORK, "before").await;
    assert_eq!(prepared["outcome"], "prepared", "{prepared}");
    std::fs::write(&unit, ENTITY_KEEP_WORK).unwrap();
    crate::loop_runner::sync_filesystem_with_graph(&state)
        .await
        .unwrap();
    let claimed = activity_report_edit(&state, &unit, ENTITY_KEEP_HEAD, ENTITY_KEEP_WORK).await;
    assert_eq!(claimed["outcome"], "attributed", "{claimed}");
    let (uncommitted, _) = entity_keep_uncommitted(&state);
    let persons = ["alpha", "beta", "delta", "gamma"]
        .into_iter()
        .map(String::from)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(uncommitted, persons, "the person's uncommitted work");
    assert_eq!(
        entity_keep_authored_steps(&entity_keep_guide(&state, None).await),
        persons
    );

    // An agent changes tally through MCP, in a unit nobody else touched.
    let tally = entity_keep_member(&state, ENTITY_KEEP_AGENT_FILE, "tally");
    let session = mcp_test_session(&state);
    let read = entity_patch_read(&state, &serde_json::json!(tally.id)).await;
    let result = mutate_http(
        &state,
        "kin_mutate",
        serde_json::json!({
            "session_id": session,
            "request_id": "entity-keep-agent-commit",
            "operations": [entity_patch_operation_public(&read, &[("return 1", "return 2")])],
            "summary": "Return two from tally",
        }),
        &session,
    )
    .await;
    assert_ne!(result.is_error, Some(true), "{}", mcp_result_text(&result));

    // The change is the base change plus the agent's unit.
    let agent_change = entity_keep_head_change(&state);
    assert_ne!(agent_change, original_base);
    assert_eq!(
        String::from_utf8(entity_keep_head_bytes(&state, ENTITY_KEEP_AGENT_FILE).unwrap()).unwrap(),
        "def tally():\n    return 2\n"
    );
    assert_eq!(
        String::from_utf8(entity_keep_head_bytes(&state, ENTITY_KEEP_FILE).unwrap()).unwrap(),
        ENTITY_KEEP_HEAD,
        "the agent's change holds none of the person's uncommitted work"
    );
    let change = kin_model::ChangeStore::get_change(state.graph.as_ref(), &agent_change)
        .unwrap()
        .unwrap();
    assert!(
        change.entity_deltas.iter().all(|delta| {
            delta
                .new_state()
                .or(delta.old_state())
                .and_then(|entity| entity.file_origin.as_ref())
                .is_some_and(|origin| origin.0 == ENTITY_KEEP_AGENT_FILE)
        }),
        "the agent's change holds only entities of the unit its operation wrote: {:?}",
        change.entity_deltas
    );
    let guide = entity_keep_step_authors(&entity_keep_guide(&state, Some(agent_change.to_string())).await);
    assert_eq!(
        guide.keys().cloned().collect::<Vec<_>>(),
        vec!["tally".to_string()],
        "the agent's change walks only the entity the agent wrote"
    );
    assert!(
        guide.values().flatten().all(|author| author != "Claude Code"),
        "nothing in the agent's change is attributed to the person's client: {guide:?}"
    );

    // The person's work is untouched, uncommitted and still theirs.
    assert_eq!(entity_keep_disk(&state, ENTITY_KEEP_FILE), ENTITY_KEEP_WORK);
    assert_eq!(
        entity_keep_disk(&state, ENTITY_KEEP_AGENT_FILE),
        "def tally():\n    return 2\n"
    );
    assert_eq!(entity_keep_uncommitted(&state).0, persons);
    let edits = state.agent_authorship.edits();
    assert_eq!(edits.len(), 1, "{edits:?}");
    assert_eq!(edits[0].change_id, None, "{edits:?}");
    assert_eq!(edits[0].base, Some(agent_change.to_string()), "{edits:?}");
    assert_eq!(edits[0].entities.len(), 4, "{edits:?}");
    assert_eq!(
        entity_keep_authored_steps(&entity_keep_guide(&state, None).await),
        persons,
        "the guide of uncommitted work still walks the person's entities under their author"
    );

    // Repository authority keeps the whole working copy, as a reopen reads it.
    let authority = ActiveApiRepositoryAuthority::open(&state).unwrap();
    let lease = authority.manager.read_authority();
    let persisted = lease
        .workspace_graph_snapshot(&authority.workspace_id)
        .unwrap()
        .unwrap();
    let live = state.graph.to_snapshot();
    assert_eq!(persisted.resolved_tree, live.resolved_tree);
    assert_eq!(persisted.entities, live.entities);
    assert_eq!(persisted.relations, live.relations);
}
