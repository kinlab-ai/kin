// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Independently checked raw Git provenance for a native bootstrap snapshot.
//! Parent commits remain an explicit unheld boundary, not imported history.

use std::collections::{BTreeMap, BTreeSet};

use kin_model::{
    decode_git_external_object, validate_semantic_change_id, ChangeOrigin, ExternalObjectId,
    ExternalObjectKind, ExternalObjectRecord, GitObjectBodyLoader, GitObjectClosureEntry,
    GitObjectDependencyKind, GitObjectFormat, GitSnapshotProof, ModelError, RepoPath, RepositoryId,
    ResolvedArtifact, ResolvedTree, SemanticChange, SemanticChangeId, TreeEntry,
};

use super::{
    GraphSnapshot, NativeAdmissionSource, PersistedRepositoryAuthority, RepositoryAuthorityState,
    RepositoryTransaction, StorageBackend,
};
use crate::storage::authority::VersionedAuthorityState;
use crate::KinDbError;

pub(super) type CheckedSnapshots = BTreeMap<SemanticChangeId, CheckedSnapshotTree>;

#[derive(Debug)]
pub(super) struct CheckedSnapshotTree {
    artifacts: BTreeMap<RepoPath, TreeEntry>,
    #[cfg(test)]
    boundary_parents: Vec<kin_model::GitObjectId>,
}

impl CheckedSnapshotTree {
    pub(super) fn allows(&self, artifact: &ResolvedArtifact) -> bool {
        self.artifacts.get(&artifact.path) == Some(&artifact.entry)
    }
}

fn refuse(reason: impl std::fmt::Display) -> KinDbError {
    ModelError::InvalidOperation(format!("Git snapshot proof: {reason}")).into()
}

#[cfg(test)]
thread_local! {
    /// Calls to [`validate_metadata`] on this thread, read as the endpoints of
    /// a span by the tests that pin which transactions re-validate a proof.
    static METADATA_VALIDATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn metadata_validations() -> usize {
    METADATA_VALIDATIONS.with(std::cell::Cell::get)
}

/// Validate the persisted shape even when raw-body verification can be carried
/// from a durable validation record for these exact snapshot bytes.
pub(super) fn validate_metadata(
    snapshot: &GraphSnapshot,
    metadata: &PersistedRepositoryAuthority,
) -> Result<(), KinDbError> {
    #[cfg(test)]
    METADATA_VALIDATIONS.with(|count| count.set(count.get() + 1));
    super::require_sorted_unique(
        &metadata.git_snapshot_proofs,
        |proof| proof.snapshot_change_id,
        "Git snapshot proof",
    )?;
    for proof in &metadata.git_snapshot_proofs {
        proof
            .validate_for_repository(&metadata.repository_id)
            .map_err(refuse)?;
        let change = snapshot
            .changes
            .read_change(&proof.snapshot_change_id)?
            .ok_or_else(|| refuse("snapshot change is absent from history"))?;
        validate_semantic_change_id(&change)?;
        if change.origin != ChangeOrigin::Native || !change.parents.is_empty() {
            return Err(refuse("the snapshot must be a native root"));
        }
        records_for(metadata, proof)?;
    }
    Ok(())
}

fn records_for(
    metadata: &PersistedRepositoryAuthority,
    proof: &GitSnapshotProof,
) -> Result<Vec<ExternalObjectRecord>, KinDbError> {
    proof
        .objects
        .iter()
        .map(|object| {
            metadata
                .external_objects
                .binary_search_by_key(object, |record| record.object)
                .map(|index| metadata.external_objects[index].clone())
                .map_err(|_| refuse(format!("raw descriptor {} is absent", object.oid)))
        })
        .collect()
}

/// An incoming proof can introduce a root, never retrofit permission onto
/// history this repository already held. The receiver independently checks
/// transferred roots; the ordinary local path is restricted to bootstrap.
pub(super) fn admit(
    current: &RepositoryAuthorityState,
    snapshot: &GraphSnapshot,
    metadata: &mut PersistedRepositoryAuthority,
    transaction: &RepositoryTransaction,
    source: NativeAdmissionSource,
) -> Result<(), KinDbError> {
    let mut proofs = metadata
        .git_snapshot_proofs
        .iter()
        .map(|proof| (proof.snapshot_change_id, proof.clone()))
        .collect::<BTreeMap<_, _>>();
    for proof in &transaction.git_snapshot_proofs {
        proof
            .validate_for_repository(&transaction.repository_id)
            .map_err(refuse)?;
        if let Some(held) = proofs.get(&proof.snapshot_change_id) {
            if held != proof {
                return Err(refuse("an existing snapshot proof is immutable"));
            }
            continue;
        }
        if current
            .snapshot
            .changes
            .contains_key(&proof.snapshot_change_id)
        {
            return Err(refuse("cannot add proof to an already-held unproved root"));
        }
        if !transaction
            .changes
            .iter()
            .any(|change| change.id == proof.snapshot_change_id)
        {
            return Err(refuse(
                "a new proof must accompany its introduced snapshot root",
            ));
        }
        if source == NativeAdmissionSource::LocalWorkspace {
            if current.generation() != 0
                || !current.snapshot.changes.is_empty()
                || transaction.changes.len() != 1
                || transaction.git_snapshot_proofs.len() != 1
            {
                return Err(refuse(
                    "local snapshot proof creation requires an empty generation-zero bootstrap",
                ));
            }
            let workspace = transaction
                .workspace_mutation
                .as_ref()
                .ok_or_else(|| refuse("local snapshot proof requires its bootstrap workspace"))?;
            if workspace.new_base_target
                != Some(kin_model::RefTarget::change(proof.snapshot_change_id))
                || workspace.new_generation != 0
                || !matches!(
                    workspace.expected,
                    kin_model::WorkspaceExpectation::MustNotExist
                )
            {
                return Err(refuse(
                    "local snapshot proof is not bound to its bootstrap workspace",
                ));
            }
        }
        proofs.insert(proof.snapshot_change_id, proof.clone());
    }
    metadata.git_snapshot_proofs = proofs.into_values().collect();
    validate_metadata(snapshot, metadata)
}

struct BackendBodies<'a, B: StorageBackend + ?Sized> {
    backend: &'a B,
    repository_id: &'a RepositoryId,
}

impl<B: StorageBackend + ?Sized> GitObjectBodyLoader for BackendBodies<'_, B> {
    type Error = KinDbError;

    fn load_body(&mut self, hash: &kin_model::Hash256) -> Result<Option<Vec<u8>>, Self::Error> {
        super::load_unbounded_body(
            self.backend,
            self.repository_id,
            *hash,
            "Git snapshot proof",
        )
        .map(Some)
    }
}

/// Return a private checked value for the exact incoming snapshot. Admission
/// never consumes an unchecked proof, a caller's actor, or a local state file.
pub(super) fn check_incoming<B: StorageBackend + ?Sized>(
    backend: &B,
    current: &RepositoryAuthorityState,
    snapshot: &GraphSnapshot,
    metadata: &PersistedRepositoryAuthority,
    transaction: &RepositoryTransaction,
) -> Result<CheckedSnapshots, KinDbError> {
    let mut checked = BTreeMap::new();
    let mut loader = BackendBodies {
        backend,
        repository_id: &metadata.repository_id,
    };
    for proof in &transaction.git_snapshot_proofs {
        let change = snapshot
            .changes
            .read_change(&proof.snapshot_change_id)?
            .ok_or_else(|| refuse("incoming snapshot root is absent"))?;
        let records = records_for(metadata, proof)?;
        let proof_tree = verify_snapshot_tree(
            &metadata.repository_id,
            &change,
            proof,
            &records,
            &mut loader,
        )?;
        // Repeating immutable metadata is harmless, but it cannot grant a new
        // workspace admission. Only the transaction introducing this root may
        // consume its bootstrap exception.
        if !current
            .snapshot
            .changes
            .contains_key(&proof.snapshot_change_id)
        {
            checked.insert(proof.snapshot_change_id, proof_tree);
        }
    }
    Ok(checked)
}

/// Reopen and frozen export validate raw closure and the native-root binding,
/// including stores whose later transactions merely carried that proof.
pub(super) fn check_all<L: GitObjectBodyLoader>(
    snapshot: &GraphSnapshot,
    metadata: &PersistedRepositoryAuthority,
    loader: &mut L,
) -> Result<(), KinDbError> {
    for proof in &metadata.git_snapshot_proofs {
        let change = snapshot
            .changes
            .read_change(&proof.snapshot_change_id)?
            .ok_or_else(|| refuse("persisted snapshot root is absent"))?;
        let records = records_for(metadata, proof)?;
        verify_snapshot_tree(&metadata.repository_id, &change, proof, &records, loader)?;
    }
    Ok(())
}

/// Prove a native root's exact tree against one commit and its complete
/// tree/blob closure. The commit's parents are reported as an unheld boundary;
/// they neither require closure bodies nor confer history authority.
fn verify_snapshot_tree<L: GitObjectBodyLoader>(
    expected_repository: &RepositoryId,
    snapshot: &SemanticChange,
    claim: &GitSnapshotProof,
    records: &[ExternalObjectRecord],
    loader: &mut L,
) -> Result<CheckedSnapshotTree, KinDbError> {
    claim
        .validate_for_repository(expected_repository)
        .map_err(refuse)?;
    if records
        .iter()
        .map(|record| record.object)
        .collect::<Vec<_>>()
        != claim.objects
    {
        return Err(refuse("raw descriptors differ from the declared closure"));
    }
    if &claim.repository_id != expected_repository {
        return Err(refuse("repository binding differs"));
    }
    if claim.snapshot_change_id != snapshot.id {
        return Err(refuse("snapshot change binding differs"));
    }
    validate_semantic_change_id(snapshot)?;
    if snapshot.origin != ChangeOrigin::Native || !snapshot.parents.is_empty() {
        return Err(refuse("the snapshot must be a native root"));
    }
    let snapshot_tree = ResolvedTree::default()
        .apply(&snapshot.tree_deltas)
        .map_err(refuse)?;

    let format = GitObjectFormat::from_oid(claim.source_commit);
    let source = ExternalObjectId::new(ExternalObjectKind::Commit, claim.source_commit);
    let mut entries = BTreeMap::new();
    let mut kinds = BTreeMap::new();
    let mut projection = None;
    for record in records {
        if kinds
            .insert(record.object.oid, record.object.kind)
            .is_some()
        {
            return Err(refuse("duplicate object identity"));
        }
        let body = loader
            .load_body(&record.body_hash)
            .map_err(|error| refuse(format!("read body: {error}")))?
            .ok_or_else(|| refuse(format!("missing body for {}", record.object.oid)))?;
        let decoded = decode_git_external_object(format, record, &body).map_err(refuse)?;
        if decoded.object == source {
            projection = decoded.commit_projection;
        }
        entries.insert(
            record.object,
            GitObjectClosureEntry {
                record: record.clone(),
                dependencies: decoded.dependencies,
            },
        );
    }
    let projection = projection.ok_or_else(|| refuse("source commit is absent"))?;

    // Exactly the selected commit and its tree closure may contribute. A raw
    // object merely present in CAS is not a proof that the commit contains it.
    let mut visited = BTreeSet::new();
    let mut pending = vec![source];
    while let Some(object) = pending.pop() {
        if !visited.insert(object) {
            continue;
        }
        let entry = entries
            .get(&object)
            .ok_or_else(|| refuse(format!("missing closure member {}", object.oid)))?;
        for dependency in &entry.dependencies {
            match &dependency.kind {
                GitObjectDependencyKind::CommitParent { .. } if object == source => {}
                GitObjectDependencyKind::CommitTree if object == source => {
                    pending.push(dependency.target);
                }
                GitObjectDependencyKind::TreeEntry { mode, .. }
                    if object.kind == ExternalObjectKind::Tree =>
                {
                    if *mode == kin_model::GitTreeEntryMode::Gitlink {
                        return Err(refuse("a snapshot proof cannot authorize a gitlink"));
                    }
                    pending.push(dependency.target);
                }
                _ => return Err(refuse("unexpected dependency in snapshot closure")),
            }
        }
    }
    if visited.len() != entries.len() {
        return Err(refuse(
            "the closure contains an object unreachable from the snapshot",
        ));
    }

    let borrowed = entries.iter().map(|(id, entry)| (*id, entry)).collect();
    let artifacts = super::materialize_git_tree(&borrowed, projection.raw_tree_oid)?;
    for path in artifacts.keys() {
        if crate::admission::intrinsic_repository_control(path).is_some() {
            return Err(refuse(format!("intrinsic control path {path}")));
        }
    }
    let proposed = snapshot_tree
        .artifacts_by_path()
        .map(|artifact| (artifact.path.clone(), artifact.entry))
        .collect::<BTreeMap<_, _>>();
    if proposed != artifacts {
        return Err(refuse(
            "snapshot path, digest, entry kind or mode differs from Git",
        ));
    }
    Ok(CheckedSnapshotTree {
        artifacts,
        #[cfg(test)]
        boundary_parents: projection.parent_oids,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use kin_model::{
        compute_semantic_change_id, ArtifactId, AuthorId, GitObjectId, Hash256, LocatedEntry,
        Timestamp, TreeDelta,
    };
    use sha2::{Digest, Sha256};

    #[derive(Clone)]
    struct Bodies(BTreeMap<Hash256, Vec<u8>>);

    impl GitObjectBodyLoader for Bodies {
        type Error = std::convert::Infallible;

        fn load_body(&mut self, hash: &Hash256) -> Result<Option<Vec<u8>>, Self::Error> {
            Ok(self.0.get(hash).cloned())
        }
    }

    #[derive(Clone)]
    struct Fixture {
        repository: RepositoryId,
        snapshot: SemanticChange,
        claim: GitSnapshotProof,
        records: Vec<ExternalObjectRecord>,
        bodies: Bodies,
    }

    fn object(
        format: GitObjectFormat,
        kind: ExternalObjectKind,
        body: Vec<u8>,
        records: &mut Vec<ExternalObjectRecord>,
        bodies: &mut Bodies,
    ) -> ExternalObjectRecord {
        let mut envelope = format!(
            "{} {}\0",
            std::str::from_utf8(kind.git_header()).unwrap(),
            body.len()
        )
        .into_bytes();
        envelope.extend_from_slice(&body);
        let oid = match format {
            GitObjectFormat::Sha256 => GitObjectId::sha256(Sha256::digest(&envelope).into()),
            GitObjectFormat::Sha1 => {
                use sha1::Digest as _;
                GitObjectId::sha1(sha1::Sha1::digest(&envelope).into())
            }
        };
        let record = ExternalObjectRecord::from_raw(kind, oid, &body).unwrap();
        bodies.0.insert(record.body_hash, body);
        records.push(record.clone());
        record
    }

    impl Fixture {
        fn new(format: GitObjectFormat, path: &str, mode: &str) -> Self {
            let mut records = Vec::new();
            let mut bodies = Bodies(BTreeMap::new());
            let body = b"ordinary fixture content\n";
            let blob = object(
                format,
                ExternalObjectKind::Blob,
                body.to_vec(),
                &mut records,
                &mut bodies,
            );
            let entry = match mode {
                "100644" => TreeEntry::blob(blob.body_hash, false),
                "100755" => TreeEntry::blob(blob.body_hash, true),
                "120000" => TreeEntry::symlink(blob.body_hash),
                "160000" => TreeEntry::gitlink(blob.object.oid),
                _ => panic!("unsupported fixture mode"),
            };
            let mut target = blob.object.oid;
            let mut current_mode = mode;
            for component in path.split('/').rev() {
                let mut tree = format!("{current_mode} {component}\0").into_bytes();
                tree.extend_from_slice(target.as_bytes());
                target = object(
                    format,
                    ExternalObjectKind::Tree,
                    tree,
                    &mut records,
                    &mut bodies,
                )
                .object
                .oid;
                current_mode = "40000";
            }
            let parent = match format {
                GitObjectFormat::Sha1 => GitObjectId::sha1([0x42; 20]),
                GitObjectFormat::Sha256 => GitObjectId::sha256([0x42; 32]),
            };
            let commit_body = format!(
                "tree {target}\nparent {parent}\nauthor Fixture <fixture@example.com> 1700000000 +0000\ncommitter Fixture <fixture@example.com> 1700000000 +0000\n\nsnapshot fixture\n"
            ).into_bytes();
            let commit = object(
                format,
                ExternalObjectKind::Commit,
                commit_body,
                &mut records,
                &mut bodies,
            );
            let repository = RepositoryId::new("snapshot-proof-prototype").unwrap();
            let mut snapshot = SemanticChange {
                id: SemanticChangeId::from_hash(Hash256::from_bytes([0; 32])),
                origin: ChangeOrigin::Native,
                parents: Vec::new(),
                timestamp: Timestamp(chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap()),
                author: AuthorId::new("fixture"),
                message: "snapshot fixture".to_string(),
                entity_deltas: Vec::new(),
                relation_deltas: Vec::new(),
                tree_deltas: vec![TreeDelta::Added {
                    artifact_id: ArtifactId::new(),
                    new: LocatedEntry::new(RepoPath::from_utf8(path).unwrap(), entry),
                }],
                admission_policy_delta: None,
                projected_files: Vec::new(),
                spec_link: None,
                evidence: Vec::new(),
                risk_summary: None,
                external_reference_deltas: Vec::new(),
                resolution_record_deltas: Vec::new(),
            };
            snapshot.id = compute_semantic_change_id(&snapshot).unwrap();
            records.sort_by_key(|record| record.object);
            let claim = GitSnapshotProof {
                repository_id: repository.clone(),
                snapshot_change_id: snapshot.id,
                source_commit: commit.object.oid,
                objects: records.iter().map(|record| record.object).collect(),
            };
            Self {
                repository,
                snapshot,
                claim,
                records,
                bodies,
            }
        }

        fn standard() -> Self {
            Self::new(GitObjectFormat::Sha256, "src/code.rs", "100644")
        }

        fn verify(&self) -> Result<CheckedSnapshotTree, KinDbError> {
            verify_snapshot_tree(
                &self.repository,
                &self.snapshot,
                &self.claim,
                &self.records,
                &mut self.bodies.clone(),
            )
        }

        fn resign_snapshot(&mut self) {
            self.snapshot.id = compute_semantic_change_id(&self.snapshot).unwrap();
            self.claim.snapshot_change_id = self.snapshot.id;
        }

        fn refusal(&self, expected: &str) {
            let error = self
                .verify()
                .expect_err("the altered fixture must be refused")
                .to_string();
            assert!(
                error.contains(expected),
                "expected {expected:?}, got {error}"
            );
        }
    }

    #[test]
    fn exact_nested_snapshot_and_unheld_parents_are_verified_for_both_hash_formats() {
        for format in [GitObjectFormat::Sha1, GitObjectFormat::Sha256] {
            for mode in ["100644", "100755", "120000"] {
                let fixture = Fixture::new(format, "src/code.rs", mode);
                let checked = fixture.verify().unwrap();
                assert_eq!(checked.artifacts.len(), 1);
                assert!(checked
                    .artifacts
                    .contains_key(&RepoPath::from_utf8("src/code.rs").unwrap()));
                assert_eq!(checked.boundary_parents.len(), 1);
                assert!(!fixture
                    .records
                    .iter()
                    .any(|record| record.object.oid == checked.boundary_parents[0]));
            }
        }
    }

    #[test]
    fn missing_commit_tree_or_blob_descriptor_is_refused() {
        let fixture = Fixture::standard();
        for index in 0..fixture.records.len() {
            let mut changed = fixture.clone();
            changed.records.remove(index);
            assert!(
                changed.verify().is_err(),
                "missing descriptor {index} must fail"
            );
        }
    }

    #[test]
    fn missing_or_altered_raw_body_is_refused_for_every_closure_member() {
        let fixture = Fixture::standard();
        for record in &fixture.records {
            let mut missing = fixture.clone();
            missing.bodies.0.remove(&record.body_hash);
            missing.refusal("missing body");
            let mut altered = fixture.clone();
            altered.bodies.0.get_mut(&record.body_hash).unwrap()[0] ^= 1;
            altered.refusal("body hash mismatch");
        }
    }

    #[test]
    fn valid_bodies_do_not_authorize_wrong_object_ids_lengths_or_formats() {
        let fixture = Fixture::standard();
        let mut wrong_id = fixture.clone();
        let blob = wrong_id
            .records
            .iter_mut()
            .find(|record| record.object.kind == ExternalObjectKind::Blob)
            .unwrap();
        blob.object.oid = GitObjectId::sha256([0x33; 32]);
        wrong_id.records.sort_by_key(|record| record.object);
        wrong_id.claim.objects = wrong_id
            .records
            .iter()
            .map(|record| record.object)
            .collect();
        wrong_id.refusal("recomputes to");
        let mut wrong_length = fixture.clone();
        wrong_length.records[0].body_len += 1;
        wrong_length.refusal("declares length");
        let mut wrong_format = fixture;
        wrong_format.records[0].object.oid = GitObjectId::sha1([0x33; 20]);
        assert!(wrong_format.verify().is_err());
    }

    #[test]
    fn repository_and_change_binding_are_checked_independently_of_content() {
        let mut wrong_repository = Fixture::standard();
        wrong_repository.claim.repository_id = RepositoryId::new("other-repository").unwrap();
        wrong_repository.refusal("belongs to repository other-repository");
        let mut wrong_change = Fixture::standard();
        wrong_change.claim.snapshot_change_id =
            SemanticChangeId::from_hash(Hash256::from_bytes([1; 32]));
        wrong_change.refusal("snapshot change binding differs");
        let mut altered_change = Fixture::standard();
        altered_change.snapshot.message.push_str(" altered");
        assert!(
            altered_change.verify().is_err(),
            "matching claimed id must still be recomputed"
        );
    }

    #[test]
    fn only_a_native_root_can_be_bound() {
        let mut git_origin = Fixture::standard();
        git_origin.snapshot.origin = ChangeOrigin::GitCommit {
            oid: git_origin.claim.source_commit,
        };
        git_origin.resign_snapshot();
        git_origin.refusal("native root");
        let mut child = Fixture::standard();
        child
            .snapshot
            .parents
            .push(SemanticChangeId::from_hash(Hash256::from_bytes([2; 32])));
        child.resign_snapshot();
        child.refusal("native root");
    }

    #[test]
    fn rehashed_snapshot_cannot_change_path_digest_mode_or_entry_kind() {
        for variant in 0..4 {
            let mut fixture = Fixture::standard();
            let TreeDelta::Added { new, .. } = &mut fixture.snapshot.tree_deltas[0] else {
                unreachable!()
            };
            let TreeEntry::Blob { hash, .. } = new.entry else {
                unreachable!()
            };
            match variant {
                0 => new.path = RepoPath::from_utf8("src/other.rs").unwrap(),
                1 => new.entry = TreeEntry::blob(Hash256::from_bytes([3; 32]), false),
                2 => new.entry = TreeEntry::blob(hash, true),
                3 => new.entry = TreeEntry::symlink(hash),
                _ => unreachable!(),
            }
            fixture.resign_snapshot();
            fixture.refusal("differs from Git");
        }
    }

    #[test]
    fn snapshot_must_contain_the_entire_tree_and_nothing_else() {
        let mut missing = Fixture::standard();
        missing.snapshot.tree_deltas.clear();
        missing.resign_snapshot();
        missing.refusal("differs from Git");
        let mut extra = Fixture::standard();
        extra.snapshot.tree_deltas.push(TreeDelta::Added {
            artifact_id: ArtifactId::new(),
            new: LocatedEntry::new(
                RepoPath::from_utf8("extra").unwrap(),
                TreeEntry::blob(Hash256::from_bytes([4; 32]), false),
            ),
        });
        extra.resign_snapshot();
        extra.refusal("differs from Git");
    }

    #[test]
    fn duplicate_or_unreachable_raw_objects_do_not_enlarge_the_proof() {
        let mut duplicate = Fixture::standard();
        duplicate.records.push(duplicate.records[0].clone());
        duplicate.records.sort_by_key(|record| record.object);
        duplicate.claim.objects = duplicate
            .records
            .iter()
            .map(|record| record.object)
            .collect();
        duplicate.refusal("must be unique");
        let mut extra = Fixture::standard();
        object(
            GitObjectFormat::Sha256,
            ExternalObjectKind::Blob,
            b"not committed".to_vec(),
            &mut extra.records,
            &mut extra.bodies,
        );
        extra.records.sort_by_key(|record| record.object);
        extra.claim.objects = extra.records.iter().map(|record| record.object).collect();
        extra.refusal("unreachable");
    }

    #[test]
    fn committed_intrinsic_paths_and_gitlinks_remain_refused() {
        // The raw Git decoder rejects this reserved component before the
        // materialized-tree intrinsic-control check can run.
        Fixture::new(GitObjectFormat::Sha256, ".git/config", "100644")
            .refusal("must not be the reserved '.git' component");
        for path in [
            ".kin/config",
            "fixture/.kin/config",
            "fixture/.git-export/HEAD",
            "nested/.kin-session.json",
            ".kin-reconcile-case/state",
        ] {
            Fixture::new(GitObjectFormat::Sha256, path, "100644").refusal("intrinsic control path");
        }
        Fixture::new(GitObjectFormat::Sha256, "vendor/dependency", "160000")
            .refusal("cannot authorize a gitlink");
    }
}

#[cfg(test)]
#[path = "test_git_snapshot_proof_integration.rs"]
mod integration_tests;
