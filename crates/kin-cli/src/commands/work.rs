// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

use anyhow::{Context, Result};
use kin_model::*;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fmt::Write as _;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum WorkRequest {
    Create {
        kind: String,
        title: String,
        description: Option<String>,
        scope: Option<String>,
        priority: Option<String>,
    },
    List {
        status: Option<String>,
        kind: Option<String>,
        scope: Option<String>,
    },
    Show {
        work_id: String,
    },
    Link {
        work_id: String,
        scope: String,
    },
    Decompose {
        parent_work_id: String,
        child_work_id: String,
    },
    Block {
        blocked_work_id: String,
        blocker_work_id: String,
    },
    Implement {
        work_id: String,
        scope: String,
    },
    Status {
        work_id: String,
        status: String,
    },
    Close {
        work_id: String,
    },
    Verify {
        work_id: String,
    },
    /// Read a work item's recorded scopes as data rather than rendered text.
    /// Commands that act on what a work item covers need the scopes typed, and
    /// parsing the rendered listing back would make the display format load
    /// bearing.
    Scopes {
        work_id: String,
    },
    TodoImport {
        path: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkScopesReport {
    pub work_id: WorkId,
    pub title: String,
    pub status: WorkStatus,
    pub scopes: Vec<WorkScope>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkResponse {
    #[serde(default)]
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scopes: Option<WorkScopesReport>,
}

#[derive(Debug, Clone)]
pub struct WorkExecution {
    pub response: WorkResponse,
    pub mutated: bool,
}

async fn run_daemon_work(request: &WorkRequest) -> Result<WorkResponse> {
    let layout = crate::commands::require_repository_layout()?;
    let daemon_url = std::env::var("KIN_DAEMON_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(Some)
        .unwrap_or(crate::daemon_client::resolve_daemon_url(&layout).await?);
    let base_url = daemon_url
        .ok_or_else(|| crate::daemon_client::daemon_required_error("work commands", &layout))?;
    let client = crate::daemon_client::DaemonClient::from_base_url(base_url)?;
    client.work(request).await.context("daemon work failed")
}

/// Read a work item's scopes as data. Callers that act on what a work item
/// covers use this rather than the rendered listing.
pub async fn request_work_scopes(work_id: &str) -> Result<WorkResponse> {
    run_daemon_work(&WorkRequest::Scopes {
        work_id: work_id.to_string(),
    })
    .await
}

fn print_work_response(response: WorkResponse) {
    print!("{}", response.text);
}

/// `kin work create` — Create a new work item.
pub async fn create(
    kind: String,
    title: String,
    description: Option<String>,
    scope: Option<String>,
    priority: Option<String>,
) -> Result<()> {
    print_work_response(
        run_daemon_work(&WorkRequest::Create {
            kind,
            title,
            description,
            scope,
            priority,
        })
        .await?,
    );
    Ok(())
}

/// `kin work list` — List work items with optional filters.
pub async fn list(
    status: Option<String>,
    kind: Option<String>,
    scope: Option<String>,
) -> Result<()> {
    print_work_response(
        run_daemon_work(&WorkRequest::List {
            status,
            kind,
            scope,
        })
        .await?,
    );
    Ok(())
}

/// `kin work show` — Show details of a work item.
pub async fn show(work_id: String) -> Result<()> {
    print_work_response(run_daemon_work(&WorkRequest::Show { work_id }).await?);
    Ok(())
}

/// `kin work link` — Link a work item to a scope.
pub async fn link(work_id: String, scope: String) -> Result<()> {
    print_work_response(run_daemon_work(&WorkRequest::Link { work_id, scope }).await?);
    Ok(())
}

/// `kin work decompose` — Link a parent work item to a child work item.
pub async fn decompose(parent_work_id: String, child_work_id: String) -> Result<()> {
    print_work_response(
        run_daemon_work(&WorkRequest::Decompose {
            parent_work_id,
            child_work_id,
        })
        .await?,
    );
    Ok(())
}

/// `kin work block` — Mark one work item as blocked by another.
pub async fn block(blocked_work_id: String, blocker_work_id: String) -> Result<()> {
    print_work_response(
        run_daemon_work(&WorkRequest::Block {
            blocked_work_id,
            blocker_work_id,
        })
        .await?,
    );
    Ok(())
}

/// `kin work implement` — Link an implementing scope to a work item.
pub async fn implement(work_id: String, scope: String) -> Result<()> {
    print_work_response(run_daemon_work(&WorkRequest::Implement { work_id, scope }).await?);
    Ok(())
}

/// `kin work status` — Update a work item status.
pub async fn status(work_id: String, status: String) -> Result<()> {
    print_work_response(run_daemon_work(&WorkRequest::Status { work_id, status }).await?);
    Ok(())
}

/// `kin work close` — Close a work item (set status to Done).
///
/// Warns if implementing entities lack test coverage but still closes.
pub async fn close(work_id: String) -> Result<()> {
    print_work_response(run_daemon_work(&WorkRequest::Close { work_id }).await?);
    Ok(())
}

/// `kin work verify` — Check verification status of a work item.
///
/// Checks that implementing entities have linked tests and reports
/// whether the work item has sufficient proof for completion.
pub async fn verify(work_id: String) -> Result<()> {
    print_work_response(run_daemon_work(&WorkRequest::Verify { work_id }).await?);
    Ok(())
}

// -- Helpers --

#[derive(Debug, Clone)]
struct TargetedTestStatus {
    test: TestCase,
    latest_run: Option<VerificationRun>,
}

#[derive(Debug, Clone)]
struct WorkVerificationReport {
    work: WorkItem,
    implementors: Vec<WorkScope>,
    direct_work_tests: Vec<TestCase>,
    direct_work_runs: Vec<VerificationRun>,
    targeted_tests: Vec<TargetedTestStatus>,
    missing_scope_proof: Vec<WorkScope>,
    tests_without_passing_run: usize,
}

fn build_work_verification_report<G>(graph: &G, work_id: &WorkId) -> Result<WorkVerificationReport>
where
    G: GraphStore,
    <G as GraphStore>::Error: std::fmt::Display + Send + Sync + 'static,
{
    let work = graph
        .get_work_item(work_id)
        .map_err(|e| anyhow::anyhow!(e.to_string()))?
        .ok_or_else(|| anyhow::anyhow!("work item not found: {}", work_id))?;

    let implementors = graph
        .get_implementors(work_id)
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    let coverage_scopes = if implementors.is_empty() {
        work.scopes.clone()
    } else {
        implementors.clone()
    };

    let direct_work_tests = graph
        .get_tests_verifying_work(work_id)
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    let direct_work_runs = graph
        .list_runs_proving_work(work_id)
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;

    let mut targeted_tests = Vec::new();
    let mut seen_tests = HashSet::new();
    let mut missing_scope_proof = Vec::new();

    for scope in &coverage_scopes {
        let tests = tests_for_scope(graph, scope)?;
        if !scope_has_proof(graph, scope, &tests)? {
            missing_scope_proof.push(scope.clone());
        }
        for test in tests {
            if seen_tests.insert(test.test_id) {
                targeted_tests.push(test);
            }
        }
    }

    for test in &direct_work_tests {
        if seen_tests.insert(test.test_id) {
            targeted_tests.push(test.clone());
        }
    }

    let targeted_tests: Vec<_> = targeted_tests
        .into_iter()
        .map(|test| {
            let latest_run = graph
                .list_runs_for_test(&test.test_id)
                .map_err(|e| anyhow::anyhow!(e.to_string()))?
                .into_iter()
                .max_by_key(|run| {
                    run.finished_at
                        .clone()
                        .unwrap_or_else(|| run.started_at.clone())
                });
            Ok(TargetedTestStatus { test, latest_run })
        })
        .collect::<Result<Vec<_>>>()?;

    let tests_without_passing_run = targeted_tests
        .iter()
        .filter(|targeted| {
            !matches!(
                targeted.latest_run.as_ref().map(|run| run.status),
                Some(VerificationStatus::Passing)
            )
        })
        .count();

    Ok(WorkVerificationReport {
        work,
        implementors,
        direct_work_tests,
        direct_work_runs,
        targeted_tests,
        missing_scope_proof,
        tests_without_passing_run,
    })
}

fn tests_for_scope<G>(graph: &G, scope: &WorkScope) -> Result<Vec<TestCase>>
where
    G: GraphStore,
    <G as GraphStore>::Error: std::fmt::Display + Send + Sync + 'static,
{
    match scope {
        WorkScope::Entity(entity_id) => graph
            .get_tests_for_entity(entity_id)
            .map_err(|e| anyhow::anyhow!(e.to_string())),
        WorkScope::Contract(contract_id) => graph
            .get_tests_covering_contract(contract_id)
            .map_err(|e| anyhow::anyhow!(e.to_string())),
        WorkScope::Artifact(_) | WorkScope::Change(_) => Ok(vec![]),
    }
}

fn scope_has_proof<G>(graph: &G, scope: &WorkScope, tests: &[TestCase]) -> Result<bool>
where
    G: GraphStore,
    <G as GraphStore>::Error: std::fmt::Display + Send + Sync + 'static,
{
    if !tests.is_empty() {
        return Ok(true);
    }

    match scope {
        WorkScope::Entity(entity_id) => Ok(graph
            .list_runs_proving_entity(entity_id)
            .map_err(|e| anyhow::anyhow!(e.to_string()))?
            .into_iter()
            .any(|run| run.status == VerificationStatus::Passing)),
        WorkScope::Contract(_) | WorkScope::Artifact(_) | WorkScope::Change(_) => Ok(false),
    }
}

fn parse_work_id(s: &str) -> Result<WorkId> {
    let uuid =
        uuid::Uuid::parse_str(s).map_err(|_| anyhow::anyhow!("invalid work item UUID: {}", s))?;
    Ok(WorkId(uuid))
}

pub(crate) fn parse_work_scope(s: &str) -> Result<WorkScope> {
    // The CLI and the MCP tools read a scope through one parser, so the
    // spellings it accepts, and the refusal an unrecognized one gets, are the
    // same on both surfaces.
    Ok(kin_mcp::handlers::common::parse_single_work_scope(s)?)
}

/// [`parse_work_scope`] after the check every scope argument asks first: a
/// scope naming a symbol outside the repository is refused by what it names,
/// in `command`'s words, rather than stored as an entity or refused as a
/// spelling. `why` finishes the refusal's sentence after the command's name,
/// the clause the MCP tool refuses the same scope with.
pub(crate) fn parse_graph_work_scope<G: GraphStore>(
    graph: &G,
    scope: &str,
    command: &str,
    why: &str,
) -> Result<WorkScope> {
    if let Some(lines) = crate::commands::external_symbols::scope_argument_refusal(
        graph,
        scope,
        &format!("`{command}` {why}"),
    )? {
        anyhow::bail!(lines.join("\n"));
    }
    parse_work_scope(scope)
}

fn render_work_list(
    graph: &kin_db::InMemoryGraph,
    status: Option<String>,
    kind: Option<String>,
    scope: Option<String>,
) -> Result<String> {
    let filter = WorkFilter {
        kinds: kind
            .map(|k| {
                k.parse::<WorkKind>()
                    .map(|wk| vec![wk])
                    .map_err(|e| anyhow::anyhow!(e))
            })
            .transpose()?,
        statuses: status
            .map(|s| {
                s.parse::<WorkStatus>()
                    .map(|ws| vec![ws])
                    .map_err(|e| anyhow::anyhow!(e))
            })
            .transpose()?,
        scope: scope
            .as_deref()
            .map(|scope| {
                parse_graph_work_scope(
                    graph,
                    scope,
                    "kin work list",
                    crate::commands::external_symbols::WORK_LIST_WHY,
                )
            })
            .transpose()?,
    };

    let items = graph.list_work_items(&filter)?;
    if items.is_empty() {
        return Ok("No work items found.\n".to_string());
    }

    let mut out = String::new();
    writeln!(
        out,
        "{:<36}  {:<12}  {:<12}  {:<8}  TITLE",
        "ID", "KIND", "STATUS", "PRIORITY"
    )?;
    writeln!(out, "{}", "-".repeat(100))?;
    for item in &items {
        writeln!(
            out,
            "{:<36}  {:<12}  {:<12}  {:<8}  {}",
            item.work_id, item.kind, item.status, item.priority, item.title,
        )?;
    }
    if let Some(scope) = scope {
        writeln!(out, "\nScope filter: {scope}")?;
    }
    writeln!(out, "\n{} work item(s)", items.len())?;
    Ok(out)
}

fn render_work_show(graph: &kin_db::InMemoryGraph, work_id: String) -> Result<String> {
    let id = parse_work_id(&work_id)?;
    let item = graph
        .get_work_item(&id)?
        .ok_or_else(|| anyhow::anyhow!("work item not found: {}", work_id))?;

    let mut out = String::new();
    writeln!(out, "Work Item: {}", item.work_id)?;
    writeln!(out, "  Kind:     {}", item.kind)?;
    writeln!(out, "  Title:    {}", item.title)?;
    writeln!(out, "  Status:   {}", item.status)?;
    writeln!(out, "  Priority: {}", item.priority)?;
    writeln!(out, "  Created:  {}", item.created_at)?;
    writeln!(
        out,
        "  Author:   {} ({:?})",
        item.created_by.name, item.created_by.kind
    )?;

    if !item.description.is_empty() {
        writeln!(out, "\n  Description:\n    {}", item.description)?;
    }
    if !item.scopes.is_empty() {
        writeln!(out, "\n  Scopes:")?;
        for scope in &item.scopes {
            writeln!(out, "    - {scope}")?;
        }
    }
    if !item.acceptance_criteria.is_empty() {
        writeln!(out, "\n  Acceptance Criteria:")?;
        for (i, crit) in item.acceptance_criteria.iter().enumerate() {
            writeln!(out, "    {}. {}", i + 1, crit)?;
        }
    }
    if !item.external_refs.is_empty() {
        writeln!(out, "\n  External References:")?;
        for ext in &item.external_refs {
            writeln!(out, "    - {} #{}", ext.system, ext.identifier)?;
        }
    }

    let children = graph.get_child_work_items(&id)?;
    let parents = graph.get_parent_work_items(&id)?;
    let blockers = graph.get_blockers(&id)?;
    let blocked_items = graph.get_blocked_work_items(&id)?;
    let annotations = graph.get_annotations_for_work_item(&id)?;

    if !children.is_empty() {
        writeln!(out, "\n  Child Items:")?;
        for child in &children {
            writeln!(
                out,
                "    - [{}] {} ({})",
                child.kind, child.title, child.status
            )?;
        }
    }
    if !parents.is_empty() {
        writeln!(out, "\n  Parent Items:")?;
        for parent in &parents {
            writeln!(
                out,
                "    - [{}] {} ({})",
                parent.kind, parent.title, parent.status
            )?;
        }
    }
    if !blockers.is_empty() {
        writeln!(out, "\n  Blocked By:")?;
        for blocker in &blockers {
            writeln!(
                out,
                "    - [{}] {} ({})",
                blocker.kind, blocker.title, blocker.status
            )?;
        }
    }
    if !blocked_items.is_empty() {
        writeln!(out, "\n  Blocking:")?;
        for blocked in &blocked_items {
            writeln!(
                out,
                "    - [{}] {} ({})",
                blocked.kind, blocked.title, blocked.status
            )?;
        }
    }

    let implementors = graph.get_implementors(&id)?;
    if !implementors.is_empty() {
        writeln!(out, "\n  Implemented By:")?;
        for scope in &implementors {
            writeln!(out, "    - {scope}")?;
        }
    }

    if !annotations.is_empty() {
        writeln!(out, "\n  Annotations:")?;
        for ann in &annotations {
            writeln!(out, "    - [{}|{}] {}", ann.kind, ann.staleness, ann.body)?;
        }
    }
    Ok(out)
}

fn render_work_verification(graph: &kin_db::InMemoryGraph, work_id: String) -> Result<String> {
    let id = parse_work_id(&work_id)?;
    let report = build_work_verification_report(graph, &id)?;
    let mut out = String::new();
    writeln!(
        out,
        "Work item: {} ({})",
        report.work.title, report.work.kind
    )?;
    writeln!(out, "  Status: {}", report.work.status)?;

    if !report.work.acceptance_criteria.is_empty() {
        writeln!(
            out,
            "  Acceptance criteria: {}",
            report.work.acceptance_criteria.len()
        )?;
        for (i, crit) in report.work.acceptance_criteria.iter().enumerate() {
            writeln!(out, "    {}. {}", i + 1, crit)?;
        }
    }

    if report.implementors.is_empty() {
        writeln!(out, "  Implementors: none (falling back to work scopes)")?;
    } else {
        writeln!(out, "  Implementors: {}", report.implementors.len())?;
    }

    if report.direct_work_tests.is_empty() {
        writeln!(out, "  Direct work tests: none")?;
    } else {
        writeln!(
            out,
            "  Direct work tests: {}",
            report.direct_work_tests.len()
        )?;
        for test in &report.direct_work_tests {
            writeln!(
                out,
                "    - {} [{}] runner={}",
                test.name, test.kind, test.runner
            )?;
        }
    }

    if report.direct_work_runs.is_empty() {
        writeln!(out, "  Direct proof runs: none")?;
    } else {
        writeln!(
            out,
            "  Direct proof runs: {}",
            report.direct_work_runs.len()
        )?;
        for run in &report.direct_work_runs {
            writeln!(out, "    - {} via {}", run.status, run.runner)?;
        }
    }

    if !report.missing_scope_proof.is_empty() {
        writeln!(out, "  Missing proof on scopes:")?;
        for scope in &report.missing_scope_proof {
            writeln!(out, "    - {scope}")?;
        }
    }

    let has_passing_work_run = report
        .direct_work_runs
        .iter()
        .any(|run| run.status == VerificationStatus::Passing);

    if report.targeted_tests.is_empty() && !has_passing_work_run {
        writeln!(out, "  Targeted test set: none")?;
        writeln!(
            out,
            "  Completion: INCOMPLETE; no targeted proof exists for this work item"
        )?;
        return Ok(out);
    }

    if report.targeted_tests.is_empty() {
        writeln!(out, "  Targeted test set: none")?;
    } else {
        writeln!(out, "  Targeted test set: {}", report.targeted_tests.len())?;
        for targeted in &report.targeted_tests {
            let latest = targeted
                .latest_run
                .as_ref()
                .map(|run| run.status.to_string())
                .unwrap_or_else(|| "not_run".to_string());
            writeln!(
                out,
                "    - {} [{}] runner={} latest={}",
                targeted.test.name, targeted.test.kind, targeted.test.runner, latest
            )?;
        }
    }

    if report.tests_without_passing_run == 0
        && report.missing_scope_proof.is_empty()
        && (!report.targeted_tests.is_empty() || has_passing_work_run)
    {
        writeln!(
            out,
            "  Completion: VERIFIED; targeted proof set is passing for this work item"
        )?;
    } else {
        writeln!(
            out,
            "  Completion: PARTIAL; {} targeted test(s) lack a passing run, {} scope(s) still lack proof",
            report.tests_without_passing_run,
            report.missing_scope_proof.len()
        )?;
    }
    Ok(out)
}

/// Run one work request against `graph`.
///
/// `held` is the repository's pinned storage capability, which the TODO import
/// reads held content through. A long-lived daemon passes the binding it
/// pinned at startup; a one-shot caller passes `None` and the import pins one
/// from `layout` itself.
pub fn execute_work_request(
    layout: &kin_core::KinLayout,
    graph: &kin_db::InMemoryGraph,
    held: Option<&kin_core::LocalRepositoryAuthorityBinding>,
    request: WorkRequest,
) -> Result<WorkExecution> {
    let mut mutated = false;
    let mut scopes = None;
    let text = match request {
        WorkRequest::Create {
            kind,
            title,
            description,
            scope,
            priority,
        } => {
            let work_kind: WorkKind = kind.parse().map_err(|e: String| anyhow::anyhow!(e))?;
            let pri: Priority = priority
                .as_deref()
                .unwrap_or("none")
                .parse()
                .map_err(|e: String| anyhow::anyhow!(e))?;
            let scopes = scope
                .map(|s| {
                    parse_graph_work_scope(
                        graph,
                        &s,
                        "kin work create",
                        crate::commands::external_symbols::WORK_SCOPE_WHY,
                    )
                })
                .transpose()?
                .into_iter()
                .collect();
            let item = WorkItem {
                work_id: WorkId::new(),
                kind: work_kind,
                title: title.clone(),
                description: description.unwrap_or_default(),
                status: WorkStatus::Proposed,
                priority: pri,
                scopes,
                acceptance_criteria: vec![],
                external_refs: vec![],
                created_by: IdentityRef::human("cli-user"),
                created_at: Timestamp::now(),
            };
            graph.create_work_item(&item)?;
            for scope in &item.scopes {
                graph.create_work_link(&WorkLink::Affects {
                    work_id: item.work_id,
                    scope: scope.clone(),
                })?;
            }
            crate::provenance::record_cli_audit_event(
                graph,
                "work.create",
                item.scopes.first().cloned(),
                Some(format!(
                    "work_id={}; kind={}; status={}",
                    item.work_id, item.kind, item.status
                )),
            )?;
            mutated = true;
            format!("Created {} '{}' ({})\n", item.kind, title, item.work_id)
        }
        WorkRequest::List {
            status,
            kind,
            scope,
        } => render_work_list(graph, status, kind, scope)?,
        WorkRequest::Show { work_id } => render_work_show(graph, work_id)?,
        WorkRequest::Link { work_id, scope } => {
            let id = parse_work_id(&work_id)?;
            let ws = parse_graph_work_scope(
                graph,
                &scope,
                "kin work link",
                crate::commands::external_symbols::WORK_SCOPE_WHY,
            )?;
            let mut item = graph
                .get_work_item(&id)?
                .ok_or_else(|| anyhow::anyhow!("work item not found: {}", work_id))?;
            if !item.scopes.contains(&ws) {
                item.scopes.push(ws.clone());
                graph.create_work_item(&item)?;
            }
            graph.create_work_link(&WorkLink::Affects {
                work_id: id,
                scope: ws.clone(),
            })?;
            mutated = true;
            format!("Linked {} -> {}\n", work_id, ws)
        }
        WorkRequest::Decompose {
            parent_work_id,
            child_work_id,
        } => {
            let parent = parse_work_id(&parent_work_id)?;
            let child = parse_work_id(&child_work_id)?;
            graph
                .get_work_item(&parent)?
                .ok_or_else(|| anyhow::anyhow!("work item not found: {}", parent_work_id))?;
            graph
                .get_work_item(&child)?
                .ok_or_else(|| anyhow::anyhow!("work item not found: {}", child_work_id))?;
            graph.create_work_link(&WorkLink::DecomposesTo { parent, child })?;
            mutated = true;
            format!(
                "Linked parent {} -> child {}\n",
                parent_work_id, child_work_id
            )
        }
        WorkRequest::Block {
            blocked_work_id,
            blocker_work_id,
        } => {
            let blocked = parse_work_id(&blocked_work_id)?;
            let blocker = parse_work_id(&blocker_work_id)?;
            graph
                .get_work_item(&blocked)?
                .ok_or_else(|| anyhow::anyhow!("work item not found: {}", blocked_work_id))?;
            graph
                .get_work_item(&blocker)?
                .ok_or_else(|| anyhow::anyhow!("work item not found: {}", blocker_work_id))?;
            graph.create_work_link(&WorkLink::BlockedBy { blocked, blocker })?;
            mutated = true;
            format!(
                "Marked {} as blocked by {}\n",
                blocked_work_id, blocker_work_id
            )
        }
        WorkRequest::Implement { work_id, scope } => {
            let id = parse_work_id(&work_id)?;
            let scope = parse_graph_work_scope(
                graph,
                &scope,
                "kin work implement",
                crate::commands::external_symbols::WORK_SCOPE_WHY,
            )?;
            graph
                .get_work_item(&id)?
                .ok_or_else(|| anyhow::anyhow!("work item not found: {}", work_id))?;
            graph.create_work_link(&WorkLink::Implements {
                scope: scope.clone(),
                work_id: id,
            })?;
            mutated = true;
            format!("Linked implementor {} -> {}\n", scope, work_id)
        }
        WorkRequest::Status { work_id, status } => {
            let id = parse_work_id(&work_id)?;
            let status = status
                .parse::<WorkStatus>()
                .map_err(|e: String| anyhow::anyhow!(e))?;
            graph
                .get_work_item(&id)?
                .ok_or_else(|| anyhow::anyhow!("work item not found: {}", work_id))?;
            graph.update_work_status(&id, status)?;
            let item = graph.get_work_item(&id)?.ok_or_else(|| {
                anyhow::anyhow!("work item not found after status update: {}", work_id)
            })?;
            crate::provenance::record_cli_audit_event(
                graph,
                "work.status",
                item.scopes.first().cloned(),
                Some(format!("work_id={}; status={}", item.work_id, item.status)),
            )?;
            mutated = true;
            format!("Updated {} -> {}\n", work_id, status)
        }
        WorkRequest::Close { work_id } => {
            let id = parse_work_id(&work_id)?;
            graph
                .get_work_item(&id)?
                .ok_or_else(|| anyhow::anyhow!("work item not found: {}", work_id))?;
            let implementors = graph.get_implementors(&id)?;
            let mut uncovered = Vec::new();
            for scope in &implementors {
                if let WorkScope::Entity(eid) = scope {
                    let tests = graph.get_tests_for_entity(eid)?;
                    if tests.is_empty() {
                        let name = graph.get_entity(eid)?.map(|entity| entity.name);
                        uncovered.push((*eid, name));
                    }
                }
            }
            graph.update_work_status(&id, WorkStatus::Done)?;
            let item = graph
                .get_work_item(&id)?
                .ok_or_else(|| anyhow::anyhow!("work item not found after close: {}", work_id))?;
            crate::provenance::record_cli_audit_event(
                graph,
                "work.close",
                item.scopes.first().cloned(),
                Some(format!(
                    "work_id={}; uncovered={}",
                    item.work_id,
                    uncovered.len()
                )),
            )?;
            mutated = true;
            let mut out = String::new();
            if !uncovered.is_empty() {
                writeln!(
                    out,
                    "Warning: {} implementing entity(ies) lack test coverage:",
                    uncovered.len()
                )?;
                for (eid, name) in &uncovered {
                    if let Some(name) = name {
                        writeln!(out, "  - {} ({})", name, eid)?;
                    } else {
                        writeln!(out, "  - {}", eid)?;
                    }
                }
                writeln!(out)?;
            }
            writeln!(out, "Closed work item {work_id}")?;
            out
        }
        WorkRequest::Verify { work_id } => render_work_verification(graph, work_id)?,
        WorkRequest::Scopes { work_id } => {
            let id = parse_work_id(&work_id)?;
            let item = graph
                .get_work_item(&id)?
                .ok_or_else(|| anyhow::anyhow!("work item not found: {}", work_id))?;
            let report = WorkScopesReport {
                work_id: item.work_id,
                title: item.title.clone(),
                status: item.status,
                scopes: item.scopes.clone(),
            };
            let mut out = format!(
                "{} scope(s) recorded for {}\n",
                report.scopes.len(),
                work_id
            );
            for scope in &report.scopes {
                writeln!(out, "  - {scope}")?;
            }
            scopes = Some(report);
            out
        }
        WorkRequest::TodoImport { path } => {
            // `path` is caller supplied: this arm serves the daemon's POST
            // /work and POST /note routes, so the value arrives in a request
            // body. It only narrows which held entities are scanned; the scan
            // reads what the graph holds and never a working tree, so no path
            // can reach content Kin does not hold.
            let prefix = todo_import_prefix(layout.working_dir(), path.as_deref())?;
            let pinned;
            let held = match held {
                Some(binding) => binding,
                None => {
                    pinned = kin_core::LocalRepositoryAuthorityBinding::from_layout(layout)
                        .context("pin the repository's held content for the TODO import")?;
                    &pinned
                }
            };
            let todos = held_todos(graph, held, prefix.as_deref())?;
            let existing = graph.list_work_items(&WorkFilter::default())?;
            let mut existing_keys: HashSet<(WorkKind, String, String)> = existing
                .into_iter()
                .flat_map(|item| {
                    item.scopes
                        .into_iter()
                        .filter_map(move |scope| match scope {
                            WorkScope::Artifact(file_id) => {
                                Some((item.kind, item.title.clone(), file_id.0))
                            }
                            _ => None,
                        })
                })
                .collect();
            let mut imported = 0usize;
            let mut skipped = 0usize;
            for todo in &todos {
                let work_kind = match todo.kind.as_str() {
                    "FIXME" => WorkKind::Issue,
                    "HACK" => WorkKind::Debt,
                    _ => WorkKind::Todo,
                };
                let key = (work_kind, todo.body.clone(), todo.file_path.clone());
                if existing_keys.contains(&key) {
                    skipped += 1;
                    continue;
                }
                let scope = WorkScope::Artifact(FilePathId::new(&todo.file_path));
                let item = WorkItem {
                    work_id: WorkId::new(),
                    kind: work_kind,
                    title: todo.body.clone(),
                    description: format!(
                        "Imported from {} (line {})",
                        todo.file_path, todo.line_number
                    ),
                    status: WorkStatus::Proposed,
                    priority: if todo.kind == "FIXME" {
                        Priority::High
                    } else {
                        Priority::Medium
                    },
                    scopes: vec![scope.clone()],
                    acceptance_criteria: vec![],
                    external_refs: vec![],
                    created_by: IdentityRef::human("kin-todo-import"),
                    created_at: Timestamp::now(),
                };
                graph.create_work_item(&item)?;
                graph.create_work_link(&WorkLink::Affects {
                    work_id: item.work_id,
                    scope,
                })?;
                existing_keys.insert(key);
                imported += 1;
            }
            mutated = imported > 0;
            if imported == 0 && skipped == 0 {
                "No TODOs found.\n".to_string()
            } else {
                let mut out = format!("Imported {} TODO(s) as work items.\n", imported);
                if skipped > 0 {
                    writeln!(
                        out,
                        "Skipped {} TODO(s) that were already imported.",
                        skipped
                    )?;
                }
                out
            }
        }
    };

    Ok(WorkExecution {
        response: WorkResponse { text, scopes },
        mutated,
    })
}

/// The repository path a TODO import is narrowed to, from a request's `path`.
///
/// A request names a place in the repository: a path relative to its root, or
/// an absolute path inside its working directory. An absolute path elsewhere,
/// or one that steps out with `..`, is refused. The import reads only what Kin
/// holds, so such a path names nothing it could scan, and refusing says so
/// rather than importing nothing in silence. `None` is the whole repository.
fn todo_import_prefix(
    working_dir: &std::path::Path,
    requested: Option<&str>,
) -> Result<Option<String>> {
    let Some(requested) = requested.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let requested_path = std::path::Path::new(requested);
    let relative = if requested_path.is_absolute() {
        requested_path.strip_prefix(working_dir).map_err(|_| {
            anyhow::anyhow!("the TODO import path {requested} is outside this repository")
        })?
    } else {
        requested_path
    };
    let mut parts = Vec::new();
    for component in relative.components() {
        match component {
            std::path::Component::Normal(part) => parts.push(part.to_str().ok_or_else(|| {
                anyhow::anyhow!("the TODO import path {requested} is not valid UTF-8")
            })?),
            std::path::Component::CurDir => {}
            _ => anyhow::bail!("the TODO import path {requested} leaves this repository"),
        }
    }
    Ok((!parts.is_empty()).then(|| parts.join("/")))
}

/// Every inline marker in the content `graph` holds under `prefix`.
///
/// The workspace tree selects each held entity and repository CAS supplies
/// its exact bytes, verified against the tree's content address, through the
/// pinned binding. Code is scanned with its comment forms and unparsed text
/// with the stricter colon form; content held as bytes has no markers. A
/// prefix that selects nothing Kin holds is refused rather than reported as no
/// markers.
fn held_todos(
    graph: &kin_db::InMemoryGraph,
    held: &kin_core::LocalRepositoryAuthorityBinding,
    prefix: Option<&str>,
) -> Result<Vec<kin_parser::ExtractedTodo>> {
    let tree = graph.resolved_tree();
    let mut todos = Vec::new();
    let mut selected = 0usize;
    for artifact in tree.artifacts_by_path() {
        let Some(path) = artifact.path.as_utf8() else {
            continue;
        };
        if let Some(prefix) = prefix {
            let under = path
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'));
            if !under {
                continue;
            }
        }
        selected += 1;
        // A link's held content is its target, and a submodule holds none.
        let TreeEntry::Blob { hash, .. } = &artifact.entry else {
            continue;
        };
        let bytes = held
            .load_source_blob(hash.0)
            .with_context(|| format!("read the held content of {path}"))?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "{path} is in the workspace tree but its content was never stored, so the \
                     TODO import cannot read it"
                )
            })?;
        let Ok(text) = std::str::from_utf8(&bytes) else {
            continue;
        };
        if text.contains('\0') {
            continue;
        }
        let code = matches!(
            kin_index::FileClassifier::classify(std::path::Path::new(path)),
            kin_index::FileClassification::EntitySource
                | kin_index::FileClassification::ShallowSyntax { .. }
        );
        if code {
            todos.extend(kin_parser::todos_in_text(text, path));
        } else {
            todos.extend(kin_parser::todos_in_unparsed_text(text, path));
        }
    }
    if let Some(prefix) = prefix {
        if selected == 0 {
            anyhow::bail!("{prefix} names nothing Kin holds in this repository");
        }
    }
    Ok(todos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kin_model::WorkStore;

    /// Work is linked to repository scopes. `kin work create`, `link`,
    /// `implement` and `list --scope` refuse a scope naming a symbol outside
    /// the repository, by its address, as an `entity:` scope or by its bare id,
    /// naming the symbol and the command that lists its callers, as the MCP
    /// tools refuse it, and nothing is written. An address naming nothing held
    /// is said to name nothing.
    #[test]
    fn work_scopes_refuse_an_external_symbol_by_what_it_is() {
        let store = crate::commands::external_symbols::fixture::external_store(false);
        let dir = tempfile::tempdir().unwrap();
        let layout = kin_core::KinLayout::new(dir.path().join(".kin"));
        let item = WorkItem {
            work_id: WorkId::new(),
            kind: WorkKind::Task,
            title: "replace the map".into(),
            description: String::new(),
            status: WorkStatus::Proposed,
            priority: Priority::None,
            scopes: vec![],
            acceptance_criteria: vec![],
            external_refs: vec![],
            created_by: IdentityRef::human("cli-user"),
            created_at: Timestamp::now(),
        };
        store.graph.create_work_item(&item).unwrap();
        let work_id = item.work_id.to_string();
        let refused = |request: WorkRequest, command: &str| {
            let error = match execute_work_request(&layout, &store.graph, None, request) {
                Ok(execution) => panic!("{command} answered: {}", execution.response.text),
                Err(error) => format!("{error:#}"),
            };
            assert!(error.contains("Array.map"), "{error}");
            assert!(error.contains(&format!("`{command}`")), "{error}");
            assert!(
                error.contains(&format!("kin refs {}", store.address())),
                "{error}"
            );
            assert!(!error.contains("unrecognized scope"), "{error}");
        };
        for scope in [
            store.address(),
            format!("entity:{}", store.node.id),
            store.node.id.to_string(),
        ] {
            refused(
                WorkRequest::Create {
                    kind: "task".into(),
                    title: "replace the map".into(),
                    description: None,
                    scope: Some(scope.clone()),
                    priority: None,
                },
                "kin work create",
            );
            refused(
                WorkRequest::Link {
                    work_id: work_id.clone(),
                    scope: scope.clone(),
                },
                "kin work link",
            );
            refused(
                WorkRequest::Implement {
                    work_id: work_id.clone(),
                    scope: scope.clone(),
                },
                "kin work implement",
            );
            refused(
                WorkRequest::List {
                    status: None,
                    kind: None,
                    scope: Some(scope),
                },
                "kin work list",
            );
        }
        let unknown = "external_reference:00000000-0000-8000-8000-000000000000";
        let error = match execute_work_request(
            &layout,
            &store.graph,
            None,
            WorkRequest::Link {
                work_id: work_id.clone(),
                scope: unknown.into(),
            },
        ) {
            Ok(execution) => panic!("an unknown address was linked: {}", execution.response.text),
            Err(error) => format!("{error:#}"),
        };
        assert!(
            error.contains("names no symbol outside the repository"),
            "{error}"
        );

        let items = store.graph.list_work_items(&WorkFilter::default()).unwrap();
        assert_eq!(items.len(), 1, "a refused create writes nothing");
        assert!(items[0].scopes.is_empty(), "a refused link writes nothing");
        assert!(store
            .graph
            .get_implementors(&item.work_id)
            .unwrap()
            .is_empty());

        // A held entity is linked as before.
        execute_work_request(
            &layout,
            &store.graph,
            None,
            WorkRequest::Link {
                work_id,
                scope: format!("entity:{}", store.caller.id),
            },
        )
        .expect("a held entity is linked");
    }

    /// The CLI reads a scope through the same parser as the MCP tools, so a bare
    /// path is refused here too rather than becoming a path scope.
    #[test]
    fn the_cli_scope_parser_refuses_a_bare_path() {
        let error = parse_work_scope("src/main.rs").expect_err("a bare path is refused");
        assert!(error.to_string().contains("artifact:<path>"), "{error}");
        assert!(matches!(
            parse_work_scope("artifact:src/main.rs"),
            Ok(WorkScope::Artifact(_))
        ));
    }

    async fn create_in_layout_direct(
        layout: &kin_core::KinLayout,
        kind: String,
        title: String,
        description: Option<String>,
        scope: Option<String>,
        priority: Option<String>,
    ) -> Result<WorkItem> {
        let work_kind: WorkKind = kind.parse().map_err(|e: String| anyhow::anyhow!(e))?;
        let pri: Priority = priority
            .as_deref()
            .unwrap_or("none")
            .parse()
            .map_err(|e: String| anyhow::anyhow!(e))?;
        let scopes = scope
            .map(|s| parse_work_scope(&s))
            .transpose()?
            .into_iter()
            .collect();
        let item = WorkItem {
            work_id: WorkId::new(),
            kind: work_kind,
            title,
            description: description.unwrap_or_default(),
            status: WorkStatus::Proposed,
            priority: pri,
            scopes,
            acceptance_criteria: vec![],
            external_refs: vec![],
            created_by: IdentityRef::human("cli-user"),
            created_at: Timestamp::now(),
        };
        let snap = crate::backend::open_kindb_snapshot(layout)?;
        let graph = snap.graph();
        graph.create_work_item(&item)?;
        for scope in &item.scopes {
            graph.create_work_link(&WorkLink::Affects {
                work_id: item.work_id,
                scope: scope.clone(),
            })?;
        }
        crate::provenance::record_cli_audit_event(
            graph.as_ref(),
            "work.create",
            item.scopes.first().cloned(),
            Some(format!(
                "work_id={}; kind={}; status={}",
                item.work_id, item.kind, item.status
            )),
        )?;
        snap.save()?;
        Ok(item)
    }

    async fn link_in_layout_direct(
        layout: &kin_core::KinLayout,
        work_id: &str,
        scope: &str,
    ) -> Result<WorkScope> {
        let id = parse_work_id(work_id)?;
        let ws = parse_work_scope(scope)?;
        let snap = crate::backend::open_kindb_snapshot(layout)?;
        let graph = snap.graph();
        let mut item = graph
            .get_work_item(&id)?
            .ok_or_else(|| anyhow::anyhow!("work item not found: {}", work_id))?;
        if !item.scopes.contains(&ws) {
            item.scopes.push(ws.clone());
            graph.create_work_item(&item)?;
        }
        graph.create_work_link(&WorkLink::Affects {
            work_id: id,
            scope: ws.clone(),
        })?;
        snap.save()?;
        Ok(ws)
    }

    async fn decompose_in_layout_direct(
        layout: &kin_core::KinLayout,
        parent_work_id: &str,
        child_work_id: &str,
    ) -> Result<()> {
        let parent = parse_work_id(parent_work_id)?;
        let child = parse_work_id(child_work_id)?;
        let snap = crate::backend::open_kindb_snapshot(layout)?;
        let graph = snap.graph();
        graph.create_work_link(&WorkLink::DecomposesTo { parent, child })?;
        snap.save()?;
        Ok(())
    }

    async fn block_in_layout_direct(
        layout: &kin_core::KinLayout,
        blocked_work_id: &str,
        blocker_work_id: &str,
    ) -> Result<()> {
        let blocked = parse_work_id(blocked_work_id)?;
        let blocker = parse_work_id(blocker_work_id)?;
        let snap = crate::backend::open_kindb_snapshot(layout)?;
        let graph = snap.graph();
        graph.create_work_link(&WorkLink::BlockedBy { blocked, blocker })?;
        snap.save()?;
        Ok(())
    }

    async fn implement_in_layout_direct(
        layout: &kin_core::KinLayout,
        work_id: &str,
        scope: &str,
    ) -> Result<WorkScope> {
        let work_id = parse_work_id(work_id)?;
        let scope = parse_work_scope(scope)?;
        let snap = crate::backend::open_kindb_snapshot(layout)?;
        let graph = snap.graph();
        graph.create_work_link(&WorkLink::Implements {
            scope: scope.clone(),
            work_id,
        })?;
        snap.save()?;
        Ok(scope)
    }

    async fn set_status_in_layout_direct(
        layout: &kin_core::KinLayout,
        work_id: &str,
        status: &str,
    ) -> Result<WorkStatus> {
        let work_id = parse_work_id(work_id)?;
        let status = status
            .parse::<WorkStatus>()
            .map_err(|e: String| anyhow::anyhow!(e))?;
        let snap = crate::backend::open_kindb_snapshot(layout)?;
        let graph = snap.graph();
        graph.update_work_status(&work_id, status)?;
        let item = graph.get_work_item(&work_id)?.ok_or_else(|| {
            anyhow::anyhow!("work item not found after status update: {}", work_id)
        })?;
        crate::provenance::record_cli_audit_event(
            graph.as_ref(),
            "work.status",
            item.scopes.first().cloned(),
            Some(format!("work_id={}; status={}", item.work_id, item.status)),
        )?;
        snap.save()?;
        Ok(status)
    }

    async fn close_in_layout_direct(
        layout: &kin_core::KinLayout,
        work_id: &str,
    ) -> Result<Vec<(EntityId, Option<String>)>> {
        let id = parse_work_id(work_id)?;
        let snap = crate::backend::open_kindb_snapshot(layout)?;
        let graph = snap.graph();
        graph.update_work_status(&id, WorkStatus::Done)?;
        let item = graph
            .get_work_item(&id)?
            .ok_or_else(|| anyhow::anyhow!("work item not found after close: {}", work_id))?;
        crate::provenance::record_cli_audit_event(
            graph.as_ref(),
            "work.close",
            item.scopes.first().cloned(),
            Some(format!("work_id={}; uncovered=0", item.work_id)),
        )?;
        snap.save()?;
        Ok(vec![])
    }

    #[tokio::test]
    async fn create_and_link_work_persist_to_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        kin_core::init(dir.path()).unwrap();
        let layout = kin_core::KinLayout::discover(dir.path()).unwrap();

        let item = create_in_layout_direct(
            &layout,
            "task".into(),
            "wire persistence".into(),
            Some("make work writes stick".into()),
            Some("artifact:src/main.rs".into()),
            Some("high".into()),
        )
        .await
        .unwrap();
        let linked_scope =
            link_in_layout_direct(&layout, &item.work_id.to_string(), "file:src/lib.rs")
                .await
                .unwrap();

        let snap = crate::backend::open_kindb_snapshot(&layout).unwrap();
        let graph = snap.graph();
        let stored = graph.get_work_item(&item.work_id).unwrap().unwrap();
        assert!(stored
            .scopes
            .contains(&WorkScope::Artifact(FilePathId::new("src/main.rs"))));
        assert!(stored.scopes.contains(&linked_scope));

        let linked_items = graph.get_work_for_scope(&linked_scope).unwrap();
        assert_eq!(linked_items.len(), 1);
        assert_eq!(linked_items[0].work_id, item.work_id);

        let audit_events = graph.query_audit_events(None, 10).unwrap();
        assert_eq!(audit_events.len(), 1);
        assert_eq!(audit_events[0].action, "work.create");
    }

    #[tokio::test]
    async fn close_work_updates_persisted_status() {
        let dir = tempfile::tempdir().unwrap();
        kin_core::init(dir.path()).unwrap();
        let layout = kin_core::KinLayout::discover(dir.path()).unwrap();
        let item =
            create_in_layout_direct(&layout, "task".into(), "close me".into(), None, None, None)
                .await
                .unwrap();

        let uncovered = close_in_layout_direct(&layout, &item.work_id.to_string())
            .await
            .unwrap();
        assert!(uncovered.is_empty());

        let snap = crate::backend::open_kindb_snapshot(&layout).unwrap();
        let graph = snap.graph();
        let stored = graph.get_work_item(&item.work_id).unwrap().unwrap();
        assert_eq!(stored.status, WorkStatus::Done);

        let audit_events = graph.query_audit_events(None, 10).unwrap();
        assert_eq!(audit_events.len(), 2);
        assert_eq!(audit_events[0].action, "work.close");
    }

    #[tokio::test]
    async fn work_relationships_persist_to_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        kin_core::init(dir.path()).unwrap();
        let layout = kin_core::KinLayout::discover(dir.path()).unwrap();

        let feature = create_in_layout_direct(
            &layout,
            "feature".into(),
            "ship semantic work graph".into(),
            None,
            Some("artifact:src/main.rs".into()),
            None,
        )
        .await
        .unwrap();
        let task = create_in_layout_direct(
            &layout,
            "task".into(),
            "wire graph queries".into(),
            None,
            Some("artifact:src/lib.rs".into()),
            None,
        )
        .await
        .unwrap();
        let blocker = create_in_layout_direct(
            &layout,
            "issue".into(),
            "resolve schema drift".into(),
            None,
            None,
            None,
        )
        .await
        .unwrap();

        decompose_in_layout_direct(
            &layout,
            &feature.work_id.to_string(),
            &task.work_id.to_string(),
        )
        .await
        .unwrap();
        block_in_layout_direct(
            &layout,
            &task.work_id.to_string(),
            &blocker.work_id.to_string(),
        )
        .await
        .unwrap();
        let implementor =
            implement_in_layout_direct(&layout, &task.work_id.to_string(), "file:src/lib.rs")
                .await
                .unwrap();
        let status = set_status_in_layout_direct(&layout, &task.work_id.to_string(), "in_progress")
            .await
            .unwrap();

        let snap = crate::backend::open_kindb_snapshot(&layout).unwrap();
        let graph = snap.graph();

        let children = graph.get_child_work_items(&feature.work_id).unwrap();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].work_id, task.work_id);

        let blockers = graph.get_blockers(&task.work_id).unwrap();
        assert_eq!(blockers.len(), 1);
        assert_eq!(blockers[0].work_id, blocker.work_id);

        let implementors = graph.get_implementors(&task.work_id).unwrap();
        assert_eq!(implementors, vec![implementor]);

        let stored = graph.get_work_item(&task.work_id).unwrap().unwrap();
        assert_eq!(stored.status, status);

        let audit_events = graph.query_audit_events(None, 10).unwrap();
        assert!(audit_events
            .iter()
            .any(|event| event.action == "work.status"));
    }

    #[test]
    fn work_verification_report_uses_runs_and_work_tests() {
        let store = kin_db::InMemoryGraph::new();

        let entity = Entity {
            id: EntityId::new(),
            kind: EntityKind::Function,
            name: "checkout".into(),
            language: LanguageId::Rust,
            fingerprint: SemanticFingerprint {
                algorithm: FingerprintAlgorithm::V1TreeSitter,
                ast_hash: Hash256::from_bytes([0; 32]),
                signature_hash: Hash256::from_bytes([1; 32]),
                behavior_hash: Hash256::from_bytes([2; 32]),
                equivalence_hash: kin_model::Hash256::from_bytes([0; 32]),
                stability_score: 1.0,
            },
            file_origin: Some(FilePathId::new("src/checkout.rs")),
            span: None,
            signature: "fn checkout()".into(),
            visibility: Visibility::Public,
            role: EntityRole::Source,
            doc_summary: None,
            metadata: EntityMetadata::default(),
            lineage_parent: None,
            created_in: None,
            superseded_by: None,
        };
        store.upsert_entity(&entity).unwrap();

        let work = WorkItem {
            work_id: WorkId::new(),
            kind: WorkKind::Feature,
            title: "Ship checkout".into(),
            description: "Implement checkout flow".into(),
            status: WorkStatus::InProgress,
            priority: Priority::High,
            scopes: vec![WorkScope::Entity(entity.id)],
            acceptance_criteria: vec!["passing checkout proof".into()],
            external_refs: vec![],
            created_by: IdentityRef::human("cli-user"),
            created_at: Timestamp::now(),
        };
        store.create_work_item(&work).unwrap();

        let test = TestCase {
            test_id: TestId::new(),
            name: "test_checkout_flow".into(),
            language: "rust".into(),
            kind: TestKind::Unit,
            scopes: vec![WorkScope::Entity(entity.id)],
            runner: TestRunner::Cargo,
            file_origin: Some(FilePathId::new("tests/checkout.rs")),
        };
        store.create_test_case(&test).unwrap();
        store
            .create_test_verifies_work(&test.test_id, &work.work_id)
            .unwrap();

        let run = VerificationRun {
            run_id: VerificationRunId::new(),
            test_ids: vec![test.test_id],
            status: VerificationStatus::Passing,
            runner: TestRunner::Cargo,
            started_at: Timestamp::now(),
            finished_at: Some(Timestamp::now()),
            duration_ms: Some(25),
            evidence_blob: None,
            exit_code: Some(0),
            ran_against: Vec::new(),
            proof: None,
        };
        store.create_verification_run(&run).unwrap();
        store
            .link_run_proves_entity(&run.run_id, &entity.id)
            .unwrap();
        store
            .link_run_proves_work(&run.run_id, &work.work_id)
            .unwrap();

        let report = build_work_verification_report(&store, &work.work_id).unwrap();
        assert_eq!(report.targeted_tests.len(), 1);
        assert_eq!(report.tests_without_passing_run, 0);
        assert!(report.missing_scope_proof.is_empty());
        assert_eq!(report.direct_work_runs.len(), 1);
        assert_eq!(report.direct_work_runs[0].run_id, run.run_id);
    }

    // Every test here drives `execute_work_request`. A `todo_import_in_layout_direct`
    // helper used to stand in for it, reproducing this arm's body verbatim, so no
    // test could observe a change to the product path and the arm shipped
    // unguarded. It is gone rather than fixed: a second copy of the code under
    // test only ever fails in the shape of a passing run.

    /// A repository whose workspace tree holds `members`, admitted from a Git
    /// commit so their content is in repository CAS, and the live graph a
    /// daemon serves over it: the workspace tree, with no entities needed.
    struct HeldTodoFixture {
        _temp: tempfile::TempDir,
        repo: std::path::PathBuf,
        layout: kin_core::KinLayout,
        binding: kin_core::LocalRepositoryAuthorityBinding,
        graph: kin_db::InMemoryGraph,
    }

    fn held_todo_fixture(members: &[(&str, &str)]) -> HeldTodoFixture {
        held_todo_fixture_with_links(members, &[])
    }

    /// As [`held_todo_fixture`], with `links` admitted as links: each names
    /// its place in the repository and the target it points at.
    fn held_todo_fixture_with_links(
        members: &[(&str, &str)],
        links: &[(&str, &std::path::Path)],
    ) -> HeldTodoFixture {
        // Publication proves the Git source more than once, and the proof reads
        // the global excludes through `HOME`; holding the environment domain
        // keeps `HOME` still for the width of the admission.
        let _domain = kin_core::test_env::EnvVarGuard::new();
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let git = |args: &[&str]| {
            let output = crate::commands::test_subprocess::fixture_git(&repo)
                .args(["-c", "maintenance.auto=false", "-c", "gc.auto=0"])
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?} failed: stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "--initial-branch=main"]);
        git(&["config", "user.email", "kin@example.invalid"]);
        git(&["config", "user.name", "Kin"]);
        for (path, text) in members {
            let place = repo.join(path);
            std::fs::create_dir_all(place.parent().unwrap()).unwrap();
            std::fs::write(place, text).unwrap();
        }
        for (path, target) in links {
            #[cfg(unix)]
            std::os::unix::fs::symlink(target, repo.join(path)).unwrap();
            #[cfg(not(unix))]
            let _ = (path, target);
        }
        git(&["add", "--all"]);
        git(&["commit", "--signoff", "-m", "seed held content"]);
        let layout = kin_core::init_from_git(&repo).unwrap().layout;
        let binding = kin_core::LocalRepositoryAuthorityBinding::from_layout(&layout).unwrap();
        let authority =
            crate::commands::repository_authority::ActiveRepositoryAuthority::open(&binding)
                .unwrap();
        let mut snapshot = kin_db::GraphSnapshot::empty();
        snapshot.resolved_tree = authority.workspace().unwrap().tree;
        let graph = kin_db::InMemoryGraph::from_snapshot(snapshot).unwrap();
        HeldTodoFixture {
            _temp: temp,
            repo,
            layout,
            binding,
            graph,
        }
    }

    impl HeldTodoFixture {
        fn import(
            &self,
            held: Option<&kin_core::LocalRepositoryAuthorityBinding>,
            path: Option<&str>,
        ) -> Result<String> {
            execute_work_request(
                &self.layout,
                &self.graph,
                held,
                WorkRequest::TodoImport {
                    path: path.map(str::to_string),
                },
            )
            .map(|execution| execution.response.text)
        }

        fn titles(&self) -> Vec<String> {
            let mut titles: Vec<String> = self
                .graph
                .list_work_items(&WorkFilter::default())
                .unwrap()
                .into_iter()
                .map(|item| item.title)
                .collect();
            titles.sort();
            titles
        }
    }

    /// The import reads what the graph holds, code and unparsed text, and
    /// nothing else: a marker written to the working tree after admission, in
    /// a held entity or in a new one, is not imported, because no working tree
    /// is read. A second import skips what the first took.
    #[test]
    fn todo_import_reads_held_content_and_skips_duplicates() {
        let fixture = held_todo_fixture(&[
            (
                "src/lib.rs",
                "// TODO: keep this stable\nfn f() {}\n// FIXME: make it safer\n",
            ),
            (
                "deploy.yml",
                "# Note on the rollout\n# TODO: pin the image digest\n",
            ),
            ("README.md", "# Note on setup\n\nPlain prose.\n"),
        ]);
        std::fs::write(
            fixture.repo.join("src/lib.rs"),
            "// TODO: only on disk\nfn f() {}\n",
        )
        .unwrap();
        std::fs::write(fixture.repo.join("loose.rs"), "// TODO: never admitted\n").unwrap();

        let first = fixture.import(Some(&fixture.binding), None).unwrap();
        assert!(
            first.contains("Imported 3 TODO(s)") && !first.contains("Skipped"),
            "the first import takes the three held markers: {first:?}"
        );
        assert_eq!(
            fixture.titles(),
            vec![
                "keep this stable".to_string(),
                "make it safer".to_string(),
                "pin the image digest".to_string(),
            ],
            "held markers only: no working-tree text, and no prose heading"
        );

        // A one-shot caller passes no binding and the import pins its own.
        let second = fixture.import(None, None).unwrap();
        assert!(
            second.contains("Imported 0 TODO(s)") && second.contains("Skipped 3 TODO(s)"),
            "the second import skips all three as duplicates: {second:?}"
        );
        assert_eq!(fixture.titles().len(), 3);
    }

    /// A request's path narrows the scan to held entities under it, relative
    /// to the repository or absolute inside its working directory. A path
    /// outside, a `..` step, or a path naming nothing held is refused, and a
    /// refused import writes nothing.
    #[test]
    fn todo_import_narrows_to_a_held_place_and_refuses_anything_else() {
        let fixture = held_todo_fixture(&[
            ("src/lib.rs", "// TODO: inside a subdirectory\n"),
            ("src_other.rs", "// TODO: beside the subdirectory\n"),
            ("top.rs", "// TODO: at the root\n"),
        ]);

        let outside = tempfile::tempdir().unwrap();
        std::fs::write(
            outside.path().join("secret.rs"),
            "// TODO: outside the repo\n",
        )
        .unwrap();
        for refused in [
            outside.path().to_string_lossy().into_owned(),
            "../escape".to_string(),
            "src/../../escape".to_string(),
            "nowhere".to_string(),
        ] {
            assert!(
                fixture.import(None, Some(&refused)).is_err(),
                "{refused} must be refused"
            );
        }
        assert!(
            fixture.titles().is_empty(),
            "a refused import writes nothing"
        );

        fixture.import(None, Some("src")).unwrap();
        assert_eq!(
            fixture.titles(),
            vec!["inside a subdirectory".to_string()],
            "`src` selects what is under src/, not src_other.rs"
        );

        let absolute = fixture.layout.working_dir().join("top.rs");
        fixture
            .import(None, Some(&absolute.to_string_lossy()))
            .unwrap();
        assert_eq!(
            fixture.titles(),
            vec![
                "at the root".to_string(),
                "inside a subdirectory".to_string()
            ],
            "an absolute path inside the working directory names a held place"
        );
    }

    /// A link is held as the name of its target, never as the content it
    /// points at, so a link out of the repository can never lead the import
    /// to read there.
    #[cfg(unix)]
    #[test]
    fn todo_import_never_follows_a_held_link() {
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("secret.rs");
        std::fs::write(&secret, "// TODO: outside the repo\n").unwrap();
        let fixture = held_todo_fixture_with_links(
            &[("src/lib.rs", "// TODO: inside\n")],
            &[("escape.rs", secret.as_path())],
        );
        assert!(
            fixture
                .graph
                .resolved_tree()
                .artifacts_by_path()
                .any(|artifact| artifact.path.as_utf8() == Some("escape.rs")),
            "the link itself is held"
        );
        fixture.import(None, None).unwrap();
        assert_eq!(fixture.titles(), vec!["inside".to_string()]);
    }
}
