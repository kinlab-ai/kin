// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

// Accepted language-server evidence larger than the crash record's old 16 MiB
// limit, carried through a crash, a publication and a restart, over parsed and
// committed declarations. No external language server is started.

/// The size the accepted-evidence record used to refuse new evidence at.
const OLD_ACCEPTED_EVIDENCE_LIMIT: u64 = 16 * 1024 * 1024;

/// A reference from `caller` to `target` carrying `sites` evidence records, so
/// a few thousand of them cross the old limit the way one cold sweep of a
/// mid-size Go repository did.
fn wide_lsp_reference(
    caller: kin_model::EntityId,
    target: kin_model::EntityId,
    sites: usize,
) -> kin_model::Relation {
    let mut relation = lsp_publication_call(caller, target);
    relation.kind = RelationKind::References;
    relation.id = kin_lsp::relation_identity::language_server_relation_id(
        RelationKind::References,
        caller,
        target,
    );
    let site = relation.evidence[0].clone();
    relation.evidence = vec![site; sites];
    relation
}

fn accepted_evidence_record_len(state: &DaemonState) -> u64 {
    std::fs::metadata(state.layout.root().join("lsp-accepted-evidence.jsonl"))
        .map(|meta| meta.len())
        .unwrap_or(0)
}

#[tokio::test]
async fn accepted_evidence_past_the_old_record_limit_survives_a_restart_and_marks_its_file() {
    let (repo, state) = lsp_publication_fixture().await;
    let caller = waiting_entity(&state, "caller.py", "run");
    let target = waiting_entity(&state, "target.py", "work");

    // Different ids over the same ends are now correctly merged as one edge.
    // Admit enough distinct declarations for every stress row to have its own
    // canonical identity. A serialized relation is a lower bound on its crash
    // record line, so reserve enough declarations for the full record below
    // without parsing and committing a second, unused set of targets.
    const BATCH_SIZE: usize = 256;
    let relation_bytes = serde_json::to_vec(&wide_lsp_reference(caller.id, target.id, 64))
        .unwrap()
        .len() as u64;
    let target_count = (2 * OLD_ACCEPTED_EVIDENCE_LIMIT)
        .div_ceil(relation_bytes)
        .div_ceil(BATCH_SIZE as u64) as usize
        * BATCH_SIZE;
    let mut source = "def work():\n    return 7\n".to_owned();
    for n in 0..target_count {
        source.push_str(&format!("\ndef work_{n:05}():\n    return 7\n"));
    }
    std::fs::write(repo.path().join("target.py"), source).unwrap();
    waiting_admit(&state, "distinct evidence targets").await;
    waiting_commit(&state, "Admit distinct evidence targets").await;
    let mut targets: Vec<_> = state
        .graph
        .query_entities(&kin_db::EntityFilter {
            file_path: Some(kin_model::FilePathId::new("target.py")),
            ..Default::default()
        })
        .unwrap()
        .into_iter()
        .filter(|entity| entity.name.starts_with("work_"))
        .collect();
    targets.sort_by(|a, b| a.name.cmp(&b.name));
    assert_eq!(targets.len(), target_count);

    // Accept evidence the way the sweep does, in write batches, until the crash
    // record holds twice what it used to refuse at. The count is fixed from
    // the first batch's size rather than read back from the record, so batches
    // keep arriving after the old limit is crossed, which is the case the
    // refusal broke.
    let mut accepted = Vec::new();
    let install = |accepted: &mut Vec<kin_model::Relation>, targets: &[kin_model::Entity]| {
        let batch: Vec<_> = targets
            .iter()
            .map(|target| wide_lsp_reference(caller.id, target.id, 64))
            .collect();
        crate::daemon::install_lsp_relations(&state, &batch);
        assert!(
            batch
                .iter()
                .all(|relation| state.graph.get_relation_by_id(&relation.id).is_some()),
            "the graph takes every relation"
        );
        accepted.extend(batch);
    };
    install(&mut accepted, &targets[..BATCH_SIZE]);
    let batch_bytes = accepted_evidence_record_len(&state).max(1);
    let batches = (2 * OLD_ACCEPTED_EVIDENCE_LIMIT).div_ceil(batch_bytes) as usize;
    assert!(batches * BATCH_SIZE <= targets.len());
    for batch in 1..batches {
        install(
            &mut accepted,
            &targets[batch * BATCH_SIZE..(batch + 1) * BATCH_SIZE],
        );
    }
    assert_eq!(
        accepted
            .iter()
            .map(|relation| relation.id)
            .collect::<std::collections::HashSet<_>>()
            .len(),
        accepted.len(),
        "the stress record must contain distinct canonical edges"
    );
    let record_len = accepted_evidence_record_len(&state);
    println!(
        "accepted {} relations in {batches} batches, record {record_len} bytes",
        accepted.len()
    );
    assert!(record_len > OLD_ACCEPTED_EVIDENCE_LIMIT);
    assert!(
        state.lsp_evidence_unrecorded.lock().unwrap().is_empty(),
        "every accepted relation must be recorded"
    );

    // A crash before the sweep's publication: nothing reached authority, so
    // everything the reopened graph holds of this came from the record.
    let layout = state.layout.clone();
    drop(state);
    let state = Arc::new(DaemonState::open(layout.clone()).unwrap());
    let missing: Vec<_> = accepted
        .iter()
        .filter(|relation| state.graph.get_relation_by_id(&relation.id).is_none())
        .map(|relation| relation.id)
        .collect();
    assert!(
        missing.is_empty(),
        "{} of {} accepted relations did not survive the restart; the record past {} bytes \
         must keep every one",
        missing.len(),
        accepted.len(),
        OLD_ACCEPTED_EVIDENCE_LIMIT
    );

    // The sweep's own publication makes the evidence durable in authority,
    // which empties the record.
    stage_lsp_publication_completion(&state, &["caller.py"]);
    assert!(matches!(
        state.save_snapshot_reporting_enrichment().unwrap(),
        crate::state::EnrichmentFlush::Proceeded
    ));
    // One authority reading for both the carried relations and the completion
    // proof: materializing a workspace this size is the expensive part.
    let durable = lsp_publication_durable_with_native_completion(&state, &["caller.py"]);
    assert!(
        accepted
            .iter()
            .all(|relation| durable.relations.contains_key(&relation.id)),
        "the publication must carry every accepted relation into authority"
    );
    drop(durable);
    assert_eq!(
        accepted_evidence_record_len(&state),
        0,
        "an authority commit empties the record"
    );

    // Recorded as enriched, and still recorded after a restart.
    let epoch = crate::daemon::current_marker_epoch(&state);
    crate::daemon::mark_files_enriched(&state, &["caller.py".to_string()], epoch);
    drop(state);
    let state = Arc::new(DaemonState::open(layout).unwrap());
    assert_lsp_publication_native_completion(&state, &["caller.py"]);
    crate::daemon::load_lsp_enriched_marker(&state);
    assert!(
        crate::daemon::file_already_enriched(&state, "caller.py"),
        "the completion marker must survive the restart"
    );
    assert!(
        accepted
            .iter()
            .all(|relation| state.graph.get_relation_by_id(&relation.id).is_some()),
        "and so must the evidence, now from authority"
    );
}

/// One sweep over `caller.py` that proves `callback()` calls `work` and then
/// retracts that proof, because the file's call-site ledger settles the site
/// otherwise. This is how a later sweep over a store an earlier sweep already
/// finished can accept language-server evidence and end exactly where
/// authority is: Flask's `ScriptInfo.load_app` gets three `Calls` to the
/// three `locate_app` overloads on every sweep, and its ledger retracts all
/// three because the answers disagree. Returns the retracted proof.
async fn sweep_that_proves_then_retracts_a_call(
    repo: &tempfile::TempDir,
    state: &Arc<DaemonState>,
) -> kin_model::Relation {
    use crate::daemon::lsp_publication::{LedgerRequest, QueryInputs};
    let caller = waiting_entity(state, "caller.py", "run");
    let target = waiting_entity(state, "target.py", "work");
    let start = LSP_PUBLICATION_CALLER.find("callback()").unwrap();
    let mut proof = lsp_publication_call(caller.id, target.id);
    proof.id =
        kin_lsp::relation_identity::language_server_relation_id(RelationKind::Calls, caller.id, target.id);
    proof.evidence[0].source_span = Some(kin_model::SourceSpan {
        file: kin_model::FilePathId::new("caller.py"),
        start_byte: start,
        end_byte: start + "callback".len(),
        start_line: 1,
        start_col: 11,
        end_line: 1,
        end_col: 19,
    });

    let inputs = QueryInputs::capture(state).await.unwrap();
    let mut pending = crate::daemon::PendingEnrichment::default();
    inputs
        .absorb(state, &mut pending, vec![proof.clone()])
        .await
        .unwrap();
    inputs.flush(state, &mut pending).await.unwrap();
    assert!(
        state.graph.get_relation_by_id(&proof.id).is_some(),
        "the sweep accepts the proof into the live graph"
    );
    assert!(accepted_evidence_record_len(state) > 0, "and records it");

    let context = state
        .graph
        .get_resolution_record(&kin_model::ResolutionRecordId::context_validation(
            kin_model::LanguageId::Python,
        ))
        .and_then(|record| match record {
            kin_model::ResolutionRecord::ContextValidation(validation) => match validation.state {
                kin_model::ContextValidationState::Validated { context } => Some(context),
                _ => None,
            },
            _ => None,
        })
        .expect("the finished baseline validated its proof context");
    let entities = inputs
        .entities
        .iter()
        .filter(|entity| {
            entity
                .file_origin
                .as_ref()
                .is_some_and(|path| path.0 == "caller.py")
        })
        .collect::<Vec<_>>();
    let index = kin_lsp::EntityIndex::new(
        inputs
            .entities
            .iter()
            .filter_map(|entity| {
                crate::daemon::lsp_entity_ref(entity, &entity.file_origin.as_ref()?.0)
            })
            .collect(),
        repo.path(),
    );
    let uri = kin_lsp::protocol::path_to_uri(&repo.path().join("caller.py"));
    let produced = Default::default();
    inputs
        .settle_file(
            state,
            LSP_PUBLICATION_CALLER,
            &[],
            &kin_lsp::call_sites::ExternalNames::new(),
            &context,
            LedgerRequest {
                file: "caller.py",
                uri: &uri,
                index: &index,
                entities: &entities,
                unproven: &[],
                produced_references: &produced,
                ending: crate::call_site_ledger::PassEnding::Complete,
                not_in_build: false,
                retract: true,
            },
        )
        .await
        .unwrap();
    assert!(
        state.graph.get_relation_by_id(&proof.id).is_none(),
        "the finished ledger pass retracts the proof it does not hold"
    );
    proof
}

fn binding_history_checked(state: &DaemonState) -> bool {
    use kin_model::EntityStore as _;
    matches!(
        state.graph.binding_history_observation(),
        kin_model::BindingHistoryObservation::Checked { .. }
    )
}

/// A restarted daemon must serve the graph, and so the binding witness, that
/// the daemon which ran the sweep held. A proof the sweep accepted and then
/// retracted is not accepted evidence any more, so the record must not hand
/// it back on the next start. Handing it back made every restarted daemon on
/// an unchanged store read `local_binding_unproven` until something committed.
#[tokio::test]
async fn a_proof_the_sweep_retracted_stays_retracted_across_a_restart() {
    let (repo, state) = lsp_publication_fixture().await;
    // A store an earlier sweep finished: authority holds the file's ledgers.
    stage_lsp_publication_completion(&state, &["caller.py", "target.py"]);
    state.save_snapshot_reporting_enrichment().unwrap();
    assert!(binding_history_checked(&state));
    let generation = state
        .snapshot_generation
        .load(std::sync::atomic::Ordering::SeqCst);

    let proof = sweep_that_proves_then_retracts_a_call(&repo, &state).await;
    // Nothing is left to publish, so nothing commits and the record stays.
    state.save_snapshot_reporting_enrichment().unwrap();
    assert_eq!(
        state
            .snapshot_generation
            .load(std::sync::atomic::Ordering::SeqCst),
        generation,
        "a sweep that ends where authority is publishes nothing"
    );
    let live = state.graph.semantic_observation();
    lsp_publication_same_semantics(&live, &lsp_publication_durable(&state));
    // The accepted proof withdrew the witness while it was held. The flush that
    // found nothing to publish rebinds it, because the live graph is again
    // exactly the authority graph.
    assert!(
        binding_history_checked(&state),
        "the daemon that ran the sweep holds exactly the authority graph and its witness"
    );

    let layout = state.layout.clone();
    drop(state);
    let restarted = waiting_cold_start(layout).await;
    assert!(
        restarted.graph.get_relation_by_id(&proof.id).is_none(),
        "the restart must not reinstall a proof the sweep retracted"
    );
    lsp_publication_same_semantics(&restarted.graph.semantic_observation(), &live);
    assert!(
        binding_history_checked(&restarted),
        "a restarted daemon over the same unchanged store reads the same witness"
    );
}

/// The control for the case above: evidence the live graph still holds when
/// the process ends is replayed, and the restarted daemon reports the binding
/// interval it cannot check as unproven rather than certifying it.
#[tokio::test]
async fn accepted_evidence_still_held_at_a_crash_leaves_the_restart_unproven() {
    use crate::daemon::lsp_publication::QueryInputs;
    let (_repo, state) = lsp_publication_fixture().await;
    stage_lsp_publication_completion(&state, &["caller.py", "target.py"]);
    state.save_snapshot_reporting_enrichment().unwrap();
    assert!(binding_history_checked(&state));
    let caller = waiting_entity(&state, "caller.py", "run");
    let target = waiting_entity(&state, "target.py", "work");
    let mut proof = lsp_publication_call(caller.id, target.id);
    proof.id =
        kin_lsp::relation_identity::language_server_relation_id(RelationKind::Calls, caller.id, target.id);
    let inputs = QueryInputs::capture(&state).await.unwrap();
    let mut pending = crate::daemon::PendingEnrichment::default();
    inputs
        .absorb(&state, &mut pending, vec![proof.clone()])
        .await
        .unwrap();
    inputs.flush(&state, &mut pending).await.unwrap();
    assert!(state.graph.get_relation_by_id(&proof.id).is_some());
    assert!(!binding_history_checked(&state));

    // A crash before the sweep's publication.
    let layout = state.layout.clone();
    drop(state);
    let restarted = waiting_cold_start(layout).await;
    assert!(
        restarted.graph.get_relation_by_id(&proof.id).is_some(),
        "accepted evidence the live graph held survives the crash"
    );
    assert!(
        !binding_history_checked(&restarted),
        "and the restarted daemon cannot certify the interval it covers"
    );
}
