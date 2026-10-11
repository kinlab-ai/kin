// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

// Registered only by the dedicated common_assertions integration target.

#[cfg(unix)]
#[test]
fn a_cached_daemon_probe_is_rejected_after_the_executable_changes() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().unwrap();
    let program = root.path().join("daemon-probe");
    let counter = root.path().join("calls");
    let runtime = IsolatedDaemonRuntime::with_cleanup_command_for_test(
        root.path(),
        PathBuf::from("/bin/sh"),
        vec![OsString::from("-c"), OsString::from(":")],
        Vec::new(),
        Duration::from_secs(5),
    );
    let quote = |text: &str| format!("'{}'", text.replace('\'', "'\\''"));
    let write_program = |stamp: &str| {
        let payload = serde_json::json!({
            "graph_snapshot_version": kin_db::GraphSnapshot::CURRENT_VERSION,
            "build": { "sha": stamp, "dirty": false },
        });
        std::fs::write(
            &program,
            format!(
                "#!/bin/sh\nprintf x >> {}\nprintf '%s\\n' {}\n",
                quote(counter.to_str().unwrap()),
                quote(&payload.to_string()),
            ),
        )
        .unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    };

    write_program(&expected_build_stamp());
    daemon_compat(&runtime, &program).unwrap();
    daemon_compat(&runtime, &program).unwrap();
    assert_eq!(std::fs::read(&counter).unwrap(), b"x");

    write_program("a different executable");
    let error = daemon_compat(&runtime, &program).unwrap_err();
    assert!(error.contains("daemon build identity"), "{error}");
    assert_eq!(std::fs::read(&counter).unwrap(), b"xx");

    std::fs::remove_file(&program).unwrap();
    assert!(daemon_compat(&runtime, &program).is_err());
}

#[cfg(windows)]
#[test]
fn isolated_daemon_runtime_is_send_and_sync() {
    fn assert_send_and_sync<T: Send + Sync>() {}

    assert_send_and_sync::<IsolatedDaemonRuntime>();
}

#[test]
fn fixture_git_commands_prepend_maintenance_suppression() {
    for program in ["git", "/usr/bin/git", "git.exe", "GIT"] {
        assert!(
            is_git_program(OsStr::new(program)),
            "{program} was not recognized as Git, so its commits would detach maintenance"
        );
    }
    for program in ["sh", "kin", "gitk", "git-upload-pack", "/usr/bin/legit"] {
        assert!(
            !is_git_program(OsStr::new(program)),
            "{program} was misread as Git"
        );
    }

    let git = Command::new("git");
    let arguments = git
        .inner_ref()
        .get_args()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        arguments.iter().map(String::as_str).collect::<Vec<_>>(),
        FIXTURE_GIT_MAINTENANCE_SUPPRESSION
            .iter()
            .chain(FIXTURE_GIT_IDENTITY_FROM_CONFIG_ONLY.iter())
            .copied()
            .collect::<Vec<_>>(),
        "a fixture Git command did not lead with the maintenance suppression and the \
         configured-identity requirement"
    );

    // The suppression is Git configuration, so it must not reach anything else.
    assert!(
        Command::new("sh").inner_ref().get_args().next().is_none(),
        "a non-Git fixture command was given Git configuration arguments"
    );
}

/// A fixture's own executables reach the child through `fixture_path_prefix`,
/// and an explicit `PATH` does not, because launch rebinds `PATH` to the
/// host's. Both halves are asserted: the second is the trap the first exists to
/// get around, and a harness that stopped rebinding would make the first pass
/// for the wrong reason.
#[test]
fn a_fixture_path_prefix_survives_the_host_path_rebinding() {
    let fixture_bin = std::env::temp_dir().join("kin-fixture-path-prefix-probe");
    let explicit = PathBuf::from("/kin-fixture-explicit-path");
    let mut command = Command::new("sh");
    command
        .env("PATH", &explicit)
        .fixture_path_prefix(&fixture_bin);
    let launched = |command: &Command| -> Vec<PathBuf> {
        let path = command
            .configured_env_for_test(OsStr::new("PATH"))
            .flatten()
            .expect("launch preparation sets PATH");
        std::env::split_paths(&path).collect()
    };

    command.prepare_for_launch_for_test();
    let entries = launched(&command);
    assert_eq!(
        entries.first(),
        Some(&fixture_bin),
        "the fixture directory must lead the launched PATH: {entries:?}"
    );
    assert!(
        !entries.contains(&explicit),
        "an explicit PATH is rebound to the host's at launch, which is why the prefix exists: \
         {entries:?}"
    );

    // Bounded launch prepares again after a test has inspected the command, so
    // preparation must not stack the prefix.
    command.prepare_for_launch_for_test();
    assert_eq!(
        launched(&command)
            .iter()
            .filter(|entry| **entry == fixture_bin)
            .count(),
        1,
        "a second preparation stacked the fixture PATH prefix"
    );
}

#[cfg(windows)]
#[test]
fn runtime_authority_names_are_case_insensitive_on_windows() {
    for hostile in [
        "kin_registry_path",
        "_kin_vfs_last_dir",
        "git_config_count",
        "Dyld_Library_Path",
        "Ld_Custom_Injection",
    ] {
        assert!(
            is_kin_authority(OsStr::new(hostile)),
            "{hostile} bypassed Windows runtime isolation"
        );
    }
    assert!(
        !is_allowed_runtime_override(OsStr::new("kin_test_runtime_owner_token")),
        "mixed-case owner token became an allowed runtime override"
    );
}

/// The startup fault levers reach the daemon a test spawns with them. The
/// isolation scrub once dropped the gate silently, so every test that held a
/// starting daemon at it was racing an unheld start.
#[test]
fn runtime_bound_commands_carry_the_startup_fault_levers() {
    for lever in [
        "KIN_DAEMON_TEST_STARTUP_GATE",
        "KIN_DAEMON_TEST_STARTUP_HOLD_SECS",
        "KIN_MCP_MACHINE_REPLIES",
        "KIN_TEST_FLUSH",
    ] {
        assert!(
            is_allowed_test_override(true, OsStr::new(lever)),
            "{lever} is scrubbed from the daemon a test spawns with it"
        );
    }
    assert!(
        !is_kin_authority(OsStr::new("KIN_TEST_FLUSH")),
        "the suite flush mode is scrubbed before reaching test children"
    );
    for authority in ["KIN_REGISTRY_PATH", "GIT_DIR", "DYLD_LIBRARY_PATH"] {
        assert!(is_kin_authority(OsStr::new(authority)));
    }
}

#[test]
fn bounded_command_capture_sink_never_retains_past_the_ceiling() {
    let state = Arc::new(Mutex::new(CapturedCommandStream::default()));
    let overflowed = Arc::new(AtomicBool::new(false));
    let cancel = Arc::new(AtomicBool::new(false));
    let post_exit = Arc::new(AtomicBool::new(false));
    let input = vec![b'x'; COMMAND_CAPTURE_LIMIT + 16 * 1024];
    drain_command_stream(
        std::io::Cursor::new(input),
        &state,
        &overflowed,
        &cancel,
        &post_exit,
    );
    let captured = state.lock().expect("capture state").clone();

    assert_eq!(captured.bytes.len(), COMMAND_CAPTURE_LIMIT);
    assert_eq!(
        captured.observed_bytes,
        (COMMAND_CAPTURE_LIMIT + 16 * 1024) as u64
    );
    assert!(captured.truncated);
    assert!(captured.done);
    assert!(overflowed.load(Ordering::Acquire));
    assert!(compact_command_capture(&captured).len() <= COMMAND_DIAGNOSTIC_LIMIT);
}

#[test]
fn compact_command_capture_hard_bounds_invalid_utf8_after_lossy_expansion() {
    let captured = CapturedCommandStream {
        bytes: vec![0xff; COMMAND_DIAGNOSTIC_LIMIT],
        observed_bytes: COMMAND_DIAGNOSTIC_LIMIT as u64,
        ..CapturedCommandStream::default()
    };
    let diagnostic = compact_command_capture(&captured);

    assert!(diagnostic.len() <= COMMAND_DIAGNOSTIC_LIMIT);
    assert!(diagnostic.ends_with(COMMAND_DIAGNOSTIC_MARKER));
    assert!(diagnostic.contains('\u{FFFD}'));
}

#[test]
fn failed_cleanup_output_is_hard_bounded_after_lossy_expansion() {
    let invalid = vec![0xff; COMMAND_CAPTURE_LIMIT];
    let diagnostic = render_failed_cleanup_output("exit status: 1", &invalid, &invalid);

    assert!(diagnostic.contains("isolated daemon cleanup failed with exit status: 1"));
    assert_eq!(
        diagnostic.matches(COMMAND_DIAGNOSTIC_MARKER).count(),
        2,
        "both cleanup streams should disclose truncation"
    );
    assert!(
        diagnostic.len() <= COMMAND_DIAGNOSTIC_LIMIT * 2 + 128,
        "cleanup diagnostic expanded past the two bounded streams: {} bytes",
        diagnostic.len()
    );
}

#[test]
fn never_eof_command_capture_is_cancelled_and_joined() {
    let mut reader = CommandCaptureReader::spawn(
        NeverEofCommandCapturePipe,
        "never-eof",
        Arc::new(AtomicBool::new(false)),
    )
    .expect("start never-EOF command capture");
    let started = Instant::now();
    reader
        .cancel_and_join_until(Instant::now() + Duration::from_secs(1))
        .expect("cancel never-EOF command capture");

    assert!(
        started.elapsed() < Duration::from_secs(1),
        "never-EOF capture exceeded its cancellation deadline"
    );
    assert!(reader.snapshot().expect("capture snapshot").done);
}

#[test]
fn bounded_capture_waits_for_a_post_exit_reader_probe() {
    let stdout_state = Arc::new(Mutex::new(CapturedCommandStream::default()));
    let stdout_reader_state = stdout_state.clone();
    let stdout_cancel = Arc::new(AtomicBool::new(false));
    let stdout_reader_cancel = stdout_cancel.clone();
    let stdout_post_exit = Arc::new(AtomicBool::new(false));
    let stdout_reader_post_exit = stdout_post_exit.clone();
    let overflowed = Arc::new(AtomicBool::new(false));
    let stdout_overflowed = overflowed.clone();
    let (release, wait_for_release) = std::sync::mpsc::sync_channel(0);
    let stdout_thread = std::thread::Builder::new()
        .name("kin-integration-delayed-stdout-capture".to_string())
        .spawn(move || {
            wait_for_release.recv().expect("release delayed reader");
            drain_command_stream(
                std::io::Cursor::new(b"late direct-child output".to_vec()),
                &stdout_reader_state,
                &stdout_overflowed,
                &stdout_reader_cancel,
                &stdout_reader_post_exit,
            );
        })
        .expect("spawn delayed stdout reader");
    let stdout = CommandCaptureReader {
        name: "stdout",
        state: stdout_state,
        thread: Some(stdout_thread),
        cancel: stdout_cancel,
        post_exit: stdout_post_exit,
    };
    let stderr =
        CommandCaptureReader::spawn(NeverEofCommandCapturePipe, "stderr", overflowed.clone())
            .expect("start never-EOF stderr reader");
    let release_thread = std::thread::spawn(move || {
        std::thread::sleep(PROCESS_QUIESCENCE.saturating_mul(2));
        release.send(()).expect("release delayed stdout reader");
    });

    let captured = BoundedCommandCapture {
        stdout,
        stderr,
        overflowed,
    }
    .finish(false)
    .expect("finish delayed post-exit capture");
    release_thread.join().expect("join delayed-reader release");

    assert_eq!(captured.stdout.bytes, b"late direct-child output");
}

#[test]
fn bounded_capture_requires_post_exit_pipe_quiescence_after_data() {
    let stdout_state = Arc::new(Mutex::new(CapturedCommandStream::default()));
    let stdout_reader_state = stdout_state.clone();
    let stdout_cancel = Arc::new(AtomicBool::new(false));
    let stdout_reader_cancel = stdout_cancel.clone();
    let stdout_post_exit = Arc::new(AtomicBool::new(false));
    let stdout_reader_post_exit = stdout_post_exit.clone();
    let overflowed = Arc::new(AtomicBool::new(false));
    let stdout_overflowed = overflowed.clone();
    let (entered_first_attempt, wait_for_first_attempt) = std::sync::mpsc::sync_channel(0);
    let (release_pre_exit_pending, wait_for_pre_exit_pending) = std::sync::mpsc::sync_channel(0);
    let (release_first_chunk, wait_for_first_chunk) = std::sync::mpsc::sync_channel(0);
    let (release_second_chunk, wait_for_second_chunk) = std::sync::mpsc::sync_channel(0);
    let stdout_thread = std::thread::Builder::new()
        .name("kin-integration-gated-stdout-capture".to_string())
        .spawn(move || {
            drain_command_stream(
                GatedTwoChunkCommandCapturePipe {
                    stage: 0,
                    first_attempt_entered: entered_first_attempt,
                    release_pre_exit_pending: wait_for_pre_exit_pending,
                    release_first_chunk: wait_for_first_chunk,
                    release_second_chunk: wait_for_second_chunk,
                    cancel: stdout_reader_cancel.clone(),
                },
                &stdout_reader_state,
                &stdout_overflowed,
                &stdout_reader_cancel,
                &stdout_reader_post_exit,
            );
        })
        .expect("spawn gated stdout reader");
    let stdout = CommandCaptureReader {
        name: "stdout",
        state: stdout_state,
        thread: Some(stdout_thread),
        cancel: stdout_cancel,
        post_exit: stdout_post_exit,
    };
    wait_for_first_attempt
        .recv()
        .expect("stdout reader entered its pre-exit probe");
    let stderr =
        CommandCaptureReader::spawn(NeverEofCommandCapturePipe, "stderr", overflowed.clone())
            .expect("start never-EOF stderr reader");
    let release_thread = std::thread::spawn(move || {
        std::thread::sleep(PROCESS_QUIESCENCE.saturating_mul(2));
        release_pre_exit_pending
            .send(())
            .expect("release pre-exit pending probe");
        std::thread::sleep(PROCESS_QUIESCENCE.saturating_mul(2));
        release_first_chunk
            .send(())
            .expect("release first stdout chunk");
        std::thread::sleep(PROCESS_QUIESCENCE.saturating_mul(2));
        release_second_chunk
            .send(())
            .expect("release second stdout chunk");
    });

    let captured = BoundedCommandCapture {
        stdout,
        stderr,
        overflowed,
    }
    .finish(false)
    .expect("finish gated post-exit capture");
    release_thread.join().expect("join gated-reader release");

    assert_eq!(captured.stdout.bytes, b"first chunk second chunk");
}

#[test]
fn daemon_rebuild_builds_the_profile_its_own_binary_was_written_into() {
    let args = |path: &str| daemon_rebuild_profile(Path::new(path));
    assert!(
        args("/w/target/debug/kin").is_empty(),
        "cargo's dev profile is the default"
    );
    assert_eq!(args("/w/target/release/kin"), ["--release"]);
    assert_eq!(
        args("/w/target/iteration/kin"),
        ["--profile", "iteration"],
        "a custom profile is only reachable by naming it"
    );
    assert_eq!(
        args("/w/target/aarch64-apple-darwin/suite/kin"),
        ["--profile", "suite"],
        "a cross build keeps its profile directory under the triple"
    );
}

#[test]
fn daemon_rebuild_follows_the_layout_its_own_binary_was_written_into() {
    let triple = "x86_64-pc-windows-msvc";

    assert_eq!(
        daemon_rebuild_target(Path::new("/w/target/debug/kin"), triple),
        None,
        "a host build already writes the directory the check reads"
    );
    assert_eq!(
        daemon_rebuild_target(
            Path::new("/w/target/x86_64-pc-windows-msvc/debug/kin.exe"),
            triple
        ),
        Some(triple),
        "a cross build is only reachable by repeating its --target"
    );
    // A target directory relocated by CARGO_TARGET_DIR keeps the layout, so the
    // triple component stays the whole signal and no path prefix is assumed.
    assert_eq!(
        daemon_rebuild_target(
            Path::new("/lane/cargo-target/nwinfix/x86_64-pc-windows-msvc/debug/kin.exe"),
            triple
        ),
        Some(triple),
    );
    assert_eq!(
        daemon_rebuild_target(
            Path::new("/w/target/aarch64-apple-darwin/debug/kin"),
            triple
        ),
        None,
        "a triple this binary was not built for must never be passed on"
    );
    assert_eq!(daemon_rebuild_target(Path::new("kin"), triple), None);
}
