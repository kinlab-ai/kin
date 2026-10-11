// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Persistence contracts for the narrow committed-Git snapshot exception.
//! Every refusal is checked against the public commit API and unchanged durable
//! authority; source bodies may be staged before admission without becoming truth.

use super::super::{LocalFileBackend, RepositoryAuthorityManager};
use crate::storage::authority::VersionedAuthorityState;
use kin_model::*;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::Arc;
use tempfile::TempDir;
use uuid::Uuid;

type Manager = RepositoryAuthorityManager<LocalFileBackend>;

fn repository_id() -> RepositoryId {
    RepositoryId::new("git-snapshot-proof-persistence-test").unwrap()
}

fn digest(body: &[u8]) -> Hash256 {
    Hash256::from_bytes(Sha256::digest(body).into())
}

fn open_manager(directory: &TempDir) -> Manager {
    RepositoryAuthorityManager::open(
        repository_id(),
        Arc::new(LocalFileBackend::new(directory.path())),
    )
    .unwrap()
}

fn transaction_shell(manager: &Manager, operation: u128) -> RepositoryTransaction {
    let lease = manager.read_authority();
    RepositoryTransaction {
        schema_version: REPOSITORY_TRANSACTION_SCHEMA_VERSION,
        operation_id: OperationId::from_uuid(Uuid::from_u128(operation)),
        repository_id: repository_id(),
        expected_generation: lease.generation(),
        expected_roots: lease.roots().clone(),
        actor: AuthorId::new("snapshot-contract-test"),
        reason: "capture a committed Git snapshot".to_string(),
        external_objects: Vec::new(),
        git_authority_delta: None,
        changes: Vec::new(),
        aliases: Vec::new(),
        ref_mutations: Vec::new(),
        default_ref_mutation: None,
        workspace_mutation: None,
        local_overlay_delta: None,
        merge_transaction_delta: None,
        sealed_observation: None,
        collaboration_delta: None,
        git_snapshot_proofs: Vec::new(),
    }
}

#[derive(Clone)]
struct Fixture {
    records: Vec<ExternalObjectRecord>,
    bodies: BTreeMap<Hash256, Vec<u8>>,
    change: SemanticChange,
    proof: GitSnapshotProof,
    shared: SharedAdmissionPolicy,
    tree: GitObjectId,
}

impl Fixture {
    fn object(
        records: &mut Vec<ExternalObjectRecord>,
        bodies: &mut BTreeMap<Hash256, Vec<u8>>,
        kind: ExternalObjectKind,
        body: Vec<u8>,
    ) -> ExternalObjectRecord {
        let mut envelope = format!(
            "{} {}\0",
            std::str::from_utf8(kind.git_header()).unwrap(),
            body.len()
        )
        .into_bytes();
        envelope.extend_from_slice(&body);
        let oid = GitObjectId::sha256(Sha256::digest(&envelope).into());
        let record = ExternalObjectRecord::from_raw(kind, oid, &body).unwrap();
        bodies.insert(record.body_hash, body);
        records.push(record.clone());
        record
    }

    fn commit_body(tree: GitObjectId, message: &str) -> Vec<u8> {
        // The parent is a deliberately unheld history boundary. A snapshot
        // proof retains the source tree closure, never its ancestry.
        let parent = GitObjectId::sha256([0x42; 32]);
        format!(
            "tree {tree}\nparent {parent}\nauthor Fixture <fixture@example.com> 1700000000 +0000\ncommitter Fixture <fixture@example.com> 1700000000 +0000\n\n{message}\n"
        )
        .into_bytes()
    }

    fn new(path: &str, mode: &str, ignored: bool, allow_sensitive: bool, body: &[u8]) -> Self {
        let mut records = Vec::new();
        let mut bodies = BTreeMap::new();
        let blob = Self::object(
            &mut records,
            &mut bodies,
            ExternalObjectKind::Blob,
            body.to_vec(),
        );
        let entry = match mode {
            "100644" => TreeEntry::blob(blob.body_hash, false),
            "100755" => TreeEntry::blob(blob.body_hash, true),
            "120000" => TreeEntry::symlink(blob.body_hash),
            _ => panic!("unsupported fixture mode"),
        };
        let mut tree_deltas = vec![TreeDelta::Added {
            artifact_id: ArtifactId(Uuid::from_u128(31)),
            new: LocatedEntry::new(RepoPath::from_utf8(path).unwrap(), entry),
        }];
        let components: Vec<_> = path.split('/').collect();
        let mut target = blob.object.oid;
        let mut current_mode = mode;
        for component in components[1..].iter().rev() {
            let mut tree_body = format!("{current_mode} {component}\0").into_bytes();
            tree_body.extend_from_slice(target.as_bytes());
            target = Self::object(
                &mut records,
                &mut bodies,
                ExternalObjectKind::Tree,
                tree_body,
            )
            .object
            .oid;
            current_mode = "40000";
        }
        let mut root_body = Vec::new();
        let mut sources = Vec::new();
        if ignored {
            let rule_body = if components.len() == 1 {
                format!("{path}\n")
            } else {
                format!("{}/\n", components[0])
            };
            let rules = Self::object(
                &mut records,
                &mut bodies,
                ExternalObjectKind::Blob,
                rule_body.into_bytes(),
            );
            root_body.extend_from_slice(b"100644 .gitignore\0");
            root_body.extend_from_slice(rules.object.oid.as_bytes());
            sources.push(AdmissionRuleSource {
                kind: AdmissionRuleSourceKind::GitIgnore,
                path: RepoPath::from_utf8(".gitignore").unwrap(),
                base_directory: None,
                body_hash: rules.body_hash,
                body_len: rules.body_len,
                precedence: 0,
            });
            tree_deltas.push(TreeDelta::Added {
                artifact_id: ArtifactId(Uuid::from_u128(30)),
                new: LocatedEntry::new(
                    RepoPath::from_utf8(".gitignore").unwrap(),
                    TreeEntry::blob(rules.body_hash, false),
                ),
            });
        }
        root_body.extend_from_slice(format!("{current_mode} {}\0", components[0]).as_bytes());
        root_body.extend_from_slice(target.as_bytes());
        let tree = Self::object(
            &mut records,
            &mut bodies,
            ExternalObjectKind::Tree,
            root_body,
        )
        .object
        .oid;
        let commit = Self::object(
            &mut records,
            &mut bodies,
            ExternalObjectKind::Commit,
            Self::commit_body(tree, "snapshot fixture"),
        );
        let allowances = if allow_sensitive {
            vec![SensitiveArtifactAllowance {
                path: RepoPath::from_utf8(path).unwrap(),
                content_hash: blob.body_hash,
                kind: match entry {
                    TreeEntry::Blob { executable, .. } => {
                        SensitiveArtifactKind::Blob { executable }
                    }
                    TreeEntry::Symlink { .. } => SensitiveArtifactKind::Symlink,
                    _ => unreachable!(),
                },
                approved_by: AuthorId::new("kin-git-import"),
                reason: "committed Git snapshot import".to_string(),
            }]
        } else {
            Vec::new()
        };
        let shared = SharedAdmissionPolicy::new(0, sources, allowances).unwrap();
        let mut change = SemanticChange {
            id: SemanticChangeId::from_hash(Hash256::from_bytes([0; 32])),
            origin: ChangeOrigin::Native,
            parents: Vec::new(),
            timestamp: Timestamp(chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap()),
            author: AuthorId::new("fixture"),
            message: "snapshot fixture".to_string(),
            entity_deltas: Vec::new(),
            relation_deltas: Vec::new(),
            tree_deltas,
            admission_policy_delta: Some(AdmissionPolicyDelta::initialize(shared.clone())),
            external_reference_deltas: Vec::new(),
            projected_files: Vec::new(),
            spec_link: None,
            evidence: Vec::new(),
            risk_summary: None,
            resolution_record_deltas: Vec::new(),
        };
        change.id = compute_semantic_change_id(&change).unwrap();
        records.sort_by_key(|record| record.object);
        let proof = GitSnapshotProof {
            repository_id: repository_id(),
            snapshot_change_id: change.id,
            source_commit: commit.object.oid,
            objects: records.iter().map(|record| record.object).collect(),
        };
        Self {
            records,
            bodies,
            change,
            proof,
            shared,
            tree,
        }
    }

    fn ignored() -> Self {
        Self::new(
            "ignored/fixture.txt",
            "100644",
            true,
            false,
            b"ordinary fixture content\n",
        )
    }

    fn save(&self, manager: &Manager) {
        for (hash, body) in &self.bodies {
            manager.save_source_blob(*hash, body).unwrap();
        }
    }

    fn resign(&mut self) {
        self.change.id = compute_semantic_change_id(&self.change).unwrap();
        self.proof.snapshot_change_id = self.change.id;
    }

    fn transaction(&self, manager: &Manager, operation: u128) -> RepositoryTransaction {
        let tree = ResolvedTree::default()
            .apply(&self.change.tree_deltas)
            .unwrap();
        let tree_hash = compute_resolved_tree_hash(&tree).unwrap();
        let workspace_id = WorkspaceId::from_uuid(Uuid::from_u128(20));
        let overlay =
            FrozenLocalOverlay::new(workspace_id, 0, AdmissionCase::Sensitive, Vec::new()).unwrap();
        let effective = EffectiveAdmissionPolicyStamp {
            shared: self.shared.stamp(),
            local: overlay.stamp(),
        };
        let main = RefName::branch(b"main").unwrap();
        let target = RefTarget::change(self.change.id);
        let mut transaction = transaction_shell(manager, operation);
        transaction.external_objects = self.records.clone();
        transaction.changes.push(self.change.clone());
        transaction.git_snapshot_proofs.push(self.proof.clone());
        transaction.ref_mutations.push(RefMutation {
            name: main.clone(),
            expected: RefExpectation::MustNotExist,
            new_target: Some(target.clone()),
            policy: RefUpdatePolicy::FastForwardOnly,
        });
        transaction.default_ref_mutation = Some(DefaultRefMutation {
            expected: DefaultRefExpectation::MustBeUnset,
            new_default: Some(main.clone()),
        });
        transaction.workspace_mutation = Some(WorkspaceMutation {
            workspace_id,
            expected: WorkspaceExpectation::MustNotExist,
            new_generation: 0,
            new_head: WorkspaceHead::Symbolic { target: main },
            new_base_target: Some(target),
            new_base_tree_hash: Some(tree_hash),
            tree_deltas: self.change.tree_deltas.clone(),
            new_tree_hash: tree_hash,
            semantic_delta: WorkspaceSemanticDelta::default(),
            new_shared_admission_policy: self.shared.clone(),
            new_admission_policy: effective,
        });
        transaction.local_overlay_delta = Some(FrozenLocalOverlayDelta::initialize(overlay));
        transaction
    }
}

fn refuse_unchanged(
    directory: &TempDir,
    manager: &Manager,
    transaction: RepositoryTransaction,
    transferred: bool,
) {
    let before = manager.read_authority();
    let roots = before.roots().clone();
    let generation = before.generation();
    let proofs = before.metadata().git_snapshot_proofs.clone();
    let changes = before.snapshot().changes.len();
    drop(before);
    let result = if transferred {
        manager
            .commit_transferred_repository_transaction(transaction, Some(AdmissionCase::Sensitive))
    } else {
        manager.commit_repository_transaction(transaction)
    };
    assert!(
        result.is_err(),
        "invalid proof transaction unexpectedly committed"
    );
    let live = manager.read_authority();
    assert_eq!(live.roots(), &roots);
    assert_eq!(live.generation(), generation);
    assert_eq!(live.metadata().git_snapshot_proofs, proofs);
    assert_eq!(live.snapshot().changes.len(), changes);
    drop(live);
    let reopened = open_manager(directory);
    let durable = reopened.read_authority();
    assert_eq!(durable.roots(), &roots);
    assert_eq!(durable.generation(), generation);
    assert_eq!(durable.metadata().git_snapshot_proofs, proofs);
    assert_eq!(durable.snapshot().changes.len(), changes);
}

#[test]
fn committed_ignored_snapshot_persists_and_reopens_with_unheld_git_parent() {
    for mode in ["100644", "100755", "120000"] {
        let directory = tempfile::tempdir().unwrap();
        let manager = open_manager(&directory);
        let fixture = Fixture::new(
            "ignored/fixture.txt",
            mode,
            true,
            false,
            b"ordinary fixture content\n",
        );
        fixture.save(&manager);
        let transaction = fixture.transaction(&manager, 100);
        let receipt = manager
            .commit_repository_transaction(transaction.clone())
            .unwrap();
        assert_eq!(receipt.outcome, RepositoryCommitOutcome::Committed);
        let repeated = manager.commit_repository_transaction(transaction).unwrap();
        assert_eq!(repeated.outcome, RepositoryCommitOutcome::IdempotentReplay);
        let roots = manager.read_authority().roots().clone();
        drop(manager);
        let reopened = open_manager(&directory);
        let authority = reopened.read_authority();
        assert_eq!(authority.roots(), &roots);
        assert_eq!(
            authority.metadata().git_snapshot_proofs,
            vec![fixture.proof]
        );
        assert!(authority
            .snapshot()
            .changes
            .contains_key(&fixture.change.id));
        assert!(authority.metadata().git_external_authority.is_none());
        assert!(authority.metadata().aliases.is_empty());
        assert!(!authority
            .metadata()
            .external_objects
            .iter()
            .any(|record| record.object.oid == GitObjectId::sha256([0x42; 32])));
    }
}

#[test]
fn sensitive_allowance_and_forged_import_attribution_never_replace_a_snapshot_proof() {
    for transferred in [false, true] {
        for forged_actor in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let manager = open_manager(&directory);
            let fixture = Fixture::new(
                "ignored.txt",
                "100644",
                true,
                true,
                b"ordinary fixture content\n",
            );
            fixture.save(&manager);
            let mut transaction = fixture.transaction(&manager, 101);
            transaction.git_snapshot_proofs.clear();
            if forged_actor {
                transaction.actor = AuthorId::new("kin-git-import");
                transaction.reason = "bootstrap committed Git snapshot".to_string();
            }
            refuse_unchanged(&directory, &manager, transaction, transferred);
        }
    }
}

#[test]
fn rehashed_native_snapshot_cannot_change_git_path_digest_mode_or_kind() {
    for variant in 0..4 {
        let directory = tempfile::tempdir().unwrap();
        let manager = open_manager(&directory);
        let mut fixture = Fixture::ignored();
        let replacement = b"different valid CAS body\n".to_vec();
        fixture
            .bodies
            .insert(digest(&replacement), replacement.clone());
        let TreeDelta::Added { new, .. } = &mut fixture.change.tree_deltas[0] else {
            unreachable!()
        };
        let TreeEntry::Blob { hash, .. } = new.entry else {
            unreachable!()
        };
        match variant {
            0 => new.path = RepoPath::from_utf8("ignored/other.txt").unwrap(),
            1 => new.entry = TreeEntry::blob(digest(&replacement), false),
            2 => new.entry = TreeEntry::blob(hash, true),
            3 => new.entry = TreeEntry::symlink(hash),
            _ => unreachable!(),
        }
        fixture.resign();
        fixture.save(&manager);
        refuse_unchanged(
            &directory,
            &manager,
            fixture.transaction(&manager, 102),
            false,
        );
    }
}

#[test]
fn closure_descriptors_and_raw_git_bodies_are_required_before_commit() {
    let baseline = Fixture::ignored();
    for index in 0..baseline.records.len() {
        for missing_body in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let manager = open_manager(&directory);
            let mut fixture = baseline.clone();
            if missing_body {
                fixture.bodies.remove(&fixture.records[index].body_hash);
            } else {
                // Preserve the proof's declared closure, remove its authority descriptor.
                fixture.records.remove(index);
            }
            fixture.save(&manager);
            refuse_unchanged(
                &directory,
                &manager,
                fixture.transaction(&manager, 103),
                false,
            );
        }
        let directory = tempfile::tempdir().unwrap();
        let manager = open_manager(&directory);
        let mut fixture = baseline.clone();
        let record = &mut fixture.records[index];
        let mut altered = fixture.bodies[&record.body_hash].clone();
        altered[0] ^= 1;
        // Both CAS key and length describe the bytes honestly. The retained Git
        // identity still names the original body, and must be recomputed.
        record.body_hash = digest(&altered);
        record.body_len = altered.len() as u64;
        fixture.bodies.insert(record.body_hash, altered);
        fixture.save(&manager);
        refuse_unchanged(
            &directory,
            &manager,
            fixture.transaction(&manager, 104),
            false,
        );
    }
}

#[test]
fn proof_rejects_wrong_repository_change_and_nonexact_object_closure() {
    for variant in 0..4 {
        let directory = tempfile::tempdir().unwrap();
        let manager = open_manager(&directory);
        let mut fixture = Fixture::ignored();
        match variant {
            0 => fixture.proof.repository_id = RepositoryId::new("another-repository").unwrap(),
            1 => {
                fixture.proof.snapshot_change_id =
                    SemanticChangeId::from_hash(Hash256::from_bytes([0x33; 32]))
            }
            2 => {
                let omitted = fixture
                    .proof
                    .objects
                    .iter()
                    .position(|object| object.kind == ExternalObjectKind::Tree)
                    .unwrap();
                fixture.proof.objects.remove(omitted);
            }
            3 => {
                let extra = Fixture::object(
                    &mut fixture.records,
                    &mut fixture.bodies,
                    ExternalObjectKind::Blob,
                    b"unreachable object\n".to_vec(),
                );
                fixture.records.sort_by_key(|record| record.object);
                fixture.proof.objects.push(extra.object);
                fixture.proof.objects.sort();
            }
            _ => unreachable!(),
        }
        fixture.save(&manager);
        refuse_unchanged(
            &directory,
            &manager,
            fixture.transaction(&manager, 105),
            false,
        );
    }
}

#[test]
fn intrinsic_control_paths_stay_forbidden_even_with_exact_committed_proof() {
    for path in [
        ".kin/config",
        "nested/.kin/config",
        "nested/.git-export/HEAD",
        "nested/.kin-session.json",
        ".kin-reconcile-case/state",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let manager = open_manager(&directory);
        let fixture = Fixture::new(path, "100644", false, true, b"ordinary fixture content\n");
        fixture.save(&manager);
        refuse_unchanged(
            &directory,
            &manager,
            fixture.transaction(&manager, 106),
            false,
        );
    }
}

#[test]
fn committed_proof_does_not_skip_sensitive_content_approval() {
    let directory = tempfile::tempdir().unwrap();
    let manager = open_manager(&directory);
    let fixture = Fixture::new(
        "ignored/id_rsa",
        "100644",
        true,
        false,
        // The header is split so a credential scan does not read the fixture as a key.
        concat!(
            "-----BE",
            "GIN RSA PRIVATE KEY-----\nfixture bytes\n-----END RSA PRIVATE KEY-----\n"
        )
        .as_bytes(),
    );
    fixture.save(&manager);
    refuse_unchanged(
        &directory,
        &manager,
        fixture.transaction(&manager, 107),
        false,
    );
}

#[test]
fn new_local_proof_is_forbidden_after_generation_zero() {
    let directory = tempfile::tempdir().unwrap();
    let manager = open_manager(&directory);
    let fixture = Fixture::ignored();
    fixture.save(&manager);
    let mut holding = transaction_shell(&manager, 108);
    holding.external_objects = fixture.records.clone();
    manager.commit_repository_transaction(holding).unwrap();
    assert_eq!(manager.read_authority().generation(), 1);
    assert!(manager.read_authority().snapshot().changes.is_empty());
    let mut transaction = fixture.transaction(&manager, 109);
    transaction.external_objects.clear();
    refuse_unchanged(&directory, &manager, transaction, false);
}

#[test]
fn a_held_unproved_native_root_cannot_be_retrofitted_locally_or_by_transfer() {
    for transferred in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let manager = open_manager(&directory);
        let fixture = Fixture::new(
            "ordinary.txt",
            "100644",
            false,
            false,
            b"ordinary fixture content\n",
        );
        fixture.save(&manager);
        let mut first = fixture.transaction(&manager, 110);
        first.git_snapshot_proofs.clear();
        manager.commit_repository_transaction(first).unwrap();
        let mut retrofit = transaction_shell(&manager, 111);
        retrofit.git_snapshot_proofs.push(fixture.proof.clone());
        // Repeating the old change in the incoming envelope must not make it new.
        retrofit.changes.push(fixture.change.clone());
        refuse_unchanged(&directory, &manager, retrofit, transferred);
    }
}

#[test]
fn identical_held_proof_repeats_but_a_different_valid_commit_cannot_rebind_it() {
    let directory = tempfile::tempdir().unwrap();
    let manager = open_manager(&directory);
    let mut fixture = Fixture::ignored();
    fixture.save(&manager);
    manager
        .commit_repository_transaction(fixture.transaction(&manager, 112))
        .unwrap();
    let before = manager.read_authority().roots().replication.clone();
    let mut identical = transaction_shell(&manager, 113);
    identical.git_snapshot_proofs.push(fixture.proof.clone());
    manager.commit_repository_transaction(identical).unwrap();
    assert_eq!(manager.read_authority().roots().replication, before);

    let replacement = Fixture::object(
        &mut fixture.records,
        &mut fixture.bodies,
        ExternalObjectKind::Commit,
        Fixture::commit_body(fixture.tree, "different source commit, identical tree"),
    );
    fixture
        .proof
        .objects
        .retain(|object| object.kind != ExternalObjectKind::Commit);
    fixture.proof.objects.push(replacement.object);
    fixture.proof.objects.sort();
    fixture.proof.source_commit = replacement.object.oid;
    fixture.save(&manager);
    let mut changed = transaction_shell(&manager, 114);
    changed.external_objects.push(replacement);
    changed.git_snapshot_proofs.push(fixture.proof);
    refuse_unchanged(&directory, &manager, changed, false);
}

#[test]
fn transferred_new_root_binds_replication_identity_when_raw_objects_are_already_held() {
    let directory = tempfile::tempdir().unwrap();
    let manager = open_manager(&directory);
    let fixture = Fixture::ignored();
    fixture.save(&manager);
    let mut holding = transaction_shell(&manager, 115);
    holding.external_objects = fixture.records.clone();
    manager.commit_repository_transaction(holding).unwrap();
    let before = manager.read_authority().roots().replication.clone();
    let mut incoming = fixture.transaction(&manager, 116);
    incoming.external_objects.clear();
    manager
        .commit_transferred_repository_transaction(incoming, Some(AdmissionCase::Sensitive))
        .unwrap();
    let roots = manager.read_authority().roots().clone();
    assert_ne!(
        roots.replication, before,
        "proof authority contributes independently of raw-object presence"
    );
    drop(manager);
    let reopened = open_manager(&directory);
    assert_eq!(reopened.read_authority().roots(), &roots);
    assert_eq!(
        reopened.read_authority().metadata().git_snapshot_proofs,
        vec![fixture.proof]
    );
}

fn held_proof_workspace_transaction(
    manager: &Manager,
    fixture: &Fixture,
    operation: u128,
    deltas: Vec<TreeDelta>,
    repeat_proof: bool,
) -> RepositoryTransaction {
    let workspace = manager.read_authority().metadata().workspaces[0].clone();
    let tree = workspace.tree.apply(&deltas).unwrap();
    let mut transaction = transaction_shell(manager, operation);
    transaction.workspace_mutation = Some(WorkspaceMutation {
        workspace_id: workspace.workspace_id,
        expected: WorkspaceExpectation::MustEqual {
            generation: workspace.generation,
            head: workspace.head.clone(),
            base_target: workspace.base_target.clone(),
            base_tree_hash: workspace.base_tree_hash,
            tree_hash: workspace.tree_hash,
            semantic_overlay_hash: workspace.semantic_overlay_hash,
            admission_policy: workspace.admission_policy,
        },
        new_generation: workspace.generation + 1,
        new_head: workspace.head,
        new_base_target: workspace.base_target,
        new_base_tree_hash: workspace.base_tree_hash,
        tree_deltas: deltas,
        new_tree_hash: compute_resolved_tree_hash(&tree).unwrap(),
        semantic_delta: WorkspaceSemanticDelta::default(),
        new_shared_admission_policy: workspace.shared_admission_policy,
        new_admission_policy: workspace.admission_policy,
    });
    if repeat_proof {
        transaction.git_snapshot_proofs.push(fixture.proof.clone());
    }
    transaction
}

#[test]
fn repeating_a_held_proof_does_not_readmit_an_ignored_path_under_a_new_artifact_id() {
    let directory = tempfile::tempdir().unwrap();
    let manager = open_manager(&directory);
    let fixture = Fixture::ignored();
    fixture.save(&manager);
    manager
        .commit_repository_transaction(fixture.transaction(&manager, 120))
        .unwrap();

    // Positive control: the same already-admitted artifact remains tracked.
    // Repeating immutable evidence is harmless metadata and need not be banned.
    let original_entry = manager.read_authority().metadata().workspaces[0]
        .tree
        .artifact_at_path(&RepoPath::from_utf8("ignored/fixture.txt").unwrap())
        .unwrap()
        .entry;
    let ordinary_addition = vec![TreeDelta::Added {
        artifact_id: ArtifactId(Uuid::from_u128(129)),
        new: LocatedEntry::new(RepoPath::from_utf8("ordinary.txt").unwrap(), original_entry),
    }];
    let tracked =
        held_proof_workspace_transaction(&manager, &fixture, 121, ordinary_addition, true);
    manager.commit_repository_transaction(tracked).unwrap();
    let original = manager.read_authority().metadata().workspaces[0]
        .tree
        .artifact_at_path(&RepoPath::from_utf8("ignored/fixture.txt").unwrap())
        .unwrap()
        .clone();
    let replacement_id = ArtifactId(Uuid::from_u128(130));
    assert_ne!(original.artifact_id, replacement_id);
    let replacement = vec![
        TreeDelta::Removed {
            artifact_id: original.artifact_id,
            old: original.located_entry(),
        },
        TreeDelta::Added {
            artifact_id: replacement_id,
            new: original.located_entry(),
        },
    ];

    // Both proposals name the same path, bytes, kind, mode, and old base root.
    // Only the second repeats the held proof. Neither is the initial bootstrap,
    // and the new artifact identity must be judged as untracked in both cases.
    for (operation, repeat_proof) in [(122, false), (123, true)] {
        let proposed = held_proof_workspace_transaction(
            &manager,
            &fixture,
            operation,
            replacement.clone(),
            repeat_proof,
        );
        assert!(proposed.changes.is_empty());
        refuse_unchanged(&directory, &manager, proposed, false);
        let lease = manager.read_authority();
        assert_eq!(
            lease.metadata().workspaces[0]
                .tree
                .get(&original.artifact_id),
            Some(&original)
        );
        assert!(lease.metadata().workspaces[0]
            .tree
            .get(&replacement_id)
            .is_none());
    }
}
