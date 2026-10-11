// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

// Language-server evidence in module-level code, across committed edits that
// move every line under it. Each round is one agent edit through the served
// route, one pass of the sweep over the edited file against a scripted
// language server, and one publication.

#[cfg(unix)]
mod edited_module_evidence {
    use super::*;
    use kin_index::binding_debt::{claims_local_binding_debt, decode_local_binding_debt};

    const HELPERS: &str =
        "class Alpha:\n    def send(self, x):\n        return x\n\n\ndef helper(x):\n    return x\n";

    /// One module-level site for each name the module takes from `helpers`.
    const ONE_SITE_EACH: &str =
        "from app.helpers import Alpha, helper\n\n\ndef first():\n    return 1\n";

    /// Module-level code reads `Alpha` on two lines, and the module's use of
    /// the type stands on both.
    const TWO_SITES: &str = "from app.helpers import Alpha, helper\n\nassert Alpha\nassert Alpha is not None\n\n\ndef first():\n    return 1\n";

    /// What repository authority holds after one round.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Round {
        /// The publication's error, when it had one.
        publication: Option<String>,
        /// Every binding obligation authority holds, as `kind source -> target`.
        owed: Vec<String>,
        /// The most records without a position any one language-server
        /// relation out of the module carries.
        most_unplaced_records: usize,
        /// The most positions any one of them carries.
        most_sites: usize,
    }

    async fn fixture(checks: &str) -> (tempfile::TempDir, Arc<DaemonState>) {
        let (dir, state) = mcp_lifecycle_fixture();
        for (path, body) in [
            ("app/__init__.py", "\"\"\"The package.\"\"\"\n"),
            ("app/helpers.py", HELPERS),
            ("app/checks.py", checks),
        ] {
            source_tree_conversion_fixture(
                &state,
                serde_json::json!({
                    "verb": "create", "target": path, "body": body,
                    "description": "edited module fixture"
                }),
            )
            .await;
        }
        (dir, state)
    }

    /// The scripted server's answers: `Alpha` and `helper` resolve to their
    /// declarations wherever `checks.py` names them. The script answers by
    /// column, so an edit that moves the lines leaves the answers standing.
    fn answers(dir: &tempfile::TempDir, checks: &str) -> serde_json::Value {
        let uri = kin_lsp::protocol::path_to_uri(&dir.path().join("app/checks.py"));
        let helpers = kin_lsp::protocol::path_to_uri(&dir.path().join("app/helpers.py"));
        let at = |line: u32, start: u32, end: u32| {
            serde_json::json!({"result": [{"uri": helpers, "range": {
                "start": {"line": line, "character": start},
                "end": {"line": line, "character": end}}}]})
        };
        let mut responses = serde_json::json!({"initialize": {"result": {"capabilities": {
            "definitionProvider": true, "callHierarchyProvider": false
        }}}});
        let import = checks.lines().next().unwrap();
        let alpha = import.find("Alpha").unwrap();
        let helper = import.rfind("helper").unwrap();
        responses[format!("textDocument/definition@{uri}#{alpha}")] = at(0, 6, 11);
        responses[format!("textDocument/definition@{uri}#{helper}")] = at(5, 4, 10);
        if checks.lines().any(|line| line.starts_with("assert Alpha")) {
            let read = "assert ".len();
            responses[format!("textDocument/definition@{uri}#{read}")] = at(0, 6, 11);
        }
        responses
    }

    /// The module's use of `Alpha` as a language server's reference pass
    /// reports it: one relation standing on every module-level line that reads
    /// the name, at the lines the current code has them on. It goes in the way
    /// a sweep's answers do.
    async fn absorb_type_use_at_every_read(dir: &tempfile::TempDir, state: &Arc<DaemonState>) {
        let file = "app/checks.py";
        let text = edit_path_disk(dir, file);
        let mut evidence = Vec::new();
        let mut line_start = 0usize;
        for (line, code) in text.split_inclusive('\n').enumerate() {
            if code.starts_with("assert Alpha") {
                let (start, end) = ("assert ".len(), "assert Alpha".len());
                evidence.push(kin_model::RelationEvidence {
                    source_span: Some(kin_model::SourceSpan {
                        file: kin_model::FilePathId::new(file),
                        start_byte: line_start + start,
                        end_byte: line_start + end,
                        start_line: line as u32,
                        start_col: start as u32,
                        end_line: line as u32,
                        end_col: end as u32,
                    }),
                    parser_rule: Some(kin_model::LSP_REFERENCES_RULE.to_string()),
                    ..Default::default()
                });
            }
            line_start += code.len();
        }
        if evidence.is_empty() {
            return;
        }
        let module = module_of(&state.graph.semantic_observation(), file);
        let alpha = module_code_entity(state, "app/helpers.py", "Alpha").id;
        let relation = kin_model::Relation {
            id: kin_model::language_server_relation_id(RelationKind::UsesType, module, alpha),
            kind: RelationKind::UsesType,
            src: GraphNodeId::Entity(module),
            dst: GraphNodeId::Entity(alpha),
            confidence: 0.85,
            origin: kin_model::RelationOrigin::Lsp,
            created_in: None,
            import_source: None,
            evidence,
        };
        let inputs = crate::daemon::lsp_publication::QueryInputs::capture(state)
            .await
            .unwrap();
        let mut pending = crate::daemon::PendingEnrichment::default();
        inputs
            .absorb(state, &mut pending, vec![relation])
            .await
            .unwrap();
        inputs.flush(state, &mut pending).await.unwrap();
    }

    fn module_of(graph: &kin_db::GraphSnapshot, file: &str) -> kin_model::EntityId {
        graph
            .entities
            .values()
            .find(|entity| {
                entity.kind == kin_model::EntityKind::Module
                    && entity
                        .file_origin
                        .as_ref()
                        .is_some_and(|origin| origin.0 == file)
            })
            .expect("the graph holds the module")
            .id
    }

    /// One pass of the sweep over `checks.py` against the scripted server,
    /// then the publication of what it proved.
    async fn sweep_and_publish(
        dir: &tempfile::TempDir,
        state: &Arc<DaemonState>,
        responses: &serde_json::Value,
    ) -> Round {
        let file = "app/checks.py";
        crate::daemon::tests::sweep_one_file(state, dir.path(), file, responses, true).await;
        absorb_type_use_at_every_read(dir, state).await;
        let publication = state.save_snapshot().err().map(|error| error.to_string());
        let durable = lsp_publication_durable(state);
        let mut owed = Vec::new();
        for relation in durable
            .relations
            .values()
            .filter(|relation| claims_local_binding_debt(relation))
        {
            let GraphNodeId::Artifact(artifact) = relation.src else {
                panic!("a binding debt is held on its source's artifact");
            };
            let entry = durable.resolved_tree.get(&artifact).unwrap();
            let file = kin_model::FilePathId::new(entry.path.as_utf8().unwrap());
            let debt = decode_local_binding_debt(&file, artifact, relation)
                .unwrap()
                .expect("the relation claims a debt");
            owed.extend(debt.obligations.iter().map(|obligation| {
                format!(
                    "{:?} {} -> {}",
                    obligation.retired_relation.kind,
                    obligation.source_name,
                    obligation.target_name
                )
            }));
        }
        owed.sort();
        let module = module_of(&durable, file);
        let from_module: Vec<_> = durable
            .relations
            .values()
            .filter(|relation| {
                relation.origin == kin_model::RelationOrigin::Lsp
                    && relation.src == GraphNodeId::Entity(module)
            })
            .collect();
        assert!(
            !from_module.is_empty(),
            "the sweep proves the module's names: {:?}",
            durable
                .relations
                .values()
                .filter(|relation| relation.origin == kin_model::RelationOrigin::Lsp)
                .collect::<Vec<_>>()
        );
        Round {
            publication,
            owed,
            most_unplaced_records: from_module
                .iter()
                .map(|relation| {
                    relation
                        .evidence
                        .iter()
                        .filter(|record| record.source_span.is_none())
                        .count()
                })
                .max()
                .unwrap_or(0),
            most_sites: from_module
                .iter()
                .map(|relation| {
                    relation
                        .evidence
                        .iter()
                        .filter(|record| record.source_span.is_some())
                        .count()
                })
                .max()
                .unwrap_or(0),
        }
    }

    /// Adds `second` after `first`, or takes it out again, through `mutate`.
    async fn edit(state: &Arc<DaemonState>, session: &str, add: bool) {
        let operation = if add {
            let first = module_code_entity(state, "app/checks.py", "first");
            let read = edit_path_read(state, &serde_json::json!(first.id)).await;
            edit_path_create(
                &read,
                "sibling_after",
                "function",
                "second",
                "def second():\n    return 2",
            )
        } else {
            let second = module_code_entity(state, "app/checks.py", "second");
            let read = edit_path_read(state, &serde_json::json!(second.id)).await;
            edit_path_remove(&read)
        };
        let (result, text) = edit_path_mutate(state, session, vec![operation]).await;
        assert_ne!(result.is_error, Some(true), "{text}");
    }

    /// The rounds of `checks`: proven as written, then after each of three
    /// committed edits that change how many lines the module has.
    async fn rounds(checks: &str) -> Vec<Round> {
        let (dir, state) = fixture(checks).await;
        let responses = answers(&dir, checks);
        let session = mcp_test_session(&state);
        let mut rounds = vec![sweep_and_publish(&dir, &state, &responses).await];
        for add in [true, false, true] {
            edit(&state, &session, add).await;
            rounds.push(sweep_and_publish(&dir, &state, &responses).await);
        }
        rounds
    }

    /// A round that published, owes nothing, and holds `sites` positions on
    /// its best-proven relation.
    fn settled(sites: usize) -> Round {
        Round {
            publication: None,
            owed: Vec::new(),
            most_unplaced_records: 0,
            most_sites: sites,
        }
    }

    /// A commit that changes a module's line count cannot say where the
    /// module-level sites a language server proved went, so it clears their
    /// positions. The record it leaves says only that the relation was proven
    /// somewhere, and it says that once however many edits pass: three
    /// committed edits, each proven again by the sweep, publish every time and
    /// leave no binding owed.
    ///
    /// Held once for each edit, the second edit's two records alike became one
    /// in the sweep's merge, the publication read that as a binding withdrawn
    /// and recorded an obligation nothing could settle, and the third edit's
    /// publication failed on it with "prior obligation identity collides".
    #[tokio::test]
    async fn edited_module_evidence_survives_three_edits_with_one_site_a_relation() {
        let rounds = rounds(ONE_SITE_EACH).await;
        let after_an_edit = Round {
            most_unplaced_records: 1,
            ..settled(1)
        };
        assert_eq!(
            rounds,
            vec![
                settled(1),
                after_an_edit.clone(),
                after_an_edit.clone(),
                after_an_edit,
            ]
        );
    }

    /// A relation proven at two module-level sites loses both positions to one
    /// edit. They are one record afterwards, not two alike, so the first edit
    /// leaves nothing owed either. Held twice, the first edit recorded the
    /// obligation and the second edit's publication failed.
    #[tokio::test]
    async fn edited_module_evidence_survives_three_edits_with_two_sites_on_one_relation() {
        let rounds = rounds(TWO_SITES).await;
        let after_an_edit = Round {
            most_unplaced_records: 1,
            ..settled(2)
        };
        assert_eq!(
            rounds,
            vec![
                settled(2),
                after_an_edit.clone(),
                after_an_edit.clone(),
                after_an_edit,
            ]
        );
    }

    /// What an agent reads when the daemon's graph has lost a relation the
    /// repository holds: nothing was written, sending the change again will
    /// not help, and what ends it. Never an instruction to send it again. A
    /// relation a sweep retired and has not yet published is not lost.
    #[tokio::test]
    async fn edited_module_evidence_a_lost_relation_refuses_a_write_in_words_an_agent_can_act_on() {
        let (dir, state) = fixture(ONE_SITE_EACH).await;
        let session = mcp_test_session(&state);
        let durable = lsp_publication_durable(&state);
        let module = module_of(&durable, "app/checks.py");
        let lost = durable
            .relations
            .values()
            .find(|relation| relation.src == GraphNodeId::Entity(module))
            .expect("authority holds a relation out of the module")
            .clone();
        state.graph.remove_relation(&lost.id).unwrap();

        let first = module_code_entity(&state, "app/checks.py", "first");
        let read = edit_path_read(&state, &serde_json::json!(first.id)).await;
        let (result, text) = edit_path_mutate(
            &state,
            &session,
            vec![edit_path_patch(&read, &[("return 1", "return 2")])],
        )
        .await;
        assert_eq!(result.is_error, Some(true), "{text}");
        assert_eq!(
            text,
            format!(
                "Nothing was written, and sending this change again will not help: the Kin \
                 daemon's graph has lost relation {} that the repository holds, and a running \
                 daemon does not get it back. Stop the daemon with `kin daemon stop`; the next \
                 call starts one that reads the repository again.",
                lost.id
            )
        );
        assert_eq!(edit_path_disk(&dir, "app/checks.py"), ONE_SITE_EACH);

        // The same relation, retired by a sweep and waiting for its
        // publication, is not lost, and the same change lands.
        state.lsp_settled_guesses.lock().unwrap().insert(lost.id);
        let read = edit_path_read(&state, &serde_json::json!(first.id)).await;
        let (result, text) = edit_path_mutate(
            &state,
            &session,
            vec![edit_path_patch(&read, &[("return 1", "return 2")])],
        )
        .await;
        assert_ne!(result.is_error, Some(true), "{text}");
        assert_eq!(
            edit_path_disk(&dir, "app/checks.py"),
            ONE_SITE_EACH.replace("return 1", "return 2")
        );
    }
}
