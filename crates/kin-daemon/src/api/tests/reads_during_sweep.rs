// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

const SWEEP_CHURN_UNIT: &str =
    "def churn_source():\n    return churn_target()\n\ndef churn_target():\n    return 2\n";

/// A language-server relation between two entities, as a sweep installs one.
fn sweep_churn_relation(
    src: kin_model::EntityId,
    dst: kin_model::EntityId,
) -> kin_model::Relation {
    kin_model::Relation {
        id: kin_model::RelationId::new(),
        kind: kin_model::RelationKind::References,
        src: kin_model::GraphNodeId::Entity(src),
        dst: kin_model::GraphNodeId::Entity(dst),
        confidence: 1.0,
        origin: kin_model::relation::RelationOrigin::Lsp,
        created_in: None,
        import_source: None,
        evidence: Vec::new(),
    }
}

/// Admit and commit a unit the reads below never ask about, so the sweep's
/// writes into it move graph authority without changing their answers.
async fn admit_sweep_churn_unit(
    repo: &tempfile::TempDir,
    state: &Arc<DaemonState>,
) -> (kin_model::EntityId, kin_model::EntityId) {
    std::fs::write(repo.path().join("churn.py"), SWEEP_CHURN_UNIT).unwrap();
    waiting_admit(state, "sweep churn unit").await;
    waiting_commit(state, "Commit the sweep churn unit").await;
    (
        waiting_entity(state, "churn.py", "churn_source").id,
        waiting_entity(state, "churn.py", "churn_target").id,
    )
}

/// One enrichment write through the product's write path: a relation buffered
/// and flushed through a captured universe, the calls a sweep makes. `None`
/// when the capture was refused as stale and must be taken again.
async fn sweep_churn_write(
    state: &Arc<DaemonState>,
    inputs: &crate::daemon::lsp_publication::QueryInputs,
    src: kin_model::EntityId,
    dst: kin_model::EntityId,
) -> Option<kin_model::RelationId> {
    let relation = sweep_churn_relation(src, dst);
    let mut pending = crate::daemon::PendingEnrichment::default();
    inputs
        .absorb(state, &mut pending, vec![relation.clone()])
        .await
        .ok()?;
    inputs.flush(state, &mut pending).await.ok()?;
    Some(relation.id)
}

/// An enrichment sweep landing writes back to back through the product's write
/// path until stopped, with no gap a read could fit in by luck.
struct SweepChurn {
    stop: Arc<std::sync::atomic::AtomicBool>,
    handle: tokio::task::JoinHandle<u64>,
}

impl SweepChurn {
    fn start(
        state: &Arc<DaemonState>,
        src: kin_model::EntityId,
        dst: kin_model::EntityId,
    ) -> Self {
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let handle = {
            let state = Arc::clone(state);
            let stop = Arc::clone(&stop);
            tokio::spawn(async move {
                let mut writes = 0_u64;
                let mut inputs = None;
                while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                    if inputs.is_none() {
                        inputs = crate::daemon::lsp_publication::QueryInputs::capture(&state)
                            .await
                            .ok();
                    }
                    let Some(captured) = inputs.as_ref() else {
                        tokio::task::yield_now().await;
                        continue;
                    };
                    match sweep_churn_write(&state, captured, src, dst).await {
                        Some(_) => writes += 1,
                        None => inputs = None,
                    }
                    tokio::task::yield_now().await;
                }
                writes
            })
        };
        Self { stop, handle }
    }

    /// Stop the sweep, answering how many writes it landed.
    async fn stop(self) -> u64 {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        self.handle.await.unwrap()
    }
}

async fn read_through_route(
    state: &Arc<DaemonState>,
    path: &str,
    body: serde_json::Value,
) -> (StatusCode, String) {
    let response = router(Arc::clone(state))
        .oneshot(
            Request::post(path)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&body).into_owned())
}

/// The field report: a store whose language-server enrichment ran after
/// conversion answered queries with 503 while that sweep ran. Every read that
/// claimed an absence, and every impact read while enrichment was owed, had to
/// be certified against a graph that held still, and the sweep moved graph
/// authority every few milliseconds for minutes, so each read waited out its
/// whole writer limit and refused.
///
/// The sweep here lands its writes back to back through the product's write
/// path. Every read answers from the current graph, the answers that depend on
/// enrichment say it is owed, and the sweep keeps landing writes between the
/// reads. Without turns the first read refuses.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reads_answer_while_an_enrichment_sweep_writes_and_say_enrichment_is_owed() {
    let _readiness = kin_mcp::edge_coverage::test_support::scoped_language_servers(&[
        kin_model::LanguageId::Python,
    ]);
    let (repo, state) = refs_source_observation_fixture().await;
    let (churn_source, churn_target) = admit_sweep_churn_unit(&repo, &state).await;
    state
        .lsp_sweep_running
        .store(true, std::sync::atomic::Ordering::SeqCst);
    // Short, so a read the sweep keeps spanning refuses in seconds, not 45 s.
    state.set_xref_writer_drain_ceiling_for_test(Duration::from_millis(1_500));
    let unused = waiting_entity(&state, "unused.py", "unused_probe");
    let work = waiting_entity(&state, "local.py", "work");

    let churn = SweepChurn::start(&state, churn_source, churn_target);
    let reads = [
        (
            "/commands/refs",
            json!({"entity": unused.id.to_string(), "kind": "calls"}),
        ),
        (
            "/commands/refs",
            json!({"entity": work.id.to_string(), "kind": "calls"}),
        ),
        (
            "/impact",
            json!({"entity": work.id.to_string(), "depth": 3}),
        ),
        (
            "/locate",
            json!({"text": "no_such_function_anywhere", "max_files": 10,
                   "max_files_explicit": true, "entity_surface": true}),
        ),
        (
            "/search",
            json!({"query": "no_such_function_anywhere", "semantic": false}),
        ),
        (
            "/context",
            json!({"entity": "no_such_function_anywhere", "budget": "8k"}),
        ),
        (
            "/trace",
            json!({"entity": "no_such_function_anywhere", "json": false, "compact": false,
                   "budget": "8k", "max_lines": 20, "nearby_limit": 2, "transitive_limit": 1}),
        ),
    ];
    let mut answers = Vec::new();
    for (path, body) in reads {
        let (status, answer) = read_through_route(&state, path, body).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "{path} must answer while the sweep writes: {answer}"
        );
        answers.push((path, answer));
    }

    // The answers that depend on enrichment say it is still owed.
    let refs_work: serde_json::Value = serde_json::from_str(&answers[1].1).unwrap();
    assert_eq!(refs_work["call_sites"]["settled"], false, "{refs_work:#}");
    assert!(
        refs_work["call_sites"]["callers_owed_enrichment"]
            .as_u64()
            .is_some_and(|owed| owed > 0),
        "{refs_work:#}"
    );
    assert!(
        refs_work["call_sites"]["clauses"]
            .as_array()
            .is_some_and(|clauses| clauses.iter().any(|clause| clause
                .as_str()
                .is_some_and(|c| c.starts_with("call_sites_owed")))),
        "{refs_work:#}"
    );
    let impact: serde_json::Value = serde_json::from_str(&answers[2].1).unwrap();
    assert!(
        !impact["enrichment"].is_null(),
        "the impact answer carries the owed call-site evidence it was read with: {impact:#}"
    );

    // The MCP tools answer the same way, and their verdict is not complete.
    let mcp = mcp_call(
        router(Arc::clone(&state)),
        "find_references",
        json!({"entity_id": work.id.to_string(), "relation_kinds": ["calls"], "answer_only": false}),
    )
    .await;
    assert_ne!(mcp.is_error, Some(true), "{}", mcp_result_text(&mcp));
    let envelope = kin_mcp::Envelope::daemon().with_health(&daemon_health_snapshot(&state).await);
    let finalized = tool_result_payload(&kin_mcp::envelope::finalize(
        mcp,
        envelope,
        "find_references",
    ));
    assert_ne!(
        finalized["_kin"]["verdict"]["state"], "complete",
        "an answer read while enrichment is owed is not complete: {finalized:#}"
    );
    let impact_mcp = mcp_call(
        router(Arc::clone(&state)),
        "impact_analysis",
        json!({"entity_ids": [work.id.to_string()], "include_traffic": false}),
    )
    .await;
    assert_ne!(
        impact_mcp.is_error,
        Some(true),
        "{}",
        mcp_result_text(&impact_mcp)
    );
    let absent = mcp_call(
        router(Arc::clone(&state)),
        "find_references",
        json!({"entity_id": unused.id.to_string(), "relation_kinds": ["calls"]}),
    )
    .await;
    assert_ne!(absent.is_error, Some(true), "{}", mcp_result_text(&absent));

    // The sweep was not held off while the reads were served.
    let landed = churn.stop().await;
    assert!(
        landed > answers.len() as u64,
        "the sweep keeps writing between reads: {landed} writes"
    );
}

/// A read nested inside another read of the same task, as a tool's own read
/// runs inside the MCP dispatcher's analysis attempt, waits for a reconcile
/// pass to publish. The outer read's pass used to hold the sweep off for that
/// whole wait. The wait now lets the task's whole hold go, and an enrichment
/// write lands during it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_nested_read_that_waits_does_not_hold_the_sweep_off() {
    let (repo, state) = refs_source_observation_fixture().await;
    let (churn_source, churn_target) = admit_sweep_churn_unit(&repo, &state).await;
    state.set_xref_writer_drain_ceiling_for_test(Duration::from_secs(20));
    let inputs = crate::daemon::lsp_publication::QueryInputs::capture(&state)
        .await
        .unwrap_or_else(|_| panic!("the fixture's graph is a capturable universe"));

    let (sampled_tx, sampled_rx) = tokio::sync::oneshot::channel();
    let (admission_tx, admission_rx) = tokio::sync::oneshot::channel::<()>();
    let reader = {
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            let mut outer = NameReadWait::begin(&state, &state.graph);
            let _epoch = outer
                .sample_analysis(&state, &state.graph, RequestGraphAuthority::Head)
                .await;
            let mut inner = NameReadWait::begin(&state, &state.graph);
            let _inner_epoch = inner.sample(&state).await;
            sampled_tx.send(()).unwrap();
            admission_rx.await.unwrap();
            // The inner read waits for the reconcile pass to publish.
            let waited = inner.wait_out(&state, "nested read").await;
            drop(inner);
            drop(outer);
            waited
        })
    };
    sampled_rx.await.unwrap();
    let admission = state.begin_pending_admission();
    admission_tx.send(()).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let written = tokio::time::timeout(
        Duration::from_secs(3),
        sweep_churn_write(&state, &inputs, churn_source, churn_target),
    )
    .await
    .expect("a nested read waiting for a reconcile pass must not hold the sweep off")
    .expect("the write validated and landed");
    assert!(state.graph.get_relation_by_id(&written).is_some());
    assert!(
        !reader.is_finished(),
        "the nested read is still waiting for the reconcile pass"
    );

    drop(admission);
    tokio::time::timeout(Duration::from_secs(5), reader)
        .await
        .expect("the nested read's wait ends once the reconcile pass publishes")
        .unwrap()
        .expect("the wait authorizes another attempt");
}

/// Reads from parallel agents that overlap without a gap used to keep the
/// sweep from ever writing, and each attempt it made held the coordination
/// gate for a quarter second. Now the write waits for reads holding nothing,
/// lands within its patience plus one read, and the reads go on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn overlapping_reads_do_not_hold_the_sweep_off_or_the_gate() {
    let (repo, state) = refs_source_observation_fixture().await;
    let (churn_source, churn_target) = admit_sweep_churn_unit(&repo, &state).await;
    let inputs = Arc::new(
        crate::daemon::lsp_publication::QueryInputs::capture(&state)
            .await
            .unwrap_or_else(|_| panic!("the fixture's graph is a capturable universe")),
    );

    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reads = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let mut readers = Vec::new();
    for offset in 0..3_u64 {
        let state = Arc::clone(&state);
        let stop = Arc::clone(&stop);
        let reads = Arc::clone(&reads);
        readers.push(tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(7 * offset)).await;
            while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                let pass = state.enter_live_read(state.graph.as_ref()).await;
                tokio::time::sleep(Duration::from_millis(20)).await;
                drop(pass);
                reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }));
    }
    tokio::time::sleep(Duration::from_millis(60)).await;

    for _ in 0..3 {
        let started = tokio::time::Instant::now();
        let write = {
            let state = Arc::clone(&state);
            let inputs = Arc::clone(&inputs);
            tokio::spawn(async move {
                sweep_churn_write(&state, &inputs, churn_source, churn_target).await
            })
        };
        // While the write waits for the reads in flight, reconcile passes and
        // commits still get the coordination gate.
        while started.elapsed() < Duration::from_millis(200) {
            assert!(
                state.coordination_gate.try_lock().is_ok(),
                "a write waiting for reads must not hold the coordination gate"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let written = tokio::time::timeout(Duration::from_secs(3), write)
            .await
            .expect("overlapping reads must not hold the sweep off")
            .unwrap()
            .expect("the write validated and landed");
        assert!(state.graph.get_relation_by_id(&written).is_some());
        let before = reads.load(std::sync::atomic::Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(
            reads.load(std::sync::atomic::Ordering::SeqCst) > before,
            "the reads go on after the write"
        );
    }
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    for reader in readers {
        reader.await.unwrap();
    }
    assert_eq!(state.live_read_gate_for_test().readers_in_flight(), 0);
}

/// Prove `run` calls `work` and retract that proof again, as one unit of a
/// sweep whose answers settle the site otherwise: the proof flushed, then the
/// file's guesses settled and its ledgers installed, retracting it.
async fn prove_and_retract_in_one_unit(
    state: &Arc<DaemonState>,
    repo: &tempfile::TempDir,
    proof: &kin_model::Relation,
    context: &kin_model::ProofContext,
) {
    use crate::daemon::lsp_publication::{LedgerRequest, QueryInputs};
    let inputs = QueryInputs::capture(state).await.unwrap();
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
    let mut pending = crate::daemon::PendingEnrichment::default();
    let unit = inputs.write_unit(state).await;
    unit.absorb(&mut pending, vec![proof.clone()]).unwrap();
    unit.flush(&mut pending).unwrap();
    assert!(
        state.graph.get_relation_by_id(&proof.id).is_some(),
        "the unit installs the proof"
    );
    unit.settle_file(
        LSP_PUBLICATION_CALLER,
        &[],
        &kin_lsp::call_sites::ExternalNames::new(),
        context,
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
    .unwrap();
    drop(unit);
    assert!(
        state.graph.get_relation_by_id(&proof.id).is_none(),
        "the unit's ledgers retract the proof they do not hold"
    );
}

/// A unit's flush and its settlement are one turn. When they were two, a read
/// waiting for the flush to land began before the settlement and saw proven
/// edges the settlement then retracted. A unit that installs a proof and
/// retracts it again must look, to every read, as if it changed nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_read_never_sees_a_unit_between_its_flush_and_its_settlement() {
    let (repo, state) = lsp_publication_fixture().await;
    stage_lsp_publication_completion(&state, &["caller.py", "target.py"]);
    let caller = waiting_entity(&state, "caller.py", "run");
    let target = waiting_entity(&state, "target.py", "work");
    let start = LSP_PUBLICATION_CALLER.find("callback()").unwrap();
    let mut proof = lsp_publication_call(caller.id, target.id);
    proof.id = kin_lsp::relation_identity::language_server_relation_id(
        RelationKind::Calls,
        caller.id,
        target.id,
    );
    proof.evidence[0].source_span = Some(kin_model::SourceSpan {
        file: kin_model::FilePathId::new("caller.py"),
        start_byte: start,
        end_byte: start + "callback".len(),
        start_line: 1,
        start_col: 11,
        end_line: 1,
        end_col: 19,
    });
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
        .expect("the staged completion validated its proof context");

    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reader = {
        let state = Arc::clone(&state);
        let stop = Arc::clone(&stop);
        let proof = proof.id;
        tokio::spawn(async move {
            let (mut reads, mut saw_half_written) = (0_u64, 0_u64);
            while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                let pass = state.enter_live_read(state.graph.as_ref()).await;
                if state.graph.get_relation_by_id(&proof).is_some() {
                    saw_half_written += 1;
                }
                drop(pass);
                reads += 1;
                tokio::task::yield_now().await;
            }
            (reads, saw_half_written)
        })
    };
    tokio::time::timeout(Duration::from_secs(60), async {
        for _ in 0..20 {
            prove_and_retract_in_one_unit(&state, &repo, &proof, &context).await;
        }
    })
    .await
    .expect("the sweep's units land between the reads");
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let (reads, saw_half_written) = reader.await.unwrap();
    assert!(reads > 0);
    assert_eq!(
        saw_half_written, 0,
        "{saw_half_written} of {reads} reads saw a proof its unit had not yet retracted"
    );
}

/// The product's enrichment write path takes its turn: a flush of language
/// server relations does not land while a read attempt is in flight, holds no
/// later read back inside its patience or the coordination gate while it
/// waits, and lands once the attempt ends.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_enrichment_write_lands_between_read_attempts_never_inside_one() {
    let (_repo, state) = refs_source_observation_fixture().await;
    let caller = waiting_entity(&state, "caller.py", "run");
    let target = waiting_entity(&state, "unused.py", "unused_probe");
    let inputs = Arc::new(
        crate::daemon::lsp_publication::QueryInputs::capture(&state)
            .await
            .unwrap_or_else(|_| panic!("the fixture's graph is a capturable universe")),
    );
    let relation = sweep_churn_relation(caller.id, target.id);
    let mut pending = crate::daemon::PendingEnrichment::default();
    inputs
        .absorb(&state, &mut pending, vec![relation.clone()])
        .await
        .unwrap_or_else(|_| panic!("a partial batch buffers"));
    assert!(state.graph.get_relation_by_id(&relation.id).is_none());

    let reading = state
        .enter_live_read(state.graph.as_ref())
        .await
        .expect("the live graph takes passes");
    let flush = {
        let state = Arc::clone(&state);
        let inputs = Arc::clone(&inputs);
        tokio::spawn(async move { inputs.flush(&state, &mut pending).await.is_ok() })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !flush.is_finished(),
        "the write must not land inside a read"
    );
    assert!(state.graph.get_relation_by_id(&relation.id).is_none());
    // Inside its patience the waiting write holds neither later reads nor the
    // coordination gate.
    let later = {
        let state = Arc::clone(&state);
        tokio::spawn(async move { drop(state.enter_live_read(state.graph.as_ref()).await) })
    };
    tokio::time::timeout(Duration::from_millis(150), later)
        .await
        .expect("a write inside its patience holds no read back")
        .unwrap();
    drop(
        state
            .coordination_gate
            .try_lock()
            .expect("a write waiting for its turn does not hold the coordination gate"),
    );

    drop(reading);
    let landed = tokio::time::timeout(Duration::from_secs(10), flush)
        .await
        .expect("the write lands once the read attempt ends")
        .unwrap();
    assert!(landed, "the flush validated and wrote");
    assert!(state.graph.get_relation_by_id(&relation.id).is_some());
    assert_eq!(state.live_read_gate_for_test().readers_in_flight(), 0);
}
