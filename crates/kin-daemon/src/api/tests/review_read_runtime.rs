// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

// A review read and the runtime worker it runs on. Included into
// `api::tests`.

/// Clears the review read pause however the case ends, so a failing assertion
/// cannot leave a pause armed for an address a later allocation might reuse.
struct ReviewReadPauseGuard;

impl Drop for ReviewReadPauseGuard {
    fn drop(&mut self) {
        *REVIEW_READ_PAUSE_FOR_TEST
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }
}

/// A review read does not hold the runtime worker it runs on, so the daemon
/// keeps answering while a long guide builds.
///
/// A multi-thread runtime with one worker, a review read held open for two
/// seconds through the test pause, and a health request sent while it is
/// held. The request can only be answered on a worker: if the read kept its
/// worker, the request waits the pause out; handed off, it is answered at
/// once. Measured before this on zod, whose guide of one committed change
/// built for 50 s: every request to its daemon, health included, waited the
/// build out, and the entity page's read of references ran out its 20 s and
/// said Kin could not read them, while the repository was reading its older
/// history and drew the blame.
///
/// Breaking it: run the review build inline on its worker again, and the
/// health request waits for the whole pause.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn a_review_read_does_not_hold_the_runtime_worker_it_runs_on() {
    let state = test_state();
    state
        .is_initialized
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let _guard = ReviewReadPauseGuard;
    *REVIEW_READ_PAUSE_FOR_TEST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        Some((Arc::as_ptr(&state) as usize, Duration::from_secs(2)));
    let reviewing = {
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            review_call(&state, serde_json::json!({ "op": "list", "state": null })).await
        })
    };

    // The test's own thread is blocked on purpose: it is not a runtime worker,
    // and a runtime timer would need the very worker this case is about.
    std::thread::sleep(Duration::from_millis(400));
    let asked = std::time::Instant::now();
    let health = {
        let app = router(Arc::clone(&state));
        tokio::spawn(async move {
            let response = app
                .oneshot(Request::get("/health").body(Body::empty()).unwrap())
                .await
                .unwrap();
            (response.status(), std::time::Instant::now())
        })
    };
    let (status, answered) = health.await.unwrap();
    let waited = answered.saturating_duration_since(asked);
    let listed = reviewing.await.unwrap();

    assert!(
        listed.text.contains("0 review(s)") || listed.text.contains("No reviews"),
        "the held read must still answer, or this case measures a failure: {}",
        listed.text
    );
    assert_eq!(status, StatusCode::OK);
    assert!(
        waited < Duration::from_millis(1000),
        "a health request sent while a review read was built waited {waited:?} for an \
         answer, so the review read is holding the runtime worker it runs on"
    );
}

/// Graph-authority writers coming and going until stopped, each held for a
/// moment, the way passes over the working copy come while a repository's
/// watcher drops events: each takes authority and, unless `writes_truth`,
/// finds nothing to change.
struct WriterChurn {
    stop: Arc<std::sync::atomic::AtomicBool>,
    handle: tokio::task::JoinHandle<u64>,
}

impl WriterChurn {
    fn start(state: &Arc<DaemonState>, writes_truth: bool) -> Self {
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let handle = {
            let state = Arc::clone(state);
            let stop = Arc::clone(&stop);
            tokio::spawn(async move {
                let mut writers = 0_u64;
                while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                    {
                        let _writer = state.begin_graph_authority_mutation();
                        if writes_truth {
                            state
                                .graph
                                .upsert_entity(&test_entity(
                                    &format!("churned_{writers}"),
                                    "churn.py",
                                ))
                                .unwrap();
                        }
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                    writers += 1;
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                writers
            })
        };
        Self { stop, handle }
    }

    async fn stop(self) -> u64 {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        self.handle.await.unwrap()
    }
}

/// POST a guide of the committed head and return the status and body.
async fn committed_head_guide(state: &Arc<DaemonState>) -> (StatusCode, String) {
    let response = router(Arc::clone(state))
        .oneshot(
            Request::post("/review")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"op": "guide", "json": true, "working": false})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 16 * 1024 * 1024)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&body).into_owned())
}

/// A guide of a committed change is served while writers that change nothing
/// come and go throughout its build.
///
/// The change, its history and the graph at that change never move, and a
/// writer that changes nothing leaves the graph's truth where it was. Measured
/// on zod while its watcher dropped events: a pass over the working copy took
/// graph authority every six to eight seconds and found nothing, a guide of
/// one commit took 20 s to build and was spanned by a pass on most attempts,
/// and the review page's guide did not load in four minutes. Here every
/// attempt is held open for 300 ms through the test pause while writers come
/// and go every 25 ms, under a 1.5 s limit.
///
/// Breaking it: serve a guide only from an attempt no writer moved the
/// authority epoch across, as before, and it refuses with 503 and "a
/// graph-authority writer was mutating the graph", the refusal the page met.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_committed_change_guide_answers_while_writers_that_change_nothing_come_and_go() {
    let (_repo, state) = refs_source_observation_fixture().await;
    state.set_xref_writer_drain_ceiling_for_test(Duration::from_millis(1_500));
    let _guard = ReviewReadPauseGuard;
    *REVIEW_READ_PAUSE_FOR_TEST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        Some((Arc::as_ptr(&state) as usize, Duration::from_millis(300)));
    let truth = state.graph.truth_epoch();

    let churn = WriterChurn::start(&state, false);
    let (status, body) = committed_head_guide(&state).await;
    let writers = churn.stop().await;

    assert!(writers >= 5, "writers must span the read: {writers}");
    assert_eq!(state.graph.truth_epoch(), truth, "no writer changed truth");
    assert_eq!(status, StatusCode::OK, "{body}");
    let answer: kin_cli::commands::review::ReviewResponse = serde_json::from_str(&body).unwrap();
    let guide: serde_json::Value = serde_json::from_str(&answer.json.unwrap()).unwrap();
    assert_eq!(guide["kind"], "change", "{guide}");
}

/// The guard still holds for a guide whose graph truth keeps moving: no
/// attempt reads a graph that held still, and the read refuses at its limit
/// with the in-flight write window named, never an answer read across writes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_committed_change_guide_is_refused_while_writes_to_truth_keep_landing() {
    let (_repo, state) = refs_source_observation_fixture().await;
    state.set_xref_writer_drain_ceiling_for_test(Duration::from_millis(1_500));
    let _guard = ReviewReadPauseGuard;
    *REVIEW_READ_PAUSE_FOR_TEST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        Some((Arc::as_ptr(&state) as usize, Duration::from_millis(300)));

    let churn = WriterChurn::start(&state, true);
    let (status, body) = committed_head_guide(&state).await;
    churn.stop().await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert!(
        body.contains(kin_mcp::negative::UNANSWERED_READ_PREFIX),
        "the refusal names the write window: {body}"
    );
}

/// A guide of a committed change is built once and kept while what it read
/// holds still. Measured under load, axum's guide of one fresh commit took 56
/// to 66 s and zod's 30 s; the page gives a read a minute, so every retry of
/// axum's started again from nothing, and Home and the review page each built
/// zod's. The second read here is answered from the guide the first built.
///
/// Breaking it: build every guide as it is asked, and the second read builds
/// again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_committed_change_guide_is_built_once_and_read_again_from_what_was_kept() {
    let (_repo, state) = refs_source_observation_fixture().await;
    let (status, first) = committed_head_guide(&state).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(state.committed_guides.builds(), 1);

    let asked = std::time::Instant::now();
    let (status, second) = committed_head_guide(&state).await;
    assert_eq!(status, StatusCode::OK, "{second}");
    assert_eq!(
        state.committed_guides.builds(),
        1,
        "the second read built the guide again"
    );
    assert_eq!(state.committed_guides.kept_served(), 1);
    assert_eq!(second, first, "the kept guide is the guide that was built");
    assert!(asked.elapsed() < Duration::from_secs(5));
}

/// A read of a change whose guide is being built joins that build, and a read
/// whose caller stops waiting leaves the build running for the next one: the
/// page's retry after its minute is answered by the build its first attempt
/// started, and Home and the review page asking together cost one build.
///
/// Breaking it: build on the request's own task, or start a build for every
/// read, and the count of builds is two or three.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_committed_change_guide_read_while_it_builds_joins_the_build() {
    let (_repo, state) = refs_source_observation_fixture().await;
    let _guard = ReviewReadPauseGuard;
    *REVIEW_READ_PAUSE_FOR_TEST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        Some((Arc::as_ptr(&state) as usize, Duration::from_millis(800)));

    // The first caller gives up long before the build ends.
    let abandoned = tokio::time::timeout(
        Duration::from_millis(150),
        committed_head_guide(&state),
    )
    .await;
    assert!(abandoned.is_err(), "the held build answered at once");
    let together = {
        let one = Arc::clone(&state);
        let two = Arc::clone(&state);
        tokio::join!(
            tokio::spawn(async move { committed_head_guide(&one).await }),
            tokio::spawn(async move { committed_head_guide(&two).await }),
        )
    };
    let (one, two) = (together.0.unwrap(), together.1.unwrap());
    assert_eq!(one.0, StatusCode::OK, "{}", one.1);
    assert_eq!(two.0, StatusCode::OK, "{}", two.1);
    assert_eq!(one.1, two.1);
    assert_eq!(
        state.committed_guides.builds(),
        1,
        "the abandoned read's build answered both reads after it"
    );
}

/// A guide is never served for a repository that has moved: a write to the
/// graph's truth since it was built builds it again, and the guide built then
/// is the one kept.
///
/// Breaking it: keep a guide by its change alone, and the read after the
/// write is answered from before it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_committed_change_guide_is_built_again_once_the_graph_moves() {
    let (_repo, state) = refs_source_observation_fixture().await;
    let (status, body) = committed_head_guide(&state).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(state.committed_guides.builds(), 1);

    let truth = state.graph.truth_epoch();
    {
        let _writer = state.begin_graph_authority_mutation();
        state
            .graph
            .upsert_entity(&test_entity("written_since", "since.py"))
            .unwrap();
    }
    assert!(state.graph.truth_epoch() > truth);
    let (status, body) = committed_head_guide(&state).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(state.committed_guides.builds(), 2);
    let (status, _) = committed_head_guide(&state).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        state.committed_guides.builds(),
        2,
        "the guide built after the write is kept"
    );
}
