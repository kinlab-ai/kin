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
use std::time::{Duration, SystemTime};

use axum::extract::{DefaultBodyLimit, Path as UrlPath, Query, State};
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

/// How long one look for projects around a home directory may take.
const LOOK_BUDGET: Duration = Duration::from_millis(900);
/// How long what a look found is kept before a request looks again.
const LOOK_KEPT: Duration = Duration::from_secs(30);
/// The directories one look lists, over every place it looks through.
const LOOK_LISTINGS: usize = 400;
/// The directories examined in any one place, most recently changed first.
const LOOK_PER_PLACE: usize = 200;
/// The projects one look keeps, most recently changed first.
const LOOK_FOUND: usize = 200;
const CANDIDATES_DEFAULT: usize = 30;
const CANDIDATES_MAX: usize = 100;
/// How long a request waits for commit counts Git has not given yet. A count
/// that is later than this is in the next answer.
const COUNT_WAIT: Duration = Duration::from_millis(600);
/// How long Git is given to count one project's commits once no request is
/// waiting on it.
const GIT_PATIENCE: Duration = Duration::from_secs(20);
/// The projects Git counts commits for at once.
const GIT_AT_ONCE: usize = 4;
/// The projects whose commit counts are remembered.
const COUNTS_KEPT: usize = 512;
/// The coding agent sessions remembered. The oldest ended one goes first.
const SESSIONS_KEPT: usize = 256;
/// The newest changes read from a running repository for the coding agents
/// that made them.
const SESSION_CHANGES_READ: usize = 50;

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
    /// The projects the last look around the home directory found, and when
    /// it looked.
    look: Mutex<Option<(std::time::Instant, Arc<Look>)>>,
    /// The directory looked around in place of the home directory. Only a
    /// test names one.
    looks_in: Mutex<Option<PathBuf>>,
    counts: Counts,
    /// The coding agent sessions the repositories on this machine have
    /// shown this server, by session.
    sessions: Mutex<HashMap<String, SessionSeen>>,
}

pub(super) fn routes() -> Router<Arc<Hub>> {
    Router::new()
        .route("/api/home/session", get(session))
        .route("/api/home", get(home))
        .route("/api/home/inspect", post(inspect))
        .route("/api/home/candidates", get(candidates))
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

/// What the add flow says of one location: what is there, what adding it
/// would do, and what this page may be asked to do with it. Everything but
/// what Git has to be asked for.
fn described(hub: &Hub, place: &Place, registered: bool) -> Value {
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
        "note": note(place, reachable.is_some()),
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
            .filter(|action| precondition(*action, place).is_none())
            .map(Action::word)
            .collect::<Vec<_>>()
    );
    if let Some(outer) = &place.inside {
        let entry = super::workspace_ui::entry(outer);
        answer["inside"] = json!({
            "key": entry.key,
            "name": entry.name,
            "url": entry.url,
            "location": location(outer.working_dir()),
        });
    }
    answer
}

/// What is known of a Git project before it is added: the branch it has
/// checked out and how many commits it holds, each when known.
fn git_block(branch: Option<&str>, commits: Option<u64>) -> Option<Value> {
    let mut git = serde_json::Map::new();
    if let Some(commits) = commits {
        git.insert("commits".into(), json!(commits));
    }
    if let Some(branch) = branch {
        git.insert("branch".into(), json!(branch));
    }
    (!git.is_empty()).then_some(Value::Object(git))
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
    let mut answer = described(&hub, &place, registered);
    if place.kind == Kind::Git {
        let commits = git_commits(&place.path, GIT_BUDGET).await;
        if let Some(git) = git_block(place.branch.as_deref(), commits) {
            answer["git"] = git;
        }
    }
    Json(answer).into_response()
}

// ---------------------------------------------------------------------------
// Projects a person is likely to add
// ---------------------------------------------------------------------------
//
// So that adding a repository starts from a list and not from a typed
// location, this page looks for projects where people keep them. It is the
// same boundary `inspect` sits on, reached without a location being typed,
// and it is kept as narrow: directory names and change times are read,
// nothing a project holds is, and what is found is only ever offered to the
// reader to add.

/// Directories directly inside a home directory that people keep projects
/// in, in lowercase. Each is looked through two levels deep, so a project
/// kept under its owner's name is found as well.
const CODE_ROOTS: [&str; 15] = [
    "code",
    "dev",
    "developer",
    "development",
    "git",
    "github",
    "gitlab",
    "projects",
    "repos",
    "repositories",
    "source",
    "src",
    "work",
    "workspace",
    "workspaces",
];

/// Directories never looked into, in lowercase: what tools install or build
/// into, and the folders an operating system keeps a person's documents,
/// media and synced storage in. Those are not where projects are looked for,
/// and macOS asks a person before any program may read some of them.
const NEVER_LOOKED_IN: [&str; 38] = [
    "__pycache__",
    "_build",
    "applications",
    "bin",
    "bower_components",
    "build",
    "coverage",
    "creative cloud files",
    "deps",
    "deriveddata",
    "desktop",
    "dist",
    "documents",
    "downloads",
    "dropbox",
    "env",
    "google drive",
    "icloud drive",
    "library",
    "movies",
    "music",
    "node_modules",
    "obj",
    "onedrive",
    "out",
    "parallels",
    "pictures",
    "pods",
    "public",
    "site-packages",
    "snap",
    "target",
    "temp",
    "templates",
    "tmp",
    "vendor",
    "venv",
    "videos",
];

/// Names at the top of a directory that say it is a project. Only the name
/// is looked at.
const PROJECT_MARKERS: [&str; 26] = [
    "build.bazel",
    "build.gradle",
    "build.gradle.kts",
    "build.sbt",
    "cargo.toml",
    "cmakelists.txt",
    "composer.json",
    "deno.json",
    "dune-project",
    "flake.nix",
    "gemfile",
    "go.mod",
    "makefile",
    "meson.build",
    "mix.exs",
    "package.json",
    "package.swift",
    "pipfile",
    "pom.xml",
    "pubspec.yaml",
    "pyproject.toml",
    "requirements.txt",
    "settings.gradle",
    "setup.py",
    "stack.yaml",
    "tsconfig.json",
];
const PROJECT_MARKER_ENDINGS: [&str; 5] = [".sln", ".xcodeproj", ".csproj", ".cabal", ".gemspec"];

/// A project a look found: where it is and when it last changed.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Spotted {
    path: PathBuf,
    /// The later of when its own directory and its Git directory changed.
    changed: Option<SystemTime>,
}

/// What one look around a home directory found.
#[derive(Debug, Default)]
struct Look {
    /// Most recently changed first.
    spotted: Vec<Spotted>,
    /// Whether every place was looked through inside the look's bounds.
    complete: bool,
}

fn lowercase_name(name: &OsStr) -> String {
    name.to_string_lossy().to_lowercase()
}

/// Whether a directory of this name is looked at at all.
fn looked_into(name: &OsStr) -> bool {
    let name = lowercase_name(name);
    !name.starts_with('.') && !NEVER_LOOKED_IN.contains(&name.as_str())
}

fn marks_a_project(name: &OsStr) -> bool {
    let name = lowercase_name(name);
    PROJECT_MARKERS.contains(&name.as_str())
        || PROJECT_MARKER_ENDINGS
            .iter()
            .any(|ending| name.ends_with(ending))
}

/// Look for projects where people keep them: the directories directly
/// inside `home`, and one and two levels inside those named like a code
/// root. Blocking, and read-only.
///
/// It reads directory names and change times and nothing a project holds. A
/// project is known by the `.git` or `.kin` directory at its top or by the
/// name of a build manifest there, and a directory that is one is never
/// looked inside. A second working copy of a project is not a second project.
/// It follows no link and enters no directory a tool installs or builds
/// into. It stops at `until`, or once it has listed [`LOOK_LISTINGS`]
/// directories, and in each place it examines the most recently changed
/// directories first, so that what a bound cuts off is the oldest.
fn look_around(home: &Path, until: std::time::Instant) -> Look {
    let mut look = Look {
        spotted: Vec::new(),
        complete: true,
    };
    let Ok(tree) = kin_io::WorktreeSource::open(home, &kin_io::Grant::ambient()) else {
        return look;
    };
    let mut listings = 0;
    // Each place still to look through: where it is under `home`, the names
    // it holds when they were already read, whether a directory in it that
    // is no project is itself looked through, and how many levels further.
    let mut places: VecDeque<(PathBuf, Option<Vec<OsString>>, usize)> =
        VecDeque::from([(PathBuf::new(), None, 0)]);
    'places: while let Some((place, names, deeper)) = places.pop_front() {
        let at_home = place.as_os_str().is_empty();
        let names = match names {
            Some(names) => names,
            None => {
                if listings >= LOOK_LISTINGS || std::time::Instant::now() >= until {
                    look.complete = false;
                    break;
                }
                listings += 1;
                tree.names(&place).unwrap_or_default()
            }
        };
        // The directories here, most recently changed first.
        let mut directories: Vec<(Option<SystemTime>, PathBuf, OsString)> = Vec::new();
        for name in names {
            if !looked_into(&name) {
                continue;
            }
            let inner = place.join(&name);
            if tree.kind(&inner).ok().flatten() != Some(kin_io::EntryKind::Directory) {
                continue;
            }
            directories.push((tree.modified(&inner).ok().flatten(), inner, name));
        }
        directories.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
        if directories.len() > LOOK_PER_PLACE {
            directories.truncate(LOOK_PER_PLACE);
            look.complete = false;
        }
        for (changed, inner, name) in directories {
            if std::time::Instant::now() >= until {
                look.complete = false;
                break 'places;
            }
            let is_directory = |name: &str| {
                tree.kind(inner.join(name)).ok().flatten() == Some(kin_io::EntryKind::Directory)
            };
            if is_directory(".git") || is_directory(".kin") {
                let git = tree.modified(inner.join(".git")).ok().flatten();
                look.spotted.push(Spotted {
                    path: home.join(&inner),
                    changed: changed.max(git),
                });
                continue;
            }
            // A `.git` that is not a directory marks one more working copy
            // of a project kept somewhere else, or a project held inside
            // another. That project is found where it is kept, so this is
            // neither offered beside it nor looked inside.
            if tree.kind(inner.join(".git")).ok().flatten().is_some() {
                continue;
            }
            // Neither Git nor Kin holds it, so its top names say whether it
            // is a project, and they are what is looked through if it is not.
            if listings >= LOOK_LISTINGS {
                look.complete = false;
                break 'places;
            }
            listings += 1;
            let inside = tree.names(&inner).unwrap_or_default();
            if inside.iter().any(|name| marks_a_project(name)) {
                look.spotted.push(Spotted {
                    path: home.join(&inner),
                    changed,
                });
            } else if at_home && CODE_ROOTS.contains(&lowercase_name(&name).as_str()) {
                places.push_back((inner, Some(inside), 1));
            } else if deeper > 0 {
                places.push_back((inner, Some(inside), deeper - 1));
            }
        }
    }
    look.spotted.sort_by(|left, right| {
        right
            .changed
            .cmp(&left.changed)
            .then_with(|| left.path.cmp(&right.path))
    });
    if look.spotted.len() > LOOK_FOUND {
        look.spotted.truncate(LOOK_FOUND);
        look.complete = false;
    }
    look
}

/// What Git said of one project's commits.
#[derive(Default)]
struct Counted {
    /// The project's change time the count was taken at. A project that has
    /// changed since is counted again.
    taken_at: Option<Option<SystemTime>>,
    commits: Option<u64>,
    in_flight: bool,
}

/// Commit counts for the projects this page offers, kept between requests so
/// a list that is read again starts no process for a project that has not
/// changed.
struct Counts {
    known: Mutex<HashMap<PathBuf, Counted>>,
    gate: tokio::sync::Semaphore,
    /// Bumped whenever a count arrives, for a request waiting on one.
    moved: tokio::sync::watch::Sender<u64>,
}

impl Default for Counts {
    fn default() -> Self {
        Self {
            known: Mutex::default(),
            gate: tokio::sync::Semaphore::new(GIT_AT_ONCE),
            moved: tokio::sync::watch::channel(0).0,
        }
    }
}

impl Counts {
    /// The count held for a project, and whether a newer one is on its way.
    fn held(&self, path: &Path) -> (Option<u64>, bool) {
        locked(&self.known)
            .get(path)
            .map_or((None, false), |counted| {
                (counted.commits, counted.in_flight)
            })
    }
}

/// Have Git count the commits of the project at `path`, unless a count is
/// held for it as it was at `changed` or one is already being taken. The
/// count is taken by a task of its own, so it outlasts the request that
/// asked and is there for the next one.
fn count_commits(hub: &Arc<Hub>, path: &Path, changed: Option<SystemTime>) {
    {
        let mut known = locked(&hub.home.counts.known);
        if known.len() >= COUNTS_KEPT && !known.contains_key(path) {
            known.retain(|_, counted| counted.in_flight);
        }
        let counted = known.entry(path.to_path_buf()).or_default();
        if counted.in_flight || counted.taken_at == Some(changed) {
            return;
        }
        counted.in_flight = true;
    }
    let (hub, path) = (hub.clone(), path.to_path_buf());
    tokio::spawn(async move {
        let counts = &hub.home.counts;
        let commits = match counts.gate.acquire().await {
            Ok(_one_at_a_time) => git_commits(&path, GIT_PATIENCE).await,
            Err(_) => None,
        };
        if let Some(counted) = locked(&counts.known).get_mut(&path) {
            *counted = Counted {
                taken_at: Some(changed),
                commits,
                in_flight: false,
            };
        }
        counts.moved.send_modify(|version| *version += 1);
    });
}

impl Home {
    /// What a look around the home directory finds, from the last look when
    /// that was within [`LOOK_KEPT`]. Blocking.
    fn looked_around(&self) -> Arc<Look> {
        if let Some((looked, look)) = locked(&self.look).as_ref() {
            if looked.elapsed() < LOOK_KEPT {
                return look.clone();
            }
        }
        // The resolved form of the home directory, so that what is found is
        // named the way the registry and `inspect` name it.
        let home = locked(&self.looks_in)
            .clone()
            .or_else(|| homes().last().cloned());
        let look = Arc::new(match home {
            Some(home) => look_around(&home, std::time::Instant::now() + LOOK_BUDGET),
            None => Look::default(),
        });
        *locked(&self.look) = Some((std::time::Instant::now(), look.clone()));
        look
    }

    /// Look around `home` in place of the home directory.
    #[cfg(test)]
    fn look_in(&self, home: &Path) {
        *locked(&self.looks_in) = Some(home.to_path_buf());
        *locked(&self.look) = None;
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CandidatesQuery {
    #[serde(default)]
    limit: Option<usize>,
}

/// The projects on this machine a person is likely to want in Kin, most
/// recently changed first, each described as `inspect` describes it.
///
/// Looking is bounded and its result is kept for a while, so a large disk
/// costs a request no more than a small one. What each project is, and what
/// may be done with it, is read again for every answer, so a project added
/// a moment ago says so. Commit counts come from Git at its own pace: the
/// ones not known yet are left out, and `counting` says how many are coming.
async fn candidates(State(hub): State<Arc<Hub>>, Query(query): Query<CandidatesQuery>) -> Response {
    let limit = query
        .limit
        .unwrap_or(CANDIDATES_DEFAULT)
        .clamp(1, CANDIDATES_MAX);
    let reading = {
        let hub = hub.clone();
        tokio::task::spawn_blocking(move || {
            let look = hub.home.looked_around();
            let registered = hub.catalog.registered_keys();
            let shown: Vec<(Spotted, Place, bool)> = look
                .spotted
                .iter()
                .take(limit)
                .filter_map(|spotted| {
                    // Read as `inspect` reads a location a person typed. A
                    // project that has gone since the look is left out.
                    let place = look_at(&spotted.path.to_string_lossy()).ok()?;
                    let known = registered.contains(&repository_key(&place.path));
                    matches!(place.kind, Kind::Kin | Kind::Git | Kind::Plain)
                        .then(|| (spotted.clone(), place, known))
                })
                .collect();
            (look.spotted.len(), look.complete, shown)
        })
        .await
    };
    let (found, complete, shown) = match reading {
        Ok(reading) => reading,
        Err(error) => return refused(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    };
    let counted: Vec<&Path> = shown
        .iter()
        .filter(|(_, place, _)| place.kind == Kind::Git)
        .map(|(spotted, place, _)| {
            count_commits(&hub, &place.path, spotted.changed);
            place.path.as_path()
        })
        .collect();
    let counts = &hub.home.counts;
    let mut moved = counts.moved.subscribe();
    let deadline = tokio::time::Instant::now() + COUNT_WAIT;
    let counting = loop {
        let counting = counted.iter().filter(|path| counts.held(path).1).count();
        if counting == 0
            || tokio::time::timeout_at(deadline, moved.changed())
                .await
                .is_err()
        {
            break counting;
        }
    };
    let rows: Vec<Value> = shown
        .iter()
        .map(|(spotted, place, registered)| {
            let mut row = described(&hub, place, *registered);
            if let Some(changed) = spotted.changed {
                row["modified_at"] = json!(chrono::DateTime::<chrono::Utc>::from(changed)
                    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
            }
            if place.kind == Kind::Git {
                let commits = counts.held(&place.path).0;
                if let Some(git) = git_block(place.branch.as_deref(), commits) {
                    row["git"] = git;
                }
            }
            row
        })
        .collect();
    Json(json!({
        "schema": "kin.workspace.home-candidates.v1",
        "read_at": now(),
        "candidates": rows,
        "found": found,
        "complete": complete,
        "counting": counting,
    }))
    .into_response()
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
/// This is the one place the local app starts a process, and it starts two:
/// this same Kin binary, to add a repository or write a Git projection
/// exactly as a terminal would, and Git, to count a project's commits before
/// it is added and to carry a published branch to a Git remote. Neither
/// answers a question about a repository's code. `environment` is set on the
/// child over what it inherits.
pub(super) async fn run_child(
    program: &OsStr,
    arguments: &[OsString],
    directory: Option<&Path>,
    outlives_request: bool,
    environment: &[(&str, &str)],
    said: &(dyn Fn(bool, &str) + Send + Sync),
) -> std::io::Result<Option<i32>> {
    let spawner = kin_io::Spawner::new(&kin_io::Grant::ambient());
    let mut command = spawner.command(program);
    command
        .args(arguments)
        .env("NO_COLOR", "1")
        .envs(environment.iter().copied())
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
/// `directory`, when it can say within `budget`. The same count `kin init`
/// opens with, asked only to word the add flow.
async fn git_commits(directory: &Path, budget: Duration) -> Option<u64> {
    let printed = Mutex::new(String::new());
    let keep = |errors: bool, line: &str| {
        if !errors {
            *locked(&printed) = line.trim().to_string();
        }
    };
    let arguments = ["rev-list", "--count", "--all"].map(OsString::from);
    let ended = tokio::time::timeout(
        budget,
        run_child(
            OsStr::new("git"),
            &arguments,
            Some(directory),
            false,
            &[],
            &keep,
        ),
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
        Ok(kin) => run_child(kin.as_os_str(), &arguments, None, true, &[], &said).await,
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

/// One coding agent session a repository showed this server: its daemon
/// listed it as connected, or its history records a change the session made.
#[derive(Clone, Debug, PartialEq, Eq)]
struct SessionSeen {
    /// The coding agent, by the id `kin setup clients` lists it by.
    client: &'static str,
    /// The repository it worked in, by key and by name.
    key: String,
    name: String,
    /// When it was last known to do anything.
    at: chrono::DateTime<chrono::Utc>,
    /// `active` or `idle` while the repository's daemon lists it, and
    /// nothing once it does not.
    listed: Option<&'static str>,
}

impl SessionSeen {
    fn state(&self) -> &'static str {
        self.listed.unwrap_or("ended")
    }
}

/// Keep what is remembered inside its bound, dropping the session that
/// ended longest ago.
fn bound_sessions(sessions: &mut HashMap<String, SessionSeen>) {
    while sessions.len() > SESSIONS_KEPT {
        let Some(oldest) = sessions
            .iter()
            .filter(|(_, seen)| seen.listed.is_none())
            .min_by_key(|(_, seen)| seen.at)
            .map(|(session, _)| session.clone())
        else {
            break;
        };
        sessions.remove(&oldest);
    }
}

/// Take in what one repository's daemon lists as connected. `None` says no
/// daemon is running for the repository, so no session of it is still
/// connected. An answer that is not a list says nothing either way.
fn sessions_listed(
    sessions: &mut HashMap<String, SessionSeen>,
    key: &str,
    name: &str,
    answer: Option<&Value>,
) {
    let agents: &[Value] = match answer {
        Some(answer) => match answer["agents"].as_array() {
            Some(agents) => agents,
            None => return,
        },
        None => &[],
    };
    let mut listed = Vec::new();
    for agent in agents {
        let Some(session) = agent["session_id"].as_str().map(str::to_ascii_lowercase) else {
            continue;
        };
        listed.push(session.clone());
        let Some(client) = super::workspace_agents::agent_client_id(
            agent["vendor"].as_str().unwrap_or_default(),
            agent["client"].as_str().unwrap_or_default(),
        ) else {
            continue;
        };
        let at = ["last_activity_at", "started_at"]
            .iter()
            .find_map(|field| recorded_time(agent[*field].as_str()?))
            .unwrap_or_else(chrono::Utc::now);
        let state = if agent["state"] == "active" {
            "active"
        } else {
            "idle"
        };
        let seen = sessions.entry(session).or_insert_with(|| SessionSeen {
            client,
            key: key.to_string(),
            name: name.to_string(),
            at,
            listed: None,
        });
        seen.client = client;
        seen.key = key.to_string();
        seen.name = name.to_string();
        seen.at = seen.at.max(at);
        seen.listed = Some(state);
    }
    for (session, seen) in sessions.iter_mut() {
        if seen.key == key && !listed.contains(session) {
            seen.listed = None;
        }
    }
    bound_sessions(sessions);
}

/// Take in one recorded change of a repository: when a coding agent made
/// it, its session was at work in that repository then.
fn session_committed(
    sessions: &mut HashMap<String, SessionSeen>,
    key: &str,
    name: &str,
    author: &str,
    at: chrono::DateTime<chrono::Utc>,
) {
    let Some(author) = super::workspace_agents::agent_author(author) else {
        return;
    };
    let Some(client) = author.client else {
        return;
    };
    let seen = sessions
        .entry(author.session)
        .or_insert_with(|| SessionSeen {
            client,
            key: key.to_string(),
            name: name.to_string(),
            at,
            listed: None,
        });
    seen.at = seen.at.max(at);
    bound_sessions(sessions);
}

/// The newest session of each coding agent, as the page is told of it. A
/// session still connected leads one that ended at the same moment.
fn last_sessions(sessions: &HashMap<String, SessionSeen>) -> HashMap<&'static str, Value> {
    let mut newest: HashMap<&'static str, &SessionSeen> = HashMap::new();
    for seen in sessions.values() {
        let rank = |seen: &SessionSeen| (seen.at, seen.listed.is_some());
        let kept = newest.entry(seen.client).or_insert(seen);
        if rank(seen) > rank(kept) {
            *kept = seen;
        }
    }
    newest
        .into_iter()
        .map(|(client, seen)| {
            let last = json!({
                "repository": {
                    "key": seen.key,
                    "name": seen.name,
                    "url": format!("/repositories/{}/workspace/", seen.key),
                },
                "at": seen.at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                "state": seen.state(),
            });
            (client, last)
        })
        .collect()
}

/// Read which coding agents one repository has seen, when its daemon is
/// already running: the sessions it lists now, and the agents behind its
/// newest changes. A repository that is not running is not started. It is
/// only noted that nothing is connected to it.
async fn read_sessions(hub: &Hub, listed: &Listed) {
    let deadline = tokio::time::Instant::now() + OBSERVE_BUDGET;
    let (key, name) = (&listed.entry.key, &listed.entry.name);
    let client = match tokio::time::timeout_at(deadline, running_daemon(&listed.layout)).await {
        Ok(Some(client)) => client,
        Ok(None) => {
            sessions_listed(&mut locked(&hub.home.sessions), key, name, None);
            return;
        }
        // Not known in time, which says nothing about what is connected.
        Err(_) => return,
    };
    let newest = super::log::LogRequest {
        count: SESSION_CHANGES_READ,
        skip: 0,
    };
    let (agents, changes) = tokio::join!(
        tokio::time::timeout_at(deadline, client.agents()),
        tokio::time::timeout_at(deadline, client.log(&newest)),
    );
    let mut sessions = locked(&hub.home.sessions);
    if let Ok(Ok(agents)) = &agents {
        sessions_listed(&mut sessions, key, name, Some(agents));
    }
    for entry in changes
        .ok()
        .and_then(Result::ok)
        .and_then(|answer| answer.report)
        .into_iter()
        .flat_map(|report| report.entries)
    {
        session_committed(
            &mut sessions,
            key,
            name,
            &entry.author.to_string(),
            entry.timestamp.0,
        );
    }
}

/// Every coding agent Kin can connect, as `kin setup clients --json` prints
/// it, from the same function, and for each the last session Kin has a
/// record of on this machine: which repository it worked in, when, and
/// whether it is still connected. `null` when Kin has no record of one.
///
/// The record is what the repositories that are running say now, the agents
/// behind their newest changes, and what this server has seen since it
/// started. Reads only, and no repository is started to answer.
async fn clients(State(hub): State<Arc<Hub>>) -> Response {
    let catalog = hub.catalog.clone();
    let reading =
        tokio::task::spawn_blocking(move || (super::setup::clients_report(), catalog.read())).await;
    let (mut report, listing) = match reading {
        Ok(reading) => reading,
        Err(error) => return refused(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    };
    futures_util::future::join_all(
        listing
            .repositories
            .iter()
            .map(|listed| read_sessions(&hub, listed)),
    )
    .await;
    let last = last_sessions(&locked(&hub.home.sessions));
    for client in report["clients"].as_array_mut().into_iter().flatten() {
        let session = client["id"]
            .as_str()
            .and_then(|id| last.get(id))
            .cloned()
            .unwrap_or(Value::Null);
        client["last_session"] = session;
    }
    Json(report).into_response()
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
    /// The last count this server read, at any generation: whether the
    /// working copy held work, how many entities it touched, and when. Kept
    /// so a row whose daemon stops answering says what was last known.
    counted: Option<(bool, usize, chrono::DateTime<chrono::Utc>)>,
    in_flight: bool,
    /// A read that failed is not started again before this.
    retry_after: Option<tokio::time::Instant>,
}

/// What a repository's daemon last told this server about its repository,
/// and when. A daemon that is busy, wedged or stopped leaves this as it
/// was, so the row says what Kin last knew instead of dropping to saying
/// that nothing is in motion.
#[derive(Clone, Debug, Default, PartialEq)]
struct Known {
    /// When the daemon last answered this server.
    at: Option<chrono::DateTime<chrono::Utc>>,
    /// The generations its branch listing was last read at.
    generation: Option<Generation>,
    branch: Option<String>,
    agents: Option<Value>,
    merge_in_progress: Option<bool>,
    activity: Option<super::daemon_activity::DaemonActivity>,
}

/// The reading of a daemon that runs in the background of the requests that
/// want it.
#[derive(Default)]
struct Asking {
    in_flight: bool,
    /// When the running reading started.
    started: Option<tokio::time::Instant>,
    /// When the last reading finished, and whether the daemon answered it.
    finished: Option<(tokio::time::Instant, bool)>,
}

struct Slot {
    reading: Mutex<Reading>,
    known: Mutex<Known>,
    asking: Mutex<Asking>,
    /// Bumped whenever `reading`, `known` or `asking` gains something, for a
    /// request waiting on it.
    moved: tokio::sync::watch::Sender<u64>,
}

impl Default for Slot {
    fn default() -> Self {
        Self {
            reading: Mutex::default(),
            known: Mutex::default(),
            asking: Mutex::default(),
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
                reading.counted = reading
                    .differs
                    .map(|differs| (differs, changed, chrono::Utc::now()));
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

/// Whether a repository's daemon answered for one reading of the list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reach {
    /// It answered, for this request or a moment before it.
    Answering,
    /// It is running and has not answered for a while.
    NotAnswering,
    /// No daemon is running for the repository.
    Stopped,
}

impl Reach {
    fn word(self) -> &'static str {
        match self {
            Self::Answering => "answering",
            Self::NotAnswering => "not_answering",
            Self::Stopped => "stopped",
        }
    }
}

/// What a row says about a repository's daemon. A reading that did not
/// arrive in time is what the daemon last told this server, with when it
/// told it, or is left out; it is never guessed and never dropped to
/// "nothing in motion".
struct Observed {
    running: bool,
    reach: Reach,
    /// When the branch, working copy, agents and merge the row carries were
    /// last read, when that was not for this request.
    known_at: Option<chrono::DateTime<chrono::Utc>>,
    branch: Option<String>,
    working: Option<Working>,
    agents: Option<Value>,
    merge_in_progress: Option<bool>,
    activity: super::daemon_activity::DaemonActivity,
    changed_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// The daemon already serving `layout`, when its recorded endpoint answers
/// and proves it is that repository's and this release's. Reads recorded
/// endpoint metadata and verifies health without discovery, cleanup, startup
/// or process control, except to replace a daemon of an older release, which
/// the page never reads through.
async fn running_daemon(layout: &kin_core::KinLayout) -> Option<DaemonClient> {
    let (pid, port) = crate::daemon_client::repo_daemon_recorded_endpoint(layout.root());
    let port = pid.and(port).filter(|port| *port != 0)?;
    DaemonClient::for_workspace_page(
        "local UI",
        format!("http://127.0.0.1:{port}"),
        layout,
        Duration::from_secs(10),
    )
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

/// How long a reading of a daemon, once started, waits on it after no
/// request is waiting on the answer any more.
const ASK_PATIENCE: Duration = Duration::from_secs(20);
/// A reading that finished this shortly before a request began is current
/// enough for it, so several pages polling at once ask a daemon once.
const ASKED_RECENTLY: Duration = Duration::from_secs(2);
/// How long what a daemon said still counts as current when it is slow to
/// answer again. Past this the row says what it shows was last known.
const STILL_CURRENT: Duration = Duration::from_secs(60);

/// What a row carries for a repository whose daemon did not answer this
/// request: what the daemon last told this server, and when.
#[derive(Debug, PartialEq)]
struct Carried {
    reach: Reach,
    known_at: Option<chrono::DateTime<chrono::Utc>>,
    branch: Option<String>,
    working: Option<Working>,
    agents: Option<Value>,
    merge_in_progress: Option<bool>,
}

/// What a row says for a repository whose daemon did not answer this
/// request. `running` is whether a daemon process is alive for it. What the
/// daemon told this server before is said with when it was told: a busy or
/// wedged daemon is still working in a repository that holds the same work,
/// branch and agents, and a stopped one still leaves its work, branch and
/// merge as they were. A stopped daemon holds no agent session, so no agent
/// is carried for it. An answer within [`STILL_CURRENT`] is current, so the
/// daemon still counts as answering.
fn unanswered(
    known: &Known,
    counted: Option<(bool, usize, chrono::DateTime<chrono::Utc>)>,
    running: bool,
    now: chrono::DateTime<chrono::Utc>,
) -> Carried {
    let recent = known.at.is_some_and(|at| {
        (now - at)
            .to_std()
            .ok()
            .is_none_or(|age| age <= STILL_CURRENT)
    });
    let reach = if !running {
        Reach::Stopped
    } else if recent {
        Reach::Answering
    } else {
        Reach::NotAnswering
    };
    let Some(at) = known.at else {
        return Carried {
            reach,
            known_at: None,
            branch: None,
            working: None,
            agents: None,
            merge_in_progress: None,
        };
    };
    let working = counted.map(|(differs, changed, _)| Working::Read {
        differs,
        changed: Some(changed),
    });
    // The oldest of what is carried says when the row was last known.
    let known_at = counted.map_or(at, |(_, _, read)| read.min(at));
    Carried {
        reach,
        known_at: Some(known_at),
        branch: known.branch.clone(),
        working,
        agents: if reach == Reach::Stopped {
            None
        } else {
            known.agents.clone()
        },
        merge_in_progress: known.merge_in_progress,
    }
}

/// The active branch a listing names, without its `refs/heads/` prefix.
fn active_branch(report: &super::branch::BranchListReport) -> Option<String> {
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
}

/// Start counting a repository's working copy unless what it would read is
/// already held for `wanted`, or a count is running or waiting to retry.
fn start_reading(
    client: &DaemonClient,
    slot: &Arc<Slot>,
    wanted: Option<Generation>,
    since: tokio::time::Instant,
) {
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

/// Ask a repository's running daemon what it says of its branch, its merge,
/// its agents and what it is doing, keep each answer in `slot` as it is
/// given, and start counting its working copy. Runs on its own, so an answer
/// that arrives after every request stopped waiting is kept for the next.
async fn ask_daemon(
    hub: Arc<Hub>,
    key: String,
    name: String,
    layout: kin_core::KinLayout,
    slot: Arc<Slot>,
) {
    use super::daemon_activity::{read_activity, Reachability};

    let started = tokio::time::Instant::now();
    let answered = match tokio::time::timeout(ASK_PATIENCE, running_daemon(&layout)).await {
        Ok(Some(client)) => {
            locked(&slot.known).at = Some(chrono::Utc::now());
            slot.announce();
            let list = super::branch::BranchRequest::List;
            let listing = super::conflicts::ConflictsRequest { bodies: false };
            let root = layout.working_dir().display().to_string();
            let (branches, conflicts, agents, activity) = tokio::join!(
                tokio::time::timeout(ASK_PATIENCE, client.branch(&list)),
                tokio::time::timeout(ASK_PATIENCE, client.conflicts(&listing)),
                tokio::time::timeout(ASK_PATIENCE, client.agents()),
                tokio::time::timeout(
                    ASK_PATIENCE,
                    read_activity(client.base_url().to_string(), root, Reachability::Answering),
                ),
            );
            // What the list reads anyway is also what says which coding
            // agents work here, so it is remembered for the page that asks
            // about them.
            if let Ok(Ok(answer)) = &agents {
                sessions_listed(&mut locked(&hub.home.sessions), &key, &name, Some(answer));
            }
            let branches = branches
                .ok()
                .and_then(Result::ok)
                .and_then(|answer| answer.report);
            let generation = branches
                .as_ref()
                .map(|report| (report.authority_generation, report.workspace_generation));
            {
                let mut known = locked(&slot.known);
                known.at = Some(chrono::Utc::now());
                if let Some(report) = &branches {
                    known.generation = generation;
                    known.branch = active_branch(report);
                }
                if let Some(seen) = agents
                    .ok()
                    .and_then(Result::ok)
                    .as_ref()
                    .and_then(agents_seen)
                {
                    known.agents = Some(seen);
                }
                if let Some(merging) = conflicts
                    .ok()
                    .and_then(Result::ok)
                    .and_then(|answer| answer.report)
                    .map(|report| {
                        report.record.is_some_and(|record| {
                            matches!(record.state, kin_model::MergeTransactionState::InProgress)
                        })
                    })
                {
                    known.merge_in_progress = Some(merging);
                }
                if let Ok(activity) = activity {
                    known.activity = Some(activity);
                }
            }
            start_reading(&client, &slot, generation, started);
            true
        }
        _ => false,
    };
    {
        let mut asking = locked(&slot.asking);
        asking.in_flight = false;
        asking.finished = Some((tokio::time::Instant::now(), answered));
    }
    slot.announce();
}

async fn observe(
    hub: &Arc<Hub>,
    listed: &Listed,
    standing: &kin_core::history_state::HistoryStanding,
) -> Observed {
    use super::daemon_activity::{activity_without_health, Reachability};

    let since = tokio::time::Instant::now();
    let deadline = since + OBSERVE_BUDGET;
    let recorded = listed
        .record
        .as_ref()
        .and_then(|record| recorded_time(&record.at));
    let slot = hub.home.slot(&listed.entry.key);
    let history = super::daemon_activity::running_history(standing);
    let (pid, _) = crate::daemon_client::repo_daemon_recorded_endpoint(listed.layout.root());
    let alive = pid.is_some_and(crate::daemon_client::is_process_alive);
    if !alive {
        // No daemon is running. A history derivation runs in a worker of
        // its own, so it can still be what this repository is doing.
        let known = locked(&slot.known).clone();
        let (counted, head) = {
            let reading = locked(&slot.reading);
            (reading.counted, reading.head_change)
        };
        let carried = unanswered(&known, counted, false, chrono::Utc::now());
        return Observed {
            running: false,
            reach: carried.reach,
            known_at: carried.known_at,
            branch: carried.branch,
            working: carried.working,
            agents: carried.agents,
            merge_in_progress: carried.merge_in_progress,
            activity: activity_without_health(Reachability::Gone, history.as_ref()),
            changed_at: last_change(recorded, head, false),
        };
    }

    // Ask the daemon in the background unless a reading is under way or one
    // just finished, then wait for it only as long as this request has.
    let mut moved = slot.moved.subscribe();
    {
        let mut asking = locked(&slot.asking);
        let recent = asking.finished.is_some_and(|(at, answered)| {
            answered && since.saturating_duration_since(at) <= ASKED_RECENTLY
        });
        if !asking.in_flight && !recent {
            asking.in_flight = true;
            asking.started = Some(since);
            tokio::spawn(ask_daemon(
                hub.clone(),
                listed.entry.key.clone(),
                listed.entry.name.clone(),
                listed.layout.clone(),
                slot.clone(),
            ));
        }
    }
    let fresh = loop {
        {
            let asking = locked(&slot.asking);
            match asking.finished {
                Some((at, answered)) if since.saturating_duration_since(at) <= ASKED_RECENTLY => {
                    break answered;
                }
                _ if !asking.in_flight => break false,
                _ => {}
            }
        }
        if tokio::time::timeout_at(deadline, moved.changed())
            .await
            .is_err()
        {
            break false;
        }
    };
    if !fresh {
        let known = locked(&slot.known).clone();
        let (counted, head) = {
            let reading = locked(&slot.reading);
            (reading.counted, reading.head_change)
        };
        let carried = unanswered(&known, counted, true, chrono::Utc::now());
        let activity = match (&carried.reach, &known.activity) {
            (Reach::Answering, Some(activity)) => activity.clone(),
            _ => activity_without_health(Reachability::NotAnswering, history.as_ref()),
        };
        return Observed {
            running: true,
            reach: carried.reach,
            known_at: carried.known_at,
            branch: carried.branch,
            working: Some(carried.working.unwrap_or(Working::Unknown)),
            agents: carried.agents,
            merge_in_progress: carried.merge_in_progress,
            activity,
            changed_at: last_change(recorded, head, false),
        };
    }

    // The daemon answered. Wait for the count of its working copy at the
    // generation it answered at, as long as this request has.
    let known = locked(&slot.known).clone();
    let wanted = known.generation;
    let counting_since = locked(&slot.asking).started.unwrap_or(since);
    let (working, head, head_is_current) = loop {
        {
            let reading = locked(&slot.reading);
            let working = working_from(&reading, wanted, counting_since);
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
                working_from(&reading, wanted, counting_since),
                reading.head_change,
                current,
            );
        }
    };
    Observed {
        running: true,
        reach: Reach::Answering,
        known_at: None,
        branch: known.branch,
        working: Some(working),
        agents: known.agents,
        merge_in_progress: known.merge_in_progress,
        activity: known.activity.unwrap_or_else(|| {
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
                "reach": observed.reach.word(),
                "activity": observed.activity,
                "history": super::workspace_ui::history_json(standing),
            });
            if let Some(owner) = &entry.owner {
                row["owner"] = json!(owner);
            }
            if let Some(host) = &entry.host {
                row["host"] = json!(host);
            }
            if let Some(at) = observed.known_at {
                row["known_at"] = json!(at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
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
    fn a_daemon_that_does_not_answer_leaves_what_it_last_said_with_when_it_said_it() {
        let at = |text: &str| recorded_time(text).unwrap();
        let answered = at("2026-10-14T09:12:00Z");
        let counted = at("2026-10-14T09:11:30Z");
        let agents = json!({ "active": 1, "connected": 1, "names": ["Claude Code"] });
        let known = Known {
            at: Some(answered),
            generation: Some((7, 9)),
            branch: Some("main".to_string()),
            agents: Some(agents.clone()),
            merge_in_progress: Some(false),
            activity: None,
        };
        let work = Some((true, 4, counted));
        let later = at("2026-10-14T09:40:00Z");

        // Busy or wedged: still running, so everything it said is carried,
        // said to be as of the oldest reading carried, and never dropped to
        // "nothing in motion".
        let busy = unanswered(&known, work, true, later);
        assert_eq!(
            busy,
            Carried {
                reach: Reach::NotAnswering,
                known_at: Some(counted),
                branch: Some("main".to_string()),
                working: Some(Working::Read {
                    differs: true,
                    changed: Some(4)
                }),
                agents: Some(agents.clone()),
                merge_in_progress: Some(false),
            }
        );
        assert_eq!(
            busy.working.unwrap().json(),
            json!({ "differs": true, "changed": 4 })
        );

        // Stopped: the work, branch and merge stay as they were, but no
        // agent session can outlive the daemon that held it.
        let stopped = unanswered(&known, work, false, later);
        assert_eq!(stopped.reach, Reach::Stopped);
        assert_eq!(stopped.agents, None);
        assert_eq!(stopped.branch.as_deref(), Some("main"));
        assert_eq!(stopped.known_at, Some(counted));

        // Slow to answer again a moment after it last did: still current.
        let moment = at("2026-10-14T09:12:40Z");
        let slow = unanswered(&known, None, true, moment);
        assert_eq!(slow.reach, Reach::Answering);
        assert_eq!(slow.known_at, Some(answered));
        assert_eq!(
            slow.working, None,
            "a working copy never counted is not guessed"
        );

        // Never heard from: nothing is carried and nothing is claimed.
        let never = unanswered(&Known::default(), None, true, later);
        assert_eq!(never.reach, Reach::NotAnswering);
        assert_eq!(never.known_at, None);
        assert_eq!(never.branch, None);
        assert_eq!(never.agents, None);
        assert_eq!(
            unanswered(&Known::default(), None, false, later).reach,
            Reach::Stopped
        );
        assert_eq!(
            [Reach::Answering, Reach::NotAnswering, Reach::Stopped].map(Reach::word),
            ["answering", "not_answering", "stopped"]
        );
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
        let ended = run_child(OsStr::new("sh"), &arguments, None, false, &[], &keep)
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

    /// A home directory with projects kept the ways people keep them, and
    /// with the places a look must leave alone.
    fn a_home() -> tempfile::TempDir {
        let home = tempfile::tempdir().unwrap();
        let make = |path: &str| std::fs::create_dir_all(home.path().join(path)).unwrap();
        let write = |path: &str, text: &str| std::fs::write(home.path().join(path), text).unwrap();
        // A Git project in a code root, with a dependency tree of its own.
        make("GitHub/alpha/.git");
        write("GitHub/alpha/.git/HEAD", "ref: refs/heads/trunk\n");
        make("GitHub/alpha/node_modules/dep/.git");
        // A project Git does not hold, kept under its owner's name.
        make("GitHub/owner/beta");
        write("GitHub/owner/beta/Cargo.toml", "");
        // Three levels down is further than a look goes.
        make("GitHub/owner/group/deep/.git");
        // What tools install, and a directory that is no project.
        make("GitHub/node_modules/pkg/.git");
        make("GitHub/notes");
        write("GitHub/notes/todo.txt", "");
        write("GitHub/readme.txt", "");
        // One more working copy of a project that is kept elsewhere.
        make("GitHub/alpha-second");
        write(
            "GitHub/alpha-second/.git",
            "gitdir: ../alpha/.git/worktrees/second\n",
        );
        write("GitHub/alpha-second/Cargo.toml", "");
        // A project directly in the home directory, and one inside a
        // directory there that is not named like a code root.
        make("loose/.git");
        make("stuff/inner/.git");
        // Where a person's documents are, and a hidden directory.
        make("Documents/report/.git");
        make(".cache/tool/.git");
        home
    }

    fn found_in(home: &Path, look: &Look) -> Vec<String> {
        look.spotted
            .iter()
            .map(|spotted| {
                spotted
                    .path
                    .strip_prefix(home)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect()
    }

    #[test]
    fn a_look_finds_projects_where_people_keep_them_and_nowhere_else() {
        let home = a_home();
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            home.path().join("GitHub/alpha"),
            home.path().join("GitHub/linked"),
        )
        .unwrap();
        let far = std::time::Instant::now() + Duration::from_secs(60);
        let look = look_around(home.path(), far);
        let mut found = found_in(home.path(), &look);
        found.sort();
        assert_eq!(found, ["GitHub/alpha", "GitHub/owner/beta", "loose"]);
        assert!(look.complete);

        // The most recently changed project leads, whichever place holds it.
        let day = Duration::from_secs(24 * 60 * 60);
        let age = |path: &str, days: u32| {
            let time = filetime::FileTime::from_system_time(SystemTime::now() - day * days);
            filetime::set_file_mtime(home.path().join(path), time).unwrap();
        };
        for (path, days) in [
            ("GitHub/alpha", 9),
            ("GitHub/alpha/.git", 2),
            ("GitHub/owner/beta", 5),
            ("loose", 7),
            ("loose/.git", 8),
        ] {
            age(path, days);
        }
        let look = look_around(home.path(), far);
        assert_eq!(
            found_in(home.path(), &look),
            ["GitHub/alpha", "GitHub/owner/beta", "loose"]
        );

        // A look that is out of time says it did not finish, and guesses
        // nothing.
        let late = look_around(home.path(), std::time::Instant::now());
        assert!(late.spotted.is_empty() && !late.complete);
        // A home directory that cannot be read holds no project.
        assert!(look_around(&home.path().join("absent"), far)
            .spotted
            .is_empty());
    }

    #[test]
    fn a_directory_is_a_project_by_the_name_of_what_is_at_its_top() {
        for name in [
            "package.json",
            "Cargo.toml",
            "go.mod",
            "Makefile",
            "App.xcodeproj",
        ] {
            assert!(marks_a_project(OsStr::new(name)), "{name}");
        }
        for name in ["notes.txt", "src", "README.md", "cargo.toml.bak"] {
            assert!(!marks_a_project(OsStr::new(name)), "{name}");
        }
        for name in [
            "node_modules",
            "target",
            "Library",
            "Documents",
            ".git",
            ".venv",
        ] {
            assert!(!looked_into(OsStr::new(name)), "{name}");
        }
        assert!(looked_into(OsStr::new("GitHub")) && looked_into(OsStr::new("kin")));
    }

    /// The projects a person is likely to add are listed without a location
    /// being typed, each described as `inspect` describes it.
    #[tokio::test]
    async fn candidates_are_listed_newest_first_with_what_may_be_done_with_each() {
        let home = a_home();
        let day = Duration::from_secs(24 * 60 * 60);
        for (path, days) in [
            ("GitHub/alpha", 1),
            ("GitHub/alpha/.git", 1),
            ("GitHub/owner/beta", 2),
            ("loose", 3),
            ("loose/.git", 3),
        ] {
            let time = filetime::FileTime::from_system_time(SystemTime::now() - day * days);
            filetime::set_file_mtime(home.path().join(path), time).unwrap();
        }
        let listener = review_ui::bind(Some(0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let hub = Hub::new(port, super::super::workspace_ui::empty_catalog(), None);
        hub.home.look_in(home.path());
        let router = super::super::workspace_ui::hub_router(hub.clone());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let client = reqwest::Client::new();
        let origin = format!("http://127.0.0.1:{port}");

        let listed: Value = client
            .get(format!("{origin}/api/home/candidates"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(listed["schema"], "kin.workspace.home-candidates.v1");
        assert_eq!(listed["found"], 3);
        assert_eq!(listed["complete"], true);
        let rows = listed["candidates"].as_array().unwrap();
        let names: Vec<&str> = rows.iter().filter_map(|row| row["name"].as_str()).collect();
        assert_eq!(names, ["alpha", "beta", "loose"]);
        let alpha = &rows[0];
        assert_eq!(alpha["kind"], "git");
        assert_eq!(alpha["git"]["branch"], "trunk");
        assert_eq!(alpha["actions"], json!(["convert"]));
        assert_eq!(alpha["registered"], false);
        assert!(alpha["modified_at"]
            .as_str()
            .is_some_and(|at| at.ends_with('Z')));
        assert!(alpha["note"]
            .as_str()
            .unwrap()
            .starts_with("Kin reads this project"));
        // A project is added by the location this answer gives for it.
        let resolved = home.path().canonicalize().unwrap();
        assert_eq!(
            alpha["path"],
            resolved.join("GitHub/alpha").display().to_string()
        );
        let beta = &rows[1];
        assert_eq!(beta["kind"], "plain");
        assert_eq!(beta["actions"], json!(["convert"]));
        assert!(beta.get("git").is_none());
        // A caller may ask for fewer, and is still told how many were found.
        let settled: Value = client
            .get(format!("{origin}/api/home/candidates?limit=1"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(settled["candidates"].as_array().unwrap().len(), 1);
        assert_eq!(settled["found"], 3);
        assert!(settled["counting"].is_u64());

        let refused = client
            .get(format!("{origin}/api/home/candidates?path=/"))
            .send()
            .await
            .unwrap();
        assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
        server.abort();
    }

    fn listed_agent(session: &str, vendor: &str, state: &str, at: &str) -> Value {
        json!({
            "session_id": session, "vendor": vendor, "client": vendor, "state": state,
            "started_at": "2026-10-13T08:00:00Z", "last_activity_at": at,
        })
    }

    #[test]
    fn the_last_session_of_a_coding_agent_is_the_newest_any_repository_showed() {
        let mut sessions = HashMap::new();
        // Two repositories are running. Claude Code is at work in one, and
        // sits idle in the other beside Codex.
        let atlas = json!({"agents": [
            listed_agent("S-ONE", "claude-code", "active", "2026-10-13T10:00:00Z"),
            listed_agent("s-two", "my-helper", "active", "2026-10-13T11:00:00Z"),
        ]});
        let harbor = json!({"agents": [
            listed_agent("s-three", "codex-mcp-client", "idle", "2026-10-13T09:00:00Z"),
            listed_agent("s-four", "claude-code", "idle", "2026-10-13T09:30:00Z"),
        ]});
        sessions_listed(&mut sessions, "k1", "atlas", Some(&atlas));
        sessions_listed(&mut sessions, "k2", "harbor", Some(&harbor));
        let last = last_sessions(&sessions);
        assert_eq!(
            last["claude-code"],
            json!({
                "repository": {"key": "k1", "name": "atlas", "url": "/repositories/k1/workspace/"},
                "at": "2026-10-13T10:00:00Z",
                "state": "active",
            })
        );
        assert_eq!(last["codex"]["state"], "idle");
        assert_eq!(last["codex"]["repository"]["name"], "harbor");
        // An agent Kin does not connect is no coding agent's session, and
        // no session is named by what identifies it.
        assert_eq!(last.len(), 2);
        assert!(!json!(last).to_string().to_lowercase().contains("s-one"));

        // A session its repository no longer lists has ended, and is still
        // the last one known when nothing newer is.
        sessions_listed(&mut sessions, "k1", "atlas", Some(&json!({"agents": []})));
        let last = last_sessions(&sessions);
        assert_eq!(last["claude-code"]["state"], "ended");
        assert_eq!(last["claude-code"]["repository"]["name"], "atlas");
        assert_eq!(last["claude-code"]["at"], "2026-10-13T10:00:00Z");

        // An answer that is not a list says nothing either way. A repository
        // that is no longer running has nothing connected.
        sessions_listed(
            &mut sessions,
            "k2",
            "harbor",
            Some(&json!({"error": "starting"})),
        );
        assert_eq!(last_sessions(&sessions)["codex"]["state"], "idle");
        sessions_listed(&mut sessions, "k2", "harbor", None);
        assert_eq!(last_sessions(&sessions)["codex"]["state"], "ended");
        assert_eq!(last_sessions(&sessions)["claude-code"]["state"], "ended");
    }

    #[test]
    fn a_change_a_coding_agent_made_is_a_record_of_its_session() {
        let mut sessions = HashMap::new();
        let noon = recorded_time("2026-10-13T12:00:00Z").unwrap();
        let author = "gemini-cli-mcp-client/Gemini <mcp-agent:s-five>";
        session_committed(&mut sessions, "k2", "harbor", author, noon);
        // A person's change, and an agent's that Kin does not connect.
        session_committed(
            &mut sessions,
            "k2",
            "harbor",
            "Ada <ada@example.invalid>",
            noon,
        );
        session_committed(
            &mut sessions,
            "k2",
            "harbor",
            "my-helper/x <mcp-agent:s-six>",
            noon,
        );
        let last = last_sessions(&sessions);
        assert_eq!(last.len(), 1);
        assert_eq!(last["gemini-cli"]["state"], "ended");
        assert_eq!(last["gemini-cli"]["at"], "2026-10-13T12:00:00Z");

        // The session is still connected: its change moves when it was last
        // at work and ends nothing.
        let listed = json!({"agents": [
            listed_agent("s-five", "gemini-cli-mcp-client", "idle", "2026-10-13T11:00:00Z"),
        ]});
        sessions_listed(&mut sessions, "k2", "harbor", Some(&listed));
        let last = last_sessions(&sessions);
        assert_eq!(last["gemini-cli"]["state"], "idle");
        assert_eq!(last["gemini-cli"]["at"], "2026-10-13T12:00:00Z");
    }

    #[test]
    fn remembered_sessions_stay_bounded_and_keep_the_connected_ones() {
        let mut sessions = HashMap::new();
        let connected = json!({"agents": [
            listed_agent("still-here", "codex", "idle", "2020-01-01T00:00:00Z"),
        ]});
        sessions_listed(&mut sessions, "k1", "atlas", Some(&connected));
        let start = recorded_time("2026-10-13T00:00:00Z").unwrap();
        for index in 0..SESSIONS_KEPT + 20 {
            let author = format!("claude-code/Claude Code <mcp-agent:ended-{index}>");
            let at = start + chrono::Duration::seconds(index as i64);
            session_committed(&mut sessions, "k2", "harbor", &author, at);
        }
        assert_eq!(sessions.len(), SESSIONS_KEPT);
        assert!(sessions.contains_key("still-here"));
        assert!(!sessions.contains_key("ended-0"));
        assert!(sessions.contains_key(&format!("ended-{}", SESSIONS_KEPT + 19)));
    }

    /// The coding agents page is told, for each agent, the last session Kin
    /// has a record of, or that it has none.
    #[tokio::test]
    async fn every_coding_agent_says_its_last_session_or_that_kin_has_none() {
        let listener = review_ui::bind(Some(0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let hub = Hub::new(port, super::super::workspace_ui::empty_catalog(), None);
        let working = json!({"agents": [
            listed_agent("s-one", "claude-code", "active", "2026-10-13T10:00:00Z"),
        ]});
        sessions_listed(
            &mut locked(&hub.home.sessions),
            "k1",
            "atlas",
            Some(&working),
        );
        let router = super::super::workspace_ui::hub_router(hub.clone());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let answer: Value = reqwest::Client::new()
            .get(format!("http://127.0.0.1:{port}/api/home/clients"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(answer["schema"], "kin.setup-clients.v1");
        let clients = answer["clients"].as_array().unwrap();
        assert!(clients.len() >= 8);
        for client in clients {
            let last = client.get("last_session").expect("every agent says");
            if client["id"] == "claude-code" {
                assert_eq!(last["state"], "active");
                assert_eq!(last["repository"]["name"], "atlas");
                assert_eq!(last["at"], "2026-10-13T10:00:00Z");
            } else {
                assert!(last.is_null(), "{client}");
            }
            // What the list already said of each agent is still said.
            assert!(client["detected"].is_boolean() && client["connected"].is_boolean());
        }
        server.abort();
    }
}
