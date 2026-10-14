// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Moving one store's workspace onto an older admitted commit, exactly.
//!
//! A store built once from a whole Git history holds every reachable commit as
//! a semantic change. `kin branch switch --detach <ref>` moves the workspace
//! onto any of them by the transition a branch switch makes: it replays the
//! target's recorded state, applies only what differs to the working tree, and
//! parses nothing. These tests hold it to the one claim that makes that useful,
//! which is that the store then serves exactly what a store admitted directly
//! at that commit serves, and that the working tree holds exactly that
//! commit's bytes.

use super::*;
use kin_model::WorkspaceHead;
use std::collections::BTreeMap;

/// One Git history of three commits, oldest first, with a store built at the
/// newest.
struct History {
    state: Arc<DaemonState>,
    repository: PathBuf,
    commits: [String; 3],
}

fn git_stdout<const N: usize>(repository: &FsPath, args: [&str; N]) -> String {
    let output = kin_git::test_support::fixture_git_in(repository)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn write(repository: &FsPath, path: &str, body: &str) {
    let path = repository.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

fn commit(repository: &FsPath, message: &str) -> String {
    run_test_git(repository, ["add", "--all"]);
    run_test_git(repository, ["commit", "-s", "-m", message]);
    git_stdout(repository, ["rev-parse", "HEAD"])
}

/// The template member holding the history's working tree, `.git` and `.kin`.
const HISTORY_MEMBER: &str = "repository";
/// The template member holding a clone of the history checked out detached at
/// its first commit and admitted there.
const REFERENCE_FIRST_MEMBER: &str = "reference-first";
/// The template member recording the history's three commit ids, oldest first.
const COMMITS_MEMBER: &str = "commits";

/// The three-commit history and the store admitted directly at its first
/// commit, built once per test binary and run.
///
/// Every test then works on its own copy: the history's store opened for the
/// first time through `DaemonState::open`, and the reference read through its
/// authority. A copy is the same repository at a new path, since neither an
/// in-tree `.kin` nor a `.git` names the directory it was built in. The commit
/// ids are the template's, so a reference copied from it names the same
/// commits.
fn history_template() -> crate::test_fixture_cache::Template {
    crate::test_fixture_cache::template("detached-switch-history", |staging| {
        let repository = staging.join(HISTORY_MEMBER);
        std::fs::create_dir(&repository).unwrap();
        let commits = build_history(&repository);
        // Admitted and never opened, so each copy's open is its first, as it
        // was when every test admitted its own history.
        let layout = kin_core::init_from_git(&repository).unwrap().layout;
        crate::test_fixture_cache::assert_in_tree_store(&repository, &layout);
        crate::test_fixture_cache::assert_names_no_path(&repository, &repository);
        let reference = staging.join(REFERENCE_FIRST_MEMBER);
        let output = kin_git::test_support::fixture_git_in(staging)
            .args(["clone", "--quiet", "--no-hardlinks"])
            .arg(&repository)
            .arg(&reference)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        run_test_git(
            &reference,
            ["checkout", "--quiet", "--detach", commits[0].as_str()],
        );
        let layout = kin_core::init_from_git(&reference).unwrap().layout;
        crate::test_fixture_cache::assert_in_tree_store(&reference, &layout);
        crate::test_fixture_cache::assert_names_no_path(&reference.join(".kin"), &reference);
        std::fs::write(staging.join(COMMITS_MEMBER), commits.join("\n")).unwrap();
    })
}

/// Python across three commits, so the older commit differs from the head by
/// edits, additions, deletions and a rename, and its relations cross files.
/// Returns the commit ids, oldest first.
fn build_history(repository: &FsPath) -> [String; 3] {
    run_test_git(repository, ["init", "--initial-branch=main"]);
    run_test_git(repository, ["config", "user.email", "kin@example.invalid"]);
    run_test_git(
        repository,
        ["config", "user.name", "Kin Detached Switch Test"],
    );

    write(repository, "README.md", "detached switch fixture\n");
    write(repository, "pkg/__init__.py", "");
    write(
        repository,
        "pkg/core.py",
        "def helper(value):\n    return value + 1\n\n\ndef unused():\n    return 0\n",
    );
    write(
        repository,
        "pkg/use.py",
        "from pkg.core import helper\n\n\ndef consume():\n    return helper(1)\n",
    );
    write(
        repository,
        "pkg/gone.py",
        "from pkg.core import helper\n\n\ndef doomed():\n    return helper(9)\n",
    );
    let first = commit(repository, "first");

    write(
        repository,
        "pkg/core.py",
        "def helper(value):\n    return value + 2\n\n\ndef twice(value):\n    return \
         helper(helper(value))\n\n\ndef unused():\n    return 0\n",
    );
    write(
        repository,
        "pkg/extra.py",
        "from pkg.core import twice\n\n\ndef more():\n    return twice(3)\n",
    );
    let second = commit(repository, "second");

    run_test_git(repository, ["rm", "-q", "pkg/gone.py"]);
    run_test_git(repository, ["mv", "pkg/extra.py", "pkg/more.py"]);
    write(
        repository,
        "pkg/use.py",
        "from pkg.core import helper, twice\n\n\ndef consume():\n    return helper(1) + \
         twice(1)\n",
    );
    write(
        repository,
        "pkg/late.py",
        "from pkg.more import more\n\n\ndef latest():\n    return more()\n",
    );
    write(repository, "README.md", "detached switch fixture, head\n");
    let third = commit(repository, "third");
    [first, second, third]
}

/// A fresh copy of the three-commit history with its store, opened afresh.
fn history(label: &str) -> History {
    install_test_registry_override();
    let template = history_template();
    let repository =
        std::env::temp_dir().join(format!("kin-daemon-{label}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&repository).unwrap();
    template.copy_member_into(HISTORY_MEMBER, &repository);
    let commits: Vec<String> = std::fs::read_to_string(template.path().join(COMMITS_MEMBER))
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    let state =
        Arc::new(DaemonState::open(crate::test_fixture_cache::copied_layout(&repository)).unwrap());
    History {
        state,
        repository,
        commits: commits.try_into().expect("three recorded commits"),
    }
}

/// What a store admitted directly at `commit` serves for its workspace: a
/// clone of the same history, checked out detached there, then admitted.
struct Reference {
    workspace: kin_model::WorkspaceState,
    graph: kin_db::GraphSnapshot,
    checkout: PathBuf,
}

/// A fresh copy of the store admitted directly at the history's first commit,
/// read through its authority.
fn admitted_directly_at_first() -> Reference {
    let checkout = std::env::temp_dir().join(format!(
        "kin-daemon-detached-reference-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&checkout).unwrap();
    history_template().copy_member_into(REFERENCE_FIRST_MEMBER, &checkout);
    let layout = crate::test_fixture_cache::copied_layout(&checkout);
    let authority = ActiveApiRepositoryAuthority::open_layout_for_test(&layout).unwrap();
    let lease = authority.manager.read_authority();
    let workspace = lease
        .metadata()
        .workspaces
        .iter()
        .find(|workspace| workspace.workspace_id == authority.workspace_id)
        .unwrap()
        .clone();
    let graph = lease
        .workspace_graph_snapshot(&authority.workspace_id)
        .unwrap()
        .unwrap();
    Reference {
        workspace,
        graph,
        checkout,
    }
}

fn workspace_of(state: &DaemonState) -> kin_model::WorkspaceState {
    let authority = ActiveApiRepositoryAuthority::open(state).unwrap();
    let lease = authority.manager.read_authority();
    lease
        .metadata()
        .workspaces
        .iter()
        .find(|workspace| workspace.workspace_id == authority.workspace_id)
        .cloned()
        .unwrap()
}

/// The change the store's import recorded for a Git commit.
fn change_for(state: &DaemonState, commit: &str) -> SemanticChangeId {
    let authority = ActiveApiRepositoryAuthority::open(state).unwrap();
    let lease = authority.manager.read_authority();
    lease
        .metadata()
        .aliases
        .iter()
        .find(|alias| alias.oid.to_string() == commit)
        .map(|alias| alias.change_id)
        .unwrap_or_else(|| panic!("commit {commit} was not imported"))
}

/// The state this store's own history resolves at `change`.
fn resolved_at(
    state: &DaemonState,
    change: SemanticChangeId,
) -> kin_model::graph::ResolvedGraphState {
    let authority = ActiveApiRepositoryAuthority::open(state).unwrap();
    let lease = authority.manager.read_authority();
    let mut snapshot = lease.snapshot().clone();
    snapshot.repository_authority = None;
    kin_db::InMemoryGraph::from_snapshot(snapshot)
        .unwrap()
        .resolve_graph_at(&change)
        .unwrap()
}

/// Every path and entry of a tree, without the artifact identity.
fn members(tree: &kin_model::ResolvedTree) -> Vec<(kin_model::RepoPath, kin_model::TreeEntry)> {
    tree.artifacts_by_path()
        .map(|artifact| (artifact.path.clone(), artifact.entry))
        .collect()
}

/// Every file under a checkout except Git's and Kin's own directories.
fn files_on_disk(root: &FsPath) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(root: &FsPath, directory: &FsPath, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in std::fs::read_dir(directory).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let relative = path.strip_prefix(root).unwrap().to_path_buf();
            if directory == root && matches!(entry.file_name().to_str(), Some(".git" | ".kin")) {
                continue;
            }
            if entry.file_type().unwrap().is_dir() {
                walk(root, &path, files);
            } else {
                files.insert(relative, std::fs::read(&path).unwrap());
            }
        }
    }
    let mut files = BTreeMap::new();
    walk(root, root, &mut files);
    files
}

async fn switch_detached(
    state: &Arc<DaemonState>,
    target: &str,
    operation_id: kin_model::OperationId,
) -> (StatusCode, String) {
    let request = kin_cli::commands::branch::BranchRequest::SwitchDetached {
        target: target.to_string(),
        operation_id,
        actor: AuthorId::new("detached-switch-test"),
    };
    let (status, body) = post_branch_request(Arc::clone(state), &request, None).await;
    (status, String::from_utf8_lossy(&body).to_string())
}

/// The whole exactness claim for one store standing at `commit`.
fn assert_serves_exactly(history: &History, reference: &Reference, commit: &str) {
    let state = &history.state;
    let workspace = workspace_of(state);
    let served = state.graph.to_snapshot();
    let change = change_for(state, commit);
    let resolved = resolved_at(state, change);

    // The head binds the commit as `kin init` binds a detached checkout.
    assert_eq!(workspace.head, reference.workspace.head);
    assert_eq!(workspace.base_target, reference.workspace.base_target);
    assert!(
        !workspace.holds_uncommitted_work(),
        "a switch onto a commit leaves nothing pending"
    );
    assert_eq!(workspace.base_tree_hash, Some(workspace.tree_hash));

    // The daemon serves the commit's recorded state...
    assert_eq!(served.entities, resolved.entities);
    assert_eq!(served.relations, resolved.relations);
    assert_eq!(served.resolved_tree, resolved.tree);
    assert_eq!(workspace.tree, resolved.tree);
    // ...which is what a store admitted directly at that commit serves.
    assert_eq!(served.entities, reference.graph.entities);
    assert_eq!(served.relations, reference.graph.relations);
    assert_eq!(
        members(&served.resolved_tree),
        members(&reference.graph.resolved_tree)
    );
    assert_eq!(served.resolved_tree, reference.graph.resolved_tree);
    assert_eq!(workspace.tree_hash, reference.workspace.tree_hash);

    // The working tree holds that commit's bytes and nothing else.
    assert_eq!(
        files_on_disk(&history.repository),
        files_on_disk(&reference.checkout)
    );
}

#[tokio::test]
async fn a_detached_switch_onto_an_older_commit_serves_what_a_store_admitted_there_serves() {
    let history = history("detached-exact");
    let [first, _, third] = history.commits.clone();
    let reference = admitted_directly_at_first();
    let before_workspace = workspace_of(&history.state);
    let before_graph = history.state.graph.to_snapshot();
    let before_files = files_on_disk(&history.repository);
    assert!(!reference.graph.entities.is_empty());
    assert!(!reference.graph.relations.is_empty());

    // A short id, as `git log --oneline` prints it.
    let (status, body) = switch_detached(
        &history.state,
        &format!("git:{}", &first[..10]),
        kin_model::OperationId::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let response: kin_cli::commands::branch::BranchResponse = serde_json::from_str(&body).unwrap();
    assert!(response.mutated, "{body}");
    assert!(
        response.lines[0].starts_with("Switched to detached"),
        "{body}"
    );
    assert_serves_exactly(&history, &reference, &first);

    // Back onto the head by its whole id: byte-identical to where it began,
    // apart from the head, which is detached rather than tracking main.
    let (status, body) = switch_detached(
        &history.state,
        &format!("git:{third}"),
        kin_model::OperationId::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let returned_graph = history.state.graph.to_snapshot();
    assert_eq!(returned_graph.entities, before_graph.entities);
    assert_eq!(returned_graph.relations, before_graph.relations);
    assert_eq!(returned_graph.resolved_tree, before_graph.resolved_tree);
    assert_eq!(files_on_disk(&history.repository), before_files);
    let returned = workspace_of(&history.state);
    assert_eq!(returned.tree_hash, before_workspace.tree_hash);
    assert_eq!(
        returned.semantic_overlay_hash,
        before_workspace.semantic_overlay_hash
    );
    assert!(matches!(returned.head, WorkspaceHead::Detached { .. }));

    // And a branch switch re-enters main from the detached head.
    let request = kin_cli::commands::branch::BranchRequest::Switch {
        name: kin_model::RefName::branch(b"main").unwrap(),
        operation_id: kin_model::OperationId::new(),
        actor: AuthorId::new("detached-switch-test"),
    };
    let (status, body) = post_branch_request(Arc::clone(&history.state), &request, None).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let rejoined = workspace_of(&history.state);
    assert_eq!(rejoined.head, before_workspace.head);
    assert_eq!(rejoined.base_target, before_workspace.base_target);
    assert_eq!(rejoined.tree_hash, before_workspace.tree_hash);
    assert_eq!(files_on_disk(&history.repository), before_files);
}

/// The benchmark's order of operations: Git moved the checkout first and the
/// daemon admitted what it found as uncommitted edits on the old base. The
/// switch onto the same commit finds the pending work already is the
/// destination, lands exactly there, and leaves nothing pending.
#[tokio::test]
async fn a_detached_switch_over_an_admitted_git_checkout_of_the_same_commit_leaves_nothing_pending()
{
    let history = history("detached-admitted-checkout");
    let first = history.commits[0].clone();
    let reference = admitted_directly_at_first();

    run_test_git(
        &history.repository,
        ["checkout", "--quiet", "--detach", &first],
    );
    crate::repository_admit::execute(&history.state)
        .await
        .unwrap();
    assert!(
        workspace_of(&history.state).holds_uncommitted_work(),
        "the admission must have recorded the checkout as pending work"
    );

    let (status, body) = switch_detached(
        &history.state,
        &format!("git:{first}"),
        kin_model::OperationId::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("already held at identical content"), "{body}");
    assert_serves_exactly(&history, &reference, &first);
}

/// The same order with nothing observing the working tree, the way a checkout
/// happens while no daemon runs. The first attempt finds tracked drift, the
/// route admits once and retries, and the retry lands exactly.
#[tokio::test]
async fn a_detached_switch_over_an_unobserved_git_checkout_admits_once_and_lands_exactly() {
    let history = history("detached-unobserved-checkout");
    let first = history.commits[0].clone();
    let reference = admitted_directly_at_first();

    run_test_git(
        &history.repository,
        ["checkout", "--quiet", "--detach", &first],
    );
    assert!(!workspace_of(&history.state).holds_uncommitted_work());

    let (status, body) = switch_detached(
        &history.state,
        &format!("git:{first}"),
        kin_model::OperationId::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_serves_exactly(&history, &reference, &first);
}

/// A relative selector is resolved once. Replaying the same operation after
/// the head moved must not re-resolve `HEAD~2` against the new head.
#[tokio::test]
async fn a_detached_switch_by_a_relative_selector_replays_idempotently() {
    let history = history("detached-relative-replay");
    let first = history.commits[0].clone();
    let reference = admitted_directly_at_first();
    let operation_id = kin_model::OperationId::new();

    let (status, body) = switch_detached(&history.state, "HEAD~2", operation_id).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_serves_exactly(&history, &reference, &first);
    let generation = workspace_of(&history.state).generation;

    let (status, body) = switch_detached(&history.state, "HEAD~2", operation_id).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let replay: kin_cli::commands::branch::BranchResponse = serde_json::from_str(&body).unwrap();
    assert!(replay.idempotent, "{body}");
    assert!(!replay.mutated, "{body}");
    assert_eq!(workspace_of(&history.state).generation, generation);
}

/// A semantic change id names the same point in history as its Git commit.
#[tokio::test]
async fn a_detached_switch_by_change_id_lands_on_that_change() {
    let history = history("detached-change-id");
    let second = history.commits[1].clone();
    let change = change_for(&history.state, &second);

    let (status, body) = switch_detached(
        &history.state,
        &format!("kin:{change}"),
        kin_model::OperationId::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let workspace = workspace_of(&history.state);
    assert_eq!(
        workspace.head,
        WorkspaceHead::Detached {
            target: kin_model::RefTarget::change(change)
        }
    );
    let resolved = resolved_at(&history.state, change);
    let served = history.state.graph.to_snapshot();
    assert_eq!(served.entities, resolved.entities);
    assert_eq!(served.relations, resolved.relations);
    assert_eq!(served.resolved_tree, resolved.tree);
    assert!(!workspace.holds_uncommitted_work());
}

/// The refusals a branch switch makes, a detached switch makes too: pending
/// work the destination would overwrite, and a commit kin never imported.
#[tokio::test]
async fn a_detached_switch_keeps_the_branch_switch_refusals() {
    let history = history("detached-refusals");
    let first = history.commits[0].clone();
    let before = workspace_of(&history.state);

    let (status, body) = switch_detached(
        &history.state,
        "git:0123456789abcdef0123456789abcdef01234567",
        kin_model::OperationId::new(),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("was never imported"), "{body}");

    write(
        &history.repository,
        "pkg/core.py",
        "def helper(value):\n    return value * 7\n",
    );
    crate::repository_admit::execute(&history.state)
        .await
        .unwrap();
    let pending = workspace_of(&history.state);
    assert!(pending.holds_uncommitted_work());

    let (status, body) = switch_detached(
        &history.state,
        &format!("git:{first}"),
        kin_model::OperationId::new(),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("cannot move onto"), "{body}");
    // The blocked work is named by the entity the edit changed, never by the
    // place it is projected at.
    assert!(body.contains("function `helper`"), "{body}");
    assert!(!body.contains("pkg/core.py"), "{body}");
    assert!(!body.contains("file"), "{body}");
    assert!(!body.contains("path"), "{body}");
    let after = workspace_of(&history.state);
    assert_eq!(after.generation, pending.generation);
    assert_eq!(after.head, before.head);
    assert_eq!(
        std::fs::read_to_string(history.repository.join("pkg/core.py")).unwrap(),
        "def helper(value):\n    return value * 7\n"
    );
}

/// A switch moves the workspace to another tree, so the record of which files
/// the previous tree's sweep finished must not survive it: the next sweep
/// skips a marked file by path before it compares a body, and would report
/// full cross-file coverage for a graph whose language-server edges the switch
/// dropped.
#[tokio::test]
async fn a_detached_switch_retires_the_previous_trees_enrichment_markers() {
    let history = history("detached-retires-markers");
    let first = history.commits[0].clone();
    let marker = history.state.layout.root().join("lsp-enriched-files.json");
    {
        let mut marked = history.state.lsp_enriched_files.lock().unwrap();
        marked.insert("pkg/core.py".to_string());
        marked.insert("app.py".to_string());
    }
    std::fs::write(&marker, br#"["pkg/core.py","app.py"]"#).unwrap();

    let (status, body) = switch_detached(
        &history.state,
        &format!("git:{first}"),
        kin_model::OperationId::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        history.state.lsp_enriched_files.lock().unwrap().is_empty(),
        "no file stays marked as enriched for a tree its sweep never read"
    );
    let persisted = std::fs::read_to_string(&marker).unwrap_or_default();
    assert!(
        !persisted.contains("pkg/core.py") && !persisted.contains("app.py"),
        "the marker file names none of the previous tree's files: {persisted}"
    );
    assert!(
        body.contains("kin daemon sweep"),
        "the reply says the enrichment is owed: {body}"
    );
    assert!(workspace_of(&history.state).enrichment_marks.is_empty());
}

/// A branch imported from Git targets its Git commit, and the branch list
/// reports the change that commit resolves to through its alias. The local
/// app's remotes and Git update read a branch's head from exactly this, so a
/// reading of the raw target alone found no head on a freshly imported branch.
#[tokio::test]
async fn the_branch_list_reports_the_change_a_git_branch_stands_on() {
    let history = history("branch-list-git-head");
    let head = history.commits[2].clone();
    let request = kin_cli::commands::branch::BranchRequest::List;
    let (status, body) = post_branch_request(Arc::clone(&history.state), &request, None).await;
    let body = String::from_utf8_lossy(&body).to_string();
    assert_eq!(status, StatusCode::OK, "{body}");
    let response: kin_cli::commands::branch::BranchResponse = serde_json::from_str(&body).unwrap();
    let report = response.report.expect("the list carries its report");
    let main = report
        .branches
        .iter()
        .find(|entry| entry.name == kin_model::RefName::branch(b"main").unwrap())
        .expect("main is listed");
    assert!(
        matches!(main.target, kin_model::RefTarget::ExternalObject { .. }),
        "an imported branch targets its Git commit: {:?}",
        main.target
    );
    assert_eq!(main.change_id, Some(change_for(&history.state, &head)));
}
