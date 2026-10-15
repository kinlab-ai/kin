// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

use anyhow::{anyhow, bail, Context, Result};
use kin_model::{
    Entity, EntityDelta, EntityFilter, EntityStore, GraphStore, Hash256, SemanticChange,
    SemanticChangeId, TestCase, TestRunner, Timestamp, VerificationRun, VerificationRunId,
    VerificationStatus, VerificationStore, WorkItem, WorkScope,
};
use kin_runtime::workspace::record_verification_evidence;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

use crate::commands::repository_authority::RequestRepositoryAuthority;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyRunRequest {
    pub entity: String,
    pub runner: String,
    pub depth: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyRunResponse {
    pub lines: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum VerifyCommandRequest {
    Entity {
        entity: String,
    },
    Summary,
    Missing,
    Plan {
        entity: String,
        depth: u32,
    },
    PlanChange {
        #[serde(default)]
        change_id: Option<String>,
        depth: u32,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyCommandResponse {
    #[serde(default)]
    pub lines: Vec<String>,
}

/// `kin verify <entity>` — Check verification / test coverage for an entity.
///
/// Shows per-entity test linkage and overall coverage summary.
pub async fn run(entity: String) -> Result<()> {
    let layout = crate::commands::require_repository_layout()?;
    print_verify_response(
        run_daemon_verify_command(&layout, &VerifyCommandRequest::Entity { entity }).await?,
    )
}

/// `kin verify --summary` — Show repository-wide coverage summary only.
pub async fn summary() -> Result<()> {
    let layout = crate::commands::require_repository_layout()?;
    print_verify_response(run_daemon_verify_command(&layout, &VerifyCommandRequest::Summary).await?)
}

/// `kin verify --missing` — Show only entities without any linked test.
pub async fn missing() -> Result<()> {
    let layout = crate::commands::require_repository_layout()?;
    print_verify_response(run_daemon_verify_command(&layout, &VerifyCommandRequest::Missing).await?)
}

/// `kin verify plan <entity> --depth 2` — Show the targeted proof set Kin would
/// use for verification, widened by downstream semantic impact.
pub async fn plan(entity: String, depth: u32) -> Result<()> {
    let layout = crate::commands::require_repository_layout()?;
    print_verify_response(
        run_daemon_verify_command(&layout, &VerifyCommandRequest::Plan { entity, depth }).await?,
    )
}

/// `kin verify change [<change-id>] --depth 2` — Show the targeted proof set
/// Kin would use for a semantic change, defaulting to the current HEAD.
pub async fn plan_change(change_id: Option<String>, depth: u32) -> Result<()> {
    let layout = crate::commands::require_repository_layout()?;
    print_verify_response(
        run_daemon_verify_command(
            &layout,
            &VerifyCommandRequest::PlanChange { change_id, depth },
        )
        .await?,
    )
}

/// `kin verify run <entity> --runner cargo` — Execute a targeted runner and
/// record a persisted `VerificationRun`.
///
/// If linked tests exist for the entity, Kin drives the runner from that proof
/// set. Otherwise it falls back to an entity-name filter and still records a
/// proof run for the entity.
pub async fn run_verification(entity: String, runner: String, depth: u32) -> Result<()> {
    let layout = crate::commands::require_repository_layout()?;
    let response = run_daemon_verify_run(
        &layout,
        &VerifyRunRequest {
            entity,
            runner,
            depth,
        },
    )
    .await?;
    for line in response.lines {
        println!("{line}");
    }
    Ok(())
}

async fn run_daemon_verify_run(
    layout: &kin_core::KinLayout,
    request: &VerifyRunRequest,
) -> Result<VerifyRunResponse> {
    let daemon_url = std::env::var("KIN_DAEMON_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(Some)
        .unwrap_or(crate::daemon_client::resolve_daemon_url(layout).await?);
    let base_url = daemon_url
        .ok_or_else(|| crate::daemon_client::daemon_required_error("verify run", layout))?;
    let client = crate::daemon_client::DaemonClient::from_base_url(base_url)?;
    client
        .verify_run(request)
        .await
        .context("daemon verify run failed")
}

async fn run_daemon_verify_command(
    layout: &kin_core::KinLayout,
    request: &VerifyCommandRequest,
) -> Result<VerifyCommandResponse> {
    let daemon_url = std::env::var("KIN_DAEMON_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(Some)
        .unwrap_or(crate::daemon_client::resolve_daemon_url(layout).await?);
    let base_url =
        daemon_url.ok_or_else(|| crate::daemon_client::daemon_required_error("verify", layout))?;
    let client = crate::daemon_client::DaemonClient::from_base_url(base_url)?;
    client
        .verify_command(request)
        .await
        .context("daemon verify failed")
}

fn print_verify_response(response: VerifyCommandResponse) -> Result<()> {
    for line in response.lines {
        println!("{line}");
    }
    Ok(())
}

pub fn execute_verify_command(
    binding: &kin_core::LocalRepositoryAuthorityBinding,
    graph: &kin_db::InMemoryGraph,
    request: &VerifyCommandRequest,
) -> Result<VerifyCommandResponse> {
    match request {
        VerifyCommandRequest::Entity { entity } => build_entity_verify_response(graph, entity),
        VerifyCommandRequest::Summary => build_verify_summary_response(graph),
        VerifyCommandRequest::Missing => build_verify_missing_response(graph),
        VerifyCommandRequest::Plan { entity, depth } => {
            let plan = build_verification_plan(graph, entity, *depth)?;
            Ok(VerifyCommandResponse {
                lines: verification_plan_lines(&plan),
            })
        }
        VerifyCommandRequest::PlanChange { change_id, depth } => {
            let change = resolve_change(graph, binding, change_id.as_deref())?;
            let plan = build_change_verification_plan(graph, &change, *depth)?;
            Ok(VerifyCommandResponse {
                lines: change_verification_plan_lines(&plan),
            })
        }
    }
}

/// Why `kin verify` has nothing to report for a symbol outside the
/// repository, in the words that finish its refusal.
const VERIFY_EXTERNAL_WHY: &str =
    "`kin verify` has no tests linked to it here, since tests link to repository entities";

fn build_entity_verify_response(
    graph: &kin_db::InMemoryGraph,
    entity: &str,
) -> Result<VerifyCommandResponse> {
    // A symbol outside the repository is refused by what it is, never with
    // the absence line the daemon waits out as a query that matched nothing.
    // An address naming nothing held stays that absence line below.
    if let Some(node) = crate::commands::external_symbols::lookup(graph, entity)? {
        return Ok(VerifyCommandResponse {
            lines: crate::commands::external_symbols::refusal_lines(&node, VERIFY_EXTERNAL_WHY),
        });
    }
    let filter = EntityFilter {
        name_pattern: Some(entity.to_string()),
        ..Default::default()
    };
    let entities = graph.query_entities(&filter)?;

    if entities.is_empty() {
        return Ok(VerifyCommandResponse {
            lines: vec![entity_not_found_line(entity)],
        });
    }

    let mut lines = Vec::new();
    let mut covered_count = 0usize;
    let mut uncovered_count = 0usize;

    for ent in &entities {
        // Recorded test cases, and the test entities that reach it.
        let mut tests = graph.get_tests_for_entity(&ent.id)?;
        tests.extend(
            tests_reaching(graph, &ent.id)?
                .iter()
                .map(|test| test_case_for_entity(test, &ent.id)),
        );
        if tests.is_empty() {
            uncovered_count += 1;
            lines.push(format!("  MISSING  {} ({:?})", ent.name, ent.kind));
        } else {
            covered_count += 1;
            lines.push(format!(
                "  COVERED  {} ({:?}): {} test(s)",
                ent.name,
                ent.kind,
                tests.len()
            ));
            for test in &tests {
                lines.push(format!(
                    "           - {} [{}] runner={}",
                    test.name, test.kind, test.runner
                ));
            }
        }
    }

    lines.push(String::new());
    lines.push(format!(
        "Matched {} entity(ies): {} covered, {} missing proof",
        entities.len(),
        covered_count,
        uncovered_count,
    ));
    lines.push(String::new());
    lines.extend(verify_summary_lines(graph)?);

    Ok(VerifyCommandResponse { lines })
}

fn build_verify_summary_response(graph: &kin_db::InMemoryGraph) -> Result<VerifyCommandResponse> {
    Ok(VerifyCommandResponse {
        lines: verify_summary_lines(graph)?,
    })
}

fn build_verify_missing_response(graph: &kin_db::InMemoryGraph) -> Result<VerifyCommandResponse> {
    let summary = coverage_reading(graph)?;
    if summary.missing_proof.is_empty() {
        return Ok(VerifyCommandResponse {
            lines: vec![format!(
                "All {} entities have linked proof.",
                summary.total_entities
            )],
        });
    }

    let mut lines = vec![format!(
        "Entities missing proof ({}/{}):",
        summary.missing_proof.len(),
        summary.total_entities
    )];

    for eid in &summary.missing_proof {
        if let Some(entity) = graph.get_entity(eid)? {
            let file = entity
                .file_origin
                .as_ref()
                .map(|f| f.to_string())
                .unwrap_or_else(|| "unknown".to_string());
            lines.push(format!(
                "  - {} ({:?}) in {}",
                entity.name, entity.kind, file
            ));
        } else {
            lines.push(format!("  - {}", eid));
        }
    }

    Ok(VerifyCommandResponse { lines })
}

fn verify_summary_lines(graph: &kin_db::InMemoryGraph) -> Result<Vec<String>> {
    let summary = coverage_reading(graph)?;
    let mut lines = vec![
        "Repository Coverage:".to_string(),
        format!(
            "  {}/{} entities covered ({:.1}%)",
            summary.covered_entities,
            summary.total_entities,
            summary.coverage_ratio * 100.0
        ),
    ];

    if summary.missing_proof.is_empty() {
        lines.push("  All entities have linked proof.".to_string());
    } else {
        lines.push(format!(
            "  {} entities missing proof",
            summary.missing_proof.len()
        ));
    }

    Ok(lines)
}

/// A `kin verify run` planned and ready to run: its proof set, the runner it
/// was asked for, and the command line that runs it.
pub struct PreparedVerifyRun {
    entity: Entity,
    work: WorkPlan,
    runner: TestRunner,
    runner_label: String,
    line: String,
    lines: Vec<String>,
}

impl PreparedVerifyRun {
    pub fn command_line(&self) -> &str {
        &self.line
    }
}

/// Plan `kin verify run`: resolve the entity, plan its proof set and build
/// the runner's command. Reads the graph only.
pub fn prepare_verify_run(
    graph: &kin_db::InMemoryGraph,
    request: &VerifyRunRequest,
) -> Result<PreparedVerifyRun> {
    let plan = build_verification_plan(graph, &request.entity, request.depth)?;
    let test_runner = parse_runner(&request.runner)?;
    let cmd_str = build_runner_command(&test_runner, &plan.entity.name, &plan.tests);
    let mut lines = Vec::new();

    if plan.tests.is_empty() {
        lines.push(format!(
            "No linked tests found for '{}'; falling back to an entity-scoped runner filter.",
            plan.entity.name
        ));
    } else {
        lines.extend(verification_plan_lines(&plan));
    }

    lines.push(format!("Running: {}", cmd_str));

    let entity = plan.entity.clone();
    let mut work = WorkPlan::from_plans(vec![plan]);
    work.capture_dependencies(graph)?;
    Ok(PreparedVerifyRun {
        entity,
        work,
        runner: test_runner,
        runner_label: request.runner.clone(),
        line: cmd_str,
        lines,
    })
}

/// The records of a finished `kin verify run`, and what it answers once they
/// are durable.
pub fn record_verify_run(
    authority: &RequestRepositoryAuthority,
    graph: &kin_db::InMemoryGraph,
    prepared: &PreparedVerifyRun,
    executed: &ExecutedRun,
) -> Result<(VerificationRecord, VerifyRunResponse)> {
    let proved_entities = prepared.work.proved_entities.len();
    let proved_work = prepared.work.proved_work_items.len();
    let record = plan_verification_record(
        authority,
        graph,
        &prepared.work,
        prepared.runner.clone(),
        executed,
        "verify.run",
        Some(WorkScope::Entity(prepared.entity.id)),
        |run| {
            format!(
                "runner={}; status={}; tests={}; impacted_entities={}",
                prepared.runner_label,
                audit_status(run),
                run.test_ids.len(),
                proved_entities
            )
        },
    )?;
    let run = &record.run;
    let mut lines = prepared.lines.clone();
    lines.push(String::new());
    lines.push("VerificationRun recorded:".to_string());
    lines.push(format!("  Run ID:   {}", run.run_id));
    lines.push(format!(
        "  Entity:   {} ({:?})",
        prepared.entity.name, prepared.entity.kind
    ));
    lines.push(format!("  Status:   {}", run_word(run)));
    if let Some(reason) = &executed.could_not_run {
        lines.push(format!("  Reason:   {reason}"));
    }
    lines.push(format!("  Duration: {}ms", executed.duration_ms));
    lines.push(format!(
        "  Exit:     {}",
        executed
            .exit_code
            .map_or_else(|| "none".to_string(), |code| code.to_string())
    ));
    lines.push(format!("  Tests:    {}", run.test_ids.len()));
    lines.push(format!("  Entities: {}", proved_entities));
    lines.push(format!("  Work:     {}", proved_work));
    if let Some(ref blob) = run.evidence_blob {
        lines.push(format!("  Evidence: {}", blob));
    }
    if run.status != VerificationStatus::Passing && !executed.output_tail.is_empty() {
        lines.push("  Last output:".to_string());
        lines.extend(
            executed
                .output_tail
                .lines()
                .map(|line| format!("    {line}")),
        );
    }
    Ok((record, VerifyRunResponse { lines }))
}

/// One execution of a test command in the repository's working tree, before
/// it is recorded.
#[derive(Debug, Clone)]
pub struct ExecutedRun {
    started_at: Timestamp,
    finished_at: Timestamp,
    duration_ms: u64,
    /// `Passing` or `Failing` when the command ran, and `Pending` when it
    /// could not run, which attests nothing about any test.
    status: VerificationStatus,
    /// The command's exit code. `None` when it has none: it never started,
    /// or a signal ended it.
    exit_code: Option<i32>,
    /// Why the command could not run, when it could not.
    could_not_run: Option<String>,
    /// The last lines it printed, as a reader is shown them.
    output_tail: String,
    evidence_text: String,
}

impl ExecutedRun {
    /// Read a command that finished: its exit code when it has one, and what
    /// it printed to each stream.
    fn finished(exit_code: Option<i32>, stdout: &str, stderr: &str) -> Self {
        let could_not_run = could_not_run_reason(exit_code, stdout, stderr);
        let status = match (&could_not_run, exit_code) {
            (Some(_), _) => VerificationStatus::Pending,
            (None, Some(0)) => VerificationStatus::Passing,
            (None, _) => VerificationStatus::Failing,
        };
        let streams = format!("=== STDOUT ===\n{stdout}\n=== STDERR ===\n{stderr}");
        let evidence_text = match &could_not_run {
            Some(reason) => format!("=== COULD NOT RUN ===\n{reason}\n{streams}"),
            None => streams,
        };
        Self {
            started_at: Timestamp::now(),
            finished_at: Timestamp::now(),
            duration_ms: 0,
            status,
            exit_code,
            could_not_run,
            output_tail: output_tail(stdout, stderr),
            evidence_text,
        }
    }

    /// A command Kin could not start at all.
    fn never_started(error: &std::io::Error) -> Self {
        let reason = format!("Kin could not start the test command: {error}.");
        Self {
            started_at: Timestamp::now(),
            finished_at: Timestamp::now(),
            duration_ms: 0,
            status: VerificationStatus::Pending,
            exit_code: None,
            output_tail: String::new(),
            evidence_text: format!("=== COULD NOT RUN ===\n{reason}\n"),
            could_not_run: Some(reason),
        }
    }

    /// Whether the command passed, failed, or could not run, as the page and
    /// `kin verify` say it.
    fn outcome(&self) -> &'static str {
        match self.status {
            VerificationStatus::Passing => "passed",
            VerificationStatus::Pending => "could_not_run",
            _ => "failed",
        }
    }
}

/// Run `command` in `working_dir`, the repository's own working tree, and
/// keep what it printed as the run's evidence.
pub fn execute_verification(working_dir: &std::path::Path, command: &str) -> ExecutedRun {
    let started_at = Timestamp::now();
    let start_instant = std::time::Instant::now();
    let output = verification_command(working_dir, command).output();
    let duration = start_instant.elapsed();
    let finished_at = Timestamp::now();

    let executed = match output {
        Ok(out) => ExecutedRun::finished(
            out.status.code(),
            &String::from_utf8_lossy(&out.stdout),
            &String::from_utf8_lossy(&out.stderr),
        ),
        Err(error) => ExecutedRun::never_started(&error),
    };
    ExecutedRun {
        started_at,
        finished_at,
        duration_ms: duration.as_millis() as u64,
        ..executed
    }
}

/// The lines a reader is shown from a command's output, at most this many.
const OUTPUT_TAIL_LINES: usize = 20;
/// The most characters of one shown line.
const OUTPUT_TAIL_LINE_CHARS: usize = 240;

/// The last lines a command printed, standard error last, without blank
/// lines or terminal color codes.
fn output_tail(stdout: &str, stderr: &str) -> String {
    let lines: Vec<String> = stdout
        .lines()
        .chain(stderr.lines())
        .map(strip_terminal_codes)
        .map(|line| line.trim_end().to_string())
        .filter(|line| !line.trim().is_empty())
        .collect();
    let start = lines.len().saturating_sub(OUTPUT_TAIL_LINES);
    lines[start..]
        .iter()
        .map(
            |line| match line.char_indices().nth(OUTPUT_TAIL_LINE_CHARS) {
                Some((cut, _)) => format!("{}…", &line[..cut]),
                None => line.clone(),
            },
        )
        .collect::<Vec<_>>()
        .join("\n")
}

/// `line` without ANSI escape sequences, which a runner prints for color.
fn strip_terminal_codes(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(character) = chars.next() {
        if character != '\u{1b}' {
            out.push(character);
            continue;
        }
        if chars.peek() == Some(&'[') {
            chars.next();
            for code in chars.by_ref() {
                if ('@'..='~').contains(&code) {
                    break;
                }
            }
        }
    }
    out
}

/// What a shell said about a command it was asked to run and could not.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ShellComplaint {
    /// It found no such command.
    NotFound(String),
    /// It found the command and could not execute it.
    NotExecutable(String),
}

/// The shells and launchers whose own complaint names a missing runner.
const COMPLAINING_SHELLS: [&str; 7] = ["sh", "bash", "zsh", "dash", "ash", "ksh", "env"];

/// Read one output line as a shell's complaint about a command, in the forms
/// the common shells print: `sh: eslint: command not found`, `/bin/sh: line 1:
/// eslint: command not found`, `sh: 1: eslint: not found`, `zsh: command not
/// found: eslint`, `env: node: No such file or directory` and `sh: ./run:
/// Permission denied`. Any other line is `None`, so a test's own output that
/// mentions "not found" is never read as one.
fn shell_complaint(line: &str) -> Option<ShellComplaint> {
    let mut parts = line.trim().split(':').map(str::trim);
    let shell = parts.next()?;
    let shell = shell.rsplit('/').next().unwrap_or(shell);
    if !COMPLAINING_SHELLS.contains(&shell) {
        return None;
    }
    // Where the shell was reading: `line 1`, or dash's and zsh's bare number.
    let rest: Vec<&str> = parts
        .skip_while(|part| {
            part.strip_prefix("line ")
                .unwrap_or(part)
                .bytes()
                .all(|byte| byte.is_ascii_digit())
        })
        .collect();
    let name = |raw: &str| {
        let name = raw.trim_matches(|c: char| matches!(c, '\'' | '"' | '‘' | '’' | '`'));
        (!name.is_empty()).then(|| name.to_string())
    };
    match rest.as_slice() {
        ["command not found", command] => name(command).map(ShellComplaint::NotFound),
        [command, "command not found" | "not found" | "No such file or directory"] => {
            name(command).map(ShellComplaint::NotFound)
        }
        [command, complaint, ..]
            if *complaint == "Permission denied" || complaint.starts_with("cannot execute") =>
        {
            name(command).map(ShellComplaint::NotExecutable)
        }
        _ => None,
    }
}

/// Whether a line reports that a command it ran exited 126 or 127, as a
/// package manager or build tool relays it: `error Command failed with exit
/// code 127.`, `npm error code 127`, `make: *** [test] Error 127`.
fn relays_could_not_run(line: &str) -> bool {
    let line = line.to_ascii_lowercase();
    ["code ", "error ", "errno ", "exit status "]
        .iter()
        .any(|lead| {
            line.match_indices(lead).any(|(at, _)| {
                let after = &line[at + lead.len()..];
                (after.starts_with("126") || after.starts_with("127"))
                    && !after[3..].starts_with(|c: char| c.is_ascii_digit())
            })
        })
}

/// Why a finished command could not run, or `None` when it ran and its exit
/// code is its outcome.
///
/// A command could not run when the shell exited 126 or 127, which POSIX
/// reserves for a command found but not executable and a command not found,
/// or when it exited otherwise and the shell's own complaint about a missing
/// runner is what ended it: the last thing it printed, or relayed by a
/// wrapper as an exit of 126 or 127. Any other exit is the outcome of tests
/// that ran, so a real failure stays a failure.
fn could_not_run_reason(exit_code: Option<i32>, stdout: &str, stderr: &str) -> Option<String> {
    let exit = exit_code?;
    if exit == 0 {
        return None;
    }
    let lines = || stdout.lines().chain(stderr.lines());
    let complaint = lines().rev().find_map(shell_complaint);
    let shell_exit = matches!(exit, 126 | 127);
    if !shell_exit {
        let last_said = stderr
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .or_else(|| stdout.lines().rev().find(|line| !line.trim().is_empty()));
        let ended_it =
            last_said.and_then(shell_complaint).is_some() || lines().any(relays_could_not_run);
        if complaint.is_none() || !ended_it {
            return None;
        }
    }
    Some(match (complaint, exit) {
        (Some(ShellComplaint::NotFound(command)), _) => {
            format!("The shell could not find `{command}`, so no test ran.")
        }
        (Some(ShellComplaint::NotExecutable(command)), _) => {
            format!("The shell found `{command}` but could not execute it, so no test ran.")
        }
        (None, 126) => "The shell found a command the test command runs but could not execute \
                        it, so no test ran."
            .to_string(),
        (None, _) => {
            "The shell could not find a command the test command runs, so no test ran.".to_string()
        }
    })
}

/// A finished run's records, planned and written nowhere yet: the run tied
/// to the fingerprints its proof set was planned against, what it proves, and
/// the audit event that says who ran it.
///
/// A run is repository authority, as a review is. Its writer commits
/// [`Self::to_delta`] to authority first and only then shows the run to a
/// graph with [`Self::apply_to`], so no graph holds a run, or a relation from
/// one, that authority does not hold. A working-copy admission checks every
/// relation's ends against authority's records, and refuses a graph that got
/// ahead of them.
#[derive(Debug, Clone)]
pub struct VerificationRecord {
    run: VerificationRun,
    proved_entities: Vec<kin_model::EntityId>,
    proved_work_items: Vec<kin_model::WorkId>,
    /// The acting identity, when the store does not hold it yet.
    actor: Option<kin_model::Actor>,
    audit_event: kin_model::AuditEvent,
    actor_label: String,
    action: &'static str,
}

impl VerificationRecord {
    pub fn run(&self) -> &VerificationRun {
        &self.run
    }

    /// Who the run is recorded as made by.
    pub fn actor_label(&self) -> &str {
        &self.actor_label
    }

    /// What the transaction that admits the run says it is.
    pub fn reason(&self) -> String {
        format!("{} {}", self.action, self.run.run_id)
    }

    /// The collaboration delta that admits these records into repository
    /// authority.
    pub fn to_delta(&self) -> Result<kin_model::CollaborationDelta> {
        let delta = kin_model::CollaborationDelta {
            verification_runs: vec![kin_model::Keyed::new(self.run.run_id, self.run.clone())],
            actors: self
                .actor
                .iter()
                .map(|actor| kin_model::Keyed::new(actor.actor_id, actor.clone()))
                .collect(),
            audit_events: vec![self.audit_event.clone()],
            ..kin_model::CollaborationDelta::default()
        };
        delta
            .validate()
            .map_err(|error| anyhow!("the verification run is not a valid record: {error}"))?;
        Ok(delta)
    }

    /// Show `graph` the run, its proof links and its audit event.
    pub fn apply_to<G>(&self, graph: &G) -> Result<()>
    where
        G: GraphStore,
        <G as GraphStore>::Error: std::fmt::Display + Send + Sync + 'static,
    {
        if let Some(actor) = &self.actor {
            if graph
                .get_actor(&actor.actor_id)
                .map_err(|err| anyhow!(err.to_string()))?
                .is_none()
            {
                graph
                    .create_actor(actor)
                    .map_err(|err| anyhow!(err.to_string()))?;
            }
        }
        record_verification_evidence(
            graph,
            &self.run,
            &self.proved_entities,
            &self.proved_work_items,
        )
        .map_err(|err: Box<dyn std::error::Error>| anyhow!(err.to_string()))?;
        graph
            .record_audit_event(&self.audit_event)
            .map_err(|err| anyhow!(err.to_string()))?;
        Ok(())
    }
}

/// Plan the records of `executed` as a run of `work`'s proof set. Keeps the
/// run's output as its evidence, and writes nothing else.
#[allow(clippy::too_many_arguments)]
fn plan_verification_record(
    authority: &RequestRepositoryAuthority,
    graph: &kin_db::InMemoryGraph,
    work: &WorkPlan,
    runner: TestRunner,
    executed: &ExecutedRun,
    action: &'static str,
    scope: Option<WorkScope>,
    details: impl FnOnce(&VerificationRun) -> String,
) -> Result<VerificationRecord> {
    let evidence_blob = store_evidence_blob(authority, &executed.evidence_text);
    let run = VerificationRun {
        run_id: VerificationRunId::new(),
        test_ids: work.tests.iter().map(|test| test.test_id).collect(),
        status: executed.status,
        runner,
        started_at: executed.started_at.clone(),
        finished_at: Some(executed.finished_at.clone()),
        duration_ms: Some(executed.duration_ms),
        evidence_blob,
        exit_code: executed.exit_code,
        ran_against: work.ran_against.clone(),
        proof: Some(kin_model::VerificationProof {
            entities: work
                .proved_entities
                .iter()
                .map(|entity| entity.id)
                .collect(),
            work_items: work
                .proved_work_items
                .iter()
                .map(|item| item.work_id)
                .collect(),
        }),
    };
    let actor_label = crate::provenance::current_actor_label();
    let (actor, audit_event) = crate::provenance::plan_audit_event(
        graph,
        &actor_label,
        action,
        scope,
        Some(details(&run)),
    )?;
    Ok(VerificationRecord {
        proved_entities: work
            .proved_entities
            .iter()
            .map(|entity| entity.id)
            .collect(),
        proved_work_items: work
            .proved_work_items
            .iter()
            .map(|work| work.work_id)
            .collect(),
        run,
        actor,
        audit_event,
        actor_label,
        action,
    })
}

// ---------------------------------------------------------------------------
// The tests that reach a piece of work, run with the repository's own command
// ---------------------------------------------------------------------------

/// What the reach rule reads of an entity that reaches another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReachClass {
    /// A test by its role or kind.
    pub is_test: bool,
    /// A module, which holds the functions that use what it imports.
    pub is_module_container: bool,
}

impl ReachClass {
    pub fn of(entity: &Entity) -> Self {
        Self {
            is_test: is_test_entity(entity),
            is_module_container: kin_model::is_module_container(entity),
        }
    }
}

/// Whether an entity is a test, by its role or its kind.
pub fn is_test_entity(entity: &Entity) -> bool {
    entity.role == kin_model::EntityRole::Test || entity.kind == kin_model::EntityKind::Test
}

/// The test entities that reach `target`, by id, over `all`, every relation
/// read for it.
///
/// A test entity reaches it when it is the source of an impact relation into
/// it and is a test by its role or kind, or the relation is itself `tests` or
/// `covers`. A module that only imports it is not one, since the functions
/// inside the module that use it are. `class` answers for a source, or
/// `None` for one the graph does not hold.
///
/// This is the direct-coverage rule shared by coverage readings and the
/// list of what no test reaches directly. Code, guided review and a proof
/// plan also read test candidates in the bounded ranked impact traversal.
pub fn reaching_test_ids(
    all: &[kin_model::Relation],
    target: &kin_model::EntityId,
    class: &mut dyn FnMut(&kin_model::EntityId) -> Option<ReachClass>,
) -> Vec<kin_model::EntityId> {
    use kin_model::RelationKind;
    let mut seen = HashSet::new();
    let mut tests = Vec::new();
    for relation in kin_review::inbound_impact_relation_refs(all, target) {
        let Some(source) = relation.src.as_entity() else {
            continue;
        };
        if source == *target || seen.contains(&source) {
            continue;
        }
        let Some(class) = class(&source) else {
            continue;
        };
        let imports = matches!(
            relation.kind,
            RelationKind::Imports | RelationKind::Includes
        );
        if imports && class.is_module_container {
            continue;
        }
        if class.is_test || matches!(relation.kind, RelationKind::Tests | RelationKind::Covers) {
            seen.insert(source);
            tests.push(source);
        }
    }
    tests
}

/// The entities no recorded test case covers, by the graph's own summary. An
/// entity is missing proof when it is one of these and no test entity
/// reaches it either ([`reaching_test_ids`]): the two halves of one coverage
/// reading. A test case's cover is a relation from the test case, which a
/// read of an entity's own relations does not carry, so it is read here.
pub fn uncovered_by_test_cases<G>(graph: &G) -> Result<HashSet<kin_model::EntityId>>
where
    G: GraphStore,
    <G as GraphStore>::Error: std::fmt::Display + Send + Sync + 'static,
{
    Ok(graph
        .get_coverage_summary()
        .map_err(|err| anyhow!(err.to_string()))?
        .missing_proof
        .into_iter()
        .collect())
}

/// The test entities that directly reach `entity` ([`reaching_test_ids`]).
/// Bounded transitive test reach is read by the ranked impact traversal.
pub fn tests_reaching<G>(graph: &G, entity: &kin_model::EntityId) -> Result<Vec<Entity>>
where
    G: GraphStore,
    <G as GraphStore>::Error: std::fmt::Display + Send + Sync + 'static,
{
    let all = graph
        .get_all_relations_for_entity(entity)
        .map_err(|err| anyhow!(err.to_string()))?;
    let mut found: HashMap<kin_model::EntityId, Entity> = HashMap::new();
    let mut failed = None;
    let ids = reaching_test_ids(&all, entity, &mut |source| {
        if let Some(held) = found.get(source) {
            return Some(ReachClass::of(held));
        }
        match graph.get_entity(source) {
            Ok(Some(held)) => {
                let class = ReachClass::of(&held);
                found.insert(*source, held);
                Some(class)
            }
            Ok(None) => None,
            Err(err) => {
                failed.get_or_insert_with(|| anyhow!(err.to_string()));
                None
            }
        }
    });
    if let Some(error) = failed {
        return Err(error);
    }
    // Test code that is not a test of its own, such as a constant in a test
    // module, reaches as the test that holds it, counted once.
    let mut tests: Vec<Entity> = Vec::new();
    let mut counted = HashSet::new();
    for id in ids {
        let Some(test) = found.remove(&id) else {
            continue;
        };
        let test = if kin_review::is_test_unit(&test) || !is_test_entity(&test) {
            Some(test)
        } else {
            kin_review::test_unit_holding(&kin_review::impact::LiveGraph(graph), &test)
                .map_err(|err| anyhow!(err.to_string()))?
        };
        if let Some(test) = test.filter(|test| counted.insert(test.id)) {
            tests.push(test);
        }
    }
    tests.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
    Ok(tests)
}

/// Repository-wide structural test reach over the recorded semantic graph.
/// Each projected entity's relations are supplied once, then one multi-source
/// traversal visits each reachable entity once. Verification/work metadata
/// cannot form a path because both endpoints must be semantic entities.
pub struct RecordedTestReach {
    known: HashSet<kin_model::EntityId>,
    test_sources: HashSet<kin_model::EntityId>,
    frontier: Vec<kin_model::EntityId>,
    covered: HashSet<kin_model::EntityId>,
    outgoing: HashMap<kin_model::EntityId, HashSet<kin_model::EntityId>>,
}

impl RecordedTestReach {
    pub fn new(entities: &[Entity], uncovered_by_test_cases: HashSet<kin_model::EntityId>) -> Self {
        let known: HashSet<_> = entities.iter().map(|entity| entity.id).collect();
        let covered: HashSet<_> = known
            .difference(&uncovered_by_test_cases)
            .copied()
            .collect();
        let mut frontier: Vec<_> = entities
            .iter()
            .filter(|entity| kin_review::ranked_impact::is_test_impact_source(entity))
            .map(|entity| entity.id)
            .collect();
        // Native test-case scopes are recorded test reach too. Their covered
        // entity's dependencies can be followed without fabricating a test entity.
        frontier.extend(covered.iter().copied());
        let test_sources = entities
            .iter()
            .filter(|entity| {
                !kin_model::is_derived_member(entity)
                    && !matches!(
                        entity.role,
                        kin_model::EntityRole::Generated | kin_model::EntityRole::Vendored
                    )
            })
            .map(|entity| entity.id)
            .collect();
        Self {
            known,
            test_sources,
            frontier,
            covered,
            outgoing: HashMap::new(),
        }
    }

    /// Use the same edge policy as ranked impact, including declaring-parent
    /// exclusions. `all` must be the projected relations of `target`.
    pub fn observe_inbound(&mut self, all: &[kin_model::Relation], target: &kin_model::EntityId) {
        for relation in kin_review::inbound_impact_relation_refs(all, target) {
            let (Some(source), Some(destination)) =
                (relation.src.as_entity(), relation.dst.as_entity())
            else {
                continue;
            };
            if source == destination
                || !self.known.contains(&source)
                || !self.known.contains(&destination)
            {
                continue;
            }
            if matches!(
                relation.kind,
                kin_model::RelationKind::Tests | kin_model::RelationKind::Covers
            ) && self.test_sources.contains(&source)
            {
                self.frontier.push(source);
            }
            self.outgoing.entry(source).or_default().insert(destination);
        }
    }

    pub fn uncovered(mut self) -> HashSet<kin_model::EntityId> {
        let mut visited: HashSet<_> = self.frontier.iter().copied().collect();
        let mut pending: std::collections::VecDeque<_> = visited.iter().copied().collect();
        while let Some(source) = pending.pop_front() {
            for destination in self.outgoing.get(&source).into_iter().flatten() {
                self.covered.insert(*destination);
                if visited.insert(*destination) {
                    pending.push_back(*destination);
                }
            }
        }
        self.known.difference(&self.covered).copied().collect()
    }
}

/// A repository's recorded structural test reach. Native test-case scopes and
/// test entities cover everything reachable through supported semantic edges.
/// The rest have no recorded test reach, which does not establish complete
/// runtime coverage or an executed, passing test.
pub fn coverage_reading<G>(graph: &G) -> Result<kin_model::CoverageSummary>
where
    G: GraphStore,
    <G as GraphStore>::Error: std::fmt::Display + Send + Sync + 'static,
{
    let mut entities = graph
        .list_all_entities()
        .map_err(|err| anyhow!(err.to_string()))?;
    entities.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
    let mut reach = RecordedTestReach::new(&entities, uncovered_by_test_cases(graph)?);
    for entity in &entities {
        let all = kin_index::relation_read::relations_for_read(graph, &entity.id)
            .map_err(|err| anyhow!(err.to_string()))?;
        reach.observe_inbound(&all, &entity.id);
    }
    let uncovered = reach.uncovered();
    let missing_proof: Vec<_> = entities
        .iter()
        .filter(|entity| uncovered.contains(&entity.id))
        .map(|entity| entity.id)
        .collect();
    let total_entities = entities.len();
    let covered_entities = total_entities - missing_proof.len();
    Ok(kin_model::CoverageSummary {
        total_entities,
        covered_entities,
        coverage_ratio: if total_entities == 0 {
            0.0
        } else {
            covered_entities as f64 / total_entities as f64
        },
        missing_proof,
    })
}

/// The test a test entity is recorded as in a verification run. Derived from
/// the entity's identity alone, so every run of it names the same test and
/// its history survives edits to it.
pub fn test_id_for_entity(entity: &kin_model::EntityId) -> kin_model::TestId {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(b"kin.verify.test-entity.v1\0");
    hasher.update(entity.0.as_bytes());
    let mut bytes = [0u8; 32];
    bytes.copy_from_slice(&hasher.finalize());
    kin_model::TestId::from_hash(Hash256::from_bytes(bytes))
}

/// What a test entity's derived [`TestCase`] names as its runner: the
/// repository's own test command, whichever it declares.
const REPOSITORY_RUNNER: &str = "repository";

fn test_case_for_entity(test: &Entity, covers: &kin_model::EntityId) -> TestCase {
    TestCase {
        test_id: test_id_for_entity(&test.id),
        name: test.name.clone(),
        language: test.language.to_string(),
        kind: kin_model::TestKind::Unit,
        scopes: vec![WorkScope::Entity(*covers)],
        runner: TestRunner::Custom(REPOSITORY_RUNNER.to_string()),
        file_origin: test.file_origin.clone(),
    }
}

/// How this repository runs its tests, read from what its own manifests
/// declare ([`kin_parser::project_manifest::DeclaredTests`]). The manifests
/// are read in their root precedence, and the first that declares a way
/// wins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepositoryTestCommand {
    /// A runner Kin narrows to the planned tests by name.
    Runner(TestRunner),
    /// The repository's own test command, which Kin runs whole: its program
    /// and arguments.
    Whole(Vec<String>),
}

impl RepositoryTestCommand {
    /// The runner a run of it is recorded under.
    fn runner(&self) -> TestRunner {
        match self {
            Self::Runner(runner) => runner.clone(),
            Self::Whole(words) => TestRunner::Custom(words.join(" ")),
        }
    }

    /// `targeted` when the command runs only the planned tests, `whole` when
    /// it runs every test the repository has.
    fn reach(&self) -> &'static str {
        match self {
            Self::Runner(_) => "targeted",
            Self::Whole(_) => "whole",
        }
    }

    /// The shell line that runs `tests`.
    fn command_line(&self, tests: &[TestCase]) -> String {
        match self {
            Self::Runner(runner) => {
                let fallback = tests.first().map_or("", |test| test.name.as_str());
                build_runner_command(runner, fallback, tests)
            }
            Self::Whole(words) => words
                .iter()
                .map(|word| shell_quote(word))
                .collect::<Vec<_>>()
                .join(" "),
        }
    }
}

/// The sentence a repository with no test command Kin can run is answered
/// with.
pub const NO_TEST_COMMAND: &str = "This repository declares no test command Kin can run.";

/// This repository's test command, from the manifests at its root, or `None`
/// when none declares one Kin can run.
pub fn repository_test_command(
    project: &dyn kin_parser::project_manifest::ProjectFiles,
) -> Option<RepositoryTestCommand> {
    use kin_parser::project_manifest::DeclaredTests;
    kin_parser::project_manifest::all()
        .iter()
        .find_map(|manifest| {
            let text = project.manifest(manifest.name).ok().flatten()?;
            (manifest.declared_tests)(&text)
        })
        .and_then(|declared| match declared {
            DeclaredTests::Runner(name) => {
                parse_runner(name).ok().map(RepositoryTestCommand::Runner)
            }
            DeclaredTests::Command(words) => Some(RepositoryTestCommand::Whole(
                words.iter().map(|word| word.to_string()).collect(),
            )),
        })
}

/// What `POST /verify/work` is asked: how this repository runs its tests,
/// the proof set for a piece of work, or to run that proof set and record it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum WorkVerifyRequest {
    TestCommand,
    Plan(WorkVerifyScope),
    Run(WorkVerifyScope),
}

/// The work whose tests are planned: the entities named, or a recorded
/// change's entities, or, when neither is named, every entity the working
/// copy adds or changes.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkVerifyScope {
    #[serde(default)]
    pub entities: Vec<String>,
    #[serde(default)]
    pub change: Option<String>,
    #[serde(default = "default_work_depth")]
    pub depth: u32,
}

/// The default test reach shared by Code, guided review and work verification.
/// Verification's depth counts intervening consumers, plus the test's final hop.
pub(crate) const WORK_TEST_REACH_DEPTH: u32 = 3;

fn default_work_depth() -> u32 {
    WORK_TEST_REACH_DEPTH - 1
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkVerifyResponse {
    /// Whether this repository declares a test command Kin can run.
    pub runnable: bool,
    /// Why not, in one sentence, when it does not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable: Option<String>,
    /// `targeted` when the command runs only the planned tests, `whole` when
    /// it runs every test the repository has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reach: Option<String>,
    /// The entities the proof set was planned for.
    #[serde(default)]
    pub entities: Vec<WorkEntity>,
    /// The tests planned.
    #[serde(default)]
    pub tests: Vec<WorkTest>,
    /// The run, once one was made and recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<WorkRun>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkEntity {
    pub id: String,
    pub name: String,
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkTest {
    /// A planned graph test, whose individual execution is not attested by
    /// a successful test-command exit alone.
    pub test_id: String,
    pub name: String,
    /// The test entity, when the test is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entity_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkRun {
    pub run_id: String,
    /// `passed` or `failed` for the repository's test command, or
    /// `could_not_run` when it never ran its tests: Kin could not start it,
    /// or the shell could not find or execute the runner it names.
    pub outcome: String,
    pub duration_ms: u64,
    /// The command's exit code, `null` when it has none: it never started,
    /// or a signal ended it.
    #[serde(default)]
    pub exit_code: Option<i32>,
    pub finished_at: String,
    /// Why the command could not run, in one sentence, when it could not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The last lines the command printed, when it did not pass.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tail: Option<String>,
}

/// A piece of work's proof set: one [`VerificationPlan`] per entity, their
/// tests together, and the fingerprints a run of them is tied to.
#[derive(Debug, Clone, Default)]
struct WorkPlan {
    subjects: Vec<Entity>,
    tests: Vec<TestCase>,
    test_entities: HashMap<kin_model::TestId, Entity>,
    proved_entities: Vec<Entity>,
    proved_work_items: Vec<WorkItem>,
    ran_against: Vec<kin_model::RunFingerprint>,
}

impl WorkPlan {
    fn from_plans(plans: Vec<VerificationPlan>) -> Self {
        let mut work = Self::default();
        let mut seen_tests = HashSet::new();
        let mut seen_proved = HashSet::new();
        let mut seen_work = HashSet::new();
        let mut fingerprinted: Vec<&Entity> = Vec::new();
        for plan in &plans {
            work.subjects.push(plan.entity.clone());
            for test in &plan.tests {
                if seen_tests.insert(test.test_id) {
                    work.tests.push(test.clone());
                }
            }
            for (test_id, entity) in &plan.test_entities {
                work.test_entities
                    .entry(*test_id)
                    .or_insert_with(|| entity.clone());
            }
            for entity in &plan.proved_entities {
                if seen_proved.insert(entity.id) {
                    work.proved_entities.push(entity.clone());
                }
            }
            for item in &plan.proved_work_items {
                if seen_work.insert(item.work_id) {
                    work.proved_work_items.push(item.clone());
                }
            }
        }
        for plan in &plans {
            fingerprinted.push(&plan.entity);
            fingerprinted.extend(plan.impacted.iter().map(|slice| &slice.entity));
        }
        fingerprinted.extend(work.test_entities.values());
        let mut seen_fingerprints = HashSet::new();
        for entity in fingerprinted {
            if seen_fingerprints.insert(entity.id) {
                work.ran_against.push(kin_model::RunFingerprint {
                    entity: entity.id,
                    behavior_hash: entity.fingerprint.behavior_hash,
                });
            }
        }
        work.ran_against.sort_by(|a, b| a.entity.cmp(&b.entity));
        work
    }

    /// A selected test can call helpers outside the inbound impact slice.
    /// Include their transitive outgoing dependencies so changing one makes
    /// the result stale while an unrelated entity leaves it current.
    fn capture_dependencies<G>(&mut self, graph: &G) -> Result<()>
    where
        G: GraphStore,
        <G as GraphStore>::Error: std::fmt::Display + Send + Sync + 'static,
    {
        let mut seen: HashSet<_> = self.ran_against.iter().map(|tied| tied.entity).collect();
        let mut queue: std::collections::VecDeque<_> = seen.iter().copied().collect();
        while let Some(id) = queue.pop_front() {
            let relations = graph
                .get_relations(&id, &[])
                .map_err(|error| anyhow!(error.to_string()))?;
            for relation in relations {
                if !kin_review::is_impact_relation(relation.kind) {
                    continue;
                }
                let Some(destination) = relation.dst.as_entity() else {
                    continue;
                };
                if !seen.insert(destination) {
                    continue;
                }
                if let Some(entity) = graph
                    .get_entity(&destination)
                    .map_err(|error| anyhow!(error.to_string()))?
                {
                    self.ran_against.push(kin_model::RunFingerprint {
                        entity: destination,
                        behavior_hash: entity.fingerprint.behavior_hash,
                    });
                    queue.push_back(destination);
                }
            }
        }
        self.ran_against.sort_by_key(|tied| tied.entity);
        Ok(())
    }

    fn response(&self, command: &RepositoryTestCommand) -> WorkVerifyResponse {
        let mut tests: Vec<WorkTest> = self
            .tests
            .iter()
            .map(|test| WorkTest {
                test_id: test.test_id.to_string(),
                name: test.name.clone(),
                entity_id: self
                    .test_entities
                    .get(&test.test_id)
                    .map(|entity| entity.id.to_string()),
            })
            .collect();
        tests.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.test_id.cmp(&b.test_id)));
        WorkVerifyResponse {
            runnable: true,
            unavailable: None,
            reach: Some(command.reach().to_string()),
            entities: self
                .subjects
                .iter()
                .map(|entity| WorkEntity {
                    id: entity.id.to_string(),
                    name: entity.name.clone(),
                    kind: kind_word(entity.kind),
                })
                .collect(),
            tests,
            run: None,
        }
    }
}

/// `TypeAlias` as `type_alias`.
fn kind_word(kind: impl std::fmt::Debug) -> String {
    let debug = format!("{kind:?}");
    let mut word = String::with_capacity(debug.len() + 4);
    for (index, character) in debug.chars().enumerate() {
        if character.is_ascii_uppercase() {
            if index > 0 {
                word.push('_');
            }
            word.push(character.to_ascii_lowercase());
        } else {
            word.push(character);
        }
    }
    word
}

/// The entities a piece of work names, as the graph holds them now.
fn work_subjects(
    authority: &RequestRepositoryAuthority,
    graph: &kin_db::InMemoryGraph,
    scope: &WorkVerifyScope,
) -> Result<Vec<Entity>> {
    let mut ids: Vec<kin_model::EntityId> = Vec::new();
    if !scope.entities.is_empty() {
        for raw in &scope.entities {
            let uuid = uuid::Uuid::parse_str(raw.trim())
                .map_err(|_| anyhow!("'{raw}' is not an entity id"))?;
            ids.push(kin_model::EntityId(uuid));
        }
    } else if let Some(change) = scope.change.as_deref() {
        let change = resolve_change(graph, authority.binding(), Some(change))?;
        for delta in &change.entity_deltas {
            match delta {
                EntityDelta::Added { new: entity } | EntityDelta::Modified { new: entity, .. } => {
                    ids.push(entity.id)
                }
                EntityDelta::Removed { .. } => {
                    bail!("Kin cannot plan tests for removed entities yet.")
                }
            }
        }
    } else {
        // What `kin diff` reads as uncommitted: the entities the working copy
        // adds or edits. A span that only moved is not an edit, and a module
        // is read through its members, as a guide reads them.
        let (report, _) = super::diff::inspect_with_endpoint_entities_at(
            authority.open()?.as_ref(),
            None,
            None,
            Some(graph),
        )?;
        for delta in report.entity_deltas {
            match delta {
                EntityDelta::Added { new } => ids.push(new.id),
                EntityDelta::Modified { old, new } => {
                    if kin_review::is_semantic_modification(&old, &new) {
                        ids.push(new.id);
                    }
                }
                EntityDelta::Removed { .. } => {
                    bail!("Kin cannot plan tests for removed entities yet.")
                }
            }
        }
    }
    let mut seen = HashSet::new();
    let mut subjects = Vec::new();
    for id in ids {
        if !seen.insert(id) {
            continue;
        }
        match graph
            .get_entity(&id)
            .map_err(|err| anyhow!(err.to_string()))?
        {
            Some(entity)
                if kin_model::is_module_container(&entity) && scope.entities.is_empty() => {}
            Some(entity) => subjects.push(entity),
            // A named entity the repository does not hold is refused. One a
            // change or the working copy names that is gone now has nothing
            // left to run against.
            None if !scope.entities.is_empty() => {
                bail!("No entity with the id {id} is in this repository.")
            }
            None => {}
        }
    }
    Ok(subjects)
}

fn plan_work(
    authority: &RequestRepositoryAuthority,
    graph: &kin_db::InMemoryGraph,
    scope: &WorkVerifyScope,
) -> Result<WorkPlan> {
    let plans = work_subjects(authority, graph, scope)?
        .into_iter()
        .map(|entity| build_verification_plan_for_entity(graph, entity, scope.depth))
        .collect::<Result<Vec<_>>>()?;
    let mut work = WorkPlan::from_plans(plans);
    work.capture_dependencies(graph)?;
    Ok(work)
}

/// A piece of work's proof set, planned and ready to run, or the answer that
/// says why nothing will run.
pub enum PreparedWork {
    /// Nothing runs: no command, or no test reaches the work. The answer is
    /// final.
    Answered(WorkVerifyResponse),
    Ready(ReadyWork),
}

/// A planned proof set and the command line that runs it.
pub struct ReadyWork {
    work: WorkPlan,
    command: RepositoryTestCommand,
    line: String,
}

impl ReadyWork {
    pub fn command_line(&self) -> &str {
        &self.line
    }

    pub fn tests_planned(&self) -> usize {
        self.work.tests.len()
    }
}

/// Answer `POST /verify/work` up to the point a command would run: the test
/// command, the plan, or a plan ready to run. Reads the graph only.
pub fn prepare_work_verify(
    authority: &RequestRepositoryAuthority,
    graph: &kin_db::InMemoryGraph,
    project: &dyn kin_parser::project_manifest::ProjectFiles,
    request: &WorkVerifyRequest,
) -> Result<PreparedWork> {
    let Some(command) = repository_test_command(project) else {
        return Ok(PreparedWork::Answered(WorkVerifyResponse {
            runnable: false,
            unavailable: Some(NO_TEST_COMMAND.to_string()),
            ..WorkVerifyResponse::default()
        }));
    };
    let (scope, run) = match request {
        WorkVerifyRequest::TestCommand => {
            return Ok(PreparedWork::Answered(WorkVerifyResponse {
                runnable: true,
                reach: Some(command.reach().to_string()),
                ..WorkVerifyResponse::default()
            }))
        }
        WorkVerifyRequest::Plan(scope) => (scope, false),
        WorkVerifyRequest::Run(scope) => (scope, true),
    };
    let work = plan_work(authority, graph, scope)?;
    if !run || work.tests.is_empty() {
        return Ok(PreparedWork::Answered(work.response(&command)));
    }
    let line = command.command_line(&work.tests);
    Ok(PreparedWork::Ready(ReadyWork {
        work,
        command,
        line,
    }))
}

/// The records of a finished run of `ready`, and what it answers once they
/// are durable.
pub fn record_work_verify(
    authority: &RequestRepositoryAuthority,
    graph: &kin_db::InMemoryGraph,
    ready: &ReadyWork,
    executed: &ExecutedRun,
) -> Result<(VerificationRecord, WorkVerifyResponse)> {
    let record = plan_verification_record(
        authority,
        graph,
        &ready.work,
        ready.command.runner(),
        executed,
        "verify.work",
        ready
            .work
            .subjects
            .first()
            .map(|entity| WorkScope::Entity(entity.id)),
        |run| {
            format!(
                "reach={}; status={}; tests={}; entities={}",
                ready.command.reach(),
                audit_status(run),
                run.test_ids.len(),
                ready.work.subjects.len()
            )
        },
    )?;
    let mut response = ready.work.response(&ready.command);
    response.run = Some(WorkRun {
        run_id: record.run.run_id.to_string(),
        outcome: executed.outcome().to_string(),
        duration_ms: executed.duration_ms,
        exit_code: executed.exit_code,
        finished_at: rfc3339(&executed.finished_at),
        reason: executed.could_not_run.clone(),
        output_tail: (executed.status != VerificationStatus::Passing
            && !executed.output_tail.is_empty())
        .then(|| executed.output_tail.clone()),
    });
    Ok((record, response))
}

/// A recorded run's status in words, as `kin verify` prints it.
fn run_word(run: &VerificationRun) -> String {
    if run.could_not_run() {
        "could not run".to_string()
    } else {
        run.status.to_string()
    }
}

/// A recorded run's status as its audit event names it.
fn audit_status(run: &VerificationRun) -> String {
    if run.could_not_run() {
        "could_not_run".to_string()
    } else {
        run.status.to_string()
    }
}

fn rfc3339(at: &Timestamp) -> String {
    at.0.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

// ---------------------------------------------------------------------------
// A test's latest result, for the work as it is now
// ---------------------------------------------------------------------------

/// `passed`, `failed`, `not_run` or `stale`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestResultState {
    Passed,
    Failed,
    NotRun,
    Stale,
}

/// A test's latest result for the work as it is now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestResultReading {
    pub state: TestResultState,
    /// When the run this result comes from finished.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

/// Whether `run` is a result for the graph as it is now: it was tied to
/// `subject` when there is one, and every entity it was tied to still has
/// the fingerprint it ran against. A run tied to nothing is never current.
fn run_is_current<G>(
    graph: &G,
    run: &VerificationRun,
    subject: Option<&kin_model::EntityId>,
    now: &mut HashMap<kin_model::EntityId, Option<Hash256>>,
) -> Result<bool>
where
    G: GraphStore,
    <G as GraphStore>::Error: std::fmt::Display + Send + Sync + 'static,
{
    if run.ran_against.is_empty() {
        return Ok(false);
    }
    if let Some(subject) = subject {
        if !run.ran_against.iter().any(|tied| tied.entity == *subject) {
            return Ok(false);
        }
    }
    for tied in &run.ran_against {
        let current = match now.get(&tied.entity) {
            Some(current) => *current,
            None => {
                let current = graph
                    .get_entity(&tied.entity)
                    .map_err(|err| anyhow!(err.to_string()))?
                    .map(|entity| entity.fingerprint.behavior_hash);
                now.insert(tied.entity, current);
                current
            }
        };
        if current != Some(tied.behavior_hash) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn finished_at(run: &VerificationRun) -> &Timestamp {
    run.finished_at.as_ref().unwrap_or(&run.started_at)
}

/// The latest result of `test` for `subject` as the graph holds both now.
///
/// The newest run of the test that is current ([`run_is_current`]) and ran
/// gives `passed` or `failed`. A run whose command could not run attests no
/// outcome, so it never gives one: when no current run ran, the newest
/// current attempt that could not run makes the test `not_run`, with why.
/// When runs of it exist and none is current, the result is `stale`: it was
/// recorded against an older fingerprint of an entity it was tied to, and is
/// never shown as current. A test no run reached is `not_run`.
pub fn test_result<G>(
    graph: &G,
    test: &kin_model::EntityId,
    subject: Option<&kin_model::EntityId>,
    now: &mut HashMap<kin_model::EntityId, Option<Hash256>>,
) -> Result<TestResultReading>
where
    G: GraphStore,
    <G as GraphStore>::Error: std::fmt::Display + Send + Sync + 'static,
{
    let mut runs = graph
        .list_runs_for_test(&test_id_for_entity(test))
        .map_err(|err| anyhow!(err.to_string()))?;
    let ran = |run: &VerificationRun| {
        matches!(
            run.status,
            VerificationStatus::Passing | VerificationStatus::Failing
        )
    };
    runs.retain(|run| ran(run) || run.could_not_run());
    runs.sort_by(|a, b| {
        finished_at(b)
            .cmp(finished_at(a))
            .then_with(|| b.run_id.0.cmp(&a.run_id.0))
    });
    let mut attempt: Option<&VerificationRun> = None;
    for run in &runs {
        if !run_is_current(graph, run, subject, now)? {
            continue;
        }
        if run.could_not_run() {
            attempt.get_or_insert(run);
            continue;
        }
        let passed = run.status == VerificationStatus::Passing;
        // A run that passed passed every test in it. One that failed says
        // which test failed only when it ran this test alone through a runner
        // Kin narrowed to it.
        let alone = run.test_ids.len() == 1 && !matches!(run.runner, TestRunner::Custom(_));
        let summary = if passed {
            if alone {
                "Passed."
            } else {
                "The test command passed; Kin does not report individual test outcomes."
            }
        } else if alone {
            "Failed."
        } else {
            "The run that included it failed, and Kin cannot tell which test in that run failed."
        };
        return Ok(TestResultReading {
            state: if passed {
                TestResultState::Passed
            } else {
                TestResultState::Failed
            },
            at: Some(rfc3339(finished_at(run))),
            duration_ms: run.duration_ms,
            summary: Some(summary.to_string()),
        });
    }
    if let Some(attempt) = attempt {
        let exit = attempt
            .exit_code
            .map(|code| format!(" It exited {code}."))
            .unwrap_or_default();
        return Ok(TestResultReading {
            state: TestResultState::NotRun,
            at: Some(rfc3339(finished_at(attempt))),
            duration_ms: attempt.duration_ms,
            summary: Some(format!(
                "The test command could not run, so this test did not run.{exit}"
            )),
        });
    }
    let Some(newest) = runs.iter().find(|run| ran(run)) else {
        return Ok(TestResultReading {
            state: TestResultState::NotRun,
            at: None,
            duration_ms: None,
            summary: None,
        });
    };
    let was = if newest.status == VerificationStatus::Passing {
        "passed"
    } else {
        "failed"
    };
    Ok(TestResultReading {
        state: TestResultState::Stale,
        at: Some(rfc3339(finished_at(newest))),
        duration_ms: newest.duration_ms,
        summary: Some(format!(
            "Its last run {was}, and an entity it ran against has changed since."
        )),
    })
}

#[cfg(unix)]
fn verification_shell_program() -> &'static str {
    // POSIX systems provide /bin/sh; resolving through PATH makes verification
    // fail under otherwise valid restricted-path environments.
    "/bin/sh"
}

#[cfg(not(unix))]
fn verification_shell_program() -> &'static str {
    "sh"
}

fn verification_command(working_dir: &std::path::Path, command: &str) -> std::process::Command {
    let mut shell = std::process::Command::new(verification_shell_program());
    shell.current_dir(working_dir).arg("-c").arg(command);
    shell
}

#[derive(Debug, Clone)]
struct VerificationPlan {
    entity: Entity,
    depth: u32,
    direct: EntityProofSlice,
    impacted: Vec<EntityProofSlice>,
    tests: Vec<TestCase>,
    /// The test entity each planned test is, for the tests that are one.
    test_entities: HashMap<kin_model::TestId, Entity>,
    latest_test_runs: HashMap<kin_model::TestId, VerificationRun>,
    proved_entities: Vec<Entity>,
    proved_work_items: Vec<WorkItem>,
}

#[derive(Debug, Clone)]
struct EntityProofSlice {
    entity: Entity,
    tests: Vec<TestCase>,
    /// The test entities among `tests`, each with the test it is recorded as.
    test_entities: Vec<(kin_model::TestId, Entity)>,
    work_items: Vec<WorkItem>,
}

#[derive(Debug, Clone)]
struct ChangeVerificationPlan {
    change: SemanticChange,
    depth: u32,
    entity_plans: Vec<VerificationPlan>,
    tests: Vec<TestCase>,
    latest_test_runs: HashMap<kin_model::TestId, VerificationRun>,
    removed_entities: Vec<kin_model::EntityId>,
}

fn build_verification_plan<G>(graph: &G, entity_query: &str, depth: u32) -> Result<VerificationPlan>
where
    G: GraphStore,
    <G as GraphStore>::Error: std::fmt::Display + Send + Sync + 'static,
{
    let entity = resolve_entity(graph, entity_query)?;
    build_verification_plan_for_entity(graph, entity, depth)
}

fn build_verification_plan_for_entity<G>(
    graph: &G,
    entity: Entity,
    depth: u32,
) -> Result<VerificationPlan>
where
    G: GraphStore,
    <G as GraphStore>::Error: std::fmt::Display + Send + Sync + 'static,
{
    let mut direct = build_entity_proof_slice(graph, entity.clone())?;
    // A test beyond an impacted consumer adds one final hop. This is the
    // same recorded impact traversal Code and guided review read, including
    // its relation policy and maximum depth.
    let ranked = kin_review::rank_impact(graph, &entity.id, depth.saturating_add(1))?;
    let mut impacted = Vec::new();
    for candidate in ranked
        .candidates
        .iter()
        .filter(|candidate| candidate.hop <= depth)
    {
        if let Some(impacted_entity) = graph
            .get_entity(&candidate.entity_id)
            .map_err(|error| anyhow!(error.to_string()))?
        {
            impacted.push(build_entity_proof_slice(graph, impacted_entity)?);
        }
    }

    // The tests the walk counts, one per test: test code inside a test
    // module is planned as the module that holds it, never as a test itself.
    let ranked_test_entities: HashSet<_> = ranked.tests.iter().map(|test| test.entity_id).collect();
    for slice in std::iter::once(&mut direct).chain(impacted.iter_mut()) {
        let rejected: HashSet<_> = slice
            .test_entities
            .iter()
            .filter(|(_, entity)| !ranked_test_entities.contains(&entity.id))
            .map(|(test, _)| *test)
            .collect();
        slice.tests.retain(|test| !rejected.contains(&test.test_id));
        slice
            .test_entities
            .retain(|(_, entity)| ranked_test_entities.contains(&entity.id));
    }

    let mut seen_test_ids = HashSet::new();
    let mut tests = Vec::new();
    let mut test_entities = HashMap::new();
    for slice in std::iter::once(&direct).chain(impacted.iter()) {
        for test in &slice.tests {
            if seen_test_ids.insert(test.test_id) {
                tests.push(test.clone());
            }
        }
        for (test_id, entity) in &slice.test_entities {
            test_entities
                .entry(*test_id)
                .or_insert_with(|| entity.clone());
        }
    }

    for reach in &ranked.tests {
        let Some(test_entity) = graph
            .get_entity(&reach.entity_id)
            .map_err(|error| anyhow!(error.to_string()))?
        else {
            continue;
        };
        let test = test_case_for_entity(&test_entity, &entity.id);
        if seen_test_ids.insert(test.test_id) {
            tests.push(test.clone());
        }
        test_entities.entry(test.test_id).or_insert(test_entity);
    }

    let mut proved_entities = Vec::new();
    let mut seen_entity_ids = HashSet::new();
    if seen_entity_ids.insert(entity.id) {
        proved_entities.push(entity.clone());
    }
    if !direct.tests.is_empty() && seen_entity_ids.insert(direct.entity.id) {
        proved_entities.push(direct.entity.clone());
    }
    for slice in &impacted {
        if !slice.tests.is_empty() && seen_entity_ids.insert(slice.entity.id) {
            proved_entities.push(slice.entity.clone());
        }
    }

    let mut proved_work_items = Vec::new();
    let mut seen_work_ids = HashSet::new();
    let selected_test_ids = tests
        .iter()
        .map(|test| test.test_id)
        .collect::<HashSet<_>>();
    for slice in std::iter::once(&direct).chain(impacted.iter()) {
        for work in &slice.work_items {
            let verifies = graph
                .get_tests_verifying_work(&work.work_id)
                .map_err(|err| anyhow!(err.to_string()))?;
            if verifies
                .iter()
                .any(|linked_test| selected_test_ids.contains(&linked_test.test_id))
                && seen_work_ids.insert(work.work_id)
            {
                proved_work_items.push(work.clone());
            }
        }
    }
    let latest_test_runs = build_latest_test_runs(graph, &tests)?;

    Ok(VerificationPlan {
        entity,
        depth,
        direct,
        impacted,
        tests,
        test_entities,
        latest_test_runs,
        proved_entities,
        proved_work_items,
    })
}

fn build_change_verification_plan<G>(
    graph: &G,
    change: &SemanticChange,
    depth: u32,
) -> Result<ChangeVerificationPlan>
where
    G: GraphStore,
    <G as GraphStore>::Error: std::fmt::Display + Send + Sync + 'static,
{
    let mut entity_plans = Vec::new();
    let mut tests = Vec::new();
    let mut seen_test_ids = HashSet::new();
    let mut removed_entities = Vec::new();

    for delta in &change.entity_deltas {
        match delta {
            EntityDelta::Added { new: entity } => {
                let plan = build_verification_plan_for_entity(graph, entity.clone(), depth)?;
                for test in &plan.tests {
                    if seen_test_ids.insert(test.test_id) {
                        tests.push(test.clone());
                    }
                }
                entity_plans.push(plan);
            }
            EntityDelta::Modified { new, .. } => {
                let plan = build_verification_plan_for_entity(graph, new.clone(), depth)?;
                for test in &plan.tests {
                    if seen_test_ids.insert(test.test_id) {
                        tests.push(test.clone());
                    }
                }
                entity_plans.push(plan);
            }
            EntityDelta::Removed { old } => removed_entities.push(old.id),
        }
    }
    let latest_test_runs = build_latest_test_runs(graph, &tests)?;

    Ok(ChangeVerificationPlan {
        change: change.clone(),
        depth,
        entity_plans,
        tests,
        latest_test_runs,
        removed_entities,
    })
}

fn build_latest_test_runs<G>(
    graph: &G,
    tests: &[TestCase],
) -> Result<HashMap<kin_model::TestId, VerificationRun>>
where
    G: GraphStore,
    <G as GraphStore>::Error: std::fmt::Display + Send + Sync + 'static,
{
    let mut latest = HashMap::new();
    for test in tests {
        if let Some(run) = latest_run_for_test(graph, test.test_id)? {
            latest.insert(test.test_id, run);
        }
    }
    Ok(latest)
}

fn latest_run_for_test<G>(graph: &G, test_id: kin_model::TestId) -> Result<Option<VerificationRun>>
where
    G: GraphStore,
    <G as GraphStore>::Error: std::fmt::Display + Send + Sync + 'static,
{
    let runs = graph
        .list_runs_for_test(&test_id)
        .map_err(|err| anyhow!(err.to_string()))?;
    Ok(runs.into_iter().max_by(|left, right| {
        let left_finished = left
            .finished_at
            .clone()
            .unwrap_or_else(|| left.started_at.clone());
        let right_finished = right
            .finished_at
            .clone()
            .unwrap_or_else(|| right.started_at.clone());
        left_finished.cmp(&right_finished)
    }))
}

fn build_entity_proof_slice<G>(graph: &G, entity: Entity) -> Result<EntityProofSlice>
where
    G: GraphStore,
    <G as GraphStore>::Error: std::fmt::Display + Send + Sync + 'static,
{
    let mut tests = graph
        .get_tests_for_entity(&entity.id)
        .map_err(|err| anyhow!(err.to_string()))?;
    let work_items = graph
        .get_work_for_scope(&WorkScope::Entity(entity.id))
        .map_err(|err| anyhow!(err.to_string()))?;
    let mut seen_test_ids = tests
        .iter()
        .map(|test| test.test_id)
        .collect::<HashSet<_>>();
    for work in &work_items {
        let linked_tests = graph
            .get_tests_verifying_work(&work.work_id)
            .map_err(|err| anyhow!(err.to_string()))?;
        for linked_test in linked_tests {
            if seen_test_ids.insert(linked_test.test_id) {
                tests.push(linked_test);
            }
        }
    }
    // The test entities that reach it, which a guide lists as its tests, are
    // part of its proof as much as a recorded test case is.
    let mut test_entities = Vec::new();
    for test in tests_reaching(graph, &entity.id)? {
        let case = test_case_for_entity(&test, &entity.id);
        if seen_test_ids.insert(case.test_id) {
            test_entities.push((case.test_id, test));
            tests.push(case);
        }
    }

    Ok(EntityProofSlice {
        entity,
        tests,
        test_entities,
        work_items,
    })
}

fn verification_plan_lines(plan: &VerificationPlan) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push(format!(
        "Targeted proof plan for {} ({:?})",
        plan.entity.name, plan.entity.kind
    ));
    lines.push(format!("  Impact depth: {}", plan.depth));
    lines.push(format!(
        "  Test reach: up to {} recorded hops",
        plan.depth
            .saturating_add(1)
            .min(kin_review::IMPACT_MAX_DEPTH)
    ));
    lines.push(format!("  Planned entities: {}", 1 + plan.impacted.len()));
    lines.push(format!(
        "  Direct scope: {} test(s), {} work item(s)",
        plan.direct.tests.len(),
        plan.direct.work_items.len()
    ));

    if !plan.impacted.is_empty() {
        lines.push("  Impacted dependents:".to_string());
        for slice in &plan.impacted {
            lines.push(format!(
                "    - {} ({:?}): {} test(s), {} work item(s)",
                slice.entity.name,
                slice.entity.kind,
                slice.tests.len(),
                slice.work_items.len()
            ));
        }
    }

    lines.push(format!(
        "  Selected proof set: {} test(s)",
        plan.tests.len()
    ));
    for test in &plan.tests {
        let latest = plan
            .latest_test_runs
            .get(&test.test_id)
            .map(run_word)
            .unwrap_or_else(|| "missing".to_string());
        lines.push(format!(
            "    - {} [{}] runner={} latest={}",
            test.name, test.kind, test.runner, latest
        ));
    }

    if !plan.proved_work_items.is_empty() {
        lines.push("  Linked work items:".to_string());
        for work in &plan.proved_work_items {
            lines.push(format!("    - {} ({})", work.title, work.work_id));
        }
    }
    lines
}

fn change_verification_plan_lines(plan: &ChangeVerificationPlan) -> Vec<String> {
    let mut lines = vec![
        format!("Targeted proof plan for change {}", plan.change.id),
        format!("  Message: {}", plan.change.message),
        format!("  Impact depth: {}", plan.depth),
        format!("  Changed entities planned: {}", plan.entity_plans.len()),
        format!("  Selected proof set: {} test(s)", plan.tests.len()),
    ];
    if !plan.removed_entities.is_empty() {
        lines.push(format!(
            "  Removed entities without active proof graph state: {}",
            plan.removed_entities.len()
        ));
        for entity_id in &plan.removed_entities {
            lines.push(format!("    - {}", entity_id));
        }
    }
    if !plan.entity_plans.is_empty() {
        lines.push("  Entity plans:".to_string());
        for entity_plan in &plan.entity_plans {
            lines.push(format!(
                "    - {} ({:?}): {} selected test(s), {} impacted dependents",
                entity_plan.entity.name,
                entity_plan.entity.kind,
                entity_plan.tests.len(),
                entity_plan.impacted.len()
            ));
        }
    }
    if !plan.tests.is_empty() {
        lines.push("  Proof tests:".to_string());
        for test in &plan.tests {
            let latest = plan
                .latest_test_runs
                .get(&test.test_id)
                .map(run_word)
                .unwrap_or_else(|| "missing".to_string());
            lines.push(format!(
                "    - {} [{}] runner={} latest={}",
                test.name, test.kind, test.runner, latest
            ));
        }
    }
    lines
}

fn resolve_change<G>(
    graph: &G,
    binding: &kin_core::LocalRepositoryAuthorityBinding,
    change_id: Option<&str>,
) -> Result<SemanticChange>
where
    G: GraphStore,
    <G as GraphStore>::Error: std::fmt::Display + Send + Sync + 'static,
{
    let change_id = match change_id {
        Some(hash) => parse_change_id(hash)?,
        None => crate::commands::repository_authority::ActiveRepositoryAuthority::open(binding)?
            .current_change_id()?
            .ok_or_else(|| {
                anyhow!(
                    "this workspace has no commits yet, so there is nothing to verify; make one \
                     with `kin commit`"
                )
            })?,
    };

    graph
        .get_change(&change_id)
        .map_err(|err| anyhow!(err.to_string()))?
        .ok_or_else(|| anyhow!("change {} not found", change_id))
}

fn parse_change_id(input: &str) -> Result<SemanticChangeId> {
    Ok(SemanticChangeId::from_hash(
        Hash256::from_hex(input).map_err(|err| anyhow!("invalid change hash: {}", err))?,
    ))
}

/// The line `kin verify` answers with when its entity query matches nothing.
///
/// Public so a caller that recognises this answer compares it with the one
/// producer rather than a copy of its wording.
pub fn entity_not_found_line(entity_query: &str) -> String {
    format!("No entity matching '{}' found.", entity_query)
}

/// A `kin verify` plan or run refused because its entity query named no one
/// entity: nothing matches it, or several do.
///
/// Typed so the daemon can hold this refusal back while an edit admission may
/// be adding the entity the query names, and serve every other failure as it
/// always did.
#[derive(Debug)]
pub struct VerifyEntityUnresolved(String);

impl std::fmt::Display for VerifyEntityUnresolved {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for VerifyEntityUnresolved {}

/// Whether a plan or run for `entity_query` names exactly one entity in
/// `graph`, by the rule [`prepare_verify_run`] resolves it with.
///
/// Public for the daemon, which asks before a run takes the coordination gate.
/// A run holds its own writer guard while it resolves, so it cannot tell from
/// the authority clock whether an edit admission was adding the entity.
pub fn verify_query_names_one_entity<G>(graph: &G, entity_query: &str) -> Result<bool>
where
    G: GraphStore,
    <G as GraphStore>::Error: std::fmt::Display + Send + Sync + 'static,
{
    Ok(verify_query_matches(graph, entity_query)?.len() == 1)
}

fn verify_query_matches<G>(graph: &G, entity_query: &str) -> Result<Vec<Entity>>
where
    G: GraphStore,
    <G as GraphStore>::Error: std::fmt::Display + Send + Sync + 'static,
{
    // An entity id names exactly one entity, so it is read as one before the
    // query is read as a name.
    if let Ok(id) = uuid::Uuid::parse_str(entity_query.trim()).map(kin_model::EntityId) {
        if let Some(entity) = graph
            .get_entity(&id)
            .map_err(|err| anyhow!(err.to_string()))?
        {
            return Ok(vec![entity]);
        }
    }
    let filter = EntityFilter {
        name_pattern: Some(entity_query.to_string()),
        ..Default::default()
    };
    graph
        .query_entities(&filter)
        .map_err(|err| anyhow!(err.to_string()))
}

fn resolve_entity<G>(graph: &G, entity_query: &str) -> Result<Entity>
where
    G: GraphStore,
    <G as GraphStore>::Error: std::fmt::Display + Send + Sync + 'static,
{
    // A plan or a run is refused for a symbol outside the repository the way
    // the entity report is, and not as an unresolved query, which the daemon
    // would wait out as an absence.
    if let Some(node) = crate::commands::external_symbols::lookup(graph, entity_query)? {
        anyhow::bail!(
            crate::commands::external_symbols::refusal_lines(&node, VERIFY_EXTERNAL_WHY).join("\n")
        );
    }
    let entities = verify_query_matches(graph, entity_query)?;

    match entities.as_slice() {
        [] => Err(anyhow::Error::new(VerifyEntityUnresolved(
            entity_not_found_line(entity_query),
        ))),
        [entity] => Ok(entity.clone()),
        many => {
            let preview = many
                .iter()
                .take(5)
                .map(|entity| entity.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            Err(anyhow::Error::new(VerifyEntityUnresolved(format!(
                "Multiple entities match '{}': {}. Use a more exact name.",
                entity_query, preview
            ))))
        }
    }
}

fn parse_runner(runner: &str) -> Result<TestRunner> {
    match runner {
        "cargo" => Ok(TestRunner::Cargo),
        "jest" => Ok(TestRunner::Jest),
        "pytest" => Ok(TestRunner::Pytest),
        "go" => Ok(TestRunner::Go),
        "junit" => Ok(TestRunner::JUnit),
        other => {
            if let Some(refusal) = custom_runner_refusal(other) {
                bail!("{refusal}");
            }
            Ok(TestRunner::Custom(other.to_string()))
        }
    }
}

/// Characters a custom runner may carry, and why the set is this short.
///
/// A runner name arrives on the `POST /verify/run` body, and the command this
/// module builds from it is handed to `/bin/sh -c`. Every character a shell
/// reads as syntax is absent here on purpose: `;`, `&`, `|`, `$`, backtick,
/// `(`, `)`, `<`, `>`, `\`, quotes and newline. What is left still spells the
/// runners this is for, `npm test`, `env FOO=1 pytest`, `./scripts/test.sh`,
/// because a runner is a program and its flags rather than a shell script.
const CUSTOM_RUNNER_EXTRA_CHARS: &str = " ._-/=+:,@";

/// Why this runner cannot be run, in the caller's terms, or `None` when it can.
///
/// Paired with the quoting in [`build_runner_command`] rather than trusted
/// alone. Either one closes the injection; both are here so that a later edit
/// to one of them cannot silently reopen it.
fn custom_runner_refusal(runner: &str) -> Option<String> {
    if runner.trim().is_empty() {
        return Some(
            "refusing an empty test runner: name one of cargo, jest, pytest, go, junit, or a \
             program to run"
                .to_string(),
        );
    }
    let bad: Vec<char> = runner
        .chars()
        .filter(|c| !c.is_ascii_alphanumeric() && !CUSTOM_RUNNER_EXTRA_CHARS.contains(*c))
        .collect();
    if bad.is_empty() {
        return None;
    }
    let rendered = bad
        .iter()
        .map(|c| format!("{c:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!(
        "refusing the test runner {runner:?}: it carries {rendered}, which a shell reads as \
         syntax rather than as part of a program name. A custom runner may carry letters, \
         digits and{CUSTOM_RUNNER_EXTRA_CHARS}. Put anything else in a script and name the script."
    ))
}

fn build_runner_command(test_runner: &TestRunner, entity_name: &str, tests: &[TestCase]) -> String {
    match test_runner {
        TestRunner::Cargo => match tests {
            [] => format!("cargo test {}", shell_quote(entity_name)),
            [test] => format!("cargo test {}", shell_quote(&test.name)),
            // The test harness takes any number of name filters after `--`.
            many => {
                let mut names: Vec<&str> = many.iter().map(|test| test.name.as_str()).collect();
                names.sort_unstable();
                names.dedup();
                let filters = names
                    .iter()
                    .map(|name| shell_quote(name))
                    .collect::<Vec<_>>()
                    .join(" ");
                format!("cargo test -- {filters}")
            }
        },
        TestRunner::Jest => {
            let pattern = if tests.is_empty() {
                entity_name.to_string()
            } else {
                tests
                    .iter()
                    .map(|test| test.name.as_str())
                    .collect::<Vec<_>>()
                    .join("|")
            };
            format!("npx jest --testNamePattern={}", shell_quote(&pattern))
        }
        TestRunner::Pytest => {
            let pattern = if tests.is_empty() {
                entity_name.to_string()
            } else {
                tests
                    .iter()
                    .map(|test| test.name.as_str())
                    .collect::<Vec<_>>()
                    .join(" or ")
            };
            format!("pytest -k {}", shell_quote(&pattern))
        }
        TestRunner::Go => {
            let pattern = if tests.is_empty() {
                entity_name.to_string()
            } else {
                tests
                    .iter()
                    .map(|test| test.name.as_str())
                    .collect::<Vec<_>>()
                    .join("|")
            };
            // Every package of the module, as the module's own `go test ./...`
            // would run them, narrowed to the planned tests.
            format!("go test -run {} ./...", shell_quote(&pattern))
        }
        TestRunner::JUnit => {
            let pattern = if tests.is_empty() {
                entity_name.to_string()
            } else {
                tests
                    .iter()
                    .map(|test| test.name.as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            };
            format!("mvn test -Dtest={}", shell_quote(&pattern))
        }
        // Quoted word by word, exactly as every arm above quotes its one
        // interpolated value. A custom runner is a program and its flags, so
        // each word is one argv slot and nothing in it reaches the shell as
        // syntax. `parse_runner` has already refused the characters that would
        // matter; this is the second of the two defences, and it is the one
        // that holds if a future caller reaches here without the first.
        TestRunner::Custom(command) => {
            let quoted = command
                .split_whitespace()
                .map(shell_quote)
                .collect::<Vec<_>>()
                .join(" ");
            format!("{} {}", quoted, shell_quote(entity_name))
        }
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

/// Keep `evidence_text` as an immutable blob, through the authority the
/// request already holds. A server hands every request one open per
/// publication, so recording a run no longer re-verifies the whole store for
/// one small blob.
fn store_evidence_blob(
    authority: &RequestRepositoryAuthority,
    evidence_text: &str,
) -> Option<Hash256> {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(evidence_text.as_bytes());
    let result = hasher.finalize();
    let mut bytes = [0u8; 32];
    bytes.copy_from_slice(&result);
    let hash = Hash256::from_bytes(bytes);

    authority
        .open()
        .ok()?
        .save_source_blob(hash, evidence_text.as_bytes())
        .ok()?;
    Some(hash)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kin_model::{
        AuthorId, ChangeOrigin, ChangeStore, EntityId, EntityKind, EntityMetadata, EntityRole,
        FilePathId, FingerprintAlgorithm, GraphNodeId, IdentityRef, LanguageId, Priority,
        ProvenanceStore, Relation, RelationId, RelationKind, RelationOrigin, SemanticFingerprint,
        TestKind, Visibility, WorkStatus, WorkStore,
    };
    fn make_entity(name: &str, file: &str) -> Entity {
        Entity {
            id: EntityId::new(),
            kind: EntityKind::Function,
            name: name.to_string(),
            language: LanguageId::Rust,
            fingerprint: SemanticFingerprint {
                algorithm: FingerprintAlgorithm::V1TreeSitter,
                ast_hash: Hash256::from_bytes([0; 32]),
                signature_hash: Hash256::from_bytes([1; 32]),
                behavior_hash: Hash256::from_bytes([2; 32]),
                equivalence_hash: kin_model::Hash256::from_bytes([0; 32]),
                stability_score: 1.0,
            },
            file_origin: Some(FilePathId::new(file)),
            span: None,
            signature: format!("fn {name}()"),
            visibility: Visibility::Public,
            role: EntityRole::Source,
            doc_summary: None,
            metadata: EntityMetadata::default(),
            lineage_parent: None,
            created_in: None,
            superseded_by: None,
        }
    }

    #[cfg(unix)]
    #[test]
    fn verification_shell_does_not_depend_on_path_lookup() {
        let dir = tempfile::tempdir().unwrap();
        let empty_path = dir.path().join("empty-path");
        std::fs::create_dir(&empty_path).unwrap();

        let output = verification_command(dir.path(), "printf path-independent")
            .env("PATH", &empty_path)
            .output()
            .unwrap();

        assert!(output.status.success());
        assert_eq!(output.stdout, b"path-independent");
    }

    #[test]
    fn parse_runner_refuses_a_runner_carrying_shell_syntax() {
        // The exact shape a `POST /verify/run` body can carry: a runner field
        // whose job is to end the runner word and start a second command.
        let refusal = parse_runner("notarealrunner; touch pwned")
            .unwrap_err()
            .to_string();
        assert!(
            refusal.contains("refusing the test runner"),
            "the refusal must name itself: {refusal}"
        );
        assert!(
            refusal.contains("';'"),
            "the refusal must name the character it rejected: {refusal}"
        );

        for hostile in [
            "pytest && curl http://example.invalid",
            "pytest | sh",
            "pytest $(id)",
            "pytest `id`",
            "pytest\nid",
            "pytest > /tmp/out",
        ] {
            assert!(
                parse_runner(hostile).is_err(),
                "a runner carrying shell syntax must be refused: {hostile:?}"
            );
        }

        // The runners this route is actually for still parse.
        assert!(matches!(parse_runner("cargo").unwrap(), TestRunner::Cargo));
        assert!(matches!(
            parse_runner("env /bin/echo").unwrap(),
            TestRunner::Custom(_)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_custom_runner_cannot_reach_the_shell_through_the_built_command() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("pwned");
        let hostile = format!("notarealrunner; touch {}", marker.display());

        // The quoting layer on its own, reached the way it would be if a later
        // caller built a command without going through `parse_runner`.
        let command = build_runner_command(&TestRunner::Custom(hostile), "Entity", &[]);
        assert!(
            !command.contains("; touch"),
            "the runner field reached the shell line unquoted: {command}"
        );

        let _ = verification_command(dir.path(), &command).output().unwrap();
        assert!(
            !marker.exists(),
            "the runner field executed a second command: {} exists",
            marker.display()
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_multi_word_custom_runner_still_runs_its_program() {
        let dir = tempfile::tempdir().unwrap();
        let runner = parse_runner("env /bin/echo").unwrap();
        let command = build_runner_command(&runner, "Entity", &[]);

        let output = verification_command(dir.path(), &command).output().unwrap();
        assert!(
            output.status.success(),
            "a legitimate multi-word runner must still run: {command}"
        );
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "Entity");
    }

    #[tokio::test]
    async fn run_verification_persists_targeted_run_and_links_work() {
        let dir = tempfile::tempdir().unwrap();
        kin_core::init(dir.path()).unwrap();
        let layout = kin_core::KinLayout::discover(dir.path()).unwrap();
        let binding = RequestRepositoryAuthority::pinned(
            kin_core::LocalRepositoryAuthorityBinding::from_layout(&layout).unwrap(),
        );
        let snap = crate::backend::open_kindb_snapshot(&layout).unwrap();
        let graph = snap.graph();

        let entity = make_entity("checkout", "src/checkout.rs");
        graph.upsert_entity(&entity).unwrap();

        let work = WorkItem {
            work_id: kin_model::WorkId::new(),
            kind: kin_model::WorkKind::Feature,
            title: "Implement checkout".into(),
            description: "Ship checkout flow".into(),
            status: WorkStatus::InProgress,
            priority: Priority::High,
            scopes: vec![WorkScope::Entity(entity.id)],
            acceptance_criteria: vec!["passing checkout proof".into()],
            external_refs: vec![],
            created_by: IdentityRef::human("cli-user"),
            created_at: Timestamp::now(),
        };
        graph.create_work_item(&work).unwrap();

        let test = TestCase {
            test_id: kin_model::TestId::new(),
            name: "test_checkout_flow".into(),
            language: "rust".into(),
            kind: TestKind::Unit,
            scopes: vec![WorkScope::Entity(entity.id)],
            runner: TestRunner::Cargo,
            file_origin: Some(FilePathId::new("tests/checkout.rs")),
        };
        graph.create_test_case(&test).unwrap();
        graph
            .create_test_verifies_work(&test.test_id, &work.work_id)
            .unwrap();
        snap.save().unwrap();

        let prepared = prepare_verify_run(
            graph.as_ref(),
            &VerifyRunRequest {
                entity: "checkout".into(),
                runner: "printf".into(),
                depth: 2,
            },
        )
        .unwrap();
        let executed = execute_verification(layout.working_dir(), prepared.command_line());
        let (record, response) =
            record_verify_run(&binding, graph.as_ref(), &prepared, &executed).unwrap();
        assert!(response
            .lines
            .iter()
            .any(|line| line.contains("VerificationRun recorded")));
        // Planning the records writes none of them: the run reaches a graph
        // only when its writer applies it, after authority holds the delta.
        assert!(graph.list_runs_for_test(&test.test_id).unwrap().is_empty());
        assert!(graph.query_audit_events(None, 10).unwrap().is_empty());
        let delta = record.to_delta().unwrap();
        assert_eq!(delta.verification_runs.len(), 1);
        assert_eq!(delta.audit_events.len(), 1);
        assert_eq!(
            record.reason(),
            format!("verify.run {}", record.run().run_id)
        );
        record.apply_to(graph.as_ref()).unwrap();
        snap.save().unwrap();
        drop(graph);
        drop(snap);

        let reopened = crate::backend::open_kindb_snapshot(&layout).unwrap();
        let graph = reopened.graph();

        let runs = graph.list_runs_for_test(&test.test_id).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, VerificationStatus::Passing);

        let entity_runs = graph.list_runs_proving_entity(&entity.id).unwrap();
        assert_eq!(entity_runs.len(), 1);
        assert_eq!(entity_runs[0].run_id, runs[0].run_id);

        let work_runs = graph.list_runs_proving_work(&work.work_id).unwrap();
        assert_eq!(work_runs.len(), 1);
        assert_eq!(work_runs[0].run_id, runs[0].run_id);

        let plan = build_verification_plan(graph.as_ref(), "checkout", 2).unwrap();
        assert_eq!(
            plan.latest_test_runs
                .get(&test.test_id)
                .map(|run| run.status),
            Some(VerificationStatus::Passing)
        );

        let audit_events = graph.query_audit_events(None, 10).unwrap();
        assert_eq!(audit_events.len(), 1);
        assert_eq!(audit_events[0].action, "verify.run");
    }

    #[test]
    fn build_verification_plan_includes_impacted_dependents() {
        let dir = tempfile::tempdir().unwrap();
        kin_core::init(dir.path()).unwrap();
        let layout = kin_core::KinLayout::discover(dir.path()).unwrap();
        let snap = crate::backend::open_kindb_snapshot(&layout).unwrap();
        let graph = snap.graph();

        let callee = make_entity("checkout_core", "src/checkout.rs");
        let caller = make_entity("checkout_handler", "src/handler.rs");
        graph.upsert_entity(&callee).unwrap();
        graph.upsert_entity(&caller).unwrap();
        graph
            .upsert_relation(&Relation {
                id: RelationId::new(),
                kind: RelationKind::Calls,
                src: GraphNodeId::Entity(caller.id),
                dst: GraphNodeId::Entity(callee.id),
                confidence: 1.0,
                origin: RelationOrigin::Parsed,
                created_in: None,
                import_source: None,
                evidence: vec![],
            })
            .unwrap();

        let caller_test = TestCase {
            test_id: kin_model::TestId::new(),
            name: "test_checkout_handler".into(),
            language: "rust".into(),
            kind: TestKind::Integration,
            scopes: vec![WorkScope::Entity(caller.id)],
            runner: TestRunner::Cargo,
            file_origin: Some(FilePathId::new("tests/checkout_handler.rs")),
        };
        graph.create_test_case(&caller_test).unwrap();

        let plan = build_verification_plan(graph.as_ref(), "checkout_core", 1).unwrap();

        assert_eq!(plan.direct.entity.id, callee.id);
        assert!(plan.direct.tests.is_empty());
        assert_eq!(plan.impacted.len(), 1);
        assert_eq!(plan.impacted[0].entity.id, caller.id);
        assert_eq!(plan.impacted[0].tests.len(), 1);
        assert_eq!(plan.impacted[0].tests[0].test_id, caller_test.test_id);
        assert_eq!(plan.tests.len(), 1);
        assert_eq!(plan.tests[0].test_id, caller_test.test_id);
        assert_eq!(plan.proved_entities.len(), 2);
        assert!(plan
            .proved_entities
            .iter()
            .any(|entity| entity.id == callee.id));
        assert!(plan
            .proved_entities
            .iter()
            .any(|entity| entity.id == caller.id));
    }

    #[test]
    fn build_change_verification_plan_aggregates_impacted_tests() {
        let dir = tempfile::tempdir().unwrap();
        kin_core::init(dir.path()).unwrap();
        let layout = kin_core::KinLayout::discover(dir.path()).unwrap();
        let snap = crate::backend::open_kindb_snapshot(&layout).unwrap();
        let graph = snap.graph();

        let callee = make_entity("checkout_core", "src/checkout.rs");
        let caller = make_entity("checkout_handler", "src/handler.rs");
        graph.upsert_entity(&callee).unwrap();
        graph.upsert_entity(&caller).unwrap();
        graph
            .upsert_relation(&Relation {
                id: RelationId::new(),
                kind: RelationKind::Calls,
                src: GraphNodeId::Entity(caller.id),
                dst: GraphNodeId::Entity(callee.id),
                confidence: 1.0,
                origin: RelationOrigin::Parsed,
                created_in: None,
                import_source: None,
                evidence: vec![],
            })
            .unwrap();

        let caller_test = TestCase {
            test_id: kin_model::TestId::new(),
            name: "test_checkout_handler".into(),
            language: "rust".into(),
            kind: TestKind::Integration,
            scopes: vec![WorkScope::Entity(caller.id)],
            runner: TestRunner::Cargo,
            file_origin: Some(FilePathId::new("tests/checkout_handler.rs")),
        };
        graph.create_test_case(&caller_test).unwrap();

        let change = SemanticChange {
            id: SemanticChangeId::from_hash(Hash256::from_bytes([7; 32])),
            parents: vec![],
            origin: ChangeOrigin::Native,
            timestamp: Timestamp::now(),
            author: AuthorId::new("test"),
            message: "touch checkout core".into(),
            entity_deltas: vec![EntityDelta::Modified {
                old: callee.clone(),
                new: callee.clone(),
            }],
            relation_deltas: vec![],
            tree_deltas: vec![],
            projected_files: vec![],
            spec_link: None,
            evidence: vec![],
            risk_summary: None,
            admission_policy_delta: None,
            external_reference_deltas: Vec::new(),
            resolution_record_deltas: Vec::new(),
        };

        let plan = build_change_verification_plan(graph.as_ref(), &change, 1).unwrap();

        assert_eq!(plan.entity_plans.len(), 1);
        assert_eq!(plan.entity_plans[0].entity.id, callee.id);
        assert_eq!(plan.entity_plans[0].impacted.len(), 1);
        assert_eq!(plan.entity_plans[0].impacted[0].entity.id, caller.id);
        assert_eq!(plan.tests.len(), 1);
        assert_eq!(plan.tests[0].test_id, caller_test.test_id);
        assert!(plan.removed_entities.is_empty());
    }

    fn calls(source: &Entity, target: &Entity) -> Relation {
        Relation {
            id: RelationId::new(),
            kind: RelationKind::Calls,
            src: GraphNodeId::Entity(source.id),
            dst: GraphNodeId::Entity(target.id),
            confidence: 1.0,
            origin: RelationOrigin::Parsed,
            created_in: None,
            import_source: None,
            evidence: vec![],
        }
    }

    fn test_entity(name: &str) -> Entity {
        Entity {
            role: EntityRole::Test,
            ..make_entity(name, "tests/math_test.rs")
        }
    }

    /// The manifests of a project that holds only what it is given.
    struct Manifests(Vec<(&'static str, &'static str)>);

    impl kin_parser::project_manifest::ProjectFiles for Manifests {
        fn manifest(
            &self,
            path: &str,
        ) -> std::result::Result<
            Option<std::sync::Arc<str>>,
            kin_parser::project_manifest::UnreadableManifest,
        > {
            Ok(self
                .0
                .iter()
                .find(|(name, _)| *name == path)
                .map(|(_, text)| std::sync::Arc::from(*text)))
        }
    }

    const NODE_PACKAGE: &str = r#"{"name":"tally","scripts":{"test":"node --test"}}"#;

    #[test]
    fn a_removed_entity_is_an_explicit_verification_gap() {
        let dir = tempfile::tempdir().unwrap();
        kin_core::init(dir.path()).unwrap();
        let layout = kin_core::KinLayout::discover(dir.path()).unwrap();
        let binding = RequestRepositoryAuthority::pinned(
            kin_core::LocalRepositoryAuthorityBinding::from_layout(&layout).unwrap(),
        );
        let snap = crate::backend::open_kindb_snapshot(&layout).unwrap();
        let graph = snap.graph();
        let removed = make_entity("removed", "src/lib.rs");
        // A persistent store refuses a change whose declared identity does not
        // recompute from its contents, so the id is derived, not invented.
        let mut change = SemanticChange {
            id: SemanticChangeId::from_hash(Hash256::from_bytes([17; 32])),
            parents: vec![],
            origin: ChangeOrigin::Native,
            timestamp: Timestamp::now(),
            author: AuthorId::new("test"),
            message: "remove callable".into(),
            entity_deltas: vec![EntityDelta::Removed { old: removed }],
            relation_deltas: vec![],
            tree_deltas: vec![],
            projected_files: vec![],
            spec_link: None,
            evidence: vec![],
            risk_summary: None,
            admission_policy_delta: None,
            external_reference_deltas: vec![],
            resolution_record_deltas: vec![],
        };
        change.id = kin_model::compute_semantic_change_id(&change).unwrap();
        graph.create_change(&change).unwrap();
        let scope = WorkVerifyScope {
            entities: vec![],
            change: Some(change.id.to_string()),
            depth: 2,
        };
        let result = prepare_work_verify(
            &binding,
            graph.as_ref(),
            &Manifests(vec![("package.json", NODE_PACKAGE)]),
            &WorkVerifyRequest::Run(scope),
        );
        match result {
            Err(error) => assert_eq!(
                error.to_string(),
                "Kin cannot plan tests for removed entities yet."
            ),
            Ok(_) => panic!("deletion must not be reported as no changed entities"),
        }
    }

    /// The tests a guide lists for an entity are the tests its plan runs: a
    /// test entity that calls it directly, and one that reaches it through a
    /// dependent, each recorded under the one test id its identity gives it.
    /// A consumer that is not a test is no test.
    #[test]
    fn the_plan_runs_the_test_entities_that_reach_an_entity_and_its_dependents() {
        let dir = tempfile::tempdir().unwrap();
        kin_core::init(dir.path()).unwrap();
        let layout = kin_core::KinLayout::discover(dir.path()).unwrap();
        let snap = crate::backend::open_kindb_snapshot(&layout).unwrap();
        let graph = snap.graph();

        let add = make_entity("add", "src/math.rs");
        let multiply = make_entity("multiply", "src/math.rs");
        let check_add = test_entity("check_add");
        let check_multiply = test_entity("check_multiply");
        for entity in [&add, &multiply, &check_add, &check_multiply] {
            graph.upsert_entity(entity).unwrap();
        }
        graph.upsert_relation(&calls(&multiply, &add)).unwrap();
        graph.upsert_relation(&calls(&check_add, &add)).unwrap();
        graph
            .upsert_relation(&calls(&check_multiply, &multiply))
            .unwrap();

        let reaching = tests_reaching(graph.as_ref(), &add.id).unwrap();
        assert_eq!(
            reaching.iter().map(|test| test.id).collect::<Vec<_>>(),
            [check_add.id],
            "multiply reaches add, but it is not a test"
        );

        let plan = build_verification_plan_for_entity(graph.as_ref(), add.clone(), 2).unwrap();
        let mut planned: Vec<_> = plan.tests.iter().map(|test| test.test_id).collect();
        planned.sort_by_key(|id| id.0);
        let mut expected = vec![
            test_id_for_entity(&check_add.id),
            test_id_for_entity(&check_multiply.id),
        ];
        expected.sort_by_key(|id| id.0);
        assert_eq!(planned, expected);
        assert_eq!(
            plan.test_entities
                .get(&test_id_for_entity(&check_multiply.id))
                .map(|entity| entity.id),
            Some(check_multiply.id)
        );
        assert_eq!(
            test_id_for_entity(&check_add.id),
            test_id_for_entity(&check_add.id),
            "a test entity is one test in every run"
        );

        let work = WorkPlan::from_plans(vec![plan]);
        let tied: HashSet<_> = work.ran_against.iter().map(|tied| tied.entity).collect();
        for entity in [&add, &multiply, &check_add, &check_multiply] {
            assert!(
                tied.contains(&entity.id),
                "{} is tied to the run",
                entity.name
            );
        }
    }

    /// Recorded structural coverage includes transitive dependencies and
    /// imports from test entities. Native test-case scopes remain included,
    /// while direct reach is separately readable for targeted proof slices.
    #[test]
    fn coverage_counts_an_entity_a_test_entity_reaches_as_covered() {
        let dir = tempfile::tempdir().unwrap();
        kin_core::init(dir.path()).unwrap();
        let layout = kin_core::KinLayout::discover(dir.path()).unwrap();
        let snap = crate::backend::open_kindb_snapshot(&layout).unwrap();
        let graph = snap.graph();

        let add = make_entity("add", "src/math.rs");
        let multiply = make_entity("multiply", "src/math.rs");
        let negate = make_entity("negate", "src/math.rs");
        let uncovered = make_entity("uncovered", "src/math.rs");
        let check_multiply = test_entity("check_multiply");
        let mut test_module = test_entity("math_test");
        test_module.kind = EntityKind::Module;
        for entity in [
            &add,
            &multiply,
            &negate,
            &uncovered,
            &check_multiply,
            &test_module,
        ] {
            graph.upsert_entity(entity).unwrap();
        }
        graph.upsert_relation(&calls(&multiply, &add)).unwrap();
        graph
            .upsert_relation(&calls(&check_multiply, &multiply))
            .unwrap();
        graph
            .upsert_relation(&Relation {
                kind: RelationKind::Imports,
                ..calls(&test_module, &negate)
            })
            .unwrap();

        assert_eq!(
            graph.get_coverage_summary().unwrap().covered_entities,
            0,
            "no test case was recorded, so the graph's own summary sees none"
        );
        let reading = coverage_reading(graph.as_ref()).unwrap();
        assert_eq!(reading.total_entities, 6);
        assert_eq!(reading.covered_entities, 3);
        let missing: HashSet<_> = reading.missing_proof.iter().copied().collect();
        assert!(!missing.contains(&multiply.id), "a test calls multiply");
        assert!(
            !missing.contains(&add.id),
            "the test reaches add through multiply"
        );
        assert!(
            !missing.contains(&negate.id),
            "the test module has recorded import reach"
        );
        assert!(missing.contains(&uncovered.id));
        assert!(
            tests_reaching(graph.as_ref(), &add.id).unwrap().is_empty(),
            "direct reach remains separately readable"
        );

        // A recorded test case still covers what it names.
        graph
            .create_test_case(&TestCase {
                test_id: kin_model::TestId::new(),
                name: "covers_negate".into(),
                language: "rust".into(),
                kind: TestKind::Unit,
                scopes: vec![WorkScope::Entity(uncovered.id)],
                runner: TestRunner::Cargo,
                file_origin: None,
            })
            .unwrap();
        let reading = coverage_reading(graph.as_ref()).unwrap();
        assert_eq!(reading.covered_entities, 4);
        assert!(!reading.missing_proof.contains(&uncovered.id));
    }

    #[test]
    fn a_repository_runs_the_test_command_its_own_manifest_declares() {
        assert_eq!(
            repository_test_command(&Manifests(vec![("package.json", NODE_PACKAGE)])),
            Some(RepositoryTestCommand::Whole(vec![
                "npm".to_string(),
                "test".to_string()
            ]))
        );
        assert_eq!(
            repository_test_command(&Manifests(vec![(
                "Cargo.toml",
                "[package]\nname = \"tally\"\n"
            )])),
            Some(RepositoryTestCommand::Runner(TestRunner::Cargo))
        );
        assert_eq!(repository_test_command(&Manifests(vec![])), None);
        assert_eq!(
            repository_test_command(&Manifests(vec![(
                "package.json",
                r#"{"name":"tally","scripts":{"build":"tsc"}}"#
            )])),
            None
        );

        let whole = RepositoryTestCommand::Whole(vec!["npm".into(), "test".into()]);
        assert_eq!(whole.command_line(&[]), "'npm' 'test'");
        let tests = ["b_test", "a_test"].map(|name| TestCase {
            test_id: kin_model::TestId::new(),
            name: name.into(),
            language: "rust".into(),
            kind: TestKind::Unit,
            scopes: vec![],
            runner: TestRunner::Cargo,
            file_origin: None,
        });
        assert_eq!(
            RepositoryTestCommand::Runner(TestRunner::Cargo).command_line(&tests),
            "cargo test -- 'a_test' 'b_test'"
        );
    }

    /// A repository with no test command is answered in one sentence that
    /// asks nothing of its reader, whatever was asked.
    #[test]
    fn a_repository_with_no_test_command_is_told_so_in_one_sentence() {
        let dir = tempfile::tempdir().unwrap();
        kin_core::init(dir.path()).unwrap();
        let layout = kin_core::KinLayout::discover(dir.path()).unwrap();
        let binding = RequestRepositoryAuthority::pinned(
            kin_core::LocalRepositoryAuthorityBinding::from_layout(&layout).unwrap(),
        );
        let snap = crate::backend::open_kindb_snapshot(&layout).unwrap();
        for request in [
            WorkVerifyRequest::TestCommand,
            WorkVerifyRequest::Run(WorkVerifyScope::default()),
        ] {
            let PreparedWork::Answered(answer) = prepare_work_verify(
                &binding,
                snap.graph().as_ref(),
                &Manifests(vec![]),
                &request,
            )
            .unwrap() else {
                panic!("nothing runs without a test command");
            };
            assert!(!answer.runnable);
            assert_eq!(answer.unavailable.as_deref(), Some(NO_TEST_COMMAND));
            assert!(answer.run.is_none());
        }
        assert_eq!(NO_TEST_COMMAND.matches('.').count(), 1);
        assert!(!NO_TEST_COMMAND.to_lowercase().contains("run `"));
    }

    /// A run of a piece of work is recorded against the fingerprints it
    /// planned from. Its result is current while they hold, the newest
    /// current run wins, and once an entity it ran against changes the
    /// result is stale, never current. A test no run reached is `not_run`.
    #[cfg(unix)]
    #[test]
    fn a_work_run_is_current_until_an_entity_it_ran_against_changes() {
        let dir = tempfile::tempdir().unwrap();
        kin_core::init(dir.path()).unwrap();
        let layout = kin_core::KinLayout::discover(dir.path()).unwrap();
        let binding = RequestRepositoryAuthority::pinned(
            kin_core::LocalRepositoryAuthorityBinding::from_layout(&layout).unwrap(),
        );
        let snap = crate::backend::open_kindb_snapshot(&layout).unwrap();
        let graph = snap.graph();

        let add = make_entity("add", "src/math.rs");
        let negate = make_entity("negate", "src/math.rs");
        let check_add = test_entity("check_add");
        let helper = make_entity("helper", "src/helpers.rs");
        let helper2 = make_entity("helper2", "src/helpers.rs");
        for entity in [&add, &negate, &check_add, &helper, &helper2] {
            graph.upsert_entity(entity).unwrap();
        }
        graph.upsert_relation(&calls(&check_add, &add)).unwrap();
        graph.upsert_relation(&calls(&check_add, &helper)).unwrap();
        graph.upsert_relation(&calls(&helper, &helper2)).unwrap();
        graph.upsert_relation(&calls(&helper2, &helper)).unwrap();
        let project = Manifests(vec![("package.json", NODE_PACKAGE)]);
        let result = |subject: &Entity| {
            test_result(
                graph.as_ref(),
                &check_add.id,
                Some(&subject.id),
                &mut HashMap::new(),
            )
            .unwrap()
        };
        assert_eq!(result(&add).state, TestResultState::NotRun);

        let scope = WorkVerifyScope {
            entities: vec![add.id.to_string()],
            change: None,
            depth: 2,
        };
        let run_with = |shell: &str| {
            let PreparedWork::Ready(ready) = prepare_work_verify(
                &binding,
                graph.as_ref(),
                &project,
                &WorkVerifyRequest::Run(scope.clone()),
            )
            .unwrap() else {
                panic!("a test reaches add, so the work runs");
            };
            assert_eq!(ready.command_line(), "'npm' 'test'");
            assert_eq!(ready.tests_planned(), 1);
            let executed = execute_verification(dir.path(), shell);
            let (record, answer) =
                record_work_verify(&binding, graph.as_ref(), &ready, &executed).unwrap();
            record.to_delta().unwrap();
            record.apply_to(graph.as_ref()).unwrap();
            answer
        };

        let failed = run_with("exit 3");
        let run = failed.run.as_ref().unwrap();
        assert_eq!(run.outcome, "failed");
        assert_eq!(run.exit_code, Some(3));
        assert_eq!(failed.reach.as_deref(), Some("whole"));
        assert_eq!(
            failed
                .tests
                .iter()
                .map(|test| test.name.as_str())
                .collect::<Vec<_>>(),
            ["check_add"]
        );
        let reading = result(&add);
        assert_eq!(reading.state, TestResultState::Failed);
        assert!(reading.at.is_some() && reading.duration_ms.is_some());
        assert!(
            reading.summary.as_deref().unwrap().contains("cannot tell"),
            "a whole-suite failure is not pinned on this test: {reading:?}"
        );

        std::thread::sleep(std::time::Duration::from_millis(5));
        let passed = run_with("true");
        assert_eq!(passed.run.as_ref().unwrap().outcome, "passed");
        assert_eq!(result(&add).state, TestResultState::Passed);
        // A step the run was not tied to has no current result from it.
        assert_eq!(result(&negate).state, TestResultState::Stale);

        // An outgoing helper dependency is outside the inbound proof slice,
        // but its transitive body still affects what check_add executes.
        let mut changed_helper = helper2.clone();
        changed_helper.fingerprint.behavior_hash = Hash256::from_bytes([7; 32]);
        graph.upsert_entity(&changed_helper).unwrap();
        assert_eq!(result(&add).state, TestResultState::Stale);
        graph.upsert_entity(&helper2).unwrap();
        assert_eq!(result(&add).state, TestResultState::Passed);
        let mut unrelated = negate.clone();
        unrelated.fingerprint.behavior_hash = Hash256::from_bytes([6; 32]);
        graph.upsert_entity(&unrelated).unwrap();
        assert_eq!(result(&add).state, TestResultState::Passed);

        // Edit add: every run of its test is now against an older add.
        let mut edited = add.clone();
        edited.fingerprint.behavior_hash = Hash256::from_bytes([9; 32]);
        graph.upsert_entity(&edited).unwrap();
        let stale = result(&edited);
        assert_eq!(stale.state, TestResultState::Stale);
        assert!(stale
            .summary
            .as_deref()
            .unwrap()
            .contains("last run passed"));

        // Put add back as it was, and the passing run is current again.
        graph.upsert_entity(&add).unwrap();
        assert_eq!(result(&add).state, TestResultState::Passed);

        // An edit to the test itself makes its result stale too.
        let mut test_edited = check_add.clone();
        test_edited.fingerprint.behavior_hash = Hash256::from_bytes([8; 32]);
        graph.upsert_entity(&test_edited).unwrap();
        assert_eq!(result(&add).state, TestResultState::Stale);
    }

    /// The runner a test command names is missing, as in a JavaScript
    /// repository with no `node_modules`: the shell exits 127. That is a
    /// command that could not run, said with the shell's own reason, its exit
    /// code and the end of its output, and recorded as attesting nothing.
    #[cfg(unix)]
    #[test]
    fn a_missing_runner_exiting_127_could_not_run() {
        let dir = tempfile::tempdir().unwrap();
        let executed = execute_verification(dir.path(), "kin-missing-runner --test");
        assert_eq!(executed.exit_code, Some(127));
        assert_eq!(executed.status, VerificationStatus::Pending);
        assert_eq!(executed.outcome(), "could_not_run");
        assert_eq!(
            executed.could_not_run.as_deref(),
            Some("The shell could not find `kin-missing-runner`, so no test ran.")
        );
        assert!(
            executed
                .output_tail
                .ends_with("kin-missing-runner: command not found")
                || executed
                    .output_tail
                    .ends_with("kin-missing-runner: not found"),
            "{:?}",
            executed.output_tail
        );
        assert!(executed
            .evidence_text
            .starts_with("=== COULD NOT RUN ===\nThe shell could not find `kin-missing-runner`"));
    }

    /// Kin could not start the command at all: no exit code, and the reason
    /// is the error that stopped it.
    #[test]
    fn a_command_that_never_started_could_not_run() {
        let dir = tempfile::tempdir().unwrap();
        let gone = dir.path().join("removed-working-tree");
        let executed = execute_verification(&gone, "true");
        assert_eq!(executed.exit_code, None);
        assert_eq!(executed.status, VerificationStatus::Pending);
        assert_eq!(executed.outcome(), "could_not_run");
        let reason = executed.could_not_run.as_deref().unwrap();
        assert!(
            reason.starts_with("Kin could not start the test command: "),
            "{reason}"
        );
        assert!(executed.evidence_text.contains(reason));
    }

    /// A test command that ran and failed stays a failure, even when what its
    /// tests printed mentions something not found, and its output is kept.
    #[cfg(unix)]
    #[test]
    fn a_real_failure_stays_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        let executed = execute_verification(
            dir.path(),
            "echo 'not ok 1 - reply carries the status'; \
             echo 'expected 200, got 404 Not Found' >&2; \
             echo 'sh: helper-tool: command not found' >&2; \
             echo '# tests 3, pass 2, fail 1' >&2; exit 1",
        );
        assert_eq!(executed.exit_code, Some(1));
        assert_eq!(executed.status, VerificationStatus::Failing);
        assert_eq!(executed.outcome(), "failed");
        assert_eq!(executed.could_not_run, None);
        assert!(executed
            .output_tail
            .starts_with("not ok 1 - reply carries the status"));
        assert!(executed.output_tail.ends_with("# tests 3, pass 2, fail 1"));
        assert!(!executed.evidence_text.contains("COULD NOT RUN"));

        let passed = execute_verification(dir.path(), "echo ok 1; exit 0");
        assert_eq!(passed.status, VerificationStatus::Passing);
        assert_eq!(passed.outcome(), "passed");
        assert_eq!(passed.could_not_run, None);
    }

    /// A wrapper that turns the shell's 127 into its own exit still relays
    /// that the runner was missing, and a script found without permission to
    /// run exits 126: neither ran a test.
    #[cfg(unix)]
    #[test]
    fn a_relayed_or_unexecutable_runner_could_not_run() {
        let dir = tempfile::tempdir().unwrap();
        let relayed = execute_verification(
            dir.path(),
            "echo 'sh: borp: command not found' >&2; \
             echo 'error Command failed with exit code 127.' >&2; exit 1",
        );
        assert_eq!(relayed.exit_code, Some(1));
        assert_eq!(relayed.status, VerificationStatus::Pending);
        assert_eq!(
            relayed.could_not_run.as_deref(),
            Some("The shell could not find `borp`, so no test ran.")
        );

        std::fs::write(dir.path().join("run-tests"), "#!/bin/sh\nexit 0\n").unwrap();
        let unexecutable = execute_verification(dir.path(), "./run-tests");
        assert_eq!(unexecutable.exit_code, Some(126));
        assert_eq!(unexecutable.status, VerificationStatus::Pending);
        assert_eq!(
            unexecutable.could_not_run.as_deref(),
            Some("The shell found `./run-tests` but could not execute it, so no test ran.")
        );
    }

    #[test]
    fn shell_complaints_are_read_in_each_shell_s_own_form() {
        let not_found = |name: &str| Some(ShellComplaint::NotFound(name.to_string()));
        for (line, expected) in [
            ("sh: eslint: command not found", not_found("eslint")),
            ("/bin/sh: eslint: command not found", not_found("eslint")),
            ("/bin/sh: line 1: tap: command not found", not_found("tap")),
            ("sh: 1: jest: not found", not_found("jest")),
            ("zsh:1: command not found: borp", not_found("borp")),
            ("env: node: No such file or directory", not_found("node")),
            (
                "/usr/bin/env: \u{2018}node\u{2019}: No such file or directory",
                not_found("node"),
            ),
            (
                "bash: ./run: cannot execute binary file: Exec format error",
                Some(ShellComplaint::NotExecutable("./run".to_string())),
            ),
            ("expected 200, got 404 Not Found", None),
            ("Error: cannot find module 'tap'", None),
            ("node: command not found in fixture", None),
        ] {
            assert_eq!(shell_complaint(line), expected, "{line}");
        }
        assert!(relays_could_not_run("npm error code 127"));
        assert!(relays_could_not_run("make: *** [test] Error 127"));
        assert!(!relays_could_not_run("exit code 1270"));
        assert!(!relays_could_not_run("expected code 1"));
        assert_eq!(
            strip_terminal_codes("\u{1b}[31mnot ok\u{1b}[0m 1"),
            "not ok 1"
        );
    }

    /// A recorded run whose command could not run attests no outcome: the
    /// tests it planned read as not run, with why, and never as failed. A real
    /// result for the same code still stands beside a later attempt that could
    /// not run, and its evidence keeps the reason.
    #[cfg(unix)]
    #[test]
    fn tests_a_command_could_not_run_read_as_not_run() {
        let dir = tempfile::tempdir().unwrap();
        kin_core::init(dir.path()).unwrap();
        let layout = kin_core::KinLayout::discover(dir.path()).unwrap();
        let authority = RequestRepositoryAuthority::pinned(
            kin_core::LocalRepositoryAuthorityBinding::from_layout(&layout).unwrap(),
        );
        let snap = crate::backend::open_kindb_snapshot(&layout).unwrap();
        let graph = snap.graph();
        let add = make_entity("add", "src/math.rs");
        let check_add = test_entity("check_add");
        for entity in [&add, &check_add] {
            graph.upsert_entity(entity).unwrap();
        }
        graph.upsert_relation(&calls(&check_add, &add)).unwrap();
        let project = Manifests(vec![("package.json", NODE_PACKAGE)]);
        let scope = WorkVerifyScope {
            entities: vec![add.id.to_string()],
            change: None,
            depth: 2,
        };
        let result = || {
            test_result(
                graph.as_ref(),
                &check_add.id,
                Some(&add.id),
                &mut HashMap::new(),
            )
            .unwrap()
        };
        let run_with = |shell: &str| {
            let PreparedWork::Ready(ready) = prepare_work_verify(
                &authority,
                graph.as_ref(),
                &project,
                &WorkVerifyRequest::Run(scope.clone()),
            )
            .unwrap() else {
                panic!("a test reaches add, so the work runs");
            };
            let executed = execute_verification(dir.path(), shell);
            let (record, answer) =
                record_work_verify(&authority, graph.as_ref(), &ready, &executed).unwrap();
            record.apply_to(graph.as_ref()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(5));
            (record, answer)
        };

        let (record, answer) = run_with("kin-missing-runner --test");
        let run = answer.run.as_ref().unwrap();
        assert_eq!(run.outcome, "could_not_run");
        assert_eq!(run.exit_code, Some(127));
        assert_eq!(
            run.reason.as_deref(),
            Some("The shell could not find `kin-missing-runner`, so no test ran.")
        );
        let tail = run.output_tail.as_deref().unwrap();
        assert!(
            tail.ends_with("kin-missing-runner: command not found")
                || tail.ends_with("kin-missing-runner: not found"),
            "{tail:?}"
        );
        assert_eq!(answer.tests.len(), 1, "the plan is still reported");
        assert!(record.run().could_not_run());
        assert_eq!(record.run().status, VerificationStatus::Pending);
        assert_eq!(record.run().exit_code, Some(127));
        let evidence = authority
            .open()
            .unwrap()
            .load_source_blob(record.run().evidence_blob.unwrap())
            .unwrap();
        assert!(String::from_utf8(evidence)
            .unwrap()
            .starts_with("=== COULD NOT RUN ===\nThe shell could not find `kin-missing-runner`"));

        let reading = result();
        assert_eq!(reading.state, TestResultState::NotRun);
        assert!(reading.at.is_some());
        assert_eq!(
            reading.summary.as_deref(),
            Some("The test command could not run, so this test did not run. It exited 127.")
        );
        let mut lines = Vec::new();
        let plan = build_verification_plan(graph.as_ref(), &add.id.to_string(), 2).unwrap();
        lines.extend(verification_plan_lines(&plan));
        assert!(
            lines
                .iter()
                .any(|line| line.ends_with("latest=could not run")),
            "{lines:?}"
        );

        // A command that ran and failed is a failure.
        run_with("exit 3");
        assert_eq!(result().state, TestResultState::Failed);
        // A later attempt that could not run does not erase that result,
        // because it attests nothing about the same code.
        run_with("kin-missing-runner --test");
        assert_eq!(result().state, TestResultState::Failed);
        run_with("true");
        assert_eq!(result().state, TestResultState::Passed);

        // An attempt against code since edited is not a current result.
        let mut edited = add.clone();
        edited.fingerprint.behavior_hash = Hash256::from_bytes([9; 32]);
        graph.upsert_entity(&edited).unwrap();
        assert_eq!(result().state, TestResultState::Stale);
    }

    /// A run recorded before runs were tied to fingerprints is never shown
    /// as current.
    #[test]
    fn a_run_tied_to_no_fingerprint_is_stale() {
        let dir = tempfile::tempdir().unwrap();
        kin_core::init(dir.path()).unwrap();
        let layout = kin_core::KinLayout::discover(dir.path()).unwrap();
        let snap = crate::backend::open_kindb_snapshot(&layout).unwrap();
        let graph = snap.graph();
        let add = make_entity("add", "src/math.rs");
        let check_add = test_entity("check_add");
        graph.upsert_entity(&add).unwrap();
        graph.upsert_entity(&check_add).unwrap();
        graph
            .create_verification_run(&VerificationRun {
                run_id: VerificationRunId::new(),
                test_ids: vec![test_id_for_entity(&check_add.id)],
                status: VerificationStatus::Passing,
                runner: TestRunner::Cargo,
                started_at: Timestamp::now(),
                finished_at: Some(Timestamp::now()),
                duration_ms: Some(4),
                evidence_blob: None,
                exit_code: Some(0),
                ran_against: Vec::new(),
                proof: None,
            })
            .unwrap();
        let reading = test_result(
            graph.as_ref(),
            &check_add.id,
            Some(&add.id),
            &mut HashMap::new(),
        )
        .unwrap();
        assert_eq!(reading.state, TestResultState::Stale);
    }

    /// `kin verify` reports the tests linked to repository entities, and a
    /// symbol outside the repository has none here. It is refused by what it
    /// is, never with the absence line the daemon waits out, and a plan or a
    /// run is refused the same way rather than as an unresolved query. An
    /// address the graph holds no symbol under keeps that absence line exactly.
    #[test]
    fn verify_refuses_an_external_symbol_and_keeps_the_absence_line_for_a_miss() {
        let store = crate::commands::external_symbols::fixture::external_store(false);
        for entity in [store.address(), store.node.id.to_string()] {
            let response = build_entity_verify_response(&store.graph, &entity).unwrap();
            assert_ne!(response.lines, [entity_not_found_line(&entity)]);
            let text = response.lines.join("\n");
            assert!(text.contains("Array.map"), "{text}");
            assert!(
                text.contains(&format!("kin refs {}", store.address())),
                "{text}"
            );
            let error = match build_verification_plan(&store.graph, &entity, 1) {
                Ok(_) => panic!("{entity} was planned"),
                Err(error) => error,
            };
            assert!(
                !error
                    .chain()
                    .any(|cause| cause.downcast_ref::<VerifyEntityUnresolved>().is_some()),
                "a symbol the graph holds is not an unresolved query: {error:#}"
            );
            assert!(format!("{error:#}").contains("Array.map"), "{error:#}");
        }
        let unknown = "external_reference:00000000-0000-8000-8000-000000000000";
        let response = build_entity_verify_response(&store.graph, unknown).unwrap();
        assert_eq!(response.lines, [entity_not_found_line(unknown)]);
    }
}
