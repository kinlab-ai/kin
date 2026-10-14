// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Publishing a branch to a Git remote from the local app, and reading where
//! the branch stands against it.
//!
//! Git is an interoperability projection, never an authority. A publish runs
//! the same exact projection `kin git export` writes, into a staged
//! repository outside the project, takes the branch's commits from there into
//! the project's own Git repository, and lets Git push them with the person's
//! own credentials. A commit Kin took from Git is its original object, and a
//! change made in Kin is the one commit the projection always writes for it,
//! so a second publish extends the first. Nothing is ever forced: a remote
//! branch that moved is refused.
//!
//! Updates admit a fetched branch only when its current Kin head is still
//! an exact imported Git commit. Descendants of published native changes need
//! an additional verified export binding and are refused until it exists.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::StatusCode;
use kin_model::{GitObjectId, RefName, SemanticChangeId};
use serde_json::{json, Value};

use super::review_ui::UiState;
use super::workspace_remotes::{
    entity_changes, local_range, local_range_reading, not_read, not_sent, overall, plural, refused,
    short_ref, since_git, unknown, Act, Ended, LocalBranch, Remote, Side,
};
use crate::daemon_client::DaemonClient;

/// The unsupported lineage, kept explicit at the local app boundary.
pub(super) const GIT_PULL_UNAVAILABLE: &str = "This branch contains Kin-native changes. Kin can \
    update Git-origin histories, but cannot yet admit Git commits descended from a published \
    native change.";

/// How long the repository's daemon is given to say whether this branch can
/// update from Git. Nothing a person waits on waits for it: a session answers
/// from what is known ([`update_ready_known`]), and the page asks for the
/// answer on its own route.
const UPDATE_STATUS_BUDGET: Duration = Duration::from_secs(30);

/// Whether persisted local authority can accept an exact Git update, asked of
/// the repository's daemon now. A caller that finds an ask running joins it
/// rather than asking again. No remote is asked and no daemon is started for
/// this observation.
pub(super) async fn update_ready(state: &UiState) -> Result<(), String> {
    ready(state).await?;
    match state.transfers.ask_update() {
        super::workspace_remotes::UpdateAsk::Join(mut asking) => asking
            .wait_for(Option::is_some)
            .await
            .ok()
            .and_then(|answer| answer.clone())
            .unwrap_or_else(|| Err(UPDATE_UNANSWERED.to_string())),
        super::workspace_remotes::UpdateAsk::Lead(sender) => {
            let answer = ask_update_status(state).await;
            state.transfers.answer_update(sender, answer.clone());
            answer
        }
    }
}

/// What a session says about updating this branch from Git without waiting
/// for the repository's daemon, given what Git itself said (`git`): the
/// daemon's last answer while it is current ([`super::workspace_remotes::UPDATE_READY_KEPT`]),
/// or `None` while no current answer is known, with an ask for one running on
/// its own. An old answer is not repeated as current: the branch or the
/// working copy may have moved since.
///
/// Measured on a busy axum and zod, the daemon took more than the five
/// seconds a session gave it to say, so every page began with ten blank
/// seconds and then said it did not answer. A pull itself still asks first
/// ([`update`]).
pub(super) fn update_ready_known(
    state: &Arc<UiState>,
    git: &Result<(), String>,
) -> Option<Result<(), String>> {
    if let Err(reason) = git {
        return Some(Err(reason.clone()));
    }
    let current = state
        .transfers
        .update_ready()
        .filter(|(age, _)| *age < super::workspace_remotes::UPDATE_READY_KEPT)
        .map(|(_, answer)| answer);
    if current.is_none() && !state.transfers.update_asking() {
        let asking = Arc::clone(state);
        tokio::spawn(async move {
            let _ = update_ready(&asking).await;
        });
    }
    current
}

/// Whether this branch can update from Git, from a fresh answer, or from the
/// ask that is running or this starts, waited for within `limit`. An ask
/// that outlasts the limit runs on, and its answer is kept for the next read.
pub(super) async fn update_ready_within(
    state: &Arc<UiState>,
    limit: Duration,
) -> Result<(), String> {
    if let Some((age, answer)) = state.transfers.update_ready() {
        if age < super::workspace_remotes::UPDATE_READY_KEPT {
            return answer;
        }
    }
    let asking = Arc::clone(state);
    let ask = tokio::spawn(async move { update_ready(&asking).await });
    tokio::time::timeout(limit, ask)
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_else(|| Err(UPDATE_UNANSWERED.to_string()))
}

const UPDATE_UNANSWERED: &str =
    "This repository's daemon did not answer its Git update eligibility.";

/// Ask the repository's daemon, within [`UPDATE_STATUS_BUDGET`], whether
/// this branch can update from Git.
async fn ask_update_status(state: &UiState) -> Result<(), String> {
    #[cfg(test)]
    pause_update_status_for_test(state).await;
    let (_, port) = crate::daemon_client::repo_daemon_recorded_endpoint(state.layout.root());
    let Some(port) = port.filter(|port| *port != 0) else {
        return Err(
            "Open repository content to read whether this branch can update from Git.".to_string(),
        );
    };
    let reading = tokio::time::timeout(UPDATE_STATUS_BUDGET, async {
        let client =
            DaemonClient::from_running_workspace(format!("http://127.0.0.1:{port}"), &state.layout)
                .await?;
        client.git_update_status().await
    })
    .await;
    match reading {
        Ok(Ok(status)) if status.can_update => Ok(()),
        Ok(Ok(status)) => Err(update_reason(
            status.reason.as_deref().unwrap_or(GIT_PULL_UNAVAILABLE),
        )),
        _ => Err(UPDATE_UNANSWERED.to_string()),
    }
}

/// How long an ask of the daemon about updating from Git takes, for the one
/// server state a test names, so a test can make the daemon slow to say.
/// Keyed by the state's address, so no other test's ask waits on it.
#[cfg(test)]
pub(super) static UPDATE_STATUS_PAUSE_FOR_TEST: std::sync::Mutex<Option<(usize, Duration)>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
async fn pause_update_status_for_test(state: &UiState) {
    let owner = state as *const UiState as usize;
    let named = *UPDATE_STATUS_PAUSE_FOR_TEST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((named_state, pause)) = named {
        if named_state == owner {
            tokio::time::sleep(pause).await;
        }
    }
}

/// Protocol refusals are diagnostics. The page states the precondition a
/// person can change without printing local locations or object identities.
fn update_reason(reason: &str) -> String {
    if reason.contains("native Kin changes") {
        return GIT_PULL_UNAVAILABLE.to_string();
    }
    if reason.contains("does not descend") || reason.contains("fast-forward") {
        return "This branch and the fetched Git branch have diverged. Updating would replace recorded work, so nothing was admitted.".to_string();
    }
    if reason.contains("graph-owned changes")
        || reason.contains("workspace projection")
        || reason.contains("working projection")
        || reason.contains("working copy")
    {
        return "This workspace holds uncommitted changes. Commit or preserve them before updating from Git.".to_string();
    }
    if reason.contains("moved") || reason.contains("stale") {
        return "This branch changed after the Git update was selected. Read its standing again before updating.".to_string();
    }
    if reason.contains("born branch") || reason.contains("current branch") {
        return "A Git update needs a current branch with a recorded imported Git commit."
            .to_string();
    }
    "Kin could not verify the fetched Git history against this branch's recorded source. Nothing was admitted.".to_string()
}

/// How long a Git remote is given to say what it holds.
const REMOTE_BUDGET: Duration = Duration::from_secs(15);
/// How long Git is given for a question about the project's own repository.
const LOCAL_BUDGET: Duration = Duration::from_secs(60);
/// How long the projection and its hand-over to the project's repository are
/// given. Both walk the whole history.
const PROJECTION_BUDGET: Duration = Duration::from_secs(30 * 60);
/// How long Git is given to carry a branch to its remote.
const PUSH_BUDGET: Duration = Duration::from_secs(10 * 60);
/// Set on every Git child: never stop to ask at a terminal nobody is at, and
/// answer in the words the refusals below are read by.
const GIT_ENVIRONMENT: &[(&str, &str)] = &[("GIT_TERMINAL_PROMPT", "0"), ("LC_ALL", "C")];

/// What one child printed and how it ended.
#[derive(Debug, Default)]
struct Ran {
    code: Option<i32>,
    out: Vec<String>,
    err: Vec<String>,
}

impl Ran {
    fn succeeded(&self) -> bool {
        self.code == Some(0)
    }

    fn first(&self) -> Option<&str> {
        self.out.first().map(|line| line.trim())
    }

    fn said(&self, words: &str) -> bool {
        self.out
            .iter()
            .chain(&self.err)
            .any(|line| line.contains(words))
    }
}

/// Why a child gave no answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stopped {
    /// The program is not on this machine.
    Missing,
    TimedOut,
    Failed,
}

/// Run `program` in `directory` to its end, keeping what it printed. Its
/// arguments are handed over one by one, never as a line a shell would read.
async fn run(
    program: &OsStr,
    directory: &Path,
    arguments: &[&str],
    budget: Duration,
) -> Result<Ran, Stopped> {
    let out = Mutex::new(Vec::new());
    let err = Mutex::new(Vec::new());
    let ended = {
        let keep = |errors: bool, line: &str| {
            let lines = if errors { &err } else { &out };
            let mut lines = lines
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if lines.len() < 2000 {
                lines.push(line.to_string());
            }
        };
        let arguments: Vec<OsString> = arguments.iter().map(OsString::from).collect();
        tokio::time::timeout(
            budget,
            super::workspace_home::run_child(
                program,
                &arguments,
                Some(directory),
                false,
                GIT_ENVIRONMENT,
                &keep,
            ),
        )
        .await
    };
    let code = match ended {
        Err(_) => return Err(Stopped::TimedOut),
        Ok(Err(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(Stopped::Missing)
        }
        Ok(Err(_)) => return Err(Stopped::Failed),
        Ok(Ok(code)) => code,
    };
    let lines = |kept: Mutex<Vec<String>>| {
        kept.into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    };
    Ok(Ran {
        code,
        out: lines(out),
        err: lines(err),
    })
}

async fn git(directory: &Path, arguments: &[&str], budget: Duration) -> Result<Ran, Stopped> {
    run(OsStr::new("git"), directory, arguments, budget).await
}

/// Whether Git answered `arguments` with success.
async fn git_says_yes(directory: &Path, arguments: &[&str]) -> bool {
    git(directory, arguments, LOCAL_BUDGET)
        .await
        .is_ok_and(|ran| ran.succeeded())
}

fn is_object_id(text: &str) -> bool {
    matches!(text.len(), 40 | 64) && text.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Whether Git can carry a branch for the project at `directory`: Git is on
/// this machine, and the project has a Git repository of its own.
async fn probe(directory: &Path) -> Result<(), String> {
    match git(directory, &["rev-parse", "--show-prefix"], LOCAL_BUDGET).await {
        Err(Stopped::Missing) => Err(
            "Git is not on this machine, so Kin cannot publish to a Git remote from here."
                .to_string(),
        ),
        Err(_) => Err("Git did not answer, so Kin cannot publish to a Git remote.".to_string()),
        // At the top of its own repository Git names no prefix.
        Ok(ran) if ran.succeeded() && ran.first().is_none_or(str::is_empty) => Ok(()),
        Ok(_) => Err(
            "This project has no Git repository of its own, so there is nothing to publish \
             through."
                .to_string(),
        ),
    }
}

/// [`probe`], asked at most once in a while for one repository.
pub(super) async fn ready(state: &UiState) -> Result<(), String> {
    if let Some(known) = state.transfers.git_ready() {
        return known;
    }
    let answer = probe(state.layout.working_dir()).await;
    state.transfers.remember_git_ready(answer.clone());
    answer
}

/// Why Git could not ask a remote what it holds, in words.
fn reach_reason(ran: &Ran, remote: &str) -> String {
    if ran.said("Authentication failed")
        || ran.said("Permission denied")
        || ran.said("could not read Username")
        || ran.said("terminal prompts disabled")
    {
        return format!("{remote} did not accept the Git credentials on this machine.");
    }
    if ran.said("does not appear to be a git repository") || ran.said("No such remote") {
        return format!("The project's own Git repository cannot reach a remote named {remote}.");
    }
    format!("Git could not reach {remote}.")
}

/// The single endpoint Git would publish to for this remote. A separate
/// fetch URL is not evidence about the destination of a push, and several
/// push URLs cannot produce one unambiguous receipt.
async fn publish_target(directory: &Path, remote: &str) -> Result<String, String> {
    match git(directory, &["remote", "get-url", "--push", "--all", "--", remote], LOCAL_BUDGET).await {
        Ok(ran) if ran.succeeded() => match ran.out.as_slice() {
            [only] if !only.trim().is_empty() => Ok(only.trim().to_string()),
            _ => Err(format!("{remote} names several Git publish destinations, so Kin cannot publish it as one operation.")),
        },
        _ => Err(format!("The project's own Git repository has no publish destination named {remote}.")),
    }
}

/// The endpoint Git fetches from for a named remote, independent of any
/// configured publish destination.
async fn fetch_target(directory: &Path, remote: &str) -> Result<String, String> {
    match git(
        directory,
        &["remote", "get-url", "--", remote],
        LOCAL_BUDGET,
    )
    .await
    {
        Ok(ran) if ran.succeeded() => ran
            .first()
            .map(str::to_string)
            .filter(|url| !url.is_empty())
            .ok_or_else(|| format!("{remote} names no Git fetch destination.")),
        _ => Err(format!(
            "The project's own Git repository has no fetch destination named {remote}."
        )),
    }
}

/// The commit `remote` holds for the branch `name` now, asked of the remote
/// through the project's own Git repository. `Ok(None)` is a remote without
/// the branch.
async fn remote_head(
    directory: &Path,
    remote: &str,
    target: &str,
    name: &str,
) -> Result<Option<String>, String> {
    let wanted = format!("refs/heads/{name}");
    match git(
        directory,
        &["ls-remote", "--heads", "--", target, &wanted],
        REMOTE_BUDGET,
    )
    .await
    {
        Ok(ran) if ran.succeeded() => Ok(ran.out.iter().find_map(|line| {
            let (object, named) = line.split_once('\t')?;
            (named.trim() == wanted && is_object_id(object)).then(|| object.to_string())
        })),
        Ok(ran) => Err(reach_reason(&ran, remote)),
        Err(Stopped::TimedOut) => Err(format!("{remote} did not answer Git in time.")),
        Err(Stopped::Missing) => Err("Git is not on this machine.".to_string()),
        Err(Stopped::Failed) => Err("Git could not be started.".to_string()),
    }
}

/// The change a commit Kin wrote names in its own header, read from the
/// commit's printed form. The header is Kin's marker on a commit it projected;
/// it is read to count. Before sending, the staged export's marked head must
/// match the selected semantic head.
fn marked_change(commit: &[String]) -> Option<SemanticChangeId> {
    let committer = commit
        .iter()
        .position(|line| line.starts_with("committer "))?;
    let marked = commit.get(committer + 1)?.strip_prefix("kin-change-id ")?;
    super::ref_grammar::parse_change_id(marked.trim()).ok()
}

/// What Kin holds for a commit a remote names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Held {
    /// The change this commit is.
    Change(SemanticChangeId),
    /// The project's own Git repository does not have the commit.
    NotHere,
    /// The commit is here and Kin holds no change for it.
    NoChange,
}

/// The change behind the commit `object`: the one Kin took it in as, or the
/// one Kin wrote it out for.
async fn change_for(directory: &Path, client: &DaemonClient, object: &str) -> Held {
    if !is_object_id(object) {
        return Held::NotHere;
    }
    let commit = match git(directory, &["cat-file", "commit", object], LOCAL_BUDGET).await {
        Ok(ran) if ran.succeeded() => ran,
        _ => return Held::NotHere,
    };
    // Resolve the exact imported-object alias in repository authority. A
    // bounded history scan cannot prove an older object was never admitted.
    let selector = format!("git:{object}");
    if let Some(change) = client
        .diff(&super::diff::DiffRequest {
            base: Some(selector.clone()),
            head: Some(selector),
            json: true,
            full_bodies: false,
        })
        .await
        .ok()
        .and_then(|answer| answer.report)
        .and_then(|report| report.head.change_id)
    {
        return Held::Change(change);
    }
    if let Some(change) = marked_change(&commit.out) {
        return Held::Change(change);
    }
    Held::NoChange
}

fn unknown_commits(remote: &str) -> String {
    format!("{remote} holds Git commits this repository has not admitted. Kin cannot publish over them.")
}

/// Where the current branch stands against one Git remote: what a publish
/// would send, in changes and entities. Moves nothing.
pub(super) async fn standing(
    state: &UiState,
    client: &DaemonClient,
    local: &LocalBranch,
    branch: &RefName,
    remote: &Remote,
    ready: &Result<(), String>,
    update: &Result<(), String>,
) -> Value {
    let since = since_git(client, local, branch)
        .await
        .unwrap_or(Value::Null);
    let unread =
        |reason: String| {
            let mut standing = not_read(reason);
            standing["pull"] =
                Side::new("blocked")
                    .because(update.as_ref().err().cloned().unwrap_or_else(|| {
                        "Kin could not read this remote's Git history.".to_string()
                    }))
                    .json();
            standing["since_git"] = since.clone();
            standing
        };
    if let Err(reason) = ready {
        return unread(reason.clone());
    }
    let directory = state.layout.working_dir();
    let name = short_ref(branch);
    let Some(head) = local.head_of(branch) else {
        return unread("This repository holds nothing on this branch yet.".to_string());
    };
    let target = match publish_target(directory, &remote.name).await {
        Ok(target) => target,
        Err(reason) => return unread(reason),
    };
    let there = match remote_head(directory, &remote.name, &target, &name).await {
        Ok(there) => there,
        Err(reason) => return unread(reason),
    };
    let push = match &there {
        None => Side::new("would_send").because(format!(
            "{} has no branch named {name} yet, so a publish creates it.",
            remote.name
        )),
        Some(object) => match change_for(directory, client, object).await {
            Held::Change(on_remote) if on_remote == head => Side::new("nothing_to_send"),
            Held::Change(on_remote) => {
                match local_range_reading(client, &local.repository_id, on_remote, head).await {
                    Ok(count) if count.behind == 0 => {
                        let mut side = Side::new("would_send");
                        side.changes = Some(count.ahead);
                        side.entities = Some(count);
                        side
                    }
                    Ok(_) | Err(None) => Side::new("blocked").because(format!(
                        "{} and this repository each hold changes the other does not, and \
                             Git never overwrites a branch that moved.",
                        remote.name
                    )),
                    Err(Some(reason)) => Side::new("unknown").because(reason),
                }
            }
            Held::NotHere | Held::NoChange => {
                Side::new("blocked").because(unknown_commits(&remote.name))
            }
        },
    };
    // A remote may fetch from one repository and publish to another. Its
    // publish reading never proves where an update from its fetch endpoint
    // stands.
    let fetch_head = match fetch_target(directory, &remote.name).await {
        Ok(fetch_target) => remote_head(directory, &remote.name, &fetch_target, &name).await,
        Err(reason) => Err(reason),
    };
    let pull = match &fetch_head {
        Ok(None) => Side::new("nothing_to_bring"),
        Ok(Some(object)) => match change_for(directory, client, object).await {
            Held::Change(remote_change) if remote_change == head => Side::new("nothing_to_bring"),
            Held::Change(remote_change) => match local_range_reading(client, &local.repository_id, remote_change, head).await {
                Ok(count) if count.behind == 0 => Side::new("nothing_to_bring"),
                Ok(count) if count.ahead == 0 && update.is_ok() => {
                    let mut side = Side::new("would_bring");
                    side.changes = Some(count.behind);
                    side.entities = local_range(client, &local.repository_id, head, remote_change).await;
                    side
                },
                Ok(_) => Side::new("blocked").because(update.as_ref().err().cloned().unwrap_or_else(|| "This branch and the fetched Git branch have diverged. An update moves only a fast-forward.".to_string())),
                Err(Some(reason)) => Side::new("unknown").because(reason),
                Err(None) => Side::new("unknown").because("Kin could not resolve the fetched branch's recorded history."),
            },
            Held::NotHere | Held::NoChange => match update {
                Ok(()) => Side::new("unknown").because("The fetch destination holds Git commits this repository has not admitted. An update verifies their ancestry before moving this branch."),
                Err(reason) => Side::new("blocked").because(reason.clone()),
            },
        },
        Err(reason) => Side::new("unknown").because(reason.clone()),
    };
    let state = overall(&push, &pull);
    let summary = match (state, push.changes, push.entities) {
        ("up_to_date", _, _) => format!("This branch matches {}.", remote.name),
        ("ahead", Some(changes), Some(count)) => format!(
            "{changes} {} touching {} {} {} not on {} yet.",
            plural(changes, "change", "changes"),
            count.entities,
            plural(count.entities, "entity", "entities"),
            plural(changes, "is", "are"),
            remote.name
        ),
        _ => push
            .reason
            .clone()
            .or_else(|| pull.reason.clone())
            .unwrap_or_else(|| {
                format!(
                    "Kin could not read where this branch stands on {}.",
                    remote.name
                )
            }),
    };
    json!({
        "read":"complete",
        "state":state,
        "summary":summary,
        "remote_head":there,
        "fetch_head":fetch_head.ok().flatten(),
        "push":push.json(),
        "pull":pull.json(),
        "since_git":since,
    })
}

/// A Git projection of the whole repository, staged outside the project. The
/// directory is removed when this is dropped.
struct Staged {
    _directory: tempfile::TempDir,
    repository: PathBuf,
}

/// Write the repository out as Git, exactly as `kin git export` does, by
/// running that command. The projection is one writer, and this is it.
async fn stage_projection(directory: &Path) -> Result<Staged, String> {
    let failed = || "Kin could not write this repository's changes out as Git commits.".to_string();
    let staging = tempfile::TempDir::new().map_err(|_| failed())?;
    let repository = staging.path().join("projection.git");
    let kin = std::env::current_exe().map_err(|_| failed())?;
    let output = repository.to_string_lossy().into_owned();
    let ran = run(
        kin.as_os_str(),
        directory,
        &["git", "export", "--output", &output],
        PROJECTION_BUDGET,
    )
    .await;
    match ran {
        Ok(ran) if ran.succeeded() => Ok(Staged {
            _directory: staging,
            repository,
        }),
        Ok(ran) => {
            tracing::debug!(lines = ?ran.err, "the Git projection for a publish failed");
            Err(failed())
        }
        Err(_) => Err(failed()),
    }
}

/// The project's own Git branch of the published name, against the commit
/// being published.
#[derive(Debug, Clone, PartialEq, Eq)]
enum OwnBranch {
    Absent,
    Current,
    /// An ancestor of the published commit, at this commit.
    Behind(String),
    /// It holds commits the published commit does not build on.
    Apart,
}

async fn own_branch(directory: &Path, name: &str, published: &str) -> OwnBranch {
    let own = format!("refs/heads/{name}^{{commit}}");
    let at = match git(
        directory,
        &["rev-parse", "--verify", "--quiet", &own],
        LOCAL_BUDGET,
    )
    .await
    {
        Ok(ran) if ran.succeeded() => ran.first().map(str::to_string),
        _ => None,
    };
    let Some(at) = at else {
        return OwnBranch::Absent;
    };
    if at == published {
        OwnBranch::Current
    } else if git_says_yes(directory, &["merge-base", "--is-ancestor", &at, published]).await {
        OwnBranch::Behind(at)
    } else {
        OwnBranch::Apart
    }
}

/// Bring the project's own Git branch to the published commit, and say what
/// became of it: `created`, `moved`, `current` or `left`. A branch that is
/// checked out moves only when Git's own index holds nothing staged, and the
/// index then follows it, so Git reports the working copy against what was
/// published. The working copy itself is never touched.
async fn settle_own_branch(
    directory: &Path,
    name: &str,
    published: &str,
    own: &OwnBranch,
) -> &'static str {
    let branch = format!("refs/heads/{name}");
    let checked_out = git(
        directory,
        &["symbolic-ref", "--quiet", "HEAD"],
        LOCAL_BUDGET,
    )
    .await
    .is_ok_and(|ran| ran.succeeded() && ran.first() == Some(branch.as_str()));
    match own {
        OwnBranch::Current => "current",
        OwnBranch::Apart => "left",
        OwnBranch::Absent => {
            if checked_out && !git_says_yes(directory, &["diff", "--cached", "--quiet"]).await {
                return "left";
            }
            if !git_says_yes(
                directory,
                &[
                    "update-ref",
                    &branch,
                    published,
                    &"0".repeat(published.len()),
                ],
            )
            .await
            {
                return "left";
            }
            if checked_out && !git_says_yes(directory, &["read-tree", published]).await {
                return "created_index_behind";
            }
            "created"
        }
        OwnBranch::Behind(at) => {
            if checked_out
                && !git_says_yes(directory, &["diff-index", "--cached", "--quiet", "HEAD"]).await
            {
                return "left";
            }
            if !git_says_yes(directory, &["update-ref", &branch, published, at]).await {
                return "left";
            }
            if checked_out && !git_says_yes(directory, &["read-tree", published]).await {
                return "moved_index_behind";
            }
            "moved"
        }
    }
}

/// Why Git did not publish, read from what it printed.
fn push_refusal(ran: &Ran, remote: &str) -> String {
    if ran.said("non-fast-forward") || ran.said("fetch first") || ran.said("stale info") {
        return format!(
            "{remote}'s branch moved while this was being published, and Git never overwrites \
             it. Nothing was published."
        );
    }
    if ran.said("Authentication failed")
        || ran.said("Permission denied")
        || ran.said("could not read Username")
        || ran.said("terminal prompts disabled")
    {
        return format!("{remote} did not accept the Git credentials on this machine.");
    }
    if ran.said("remote rejected") || ran.said("hook declined") || ran.said("protected branch") {
        return format!("{remote} refused the branch under its own rules.");
    }
    if ran.said("Could not resolve host")
        || ran.said("unable to access")
        || ran.said("Could not read from remote repository")
    {
        return format!("Git could not reach {remote}.");
    }
    format!("Git did not publish to {remote}.")
}

fn nothing_to_send(head: Option<SemanticChangeId>, summary: String) -> Ended {
    Ended {
        outcome: "nothing_to_send",
        status: StatusCode::OK,
        body: json!({"head":head.map(|head| head.to_string()),"summary":summary}),
    }
}

/// Publish `branch` to the Git remote `remote`, once. The second value says
/// whether Git was asked to push.
pub(super) async fn publish(
    state: &Arc<UiState>,
    operation: kin_model::OperationId,
    client: &DaemonClient,
    local: &LocalBranch,
    branch: &RefName,
    remote: &Remote,
) -> (Ended, bool) {
    let push = Act::Push;
    let directory = state.layout.working_dir();
    let name = short_ref(branch);
    let Some(head) = local.head_of(branch) else {
        let ended = if local.branch.as_ref() == Some(branch) {
            nothing_to_send(
                None,
                "This repository holds nothing on this branch yet.".to_string(),
            )
        } else {
            not_sent(push, format!("This repository has no branch named {name}."))
        };
        return (ended, false);
    };
    if !git_says_yes(directory, &["check-ref-format", "--branch", &name]).await {
        return (
            not_sent(push, format!("Git does not take {name} as a branch name.")),
            false,
        );
    }
    let target = match publish_target(directory, &remote.name).await {
        Ok(target) => target,
        Err(reason) => return (not_sent(push, reason), false),
    };
    let before = match remote_head(directory, &remote.name, &target, &name).await {
        Ok(before) => before,
        Err(reason) => return (not_sent(push, reason), false),
    };

    // The exact projection, staged outside the project, and from there the
    // branch's commits into the project's own Git repository under a ref of
    // Kin's own. Git copies the objects, so they arrive as they were written.
    let staged = match stage_projection(directory).await {
        Ok(staged) => staged,
        Err(reason) => return (not_sent(push, reason), false),
    };
    let holding = format!("refs/kin/export/{name}");
    let source = staged.repository.to_string_lossy().into_owned();
    let taking = format!("+refs/heads/{name}:{holding}");
    let taken = git(
        directory,
        &["fetch", "--no-tags", "--quiet", "--", &source, &taking],
        PROJECTION_BUDGET,
    )
    .await
    .is_ok_and(|ran| ran.succeeded());
    drop(staged);
    let held = format!("{holding}^{{commit}}");
    let published = match git(
        directory,
        &["rev-parse", "--verify", "--quiet", &held],
        LOCAL_BUDGET,
    )
    .await
    {
        Ok(ran) if taken && ran.succeeded() => ran.first().unwrap_or_default().to_string(),
        _ => {
            return (
                not_sent(
                    push,
                    "Git could not take the written commits into the project's own repository.",
                ),
                false,
            )
        }
    };
    if !is_object_id(&published) {
        return (
            not_sent(push, "Git named no commit for the written branch."),
            false,
        );
    }

    if change_for(directory, client, &published).await != Held::Change(head) {
        return (
            not_sent(push, "This branch changed while its Git projection was being written. Read its standing again before publishing."),
            false,
        );
    }
    let own = own_branch(directory, &name, &published).await;
    if own == OwnBranch::Apart {
        return (
            not_sent(
                push,
                format!(
                    "The project's own Git branch {name} holds commits Kin does not hold. \
                     Publishing would leave them behind, so nothing was published."
                ),
            ),
            false,
        );
    }
    if before.as_deref() == Some(published.as_str()) {
        let own = settle_own_branch(directory, &name, &published, &own).await;
        let mut ended = nothing_to_send(
            Some(head),
            format!("{} already holds everything on this branch.", remote.name),
        );
        ended.body["git_commit"] = json!(published);
        ended.body["git_branch"] = json!(own);
        return (ended, false);
    }
    let mut on_remote = None;
    let mut changes = None;
    if let Some(there) = &before {
        // The remote's commit has to be one this branch builds on. Git checks
        // that again when it pushes, and never overwrites.
        if !git_says_yes(
            directory,
            &["cat-file", "-e", &format!("{there}^{{commit}}")],
        )
        .await
        {
            return (not_sent(push, unknown_commits(&remote.name)), false);
        }
        if !git_says_yes(
            directory,
            &["merge-base", "--is-ancestor", there, &published],
        )
        .await
        {
            return (
                not_sent(
                    push,
                    format!(
                        "{} and this repository each hold changes the other does not, and Git \
                         never overwrites a branch that moved. Nothing was published.",
                        remote.name
                    ),
                ),
                false,
            );
        }
        changes = git(
            directory,
            &["rev-list", "--count", &format!("{there}..{published}")],
            LOCAL_BUDGET,
        )
        .await
        .ok()
        .filter(Ran::succeeded)
        .and_then(|ran| ran.first()?.parse::<usize>().ok());
        if let Held::Change(change) = change_for(directory, client, there).await {
            on_remote = Some(change);
        }
    }

    // Sent once. From here the remote may have moved, so nothing below is a
    // plain refusal until a fresh reading says the branch is where it was.
    state.transfers.mark_sent(operation);
    let pushed_ref = format!("{holding}:refs/heads/{name}");
    let pushed = git(
        directory,
        &[
            "push",
            "--porcelain",
            "--no-follow-tags",
            "--",
            &target,
            &pushed_ref,
        ],
        PUSH_BUDGET,
    )
    .await;
    match pushed {
        Ok(ran) if ran.succeeded() => {}
        Ok(ran) => match remote_head(directory, &remote.name, &target, &name).await {
            Ok(after) if after.as_deref() == Some(published.as_str()) => {}
            Ok(after) if after == before => {
                return (refused(push, push_refusal(&ran, &remote.name)), true)
            }
            _ => {
                return (
                    unknown(
                        push,
                        format!(
                            "Git stopped while publishing to {}, and what it holds now could \
                             not be told apart from before.",
                            remote.name
                        ),
                    ),
                    true,
                )
            }
        },
        Err(_) => {
            return (
                unknown(
                    push,
                    format!(
                        "Git did not finish publishing to {} before the app stopped waiting.",
                        remote.name
                    ),
                ),
                true,
            )
        }
    }
    let own = settle_own_branch(directory, &name, &published, &own).await;
    let counted = match on_remote {
        Some(on_remote) => local_range(client, &local.repository_id, on_remote, head).await,
        None => None,
    };
    let ended = Ended {
        outcome: "pushed",
        status: StatusCode::OK,
        body: json!({
            "head":head.to_string(),
            "git_commit":published,
            "git_branch":own,
            "changes":changes.or(counted.map(|count| count.ahead)),
            "entities":counted.map(|count| count.entities),
            "entity_changes":counted.as_ref().map(entity_changes),
        }),
    };
    (ended, true)
}

/// Bring the named Git branch in through the daemon's exact admission
/// transaction. Fetching Git objects is preparation; only the daemon can
/// advance semantic history or the workspace.
pub(super) async fn update(
    state: &Arc<UiState>,
    operation: kin_model::OperationId,
    client: &DaemonClient,
    local: &LocalBranch,
    branch: &RefName,
    remote: &Remote,
) -> (Ended, bool) {
    let pull = Act::Pull;
    if let Err(reason) = update_ready(state).await {
        return (not_sent(pull, reason), false);
    }
    if local.branch.as_ref() != Some(branch) {
        return (not_sent(pull, "A Git update advances the current workspace branch. Open that branch before updating it."), false);
    }
    let Some(before) = local.head_of(branch) else {
        return (
            not_sent(pull, "This branch holds no imported Git head to update."),
            false,
        );
    };
    let directory = state.layout.working_dir();
    let name = short_ref(branch);
    if !git_says_yes(directory, &["check-ref-format", "--branch", &name]).await {
        return (
            not_sent(pull, "Git does not accept this branch name."),
            false,
        );
    }
    let target = match fetch_target(directory, &remote.name).await {
        Ok(target) => target,
        Err(reason) => return (not_sent(pull, reason), false),
    };
    let there = match remote_head(directory, &remote.name, &target, &name).await {
        Ok(Some(there)) => there,
        Ok(None) => {
            return (
                nothing_to_send(
                    Some(before),
                    "The remote holds nothing on this branch.".to_string(),
                ),
                false,
            )
        }
        Err(reason) => return (not_sent(pull, reason), false),
    };
    let holding = format!("refs/kin/update/{name}");
    let taking = format!("+refs/heads/{name}:{holding}");
    let fetched = git(
        directory,
        &["fetch", "--no-tags", "--quiet", "--", &target, &taking],
        PROJECTION_BUDGET,
    )
    .await;
    match fetched {
        Ok(ran) if ran.succeeded() => {}
        Ok(ran) => return (not_sent(pull, reach_reason(&ran, &remote.name)), false),
        Err(_) => {
            return (
                not_sent(
                    pull,
                    "Git did not finish fetching this branch. No semantic update was sent.",
                ),
                false,
            )
        }
    }
    let fetched = match git(
        directory,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{holding}^{{commit}}"),
        ],
        LOCAL_BUDGET,
    )
    .await
    {
        Ok(ran) if ran.succeeded() => ran.first().unwrap_or_default().to_string(),
        _ => {
            return (
                not_sent(pull, "Git did not identify the fetched branch's commit."),
                false,
            )
        }
    };
    if fetched != there {
        return (not_sent(pull, "The remote's branch changed while it was being fetched. Read its standing again before updating."), false);
    }
    let bytes = match hex::decode(&fetched) {
        Ok(bytes) => bytes,
        Err(_) => {
            return (
                not_sent(pull, "Git returned an invalid fetched object identity."),
                false,
            )
        }
    };
    let source_oid = match bytes.as_slice() {
        bytes if bytes.len() == 20 => {
            GitObjectId::sha1(bytes.try_into().expect("checked SHA-1 width"))
        }
        bytes if bytes.len() == 32 => {
            GitObjectId::sha256(bytes.try_into().expect("checked SHA-256 width"))
        }
        _ => {
            return (
                not_sent(pull, "Git returned an unsupported object identity."),
                false,
            )
        }
    };
    let request = crate::git_update::GitUpdateRequest {
        operation_id: operation,
        source_dir: directory.to_path_buf(),
        source_ref: RefName::from_bytes(holding.into_bytes()).expect("valid internal Git ref"),
        source_oid,
        destination_ref: branch.clone(),
        expected_head: before,
    };
    state.transfers.mark_sent(operation);
    match client.git_update_once(&request).await {
        crate::git_update::GitUpdateDispatch::Answered(response) => {
            let counted = local_range(client, &local.repository_id, before, response.head).await;
            let own = own_branch(directory, &name, &fetched).await;
            let git_branch = settle_own_branch(directory, &name, &fetched, &own).await;
            (Ended {
                outcome: if response.head == before { "nothing_to_send" } else { "pulled" },
                status: StatusCode::OK,
                body: json!({
                    "head":response.head.to_string(),
                    "previous_head":response.previous_head.to_string(),
                    "git_commit":fetched,
                    "git_branch":git_branch,
                    "changes":response.admitted_changes,
                    "entities":counted.map(|count| count.entities),
                    "entity_changes":counted.as_ref().map(entity_changes),
                    "workspace":super::workspace_remotes::workspace_json(&response.workspace_follow),
                }),
            }, true)
        },
        crate::git_update::GitUpdateDispatch::Refused { status, .. } if status >= 500 => (
            unknown(pull, "The daemon could not confirm how this Git update ended. It may have admitted the fetched history.".to_string()),
            true,
        ),
        crate::git_update::GitUpdateDispatch::Refused { body, .. } => {
            let after = super::workspace_remotes::local_branch(client).await.ok().and_then(|now| now.head_of(branch));
            if after == Some(before) {
                (refused(pull, update_reason(&body)), true)
            } else {
                (unknown(pull, "The Git update was refused after this branch's reading changed. Read its standing and recent changes before another update.".to_string()), true)
            }
        },
        crate::git_update::GitUpdateDispatch::Lost(_) => (unknown(pull, "The daemon's answer to this Git update was lost. It may have admitted the fetched history.".to_string()), true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ran(code: i32, out: &[&str], err: &[&str]) -> Ran {
        Ran {
            code: Some(code),
            out: out.iter().map(ToString::to_string).collect(),
            err: err.iter().map(ToString::to_string).collect(),
        }
    }

    #[test]
    fn a_commit_kin_wrote_names_its_change_right_after_the_committer() {
        let change = "ab".repeat(32);
        let lines = |extra: &[String]| {
            let mut lines = vec![
                "tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904".to_string(),
                "author Ada <ada@example.invalid> 1700000000 +0000".to_string(),
                "committer Ada <ada@example.invalid> 1700000000 +0000".to_string(),
            ];
            lines.extend_from_slice(extra);
            lines
        };
        let marked = marked_change(&lines(&[format!("kin-change-id {change}"), "work".into()]));
        assert_eq!(marked.map(|id| id.to_string()), Some(change.clone()));
        // A message that merely quotes the header names nothing.
        assert_eq!(
            marked_change(&lines(&["work".into(), format!("kin-change-id {change}")])),
            None
        );
        assert_eq!(
            marked_change(&lines(&["kin-change-id nonsense".into()])),
            None
        );
        assert_eq!(marked_change(&[]), None);
    }

    #[test]
    fn an_object_id_is_sha1_or_sha256_hex_and_nothing_else() {
        assert!(is_object_id(&"0a".repeat(20)));
        assert!(is_object_id(&"0a".repeat(32)));
        assert!(!is_object_id("--upload-pack=sh"));
        assert!(!is_object_id(&"0a".repeat(19)));
        assert!(!is_object_id(&"zz".repeat(20)));
    }

    #[test]
    fn a_git_update_refusal_names_the_actionable_precondition() {
        assert!(
            update_reason("fetched Git commit does not descend from current head")
                .contains("diverged")
        );
        assert!(
            update_reason("commit or preserve current graph-owned changes")
                .contains("uncommitted changes")
        );
        // What the daemon says when tracked bytes changed and were not yet
        // admitted, so the edit is still only in the working copy.
        assert!(update_reason(
            "the tracked working projection changed; admit or preserve those changes before a \
             Git update"
        )
        .contains("uncommitted changes"));
        assert!(update_reason(
            "Git updates after native Kin changes require verified native export bindings"
        )
        .contains("Kin-native changes"));
        let reason =
            update_reason("source closure at /private/project/object/abcdef did not verify");
        assert!(!reason.contains("/private") && !reason.contains("abcdef"));
    }

    #[test]
    fn a_push_git_refused_is_said_in_words_without_a_command() {
        let moved = ran(
            1,
            &["!\trefs/kin/export/main:refs/heads/main\t[rejected] (fetch first)"],
            &["hint: Updates were rejected because the remote contains work that you do not have"],
        );
        assert!(push_refusal(&moved, "origin").contains("origin's branch moved"));
        let credentials = ran(
            128,
            &[],
            &["fatal: Authentication failed for 'https://example.invalid/'"],
        );
        assert!(push_refusal(&credentials, "origin").contains("did not accept the Git credentials"));
        let rules = ran(
            1,
            &["!\ta:b\t[remote rejected] (pre-receive hook declined)"],
            &[],
        );
        assert!(push_refusal(&rules, "origin").contains("under its own rules"));
        let other = ran(1, &[], &["error: something else"]);
        for reason in [
            push_refusal(&moved, "origin"),
            push_refusal(&credentials, "origin"),
            push_refusal(&rules, "origin"),
            push_refusal(&other, "origin"),
            reach_reason(&other, "origin"),
            unknown_commits("origin"),
            GIT_PULL_UNAVAILABLE.to_string(),
        ] {
            assert!(!reason.contains('`'), "{reason}");
            assert!(!reason.contains("git push"), "{reason}");
        }
        assert_eq!(
            push_refusal(&other, "origin"),
            "Git did not publish to origin."
        );
    }

    #[tokio::test]
    async fn publication_reads_one_push_destination_and_refuses_several() {
        let fixture = tempfile::TempDir::new().unwrap();
        let here = fixture.path();
        assert!(git_says_yes(here, &["init", "--quiet", "--initial-branch=main"]).await);
        assert!(git_says_yes(here, &["remote", "add", "origin", "fetch-only"]).await);
        assert_eq!(publish_target(here, "origin").await.unwrap(), "fetch-only");
        assert!(
            git_says_yes(
                here,
                &["remote", "set-url", "--push", "origin", "publish-here"]
            )
            .await
        );
        assert_eq!(
            publish_target(here, "origin").await.unwrap(),
            "publish-here"
        );
        assert!(
            git_says_yes(
                here,
                &[
                    "remote",
                    "set-url",
                    "--add",
                    "--push",
                    "origin",
                    "another-place"
                ]
            )
            .await
        );
        assert!(publish_target(here, "origin")
            .await
            .unwrap_err()
            .contains("several Git publish destinations"));
    }

    #[tokio::test]
    async fn an_unborn_checked_out_branch_follows_its_index_without_discarding_staging() {
        let fixture = tempfile::TempDir::new().unwrap();
        let here = fixture.path();
        assert!(git_says_yes(here, &["init", "--quiet", "--initial-branch=main"]).await);
        // An empty tree and an exact commit are enough to witness a ref and
        // index transition without depending on a person's author settings.
        let tree = git(here, &["mktree"], LOCAL_BUDGET).await.unwrap();
        assert!(tree.succeeded());
        let tree = tree.first().unwrap();
        let commit = git(
            here,
            &[
                "-c",
                "user.name=Test Author",
                "-c",
                "user.email=test@example.invalid",
                "commit-tree",
                tree,
                "-m",
                "initial",
            ],
            LOCAL_BUDGET,
        )
        .await
        .unwrap();
        assert!(commit.succeeded());
        let commit = commit.first().unwrap();
        assert_eq!(
            settle_own_branch(here, "main", commit, &OwnBranch::Absent).await,
            "created"
        );
        assert!(git_says_yes(here, &["diff", "--cached", "--quiet", "HEAD"]).await);
        assert!(git_says_yes(here, &["update-ref", "-d", "refs/heads/main"]).await);
        std::fs::write(here.join("staged.txt"), "staged work").unwrap();
        assert!(git_says_yes(here, &["add", "staged.txt"]).await);
        assert_eq!(
            settle_own_branch(here, "main", commit, &OwnBranch::Absent).await,
            "left"
        );
        assert!(!git_says_yes(here, &["rev-parse", "--verify", "HEAD"]).await);
        let staged = git(here, &["show", ":staged.txt"], LOCAL_BUDGET)
            .await
            .unwrap();
        assert_eq!(staged.first(), Some("staged work"));
    }

    /// Git is run with its arguments handed over one by one, and a program
    /// that is not there is told apart from one that refused.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_child_keeps_what_it_printed_and_a_missing_program_is_said() {
        let here = std::env::temp_dir();
        let answered = run(
            OsStr::new("sh"),
            &here,
            &["-c", "printf 'one\\ntwo\\n'; printf 'no\\n' >&2; exit 3"],
            Duration::from_secs(20),
        )
        .await
        .unwrap();
        assert_eq!(answered.code, Some(3));
        assert_eq!(answered.out, ["one", "two"]);
        assert_eq!(answered.err, ["no"]);
        assert!(answered.said("two") && !answered.succeeded());
        let missing = run(
            OsStr::new("kin-no-such-program-for-this-test"),
            &here,
            &[],
            Duration::from_secs(20),
        )
        .await;
        assert_eq!(missing.unwrap_err(), Stopped::Missing);
        let slow = run(
            OsStr::new("sh"),
            &here,
            &["-c", "sleep 30"],
            Duration::from_millis(200),
        )
        .await;
        assert_eq!(slow.unwrap_err(), Stopped::TimedOut);
    }
}
