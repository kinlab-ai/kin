// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

impl IsolatedDaemonRuntime {
    /// Initialize an unborn replica under this runtime's private registry.
    /// The library boundary accepts a custom default branch; a child keeps
    /// that boundary in the same environment as the CLI without mutating the
    /// parent test process's environment.
    pub fn initialize_native_replica(
        &self,
        repository: &Path,
        default_branch: &str,
    ) -> kin_core::KinLayout {
        let output = self
            .process_command_for_test(std::env::current_exe().expect("current fixture binary"))
            .args([
                "--exact",
                "common::kin_native_replica_fixture_worker",
                "--nocapture",
            ])
            .env("KIN_TEST_NATIVE_REPLICA_PATH", repository)
            .env("KIN_TEST_NATIVE_REPLICA_BRANCH", default_branch)
            .output_within(Duration::from_secs(30))
            .expect("initialize the owned native replica");
        assert!(
            output.status.success()
                && String::from_utf8_lossy(&output.stdout).contains("1 passed;"),
            "native initializer did not run exactly one successful worker: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let layout = self.layout_for(repository);
        let config = kin_core::KinConfig::load_or_default(&layout.config_path())
            .expect("read the initialized native configuration");
        assert_eq!(config.default_branch, default_branch);
        layout
    }
}

// Like the process-group guardian worker, this entry point is included in
// each integration binary that can invoke it. Ordinary test enumeration does
// no fixture work; the owning runtime supplies both explicit inputs.
#[test]
fn kin_native_replica_fixture_worker() {
    let Some(repository) = std::env::var_os("KIN_TEST_NATIVE_REPLICA_PATH") else {
        return;
    };
    let branch =
        std::env::var("KIN_TEST_NATIVE_REPLICA_BRANCH").expect("native worker default branch");
    kin_core::init_replica(Path::new(&repository), &branch)
        .expect("initialize native authority under the owned registry");
}
