// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

// A watcher whose backend delivers nothing, the way a saturated macOS FSEvents
// stream behaves. Included into `loop_runner::tests`.

/// Short enough that a silent backend's loss is found well inside a second.
#[cfg(unix)]
const SILENT_CANARY: DeliveryCanary = DeliveryCanary {
    period: Duration::from_millis(100),
    bound: Duration::from_millis(300),
};

#[cfg(unix)]
fn live_graph_holds(state: &DaemonState, rel_path: &str, name: &str) -> bool {
    state
        .graph
        .query_entities(&EntityFilter {
            file_path: Some(FilePathId::new(rel_path)),
            ..Default::default()
        })
        .unwrap()
        .iter()
        .any(|entity| entity.name == name)
}

/// A daemon whose watcher never acknowledges its readiness probe arms and
/// serves anyway, and an edit no event ever reports still reaches the graph.
///
/// On a saturated macOS host FSEvents dropped more than nine in ten events for
/// seconds at a time and flagged none of it. A daemon there refused to start
/// three times in a row with "watcher did not acknowledge its readiness probe",
/// each time after its startup phases had finished, and a daemon that did start
/// never admitted the edits the host dropped, because nothing told it they
/// happened. Here the backend delivers nothing at all, so the readiness probe,
/// every canary and the edit are all dropped exactly that way. The edit can
/// reach the graph only through the loop noticing its own blindness and
/// recovering with a complete exact-tree admission.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_watcher_the_host_never_answers_arms_and_recovers_an_edit_no_event_reports() {
    // It needs the default central store. A process several tests share pins
    // in-tree stores once any fixture template is built there
    // (test_fixture_cache::pin_in_tree_stores), so this test runs in a process
    // of its own and states the placement it needs.
    if !kin_core::test_env::alone_in_process() {
        return;
    }
    let _central = kin_core::test_env::EnvVarGuard::unset(kin_core::layout::KIN_STORE_LAYOUT_ENV);
    let repo = tempfile::tempdir().unwrap();
    let state = open_test_state(&repo);
    admit_and_derive(&state, "pkg/quiet.py", "def before():\n    return 1\n");
    let working_dir = state.layout.working_dir().to_path_buf();
    assert!(
        !working_dir.join(".kin").exists(),
        "the default central store must exercise probes outside the working tree"
    );
    use_silent_backend_for_test(&working_dir, Duration::from_millis(300));
    use_delivery_canary_for_test(&working_dir, Some(SILENT_CANARY));

    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let (armed_tx, armed_rx) = tokio::sync::oneshot::channel();
    let mut runner = tokio::spawn(run_loop_armed(
        Arc::clone(&state),
        LoopConfig {
            poll_interval_ms: 20,
            batch_size: 64,
        },
        cancel_rx,
        Some(WatchArmed::new(armed_tx)),
    ));
    let arming = crate::daemon::await_watch_armed(armed_rx, WATCH_ARMING_BOUND).await;

    // The unanswered probe is recorded durably and a complete admission covers
    // it, rather than the daemon refusing to serve.
    let readiness_recovered = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let crate::watcher_loss::WatcherLossRead::Recorded(record) =
                crate::watcher_loss::read(&state.layout)
            {
                if record.recovered_through >= 1 {
                    return record;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;

    // An edit the host never reports.
    std::fs::write(
        working_dir.join("pkg/quiet.py"),
        "def after():\n    return 2\n",
    )
    .unwrap();
    let admitted = tokio::time::timeout(Duration::from_secs(30), async {
        while !live_graph_holds(&state, "pkg/quiet.py", "after") {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .is_ok();
    let loss = crate::watcher_loss::read(&state.layout);
    let delivered = host_notifications_for_test(&working_dir, "pkg/quiet.py");
    cancel_tx.send(true).ok();
    let joined = tokio::time::timeout(crate::daemon::TASK_DRAIN_BOUND, &mut runner).await;
    if joined.is_err() {
        runner.abort();
        let _ = runner.await;
    }

    assert!(
        !working_dir.join(".kin").exists(),
        "delivery sampling must leave the working tree free of store controls"
    );
    assert_eq!(
        arming,
        crate::daemon::WatchArming::Armed,
        "a readiness probe the host never answers must not stop the daemon from serving"
    );
    let readiness_recovered = readiness_recovered
        .expect("the unanswered readiness probe is recorded as a loss and recovered while serving");
    assert!(
        readiness_recovered.generation >= 1,
        "{readiness_recovered:?}"
    );
    assert_eq!(
        delivered, 0,
        "the fixture's backend delivered nothing, so no event carried the edit"
    );
    assert!(
        admitted,
        "an edit no event reported must still reach the graph through the loop's own loss recovery"
    );
    match loss {
        crate::watcher_loss::WatcherLossRead::Recorded(record) => {
            assert!(
                record.generation >= 2,
                "the canaries lost after readiness are recorded as losses too: {record:?}"
            );
            assert!(
                record.reason.as_deref().is_some_and(|reason| {
                    reason.contains("delivery canary") || reason.contains("readiness probe")
                }),
                "the record names the probe that did not come back: {record:?}"
            );
        }
        other => panic!("a silent backend's losses are recorded durably: {other:?}"),
    }
    joined
        .expect("the loop stops")
        .expect("the loop task must not panic")
        .expect("the loop exits cleanly");
}

/// Each complete admission that recovers a watcher loss, with no canary
/// acknowledged since the last, waits longer before the next may start, up to
/// a minute.
#[test]
fn loss_recoveries_without_an_acknowledged_canary_are_spaced_on_a_ladder() {
    assert_eq!(delivery_recovery_spacing(1), Duration::from_secs(2));
    assert_eq!(delivery_recovery_spacing(2), Duration::from_secs(4));
    assert_eq!(delivery_recovery_spacing(3), Duration::from_secs(8));
    assert_eq!(delivery_recovery_spacing(5), Duration::from_secs(32));
    assert_eq!(
        delivery_recovery_spacing(6),
        DELIVERY_RECOVERY_SPACING_CEILING
    );
    assert_eq!(
        delivery_recovery_spacing(u64::MAX),
        DELIVERY_RECOVERY_SPACING_CEILING
    );
}
