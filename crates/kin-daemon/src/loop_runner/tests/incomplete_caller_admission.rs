// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

// A complete exact-tree admission whose changed files include a caller that no
// longer parses completely. Included into `loop_runner::tests`.

/// The coherent batch derives only complete sources, so a caller whose new
/// bytes this build cannot parse completely stays outside it while a target it
/// bound to in another file departs. The admission must still cross authority
/// and keep the withdrawn binding recorded on that caller, sealed to the bytes
/// the tree now admits for it. Falsify by restoring the strict current-source
/// read in `kin_reconcile::batch_debt`: the whole admission then fails with
/// "current source is incomplete" and the generation does not move.
#[cfg(unix)]
#[tokio::test]
async fn an_admission_keeps_the_withdrawn_binding_of_a_caller_that_no_longer_parses() {
    const CALLER: &str = "from b import beta\n\ndef run(value):\n    return beta(value)\n";
    // A real file this build's grammar cannot parse completely; in the field
    // it is a valid file whose syntax the grammar does not yet accept.
    const PARTIAL: &str = "from b import beta\n\ndef run(value):\n    return beta(value\n";
    let repo = tempfile::tempdir().unwrap();
    let state = open_test_state(&repo);
    std::fs::write(
        repo.path().join("b.py"),
        "def beta(value):\n    return value\n",
    )
    .unwrap();
    std::fs::write(repo.path().join("c.py"), CALLER).unwrap();
    sync_filesystem_with_graph(&state).await.unwrap();
    state.is_initialized.store(true, Ordering::Relaxed);
    let entity = |path: &str, name: &str| {
        state
            .graph
            .query_entities(&EntityFilter {
                file_path: Some(FilePathId::new(path)),
                ..Default::default()
            })
            .unwrap()
            .into_iter()
            .find(|entity| entity.name == name)
            .unwrap_or_else(|| panic!("{path}::{name} is admitted"))
    };
    let run = entity("c.py", "run").id;
    let beta = entity("b.py", "beta").id;
    let retired = state
        .graph
        .get_all_relations_for_entity(&run)
        .unwrap()
        .into_iter()
        .find(|relation| {
            relation.kind == kin_model::RelationKind::Calls
                && relation.dst == kin_model::GraphNodeId::Entity(beta)
        })
        .expect("the fixture needs the caller bound to beta before the edit");
    let generation = authority_generation(&state);

    std::fs::write(
        repo.path().join("b.py"),
        "def gamma(value):\n    return value * 2\n",
    )
    .unwrap();
    std::fs::write(repo.path().join("c.py"), PARTIAL).unwrap();
    sync_filesystem_with_graph(&state)
        .await
        .expect("one caller that does not parse must not refuse the whole admission");

    assert_eq!(authority_generation(&state), generation + 1);
    assert_eq!(state.graph.resolved_tree(), authority_tree(&state));
    let caller = FilePathId::new("c.py");
    let artifact = state
        .graph
        .artifact_id_at_path(&test_repo_path("c.py"))
        .unwrap();
    let Some(TreeEntry::Blob { hash, .. }) = state.graph.get_tree_entry(&caller).unwrap() else {
        panic!("the caller stays an admitted body");
    };
    assert_eq!(
        hash,
        Hash256::from_bytes(kin_blobs::digest(PARTIAL.as_bytes()).0),
        "the tree admits the caller's new bytes"
    );
    let relations = state
        .graph
        .get_all_relations_for_node(&kin_model::GraphNodeId::Artifact(artifact))
        .unwrap();
    let debt = kin_index::binding_debt::inspect_local_binding_debt(
        &caller,
        artifact,
        hash,
        &relations.iter().collect::<Vec<_>>(),
    )
    .unwrap()
    .expect("the withdrawn binding stays recorded, sealed to the caller's current bytes");
    let obligation = debt
        .obligations
        .iter()
        .find(|obligation| obligation.retired_relation.id == retired.id)
        .expect("the retired call is an obligation");
    assert_eq!(
        obligation.source_digest,
        Hash256::from_bytes(kin_blobs::digest(CALLER.as_bytes()).0),
        "the obligation keeps the immutable body that bound it"
    );
    assert_eq!(obligation.target_file, FilePathId::new("b.py"));
}
