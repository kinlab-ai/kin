// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

// Controlled JSON-RPC peer, not an installed language server or semantic oracle.
// It exercises the production generator against captured admitted CAS text.
const LSP_SOURCE_SPANS_PEER: &str = r#"
import json,sys
responses=json.loads(sys.argv[1])
while True:
    headers={}
    while True:
        line=sys.stdin.buffer.readline()
        if not line: sys.exit(0)
        if line in (b'\n',b'\r\n'): break
        key,value=line.decode().split(':',1);headers[key.lower()]=value.strip()
    msg=json.loads(sys.stdin.buffer.read(int(headers['content-length'])))
    if 'id' not in msg: continue
    result=responses.get(msg['method'])
    data=json.dumps({'jsonrpc':'2.0','id':msg['id'],'result':result}).encode()
    sys.stdout.buffer.write(f'Content-Length: {len(data)}\r\n\r\n'.encode()+data);sys.stdout.buffer.flush()
"#;

#[tokio::test]
async fn fresh_lsp_source_spans_preserve_prior_call_through_removal_and_cold_start() {
    use crate::daemon::lsp_publication::QueryInputs;
    const CALLER: &str = "def run(callback):\r\n    marker = \"😀\"; return callback()\r\n";
    let (repo, state) = mcp_lifecycle_fixture();
    std::fs::write(repo.path().join("caller.py"), CALLER).unwrap();
    std::fs::write(repo.path().join("target.py"), "def work():\n    return 7\n").unwrap();
    waiting_admit(&state, "fresh LSP sources").await;
    waiting_commit(&state, "fresh LSP source baseline").await;
    let caller = waiting_entity(&state, "caller.py", "run");
    let target = waiting_entity(&state, "target.py", "work");
    let make_ref = |entity: &kin_model::Entity, name_col| {
        let span = entity.span.as_ref().unwrap();
        kin_lsp::EntityRef {
            id: entity.id,
            name: entity.name.clone(),
            file_path: entity.file_origin.as_ref().unwrap().0.clone(),
            start_line: span.start_line,
            start_col: span.start_col,
            end_line: span.end_line,
            name_line: span.start_line,
            name_col,
            declares_name: true,
            declares_only: false,
            kind: kin_model::EntityKind::Function,
        }
    };
    let caller_ref = make_ref(&caller, 4);
    let target_ref = make_ref(&target, 4);
    let index =
        kin_lsp::EntityIndex::new(vec![caller_ref.clone(), target_ref.clone()], repo.path());
    let inputs = QueryInputs::capture(&state).await.unwrap();
    let text = inputs.document("caller.py").unwrap();
    assert_eq!(text, CALLER);
    let source_digest = kin_model::Hash256::from_bytes(kin_blobs::digest(CALLER.as_bytes()).0);
    let source_artifact = state
        .graph
        .resolved_tree()
        .artifact_at_path(&kin_model::RepoPath::from_utf8("caller.py").unwrap())
        .unwrap()
        .artifact_id;
    let item = |entity: &kin_lsp::EntityRef| {
        json!({
            "name":entity.name,"kind":12,
            "uri":kin_lsp::protocol::path_to_uri(&repo.path().join(&entity.file_path)),
            "range":{"start":{"line":entity.start_line,"character":0},"end":{"line":entity.end_line,"character":0}},
            "selectionRange":{"start":{"line":entity.name_line,"character":entity.name_col},"end":{"line":entity.name_line,"character":entity.name_col+entity.name.len() as u32}},
        })
    };
    let line = CALLER.lines().nth(1).unwrap();
    let column = line.find("callback").unwrap();
    let utf16 = line[..column].encode_utf16().count();
    let responses = json!({
        "initialize":{"capabilities":{"callHierarchyProvider":true,"positionEncoding":"utf-16"}},
        "textDocument/prepareCallHierarchy":[item(&caller_ref)],
        "callHierarchy/outgoingCalls":[{"to":item(&target_ref),"fromRanges":[{
            "start":{"line":1,"character":utf16},"end":{"line":1,"character":utf16+8}
        }]}]
    })
    .to_string();
    let server = kin_lsp::lifecycle::LspServer::start(
        "python3",
        &["-u", "-c", LSP_SOURCE_SPANS_PEER, &responses],
        repo.path(),
        None,
        None,
    )
    .await
    .unwrap();
    server
        .client
        .notify(
            "textDocument/didOpen",
            json!({"textDocument":{
                "uri":kin_lsp::protocol::path_to_uri(&repo.path().join("caller.py")),
                "languageId":"python","version":1,"text":text,
            }}),
        )
        .await
        .unwrap();
    let generated = kin_lsp::enrichment::enrich_entity_calls(
        &server,
        &caller_ref,
        &index,
        repo.path(),
        Some(&|path| inputs.document(path)),
    )
    .await
    .unwrap()
    .relations;
    server.shutdown().await.unwrap();
    assert_eq!(generated.len(), 1);
    let prior = generated[0].clone();
    let span = prior.evidence[0].source_span.as_ref().unwrap();
    assert_eq!(&CALLER[span.start_byte..span.end_byte], "callback");
    assert_eq!(span.start_byte, CALLER.rfind("callback").unwrap());
    assert_eq!(span.start_col, column as u32);
    assert_eq!(prior.origin, kin_model::RelationOrigin::Lsp);
    let mut pending = crate::daemon::PendingEnrichment::default();
    inputs
        .absorb(&state, &mut pending, generated)
        .await
        .unwrap();
    inputs.flush(&state, &mut pending).await.unwrap();
    assert_eq!(
        state.graph.semantic_observation().relations.get(&prior.id),
        Some(&prior)
    );
    state.save_snapshot().unwrap();
    assert_eq!(
        lsp_publication_durable(&state).relations.get(&prior.id),
        Some(&prior)
    );
    // This answer was already accepted and durable before ordinary removal;
    // the previous late-answer race is not the causal mechanism here.
    std::fs::remove_file(repo.path().join("target.py")).unwrap();
    waiting_admit(
        &state,
        "ordinary target deletion after fresh LSP publication",
    )
    .await;
    waiting_commit(&state, "retain exact fresh LSP prior binding").await;
    let check = |snapshot: &kin_db::GraphSnapshot| {
        assert!(!snapshot.entities.contains_key(&target.id));
        assert!(!snapshot.relations.contains_key(&prior.id));
        let relation = snapshot
            .relations
            .get(&kin_index::binding_debt::local_binding_debt_id(
                source_artifact,
            ))
            .unwrap();
        let debt = kin_index::binding_debt::decode_local_binding_debt(
            &kin_model::FilePathId::new("caller.py"),
            source_artifact,
            relation,
        )
        .unwrap()
        .unwrap();
        assert_eq!(debt.obligations.len(), 1);
        assert_eq!(debt.observed_source_digest, source_digest);
        assert_eq!(debt.obligations[0].source_digest, source_digest);
        assert_eq!(debt.obligations[0].retired_relation, prior);
        assert_eq!(debt.obligations[0].target_file.0, "target.py");
        debt
    };
    let warm = check(&state.graph.semantic_observation());
    let durable = check(&lsp_publication_durable(&state));
    assert_eq!(warm, durable);
    binding_disclosure_impact_with_coverage(
        &state,
        caller.id,
        true,
        false,
        "fresh LSP span warm removal",
    )
    .await;
    let layout = state.layout.clone();
    drop(inputs);
    drop(state);
    let cold = waiting_cold_start(layout).await;
    let reopened = check(&cold.graph.semantic_observation());
    assert_eq!(warm, reopened);
    binding_disclosure_impact_with_coverage(
        &cold,
        caller.id,
        true,
        false,
        "fresh LSP span canonical cold startup",
    )
    .await;
    assert_eq!(
        std::fs::read_to_string(repo.path().join("caller.py")).unwrap(),
        CALLER
    );
    println!(
        "fresh LSP source span lifecycle: {}",
        json!({
            "fixture":"controlled protocol through production enrichment",
            "prior_relation":prior,"source_digest":source_digest,"warm_debt":warm,
            "durable_debt":durable,"cold_debt":reopened,
            "legacy_migration":false,"settlement_proved":false,
        })
    );
}

#[tokio::test]
async fn an_unknown_noncall_definition_preserves_its_positive_reference_after_publication() {
    noncall_reference_continuity_control(
        true,
        kin_lsp::call_sites::UnprovenAnswer::NoAnswer,
        false,
    )
    .await;
}

#[tokio::test]
async fn a_disagreed_noncall_definition_preserves_uncertainty_after_publication() {
    noncall_reference_continuity_control(
        true,
        kin_lsp::call_sites::UnprovenAnswer::AnswersDisagree,
        false,
    )
    .await;
}

#[tokio::test]
async fn a_zero_call_reference_gap_survives_a_crash_before_marker_publication() {
    noncall_reference_continuity_control(
        false,
        kin_lsp::call_sites::UnprovenAnswer::NoAnswer,
        true,
    )
    .await;
}

#[tokio::test]
async fn a_timed_out_noncall_definition_preserves_retryable_uncertainty_after_publication() {
    noncall_reference_continuity_control(true, kin_lsp::call_sites::UnprovenAnswer::Timeout, false)
        .await;
}

/// A reference site no pass can decide stops keeping its store owed. The first
/// finished pass over a unit leaves the site owed; a second over the same bytes
/// and proof context is the last the bound allows, so it records the site as
/// undecided: the positive Reference is still served, its evidence names the
/// site as undecided, the unit owes nothing, and it can be recorded finished.
#[tokio::test]
async fn an_undecided_reference_site_settles_after_the_bounded_passes() {
    use crate::daemon::lsp_publication::{LedgerRequest, QueryInputs};
    use kin_lsp::call_sites::{ExternalNames, UnprovenAnswer, UnprovenSite};
    const BODY: &str = "def run(logs):\n    logs.errorsCount = 0\n";
    let (repo, state) = mcp_lifecycle_fixture();
    std::fs::write(repo.path().join("reference.py"), BODY).unwrap();
    std::fs::write(
        repo.path().join("logs.py"),
        "class ComponentLogs:\n    errorsCount: int\n",
    )
    .unwrap();
    waiting_admit(&state, "admitted an undecided reference body").await;
    waiting_commit(&state, "undecided reference baseline").await;
    let caller = waiting_entity(&state, "reference.py", "run");
    let target = waiting_entity(&state, "logs.py", "ComponentLogs");
    let start = BODY.find("errorsCount").unwrap();
    let span = kin_model::SourceSpan {
        file: kin_model::FilePathId::new("reference.py"),
        start_byte: start,
        end_byte: start + "errorsCount".len(),
        start_line: 1,
        start_col: 9,
        end_line: 1,
        end_col: 20,
    };
    let prior = kin_model::Relation {
        id: kin_model::language_server_relation_id(RelationKind::References, caller.id, target.id),
        kind: RelationKind::References,
        src: GraphNodeId::Entity(caller.id),
        dst: GraphNodeId::Entity(target.id),
        confidence: 0.95,
        origin: kin_model::RelationOrigin::Lsp,
        created_in: None,
        import_source: None,
        evidence: vec![kin_model::RelationEvidence {
            source_span: Some(span.clone()),
            parser_rule: Some(kin_lsp::call_sites::DEFINITION_RULE.to_owned()),
            ..Default::default()
        }],
    };
    let inputs = QueryInputs::capture(&state).await.unwrap();
    let mut pending = crate::daemon::PendingEnrichment::default();
    inputs
        .absorb(&state, &mut pending, vec![prior.clone()])
        .await
        .unwrap();
    inputs.flush(&state, &mut pending).await.unwrap();
    // Published once, which validates the language's proof context the way a
    // finished sweep does.
    state.save_snapshot_reporting_enrichment().unwrap();
    stage_lsp_publication_completion(&state, &["reference.py", "logs.py"]);
    state.save_snapshot_reporting_enrichment().unwrap();
    let inputs = QueryInputs::capture(&state).await.unwrap();
    inputs.flush(&state, &mut pending).await.unwrap();
    drop(inputs);
    let context = state
        .graph
        .get_resolution_record(&kin_model::ResolutionRecordId::context_validation(
            kin_model::LanguageId::Python,
        ))
        .and_then(|record| match record {
            kin_model::ResolutionRecord::ContextValidation(v) => match v.state {
                kin_model::ContextValidationState::Validated { context } => Some(context),
                _ => None,
            },
            _ => None,
        })
        .unwrap();
    let uri = kin_lsp::protocol::path_to_uri(&repo.path().join("reference.py"));
    let unproven = [UnprovenSite {
        source: caller.id,
        start_byte: start,
        end_byte: span.end_byte,
        answer: UnprovenAnswer::NoAnswer,
    }];
    let pass = |inputs: QueryInputs| {
        let state = Arc::clone(&state);
        let context = context.clone();
        let uri = uri.clone();
        let repo = repo.path().to_path_buf();
        let unproven = unproven.to_vec();
        async move {
            let entities = inputs
                .entities
                .iter()
                .filter(|e| e.file_origin.as_ref().is_some_and(|p| p.0 == "reference.py"))
                .collect::<Vec<_>>();
            let index = kin_lsp::EntityIndex::new(
                inputs
                    .entities
                    .iter()
                    .filter_map(|e| crate::daemon::lsp_entity_ref(e, &e.file_origin.as_ref()?.0))
                    .collect(),
                &repo,
            );
            let produced = Vec::new();
            let (_, ledgers) = inputs
                .settle_file(
                    &state,
                    BODY,
                    &[],
                    &ExternalNames::new(),
                    &context,
                    LedgerRequest {
                        file: "reference.py",
                        uri: &uri,
                        index: &index,
                        entities: &entities,
                        unproven: &unproven,
                        produced_references: &produced,
                        ending: crate::call_site_ledger::PassEnding::Complete,
                        not_in_build: false,
                        retract: true,
                    },
                )
                .await
                .unwrap();
            (inputs, ledgers)
        }
    };

    // The first finished pass: the site is owed.
    let (inputs, first) = pass(QueryInputs::capture(&state).await.unwrap()).await;
    assert_eq!(
        (first.reference_uncertainty.sites, first.reference_uncertainty.settled),
        (1, 0)
    );
    {
        let owed = state.lsp_owed_files.lock().unwrap();
        let entry = &owed["reference.py"];
        assert!(entry.reference_uncertainty && !entry.reference_retryable);
        assert_eq!(entry.attempts, 1);
    }
    assert!(!inputs.record_file_completed(&state, "reference.py").await.unwrap());
    drop(inputs);

    // The second, over the same bytes and context: the site is undecided and
    // the unit owes nothing.
    let (inputs, second) = pass(QueryInputs::capture(&state).await.unwrap()).await;
    assert_eq!(
        (second.reference_uncertainty.sites, second.reference_uncertainty.settled),
        (0, 1)
    );
    assert!(!state.lsp_owed_files.lock().unwrap().contains_key("reference.py"));
    let held = state
        .graph
        .semantic_observation()
        .relations
        .get(&prior.id)
        .cloned()
        .expect("the positive Reference is still served");
    assert_eq!(held.evidence.len(), 1);
    assert_eq!(held.evidence[0].source_span, Some(span));
    assert_eq!(
        held.evidence[0].parser_rule.as_deref(),
        Some(kin_model::LSP_REFERENCE_UNDECIDED_RULE)
    );
    assert!(inputs.record_file_completed(&state, "reference.py").await.unwrap());
    let status = reference_uncertainty_status(&state).await;
    assert!(
        !status["owed_files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["file"] == "reference.py"),
        "{status}"
    );

    // The answer still serves the reference, lists the undecided site by its
    // caller and its line inside it, and is never complete because of it.
    let references = tool_result_payload(
        &mcp_call(
            router(Arc::clone(&state)),
            "find_references",
            json!({"entity_id":target.id, "relation_kinds":["references"],
                "answer_only":false, "max_chars":60000}),
        )
        .await,
    );
    assert_eq!(references["references"][0]["entity_id"], caller.id.to_string(), "{references}");
    let undecided = &references["undecided_reference_sites"];
    assert_eq!(undecided["count"], 1, "{references}");
    assert_eq!(undecided["sites"][0]["caller"], caller.id.to_string(), "{references}");
    assert_eq!(undecided["sites"][0]["caller_name"], "run", "{references}");
    assert_eq!(undecided["sites"][0]["line_in_entity"], 1, "{references}");
    let verdict = &references["_kin"]["verdict"];
    assert_ne!(verdict["state"], "complete", "{references}");
    assert_ne!(verdict["state"], "certified", "{references}");
    assert!(
        verdict["limiting_factor"]
            .as_str()
            .is_some_and(|factor| factor.contains("reference_sites_undecided_listed")),
        "{references}"
    );
}

async fn reference_uncertainty_status(state: &Arc<DaemonState>) -> serde_json::Value {
    let response = router(Arc::clone(state))
        .oneshot(
            Request::get("/lsp/sweep/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 65536)
        .await
        .unwrap();
    serde_json::from_slice(&body).unwrap()
}

#[tokio::test]
async fn reference_uncertainty_status_preserves_legacy_retry_metadata() {
    let (_repo, state) = mcp_lifecycle_fixture();
    let legacy = serde_json::from_value(json!({"legacy.py":{"blob":"body","attempts":1,"last_attempt_unix_s":1,"reason":"legacy timeout"}})).unwrap();
    *state.lsp_owed_files.lock().unwrap() = legacy;
    let status = reference_uncertainty_status(&state).await;
    let row = &status["owed_files"][0];
    assert_eq!(row["reference_uncertainty"], false);
    assert_eq!(row["automatic_retry"], true);
    assert!(row["retry_after_s"].as_u64().is_some());
}

async fn noncall_reference_continuity_control(
    with_call: bool,
    answer: kin_lsp::call_sites::UnprovenAnswer,
    crash_before_publication: bool,
) {
    use crate::daemon::lsp_publication::{LedgerRequest, QueryInputs};
    use kin_lsp::call_sites::{ExternalNames, SiteAnswer, SiteTarget, UnprovenSite};
    let retryable = answer == kin_lsp::call_sites::UnprovenAnswer::Timeout;
    const BODY: &str = "def run(logs, callback):\n    logs.errorsCount = 0\n    callback()\n";
    let body = if with_call {
        BODY
    } else {
        "def run(logs, callback):\n    logs.errorsCount = 0\n"
    };
    let (repo, state) = mcp_lifecycle_fixture();
    std::fs::write(repo.path().join("reference.py"), body).unwrap();
    std::fs::write(
        repo.path().join("logs.py"),
        "class ComponentLogs:\n    errorsCount: int\n",
    )
    .unwrap();
    waiting_admit(&state, "admitted noncall reference bodies").await;
    waiting_commit(&state, "noncall reference baseline").await;
    let caller = waiting_entity(&state, "reference.py", "run");
    let target = waiting_entity(&state, "logs.py", "ComponentLogs");
    let start = body.find("errorsCount").unwrap();
    let span = kin_model::SourceSpan {
        file: kin_model::FilePathId::new("reference.py"),
        start_byte: start,
        end_byte: start + "errorsCount".len(),
        start_line: 1,
        start_col: 9,
        end_line: 1,
        end_col: 20,
    };
    let prior = kin_model::Relation {
        id: kin_model::language_server_relation_id(RelationKind::References, caller.id, target.id),
        kind: RelationKind::References,
        src: GraphNodeId::Entity(caller.id),
        dst: GraphNodeId::Entity(target.id),
        confidence: 0.95,
        origin: kin_model::RelationOrigin::Lsp,
        created_in: None,
        import_source: None,
        evidence: vec![kin_model::RelationEvidence {
            source_span: Some(span.clone()),
            parser_rule: Some(kin_lsp::call_sites::DEFINITION_RULE.to_owned()),
            ..Default::default()
        }],
    };
    let inputs = QueryInputs::capture(&state).await.unwrap();
    assert_eq!(inputs.document("reference.py").as_deref(), Some(body));
    let mut pending = crate::daemon::PendingEnrichment::default();
    inputs
        .absorb(&state, &mut pending, vec![prior.clone()])
        .await
        .unwrap();
    inputs.flush(&state, &mut pending).await.unwrap();
    state.save_snapshot_reporting_enrichment().unwrap();
    stage_lsp_publication_completion(&state, &["reference.py", "logs.py"]);
    state.save_snapshot_reporting_enrichment().unwrap();
    let inputs = QueryInputs::capture(&state).await.unwrap();
    inputs.flush(&state, &mut pending).await.unwrap();
    assert_lsp_publication_native_completion(&state, &["reference.py", "logs.py"]);
    let native_marks = |state: &DaemonState| {
        let context =
            crate::local_repository_authority::LocalRepositoryAuthorityContext::from_state(state)
                .unwrap();
        let authority = context.open().unwrap();
        let marks = authority
            .read_authority()
            .workspace_enrichment_marks(&context.workspace_id())
            .to_vec();
        marks
    };
    let unaffected = native_marks(&state)
        .into_iter()
        .find(|mark| mark.path == "logs.py")
        .unwrap();
    assert_eq!(
        lsp_publication_durable(&state).relations.get(&prior.id),
        Some(&prior)
    );
    let entities = inputs
        .entities
        .iter()
        .filter(|e| {
            e.file_origin
                .as_ref()
                .is_some_and(|p| p.0 == "reference.py")
        })
        .collect::<Vec<_>>();
    let index = kin_lsp::EntityIndex::new(
        inputs
            .entities
            .iter()
            .filter_map(|e| crate::daemon::lsp_entity_ref(e, &e.file_origin.as_ref()?.0))
            .collect(),
        repo.path(),
    );
    let context = state
        .graph
        .get_resolution_record(&kin_model::ResolutionRecordId::context_validation(
            kin_model::LanguageId::Python,
        ))
        .and_then(|record| match record {
            kin_model::ResolutionRecord::ContextValidation(v) => match v.state {
                kin_model::ContextValidationState::Validated { context } => Some(context),
                _ => None,
            },
            _ => None,
        })
        .unwrap();
    assert_eq!(
        state.graph.semantic_observation().relations.get(&prior.id),
        Some(&prior),
        "the accepted canonical prior Reference is present before settlement"
    );
    let unproven = [UnprovenSite {
        source: caller.id,
        start_byte: start,
        end_byte: span.end_byte,
        answer,
    }];
    // The independent Call has a definite answer while the non-call
    // Reference remains uncertain. Its census cannot settle that Reference.
    let call_answers = if with_call {
        let start = body.rfind("callback").unwrap();
        vec![SiteAnswer {
            source: caller.id,
            site: kin_model::SourceSpan {
                file: kin_model::FilePathId::new("reference.py"),
                start_byte: start,
                end_byte: start + "callback".len(),
                start_line: 2,
                start_col: 4,
                end_line: 2,
                end_col: 12,
            },
            target: SiteTarget::Outside(kin_lsp::call_sites::OutsideLocation {
                uri: "file:///opt/lib/runtime/callback.py".to_string(),
                range: kin_lsp::call_sites::LocationRange {
                    start_line: 0, start_character: 4, end_line: 0, end_character: 12,
                },
            }),
            rule: kin_lsp::call_sites::DEFINITION_RULE,
        }]
    } else {
        Vec::new()
    };
    let uri = kin_lsp::protocol::path_to_uri(&repo.path().join("reference.py"));
    let produced = Default::default();
    inputs
        .settle_file(
            &state,
            body,
            &call_answers,
            &ExternalNames::new(),
            &context,
            LedgerRequest {
                file: "reference.py",
                uri: &uri,
                index: &index,
                entities: &entities,
                unproven: &unproven,
                produced_references: &produced,
                ending: if retryable {
                    crate::call_site_ledger::PassEnding::Stopped(kin_model::ServerFailure::Timeout)
                } else {
                    crate::call_site_ledger::PassEnding::Complete
                },
                not_in_build: false,
                retract: !retryable,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        state.graph.semantic_observation().relations.get(&prior.id),
        Some(&prior),
        "no answer at an old noncall site does not contradict its positive reference"
    );
    assert!(
        state.enrichment_bookkeeping_owes_publication(),
        "unknown-only settlement owes native mark retirement"
    );
    assert!(state.lsp_owed_files.lock().unwrap()["reference.py"].reference_uncertainty);
    assert_eq!(
        crate::owed_enrichment::next_retry_delay(&state.lsp_owed_files.lock().unwrap(), u64::MAX)
            .is_some(),
        retryable
    );
    assert!(!inputs
        .record_file_completed(&state, "reference.py")
        .await
        .unwrap());
    assert!(
        inputs
            .record_file_completed(&state, "logs.py")
            .await
            .unwrap(),
        "a gap in one file cannot invalidate this capture of an unaffected file"
    );
    inputs.flush(&state, &mut pending).await.unwrap();
    if !crash_before_publication {
        state.save_snapshot_reporting_enrichment().unwrap();
    }
    if !crash_before_publication {
        assert!(
            !native_marks(&state)
                .iter()
                .any(|mark| mark.path == "reference.py"),
            "an already valid mark must actually retire"
        );
        assert!(
            native_marks(&state).contains(&unaffected),
            "unaffected native proof is preserved byte for byte"
        );
    }
    assert_eq!(
        lsp_publication_durable(&state).relations.get(&prior.id),
        Some(&prior)
    );
    let layout = state.layout.clone();
    drop(inputs);
    drop(state);
    let cold = waiting_cold_start(layout).await;
    assert_eq!(
        cold.graph.semantic_observation().relations.get(&prior.id),
        Some(&prior)
    );
    assert!(cold.lsp_owed_files.lock().unwrap()["reference.py"].reference_uncertainty);
    assert!(!native_marks(&cold)
        .iter()
        .any(|mark| mark.path == "reference.py"));
    assert!(native_marks(&cold).contains(&unaffected));
    assert!(!crate::daemon::file_resumed(
        &cold,
        "reference.py",
        Some(&inputs_body(&cold, "reference.py"))
    ));
    let sweep_status = reference_uncertainty_status(&cold).await;
    let row = sweep_status["owed_files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["file"] == "reference.py")
        .unwrap();
    assert_eq!(row["reference_uncertainty"], true);
    assert_eq!(row["automatic_retry"], retryable);
    assert_eq!(row["retry_after_s"].is_null(), !retryable);
    let selected = mcp_call(
        router(Arc::clone(&cold)),
        "kin_graph_status",
        json!({"dependencies":["reference.py","logs.py"], "max_chars":60000}),
    )
    .await;
    assert_eq!(selected.is_error, Some(true));
    assert!(mcp_result_text(&selected).contains("selector is not supported"));
    let public = mcp_call(
        router(Arc::clone(&cold)),
        "kin_graph_status",
        json!({"max_chars":60000}),
    )
    .await;
    let public = parse_public_graph_status(&public);
    assert_eq!(public["scope"], "head");
    assert_eq!(public["entity_count"], cold.graph.entity_count());
    let status = mcp_graph_status_snapshot(
        &cold,
        None,
        &cold.graph,
        RequestGraphAuthority::Head,
        kin_mcp::handlers::entities::GraphStatusScope::Head,
        &kin_mcp::status_pages::StatusRequest {
            dependencies: vec!["reference.py".into(), "logs.py".into()],
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let status = parse_graph_status(&status);
    assert_eq!(public["authority_epoch"], status.authority_epoch);
    assert_eq!(public["entity_count"], status.entity_count);
    assert_eq!(public["relation_count"], status.relation_count);
    assert_eq!(public["embeddings_indexed"], status.embeddings_indexed);
    assert_eq!(public["embeddings_pending"], status.embeddings_pending);
    assert_eq!(public["embeddings_total"], status.embeddings_total);
    let rows = status.enrichment.as_ref().unwrap()["files"]
        .as_array()
        .unwrap();
    let reference_status = rows
        .iter()
        .find(|row| {
            row["projection_path"] == json!(kin_model::RepoPath::from_utf8("reference.py").unwrap())
        })
        .unwrap();
    assert_eq!(
        reference_status["current_completion"], "owed",
        "machine native metadata must disclose the noncall gap even when its call census is empty"
    );
    if with_call {
        let facts = kin_mcp::call_sites::GraphSiteFacts::new(cold.graph.as_ref());
        let reading = kin_model::read_caller_sites(&facts, &caller);
        assert_eq!(reading.wire(), "current", "{reading:?}");
        let ledger = reading.ledger().unwrap();
        assert_eq!(ledger.sites.len(), 1);
        assert!(ledger.sites.iter().all(|site|
            kin_model::SiteStateKind::of(&site.state).is_settled()));
    }
    let references = tool_result_payload(&mcp_call(
        router(Arc::clone(&cold)),
        "find_references",
        json!({"entity_id":target.id, "relation_kinds":["references"],
            "answer_only":false, "max_chars":60000}),
    ).await);
    assert_eq!(references["references"].as_array().unwrap().len(), 1, "{references}");
    assert_eq!(references["references"][0]["entity_id"], caller.id.to_string(), "{references}");
    assert_eq!(references["references"][0]["relation_kinds"], json!(["references"]), "{references}");
    assert_eq!(references["references"][0]["site_count"], 1, "{references}");
    assert_eq!(references["_kin"]["verdict"]["state"], "inconclusive", "{references}");
    assert!(references["_kin"]["verdict"]["limiting_factor"].as_str()
        .is_some_and(|factor| factor.contains("reference_completion_unmeasured")), "{references}");
    assert!(!references["_kin"]["verdict"]["safe_to_conclude_absent"].as_bool().unwrap());
    // A references question takes the census's reading of non-call reads and
    // no call census. The focal holds members, and `logs.errorsCount` is a
    // reference to it that spells no name, so that reading cannot speak for
    // references whatever it matched.
    assert_eq!(
        references["call_sites"]["scope"],
        json!(kin_mcp::call_sites::MENTION_SCOPE),
        "{references}"
    );
    assert!(references["call_sites"].get("settled").is_none(), "{references}");
    assert!(
        references["call_sites"]["focal_escape"]["residue"]["mentions"]
            .get("references_unmeasured")
            .is_some(),
        "{references}"
    );
    assert!(references.get("caller_arrival").is_none(), "{references}");
    assert_eq!(cold.graph.semantic_observation().relations.get(&prior.id), Some(&prior));
    assert!(cold.lsp_owed_files.lock().unwrap()["reference.py"].reference_uncertainty);
    assert_eq!(
        cold.census_settled_retirements().get("References"),
        None,
        "holding uncertainty is not census retirement credit"
    );
    // A real fresh same-site positive proof clears the debt, then completion
    // can legitimately be published again over current captured inputs.
    let inputs = QueryInputs::capture(&cold).await.unwrap();
    let entities = inputs
        .entities
        .iter()
        .filter(|e| {
            e.file_origin
                .as_ref()
                .is_some_and(|p| p.0 == "reference.py")
        })
        .collect::<Vec<_>>();
    let index = kin_lsp::EntityIndex::new(
        inputs
            .entities
            .iter()
            .filter_map(|e| crate::daemon::lsp_entity_ref(e, &e.file_origin.as_ref()?.0))
            .collect(),
        repo.path(),
    );
    inputs
        .settle_file(
            &cold,
            body,
            &[],
            &ExternalNames::new(),
            &context,
            LedgerRequest {
                file: "reference.py",
                uri: &uri,
                index: &index,
                entities: &entities,
                unproven: &[],
                produced_references: &vec![prior.clone()],
                ending: crate::call_site_ledger::PassEnding::Complete,
                not_in_build: false,
                retract: true,
            },
        )
        .await
        .unwrap();
    assert!(!cold
        .lsp_owed_files
        .lock()
        .unwrap()
        .contains_key("reference.py"));
    inputs
        .record_file_completed(&cold, "reference.py")
        .await
        .unwrap();
    cold.save_snapshot_reporting_enrichment().unwrap();
    assert_lsp_publication_native_completion(&cold, &["reference.py", "logs.py"]);
}

/// The sweep's own order over one file: its relations land, then its
/// settlement reads what the file's callers hold, which by then includes them.
/// A definition answer that disagrees with itself still mints a positive
/// Reference at the site, as a union receiver or a second location does. That
/// Reference is this pass's current evidence, so the file completes and owes
/// nothing. Settling it as uncertain held back 97 of express's 138 files and 9
/// of requests's 37 on every cold sweep, permanently, because the same inputs
/// give the same answer.
#[tokio::test]
async fn a_reference_this_pass_minted_at_a_disagreed_site_completes_its_file() {
    use crate::daemon::lsp_publication::{LedgerRequest, QueryInputs};
    use kin_lsp::call_sites::{ExternalNames, UnprovenAnswer, UnprovenSite};
    const BODY: &str = "def run(logs, callback):\n    logs.errorsCount = 0\n    callback()\n";
    let (repo, state) = mcp_lifecycle_fixture();
    std::fs::write(repo.path().join("reference.py"), BODY).unwrap();
    std::fs::write(
        repo.path().join("logs.py"),
        "class ComponentLogs:\n    errorsCount: int\n",
    )
    .unwrap();
    waiting_admit(&state, "admitted reference bodies").await;
    waiting_commit(&state, "reference baseline").await;
    let caller = waiting_entity(&state, "reference.py", "run");
    let target = waiting_entity(&state, "logs.py", "ComponentLogs");
    let start = BODY.find("errorsCount").unwrap();
    let span = kin_model::SourceSpan {
        file: kin_model::FilePathId::new("reference.py"),
        start_byte: start,
        end_byte: start + "errorsCount".len(),
        start_line: 1,
        start_col: 9,
        end_line: 1,
        end_col: 20,
    };
    let minted = kin_model::Relation {
        id: kin_model::language_server_relation_id(RelationKind::References, caller.id, target.id),
        kind: RelationKind::References,
        src: GraphNodeId::Entity(caller.id),
        dst: GraphNodeId::Entity(target.id),
        confidence: 0.95,
        origin: kin_model::RelationOrigin::Lsp,
        created_in: None,
        import_source: None,
        evidence: vec![kin_model::RelationEvidence {
            source_span: Some(span.clone()),
            parser_rule: Some(kin_lsp::call_sites::DEFINITION_RULE.to_owned()),
            ..Default::default()
        }],
    };
    let inputs = QueryInputs::capture(&state).await.unwrap();
    let mut pending = crate::daemon::PendingEnrichment::default();
    inputs
        .absorb(&state, &mut pending, vec![minted.clone()])
        .await
        .unwrap();
    inputs.flush(&state, &mut pending).await.unwrap();
    let entities = inputs
        .entities
        .iter()
        .filter(|e| {
            e.file_origin
                .as_ref()
                .is_some_and(|p| p.0 == "reference.py")
        })
        .collect::<Vec<_>>();
    let index = kin_lsp::EntityIndex::new(
        inputs
            .entities
            .iter()
            .filter_map(|e| crate::daemon::lsp_entity_ref(e, &e.file_origin.as_ref()?.0))
            .collect(),
        repo.path(),
    );
    let context = kin_model::ProofContext {
        language: kin_model::LanguageId::Python,
        resolver: "lsp:fixture".to_owned(),
        resolver_version: "1".to_owned(),
        configuration_hash: kin_model::Hash256::from_bytes([1; 32]),
        environment_hash: kin_model::Hash256::from_bytes([2; 32]),
        environment_summary: String::new(),
    };
    let disagreed = [UnprovenSite {
        source: caller.id,
        start_byte: span.start_byte,
        end_byte: span.end_byte,
        answer: UnprovenAnswer::AnswersDisagree,
    }];
    let uri = kin_lsp::protocol::path_to_uri(&repo.path().join("reference.py"));
    let produced = vec![minted.clone()];
    let (_, ledgers) = inputs
        .settle_file(
            &state,
            BODY,
            &[],
            &ExternalNames::new(),
            &context,
            LedgerRequest {
                file: "reference.py",
                uri: &uri,
                index: &index,
                entities: &entities,
                unproven: &disagreed,
                produced_references: &produced,
                ending: crate::call_site_ledger::PassEnding::Complete,
                not_in_build: false,
                retract: true,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        ledgers.reference_uncertainty,
        Default::default(),
        "the Reference this pass minted is its current evidence"
    );
    assert_eq!(
        state.graph.semantic_observation().relations.get(&minted.id),
        Some(&minted),
        "the minted Reference stays as it landed"
    );
    assert!(
        !state
            .lsp_owed_files
            .lock()
            .unwrap()
            .contains_key("reference.py"),
        "nothing is owed for evidence the pass just produced"
    );
    assert!(
        inputs
            .record_file_completed(&state, "reference.py")
            .await
            .unwrap(),
        "the file completes"
    );
}

fn inputs_body(state: &DaemonState, file: &str) -> String {
    let snapshot = state.graph.semantic_observation();
    let path = kin_model::RepoPath::from_utf8(file).unwrap();
    match &snapshot
        .resolved_tree
        .artifact_at_path(&path)
        .unwrap()
        .entry
    {
        kin_model::TreeEntry::Blob { hash, .. } => hash.to_string(),
        _ => panic!("fixture path must have an admitted body"),
    }
}
