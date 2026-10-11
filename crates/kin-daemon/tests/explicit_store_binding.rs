// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! An explicit store remains authoritative after startup and through a commit,
//! even when the daemon's ambient store and registry belong to another repo.

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::process::Command;

mod common;

use common::{
    daemon_test_output, initialize_native_repository, isolate_daemon_test_command,
    spawn_daemon_test_command, terminate_daemon, DaemonChild, NativeDaemonRepository,
};

const READINESS_TIMEOUT: Duration = Duration::from_secs(180);
const TOKEN: &str = "explicit-store-binding-test-token";

fn explicit_store_command(
    selected: &NativeDaemonRepository,
    repo: &Path,
    runtime_home: &Path,
    ambient_store: &Path,
) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_kin-daemon"));
    isolate_daemon_test_command(&mut command);
    // Deliberately do not use selected.bind(): neither ambient lookup path
    // knows this store. Only the daemon's explicit argument selects it.
    command
        .arg("--repo")
        .arg(repo)
        .arg("--store")
        .arg(selected.root())
        .args(["--port", "0"])
        .env("KIN_HOME", runtime_home)
        .env("KIN_REGISTRY_PATH", runtime_home.join("registry.toml"))
        .env("KIN_STORE", ambient_store)
        .env("KIN_DAEMON_DISABLE_LSP", "1")
        .env("KIN_DAEMON_AUTO_EMBED", "0")
        .env("KIN_DAEMON_AUTH_TOKEN", TOKEN)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    command
}

async fn ready_base(
    child: &mut DaemonChild,
    selected: &NativeDaemonRepository,
    client: &reqwest::Client,
) -> String {
    let deadline = Instant::now() + READINESS_TIMEOUT;
    loop {
        assert!(
            child
                .try_wait()
                .expect("poll explicit-store daemon")
                .is_none(),
            "explicit-store daemon exited before readiness"
        );
        let port = std::fs::read_to_string(selected.root().join("daemon.port"))
            .ok()
            .and_then(|body| body.trim().parse::<u16>().ok())
            .filter(|port| *port != 0);
        if let Some(port) = port {
            let base = format!("http://127.0.0.1:{port}");
            if let Ok(response) = client
                .get(format!("{base}/readiness"))
                .bearer_auth(TOKEN)
                .send()
                .await
            {
                if response.status().is_success() {
                    return base;
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "explicit-store daemon never reached readiness"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn commit_with_foreign_environment(populated_registry: bool) {
    let selected_tree = tempfile::tempdir().unwrap();
    let selected = initialize_native_repository(selected_tree.path()).await;
    let unrelated_tree = tempfile::tempdir().unwrap();
    let unrelated = initialize_native_repository(unrelated_tree.path()).await;
    let empty_home = tempfile::tempdir().unwrap();
    let runtime_home = if populated_registry {
        unrelated.home.path()
    } else {
        empty_home.path()
    };
    let registry = runtime_home.join("registry.toml");
    let unrelated_binding =
        kin_core::LocalRepositoryAuthorityBinding::from_layout(&unrelated.layout).unwrap();
    if populated_registry {
        let mut foreign_registry = kin_core::registry::KinRegistry::default();
        foreign_registry.upsert(
            unrelated_binding.repository_id().to_string(),
            unrelated.working_dir().to_path_buf(),
            0,
        );
        foreign_registry.save_to(&registry).unwrap();
    }
    let registry_before = std::fs::read(&registry).ok();
    assert_eq!(registry_before.is_some(), populated_registry);
    let unrelated_roots = unrelated_binding
        .open_manager()
        .unwrap()
        .read_authority()
        .roots()
        .clone();
    let mut child = spawn_daemon_test_command(
        explicit_store_command(
            &selected,
            selected.working_dir(),
            runtime_home,
            unrelated.root(),
        ),
        "explicit-store daemon",
    )
    .expect("spawn contained explicit-store daemon");
    let client = reqwest::Client::builder()
        .timeout(READINESS_TIMEOUT)
        .build()
        .unwrap();
    let base = ready_base(&mut child, &selected, &client).await;
    let health: Value = client
        .get(format!("{base}/health"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        health["repo_root"].as_str(),
        selected.working_dir().to_str(),
        "readiness must serve the explicitly selected working tree"
    );

    // Exercise the next ordinary publication, not just startup recovery.
    // The commit route admits this exact working-copy observation itself.
    std::fs::write(
        selected.working_dir().join("answer.py"),
        b"def answer():\n    return 42\n",
    )
    .unwrap();
    let response = client
        .post(format!("{base}/commands/commit"))
        .bearer_auth(TOKEN)
        .json(&json!({
            "operation_id": kin_model::OperationId::new(),
            "timestamp": kin_model::Timestamp::now(),
            "author": "Store Test <store@example.invalid>",
            "message": "Publish through the explicitly selected store"
        }))
        .send()
        .await
        .expect("ordinary source commit answers");
    let status = response.status();
    let body = response.text().await.unwrap();
    // Explicit cleanup precedes assertions about publication. DaemonChild
    // also owns containment if an earlier assertion or request panics.
    terminate_daemon(&mut child, "explicit-store daemon")
        .await
        .expect("terminate and reap explicit-store daemon");
    assert!(
        status.is_success(),
        "ordinary source commit: {status} {body}"
    );
    let committed: Value = serde_json::from_str(&body).unwrap();
    let change = kin_model::SemanticChangeId::from_hash(
        kin_model::Hash256::from_hex(committed["change_id"].as_str().unwrap()).unwrap(),
    );
    let binding = kin_core::LocalRepositoryAuthorityBinding::from_layout(&selected.layout).unwrap();
    let authority = binding.open_manager().unwrap();
    let lease = authority.read_authority();
    assert!(
        lease.snapshot().changes.contains_key(&change),
        "the returned change must survive reopening the selected store"
    );
    let workspace = lease
        .metadata()
        .workspaces
        .iter()
        .find(|workspace| workspace.workspace_id == binding.workspace_id())
        .unwrap();
    assert!(workspace
        .tree
        .artifact_at_path(&kin_model::RepoPath::from_utf8("answer.py").unwrap())
        .is_some());
    assert_eq!(
        unrelated_binding
            .open_manager()
            .unwrap()
            .read_authority()
            .roots(),
        &unrelated_roots,
        "the ambient store must retain its exact authority"
    );
    assert_eq!(std::fs::read(&registry).ok(), registry_before);
    assert!(!selected.working_dir().join(".kin").exists());
    assert!(!unrelated.root().join("daemon.port").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn explicit_store_commits_with_an_empty_runtime_registry() {
    commit_with_foreign_environment(false).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn explicit_store_commits_with_an_unrelated_runtime_registry() {
    commit_with_foreign_environment(true).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn explicit_store_rejects_a_different_working_tree_before_readiness() {
    let selected_tree = tempfile::tempdir().unwrap();
    let selected = initialize_native_repository(selected_tree.path()).await;
    let unrelated_tree = tempfile::tempdir().unwrap();
    let unrelated = initialize_native_repository(unrelated_tree.path()).await;
    let output = daemon_test_output(
        explicit_store_command(
            &selected,
            unrelated.working_dir(),
            unrelated.home.path(),
            unrelated.root(),
        ),
        "mismatched explicit-store daemon",
        READINESS_TIMEOUT,
    )
    .await
    .expect("reap mismatched explicit-store daemon");
    assert!(
        !output.status.success(),
        "a mismatched store must be refused"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("serves") && stderr.contains("not"),
        "expected the explicit working-tree mismatch diagnostic: {stderr}"
    );
    assert!(!selected.root().join("daemon.port").exists());
    assert!(!unrelated.root().join("daemon.port").exists());
    assert!(!selected.working_dir().join(".kin").exists());
    assert!(!unrelated.working_dir().join(".kin").exists());
}
