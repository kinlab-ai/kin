// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

use super::*;

/// Capture one fetched direct commit ref and its complete ancestor closure.
///
/// HEAD and other refs are outside this ingestion boundary. The snapshot's
/// detached head names the selected commit, without moving the source's HEAD,
/// index, refs, or working copy. The selected ref must retain its supplied OID
/// throughout capture, and shallow or incomplete ancestors fail closed.
pub fn capture_lossless_git_ref(
    repo_path: &WorktreeRoot,
    repository_id: RepositoryId,
    name: &RefName,
    expected_oid: GitObjectId,
    blob_store: &BlobStore,
) -> Result<LosslessGitRepository> {
    let repo = open_repo(repo_path.path())?;
    reject_shallow_repository(&repo)?;
    let object_format = object_format(&repo)?;
    let selected = selected_ref(&repo, name, expected_oid)?;
    let RawTarget::Direct(oid) = &selected.target else {
        unreachable!("selected_ref requires a direct target")
    };
    let pending = VecDeque::from([PendingObject {
        oid: *oid,
        expected_kind: Some(ExternalObjectKind::Commit),
        context: format!("selected fetched ref {name}"),
    }]);
    let (mut objects, _, history_boundary) = capture_objects(&repo, blob_store, pending, true)?;
    objects.sort_by_key(|record| record.object);
    reject_shallow_repository(&repo)?;
    if selected_ref(&repo, name, expected_oid)? != selected {
        return Err(GitError::InvalidSnapshot(format!(
            "selected fetched ref {name} changed during capture; fetch again"
        )));
    }
    let target = RefTarget::external_object(ExternalObjectId::new(
        ExternalObjectKind::Commit,
        expected_oid,
    ));
    let snapshot = LosslessGitRepository {
        repository_id: repository_id.clone(),
        object_format,
        objects,
        refs: RepositoryRefState {
            refs: vec![RepositoryRef {
                repository_id,
                name: name.clone(),
                target: target.clone(),
            }],
            default_ref: None,
        },
        head: WorkspaceHead::Detached { target },
        history_boundary,
        scope: CaptureScope::Reachable,
    };
    validate_snapshot(&snapshot, blob_store)?;
    Ok(snapshot)
}

fn selected_ref(
    repo: &gix::Repository,
    name: &RefName,
    expected_oid: GitObjectId,
) -> Result<RawRef> {
    let selected = collect_refs(repo)?
        .into_iter()
        .find(|raw| raw.name == name.as_bytes())
        .ok_or_else(|| {
            GitError::InvalidSnapshot(format!("selected fetched ref {name} is absent"))
        })?;
    match &selected.target {
        RawTarget::Direct(oid) if git_object_id(*oid)? == expected_oid => Ok(selected),
        _ => Err(GitError::InvalidSnapshot(format!(
            "selected fetched ref {name} no longer directly names {expected_oid}; fetch again"
        ))),
    }
}
