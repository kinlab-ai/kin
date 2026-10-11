// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The identity of a Git repository's exact refs and `HEAD`.
//!
//! A Git admission captures every `refs/*` reference and the raw `HEAD`, and
//! the history it derives is a function of exactly those and the objects they
//! reach. Git objects are content-addressed, so two repositories whose raw
//! refs and `HEAD` name the same objects hold the same reachable history. This
//! module states that identity once, over the same raw refs and `HEAD` the
//! lossless capture reads, so the identity of a checkout nobody has admitted
//! yet and the identity of the Git authority a store admitted are computed by
//! one rule and compare equal exactly when an admission of the checkout would
//! capture what the store captured.
//!
//! It reads refs only, never an object, so it costs a reference walk rather
//! than a capture.

#[cfg(test)]
use std::path::Path;

use kin_io::WorktreeRoot;

use kin_model::{GitExternalAuthority, GitObjectFormat as AuthorityObjectFormat, GitRawTarget};
use sha2::{Digest, Sha256};

use crate::error::Result;
use crate::lossless::{self, GitObjectFormat, RawTarget};

/// Schema token of the identity, so a change to what it covers reads as a
/// different identity rather than a colliding one.
pub const GIT_REF_IDENTITY_SCHEMA: &str = "kin.git-ref-identity.v1";

/// One raw target: an object id in lowercase hex, or a symbolic ref name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum RefIdentityTarget {
    Direct(String),
    Symbolic(Vec<u8>),
}

/// The exact raw refs and `HEAD` of one repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitRefIdentity {
    pub object_format: GitObjectFormat,
    /// Sorted by exact raw ref name.
    pub refs: Vec<(Vec<u8>, RefIdentityTarget)>,
    pub head: RefIdentityTarget,
}

impl GitRefIdentity {
    /// The identity of the Git repository at `repo_path`, read from its refs.
    ///
    /// Refuses a shallow repository, exactly as the lossless capture does,
    /// because its history is not the history an admission could hold.
    pub fn of_checkout(repo_path: &WorktreeRoot) -> Result<Self> {
        let repo_path = repo_path.path();
        let repo = lossless::open_repo(repo_path)?;
        lossless::reject_shallow_repository(&repo)?;
        let object_format = lossless::object_format(&repo)?;
        let mut refs = lossless::collect_refs(&repo)?
            .into_iter()
            .map(|raw| (raw.name, from_raw(raw.target)))
            .collect::<Vec<_>>();
        refs.sort();
        let head = from_raw(lossless::collect_head(&repo)?);
        Ok(Self {
            object_format,
            refs,
            head,
        })
    }

    /// The identity of the Git authority a store admitted.
    pub fn of_authority(authority: &GitExternalAuthority) -> Self {
        let mut refs = authority
            .raw_refs
            .iter()
            .map(|raw| (raw.name.as_bytes().to_vec(), from_model(&raw.target)))
            .collect::<Vec<_>>();
        refs.sort();
        Self {
            object_format: match authority.object_format {
                AuthorityObjectFormat::Sha1 => GitObjectFormat::Sha1,
                AuthorityObjectFormat::Sha256 => GitObjectFormat::Sha256,
            },
            refs,
            head: from_model(&authority.raw_head),
        }
    }

    /// The commit `HEAD` names directly or through the ref it names, when the
    /// identity holds one.
    pub fn head_object(&self) -> Option<&str> {
        let mut target = &self.head;
        for _ in 0..=self.refs.len() {
            match target {
                RefIdentityTarget::Direct(oid) => return Some(oid),
                RefIdentityTarget::Symbolic(name) => {
                    target = &self.refs.iter().find(|(candidate, _)| candidate == name)?.1;
                }
            }
        }
        None
    }

    /// `git-refs:sha256:<hex>` over the canonical form of every field.
    pub fn digest(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(GIT_REF_IDENTITY_SCHEMA.as_bytes());
        hasher.update(b"\0");
        hasher.update(self.object_format.to_string().as_bytes());
        hasher.update(b"\0");
        for (name, target) in &self.refs {
            hasher.update(b"ref\0");
            hasher.update((name.len() as u64).to_le_bytes());
            hasher.update(name);
            update_target(&mut hasher, target);
        }
        hasher.update(b"head\0");
        update_target(&mut hasher, &self.head);
        let digest: [u8; 32] = hasher.finalize().into();
        format!("git-refs:sha256:{}", hex::encode(digest))
    }
}

fn update_target(hasher: &mut Sha256, target: &RefIdentityTarget) {
    match target {
        RefIdentityTarget::Direct(oid) => {
            hasher.update(b"direct\0");
            hasher.update(oid.as_bytes());
        }
        RefIdentityTarget::Symbolic(name) => {
            hasher.update(b"symbolic\0");
            hasher.update((name.len() as u64).to_le_bytes());
            hasher.update(name);
        }
    }
    hasher.update(b"\0");
}

fn from_raw(target: RawTarget) -> RefIdentityTarget {
    match target {
        RawTarget::Direct(oid) => RefIdentityTarget::Direct(oid.to_hex().to_string()),
        RawTarget::Symbolic(name) => RefIdentityTarget::Symbolic(name),
    }
}

fn from_model(target: &GitRawTarget) -> RefIdentityTarget {
    match target {
        GitRawTarget::Direct { object } => RefIdentityTarget::Direct(object.oid.to_string()),
        GitRawTarget::Symbolic { target } => {
            RefIdentityTarget::Symbolic(target.as_bytes().to_vec())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::fixture_git_in;

    fn git(repo: &Path, args: &[&str]) {
        let output = fixture_git_in(repo).args(args).output().unwrap();
        assert!(output.status.success(), "git {args:?}: {output:?}");
    }

    fn repository(root: &Path, name: &str) -> std::path::PathBuf {
        let repo = root.join(name);
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "--initial-branch=main"]);
        std::fs::write(repo.join("a.py"), b"def f():\n    return 1\n").unwrap();
        git(&repo, &["add", "a.py"]);
        let output = fixture_git_in(&repo)
            .author_name("Kin Test")
            .author_email("kin@example.invalid")
            .committer_name("Kin Test")
            .committer_email("kin@example.invalid")
            .args(["commit", "-q", "-m", "one"])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        repo
    }

    /// A clone holds the same refs as its source once the remote-tracking refs
    /// a clone adds are the same, and any ref that moves moves the identity.
    #[test]
    fn the_identity_moves_with_every_ref_and_head_and_nothing_else() {
        fn copy_fixture_tree(source: &std::path::Path, destination: &std::path::Path) {
            let metadata = std::fs::symlink_metadata(source).unwrap();
            if metadata.is_dir() {
                std::fs::create_dir(destination).unwrap();
                for entry in std::fs::read_dir(source).unwrap() {
                    let entry = entry.unwrap();
                    copy_fixture_tree(&entry.path(), &destination.join(entry.file_name()));
                }
                std::fs::set_permissions(destination, metadata.permissions()).unwrap();
            } else {
                assert!(metadata.is_file(), "fixture contains a non-regular entry");
                std::fs::copy(source, destination).unwrap();
            }
        }

        let root = tempfile::tempdir().unwrap();
        let repo = repository(root.path(), "one");
        let first = GitRefIdentity::of_checkout(&kin_io::WorktreeRoot::new(
            &repo,
            &kin_io::Grant::ambient(),
        ))
        .unwrap();
        assert_eq!(
            first,
            GitRefIdentity::of_checkout(&kin_io::WorktreeRoot::new(
                &repo,
                &kin_io::Grant::ambient()
            ))
            .unwrap()
        );
        assert!(first.digest().starts_with("git-refs:sha256:"));
        assert!(first.head_object().is_some());

        // A copy of the repository elsewhere is the same identity.
        let copy = root.path().join("copy");
        copy_fixture_tree(&repo, &copy);
        assert_eq!(
            GitRefIdentity::of_checkout(&kin_io::WorktreeRoot::new(
                &copy,
                &kin_io::Grant::ambient()
            ))
            .unwrap()
            .digest(),
            first.digest()
        );

        // A new branch is another identity, and so is a detached HEAD.
        git(&repo, &["branch", "topic"]);
        let branched = GitRefIdentity::of_checkout(&kin_io::WorktreeRoot::new(
            &repo,
            &kin_io::Grant::ambient(),
        ))
        .unwrap();
        assert_ne!(branched.digest(), first.digest());
        git(&repo, &["checkout", "-q", "--detach"]);
        assert_ne!(
            GitRefIdentity::of_checkout(&kin_io::WorktreeRoot::new(
                &repo,
                &kin_io::Grant::ambient()
            ))
            .unwrap()
            .digest(),
            branched.digest()
        );
    }
}
