// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

// Registered only by the dedicated common_assertions integration target.

const DAEMON_AUTHORITY_SCRUB_WORKER: &str = "KIN_TEST_DAEMON_AUTHORITY_SCRUB_WORKER";

/// Deliberately bypass only the final external-authority scrub so the
/// adversarial scrub test can inject ambient Git/loader variables into its
/// worker. Lifecycle bounds and OS descendant containment remain identical to
/// every other direct daemon test launch.
async fn adversarial_authority_worker_output(
    command: Command,
    label: &str,
    timeout: Duration,
) -> Result<Output, String> {
    contained_test_output(command, label, timeout, false).await
}

#[cfg(windows)]
#[test]
fn daemon_isolation_treats_windows_environment_names_case_insensitively() {
    for hostile in [
        "kin_registry_path",
        "_kin_vfs_last_dir",
        "git_config_count",
        "Dyld_Library_Path",
        "Ld_Custom_Injection",
    ] {
        assert!(
            is_daemon_test_authority(std::ffi::OsStr::new(hostile)),
            "{hostile} bypassed Windows environment-name isolation"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn daemon_isolation_scrubs_ambient_and_command_local_git_and_loader_authority() {
    const HOSTILE_KEYS: &[&str] = &[
        "GIT_DIR",
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_KEY_0",
        "GIT_CONFIG_VALUE_0",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_NAMESPACE",
        "GIT_DEFAULT_HASH",
        "GIT_DEFAULT_REF_FORMAT",
        "LD_CUSTOM_INJECTION",
    ];
    if let Some(marker) = std::env::var_os(DAEMON_AUTHORITY_SCRUB_WORKER) {
        assert_eq!(std::env::var("KIN_TEST_FLUSH").as_deref(), Ok("fsync"));
        assert!(!is_daemon_test_authority(std::ffi::OsStr::new(
            "KIN_TEST_FLUSH"
        )));
        assert!(is_daemon_test_authority(std::ffi::OsStr::new(
            "KIN_REGISTRY_PATH"
        )));
        for phase in ["ambient", "command-local-before", "command-local-after"] {
            let mut command =
                Command::new(std::env::current_exe().expect("current test executable"));
            if phase == "command-local-before" {
                command.env("KIN_TEST_FLUSH", "fsync");
                for key in HOSTILE_KEYS {
                    command.env(key, "command-local-hostile");
                }
            }
            isolate_daemon_test_command(&mut command);
            let flush = command
                .as_std()
                .get_envs()
                .find(|(key, _)| env_name_eq(&key.to_string_lossy(), "KIN_TEST_FLUSH"));
            assert!(
                flush.is_none() || flush.is_some_and(|(_, value)| value == Some("fsync".as_ref())),
                "suite flush mode was erased by {phase} daemon isolation"
            );
            if phase == "command-local-after" {
                for key in HOSTILE_KEYS {
                    command.env(key, "post-isolation-hostile");
                }
                command.env("KIN_DAEMON_DISABLE_LSP", "1");
                prepare_daemon_test_command_for_spawn(&mut command);
                let intentional_kin = command
                    .as_std()
                    .get_envs()
                    .find(|(configured, _)| {
                        env_name_eq(&configured.to_string_lossy(), "KIN_DAEMON_DISABLE_LSP")
                    })
                    .and_then(|(_, value)| value)
                    .map(std::ffi::OsStr::to_os_string);
                assert_eq!(
                    intentional_kin,
                    Some("1".into()),
                    "final external-authority scrub erased intentional Kin config"
                );
            }
            for key in HOSTILE_KEYS {
                let configured = command
                    .as_std()
                    .get_envs()
                    .find(|(configured, _)| env_name_eq(&configured.to_string_lossy(), key))
                    .map(|(_, value)| value.map(std::ffi::OsStr::to_os_string));
                assert_eq!(
                    configured,
                    Some(None),
                    "{key} survived {phase} daemon test isolation"
                );
            }
        }
        std::fs::write(marker, b"scrubbed").expect("write daemon authority scrub marker");
        return;
    }

    let root = tempfile::TempDir::new().expect("tempdir");
    let marker = root.path().join("authority-scrub.marker");
    let mut worker = Command::new(std::env::current_exe().expect("current test executable"));
    worker
        .args([
            "--exact",
            "common::daemon_isolation_scrubs_ambient_and_command_local_git_and_loader_authority",
            "--nocapture",
        ])
        .env(DAEMON_AUTHORITY_SCRUB_WORKER, &marker)
        .env("KIN_TEST_FLUSH", "fsync")
        .stdin(Stdio::null());
    for key in HOSTILE_KEYS {
        worker.env(key, "ambient-hostile");
    }
    let output = adversarial_authority_worker_output(
        worker,
        "daemon authority scrub worker",
        Duration::from_secs(10),
    )
    .await
    .expect("run daemon authority scrub worker");
    assert!(
        output.status.success(),
        "daemon authority scrub worker failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read(&marker).expect("read daemon authority scrub marker"),
        b"scrubbed"
    );
}

#[tokio::test]
async fn bounded_daemon_capture_sink_never_retains_past_the_ceiling() {
    let overflowed = Arc::new(AtomicBool::new(false));
    let input = vec![b'x'; DAEMON_TEST_CAPTURE_LIMIT + 16 * 1024];
    let captured = drain_daemon_test_stream(&input[..], overflowed.clone())
        .await
        .expect("drain bounded daemon capture fixture");

    assert_eq!(captured.bytes.len(), DAEMON_TEST_CAPTURE_LIMIT);
    assert_eq!(
        captured.observed_bytes,
        (DAEMON_TEST_CAPTURE_LIMIT + 16 * 1024) as u64
    );
    assert!(captured.truncated);
    assert!(overflowed.load(Ordering::Acquire));
    assert!(compact_daemon_test_capture(&captured).len() <= DAEMON_TEST_DIAGNOSTIC_LIMIT);
}

#[tokio::test]
async fn compact_daemon_capture_hard_bounds_invalid_utf8_after_lossy_expansion() {
    let captured = CapturedDaemonTestStream {
        bytes: vec![0xff; DAEMON_TEST_DIAGNOSTIC_LIMIT],
        observed_bytes: DAEMON_TEST_DIAGNOSTIC_LIMIT as u64,
        truncated: false,
    };
    let diagnostic = compact_daemon_test_capture(&captured);

    assert!(diagnostic.len() <= DAEMON_TEST_DIAGNOSTIC_LIMIT);
    assert!(diagnostic.ends_with(DAEMON_TEST_DIAGNOSTIC_MARKER));
    assert!(diagnostic.contains('\u{FFFD}'));
}

#[tokio::test]
async fn never_eof_daemon_capture_is_aborted_at_its_deadline() {
    let (_writer, reader) = tokio::io::duplex(64);
    let task = tokio::spawn(drain_daemon_test_stream(
        reader,
        Arc::new(AtomicBool::new(false)),
    ));
    let started = Instant::now();
    let error = finish_daemon_capture_task(
        task,
        "never-eof",
        Instant::now() + Duration::from_millis(50),
    )
    .await
    .expect_err("never-EOF capture must be aborted");

    assert!(error.contains("aborted after its EOF deadline"), "{error}");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "async capture abort exceeded its bounded join window"
    );
}
