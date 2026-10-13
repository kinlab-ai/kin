// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The page that lists this machine's Kin repositories, and how a repository
//! is added from it.
//!
//! Nothing here answers a question about a repository's code. The list is the
//! registry's own records, joined with what a daemon that is already running
//! says about its repository. Reading it never starts a daemon, opens a store
//! or enriches anything, so a machine with fifty repositories lists them at
//! the cost of fifty small reads, and a repository nobody is working in costs
//! nothing more than its registry row.
//!
//! Adding a repository runs Kin's own commands. Converting a project is `kin
//! init`, run as a child of this server by this same binary with the
//! arguments a terminal would give it. The page and the terminal therefore
//! cannot convert a project two different ways, and a long conversion never
//! holds up the server. What the child prints is the operation's progress.
//!
//! A location on this machine is named only here: a reader has to be able to
//! say which directory to add and to tell two repositories of one name apart.
//! Every write needs this page's own token, which no repository's page holds.

use std::collections::{HashMap, VecDeque};
use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use axum::extract::{DefaultBodyLimit, Path as UrlPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncReadExt};

use super::review_ui;
use super::workspace_ui::{repository_key, Hub, Listed, RepositoryEntry};
use crate::daemon_client::DaemonClient;

/// How long one request waits on one repository's running daemon. A reading
/// that has not arrived by then is said to be unknown or left out, and what
/// can still arrive is kept for the next request.
const OBSERVE_BUDGET: Duration = Duration::from_millis(1500);
/// How long Git is given to count a project's commits for the add flow.
const GIT_BUDGET: Duration = Duration::from_millis(1500);
/// The progress lines an operation keeps, newest last.
const LINES_KEPT: usize = 40;
/// What a child said on its error stream, kept for a failure's detail.
const ERRORS_KEPT: usize = 12;
const LINE_MAX_CHARS: usize = 300;
/// The operations remembered. The oldest finished one is dropped first.
const OPERATIONS_KEPT: usize = 50;
const LOCATION_MAX_BYTES: usize = 4096;
const SOURCE_MAX_BYTES: usize = 2048;

/// What the listing page's server remembers while it runs.
#[derive(Default)]
pub(super) struct Home {
    /// Oldest first.
    operations: Mutex<Vec<Arc<Operation>>>,
    /// What each running repository's daemon last said about its working
    /// copy, by repository key. It is read again only once the repository
    /// has moved, so a page that polls costs each daemon one comparison and
    /// not one diff.
    readings: Mutex<HashMap<String, Arc<Slot>>>,
}

pub(super) fn routes() -> Router<Arc<Hub>> {
    Router::new()
        .route("/api/home/session", get(session))
        .route("/api/home", get(home))
        .route("/api/home/inspect", post(inspect))
        .route("/api/home/repositories", post(add))
        .route("/api/home/clients", get(clients).post(connect))
        .route("/api/home/operations", get(operations))
        .route("/api/home/operations/{id}", get(operation))
        .layer(DefaultBodyLimit::max(64 * 1024))
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn refused(status: StatusCode, sentence: impl Into<String>) -> Response {
    (status, Json(json!({ "error": sentence.into() }))).into_response()
}

fn locked<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

// ---------------------------------------------------------------------------
// Who is reading, and where things are
// ---------------------------------------------------------------------------

/// Who the listing page shows its reader as: the name a repository's own
/// pages show, resolved for the repository the server was started in, or for
/// the directory it was started in when that is no repository.
pub(super) fn reader(selected: Option<&kin_core::KinLayout>) -> Option<String> {
    let name = match selected {
        Some(layout) => review_ui::page_actor(layout),
        None => {
            let here = std::env::current_dir().ok()?;
            review_ui::page_actor(&kin_core::KinLayout::for_working_dir(&here))
        }
    };
    Some(name).filter(|name| !name.trim().is_empty())
}

/// This user's home directory, as given and as the system resolves it.
fn homes() -> &'static [PathBuf] {
    static HOMES: OnceLock<Vec<PathBuf>> = OnceLock::new();
    HOMES.get_or_init(|| {
        let Some(home) = directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf())
        else {
            return Vec::new();
        };
        let (resolved, _) = resolve_location(&home);
        if resolved == home {
            vec![home]
        } else {
            vec![home, resolved]
        }
    })
}

/// A location as a person writes it, with their home directory as `~`.
fn location(path: &Path) -> String {
    for home in homes() {
        if let Ok(rest) = path.strip_prefix(home) {
            return if rest.as_os_str().is_empty() {
                "~".to_string()
            } else {
                format!("~/{}", rest.display())
            };
        }
    }
    path.display().to_string()
}

/// The full path a reader's words name, or why they name none.
fn expand(raw: &str) -> Result<PathBuf, String> {
    let text = raw.trim();
    if text.is_empty() {
        return Err("Give the location of a directory.".into());
    }
    if text.len() > LOCATION_MAX_BYTES || text.chars().any(char::is_control) {
        return Err("That location is not one Kin can use.".into());
    }
    let home = || {
        homes().first().cloned().ok_or_else(|| {
            "Kin could not find your home directory. Give the full location.".to_string()
        })
    };
    let path = if text == "~" {
        home()?
    } else if let Some(rest) = text.strip_prefix("~/") {
        home()?.join(rest)
    } else {
        PathBuf::from(text)
    };
    if !path.is_absolute() {
        return Err("Give the full location, starting from / or ~.".into());
    }
    if path
        .components()
        .any(|part| matches!(part, Component::ParentDir))
    {
        return Err("Give the location without `..` in it.".into());
    }
    Ok(path)
}

/// The one form a location is known by, and whether anything is there.
///
/// A location that exists is the path the system resolves it to. One that
/// does not is named from the nearest directory that does, so two spellings
/// of a directory about to be created are still one location.
fn resolve_location(wanted: &Path) -> (PathBuf, bool) {
    if let Ok(found) = wanted.canonicalize() {
        return (found, true);
    }
    let mut missing = Vec::new();
    let mut parent = wanted.to_path_buf();
    while let Some(name) = parent.file_name().map(OsString::from) {
        missing.push(name);
        if !parent.pop() {
            break;
        }
        if let Ok(found) = parent.canonicalize() {
            let named = missing
                .iter()
                .rev()
                .fold(found, |path, name| path.join(name));
            return (named, false);
        }
    }
    (wanted.to_path_buf(), false)
}

// ---------------------------------------------------------------------------
// What is at a location
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Kin,
    Git,
    Plain,
    Missing,
    NotADirectory,
}

impl Kind {
    fn word(self) -> &'static str {
        match self {
            Self::Kin => "kin",
            Self::Git => "git",
            Self::Plain => "plain",
            Self::Missing => "missing",
            Self::NotADirectory => "not_a_directory",
        }
    }
}

/// What the add flow needs to know about one location.
struct Place {
    path: PathBuf,
    kind: Kind,
    /// Whether a directory that exists holds nothing.
    empty: bool,
    /// The Kin repository at exactly this location.
    layout: Option<kin_core::KinLayout>,
    /// The branch a Git project has checked out.
    branch: Option<String>,
    /// A Kin repository that already contains this location.
    inside: Option<kin_core::KinLayout>,
    /// Why a repository is never started over what this location holds: it
    /// is a home directory, the top of a filesystem or Kin's own home.
    never_a_project: Option<&'static str>,
}

/// Look at the location `raw` names. Blocking, and read-only: it asks what
/// kind of directory is there and reads nothing a project holds beyond the
/// name of its checked-out Git branch.
fn look_at(raw: &str) -> Result<Place, String> {
    let (path, exists) = resolve_location(&expand(raw)?);
    let mut place = Place {
        path,
        kind: Kind::Missing,
        empty: false,
        layout: None,
        branch: None,
        inside: None,
        never_a_project: None,
    };
    if exists {
        let directory = match kin_io::WorktreeSource::open(&place.path, &kin_io::Grant::ambient()) {
            Ok(directory) => directory,
            Err(error) if error.kind() == std::io::ErrorKind::NotADirectory => {
                place.kind = Kind::NotADirectory;
                return Ok(place);
            }
            Err(error) => return Err(format!("Kin cannot read this location: {error}")),
        };
        let unreadable = |error: std::io::Error| format!("Kin cannot read this location: {error}");
        place.empty = directory.names("").map_err(unreadable)?.is_empty();
        let git = directory.kind(".git").map_err(unreadable)?;
        place.layout = kin_core::layout::locate_store(&place.path);
        place.kind = match (&place.layout, git) {
            (Some(_), _) => Kind::Kin,
            (None, Some(_)) => Kind::Git,
            (None, None) => Kind::Plain,
        };
        if place.kind == Kind::Plain && !place.empty {
            place.never_a_project = super::init::never_a_project(&place.path);
        }
        if place.kind == Kind::Git {
            place.branch = directory
                .read(".git/HEAD", 1024)
                .ok()
                .flatten()
                .and_then(|head| checked_out_branch(&String::from_utf8_lossy(&head)));
        }
    }
    if place.layout.is_none() {
        // Discovery walks up, so anything it finds from here contains this
        // location and is not this location.
        let from = if exists {
            Some(place.path.as_path())
        } else {
            place.path.parent()
        };
        place.inside = from.and_then(kin_core::KinLayout::discover);
    }
    Ok(place)
}

/// The branch a Git `HEAD` names. A detached head names none.
fn checked_out_branch(head: &str) -> Option<String> {
    let name = head.trim().strip_prefix("ref: refs/heads/")?.trim();
    (!name.is_empty() && !name.chars().any(char::is_control)).then(|| name.to_string())
}

/// What adding the location will do, in one or two plain sentences.
fn note(place: &Place, known: bool) -> String {
    if let Some(reason) = place.never_a_project {
        return reason.to_string();
    }
    if let Some(outer) = &place.inside {
        let (name, _) = super::workspace_ui::display_identity(outer);
        if matches!(place.kind, Kind::Git | Kind::Plain | Kind::Missing) {
            return format!(
                "This location is inside the Kin repository {name}. Adding it makes a separate \
                 repository within that one."
            );
        }
    }
    match place.kind {
        Kind::Kin if known => {
            "This is already a Kin repository on this machine. Opening it changes nothing."
        }
        Kind::Kin => {
            "This is a Kin repository that this machine's list does not hold yet. Opening it \
             adds it to the list and changes nothing in it."
        }
        Kind::Git => {
            "Kin reads this project and its Git history into a Kin repository. The project and \
             its Git history stay as they are."
        }
        Kind::Plain if place.empty => "This directory is empty. Kin starts a new repository in it.",
        Kind::Plain => {
            "This project has no Git history. Kin starts a repository here and everything in \
             it becomes uncommitted work for you to commit."
        }
        Kind::Missing => {
            "Nothing is at this location yet. Kin creates the directory and starts a new \
             repository in it."
        }
        Kind::NotADirectory => "This location is not a directory, so Kin cannot add it.",
    }
    .to_string()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InspectBody {
    path: String,
}

async fn inspect(State(hub): State<Arc<Hub>>, Json(body): Json<InspectBody>) -> Response {
    let reading = {
        let hub = hub.clone();
        tokio::task::spawn_blocking(move || {
            let place = look_at(&body.path)?;
            let registered = hub.catalog.registered_at(&place.path);
            Ok::<_, String>((place, registered))
        })
        .await
    };
    let (place, registered) = match reading {
        Ok(Ok(reading)) => reading,
        Ok(Err(sentence)) => return refused(StatusCode::BAD_REQUEST, sentence),
        Err(error) => return refused(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    };
    // A repository answers at its URL once the registry names it or this
    // server already serves it. Before that the URL would lead nowhere.
    let key = place
        .layout
        .as_ref()
        .map(|layout| repository_key(layout.working_dir()));
    let reachable = key
        .as_ref()
        .filter(|key| registered || hub.state(key).is_some());
    let mut answer = json!({
        "path": place.path.display().to_string(),
        "location": location(&place.path),
        "exists": place.kind != Kind::Missing,
        "kind": place.kind.word(),
        "name": place.path.file_name().map(|name| name.to_string_lossy().into_owned()),
        "registered": registered,
        "note": note(&place, reachable.is_some()),
    });
    if let Some(key) = reachable {
        answer["repository"] = json!({
            "key": key,
            "url": format!("/repositories/{key}/workspace/"),
        });
    }
    if matches!(place.kind, Kind::Kin | Kind::Git | Kind::Plain) {
        answer["empty"] = json!(place.empty);
    }
    // What this page may be asked to do here, so it offers nothing that
    // would be refused. For `clone` the location is the destination.
    answer["actions"] = json!(
        [Action::Convert, Action::Create, Action::Open, Action::Clone]
            .into_iter()
            .filter(|action| precondition(*action, &place).is_none())
            .map(Action::word)
            .collect::<Vec<_>>()
    );
    if place.kind == Kind::Git {
        let mut git = serde_json::Map::new();
        if let Some(commits) = git_commits(&place.path).await {
            git.insert("commits".into(), json!(commits));
        }
        if let Some(branch) = &place.branch {
            git.insert("branch".into(), json!(branch));
        }
        if !git.is_empty() {
            answer["git"] = Value::Object(git);
        }
    }
    if let Some(outer) = &place.inside {
        let entry = super::workspace_ui::entry(outer);
        answer["inside"] = json!({
            "key": entry.key,
            "name": entry.name,
            "url": entry.url,
            "location": location(outer.working_dir()),
        });
    }
    Json(answer).into_response()
}

// ---------------------------------------------------------------------------
// The processes this page starts
// ---------------------------------------------------------------------------

/// A line a person can be shown: no terminal control sequences, no
/// surrounding space, and no longer than a line.
fn shown(raw: &str) -> Option<String> {
    let mut text = String::with_capacity(raw.len());
    let mut characters = raw.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\u{1b}' {
            // A control sequence runs from its introducer to its final byte.
            if characters.peek() == Some(&'[') {
                characters.next();
                for inner in characters.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&inner) {
                        break;
                    }
                }
            }
            continue;
        }
        if !character.is_control() || character == '\t' {
            text.push(character);
        }
    }
    let text = text.trim();
    (!text.is_empty()).then(|| text.chars().take(LINE_MAX_CHARS).collect())
}

/// Whether a line is one of Kin's own log records rather than progress: a
/// timestamp, then a level. A record below an error is diagnostics for
/// whoever runs the server, and is left out of what the page shows.
fn is_log_record(line: &str) -> bool {
    let mut words = line.split_whitespace();
    let stamped = words.next().is_some_and(|stamp| {
        stamp.len() >= 20
            && stamp.ends_with('Z')
            && stamp.as_bytes()[..4].iter().all(u8::is_ascii_digit)
            && stamp.as_bytes()[10] == b'T'
    });
    stamped && matches!(words.next(), Some("TRACE" | "DEBUG" | "INFO" | "WARN"))
}

/// Hand each line a child prints to `said`, as it arrives. A carriage return
/// ends a line too, since that is how a redrawn progress line is written.
async fn relay<R: AsyncRead + Unpin>(
    stream: Option<R>,
    errors: bool,
    said: &(dyn Fn(bool, &str) + Send + Sync),
) {
    let Some(mut stream) = stream else {
        return;
    };
    let mut line = Vec::new();
    let mut chunk = [0u8; 4096];
    let flush = |line: &mut Vec<u8>| {
        if !line.is_empty() {
            said(errors, &String::from_utf8_lossy(line));
            line.clear();
        }
    };
    loop {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                for byte in &chunk[..read] {
                    if matches!(byte, b'\n' | b'\r') {
                        flush(&mut line);
                    } else if line.len() < LINE_MAX_CHARS * 4 {
                        line.push(*byte);
                    }
                }
            }
        }
    }
    flush(&mut line);
}

/// Run `program` to its end and answer its exit status, handing `said` each
/// line it prints and whether the line came on its error stream.
///
/// This is the one place this page starts a process, and it starts two: this
/// same Kin binary, to add a repository exactly as a terminal would, and Git,
/// to count a project's commits before it is added. Neither answers a
/// question about a repository's code.
async fn run_child(
    program: &OsStr,
    arguments: &[OsString],
    directory: Option<&Path>,
    outlives_request: bool,
    said: &(dyn Fn(bool, &str) + Send + Sync),
) -> std::io::Result<Option<i32>> {
    let spawner = kin_io::Spawner::new(&kin_io::Grant::ambient());
    let mut command = spawner.command(program);
    command
        .args(arguments)
        .env("NO_COLOR", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if let Some(directory) = directory {
        command.current_dir(directory);
    }
    let mut command = tokio::process::Command::from(command);
    // A conversion is the reader's work and runs to its end. A question whose
    // answer nobody is waiting for any more is stopped.
    command.kill_on_drop(!outlives_request);
    let mut child = command.spawn()?;
    let (printed, complained) = (child.stdout.take(), child.stderr.take());
    let (_, _, ended) = tokio::join!(
        relay(printed, false, said),
        relay(complained, true, said),
        child.wait(),
    );
    Ok(ended?.code())
}

/// How many commits Git reaches from every ref of the project at
/// `directory`, when it can say within [`GIT_BUDGET`]. The same count `kin
/// init` opens with, asked only to word the add flow.
async fn git_commits(directory: &Path) -> Option<u64> {
    let printed = Mutex::new(String::new());
    let keep = |errors: bool, line: &str| {
        if !errors {
            *locked(&printed) = line.trim().to_string();
        }
    };
    let arguments = ["rev-list", "--count", "--all"].map(OsString::from);
    let ended = tokio::time::timeout(
        GIT_BUDGET,
        run_child(OsStr::new("git"), &arguments, Some(directory), false, &keep),
    )
    .await
    .ok()?
    .ok()?;
    let count = locked(&printed).parse().ok();
    count.filter(|_| ended == Some(0))
}

// ---------------------------------------------------------------------------
// Operations
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Convert,
    Create,
    Open,
    Clone,
    /// Connect coding agents to Kin. It names no repository.
    Connect,
}

impl Action {
    fn word(self) -> &'static str {
        match self {
            Self::Convert => "convert",
            Self::Create => "create",
            Self::Open => "open",
            Self::Clone => "clone",
            Self::Connect => "connect",
        }
    }

    fn failure(self) -> &'static str {
        match self {
            Self::Convert => "Kin could not convert this project.",
            Self::Create => "Kin could not start a repository here.",
            Self::Open => "Kin could not open this repository.",
            Self::Clone => "Kin could not copy this repository.",
            Self::Connect => "Kin could not connect every coding agent.",
        }
    }

    /// What a finished operation's phase says.
    fn finished(self) -> &'static str {
        match self {
            Self::Connect => "The coding agents are connected.",
            _ => "The repository is ready.",
        }
    }
}

/// How far an operation has come, as a code. The page words each stage
/// itself, so it never has to read what a command printed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Stage {
    #[default]
    Starting,
    /// Reading the project and, for a Git project, what Git holds of it.
    Reading,
    /// Building the repository's graph and saving it.
    Graph,
    /// Linking references between entities.
    References,
    /// Reading the project's whole history into the repository.
    History,
    /// The work is done and its result is being reported.
    Finishing,
}

impl Stage {
    fn word(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Reading => "reading",
            Self::Graph => "graph",
            Self::References => "references",
            Self::History => "history",
            Self::Finishing => "finishing",
        }
    }
}

/// The steps a conversion announces, by the names Kin records them under,
/// and the stage each belongs to. The words a step is printed in come from
/// Kin's own table, so this holds no second copy of them.
const CONVERSION_STEPS: [(&str, Stage); 17] = [
    ("check admission blockers", Stage::Reading),
    ("capture Git repository", Stage::Reading),
    ("capture checked-out commit", Stage::Reading),
    ("build Git authority", Stage::Reading),
    ("plan semantic import", Stage::Reading),
    ("derive semantic history", Stage::History),
    ("apply admission policy", Stage::Reading),
    ("prove Git source", Stage::Reading),
    ("stage repository layout", Stage::Graph),
    ("copy Git objects", Stage::Graph),
    ("seal staged content", Stage::Graph),
    ("build bootstrap transaction", Stage::Graph),
    ("commit bootstrap transaction", Stage::Graph),
    ("re-prove Git source", Stage::Graph),
    ("publish repository", Stage::Graph),
    ("prove Git source after publication", Stage::Graph),
    ("seal published content", Stage::Graph),
];

/// `[ 4/15] words` as the step number, the step count and the words.
fn numbered_step(line: &str) -> Option<(u64, u64, &str)> {
    let (counts, words) = line.strip_prefix('[')?.split_once(']')?;
    let (step, steps) = counts.split_once('/')?;
    Some((
        step.trim().parse().ok()?,
        steps.trim().parse().ok()?,
        words.trim(),
    ))
}

/// `checked 12 of 40 ...` as the two counts.
fn checked_counts(line: &str) -> Option<(u64, u64)> {
    let mut words = line.strip_prefix("checked ")?.split_whitespace();
    let done = words.next()?.parse().ok()?;
    let total = (words.next()? == "of")
        .then(|| words.next())
        .flatten()?
        .parse()
        .ok()?;
    Some((done, total))
}

/// The stage one line of a child's output puts its operation in, and the
/// counts the line carries. `printed` says the line came on the child's
/// standard output, where `kin init` and `kin clone` write only their result:
/// progress goes to their error stream.
fn stage_after(current: Stage, printed: bool, line: &str) -> (Stage, Option<(u64, u64)>) {
    if current == Stage::Finishing || printed {
        return (Stage::Finishing, None);
    }
    if let Some((step, steps, words)) = numbered_step(line) {
        // The longest name wins: one step's words begin another's.
        let stage = CONVERSION_STEPS
            .iter()
            .map(|(name, stage)| (kin_core::admission_phase_shown(name), *stage))
            .filter(|(shown, _)| words.starts_with(shown))
            .max_by_key(|(shown, _)| shown.len())
            .map_or(current, |(_, stage)| stage);
        let stage = if stage == Stage::Starting {
            Stage::Reading
        } else {
            stage
        };
        return (stage, Some((step, steps)));
    }
    // A project with no Git history has no numbered steps: it is read in
    // one pass, which `kin init` announces in these words.
    if line
        .trim_start()
        .starts_with(super::init::TAKING_IN_WAITING_WORK)
    {
        return (Stage::Reading, None);
    }
    if let Some(counts) = checked_counts(line) {
        return (Stage::References, Some(counts));
    }
    if line.starts_with("Linking references") || line.starts_with("Linked references") {
        return (Stage::References, None);
    }
    (current, None)
}

enum Outcome {
    Done {
        /// The repository the operation left, when it was about one.
        repository: Option<RepositoryEntry>,
        note: Option<String>,
    },
    Failed {
        error: String,
        detail: String,
    },
}

#[derive(Default)]
struct Progress {
    phase: String,
    stage: Stage,
    /// The counts the latest line carried, when it carried any.
    counts: Option<(u64, u64)>,
    lines: VecDeque<String>,
    errors: VecDeque<String>,
    /// What became of each coding agent a connect operation was asked for.
    clients: Option<Value>,
    finished_at: Option<String>,
    outcome: Option<Outcome>,
}

/// One thing this page was asked to do that takes a while, and how far it
/// has come: adding a repository, or connecting coding agents.
struct Operation {
    id: uuid::Uuid,
    action: Action,
    /// Where the repository is, or will be. A connect operation has none.
    target: Option<PathBuf>,
    started_at: String,
    progress: Mutex<Progress>,
}

fn push_bounded(lines: &mut VecDeque<String>, line: String, kept: usize) {
    if lines.len() == kept {
        lines.pop_front();
    }
    lines.push_back(line);
}

impl Operation {
    fn new(action: Action, target: Option<PathBuf>, phase: &str) -> Arc<Self> {
        Arc::new(Self {
            id: uuid::Uuid::new_v4(),
            action,
            target,
            started_at: now(),
            progress: Mutex::new(Progress {
                phase: phase.to_string(),
                ..Progress::default()
            }),
        })
    }

    fn running(&self) -> bool {
        locked(&self.progress).outcome.is_none()
    }

    fn say(&self, errors: bool, raw: &str) {
        let Some(line) = shown(raw).filter(|line| !is_log_record(line)) else {
            return;
        };
        let mut progress = locked(&self.progress);
        if errors {
            push_bounded(&mut progress.errors, line.clone(), ERRORS_KEPT);
        }
        (progress.stage, progress.counts) = stage_after(progress.stage, !errors, &line);
        progress.phase.clone_from(&line);
        push_bounded(&mut progress.lines, line, LINES_KEPT);
    }

    /// Move to `stage` with words of this server's own, for work that no
    /// child process reports on.
    fn enter(&self, stage: Stage, phase: &str) {
        let mut progress = locked(&self.progress);
        progress.stage = stage;
        progress.counts = None;
        progress.phase = phase.to_string();
        push_bounded(&mut progress.lines, phase.to_string(), LINES_KEPT);
    }

    /// Kin's own words for a failure: its closing `Error:` and what follows
    /// it, or else the last of what the child said on its error stream, or
    /// of what it printed when it said nothing there.
    fn said(&self) -> String {
        let progress = locked(&self.progress);
        let kept = if progress.errors.is_empty() {
            &progress.lines
        } else {
            &progress.errors
        };
        let closing = kept.iter().rposition(|line| line.starts_with("Error:"));
        let skipped = closing.unwrap_or_else(|| kept.len().saturating_sub(ERRORS_KEPT));
        kept.iter()
            .skip(skipped)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Close the operation. Its phase stops being the child's last line,
    /// which is advice for a terminal, and says how it ended. A failed
    /// operation keeps the stage it failed in.
    fn finish(&self, outcome: Outcome) {
        let mut progress = locked(&self.progress);
        progress.finished_at = Some(now());
        progress.counts = None;
        progress.phase = match &outcome {
            Outcome::Done { .. } => {
                progress.stage = Stage::Finishing;
                self.action.finished().to_string()
            }
            Outcome::Failed { error, .. } => error.clone(),
        };
        progress.outcome = Some(outcome);
    }

    fn json(&self) -> Value {
        let progress = locked(&self.progress);
        let mut body = json!({
            "operation_id": self.id,
            "action": self.action.word(),
            "location": self.target.as_deref().map(location),
            "state": match progress.outcome {
                None => "running",
                Some(Outcome::Done { .. }) => "done",
                Some(Outcome::Failed { .. }) => "failed",
            },
            "stage": progress.stage.word(),
            "phase": progress.phase,
            "lines": progress.lines,
            "started_at": self.started_at,
            "finished_at": progress.finished_at,
            "status_url": format!("/api/home/operations/{}", self.id),
        });
        if let Some((done, total)) = progress.counts {
            body["progress"] = json!({ "done": done, "total": total });
        }
        if let Some(clients) = &progress.clients {
            body["clients"] = clients.clone();
        }
        match &progress.outcome {
            Some(Outcome::Done { repository, note }) => {
                if let Some(repository) = repository {
                    body["repository"] = json!(repository);
                }
                if let Some(note) = note {
                    body["note"] = json!(note);
                }
            }
            Some(Outcome::Failed { error, detail }) => {
                body["error"] = json!(error);
                body["detail"] = json!(detail);
            }
            None => {}
        }
        body
    }
}

impl Home {
    /// The operation running for `target`, or a new one. One at a time for a
    /// location: a second request joins the first and starts nothing. An
    /// operation with no location is one of a kind while it runs.
    fn begin(&self, action: Action, target: Option<&Path>, phase: &str) -> (Arc<Operation>, bool) {
        let mut operations = locked(&self.operations);
        if let Some(running) = operations.iter().find(|operation| {
            operation.running()
                && operation.target.as_deref() == target
                && (target.is_some() || operation.action == action)
        }) {
            return (running.clone(), false);
        }
        let operation = Operation::new(action, target.map(Path::to_path_buf), phase);
        operations.push(operation.clone());
        while operations.len() > OPERATIONS_KEPT {
            let Some(oldest) = operations.iter().position(|operation| !operation.running()) else {
                break;
            };
            operations.remove(oldest);
        }
        (operation, true)
    }

    fn find(&self, id: uuid::Uuid) -> Option<Arc<Operation>> {
        locked(&self.operations)
            .iter()
            .find(|operation| operation.id == id)
            .cloned()
    }
}

/// What the exit status of a finished `kin init` or `kin clone` adds to a
/// repository that is ready. Both statuses mean the store is whole.
fn exit_note(code: i32) -> Option<&'static str> {
    match code {
        super::init::EXIT_ENRICHMENT_UNATTESTED => Some(
            "The repository is ready. Kin could not confirm that linking references across \
             entities finished, so some links may arrive later.",
        ),
        super::init::EXIT_GRAPH_SECTION_UNMATERIALIZED => Some(
            "The repository is ready. A record that makes it open faster was not saved, so its \
             next open may take longer.",
        ),
        _ => None,
    }
}

/// Serve the repository at `layout` and name it. Blocking.
fn serve(hub: &Hub, layout: kin_core::KinLayout) -> RepositoryEntry {
    let entry = super::workspace_ui::entry(&layout);
    hub.open(layout);
    entry
}

/// Run this same binary with `arguments`, relaying what it prints, and serve
/// the repository it leaves at the operation's location.
async fn run_kin(hub: Arc<Hub>, operation: Arc<Operation>, arguments: Vec<OsString>) {
    let said = |errors: bool, line: &str| operation.say(errors, line);
    let ended = match std::env::current_exe() {
        Ok(kin) => run_child(kin.as_os_str(), &arguments, None, true, &said).await,
        Err(error) => Err(error),
    };
    let failed = |detail: String| Outcome::Failed {
        error: operation.action.failure().to_string(),
        detail,
    };
    let outcome = match ended {
        Ok(Some(code)) if code == 0 || exit_note(code).is_some() => {
            let target = operation.target.clone();
            let found = tokio::task::spawn_blocking(move || {
                kin_core::layout::locate_store(&target?).map(|layout| serve(&hub, layout))
            })
            .await;
            match found {
                Ok(Some(repository)) => Outcome::Done {
                    repository: Some(repository),
                    note: exit_note(code).map(str::to_string),
                },
                _ => failed("Kin finished, and no repository answers at this location.".into()),
            }
        }
        Ok(Some(_)) => failed(operation.said()),
        Ok(None) => failed("Kin was stopped before it finished.".into()),
        Err(error) => failed(format!("Kin could not be started: {error}")),
    };
    operation.finish(outcome);
}

/// How many entities the repository at `layout` holds, read without starting
/// anything: from its daemon when one is running, and otherwise by one read
/// of its own records, the count `kin init` registers.
async fn count_entities(layout: &kin_core::KinLayout) -> anyhow::Result<usize> {
    if let Some(client) = running_daemon(layout).await {
        let request = super::status::CommandStatusRequest::new(false);
        if let Ok(status) = client.command_status(&request).await {
            return Ok(status.report.semantic_enrichment.entity_count);
        }
    }
    let layout = layout.clone();
    tokio::task::spawn_blocking(move || {
        let binding = kin_core::LocalRepositoryAuthorityBinding::from_layout(&layout)?;
        let unobserved = super::status::EmbeddingCoverage::Unobserved {
            reason: super::status::EmbeddingCoverageUnobserved::NoRunningDaemon,
        };
        let report = super::status::inspect(&layout, &binding, unobserved)?;
        Ok(report.semantic_enrichment.entity_count)
    })
    .await?
}

/// Add a Kin repository the registry does not name: read how much it holds,
/// record it as `kin init` would have, and serve it.
async fn register(hub: Arc<Hub>, operation: Arc<Operation>, layout: kin_core::KinLayout) {
    operation.enter(Stage::Reading, "Reading the repository");
    let outcome = match count_entities(&layout).await {
        Ok(entities) => {
            let recorded = tokio::task::spawn_blocking(move || {
                kin_migrate::update_registry(layout.working_dir(), entities, None)
                    .map(|()| serve(&hub, layout))
            })
            .await;
            match recorded {
                Ok(Ok(repository)) => Outcome::Done {
                    repository: Some(repository),
                    note: None,
                },
                Ok(Err(error)) => Outcome::Failed {
                    error: "Kin could not add this repository to this machine's list.".into(),
                    detail: format!("{error:#}"),
                },
                Err(error) => Outcome::Failed {
                    error: operation.action.failure().into(),
                    detail: error.to_string(),
                },
            }
        }
        Err(error) => Outcome::Failed {
            error: operation.action.failure().into(),
            detail: format!("{error:#}"),
        },
    };
    operation.finish(outcome);
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum AddBody {
    Convert { path: String },
    Create { path: String },
    Open { path: String },
    Clone { source: String, path: String },
}

/// Where `kin clone` is asked to copy from: an address, or a location on this
/// machine with `~` written out.
fn clone_source(raw: &str) -> Result<String, String> {
    let text = raw.trim();
    if text.is_empty() {
        return Err("Give the address or location to copy from.".into());
    }
    if text.len() > SOURCE_MAX_BYTES || text.starts_with('-') || text.chars().any(char::is_control)
    {
        return Err("That address is not one Kin can use.".into());
    }
    if text == "~" || text.starts_with("~/") {
        return Ok(expand(text)?.display().to_string());
    }
    Ok(text.to_string())
}

/// Why `action` cannot be taken on what is at `place`, when it cannot.
fn precondition(action: Action, place: &Place) -> Option<&'static str> {
    let holds_nothing = place.kind == Kind::Missing || (place.kind == Kind::Plain && place.empty);
    match (action, place.kind) {
        // Not an action on a location at all, so no location accepts it.
        (Action::Connect, _) => Some("Coding agents are not connected to a location."),
        (_, Kind::NotADirectory) => Some("This location is not a directory."),
        (Action::Open, Kind::Kin) => None,
        (Action::Open, Kind::Missing) => Some("Nothing is at this location."),
        (Action::Open, _) => Some("This is not a Kin repository yet. Convert it instead."),
        (_, Kind::Kin) => Some("This is already a Kin repository. Open it instead."),
        (Action::Convert, Kind::Missing) => Some(
            "Nothing is at this location. Choose a project that exists, or create a new \
             repository here.",
        ),
        (Action::Convert, Kind::Plain) => place.never_a_project,
        (Action::Convert, _) => None,
        (Action::Create, _) if holds_nothing => None,
        (Action::Create, _) => Some("This directory already holds a project. Convert it instead."),
        (Action::Clone, _) if holds_nothing => None,
        (Action::Clone, _) => {
            Some("The destination already holds something. Choose an empty or a new directory.")
        }
    }
}

async fn add(
    State(hub): State<Arc<Hub>>,
    headers: HeaderMap,
    Json(body): Json<AddBody>,
) -> Response {
    if !review_ui::carries_write_authority(&headers, &hub.token, hub.port) {
        return review_ui::write_refused();
    }
    let (action, raw, source) = match body {
        AddBody::Convert { path } => (Action::Convert, path, None),
        AddBody::Create { path } => (Action::Create, path, None),
        AddBody::Open { path } => (Action::Open, path, None),
        AddBody::Clone { source, path } => match clone_source(&source) {
            Ok(source) => (Action::Clone, path, Some(source)),
            Err(sentence) => return refused(StatusCode::BAD_REQUEST, sentence),
        },
    };
    let reading = {
        let hub = hub.clone();
        tokio::task::spawn_blocking(move || {
            let place = look_at(&raw)?;
            let registered = hub.catalog.registered_at(&place.path);
            Ok::<_, String>((place, registered))
        })
        .await
    };
    let (place, registered) = match reading {
        Ok(Ok(reading)) => reading,
        Ok(Err(sentence)) => return refused(StatusCode::BAD_REQUEST, sentence),
        Err(error) => return refused(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    };
    if let Some(sentence) = precondition(action, &place) {
        return refused(StatusCode::CONFLICT, sentence);
    }
    let target = place.path.as_os_str().to_os_string();
    let arguments: Vec<OsString> = match (&source, place.layout) {
        // A repository the registry already names needs nothing run. It is
        // served, and the operation is finished before it is answered.
        (None, Some(layout)) if registered => {
            let (operation, fresh) = hub.home.begin(action, Some(&place.path), "Opening");
            // A conversion registers its repository before it has finished.
            // A request that arrives then joins it and closes nothing early.
            if !fresh {
                return (StatusCode::ACCEPTED, Json(operation.json())).into_response();
            }
            let serving = hub.clone();
            let served = tokio::task::spawn_blocking(move || serve(&serving, layout)).await;
            operation.finish(match served {
                Ok(repository) => Outcome::Done {
                    repository: Some(repository),
                    note: None,
                },
                Err(error) => Outcome::Failed {
                    error: action.failure().into(),
                    detail: error.to_string(),
                },
            });
            return Json(operation.json()).into_response();
        }
        (None, Some(layout)) => {
            let (operation, fresh) =
                hub.home
                    .begin(action, Some(&place.path), "Reading the repository");
            if fresh {
                tokio::spawn(register(hub.clone(), operation.clone(), layout));
            }
            return (StatusCode::ACCEPTED, Json(operation.json())).into_response();
        }
        (Some(source), _) => ["clone", "--verbose", "--"]
            .into_iter()
            .map(OsString::from)
            .chain([OsString::from(source), target])
            .collect(),
        (None, None) => ["init", "--verbose", "--history", "background", "--"]
            .into_iter()
            .map(OsString::from)
            .chain([target])
            .collect(),
    };
    let (operation, fresh) = hub.home.begin(action, Some(&place.path), "Starting");
    if fresh {
        tokio::spawn(run_kin(hub.clone(), operation.clone(), arguments));
    }
    (StatusCode::ACCEPTED, Json(operation.json())).into_response()
}

// ---------------------------------------------------------------------------
// Coding agents
// ---------------------------------------------------------------------------

/// The most coding agents one request may name. Kin knows fewer.
const CLIENTS_PER_REQUEST: usize = 16;

/// Every coding agent Kin can connect, as `kin setup clients --json` prints
/// it, from the same function. Reads only.
async fn clients() -> Response {
    match tokio::task::spawn_blocking(super::setup::clients_report).await {
        Ok(report) => Json(report).into_response(),
        Err(error) => refused(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConnectBody {
    connect: Vec<String>,
}

/// The ids a request names, each one an id the list above holds, or why the
/// request names nothing Kin can connect.
fn named_clients(report: &Value, asked: &[String]) -> Result<Vec<String>, String> {
    let known: Vec<&str> = report["clients"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|client| client["id"].as_str())
        .collect();
    let mut named: Vec<String> = Vec::new();
    for id in asked {
        let id = id.trim().to_ascii_lowercase();
        if !known.contains(&id.as_str()) {
            return Err("Kin does not know one of these coding agents.".into());
        }
        if !named.contains(&id) {
            named.push(id);
        }
    }
    if named.is_empty() {
        return Err("Name at least one coding agent to connect.".into());
    }
    Ok(named)
}

/// What a connect operation says of each coding agent it was asked for, and
/// the reasons of those that failed.
fn connections_seen(
    connections: &[super::setup::NamedClientConnection],
) -> (Vec<Value>, Vec<String>, Vec<String>) {
    use super::setup::ClientConnected;
    let mut rows = Vec::new();
    let mut lines = Vec::new();
    let mut failures = Vec::new();
    for connection in connections {
        let name = connection.name;
        let (outcome, line) = match &connection.outcome {
            ClientConnected::Connected => ("connected", format!("{name} is connected.")),
            ClientConnected::Waiting => (
                "waiting",
                format!("{name} connects when a repository is next added."),
            ),
            ClientConnected::NotInstalled => (
                "not_installed",
                format!("{name} is not installed on this machine."),
            ),
            ClientConnected::Failed(reason) => {
                failures.push(format!("{name}: {reason}"));
                ("failed", format!("{name} could not be connected."))
            }
        };
        rows.push(json!({ "id": connection.id, "name": name, "outcome": outcome }));
        lines.push(line);
    }
    (rows, lines, failures)
}

/// Connect the named coding agents and nothing else, on a thread of its own:
/// the work writes each agent's own configuration.
async fn connect_clients(operation: Arc<Operation>, ids: Vec<String>) {
    let connected =
        tokio::task::spawn_blocking(move || super::setup::connect_named_clients(&ids)).await;
    let outcome = match connected {
        Ok(Ok(connections)) => {
            let (rows, lines, failures) = connections_seen(&connections);
            for line in &lines {
                operation.enter(Stage::Finishing, line);
            }
            locked(&operation.progress).clients = Some(json!(rows));
            if failures.is_empty() {
                Outcome::Done {
                    repository: None,
                    note: None,
                }
            } else {
                Outcome::Failed {
                    error: operation.action.failure().into(),
                    detail: failures.join("\n"),
                }
            }
        }
        Ok(Err(error)) => Outcome::Failed {
            error: operation.action.failure().into(),
            detail: format!("{error:#}"),
        },
        Err(error) => Outcome::Failed {
            error: operation.action.failure().into(),
            detail: error.to_string(),
        },
    };
    operation.finish(outcome);
}

/// Connect coding agents to Kin: only the ones named, and nothing else on
/// this machine is changed.
async fn connect(
    State(hub): State<Arc<Hub>>,
    headers: HeaderMap,
    Json(body): Json<ConnectBody>,
) -> Response {
    if !review_ui::carries_write_authority(&headers, &hub.token, hub.port) {
        return review_ui::write_refused();
    }
    if body.connect.len() > CLIENTS_PER_REQUEST {
        return refused(
            StatusCode::BAD_REQUEST,
            "Kin does not know one of these coding agents.",
        );
    }
    let report = match tokio::task::spawn_blocking(super::setup::clients_report).await {
        Ok(report) => report,
        Err(error) => return refused(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    };
    let ids = match named_clients(&report, &body.connect) {
        Ok(ids) => ids,
        Err(sentence) => return refused(StatusCode::BAD_REQUEST, sentence),
    };
    let (operation, fresh) = hub.home.begin(Action::Connect, None, "Connecting");
    if fresh {
        tokio::spawn(connect_clients(operation.clone(), ids));
    }
    (StatusCode::ACCEPTED, Json(operation.json())).into_response()
}

async fn operations(State(hub): State<Arc<Hub>>) -> Response {
    let listed: Vec<Value> = locked(&hub.home.operations)
        .iter()
        .rev()
        .map(|operation| operation.json())
        .collect();
    Json(json!({
        "schema": "kin.workspace.home-operations.v1",
        "operations": listed,
    }))
    .into_response()
}

async fn operation(State(hub): State<Arc<Hub>>, UrlPath(id): UrlPath<String>) -> Response {
    match uuid::Uuid::parse_str(&id)
        .ok()
        .and_then(|id| hub.home.find(id))
    {
        Some(operation) => Json(operation.json()).into_response(),
        None => refused(
            StatusCode::NOT_FOUND,
            "This server has no such operation. It keeps them only while it runs.",
        ),
    }
}

// ---------------------------------------------------------------------------
// The list
// ---------------------------------------------------------------------------

async fn session(State(hub): State<Arc<Hub>>) -> Response {
    Json(json!({
        "schema": "kin.workspace.home-session.v1",
        "version": env!("CARGO_PKG_VERSION"),
        "actor": hub.actor,
        "token": hub.token,
    }))
    .into_response()
}

/// The authority and workspace generations a reading was taken at.
type Generation = (u64, u64);

/// How many entities a working copy adds, changes or removes, from Kin's own
/// summary of it. When the only work is in entities Kin holds unparsed, it
/// counts those. Relations are not counted: Kin derives them for itself, and
/// a derived one is not work a reader left behind.
fn changed_in(summary: &super::diff::DiffSummary) -> usize {
    let entities = summary.entities_added + summary.entities_modified + summary.entities_removed;
    let unparsed = summary.artifacts_added + summary.artifacts_updated + summary.artifacts_removed;
    if entities > 0 {
        entities
    } else {
        unparsed
    }
}

/// Whether a working copy holds uncommitted work, once the entities and
/// unparsed entities it changed have been counted. The daemon's status calls a
/// working copy dirty for any difference from its base, which includes the
/// relations Kin linked for itself after the head was written. Work that
/// changed no entity and no unparsed entity is not the person's own, so it
/// does not make the repository differ, as its guided review says too.
fn differs_after_count(dirty: bool, changed: usize) -> bool {
    dirty && changed > 0
}

/// What one repository's daemon last said about its working copy and its
/// newest change, and the state of the repository it said it at.
///
/// Read by a task of its own, never inside a request's budget. A daemon that
/// is busy answers late, and a request that gave up waiting used to leave the
/// row saying nothing was in motion. Now the answer arrives when it arrives
/// and the next read has it.
#[derive(Default)]
struct Reading {
    generation: Option<Generation>,
    /// When this server last read `differs`.
    read_at: Option<tokio::time::Instant>,
    /// Whether the working copy holds work that is not committed.
    differs: Option<bool>,
    /// How many entities that work touches, for `generation`.
    changed: Option<usize>,
    /// When the newest change on the current branch was made, and when this
    /// server learned it. Kept after the repository moves, since it is still
    /// the newest change this server knows of.
    head_change: Option<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)>,
    /// The generation `head_change` was read at.
    head_generation: Option<Generation>,
    in_flight: bool,
    /// A read that failed is not started again before this.
    retry_after: Option<tokio::time::Instant>,
}

struct Slot {
    reading: Mutex<Reading>,
    /// Bumped whenever `reading` gains something, for a request waiting on it.
    moved: tokio::sync::watch::Sender<u64>,
}

impl Default for Slot {
    fn default() -> Self {
        Self {
            reading: Mutex::default(),
            moved: tokio::sync::watch::channel(0).0,
        }
    }
}

impl Slot {
    fn announce(&self) {
        self.moved.send_modify(|version| *version += 1);
    }
}

/// How long the reader may wait on a daemon once no request is waiting on
/// the reader.
const STATUS_PATIENCE: Duration = Duration::from_secs(30);
const COUNT_PATIENCE: Duration = Duration::from_secs(120);
/// How long before a read that failed is tried again.
const READ_RETRY: Duration = Duration::from_secs(5);

/// Ask a running daemon what its working copy holds, cheapest answer first:
/// whether it differs from its base, which the daemon's status already knows,
/// then how many entities that work touches and when the newest change was
/// made. Each answer is published as it arrives.
async fn read_repository(client: DaemonClient, slot: Arc<Slot>) {
    let failed = |slot: &Slot| {
        let mut reading = locked(&slot.reading);
        reading.in_flight = false;
        reading.retry_after = Some(tokio::time::Instant::now() + READ_RETRY);
    };
    let request = super::status::CommandStatusRequest::new(false);
    let Ok(Ok(status)) =
        tokio::time::timeout(STATUS_PATIENCE, client.command_status(&request)).await
    else {
        failed(&slot);
        slot.announce();
        return;
    };
    let generation = (
        status.report.repository.generation,
        status.report.workspace.generation,
    );
    let (count_wanted, head_wanted) = {
        let mut reading = locked(&slot.reading);
        if reading.generation != Some(generation) {
            reading.changed = None;
        }
        reading.generation = Some(generation);
        reading.differs = Some(status.report.workspace.dirty);
        reading.read_at = Some(tokio::time::Instant::now());
        (
            reading.changed.is_none(),
            reading.head_generation != Some(generation),
        )
    };
    slot.announce();
    let count = async {
        if !count_wanted {
            return Ok(None);
        }
        let whole = super::diff::DiffRequest {
            base: None,
            head: None,
            json: false,
            full_bodies: false,
        };
        match tokio::time::timeout(COUNT_PATIENCE, client.diff(&whole)).await {
            Ok(Ok(answer)) => Ok(answer.report.map(|report| changed_in(&report.summary))),
            _ => Err(()),
        }
    };
    let head = async {
        if !head_wanted {
            return Ok(None);
        }
        let newest = super::log::LogRequest { count: 1, skip: 0 };
        match tokio::time::timeout(COUNT_PATIENCE, client.log(&newest)).await {
            // A repository with no change yet has no newest one, which is an
            // answer and not a failure.
            Ok(Ok(answer)) => Ok(Some(
                answer
                    .report
                    .and_then(|report| report.entries.into_iter().next())
                    .map(|entry| entry.timestamp.0),
            )),
            _ => Err(()),
        }
    };
    let (count, head) = tokio::join!(count, head);
    {
        let mut reading = locked(&slot.reading);
        // An answer belongs to the generation it was asked at. If the
        // repository has been read at a later one since, it is dropped.
        if reading.generation == Some(generation) {
            if let Ok(Some(changed)) = count {
                reading.changed = Some(changed);
                reading.differs = reading
                    .differs
                    .map(|dirty| differs_after_count(dirty, changed));
            }
            if let Ok(Some(made)) = head {
                reading.head_generation = Some(generation);
                if let Some(made) = made {
                    reading.head_change = Some((made, chrono::Utc::now()));
                }
            }
        }
        reading.in_flight = false;
        reading.retry_after =
            (count.is_err() || head.is_err()).then(|| tokio::time::Instant::now() + READ_RETRY);
    }
    slot.announce();
}

/// What a row says about a running repository's working copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Working {
    /// Read: whether it differs from its base, and how many entities the
    /// work touches once that count has arrived.
    Read {
        differs: bool,
        changed: Option<usize>,
    },
    /// Not read in time. Never reported as clean.
    Unknown,
}

impl Working {
    fn json(self) -> Value {
        match self {
            Self::Read {
                differs,
                changed: Some(changed),
            } => json!({ "differs": differs, "changed": changed }),
            Self::Read {
                differs,
                changed: None,
            } => json!({ "differs": differs }),
            Self::Unknown => json!({ "unknown": true }),
        }
    }
}

/// What `reading` says for a request that began at `since` and knows the
/// repository to be at `wanted`, when it knows. A reading taken at an older
/// generation, or before the request began when the generation is not known,
/// says nothing about the working copy as it is now.
fn working_from(
    reading: &Reading,
    wanted: Option<Generation>,
    since: tokio::time::Instant,
) -> Working {
    let current = match wanted {
        Some(wanted) => reading.generation.is_some_and(|read| read >= wanted),
        None => reading.read_at.is_some_and(|read| read >= since),
    };
    match reading.differs {
        Some(differs) if current => Working::Read {
            differs,
            changed: reading.changed,
        },
        _ => Working::Unknown,
    }
}

/// When a repository last changed: the time of its newest change when this
/// server has read one, and otherwise what the registry records. The
/// registry's time wins over an older reading it was written after, because
/// a commit refreshes the registry and so says something newer happened.
fn last_change(
    recorded: Option<chrono::DateTime<chrono::Utc>>,
    head: Option<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)>,
    head_is_current: bool,
) -> Option<chrono::DateTime<chrono::Utc>> {
    match (head, recorded) {
        (Some((made, _)), _) if head_is_current => Some(made),
        (Some((_, learned)), Some(recorded)) if recorded > learned => Some(recorded),
        (Some((made, _)), _) => Some(made),
        (None, recorded) => recorded,
    }
}

/// What a running daemon said about its repository. A reading that did not
/// arrive in time is left out or said to be unknown, never guessed.
struct Observed {
    running: bool,
    branch: Option<String>,
    working: Option<Working>,
    agents: Option<Value>,
    merge_in_progress: Option<bool>,
    activity: super::daemon_activity::DaemonActivity,
    changed_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// The daemon already serving `layout`, when its recorded endpoint answers
/// and proves it is that repository's. Reads recorded endpoint metadata and
/// verifies health without discovery, cleanup, startup or process control.
async fn running_daemon(layout: &kin_core::KinLayout) -> Option<DaemonClient> {
    let (pid, port) = crate::daemon_client::repo_daemon_recorded_endpoint(layout.root());
    let port = pid.and(port).filter(|port| *port != 0)?;
    DaemonClient::from_running_workspace(format!("http://127.0.0.1:{port}"), layout)
        .await
        .ok()
}

/// Who the daemon lists as connected, by the names a reader knows them by.
fn agents_seen(payload: &Value) -> Option<Value> {
    let agents = payload["agents"].as_array()?;
    let mut names: Vec<String> = Vec::new();
    let mut active = 0;
    for agent in agents {
        if agent["state"] == "active" {
            active += 1;
        }
        let name = super::workspace_agents::agent_display_name(
            agent["vendor"].as_str().unwrap_or_default(),
            agent["client"].as_str().unwrap_or_default(),
        );
        if !names.contains(&name) {
            names.push(name);
        }
    }
    Some(json!({ "active": active, "connected": agents.len(), "names": names }))
}

impl Home {
    fn slot(&self, key: &str) -> Arc<Slot> {
        locked(&self.readings)
            .entry(key.to_string())
            .or_default()
            .clone()
    }
}

async fn observe(
    hub: &Hub,
    listed: &Listed,
    standing: &kin_core::history_state::HistoryStanding,
) -> Observed {
    use super::daemon_activity::{activity_without_health, read_activity, Reachability};

    let since = tokio::time::Instant::now();
    let deadline = since + OBSERVE_BUDGET;
    let recorded = listed
        .record
        .as_ref()
        .and_then(|record| recorded_time(&record.at));
    let slot = hub.home.slot(&listed.entry.key);
    let history = super::daemon_activity::running_history(standing);
    let Ok(Some(client)) = tokio::time::timeout_at(deadline, running_daemon(&listed.layout)).await
    else {
        // No daemon answers. A history derivation runs in a worker of its
        // own, so it can still be what this repository is doing.
        let head = locked(&slot.reading).head_change;
        return Observed {
            running: false,
            branch: None,
            working: None,
            agents: None,
            merge_in_progress: None,
            activity: activity_without_health(Reachability::Gone, history.as_ref()),
            changed_at: last_change(recorded, head, false),
        };
    };
    let list = super::branch::BranchRequest::List;
    let listing = super::conflicts::ConflictsRequest { bodies: false };
    let root = listed.layout.working_dir().display().to_string();
    let (branches, conflicts, agents, activity) = tokio::join!(
        tokio::time::timeout_at(deadline, client.branch(&list)),
        tokio::time::timeout_at(deadline, client.conflicts(&listing)),
        tokio::time::timeout_at(deadline, client.agents()),
        tokio::time::timeout_at(
            deadline,
            read_activity(client.base_url().to_string(), root, Reachability::Answering),
        ),
    );
    let branches = branches
        .ok()
        .and_then(Result::ok)
        .and_then(|answer| answer.report);
    let wanted = branches
        .as_ref()
        .map(|report| (report.authority_generation, report.workspace_generation));

    // Start the reader unless what it would read is already held for this
    // generation, then wait for it only as long as this request has.
    let mut moved = slot.moved.subscribe();
    {
        let mut reading = locked(&slot.reading);
        let settled = matches!(
            working_from(&reading, wanted, since),
            Working::Read {
                changed: Some(_),
                ..
            }
        ) && wanted.is_some_and(|wanted| reading.head_generation == Some(wanted));
        let may_start = reading
            .retry_after
            .is_none_or(|after| tokio::time::Instant::now() >= after);
        if !settled && !reading.in_flight && may_start {
            reading.in_flight = true;
            tokio::spawn(read_repository(client.clone(), slot.clone()));
        }
    }
    let (working, head, head_is_current) = loop {
        {
            let reading = locked(&slot.reading);
            let working = working_from(&reading, wanted, since);
            let counted = matches!(
                working,
                Working::Read {
                    changed: Some(_),
                    ..
                }
            );
            if counted || !reading.in_flight {
                let current = wanted.is_some_and(|wanted| reading.head_generation == Some(wanted));
                break (working, reading.head_change, current);
            }
        }
        if tokio::time::timeout_at(deadline, moved.changed())
            .await
            .is_err()
        {
            let reading = locked(&slot.reading);
            let current = wanted.is_some_and(|wanted| reading.head_generation == Some(wanted));
            break (
                working_from(&reading, wanted, since),
                reading.head_change,
                current,
            );
        }
    };
    Observed {
        running: true,
        branch: branches.as_ref().and_then(|report| {
            report
                .branches
                .iter()
                .find(|branch| branch.active)
                .map(|branch| {
                    let full = branch.name.to_string();
                    full.strip_prefix("refs/heads/")
                        .unwrap_or(&full)
                        .to_string()
                })
        }),
        working: Some(working),
        agents: agents
            .ok()
            .and_then(Result::ok)
            .as_ref()
            .and_then(agents_seen),
        merge_in_progress: conflicts
            .ok()
            .and_then(Result::ok)
            .and_then(|answer| answer.report)
            .map(|report| {
                report.record.is_some_and(|record| {
                    matches!(record.state, kin_model::MergeTransactionState::InProgress)
                })
            }),
        activity: activity.unwrap_or_else(|_| {
            activity_without_health(Reachability::NotAnswering, history.as_ref())
        }),
        changed_at: last_change(recorded, head, head_is_current),
    }
}

/// A registry timestamp in one form, or nothing when it is not a time.
fn recorded_time(raw: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|time| time.with_timezone(&chrono::Utc))
}

async fn home(State(hub): State<Arc<Hub>>) -> Response {
    let catalog = hub.catalog.clone();
    let read = tokio::task::spawn_blocking(move || {
        let listing = catalog.read();
        // Each store's own record of its history: two small reads, and no
        // daemon asked.
        let standings: Vec<_> = listing
            .repositories
            .iter()
            .map(|listed| kin_core::history_state::standing(&listed.layout))
            .collect();
        (listing, standings)
    })
    .await;
    let (listing, standings) = match read {
        Ok(read) => read,
        Err(error) => return refused(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    };
    let observed = futures_util::future::join_all(
        listing
            .repositories
            .iter()
            .zip(&standings)
            .map(|(listed, standing)| observe(&hub, listed, standing)),
    )
    .await;
    let mut rows: Vec<(bool, Option<chrono::DateTime<chrono::Utc>>, String, Value)> = listing
        .repositories
        .iter()
        .zip(&standings)
        .zip(observed)
        .map(|((listed, standing), observed)| {
            let entry = &listed.entry;
            let mut row = json!({
                "key": entry.key,
                "name": entry.name,
                "url": entry.url,
                "location": location(listed.layout.working_dir()),
                "available": true,
                "entities": listed.record.as_ref().map(|record| record.entities),
                "last_change_at": observed
                    .changed_at
                    .map(|time| time.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
                "runtime": if observed.running { "running" } else { "idle" },
                "activity": observed.activity,
                "history": super::workspace_ui::history_json(standing),
            });
            if let Some(owner) = &entry.owner {
                row["owner"] = json!(owner);
            }
            if let Some(host) = &entry.host {
                row["host"] = json!(host);
            }
            if let Some(branch) = observed.branch {
                row["branch"] = json!(branch);
            }
            if let Some(working) = observed.working {
                row["working"] = working.json();
            }
            if let Some(agents) = observed.agents {
                row["agents"] = agents;
            }
            if let Some(merging) = observed.merge_in_progress {
                row["merge_in_progress"] = json!(merging);
            }
            (
                observed.running,
                observed.changed_at,
                entry.name.to_lowercase(),
                row,
            )
        })
        .collect();
    // Repositories someone is working in first, then the most recently
    // changed, then by name so equal rows keep one order between reads.
    rows.sort_by(|left, right| {
        right
            .0
            .cmp(&left.0)
            .then_with(|| right.1.cmp(&left.1))
            .then_with(|| left.2.cmp(&right.2))
    });
    Json(json!({
        "schema": "kin.workspace.home.v1",
        "version": env!("CARGO_PKG_VERSION"),
        "read_at": now(),
        "repositories": rows.into_iter().map(|row| row.3).collect::<Vec<_>>(),
        "unavailable": listing.unavailable,
        "warning": listing.warning,
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_location_is_full_or_begins_at_home_and_never_climbs() {
        assert!(expand("relative/project").is_err());
        assert!(expand("  ").is_err());
        assert!(expand("/work/../etc").is_err());
        assert!(expand("/work/pro\u{7}ject").is_err());
        assert_eq!(
            expand(" /work/project ").unwrap(),
            Path::new("/work/project")
        );
        if let Some(home) = homes().first() {
            assert_eq!(expand("~").unwrap(), *home);
            assert_eq!(expand("~/code/project").unwrap(), home.join("code/project"));
            assert_eq!(location(&home.join("code/project")), "~/code/project");
            assert_eq!(location(home), "~");
        }
        // Another user's home is not this one's to name.
        assert!(expand("~other/project").is_err());
    }

    #[test]
    fn a_location_not_yet_there_is_named_from_the_directory_that_is() {
        let parent = tempfile::tempdir().unwrap();
        let resolved = parent.path().canonicalize().unwrap();
        assert_eq!(resolve_location(parent.path()), (resolved.clone(), true));
        assert_eq!(
            resolve_location(&parent.path().join("new/project")),
            (resolved.join("new/project"), false)
        );
    }

    #[test]
    fn what_is_at_a_location_decides_what_adding_it_means() {
        let root = tempfile::tempdir().unwrap();
        let at = |name: &str| root.path().join(name).display().to_string();
        std::fs::create_dir(root.path().join("empty")).unwrap();
        std::fs::create_dir_all(root.path().join("project/.git")).unwrap();
        std::fs::write(
            root.path().join("project/.git/HEAD"),
            "ref: refs/heads/trunk\n",
        )
        .unwrap();
        std::fs::create_dir(root.path().join("plain")).unwrap();
        std::fs::write(root.path().join("plain/notes.txt"), "work").unwrap();

        let missing = look_at(&at("absent")).unwrap();
        assert_eq!((missing.kind, missing.empty), (Kind::Missing, false));
        let empty = look_at(&at("empty")).unwrap();
        assert_eq!((empty.kind, empty.empty), (Kind::Plain, true));
        let git = look_at(&at("project")).unwrap();
        assert_eq!((git.kind, git.empty), (Kind::Git, false));
        assert_eq!(git.branch.as_deref(), Some("trunk"));
        let plain = look_at(&at("plain")).unwrap();
        assert_eq!((plain.kind, plain.empty), (Kind::Plain, false));
        let entry = look_at(&at("plain/notes.txt")).unwrap();
        assert_eq!(entry.kind, Kind::NotADirectory);

        assert!(precondition(Action::Create, &missing).is_none());
        assert!(precondition(Action::Create, &empty).is_none());
        assert!(precondition(Action::Create, &plain).is_some());
        assert!(precondition(Action::Create, &git).is_some());
        assert!(precondition(Action::Convert, &git).is_none());
        // A project Git has never recorded is added too. Nothing in it is
        // recorded until its reader commits, and the note says so.
        assert!(precondition(Action::Convert, &plain).is_none());
        assert_eq!(
            note(&plain, false),
            "This project has no Git history. Kin starts a repository here and everything in \
             it becomes uncommitted work for you to commit."
        );
        // Unless it is a place that is never one project.
        let mut everything = look_at(&at("plain")).unwrap();
        everything.never_a_project = super::super::init::never_a_project(Path::new("/"));
        let reason = precondition(Action::Convert, &everything).expect("never a project");
        assert_eq!(note(&everything, false), reason);
        assert!(reason.contains("top of the filesystem"), "{reason}");
        assert!(precondition(Action::Convert, &empty).is_none());
        assert!(precondition(Action::Convert, &missing).is_some());
        assert!(precondition(Action::Open, &git).is_some());
        assert!(precondition(Action::Open, &missing).is_some());
        assert!(precondition(Action::Clone, &missing).is_none());
        assert!(precondition(Action::Clone, &plain).is_some());
        for action in [Action::Convert, Action::Create, Action::Open, Action::Clone] {
            assert!(precondition(action, &entry).is_some());
        }
        // No sentence a reader is shown speaks of what a project is stored as.
        for place in [&missing, &empty, &git, &plain, &entry] {
            for known in [false, true] {
                assert!(!note(place, known).to_lowercase().contains("file"));
            }
        }
    }

    #[test]
    fn a_detached_head_names_no_branch() {
        assert_eq!(
            checked_out_branch("ref: refs/heads/feat/list\n").as_deref(),
            Some("feat/list")
        );
        assert_eq!(checked_out_branch("4f2a9c0d\n"), None);
        assert_eq!(checked_out_branch("ref: refs/heads/\n"), None);
    }

    #[test]
    fn a_progress_line_is_shown_without_terminal_control() {
        assert_eq!(
            shown("\u{1b}[1m  Reading\u{1b}[0m 12 of 40\t\u{7}").as_deref(),
            Some("Reading 12 of 40")
        );
        assert_eq!(shown(" \u{1b}[2K "), None);
        assert!(is_log_record(
            "2026-01-02T03:04:05.678901Z  WARN kin_core::env: an override is active"
        ));
        assert!(!is_log_record(
            "2026-01-02T03:04:05.678901Z ERROR kin_core::store: the store could not be read"
        ));
        assert!(!is_log_record("checked 12 of 40"));
        assert!(!is_log_record("WARN is a word a project may print"));
        assert_eq!(
            shown(&"x".repeat(LINE_MAX_CHARS + 50)).map(|line| line.chars().count()),
            Some(LINE_MAX_CHARS)
        );
    }

    #[test]
    fn one_operation_runs_for_a_location_at_a_time() {
        let home = Home::default();
        let (first, fresh) = home.begin(Action::Convert, Some(Path::new("/work/a")), "Starting");
        assert!(fresh);
        let (joined, fresh) = home.begin(Action::Convert, Some(Path::new("/work/a")), "Starting");
        assert!(!fresh);
        assert_eq!(joined.id, first.id);
        let (other, fresh) = home.begin(Action::Convert, Some(Path::new("/work/b")), "Starting");
        assert!(fresh);
        assert_ne!(other.id, first.id);

        first.say(true, "Reading 4 of 9");
        // A log record is for whoever runs the server, not for the page.
        first.say(
            true,
            "2026-01-02T03:04:05.678901Z  WARN kin_core::env: an override is active",
        );
        first.say(true, "[ 1/15] check that the project can be imported...");
        let running = first.json();
        assert_eq!(running["state"], "running");
        assert_eq!(running["progress"], json!({"done": 1, "total": 15}));
        assert_eq!(
            running["phase"],
            "[ 1/15] check that the project can be imported..."
        );
        assert_eq!(running["lines"].as_array().unwrap().len(), 2);
        assert_eq!(running["finished_at"], Value::Null);
        first.say(true, "Error: admit the checked-out Git commit");
        first.say(true, "Caused by:");
        first.say(true, "a Git operation is in progress");
        first.finish(Outcome::Failed {
            error: Action::Convert.failure().into(),
            detail: first.said(),
        });
        let failed = first.json();
        assert_eq!(failed["state"], "failed");
        // The detail is Kin's closing error, without the progress before it,
        // and a finished operation's phase says how it ended.
        assert_eq!(
            failed["detail"],
            "Error: admit the checked-out Git commit\nCaused by:\na Git operation is in progress"
        );
        assert_eq!(failed["error"], "Kin could not convert this project.");
        assert_eq!(failed["phase"], failed["error"]);
        assert!(failed["finished_at"].is_string());

        // A finished operation no longer holds its location.
        let (again, fresh) = home.begin(Action::Convert, Some(Path::new("/work/a")), "Starting");
        assert!(fresh);
        assert_ne!(again.id, first.id);
        assert_eq!(home.find(first.id).unwrap().id, first.id);
    }

    #[test]
    fn finished_operations_are_forgotten_before_running_ones() {
        let home = Home::default();
        let (running, _) = home.begin(
            Action::Convert,
            Some(Path::new("/work/running")),
            "Starting",
        );
        for index in 0..OPERATIONS_KEPT + 5 {
            let target = PathBuf::from(format!("/work/{index}"));
            let (operation, _) = home.begin(Action::Create, Some(&target), "Starting");
            operation.finish(Outcome::Failed {
                error: String::new(),
                detail: String::new(),
            });
        }
        assert_eq!(locked(&home.operations).len(), OPERATIONS_KEPT);
        assert!(home.find(running.id).is_some());
    }

    #[test]
    fn a_request_names_one_action_and_nothing_else() {
        let parsed = |body: Value| serde_json::from_value::<AddBody>(body);
        assert!(matches!(
            parsed(json!({"action":"convert","path":"/work/a"})),
            Ok(AddBody::Convert { .. })
        ));
        assert!(matches!(
            parsed(json!({"action":"clone","source":"https://example.com/a.git","path":"/work/a"})),
            Ok(AddBody::Clone { .. })
        ));
        assert!(parsed(json!({"action":"convert","path":"/work/a","actor":"caller"})).is_err());
        assert!(parsed(json!({"action":"remove","path":"/work/a"})).is_err());
        assert!(parsed(json!({"action":"clone","path":"/work/a"})).is_err());
        assert!(clone_source("--upload-pack=touch /work/owned").is_err());
        assert!(clone_source(" ").is_err());
        assert_eq!(
            clone_source(" https://example.com/a.git ").unwrap(),
            "https://example.com/a.git"
        );
    }

    #[test]
    fn work_only_in_unparsed_entities_still_counts_as_work() {
        let summary =
            |entities: usize, unparsed: usize, relations: usize| super::super::diff::DiffSummary {
                artifacts_added: 0,
                artifacts_updated: unparsed,
                artifacts_removed: 0,
                entities_added: 0,
                entities_modified: entities,
                entities_removed: 0,
                entities_unchanged: 0,
                relations_added: relations,
                relations_modified: 0,
                relations_removed: 0,
            };
        assert_eq!(changed_in(&summary(3, 1, 0)), 3);
        assert_eq!(changed_in(&summary(0, 2, 0)), 2);
        // Relations Kin derived for itself are not work a reader left behind.
        assert_eq!(changed_in(&summary(0, 0, 40)), 0);
    }

    /// The row never says a working copy is clean unless that was read for
    /// the repository as it is now.
    /// The daemon calls a working copy dirty when Kin only linked relations
    /// for itself; once the count says no entity of the person's changed, the
    /// row says the work does not differ.
    #[test]
    fn kins_own_linking_alone_is_not_uncommitted_work() {
        assert!(!differs_after_count(true, 0));
        assert!(differs_after_count(true, 3));
        assert!(!differs_after_count(false, 0));
    }

    #[test]
    fn a_working_copy_is_only_called_clean_when_that_was_read_for_this_generation() {
        let since = tokio::time::Instant::now();
        let before = since - Duration::from_secs(60);
        let read = |generation: Generation, at, differs, changed| Reading {
            generation: Some(generation),
            read_at: Some(at),
            differs: Some(differs),
            changed,
            ..Reading::default()
        };
        // Nothing read yet.
        assert_eq!(
            working_from(&Reading::default(), Some((4, 2)), since),
            Working::Unknown
        );
        assert_eq!(Working::Unknown.json(), json!({"unknown": true}));
        // Read for this generation: whether it differs is known, and the
        // count joins it once it has arrived.
        let differs = working_from(&read((4, 2), before, true, None), Some((4, 2)), since);
        assert_eq!(differs.json(), json!({"differs": true}));
        let counted = working_from(&read((4, 2), before, true, Some(2)), Some((4, 2)), since);
        assert_eq!(counted.json(), json!({"differs": true, "changed": 2}));
        let clean = working_from(&read((4, 2), before, false, Some(0)), Some((4, 2)), since);
        assert_eq!(clean.json(), json!({"differs": false, "changed": 0}));
        // The repository has moved since: a clean reading from before says
        // nothing about it now.
        assert_eq!(
            working_from(&read((4, 2), before, false, Some(0)), Some((5, 3)), since),
            Working::Unknown
        );
        // A reading taken at a later generation than the row asked for is of
        // the repository as it is now.
        assert_ne!(
            working_from(&read((6, 3), before, true, None), Some((5, 3)), since),
            Working::Unknown
        );
        // With the generation unknown, only a reading taken during this
        // request counts.
        assert_eq!(
            working_from(&read((4, 2), before, false, Some(0)), None, since),
            Working::Unknown
        );
        assert_eq!(
            working_from(&read((4, 2), since, false, None), None, since).json(),
            json!({"differs": false})
        );
    }

    #[test]
    fn the_newest_change_wins_unless_the_registry_was_written_after_it_was_read() {
        let at = |text: &str| recorded_time(text).unwrap();
        let registered = at("2026-05-01T10:00:00Z");
        let made = at("2026-03-01T09:00:00Z");
        let learned = at("2026-05-01T10:05:00Z");
        // Nothing read from a daemon: the registry's time is all there is.
        assert_eq!(last_change(Some(registered), None, false), Some(registered));
        assert_eq!(last_change(None, None, false), None);
        // A converted project's newest change predates its conversion, and
        // it is the newest change that the row shows.
        assert_eq!(
            last_change(Some(registered), Some((made, learned)), true),
            Some(made)
        );
        assert_eq!(
            last_change(Some(registered), Some((made, learned)), false),
            Some(made)
        );
        // A commit refreshes the registry. Until the daemon is read again,
        // that later time is the newest change this server knows of.
        let committed = at("2026-05-01T10:09:00Z");
        assert_eq!(
            last_change(Some(committed), Some((made, learned)), false),
            Some(committed)
        );
        assert!(recorded_time("yesterday").is_none());
    }

    #[test]
    fn a_stage_is_read_from_what_a_conversion_reports_and_never_from_its_result() {
        // Every step Kin's conversion announces has a stage here, so a new
        // step cannot arrive unstaged.
        for name in kin_core::COMPLETE_ADMISSION_PHASE_LABELS {
            assert!(
                CONVERSION_STEPS.iter().any(|(known, _)| *known == name),
                "{name} has no stage"
            );
        }
        let after = |current, printed, line: &str| stage_after(current, printed, line);
        let step = |name: &str| kin_core::admission_phase_shown(name).to_string();
        assert_eq!(
            after(
                Stage::Starting,
                false,
                &format!("[ 1/15] {}...", step("check admission blockers"))
            ),
            (Stage::Reading, Some((1, 15)))
        );
        // One step's words begin another's; the longer name is the step.
        assert_eq!(
            after(
                Stage::Reading,
                false,
                &format!("[ 7/15] {} 0.0s", step("prove Git source"))
            ),
            (Stage::Reading, Some((7, 15)))
        );
        assert_eq!(
            after(
                Stage::Reading,
                false,
                &format!("[12/15] {}...", step("re-prove Git source"))
            ),
            (Stage::Graph, Some((12, 15)))
        );
        assert_eq!(
            after(
                Stage::Reading,
                false,
                &format!("[ 5/16] {}...", step("derive semantic history"))
            ),
            (Stage::History, Some((5, 16)))
        );
        // A step this build does not know keeps the stage and its counts.
        assert_eq!(
            after(Stage::Graph, false, "[ 3/9] weigh the anchor..."),
            (Stage::Graph, Some((3, 9)))
        );
        assert_eq!(
            after(Stage::Starting, false, "[ 3/9] weigh the anchor..."),
            (Stage::Reading, Some((3, 9)))
        );
        assert_eq!(
            after(
                Stage::Graph,
                false,
                "admitted the checked-out commit in 1.1s"
            ),
            (Stage::Graph, None)
        );
        assert_eq!(
            after(Stage::Graph, false, "checked 392 of 393 entities"),
            (Stage::References, Some((392, 393)))
        );
        // A project with no Git history is read in one pass, announced in
        // the words `kin init` owns.
        let reading = format!(
            "{}, as uncommitted work...",
            super::super::init::TAKING_IN_WAITING_WORK
        );
        assert_eq!(
            after(Stage::Starting, false, &reading),
            (Stage::Reading, None)
        );
        assert_eq!(
            after(
                Stage::Graph,
                false,
                "Linking references across the project..."
            ),
            (Stage::References, None)
        );
        assert_eq!(
            after(Stage::References, false, "checked of them"),
            (Stage::References, None)
        );
        // The result is printed last, on the other stream, and nothing after
        // it moves the stage back.
        assert_eq!(
            after(Stage::References, true, "Repository: 6736a60c"),
            (Stage::Finishing, None)
        );
        assert_eq!(
            after(Stage::Finishing, false, "checked 3 of 9 entities"),
            (Stage::Finishing, None)
        );
        for stage in [
            Stage::Starting,
            Stage::Reading,
            Stage::Graph,
            Stage::References,
            Stage::History,
            Stage::Finishing,
        ] {
            assert!(stage.word().bytes().all(|byte| byte.is_ascii_lowercase()));
        }
    }

    #[test]
    fn a_request_to_connect_names_only_coding_agents_kin_knows() {
        let report = json!({"schema":"kin.setup-clients.v1","clients":[
            {"id":"harbor-editor","name":"Harbor Editor"},
            {"id":"lantern","name":"Lantern"},
        ]});
        let ids = |asked: &[&str]| {
            let asked: Vec<String> = asked.iter().map(|id| id.to_string()).collect();
            named_clients(&report, &asked)
        };
        assert_eq!(
            ids(&[" Lantern ", "harbor-editor", "lantern"]).unwrap(),
            ["lantern", "harbor-editor"]
        );
        assert!(ids(&[]).is_err());
        assert!(ids(&["lantern", "--shell"]).is_err());
        assert!(ids(&["unknown"]).is_err());
        let parsed = |body: Value| serde_json::from_value::<ConnectBody>(body);
        assert!(parsed(json!({"connect":["lantern"]})).is_ok());
        assert!(parsed(json!({"connect":["lantern"],"shell":"zsh"})).is_err());
        assert!(parsed(json!({"connect":"lantern"})).is_err());
    }

    #[test]
    fn a_connect_operation_says_what_became_of_each_coding_agent() {
        use super::super::setup::{ClientConnected, NamedClientConnection};
        let connection = |id, name, outcome| NamedClientConnection { id, name, outcome };
        let (rows, lines, failures) = connections_seen(&[
            connection("harbor-editor", "Harbor Editor", ClientConnected::Connected),
            connection("lantern", "Lantern", ClientConnected::Waiting),
            connection("quill", "Quill", ClientConnected::NotInstalled),
            connection(
                "anvil",
                "Anvil",
                ClientConnected::Failed("its configuration is read-only".into()),
            ),
        ]);
        let outcomes: Vec<&str> = rows
            .iter()
            .map(|row| row["outcome"].as_str().unwrap())
            .collect();
        assert_eq!(
            outcomes,
            ["connected", "waiting", "not_installed", "failed"]
        );
        assert_eq!(lines.len(), 4);
        assert_eq!(failures, ["Anvil: its configuration is read-only"]);

        let home = Home::default();
        let (operation, fresh) = home.begin(Action::Connect, None, "Connecting");
        assert!(fresh);
        // One connect at a time, and it holds no location.
        assert!(!home.begin(Action::Connect, None, "Connecting").1);
        let running = operation.json();
        assert_eq!(running["action"], "connect");
        assert_eq!(running["location"], Value::Null);
        assert_eq!(running["stage"], "starting");
        locked(&operation.progress).clients = Some(json!(rows));
        operation.finish(Outcome::Done {
            repository: None,
            note: None,
        });
        let done = operation.json();
        assert_eq!(done["state"], "done");
        assert_eq!(done["stage"], "finishing");
        assert_eq!(done["phase"], "The coding agents are connected.");
        assert_eq!(done["clients"].as_array().unwrap().len(), 4);
        assert!(done.get("repository").is_none());
        assert!(home.begin(Action::Connect, None, "Connecting").1);
    }

    #[test]
    fn connected_agents_are_named_once_and_the_working_ones_counted() {
        let seen = agents_seen(&json!({"agents":[
            {"vendor":"harbor-editor","client":"harbor-editor","state":"active"},
            {"vendor":"harbor-editor","client":"harbor-editor","state":"idle"},
            {"vendor":"lantern","client":"lantern","state":"idle"},
        ]}))
        .unwrap();
        assert_eq!(
            seen,
            json!({"active":1,"connected":3,"names":["Harbor Editor","Lantern"]})
        );
        assert!(agents_seen(&json!({"error":"starting"})).is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_child_is_relayed_line_by_line_and_answers_its_exit_status() {
        let heard = Mutex::new(Vec::new());
        let keep = |errors: bool, line: &str| locked(&heard).push((errors, line.to_string()));
        let script = "printf 'one\\rtwo\\n'; printf 'refused\\n' >&2; exit 7";
        let arguments = ["-c", script].map(OsString::from);
        let ended = run_child(OsStr::new("sh"), &arguments, None, false, &keep)
            .await
            .unwrap();
        assert_eq!(ended, Some(7));
        let heard = locked(&heard).clone();
        let printed: Vec<&str> = heard
            .iter()
            .filter(|(errors, _)| !errors)
            .map(|(_, line)| line.as_str())
            .collect();
        assert_eq!(printed, ["one", "two"]);
        assert!(heard.contains(&(true, "refused".to_string())));
        assert!(exit_note(7).is_some() && exit_note(8).is_some() && exit_note(1).is_none());
    }
}
