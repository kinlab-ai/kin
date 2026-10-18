// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

use super::*;
use kin_model::{WorkspaceExpectation, WorkspaceMutation};
use std::path::Path;
use std::sync::Arc;

struct Fixture {
    _root: tempfile::TempDir,
    source: PathBuf,
    initialized: crate::InitResult,
    manager: RepositoryAuthorityManager<LocalFileBackend>,
    store: BlobStore,
    initial_oid: GitObjectId,
    initial_head: SemanticChangeId,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        std::fs::create_dir(&source).unwrap();
        let hooks = source.join(".git/hooks");
        let excludes = source.join(".git/fixture-excludes");
        kin_git::test_support::FixtureGitBatch::new()
            .git(["init", "--initial-branch=main"])
            .git(["config", "user.email", "kin@example.invalid"])
            .git(["config", "user.name", "Kin Test"])
            .git(["config", "core.hooksPath", hooks.to_str().unwrap()])
            .git(["config", "core.excludesFile", excludes.to_str().unwrap()])
            .run_in(&source);
        std::fs::create_dir_all(hooks).unwrap();
        std::fs::write(excludes, b"").unwrap();
        std::fs::write(source.join("README.md"), b"initial body\n").unwrap();
        commit(&source, "initial");
        git(&source, &["branch", "retained"]);
        git(&source, &["tag", "retained-tag"]);
        let initial_oid = oid(&git(&source, &["rev-parse", "HEAD"]));
        let initialized = crate::git_init::init_from_git(&source).unwrap();
        let initial_head = initialized.authority.initial_change_id.unwrap();
        let manager = RepositoryAuthorityManager::open(
            initialized.repository_id.clone(),
            Arc::new(LocalFileBackend::new(initialized.layout.kindb_dir())),
        )
        .unwrap();
        let store = BlobStore::new(root.path().join("capture")).unwrap();
        Self {
            _root: root,
            source,
            initialized,
            manager,
            store,
            initial_oid,
            initial_head,
        }
    }

    fn request(&self, source_oid: GitObjectId) -> GitUpdateRequest {
        GitUpdateRequest {
            operation_id: OperationId::new(),
            source_dir: self.source.clone(),
            source_ref: ref_name("refs/kin/update/main"),
            source_oid,
            destination_ref: ref_name("refs/heads/main"),
            expected_head: self.initial_head,
        }
    }

    fn fetch_descendant(&self) -> GitObjectId {
        std::fs::write(self.source.join("README.md"), b"descendant body\n").unwrap();
        commit(&self.source, "descendant");
        let fetched = oid(&git(&self.source, &["rev-parse", "HEAD"]));
        self.retain_fetched(fetched);
        fetched
    }

    fn retain_fetched(&self, fetched: GitObjectId) {
        git(
            &self.source,
            &["update-ref", "refs/kin/update/main", &fetched.to_string()],
        );
        git(
            &self.source,
            &["reset", "--hard", &self.initial_oid.to_string()],
        );
    }
}

#[test]
fn exact_fast_forward_retains_authority_and_survives_reopen() {
    if isolated("exact_fast_forward_retains_authority_and_survives_reopen") {
        return;
    }
    let fixture = Fixture::new();
    let before = fixture.manager.read_authority();
    let old_metadata = before.metadata().clone();
    let old_roots = before.roots().clone();
    let retained = before
        .resolve_ref_target(&ref_name("refs/heads/retained"))
        .unwrap();
    let fetched = fixture.fetch_descendant();
    git(
        &fixture.source,
        &["update-ref", "refs/heads/unadmitted", &fetched.to_string()],
    );
    let request = fixture.request(fetched);
    let mut prepared = prepare_git_update(
        &fixture.manager,
        fixture.initialized.workspace_id,
        &request,
        &fixture.store,
    )
    .unwrap();
    assert_eq!(prepared.admitted_changes, 1);
    assert_eq!(prepared.previous_head, fixture.initial_head);
    assert_ne!(prepared.head, fixture.initial_head);
    let new_head = prepared.head;
    let transaction = prepared.transaction.as_ref().unwrap();
    assert_eq!(transaction.changes.len(), 1);
    assert_eq!(transaction.changes[0].parents, vec![fixture.initial_head]);
    assert_eq!(transaction.aliases.len(), 1);
    assert_eq!(transaction.aliases[0].oid, fetched);
    assert_eq!(transaction.ref_mutations.len(), 1);
    assert_eq!(transaction.ref_mutations[0].name, request.destination_ref);
    // The workspace stays on its old base until the daemon follows the ref,
    // and a mutation that would leave it unchanged is not one to carry.
    assert!(transaction.workspace_mutation.is_none());
    prepared
        .seal_in_repository(&fixture.manager, &fixture.store)
        .unwrap();
    let transaction = prepared.transaction.take().unwrap();
    let mut stale = transaction.clone();
    stale.operation_id = OperationId::new();
    let (materialized, receipt, freeze) =
        crate::tree::verify_repository_workspace_projection_and_commit_repository_transaction(
            &fixture.source,
            &prepared.workspace.tree,
            &fixture.manager,
            transaction,
        )
        .unwrap();
    assert_eq!(materialized, prepared.workspace.tree.artifacts().len());
    assert_eq!(receipt.roots_before, old_roots);
    assert_eq!(freeze.roots(), &receipt.roots_after);
    // The seal reads graph-owned content under the store's shared lock, which
    // the freeze holds exclusively, so it is checked once the freeze is gone.
    drop(freeze);
    prepared.verify_published_seal(&fixture.manager).unwrap();
    assert!(fixture
        .manager
        .commit_repository_transaction(stale)
        .is_err());
    let reopened = RepositoryAuthorityManager::open(
        fixture.initialized.repository_id.clone(),
        Arc::new(LocalFileBackend::new(
            fixture.initialized.layout.kindb_dir(),
        )),
    )
    .unwrap();
    let after = reopened.read_authority();
    assert_eq!(after.roots(), &receipt.roots_after);
    assert_eq!(
        after
            .resolve_target_change_id(
                &after
                    .resolve_ref_target(&request.destination_ref)
                    .unwrap()
                    .unwrap()
            )
            .unwrap(),
        new_head
    );
    assert_eq!(
        after
            .resolve_ref_target(&ref_name("refs/heads/retained"))
            .unwrap(),
        retained
    );
    assert!(after
        .resolve_ref_target(&ref_name("refs/heads/unadmitted"))
        .unwrap()
        .is_none());
    for alias in &old_metadata.aliases {
        assert!(after.metadata().aliases.contains(alias));
    }
    let old_git = old_metadata.git_external_authority.as_ref().unwrap();
    let new_git = after.metadata().git_external_authority.as_ref().unwrap();
    assert_eq!(new_git.raw_head, old_git.raw_head);
    for raw_ref in &old_git.raw_refs {
        if raw_ref.name != request.destination_ref {
            assert!(new_git.raw_refs.contains(raw_ref));
        }
    }
    assert_eq!(
        find_workspace(&after, fixture.initialized.workspace_id).unwrap(),
        &prepared.workspace,
        "admission must leave the workspace for the follow to move"
    );
    assert_eq!(
        std::fs::read(fixture.source.join("README.md")).unwrap(),
        b"initial body\n"
    );
}

#[test]
fn admission_refuses_over_changed_tracked_bytes() {
    if isolated("admission_refuses_over_changed_tracked_bytes") {
        return;
    }
    let fixture = Fixture::new();
    let request = fixture.request(fixture.fetch_descendant());
    let mut prepared = prepare_git_update(
        &fixture.manager,
        fixture.initialized.workspace_id,
        &request,
        &fixture.store,
    )
    .unwrap();
    prepared
        .seal_in_repository(&fixture.manager, &fixture.store)
        .unwrap();
    let roots = fixture.manager.read_authority().roots().clone();
    std::fs::write(fixture.source.join("README.md"), b"edited, not admitted\n").unwrap();
    let error =
        crate::tree::verify_repository_workspace_projection_and_commit_repository_transaction(
            &fixture.source,
            &prepared.workspace.tree,
            &fixture.manager,
            prepared.transaction.take().unwrap(),
        )
        .expect_err("an edited tracked member must stop the admission");
    assert!(!error.to_string().is_empty());
    assert_eq!(fixture.manager.read_authority().roots(), &roots);
    assert_eq!(
        std::fs::read(fixture.source.join("README.md")).unwrap(),
        b"edited, not admitted\n"
    );
}

#[test]
fn selected_head_must_still_match_persisted_authority() {
    if isolated("selected_head_must_still_match_persisted_authority") {
        return;
    }
    let fixture = Fixture::new();
    let mut request = fixture.request(fixture.fetch_descendant());
    request.expected_head = SemanticChangeId::from_hash(Hash256::from_bytes([7; 32]));
    let roots = fixture.manager.read_authority().roots().clone();
    let error = prepare_git_update(
        &fixture.manager,
        fixture.initialized.workspace_id,
        &request,
        &fixture.store,
    )
    .err()
    .expect("a stale selected head must refuse");
    assert!(error.to_string().contains("moved after"));
    assert_eq!(fixture.manager.read_authority().roots(), &roots);
}

#[test]
fn fetched_head_must_descend_from_current_git_head() {
    if isolated("fetched_head_must_descend_from_current_git_head") {
        return;
    }
    let fixture = Fixture::new();
    git(&fixture.source, &["checkout", "--orphan", "unrelated"]);
    std::fs::write(fixture.source.join("README.md"), b"unrelated body\n").unwrap();
    commit(&fixture.source, "unrelated root");
    let fetched = oid(&git(&fixture.source, &["rev-parse", "HEAD"]));
    git(&fixture.source, &["checkout", "main"]);
    fixture.retain_fetched(fetched);
    let roots = fixture.manager.read_authority().roots().clone();
    let error = prepare_git_update(
        &fixture.manager,
        fixture.initialized.workspace_id,
        &fixture.request(fetched),
        &fixture.store,
    )
    .err()
    .expect("an unrelated root must refuse");
    assert!(error.to_string().contains("does not descend"));
    assert_eq!(fixture.manager.read_authority().roots(), &roots);
}

#[test]
fn fetched_ref_must_still_name_selected_exact_oid() {
    if isolated("fetched_ref_must_still_name_selected_exact_oid") {
        return;
    }
    let fixture = Fixture::new();
    let fetched = fixture.fetch_descendant();
    let request = fixture.request(fetched);
    git(
        &fixture.source,
        &[
            "update-ref",
            "refs/kin/update/main",
            &fixture.initial_oid.to_string(),
        ],
    );
    let roots = fixture.manager.read_authority().roots().clone();
    let error = prepare_git_update(
        &fixture.manager,
        fixture.initialized.workspace_id,
        &request,
        &fixture.store,
    )
    .err()
    .expect("a moved fetched ref must refuse");
    assert!(error.to_string().contains("exact Git update refused"));
    assert_eq!(fixture.manager.read_authority().roots(), &roots);
}

#[test]
fn fetching_same_exact_oid_is_a_no_op() {
    if isolated("fetching_same_exact_oid_is_a_no_op") {
        return;
    }
    let fixture = Fixture::new();
    fixture.retain_fetched(fixture.initial_oid);
    let roots = fixture.manager.read_authority().roots().clone();
    let mut prepared = prepare_git_update(
        &fixture.manager,
        fixture.initialized.workspace_id,
        &fixture.request(fixture.initial_oid),
        &fixture.store,
    )
    .unwrap();
    assert!(prepared.transaction.is_none());
    assert_eq!(prepared.admitted_changes, 0);
    assert_eq!(prepared.head, fixture.initial_head);
    prepared
        .seal_in_repository(&fixture.manager, &fixture.store)
        .unwrap();
    assert_eq!(fixture.manager.read_authority().roots(), &roots);
}

#[test]
fn native_export_marker_in_fetched_ancestry_requires_native_binding() {
    if isolated("native_export_marker_in_fetched_ancestry_requires_native_binding") {
        return;
    }
    let fixture = Fixture::new();
    let tree = git(&fixture.source, &["rev-parse", "HEAD^{tree}"]);
    let commit_body = format!(
        "tree {}\nparent {}\nauthor Kin Test <kin@example.invalid> 1700000000 +0000\ncommitter Kin Test <kin@example.invalid> 1700000000 +0000\nkin-change-id {}\n\nexported native ancestor\n",
        tree.trim(), fixture.initial_oid, fixture.initial_head
    );
    let output = kin_git::test_support::fixture_git_in(&fixture.source)
        .args(["hash-object", "-t", "commit", "-w", "--stdin"])
        .output_with_input(commit_body.as_bytes())
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let native_oid = oid(&String::from_utf8(output.stdout).unwrap());
    git(
        &fixture.source,
        &["reset", "--hard", &native_oid.to_string()],
    );
    std::fs::write(
        fixture.source.join("README.md"),
        b"child of native export\n",
    )
    .unwrap();
    commit(&fixture.source, "ordinary child");
    let fetched = oid(&git(&fixture.source, &["rev-parse", "HEAD"]));
    fixture.retain_fetched(fetched);
    let roots = fixture.manager.read_authority().roots().clone();
    let error = prepare_git_update(
        &fixture.manager,
        fixture.initialized.workspace_id,
        &fixture.request(fetched),
        &fixture.store,
    )
    .err()
    .expect("native export ancestry must not become fresh Git provenance");
    assert!(error.to_string().contains("native"));
    assert_eq!(fixture.manager.read_authority().roots(), &roots);
}

#[test]
fn native_current_head_reports_explicit_update_gap() {
    if isolated("native_current_head_reports_explicit_update_gap") {
        return;
    }
    let fixture = Fixture::new();
    let authority = fixture.manager.read_authority();
    assert!(git_update_status(&authority, fixture.initialized.workspace_id).can_update);
    let workspace = find_workspace(&authority, fixture.initialized.workspace_id).unwrap();
    let branch = ref_name("refs/heads/main");
    let target = authority.resolve_ref_target(&branch).unwrap().unwrap();
    let mut native = authority
        .snapshot()
        .changes
        .read_change(&fixture.initial_head)
        .unwrap()
        .unwrap();
    native.origin = ChangeOrigin::Native;
    native.parents = vec![fixture.initial_head];
    native.message = "native metadata change".to_string();
    native.entity_deltas.clear();
    native.relation_deltas.clear();
    native.tree_deltas.clear();
    native.admission_policy_delta = None;
    native.external_reference_deltas.clear();
    native.resolution_record_deltas.clear();
    native.projected_files.clear();
    native.id = crate::compute_semantic_change_id(&native).unwrap();
    let mut workspace_mutation = unchanged_workspace_mutation(workspace).unwrap();
    workspace_mutation.new_base_target = Some(RefTarget::change(native.id));
    fixture
        .manager
        .commit_repository_transaction(RepositoryTransaction {
            schema_version: REPOSITORY_TRANSACTION_SCHEMA_VERSION,
            operation_id: OperationId::new(),
            repository_id: fixture.initialized.repository_id.clone(),
            expected_generation: authority.roots().generation,
            expected_roots: authority.roots().clone(),
            actor: AuthorId::new("native-author"),
            reason: "advance native head".to_string(),
            changes: vec![native],
            aliases: Vec::new(),
            external_objects: Vec::new(),
            git_authority_delta: None,
            ref_mutations: vec![RefMutation {
                name: branch,
                expected: RefExpectation::MustEqual { target },
                new_target: workspace_mutation.new_base_target.clone(),
                policy: RefUpdatePolicy::FastForwardOnly,
            }],
            default_ref_mutation: None,
            workspace_mutation: Some(workspace_mutation),
            local_overlay_delta: None,
            merge_transaction_delta: None,
            sealed_observation: None,
            collaboration_delta: None,
            git_snapshot_proofs: Vec::new(),
            entity_edit_lease: None,
        })
        .unwrap();
    let status = git_update_status(
        &fixture.manager.read_authority(),
        fixture.initialized.workspace_id,
    );
    assert!(!status.can_update);
    assert!(status.reason.unwrap().contains("native Kin changes"));
    assert!(status.git_oid.is_none());
}

#[test]
fn dirty_graph_owned_workspace_refuses_before_admission() {
    if isolated("dirty_graph_owned_workspace_refuses_before_admission") {
        return;
    }
    let fixture = Fixture::new();
    let request = fixture.request(fixture.fetch_descendant());
    let authority = fixture.manager.read_authority();
    let workspace = find_workspace(&authority, fixture.initialized.workspace_id).unwrap();
    let dirty_body = b"uncommitted graph-owned body\n";
    let dirty_hash = Hash256::from_bytes(kin_blobs::digest_bytes(dirty_body));
    fixture
        .manager
        .save_source_blob(dirty_hash, dirty_body)
        .unwrap();
    let mut artifacts = workspace.tree.artifacts().cloned().collect::<Vec<_>>();
    assert_eq!(artifacts.len(), 1);
    artifacts[0].entry = kin_model::TreeEntry::Blob {
        hash: dirty_hash,
        executable: false,
    };
    let dirty_tree = kin_model::ResolvedTree::from_artifacts(artifacts).unwrap();
    let mut mutation = unchanged_workspace_mutation(workspace).unwrap();
    mutation.tree_deltas = crate::exact_tree_correction(&workspace.tree, &dirty_tree).unwrap();
    mutation.new_tree_hash = kin_model::compute_resolved_tree_hash(&dirty_tree).unwrap();
    fixture
        .manager
        .commit_repository_transaction(RepositoryTransaction {
            schema_version: REPOSITORY_TRANSACTION_SCHEMA_VERSION,
            operation_id: OperationId::new(),
            repository_id: fixture.initialized.repository_id.clone(),
            expected_generation: authority.roots().generation,
            expected_roots: authority.roots().clone(),
            actor: AuthorId::new("workspace-author"),
            reason: "retain uncommitted workspace body".to_string(),
            changes: Vec::new(),
            aliases: Vec::new(),
            external_objects: Vec::new(),
            git_authority_delta: None,
            ref_mutations: Vec::new(),
            default_ref_mutation: None,
            workspace_mutation: Some(mutation),
            local_overlay_delta: None,
            merge_transaction_delta: None,
            sealed_observation: None,
            collaboration_delta: None,
            git_snapshot_proofs: Vec::new(),
            entity_edit_lease: None,
        })
        .unwrap();
    let dirty_authority = fixture.manager.read_authority();
    let roots = dirty_authority.roots().clone();
    let status = git_update_status(&dirty_authority, fixture.initialized.workspace_id);
    assert!(!status.can_update);
    assert!(status.reason.unwrap().contains("graph-owned changes"));
    let error = prepare_git_update(
        &fixture.manager,
        fixture.initialized.workspace_id,
        &request,
        &fixture.store,
    )
    .err()
    .expect("uncommitted graph-owned work must refuse");
    assert!(error.to_string().contains("graph-owned changes"));
    assert_eq!(fixture.manager.read_authority().roots(), &roots);
}

/// A mutation of `workspace` that changes nothing yet, for a test to edit
/// into the change it commits.
fn unchanged_workspace_mutation(workspace: &WorkspaceState) -> Result<WorkspaceMutation> {
    Ok(WorkspaceMutation {
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
        new_head: workspace.head.clone(),
        new_base_target: workspace.base_target.clone(),
        new_base_tree_hash: workspace.base_tree_hash,
        tree_deltas: Vec::new(),
        new_tree_hash: workspace.tree_hash,
        semantic_delta: Default::default(),
        new_shared_admission_policy: workspace.shared_admission_policy.clone(),
        new_admission_policy: workspace.admission_policy,
    })
}

fn isolated(name: &str) -> bool {
    crate::git_init::tests::run_in_isolated_store(
        &format!("git_update::tests::{name}"),
        &["central"],
    )
}

fn ref_name(name: &str) -> RefName {
    RefName::from_utf8(name).unwrap()
}

fn oid(value: &str) -> GitObjectId {
    match value.trim().parse::<gix::ObjectId>().unwrap() {
        gix::ObjectId::Sha1(bytes) => GitObjectId::sha1(bytes),
        gix::ObjectId::Sha256(bytes) => GitObjectId::sha256(bytes),
        _ => panic!("fixture Git returned an unsupported object ID algorithm"),
    }
}

fn commit(source: &Path, message: &str) {
    kin_git::test_support::FixtureGitBatch::new()
        .git(["add", "--all"])
        .git(["commit", "-m", message])
        .run_in(source);
}

fn git(source: &Path, args: &[&str]) -> String {
    let output = kin_git::test_support::fixture_git_in(source)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}
