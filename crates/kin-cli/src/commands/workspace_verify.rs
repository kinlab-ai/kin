// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Running the tests that reach a piece of work, from a repository's page.
//!
//! The page asks, and this server answers at once with an operation it can
//! follow. The repository's daemon does the work, as it does for `kin verify
//! run`: it plans the tests that reach the work, runs them with the test
//! command the repository's own manifests declare, in its working tree, and
//! records the run against the fingerprints of the entities it ran against.
//! A guide of uncommitted work then reads each test's latest result from that
//! record, so the page and the terminal cannot disagree about a test.
//!
//! This server keeps only the operations it started, while it runs. The
//! results themselves are the repository's.

use std::sync::{Arc, Mutex};

use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use super::review_ui::{self, UiState};
use super::verify::{WorkVerifyRequest, WorkVerifyResponse, WorkVerifyScope};
use crate::daemon_client::DaemonClient;

/// The operations remembered. The oldest finished one is dropped first.
const RUNS_KEPT: usize = 20;
/// The most entities one request may name.
const ENTITIES_PER_REQUEST: usize = 500;

pub(super) fn routes() -> Router<Arc<UiState>> {
    Router::new()
        .route("/api/workspace/verify", post(start))
        .route("/api/workspace/operations/{id}", get(operation))
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn locked<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn refused(status: StatusCode, sentence: impl Into<String>) -> Response {
    (status, Json(json!({ "error": sentence.into() }))).into_response()
}

/// The test runs this page started, oldest first, and what the repository's
/// daemon last said about how this repository runs its tests.
#[derive(Default)]
pub(super) struct VerifyRuns {
    runs: Mutex<Vec<Arc<VerifyRun>>>,
    test_command: Mutex<TestCommandReading>,
}

/// What the daemon last said about how this repository runs its tests: why it
/// cannot, or nothing when it can, and when; and whether a read is running.
#[derive(Default)]
struct TestCommandReading {
    known: Option<(std::time::Instant, Option<String>)>,
    asking: bool,
}

/// How long the daemon's answer on how this repository runs its tests is
/// taken as current before a session reads it again.
const TEST_COMMAND_KEPT: std::time::Duration = std::time::Duration::from_secs(60);
/// How long that read is given. Nothing waits for it.
const TEST_COMMAND_BUDGET: std::time::Duration = std::time::Duration::from_secs(60);

/// Why this repository's tests cannot be run from here, as the daemon last
/// said: `Some(None)` when they can, and `None` when it has not said yet. A
/// read for a newer answer starts on its own when this one is old, and the
/// caller never waits for it.
///
/// Measured on a busy zod, the daemon took more than 30 s to read its test
/// command from its manifests while it wrote, and a session that waited for
/// it, within the five seconds it gives the daemon, reported the whole runtime
/// as unobserved and drew nothing for five seconds.
pub(super) fn test_command_known(
    state: &Arc<UiState>,
    client: DaemonClient,
) -> Option<Option<String>> {
    let mut reading = locked(&state.verify_runs.test_command);
    let known = reading.known.clone();
    let fresh = known
        .as_ref()
        .is_some_and(|(read_at, _)| read_at.elapsed() < TEST_COMMAND_KEPT);
    if !fresh && !reading.asking {
        reading.asking = true;
        let state = Arc::clone(state);
        tokio::spawn(async move {
            let answer = tokio::time::timeout(
                TEST_COMMAND_BUDGET,
                client.verify_work(&WorkVerifyRequest::TestCommand),
            )
            .await;
            let mut reading = locked(&state.verify_runs.test_command);
            reading.asking = false;
            if let Ok(Ok(answer)) = answer {
                let unavailable = if answer.runnable {
                    None
                } else {
                    answer.unavailable
                };
                reading.known = Some((std::time::Instant::now(), unavailable));
            }
        });
    }
    known.map(|(_, unavailable)| unavailable)
}

/// How far a run has come, as a code. The page words each stage itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    /// Reading the latest edits into the repository, so the plan sees them.
    Reading,
    /// Planning the tests that reach the work.
    Planning,
    /// The repository's test command is running.
    Running,
    /// The run is recorded, or nothing was run, and the answer is final.
    Finishing,
}

impl Stage {
    fn word(self) -> &'static str {
        match self {
            Self::Reading => "reading",
            Self::Planning => "planning",
            Self::Running => "running",
            Self::Finishing => "finishing",
        }
    }
}

enum Outcome {
    /// The daemon answered: a recorded run, or a plan with no test in it.
    Answered(WorkVerifyResponse),
    Failed(String),
}

struct Progress {
    stage: Stage,
    phase: String,
    /// The plan, once it is read.
    planned: Option<WorkVerifyResponse>,
    finished_at: Option<String>,
    outcome: Option<Outcome>,
}

/// One request to run the tests that reach a piece of work, and how far it
/// has come.
struct VerifyRun {
    id: kin_model::OperationId,
    scope: WorkVerifyScope,
    started_at: String,
    status_url: String,
    progress: Mutex<Progress>,
}

impl VerifyRun {
    fn running(&self) -> bool {
        locked(&self.progress).outcome.is_none()
    }

    fn enter(&self, stage: Stage, phase: impl Into<String>) {
        let mut progress = locked(&self.progress);
        progress.stage = stage;
        progress.phase = phase.into();
    }

    fn finish(&self, outcome: Outcome) {
        let mut progress = locked(&self.progress);
        progress.stage = Stage::Finishing;
        progress.finished_at = Some(now());
        progress.phase = match &outcome {
            Outcome::Answered(answer) => match &answer.run {
                Some(run) if run.outcome == "passed" => "The test command passed.".to_string(),
                // It never reached a test, so it is not said as a failure.
                Some(run) if run.outcome == "could_not_run" => {
                    "The test command could not run.".to_string()
                }
                Some(_) => "The test command failed.".to_string(),
                None => no_tests_sentence(answer),
            },
            Outcome::Failed(error) => error.clone(),
        };
        progress.outcome = Some(outcome);
    }

    fn json(&self) -> Value {
        let progress = locked(&self.progress);
        let mut body = json!({
            "operation_id": self.id,
            "action": "verify",
            "state": match progress.outcome {
                None => "running",
                Some(Outcome::Answered(_)) => "done",
                Some(Outcome::Failed(_)) => "failed",
            },
            "stage": progress.stage.word(),
            "phase": progress.phase,
            "scope": {
                "entities": self.scope.entities,
                "change": self.scope.change,
            },
            "started_at": self.started_at,
            "finished_at": progress.finished_at,
            "status_url": self.status_url,
        });
        if let Some(planned) = &progress.planned {
            body["progress"] = json!({
                "tests": planned.tests.len(),
                "entities": planned.entities.len(),
                "reach": planned.reach,
            });
        }
        match &progress.outcome {
            Some(Outcome::Answered(answer)) => {
                body["outcome"] = json!(match &answer.run {
                    Some(run) => run.outcome.as_str(),
                    None => "no_tests",
                });
                body["result"] = json!({
                    "run": answer.run,
                    "reach": answer.reach,
                    "entities": answer.entities,
                    "tests": answer.tests,
                });
            }
            Some(Outcome::Failed(error)) => body["error"] = json!(error),
            None => {}
        }
        body
    }
}

enum Reservation {
    Started,
    Known(Arc<VerifyRun>),
    Busy(Arc<VerifyRun>),
}

/// What a finished request that ran nothing says.
fn no_tests_sentence(answer: &WorkVerifyResponse) -> String {
    if answer.entities.is_empty() {
        "No entity is changed, so there are no tests to run.".to_string()
    } else {
        format!(
            "No test reaches this work within {} recorded hops.",
            super::verify::WORK_TEST_REACH_DEPTH
        )
    }
}

impl VerifyRuns {
    fn find(&self, id: kin_model::OperationId) -> Option<Arc<VerifyRun>> {
        locked(&self.runs).iter().find(|run| run.id == id).cloned()
    }

    fn running(&self) -> Option<Arc<VerifyRun>> {
        locked(&self.runs).iter().find(|run| run.running()).cloned()
    }

    /// Reserve the working tree and operation id together, after any async
    /// preflight. A simultaneous request cannot launch a second suite.
    fn reserve(&self, run: Arc<VerifyRun>) -> Reservation {
        let mut runs = locked(&self.runs);
        if let Some(known) = runs.iter().find(|known| known.id == run.id) {
            return Reservation::Known(known.clone());
        }
        if let Some(running) = runs.iter().find(|known| known.running()) {
            return Reservation::Busy(running.clone());
        }
        runs.push(run);
        Self::trim(&mut runs);
        Reservation::Started
    }

    fn trim(runs: &mut Vec<Arc<VerifyRun>>) {
        while runs.len() > RUNS_KEPT {
            let Some(oldest) = runs.iter().position(|run| !run.running()) else {
                break;
            };
            runs.remove(oldest);
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct VerifyBody {
    #[serde(default)]
    entities: Option<Vec<String>>,
    #[serde(default)]
    change: Option<String>,
    operation_id: kin_model::OperationId,
}

/// The work a request names, checked before anything is asked of the daemon.
fn scope_of(body: &VerifyBody) -> Result<WorkVerifyScope, String> {
    if body.operation_id.0.is_nil() {
        return Err("A test run needs a nonempty operation id.".to_string());
    }
    let entities = body.entities.clone().unwrap_or_default();
    let change = body
        .change
        .as_deref()
        .map(str::trim)
        .filter(|change| !change.is_empty())
        .map(str::to_string);
    if !entities.is_empty() && change.is_some() {
        return Err("Name entities or a change, not both.".to_string());
    }
    if entities.len() > ENTITIES_PER_REQUEST {
        return Err(format!(
            "One test run names at most {ENTITIES_PER_REQUEST} entities."
        ));
    }
    let mut ids = Vec::with_capacity(entities.len());
    for raw in entities {
        let id = raw.trim();
        if uuid::Uuid::parse_str(id).is_err() {
            return Err(format!("'{id}' is not an entity id."));
        }
        ids.push(id.to_string());
    }
    if let Some(change) = &change {
        if change.len() != 64 || !change.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(format!("'{change}' is not a full change id."));
        }
    }
    Ok(WorkVerifyScope {
        entities: ids,
        change,
        depth: super::verify::WORK_TEST_REACH_DEPTH - 1,
    })
}

/// `POST /api/workspace/verify`: start running the tests that reach a piece
/// of work, or say why none will run.
async fn start(
    State(state): State<Arc<UiState>>,
    headers: HeaderMap,
    Json(body): Json<VerifyBody>,
) -> Response {
    if !review_ui::may_write(&headers, &state) {
        return review_ui::write_refused();
    }
    // A request sent again is answered with the run it started.
    if let Some(known) = state.verify_runs.find(body.operation_id) {
        return Json(known.json()).into_response();
    }
    let scope = match scope_of(&body) {
        Ok(scope) => scope,
        Err(sentence) => return refused(StatusCode::BAD_REQUEST, sentence),
    };
    // One run at a time in one working tree: two suites at once would read
    // and write each other's files.
    if let Some(running) = state.verify_runs.running() {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "Kin is already running tests for this repository.",
                "operation": running.json(),
            })),
        )
            .into_response();
    }
    let client = match review_ui::daemon(&state).await {
        Ok(client) => client,
        Err(error) => return review_ui::refusal(StatusCode::BAD_GATEWAY, &error),
    };
    // Whether the repository has a command to run is known before anything
    // starts, so the answer can say so at once.
    match client.verify_work(&WorkVerifyRequest::TestCommand).await {
        Ok(answer) if !answer.runnable => {
            return Json(json!({
                "operation_id": body.operation_id,
                "action": "verify",
                "state": "unavailable",
                "reason": answer.unavailable.unwrap_or_else(|| {
                    super::verify::NO_TEST_COMMAND.to_string()
                }),
            }))
            .into_response()
        }
        Ok(_) => {}
        Err(error) => return review_ui::refusal(StatusCode::BAD_GATEWAY, &error),
    }
    let run = Arc::new(VerifyRun {
        id: body.operation_id,
        scope,
        started_at: now(),
        status_url: format!(
            "{}/api/workspace/operations/{}",
            state.base_path, body.operation_id
        ),
        progress: Mutex::new(Progress {
            stage: Stage::Reading,
            phase: "Reading the latest edits.".to_string(),
            planned: None,
            finished_at: None,
            outcome: None,
        }),
    });
    match state.verify_runs.reserve(run.clone()) {
        Reservation::Known(known) => return Json(known.json()).into_response(),
        Reservation::Busy(running) => {
            return (
                StatusCode::CONFLICT,
                Json(json!({
                    "error": "Kin is already running tests for this repository.",
                    "operation": running.json(),
                })),
            )
                .into_response();
        }
        Reservation::Started => {}
    }
    // Run to its end whatever becomes of this request: the page follows it
    // by its operation, and a closed page must not stop a recorded run.
    tokio::spawn(drive(client, run.clone()));
    (StatusCode::ACCEPTED, Json(run.json())).into_response()
}

/// Take a run through its stages: read the latest edits, plan, run and
/// record, each by the repository's daemon.
async fn drive(client: DaemonClient, run: Arc<VerifyRun>) {
    // The runner reads the working tree, so its graph fingerprints establish
    // what ran only after a complete admission of those bytes.
    if let Some(cause) = review_ui::admit_working_for_ui(&client).await {
        let sentence = unread_edits(&client, &cause).await;
        return run.finish(Outcome::Failed(sentence));
    }
    run.enter(Stage::Planning, "Planning the tests that reach this work.");
    let planned = match client
        .verify_work(&WorkVerifyRequest::Plan(run.scope.clone()))
        .await
    {
        Ok(planned) => planned,
        Err(error) => return run.finish(Outcome::Failed(failure(&error))),
    };
    if !planned.runnable {
        let sentence = planned
            .unavailable
            .clone()
            .unwrap_or_else(|| super::verify::NO_TEST_COMMAND.to_string());
        return run.finish(Outcome::Failed(sentence));
    }
    let tests = planned.tests.len();
    {
        let mut progress = locked(&run.progress);
        progress.planned = Some(planned.clone());
    }
    if tests == 0 {
        return run.finish(Outcome::Answered(planned));
    }
    run.enter(
        Stage::Running,
        match tests {
            1 => "Running the test command for 1 planned test.".to_string(),
            tests => format!("Running the test command for {tests} planned tests."),
        },
    );
    match client
        .verify_work(&WorkVerifyRequest::Run(run.scope.clone()))
        .await
    {
        Ok(answer) => run.finish(Outcome::Answered(answer)),
        Err(error) => run.finish(Outcome::Failed(failure(&error))),
    }
}

/// What a run says when the latest edits could not be read: that it ran
/// nothing, then why. An edit that removed an entity something still relates
/// to is said with the entity's name, read from the graph that still holds it.
async fn unread_edits(client: &DaemonClient, cause: &str) -> String {
    let removed = match stranded_entity(cause) {
        Some(id) => removed_entity_words(client, &id).await,
        None => None,
    };
    unread_edits_sentence(cause, removed.as_deref())
}

/// The lead every such answer starts with, so a reader and the page can tell
/// at once that nothing ran.
const UNREAD_EDITS: &str = "Kin could not read the latest edits, so it did not run the tests.";

/// The admission layers a cause is wrapped in, which say where it failed
/// inside Kin rather than why.
const CAUSE_LAYERS: [&str; 7] = [
    "complete exact-tree admission failed: ",
    "graph error: ",
    "storage error: ",
    "reconcile error: ",
    "index error: ",
    "core error: ",
    "io error: ",
];

fn unread_edits_sentence(cause: &str, removed: Option<&str>) -> String {
    let reason = if stranded_entity(cause).is_some() {
        match removed {
            Some(entity) => format!(
                "The edits removed {entity}, and the graph still holds a relationship that \
                 names it."
            ),
            None => "The edits removed an entity, and the graph still holds a relationship \
                     that names it."
                .to_string(),
        }
    } else {
        let mut reason = cause.trim();
        while let Some(layer) = CAUSE_LAYERS.iter().find(|layer| {
            reason
                .get(..layer.len())
                .is_some_and(|head| head.eq_ignore_ascii_case(layer))
        }) {
            reason = reason[layer.len()..].trim_start();
        }
        let mut reason = reason.to_string();
        if let Some(first) = reason.get(..1) {
            let upper = first.to_uppercase();
            reason.replace_range(..1, &upper);
        }
        if !reason.is_empty() && !reason.ends_with(['.', '!', '?']) {
            reason.push('.');
        }
        reason
    };
    if reason.is_empty() {
        UNREAD_EDITS.to_string()
    } else {
        format!("{UNREAD_EDITS} {reason}")
    }
}

/// The entity a refused transition would have left a relationship naming,
/// from the graph's refusal: `... unadmitted destination endpoint entity:<id>`.
fn stranded_entity(cause: &str) -> Option<String> {
    let (_, rest) = cause.split_once("endpoint entity:")?;
    let id = rest.get(..36)?;
    uuid::Uuid::parse_str(id).ok()?;
    Some(id.to_string())
}

/// The removed entity as a reader knows it, such as the function `negate`,
/// read while the graph that refused the edit still holds it. `None` when it
/// cannot be read in time.
async fn removed_entity_words(client: &DaemonClient, id: &str) -> Option<String> {
    let answer = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        client.workspace_entity_owners(id),
    )
    .await
    .ok()?
    .ok()?;
    let name = answer["name"].as_str()?.trim();
    if name.is_empty() {
        return None;
    }
    let kind = super::workspace_explore::kind_label(answer["kind"].as_str().unwrap_or_default())
        .replace('_', " ");
    Some(if kind.is_empty() {
        format!("`{name}`")
    } else {
        format!("the {kind} `{name}`")
    })
}

/// A failure in Kin's own words. A refusal from the daemon is its sentence,
/// without the name of the route that carried it.
fn failure(error: &anyhow::Error) -> String {
    let text = format!("{error:#}");
    let sentence = match text.split_once("kin verify work refused (HTTP ") {
        Some((_, rest)) => rest
            .split_once("): ")
            .map_or(rest, |(_, sentence)| sentence),
        None => &text,
    };
    format!("Kin could not run the tests: {}", sentence.trim())
}

/// `GET /api/workspace/operations/{id}`: how far a test run has come.
async fn operation(State(state): State<Arc<UiState>>, UrlPath(id): UrlPath<String>) -> Response {
    let found = uuid::Uuid::parse_str(&id)
        .ok()
        .and_then(|id| state.verify_runs.find(kin_model::OperationId(id)));
    match found {
        Some(run) => Json(run.json()).into_response(),
        None => refused(
            StatusCode::NOT_FOUND,
            "This page has no such operation. It keeps them only while it runs.",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(entities: Option<Vec<&str>>, change: Option<&str>) -> VerifyBody {
        VerifyBody {
            entities: entities.map(|ids| ids.into_iter().map(str::to_string).collect()),
            change: change.map(str::to_string),
            operation_id: kin_model::OperationId(uuid::Uuid::new_v4()),
        }
    }

    #[test]
    fn a_request_names_entities_or_a_change_or_the_whole_work() {
        let entity = "196d8edb-c20c-4d44-8f59-d0e96e6f74c1";
        let scope = scope_of(&body(Some(vec![entity]), None)).unwrap();
        assert_eq!(scope.entities, [entity]);
        assert_eq!(scope.change, None);

        let whole = scope_of(&body(None, None)).unwrap();
        assert!(whole.entities.is_empty() && whole.change.is_none());

        let change = "a".repeat(64);
        assert_eq!(
            scope_of(&body(None, Some(&change))).unwrap().change,
            Some(change.clone())
        );

        assert_eq!(
            scope_of(&body(Some(vec![entity]), Some(&change))).unwrap_err(),
            "Name entities or a change, not both."
        );
        assert_eq!(
            scope_of(&body(Some(vec!["src/math.js"]), None)).unwrap_err(),
            "'src/math.js' is not an entity id."
        );
        assert!(scope_of(&body(None, Some("abc123"))).is_err());
        let mut nil = body(None, None);
        nil.operation_id = kin_model::OperationId(uuid::Uuid::nil());
        assert!(scope_of(&nil).is_err());
    }

    fn a_run(stage: Stage) -> Arc<VerifyRun> {
        Arc::new(VerifyRun {
            id: kin_model::OperationId(uuid::Uuid::new_v4()),
            scope: WorkVerifyScope::default(),
            started_at: now(),
            status_url: "/api/workspace/operations/x".to_string(),
            progress: Mutex::new(Progress {
                stage,
                phase: String::new(),
                planned: None,
                finished_at: None,
                outcome: None,
            }),
        })
    }

    #[test]
    fn a_run_reports_its_stage_until_it_is_done_and_then_its_outcome() {
        let run = a_run(Stage::Reading);
        assert_eq!(run.json()["state"], "running");
        assert_eq!(run.json()["stage"], "reading");

        let planned = WorkVerifyResponse {
            runnable: true,
            reach: Some("whole".to_string()),
            entities: vec![super::super::verify::WorkEntity {
                id: "e".to_string(),
                name: "add".to_string(),
                kind: "function".to_string(),
            }],
            tests: vec![super::super::verify::WorkTest {
                test_id: "t".to_string(),
                name: "checkAdd".to_string(),
                entity_id: Some("c".to_string()),
            }],
            ..WorkVerifyResponse::default()
        };
        locked(&run.progress).planned = Some(planned.clone());
        run.enter(Stage::Running, "Running 1 test.");
        let reading = run.json();
        assert_eq!(reading["stage"], "running");
        assert_eq!(reading["progress"]["tests"], 1);
        assert_eq!(reading["progress"]["reach"], "whole");

        let mut answered = planned;
        answered.run = Some(super::super::verify::WorkRun {
            run_id: "r".to_string(),
            outcome: "failed".to_string(),
            duration_ms: 12,
            exit_code: Some(1),
            finished_at: now(),
            reason: None,
            output_tail: Some("not ok 1 - checkAdd".to_string()),
        });
        run.finish(Outcome::Answered(answered));
        let done = run.json();
        assert_eq!(done["state"], "done");
        assert_eq!(done["stage"], "finishing");
        assert_eq!(done["outcome"], "failed");
        assert_eq!(done["phase"], "The test command failed.");
        assert_eq!(done["result"]["run"]["exit_code"], 1);
        assert_eq!(done["result"]["tests"][0]["name"], "checkAdd");
        assert!(done["finished_at"].is_string());
    }

    /// A command whose runner is missing is said as one that could not run,
    /// with why, its exit code and the end of its output, never as a failed
    /// run of the tests it planned.
    #[test]
    fn a_run_whose_command_could_not_run_is_not_said_as_a_failure() {
        let run = a_run(Stage::Running);
        run.finish(Outcome::Answered(WorkVerifyResponse {
            runnable: true,
            reach: Some("whole".to_string()),
            tests: vec![super::super::verify::WorkTest {
                test_id: "t".to_string(),
                name: "404s.test".to_string(),
                entity_id: Some("c".to_string()),
            }],
            run: Some(super::super::verify::WorkRun {
                run_id: "r".to_string(),
                outcome: "could_not_run".to_string(),
                duration_ms: 395,
                exit_code: Some(127),
                finished_at: now(),
                reason: Some("The shell could not find `eslint`, so no test ran.".to_string()),
                output_tail: Some("sh: eslint: command not found".to_string()),
            }),
            ..WorkVerifyResponse::default()
        }));
        let done = run.json();
        assert_eq!(done["state"], "done");
        assert_eq!(done["outcome"], "could_not_run");
        assert_eq!(done["phase"], "The test command could not run.");
        assert!(!done["phase"].as_str().unwrap().contains("failed"));
        assert_eq!(done["result"]["run"]["exit_code"], 127);
        assert_eq!(
            done["result"]["run"]["reason"],
            "The shell could not find `eslint`, so no test ran."
        );
        assert_eq!(
            done["result"]["run"]["output_tail"],
            "sh: eslint: command not found"
        );
    }

    #[test]
    fn work_no_test_reaches_finishes_without_a_run() {
        let run = a_run(Stage::Planning);
        run.finish(Outcome::Answered(WorkVerifyResponse {
            runnable: true,
            entities: vec![super::super::verify::WorkEntity {
                id: "e".to_string(),
                name: "negate".to_string(),
                kind: "function".to_string(),
            }],
            ..WorkVerifyResponse::default()
        }));
        let done = run.json();
        assert_eq!(done["outcome"], "no_tests");
        assert_eq!(
            done["phase"],
            "No test reaches this work within 3 recorded hops."
        );
    }

    #[test]
    fn a_daemon_refusal_is_reported_as_its_own_sentence() {
        let refused = anyhow::anyhow!(
            "kin verify work refused (HTTP 400): No entity with the id 7 is in this repository."
        );
        assert_eq!(
            failure(&refused),
            "Kin could not run the tests: No entity with the id 7 is in this repository."
        );
        assert_eq!(
            failure(&anyhow::anyhow!("the daemon stopped answering")),
            "Kin could not run the tests: the daemon stopped answering"
        );
    }

    #[test]
    fn simultaneous_requests_reserve_only_one_run_and_duplicate_ids_reuse_it() {
        let runs = Arc::new(VerifyRuns::default());
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let runs = runs.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let run = a_run(Stage::Reading);
                    barrier.wait();
                    matches!(runs.reserve(run), Reservation::Started)
                })
            })
            .collect();
        let started = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .filter(|started| *started)
            .count();
        assert_eq!(started, 1);
        assert_eq!(locked(&runs.runs).len(), 1);
        let running = runs.running().unwrap();
        match runs.reserve(running.clone()) {
            Reservation::Known(known) => assert!(Arc::ptr_eq(&known, &running)),
            _ => panic!("a repeated operation must reuse its original run"),
        }
    }

    /// Drive one run against a daemon whose admission answers `admitted`,
    /// whose graph names entities as `entity` says, and whose test routes
    /// count every request. Returns how many test requests arrived and the
    /// run as the page reads it.
    async fn drive_after_admission(admitted: Value, entity: Value) -> (usize, Value) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let mut identity =
            kin_core::runtime_identity::RuntimeServiceIdentity::current("hosted-daemon").unwrap();
        identity.namespace = None;
        let app = Router::new()
            .route(
                "/runtime/identity",
                axum::routing::get(move || {
                    let identity = identity.clone();
                    async move { Json(identity) }
                }),
            )
            .route(
                "/commands/admit",
                post(move || {
                    let admitted = admitted.clone();
                    async move { Json(admitted) }
                }),
            )
            .route(
                "/mcp/tools/call",
                post(move || {
                    let entity = entity.clone();
                    async move {
                        Json(json!({"content": [{"type": "text", "text": entity.to_string()}]}))
                    }
                }),
            )
            .route(
                "/verify/work",
                post(move || {
                    let count = count.clone();
                    async move {
                        count.fetch_add(1, Ordering::SeqCst);
                        Json(json!({"runnable": true, "entities": [], "tests": []}))
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = DaemonClient::from_base_url_with_explicit_authority(
            format!("http://{address}"),
            None,
            None,
        )
        .unwrap();
        let run = a_run(Stage::Reading);
        drive(client, run.clone()).await;
        server.abort();
        (calls.load(Ordering::SeqCst), run.json())
    }

    #[tokio::test]
    async fn tests_are_not_planned_or_run_after_an_incomplete_admission() {
        let (calls, run) = drive_after_admission(
            json!({"lines": [], "mutated": false, "report": null}),
            json!({}),
        )
        .await;
        assert_eq!(calls, 0);
        assert_eq!(run["state"], "failed");
        assert_eq!(
            run["phase"],
            "Kin could not read the latest edits, so it did not run the tests. The daemon \
             returned no admission report, so whether the working copy was admitted is unknown; \
             read `kin graph status` for the state it left behind."
        );
        assert_eq!(run["error"], run["phase"]);
    }

    /// An edit removed an entity a recorded relationship still named, and the
    /// run used to say only that the edits could not be read. It names the
    /// entity, read from the graph that still holds it, and never the relation
    /// or entity ids.
    #[tokio::test]
    async fn an_unread_edit_that_removed_an_entity_names_it() {
        let negate = "14dd277c-657c-4b98-832d-dad86ae678a9";
        let refusal = format!(
            "Complete exact-tree admission failed: Graph error: storage error: transaction \
             relation d5442745-dfb2-5501-ab92-d8e315c6bda8 has unadmitted destination endpoint \
             entity:{negate}"
        );
        let (calls, run) = drive_after_admission(
            json!({"lines": [refusal], "mutated": false, "report": null}),
            json!({"entity_id": negate, "name": "negate", "kind": "Function"}),
        )
        .await;
        assert_eq!(
            calls, 0,
            "nothing is planned or run after a failed admission"
        );
        assert_eq!(run["state"], "failed");
        let phase = run["phase"].as_str().unwrap();
        assert_eq!(
            phase,
            "Kin could not read the latest edits, so it did not run the tests. The edits removed \
             the function `negate`, and the graph still holds a relationship that names it."
        );
        assert!(!phase.contains(negate) && !phase.contains("d5442745"));
    }

    #[test]
    fn an_unread_edit_keeps_its_cause_without_the_layers_it_failed_through() {
        assert_eq!(
            unread_edits_sentence(
                "complete exact-tree admission failed: Graph error: storage error: the store \
                 refused the transition",
                None
            ),
            "Kin could not read the latest edits, so it did not run the tests. The store \
             refused the transition."
        );
        assert_eq!(
            unread_edits_sentence(
                "Graph error: storage error: transaction relation r1 has unadmitted source \
                 endpoint entity:14dd277c-657c-4b98-832d-dad86ae678a9",
                None
            ),
            "Kin could not read the latest edits, so it did not run the tests. The edits removed \
             an entity, and the graph still holds a relationship that names it."
        );
        assert_eq!(
            unread_edits_sentence("   ", None),
            "Kin could not read the latest edits, so it did not run the tests."
        );
        assert_eq!(
            stranded_entity("has unadmitted destination endpoint entity:not-an-id"),
            None
        );
    }

    #[test]
    fn finished_runs_are_forgotten_before_running_ones() {
        let runs = VerifyRuns::default();
        let running = a_run(Stage::Running);
        assert!(matches!(
            runs.reserve(running.clone()),
            Reservation::Started
        ));
        for _ in 0..RUNS_KEPT + 3 {
            let finished = a_run(Stage::Planning);
            finished.finish(Outcome::Failed("x".to_string()));
            let mut kept = locked(&runs.runs);
            kept.push(finished);
            VerifyRuns::trim(&mut kept);
        }
        assert_eq!(locked(&runs.runs).len(), RUNS_KEPT);
        assert!(runs.find(running.id).is_some());
        assert_eq!(runs.running().map(|run| run.id), Some(running.id));
    }
}
