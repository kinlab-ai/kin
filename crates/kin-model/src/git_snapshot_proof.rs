// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Exact raw-object evidence for one native snapshot taken from Git.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    ExternalObjectId, ExternalObjectKind, GitObjectFormat, GitObjectId, ModelError, RepositoryId,
    Result, SemanticChangeId,
};

/// A repository-scoped binding from a native root to one raw Git commit.
///
/// This is evidence to verify, never a caller-issued admission grant. Storage
/// checks every named descriptor and raw body, derives the commit's tree/blob
/// closure, and compares its complete tree with the named native snapshot.
/// Parent commits are an unheld boundary; this claims no complete Git history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GitSnapshotProof {
    pub repository_id: RepositoryId,
    pub snapshot_change_id: SemanticChangeId,
    pub source_commit: GitObjectId,
    /// Strictly ordered exact closure identities. Descriptors and raw bytes
    /// live in repository external-object authority and immutable CAS.
    pub objects: Vec<ExternalObjectId>,
}

impl GitSnapshotProof {
    /// Check canonical shape only. This does not verify a commit or its tree.
    pub fn validate(&self) -> Result<()> {
        if self.objects.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(ModelError::InvalidOperation(
                "Git snapshot proof objects must be unique and in canonical order".into(),
            ));
        }
        let source = ExternalObjectId::new(ExternalObjectKind::Commit, self.source_commit);
        if !self.objects.contains(&source) {
            return Err(ModelError::InvalidOperation(
                "Git snapshot proof must contain its source commit".into(),
            ));
        }
        let format = GitObjectFormat::from_oid(self.source_commit);
        for object in &self.objects {
            if !format.matches(object.oid) {
                return Err(ModelError::InvalidOperation(
                    "Git snapshot proof cannot mix object hash formats".into(),
                ));
            }
            match object.kind {
                ExternalObjectKind::Commit if *object == source => {}
                ExternalObjectKind::Tree | ExternalObjectKind::Blob => {}
                _ => {
                    return Err(ModelError::InvalidOperation(
                        "Git snapshot proof contains only its source commit, trees and blobs"
                            .into(),
                    ));
                }
            }
        }
        Ok(())
    }

    pub fn validate_for_repository(&self, repository: &RepositoryId) -> Result<()> {
        if &self.repository_id != repository {
            return Err(ModelError::InvalidOperation(format!(
                "Git snapshot proof belongs to repository {}, not {}",
                self.repository_id, repository
            )));
        }
        self.validate()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::Hash256;

    pub(crate) fn sample(
        repository_id: RepositoryId,
        snapshot_change_id: SemanticChangeId,
    ) -> GitSnapshotProof {
        let source_commit = GitObjectId::sha1([1; 20]);
        GitSnapshotProof {
            repository_id,
            snapshot_change_id,
            source_commit,
            objects: vec![
                ExternalObjectId::new(ExternalObjectKind::Commit, source_commit),
                ExternalObjectId::new(ExternalObjectKind::Tree, GitObjectId::sha1([2; 20])),
                ExternalObjectId::new(ExternalObjectKind::Blob, GitObjectId::sha1([3; 20])),
            ],
        }
    }

    fn fixture() -> GitSnapshotProof {
        sample(
            RepositoryId::new("snapshot-proof").unwrap(),
            SemanticChangeId::from_hash(Hash256::from_bytes([4; 32])),
        )
    }

    #[test]
    fn snapshot_proof_shape_is_exact_and_round_trips() {
        let proof = fixture();
        proof.validate_for_repository(&proof.repository_id).unwrap();
        assert_eq!(
            serde_json::from_slice::<GitSnapshotProof>(&serde_json::to_vec(&proof).unwrap())
                .unwrap(),
            proof
        );
        assert_eq!(
            rmp_serde::from_slice::<GitSnapshotProof>(&rmp_serde::to_vec(&proof).unwrap()).unwrap(),
            proof
        );
    }

    #[test]
    fn snapshot_proof_shape_refuses_duplicates_order_missing_commit_and_wrong_repository() {
        let proof = fixture();
        let mut duplicate = proof.clone();
        duplicate.objects.insert(0, duplicate.objects[0]);
        assert!(duplicate.validate().is_err());
        let mut reversed = proof.clone();
        reversed.objects.reverse();
        assert!(reversed.validate().is_err());
        let mut missing = proof.clone();
        missing.objects.remove(0);
        assert!(missing.validate().is_err());
        assert!(proof
            .validate_for_repository(&RepositoryId::new("other").unwrap())
            .is_err());
    }

    #[test]
    fn snapshot_proof_shape_refuses_parents_tags_and_mixed_hash_formats() {
        let proof = fixture();
        for object in [
            ExternalObjectId::new(ExternalObjectKind::Commit, GitObjectId::sha1([5; 20])),
            ExternalObjectId::new(ExternalObjectKind::Tag, GitObjectId::sha1([6; 20])),
            ExternalObjectId::new(ExternalObjectKind::Blob, GitObjectId::sha256([7; 32])),
        ] {
            let mut changed = proof.clone();
            changed.objects.push(object);
            changed.objects.sort();
            assert!(changed.validate().is_err(), "{object:?}");
        }
    }
}
