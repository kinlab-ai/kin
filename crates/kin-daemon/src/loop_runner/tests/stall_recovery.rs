// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

// The reconcile loop against the background-work supervisor: a batch that runs
// longer than the stall threshold, a stall the loop confirms at its own
// checkpoint, and the recovery a `kin admit` shares with the loop. Included
// into `loop_runner::tests`.

/// The reconcile loop running against a real watch, with its arming awaited.
#[cfg(unix)]
async fn start_watched_loop(
    state: &Arc<DaemonState>,
) -> (
    tokio::sync::watch::Sender<bool>,
    tokio::task::JoinHandle<Result<()>>,
) {
    // These cases grade the rounds a notification causes. A lost canary's
    // complete admission would be a second route to the states they assert on.
    use_delivery_canary_for_test(state.layout.working_dir(), None);
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let (armed_tx, armed_rx) = tokio::sync::oneshot::channel();
    let runner = tokio::spawn(run_loop_armed(
        Arc::clone(state),
        LoopConfig {
            poll_interval_ms: 10,
            batch_size: 16,
        },
        cancel_rx,
        Some(WatchArmed::new(armed_tx)),
    ));
    assert_eq!(
        crate::daemon::await_watch_armed(armed_rx, WATCH_ARMING_BOUND).await,
        crate::daemon::WatchArming::Armed,
        "the real watch must acknowledge delivery before the controlled write"
    );
    (cancel_tx, runner)
}

#[cfg(unix)]
async fn stop_watched_loop(
    cancel_tx: tokio::sync::watch::Sender<bool>,
    runner: tokio::task::JoinHandle<Result<()>>,
) {
    cancel_tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(30), runner)
        .await
        .expect("the loop stops on shutdown")
        .expect("the loop task does not panic")
        .expect("the loop exits cleanly");
}

fn entity_named(state: &DaemonState, name: &str) -> bool {
    !state
        .graph
        .query_entities(&EntityFilter {
            name_pattern: Some(name.into()),
            ..Default::default()
        })
        .unwrap()
        .is_empty()
}

/// A batch whose stages outlast the stall threshold is not parked.
///
/// Measured on 2026-10-19: one complete admission of a 1,855-file kin-router
/// delta spent 2,722 s in `prepare_changed_sources`. The supervisor saw a
/// working pass with no durable progress for 629 s and asked it to stop. The
/// batch finished anyway, because nothing inside it reads the halt, and then
/// the loop parked at its next checkpoint and admitted nothing more until the
/// daemon restarted.
///
/// Here the stage is made to outlast the threshold by sweeping the supervisor
/// from inside the admission, between the capture of its live observation and
/// its publication, with a clock a whole threshold ahead. That is the
/// supervisor's view of one silent stage longer than the threshold, and its
/// verdict is the right one for what it can see. What must follow is that the
/// batch publishes, the loop withdraws the verdict at its checkpoint because
/// the pass completed work after it, and the next edit is admitted.
///
/// Breaking it: park on every halt at the checkpoint again, as before, and the
/// second write is never admitted.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_batch_that_outlasts_the_stall_threshold_publishes_and_the_loop_keeps_admitting() {
    let repo = tempfile::tempdir().unwrap();
    let state = open_test_state(&repo);
    let (cancel_tx, runner) = start_watched_loop(&state).await;
    let pass = state
        .background_work
        .pass(crate::background_work::PASS_RECONCILE);

    let verdicts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = Arc::clone(&verdicts);
    set_admission_capture_hook_for_test(
        &state,
        Box::new(move |state: &DaemonState| {
            let ahead = state.background_work.stall_threshold() + Duration::from_secs(60);
            let stopped = state.background_work.sweep(Instant::now() + ahead);
            seen.store(stopped.len(), Ordering::SeqCst);
        }),
    );

    let progress = reconcile_progress(&state);
    write_and_await_progress(
        &state,
        "long_stage.py",
        "def long_stage():\n    return 1\n",
        progress,
    )
    .await;
    assert_eq!(
        verdicts.load(Ordering::SeqCst),
        1,
        "the supervisor must have judged the silent stage a stall from where it stood"
    );
    assert!(
        entity_named(&state, "long_stage"),
        "the stop must never discard the batch that was already prepared when it landed"
    );

    let progress = reconcile_progress(&state);
    write_and_await_progress(&state, "after.py", "def after():\n    return 2\n", progress).await;
    assert!(entity_named(&state, "after"));
    assert!(
        !pass.halted(),
        "a pass that completed work after the verdict must not stay stopped"
    );
    assert_ne!(
        state.reconciliation_status.load(Ordering::Relaxed),
        RECON_PARKED,
        "the loop must not report itself parked"
    );
    assert!(state
        .background_work
        .reconcile_report(Instant::now())
        .parked
        .is_none());
    assert_eq!(
        pass.stall_restarts(),
        0,
        "a withdrawn verdict is not a restart and must not spend the recovery allowance"
    );

    stop_watched_loop(cancel_tx, runner).await;
}

/// A stall the loop confirms at its checkpoint restarts it after a backoff.
///
/// Nothing completed between the supervisor's verdict and the checkpoint, so
/// the stall was real. The loop used to park for the life of the daemon with
/// "a daemon restart retries the pass". It now reports itself parked while it
/// waits out the backoff, restarts, and admits the next edit, with the restart
/// counted against its recovery allowance.
///
/// Breaking it: park on a supervisor stall as before, and the write below is
/// never admitted.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stall_confirmed_at_the_checkpoint_restarts_the_loop_after_a_backoff() {
    let repo = tempfile::tempdir().unwrap();
    let state = open_test_state(&repo);
    state
        .background_work
        .set_stall_restart_base_for_test(Duration::from_millis(300));
    let pass = state
        .background_work
        .pass(crate::background_work::PASS_RECONCILE);
    // The verdict on a pass that worked a whole threshold and completed
    // nothing, delivered before the loop's first checkpoint.
    let started = Instant::now();
    pass.working(started);
    let stopped = state
        .background_work
        .sweep(started + state.background_work.stall_threshold() + Duration::from_secs(1));
    assert_eq!(stopped.len(), 1, "the starved pass is stopped");
    assert!(pass.supervisor_stalled());

    let (cancel_tx, runner) = start_watched_loop(&state).await;
    // The wait is reported as a park while it lasts, so a reader can see it.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut saw_parked = false;
    while pass.halted() {
        saw_parked |= state.reconciliation_status.load(Ordering::Relaxed) == RECON_PARKED;
        assert!(
            Instant::now() < deadline,
            "the loop never restarted after a confirmed stall"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        saw_parked,
        "the backoff before a restart must be visible as a park"
    );
    assert_eq!(pass.stall_restarts(), 1, "the restart is counted");

    let progress = reconcile_progress(&state);
    write_and_await_progress(
        &state,
        "restarted.py",
        "def restarted():\n    return 1\n",
        progress,
    )
    .await;
    assert!(entity_named(&state, "restarted"));
    assert!(pass.halt_reason().is_none());

    stop_watched_loop(cancel_tx, runner).await;
}

/// Every stage of a complete admission, and every file its batch indexes,
/// reports completed work to a working reconcile pass.
///
/// Those reports are the supervisor's only evidence that a batch which runs
/// longer than the stall threshold is still advancing, because the batch
/// records durable progress only when it publishes. The pass starts working,
/// the admission runs, and the supervisor then looks from a clock that is a
/// whole threshold past the start of the working stretch but not past the
/// admission's last report: a pass with those reports is running, and one
/// without them would be stopped.
///
/// Breaking it: drop the reports from `admission_stage` and the batch's
/// indexing loop, and the sweep stops the pass.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_complete_admission_reports_each_stage_and_file_to_a_working_reconcile_pass() {
    let repo = tempfile::tempdir().unwrap();
    let state = open_test_state(&repo);
    for (path, body) in [
        ("one.py", "def one():\n    return 1\n"),
        ("two.py", "def two():\n    return 2\n"),
        ("three.py", "def three():\n    return 3\n"),
    ] {
        std::fs::write(state.layout.working_dir().join(path), body).unwrap();
    }
    let pass = state
        .background_work
        .pass(crate::background_work::PASS_RECONCILE);
    let started = Instant::now();
    pass.working(started);
    // A monotonic gap between the start of the stretch and the first report,
    // so the sweep below can sit after the one and before the other.
    std::thread::sleep(Duration::from_millis(50));
    let before = pass.liveness_units();
    {
        let mut reconciler = state.reconciler.write().await;
        exact_tree_admission_with_reconciler(
            &state,
            None,
            TreePublication::Standalone,
            &mut reconciler,
        )
        .expect("the admission succeeds");
    }
    let reported = pass.liveness_units() - before;
    // Nine stages (scan, observe, sensitive preflight, stage, prepare, join,
    // capture, publish, apply) and one per file the batch indexed. Graded
    // loosely, so a stage this admission legitimately skips does not fail it.
    assert!(
        reported >= 9,
        "a complete admission of three sources reported only {reported} completed units"
    );
    let threshold = state.background_work.stall_threshold();
    assert!(
        state
            .background_work
            .sweep(started + threshold + Duration::from_millis(25))
            .is_empty(),
        "a pass whose admission reported its stages was stopped"
    );
    assert!(!pass.halted());
    // The other direction with the same clock: silence for a whole threshold
    // after the last report is still a stall. Without this the assertion above
    // would hold for a supervisor that stops nothing.
    let stopped = state
        .background_work
        .sweep(Instant::now() + threshold + Duration::from_secs(1));
    assert_eq!(stopped.len(), 1, "a silent pass must still be stopped");
}

/// A `kin admit` that arrives while the loop is recovering a watcher loss
/// attaches to that recovery instead of deriving the whole tree again.
///
/// Measured on 2026-10-19: the loop's recovery of a 1,855-file delta held the
/// coordination gate for 2,722 s, a `kin admit` that had arrived meanwhile then
/// ran its own complete admission for another 2,189 s, and both refused the
/// same way. The recovery is the same pass as the request, so it takes the
/// same in-flight slot, and the request reports the recovery's transition.
///
/// Breaking it: run the recovery without claiming the slot, and the request
/// queues on the gate, re-observes a tree the recovery already published, and
/// reports that nothing moved.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_kin_admit_during_a_watcher_loss_recovery_reports_that_recovery() {
    let parent = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir_in(parent.path()).unwrap();
    let state = open_test_state(&repo);
    state.is_initialized.store(true, Ordering::Relaxed);
    std::fs::write(
        state.layout.working_dir().join("recovered.py"),
        "def recovered():\n    return 1\n",
    )
    .unwrap();

    let _guard = AdmissionPauseGuard;
    *ADMISSION_PAUSE_FOR_TEST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        Some((Arc::as_ptr(&state) as usize, Duration::from_millis(1_500)));
    let recovering = {
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            crate::repository_admit::recover_watcher_loss(&state, || true)
                .await
                .map(|response| response.report.and_then(|report| report.tree_moved))
        })
    };
    // Let the recovery claim the slot and reach its pause.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let requested = crate::repository_admit::execute(&state)
        .await
        .expect("the request reports an outcome");
    let recovered = recovering
        .await
        .unwrap()
        .expect("the recovery reports an outcome");

    assert_eq!(recovered, Some(true), "the recovery moved the tree");
    assert_eq!(
        requested.report.and_then(|report| report.tree_moved),
        Some(true),
        "the request must report the recovery's transition, not a second pass that found \
         nothing left to admit"
    );
    assert!(entity_named(&state, "recovered"));
}

/// A failed watcher-loss recovery waits at least four times what it cost.
///
/// The ladder alone started the next complete admission 2 s after one that
/// had spent 2,410 s and failed, so a recovery that keeps failing held a core
/// for as long as the daemon stayed up.
#[test]
fn watcher_recovery_spacing_scales_with_what_the_failed_attempt_cost() {
    // A cheap attempt keeps the ladder's own schedule.
    assert_eq!(
        watcher_recovery_spacing(1, Duration::from_millis(100)),
        ADMISSION_HOLD_BASE
    );
    assert_eq!(
        watcher_recovery_spacing(40, Duration::from_millis(100)),
        ADMISSION_HOLD_CEILING
    );
    // An expensive one waits four times its cost, past the ladder's ceiling.
    assert_eq!(
        watcher_recovery_spacing(1, Duration::from_secs(2_410)),
        Duration::from_secs(9_640)
    );
    assert_eq!(
        watcher_recovery_spacing(40, Duration::from_secs(2_410)),
        Duration::from_secs(9_640)
    );
}

/// A refusal counts as a repeat only for the same cause over a store that did
/// not move, which is the only case in which a third attempt is certain to
/// derive the same tree and refuse it the same way.
#[test]
fn a_recovery_failure_repeats_only_for_the_same_cause_over_an_unmoved_store() {
    let marks = |generation: u64| RepositoryMarks {
        authority_generation: generation,
        graph_version: generation,
    };
    let failure = |cause: Option<&str>, before: u64, after: u64| RecoveryFailure {
        cause: cause.map(str::to_string),
        marks_before: marks(before),
        marks_after: marks(after),
    };
    let first = failure(Some("current source is incomplete"), 1, 1);

    assert!(failure(Some("current source is incomplete"), 1, 1).repeats(&first));
    // Another cause is not the same refusal.
    assert!(!failure(Some("sensitive content"), 1, 1).repeats(&first));
    // A failure that reports no cause never counts.
    assert!(!failure(None, 1, 1).repeats(&failure(None, 1, 1)));
    // The store moved between the two attempts, or during the second.
    assert!(!failure(Some("current source is incomplete"), 2, 2).repeats(&first));
    assert!(!failure(Some("current source is incomplete"), 1, 2).repeats(&first));
}
