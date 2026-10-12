// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

/// A native repository whose default central store and registry stay private
/// for the entire lifetime of the test and all of its daemon children.
#[allow(dead_code)]
pub struct NativeDaemonRepository {
    pub layout: kin_core::layout::KinLayout,
    pub home: tempfile::TempDir,
}

#[allow(dead_code)]
impl NativeDaemonRepository {
    pub fn root(&self) -> &std::path::Path {
        self.layout.root()
    }

    pub fn working_dir(&self) -> &std::path::Path {
        self.layout.working_dir()
    }

    /// Apply this fixture's explicit authority after command isolation.
    pub fn bind(&self, command: &mut tokio::process::Command) {
        command
            .env("KIN_HOME", self.home.path())
            .env("KIN_REGISTRY_PATH", self.home.path().join("registry.toml"))
            .env("KIN_STORE", self.layout.root());
    }
}

/// Initialize through a contained worker so parallel tests never change the
/// parent process's environment while choosing a default central store.
#[allow(dead_code)]
pub async fn initialize_native_repository(repo: &std::path::Path) -> NativeDaemonRepository {
    let repo = repo.canonicalize().expect("canonical repository path");
    let home = tempfile::tempdir().expect("private repository home");
    let report = home.path().join("native-init.json");
    let mut command = tokio::process::Command::new(
        std::env::current_exe().expect("daemon integration test binary"),
    );
    isolate_daemon_test_command(&mut command);
    command
        .arg("--exact")
        .arg("common::native_repository_init_worker")
        .arg("--nocapture")
        .env("KIN_HOME", home.path())
        .env("KIN_REGISTRY_PATH", home.path().join("registry.toml"))
        .env("KIN_TEST_NATIVE_REPOSITORY_INIT_ROOT", &repo)
        .env("KIN_TEST_NATIVE_REPOSITORY_INIT_REPORT", &report);
    let output = daemon_test_output(
        command,
        "native repository initialization",
        std::time::Duration::from_secs(180),
    )
    .await
    .expect("contained native repository initialization");
    assert!(
        output.status.success(),
        "native repository initialization failed: {}; stdout: {}; stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&report).expect("native initialization report"))
            .expect("valid native initialization report");
    let root = std::path::PathBuf::from(
        result["root"]
            .as_str()
            .expect("reported central store root"),
    );
    let working_dir = std::path::PathBuf::from(
        result["working_dir"]
            .as_str()
            .expect("reported working directory"),
    );
    let stores = home
        .path()
        .join("stores")
        .canonicalize()
        .expect("private stores directory");
    assert_eq!(root, root.canonicalize().expect("canonical store root"));
    assert!(
        root != stores && root.starts_with(&stores),
        "store escaped its private home"
    );
    assert_eq!(working_dir, repo, "worker initialized another working tree");
    assert_eq!(
        kin_core::layout::read_working_tree_record(&root).as_deref(),
        Some(repo.as_path()),
        "store does not record this working tree"
    );
    NativeDaemonRepository {
        layout: kin_core::layout::KinLayout::external(root, repo),
        home,
    }
}

#[test]
fn native_repository_init_worker() {
    let Some(repo) = std::env::var_os("KIN_TEST_NATIVE_REPOSITORY_INIT_ROOT") else {
        return;
    };
    let report = std::env::var_os("KIN_TEST_NATIVE_REPOSITORY_INIT_REPORT")
        .expect("native initialization report path");
    let initialized =
        kin_core::init(std::path::Path::new(&repo)).expect("initialize native repository");
    assert!(
        initialized.layout.is_external(),
        "default central placement"
    );
    let output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(report)
        .expect("create owned native initialization report");
    serde_json::to_writer(
        output,
        &serde_json::json!({
            "root": initialized.layout.root(),
            "working_dir": initialized.layout.working_dir(),
        }),
    )
    .expect("write native initialization report");
}
