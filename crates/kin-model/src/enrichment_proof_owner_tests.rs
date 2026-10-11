// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

// Included inside repository.rs's tests, where semantic_entity and lsp_relation
// provide the same declaration and relation fixtures as the existing controls.
mod enrichment_proof_owner_controls {
    use super::*;
    use crate::{
        CallSite, CallSiteLedger, CallSiteState, ContextValidation, ContextValidationState,
        ProofContext, Relation, RelationEvidence, RelationKind, RelationOrigin, ResolutionRecord,
    };

    const A: &str = "src/a.rs";
    const B: &str = "src/b.rs";

    #[derive(Clone)]
    struct OwnerInputs {
        entities: Vec<Entity>,
        relations: Vec<Relation>,
        records: Vec<ResolutionRecord>,
    }

    impl OwnerInputs {
        fn digests(&self) -> BTreeMap<String, Hash256> {
            crate::enrichment_proof_inputs_by_file(
                [A, B],
                &self.entities,
                &self.relations,
                &self.records,
            )
            .unwrap()
        }
    }

    fn context(marker: u8) -> ProofContext {
        ProofContext {
            language: LanguageId::Rust,
            resolver: "lsp:owner-control".into(),
            resolver_version: "1".into(),
            configuration_hash: Hash256::from_bytes([marker; 32]),
            environment_hash: Hash256::from_bytes([marker + 1; 32]),
            environment_summary: format!("owner {marker}"),
        }
    }

    fn inputs() -> OwnerInputs {
        let mut a = semantic_entity(1, "caller");
        a.file_origin = Some(crate::FilePathId::new(A));
        let mut b = semantic_entity(2, "target");
        b.file_origin = Some(crate::FilePathId::new(B));
        let own = ResolutionRecord::ProofContext(context(1));
        let peer = ResolutionRecord::ProofContext(context(3));
        let mut relation = lsp_relation(0xe001, A);
        relation.kind = RelationKind::References;
        relation.confidence = 0.95;
        relation.evidence[0].parser_rule = Some("lsp_definition".into());
        relation.evidence[0].token = Some(own.id().context_token());
        let mut reference = relation.evidence[0].clone();
        reference.parser_rule = Some(crate::LSP_REFERENCES_RULE.into());
        reference.token = Some(peer.id().context_token());
        // Both observations describe the same source position and relation.
        // The definition query is A's; the references query about target is B's.
        relation.evidence.push(reference);
        let validation = ResolutionRecord::ContextValidation(ContextValidation {
            language: LanguageId::Rust,
            state: ContextValidationState::Validated {
                context: own.as_proof_context().unwrap().clone(),
            },
        });
        let ledger = ResolutionRecord::CallSites(CallSiteLedger {
            caller: a.id,
            behavior_hash: a.fingerprint.behavior_hash,
            body_hash: Hash256::from_bytes([5; 32]),
            context: own.id(),
            census: 1,
            sites: vec![CallSite {
                offset: 3,
                length: 4,
                state: CallSiteState::Binding { may_call: None },
            }],
        });
        OwnerInputs {
            entities: vec![a, b],
            relations: vec![relation],
            records: vec![own, peer, validation, ledger],
        }
    }

    #[test]
    fn a_peer_context_is_bound_only_by_the_owner_of_its_evidence() {
        let original = inputs();
        let baseline = original.digests();
        let peer_id = original.records[1].id();

        let mut missing = original.clone();
        missing.records.retain(|record| record.id() != peer_id);
        let observed = missing.digests();
        assert_eq!(
            observed[A], baseline[A],
            "B's missing context is not A's input"
        );
        assert_ne!(
            observed[B], baseline[B],
            "B must bind an explicitly missing context"
        );

        let mut changed = original;
        let mut peer = changed.records[1].as_proof_context().unwrap().clone();
        peer.environment_hash = Hash256::from_bytes([9; 32]);
        peer.environment_summary = "peer resolver environment changed".into();
        let peer = ResolutionRecord::ProofContext(peer);
        assert_ne!(
            peer.id(),
            peer_id,
            "contexts are addressed by their content"
        );
        changed.relations[0].evidence[1].token = Some(peer.id().context_token());
        changed.records[1] = peer;
        let observed = changed.digests();
        assert_eq!(
            observed[A], baseline[A],
            "B's replacement proof is not A's input"
        );
        assert_ne!(
            observed[B], baseline[B],
            "B's replacement proof changes its observation"
        );
    }

    #[test]
    fn projecting_peer_evidence_keeps_genuine_completion_dependencies() {
        let original = inputs();
        let baseline = original.digests();
        let own_id = original.records[0].id();
        let mut missing = original.clone();
        missing.records.retain(|record| record.id() != own_id);
        let observed = missing.digests();
        assert_ne!(
            observed[A], baseline[A],
            "A's own context cannot disappear silently"
        );
        assert_eq!(
            observed[B], baseline[B],
            "B does not read A's context record"
        );

        let mut changed = original.clone();
        let mut own = changed.records[0].as_proof_context().unwrap().clone();
        own.environment_hash = Hash256::from_bytes([10; 32]);
        let own = ResolutionRecord::ProofContext(own);
        changed.relations[0].evidence[0].token = Some(own.id().context_token());
        let ResolutionRecord::CallSites(ledger) = &mut changed.records[3] else {
            panic!("the fixture holds A's ledger")
        };
        ledger.context = own.id();
        changed.records[0] = own;
        let observed = changed.digests();
        assert_ne!(
            observed[A], baseline[A],
            "A's replacement context changes its observation"
        );
        assert_eq!(observed[B], baseline[B]);

        let mut changed = original.clone();
        let validation_id = changed.records[2].id();
        changed.records[2] = ResolutionRecord::ContextValidation(ContextValidation {
            language: LanguageId::Rust,
            state: ContextValidationState::Unverified {
                reason: "the selected resolver environment changed".into(),
            },
        });
        assert_eq!(changed.records[2].id(), validation_id);
        let observed = changed.digests();
        assert_ne!(
            observed[A], baseline[A],
            "same-ID selected validation changes still invalidate"
        );
        assert_ne!(
            observed[B], baseline[B],
            "language validation is shared by both owners"
        );

        let mut changed = original.clone();
        changed.relations[0].confidence = 0.85;
        let observed = changed.digests();
        assert_ne!(
            observed[A], baseline[A],
            "the shared relation header is retained"
        );
        assert_ne!(observed[B], baseline[B]);

        let mut changed = original.clone();
        changed.relations[0].evidence[0].parser_rule = Some("lsp_definition_alias".into());
        let observed = changed.digests();
        assert_ne!(
            observed[A], baseline[A],
            "A's own evidence payload is retained"
        );
        assert_eq!(observed[B], baseline[B]);

        let mut changed = original.clone();
        changed.relations[0].evidence.remove(0);
        let observed = changed.digests();
        assert_ne!(
            observed[A], baseline[A],
            "removing A's proof cannot preserve completion"
        );
        assert_eq!(observed[B], baseline[B]);

        let mut changed = original;
        let ledger_id = changed.records[3].id();
        let ResolutionRecord::CallSites(ledger) = &mut changed.records[3] else {
            panic!("the fixture holds A's ledger")
        };
        ledger.sites[0].state = CallSiteState::ProvenOutside;
        assert_eq!(changed.records[3].id(), ledger_id);
        let observed = changed.digests();
        assert_ne!(
            observed[A], baseline[A],
            "same-ID own ledger payload changes still invalidate"
        );
        assert_eq!(observed[B], baseline[B]);
    }

    #[test]
    fn an_unassigned_lsp_certificate_and_its_context_remain_proof_inputs() {
        let mut original = inputs();
        let before_certificate = original.digests();
        let unknown_context = ResolutionRecord::ProofContext(context(7));
        let unknown_id = unknown_context.id();
        original.records.push(unknown_context);
        original.relations[0].evidence.push(RelationEvidence {
            parser_rule: Some("unclassified_lsp_certificate".into()),
            token: Some(unknown_id.context_token()),
            ..Default::default()
        });
        let baseline = original.digests();
        for path in [A, B] {
            assert_ne!(
                baseline[path], before_certificate[path],
                "{path} retains unassigned evidence"
            );
        }

        let mut changed = original.clone();
        changed.relations[0].evidence[2].occurrence_count = 2;
        let observed = changed.digests();
        for path in [A, B] {
            assert_ne!(
                observed[path], baseline[path],
                "{path} binds the spanless certificate payload"
            );
        }

        let mut missing = original;
        missing.records.retain(|record| record.id() != unknown_id);
        let observed = missing.digests();
        for path in [A, B] {
            assert_ne!(
                observed[path], baseline[path],
                "{path} also binds its unassigned context dependency"
            );
        }
    }

    #[test]
    fn a_parsed_calls_certificate_remains_in_its_owners_fingerprint() {
        let mut original = inputs();
        let relation = &mut original.relations[0];
        relation.origin = RelationOrigin::Parsed;
        relation.kind = RelationKind::Calls;
        relation.evidence.truncate(1);
        relation.evidence[0].parser_rule = None;
        relation.evidence[0].token = None;
        let before_certificate = original.digests();
        original.relations[0].evidence.push(RelationEvidence {
            parser_rule: Some("parser_occurrence_resolution_v2".into()),
            token: Some("occurrence-certificate-a".into()),
            ..Default::default()
        });
        let baseline = original.digests();
        assert_ne!(baseline[A], before_certificate[A]);
        assert_eq!(
            baseline[B], before_certificate[B],
            "a spanless certificate invents no second owner"
        );

        let mut changed = original.clone();
        changed.relations[0].evidence[1].token = Some("changed-occurrence-certificate-a".into());
        let observed = changed.digests();
        assert_ne!(
            observed[A], baseline[A],
            "parsed Calls retain the complete certificate payload"
        );
        assert_eq!(observed[B], baseline[B]);

        original.relations[0].evidence.pop();
        assert_eq!(original.digests(), before_certificate);
    }

    #[test]
    fn an_unknown_only_relation_is_not_given_an_invented_completion_owner() {
        let original = inputs();
        let baseline = original.digests();
        let unknown_context = ResolutionRecord::ProofContext(context(7));
        for (origin, kind) in [
            (RelationOrigin::Lsp, RelationKind::References),
            (RelationOrigin::Parsed, RelationKind::Calls),
        ] {
            let mut unassigned = lsp_relation(0xe002, A);
            unassigned.origin = origin;
            unassigned.kind = kind;
            unassigned.evidence = vec![RelationEvidence {
                parser_rule: Some("unclassified_certificate".into()),
                token: Some(unknown_context.id().context_token()),
                ..Default::default()
            }];
            let mut added = original.clone();
            added.relations.push(unassigned);
            added.records.push(unknown_context.clone());
            assert_eq!(
                added.digests(),
                baseline,
                "{origin:?} endpoint identities alone do not assign a spanless proof"
            );
            let mut changed = added;
            changed.relations[1].evidence[0].occurrence_count = 2;
            changed.relations[1].confidence = 0.8;
            assert_eq!(
                changed.digests(),
                baseline,
                "an unassigned relation's payload must not contaminate either owner"
            );
        }
    }
}
