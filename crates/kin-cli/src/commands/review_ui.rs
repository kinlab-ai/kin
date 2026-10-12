// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! `kin review open`: the guided review as a page, served from this machine.
//!
//! The page is a reader for what `kin review guide --json` answers and for the
//! history `kin log` answers. It holds no state of its own and computes
//! nothing about the repository: every request it makes is forwarded to the
//! repository's daemon, which is the one authority a terminal command asks
//! too, so the page and the terminal cannot describe two different changes.
//!
//! It is local by construction. The server binds 127.0.0.1 only, the page and
//! its assets are compiled into this binary, and it loads nothing from the
//! network. It serves source text, so it also refuses any request whose `Host`
//! is not this server's own loopback address. Without that check a page on
//! another origin could resolve a name it controls to 127.0.0.1 and read the
//! answers as its own.
//!
//! The page can also record a decision or a note. Those are repository
//! writes, made the way `kin review decide` and `kin review note` make them:
//! the daemon's review writer plans and commits each one, under the actor the
//! terminal would record. A write needs more than a read does. Any page in the
//! reader's browser can send a request to a loopback port, so a write must
//! carry a token this process minted and handed only to its own page, and an
//! `Origin` that is this server's own when the browser sends one. Another
//! origin can neither read the token nor forge the header without a preflight
//! this server never answers.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::{Query, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;

use super::review::{GuideTarget, ReviewRequest};

const INDEX_HTML: &str = include_str!("../../assets/review_ui/index.html");
const APP_JS: &str = include_str!("../../assets/review_ui/app.js");
const STYLE_CSS: &str = include_str!("../../assets/review_ui/style.css");
const MARK_SVG: &str = include_str!("../../assets/review_ui/mark.svg");
const DIFF_JS: &str = include_str!("../../assets/review_ui/diff.js");
const MAP_JS: &str = include_str!("../../assets/review_ui/map.js");
const HIGHLIGHT_JS: &str = include_str!("../../assets/review_ui/highlight.js");

/// The port the page asks for first. The daemon's default is 4219 and
/// `kin graph viz` takes 4220.
pub const DEFAULT_PORT: u16 = 4221;

/// How many ports past the default are tried before asking the system for
/// any free one.
const PORT_ATTEMPTS: u16 = 10;

/// How many changes the history rail asks for when the page names no count,
/// and the most one read answers. A longer history is read a page at a time.
const CHANGES_DEFAULT: usize = 60;
const CHANGES_MAX: usize = 500;

/// Everything the page may load, and nothing else: its own script and style,
/// no inline script, no frames, no other origin.
const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; script-src 'self'; style-src 'self'; \
     img-src 'self' data:; connect-src 'self'; base-uri 'none'; form-action 'none'; \
     frame-ancestors 'none'";

/// The header a write carries its token in.
const WRITE_TOKEN_HEADER: &str = "x-kin-review-token";

/// The longest comment or note the page records, in characters.
const TEXT_MAX_CHARS: usize = 10_000;

/// The longest title a review the page opens is given.
const TITLE_MAX_CHARS: usize = 200;

pub(super) struct UiState {
    pub(super) layout: kin_core::KinLayout,
    pub(super) repository: String,
    /// The working tree's own path, so the page can hand a step's place to
    /// an editor on this machine.
    pub(super) root: String,
    /// What the command was asked to open first.
    initial: GuideTarget,
    pub(super) port: u16,
    /// Minted once per run and handed to this server's own page only.
    pub(super) token: String,
    /// Who a decision or note made on the page is signed as; see
    /// [`page_actor`].
    pub(super) actor: String,
    /// Held while a write finds the review it lands on or opens one. Finding
    /// and opening are two requests to the daemon, so two first writes on one
    /// change, a decision and a note sent together, would each find nothing
    /// and each open a review. One at a time, the second finds the first's.
    opening_review: tokio::sync::Mutex<()>,
    pub(super) base_path: String,
    pub(super) catalog: Option<Arc<super::workspace_ui::Catalog>>,
    /// The agent sessions this server has seen, for the pages that show them.
    pub(super) agents: super::workspace_agents::AgentMemory,
}

impl UiState {
    pub(super) fn workspace(
        layout: kin_core::KinLayout,
        port: u16,
        base_path: String,
        catalog: Arc<super::workspace_ui::Catalog>,
    ) -> Self {
        let root = layout.working_dir().to_string_lossy().into_owned();
        let repository = layout
            .working_dir()
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "repository".into());
        Self {
            repository,
            root,
            initial: GuideTarget::default(),
            port,
            token: uuid::Uuid::new_v4().simple().to_string(),
            actor: page_actor(&layout),
            layout,
            opening_review: tokio::sync::Mutex::new(()),
            base_path,
            catalog: Some(catalog),
            agents: Default::default(),
        }
    }
}

/// Who a decision or note made on the page is signed as: `KIN_ACTOR` when
/// it is set, then the name this repository's commits carry, then the
/// shell's user. The page shows the reader the name they are recorded under,
/// and that is the name their commits already carry.
pub(super) fn page_actor(layout: &kin_core::KinLayout) -> String {
    if let Some(actor) = std::env::var("KIN_ACTOR")
        .ok()
        .filter(|value| !value.trim().is_empty())
    {
        return actor;
    }
    kin_core::resolve_commit_identity(layout)
        .ok()
        .map(|identity| author_display(&identity.author))
        .filter(|name| !name.is_empty() && name != "Unnamed author")
        .unwrap_or_else(crate::provenance::current_actor_label)
}

/// Whether Kin recorded this change for itself: a change of Kin's own
/// making, such as the checkpoint `kin upgrade` or a daemon's start records
/// when a new build of Kin reads the same code differently. It is native to
/// Kin, touches no code and no admission policy, and follows one parent.
/// Read from the change's own shape, never from its title, so a store made
/// by any release reads the same.
pub(super) fn is_kin_checkpoint(entry: &super::log::LogEntry) -> bool {
    matches!(entry.origin, kin_model::ChangeOrigin::Native)
        && entry.tree_delta_count == 0
        && !entry.admission_policy_changed
        && entry.parents.len() == 1
}

#[derive(Serialize)]
struct SessionJson<'a> {
    repository_key: String,
    repository: &'a str,
    root: &'a str,
    /// The editor this shell names or was started from, when it is one the
    /// page knows how to open a place in.
    #[serde(skip_serializing_if = "Option::is_none")]
    editor: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    initial_change: Option<&'a str>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    initial_working: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    initial_base: Option<&'a str>,
    version: &'static str,
    actor: &'a str,
    token: &'a str,
    /// The repository's owner and host, as its remote names them.
    #[serde(skip_serializing_if = "Option::is_none")]
    owner: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    host: Option<String>,
}

#[derive(Deserialize)]
struct ReviewsQuery {
    change: String,
    /// Set for a range: the change it reads "before" at.
    #[serde(default)]
    range_base: Option<String>,
}

/// The review a write lands on: the change it is about, and what a new review
/// of it is opened with when none is stored yet.
#[derive(Deserialize)]
struct ReviewTarget {
    change: String,
    #[serde(default)]
    base: Option<String>,
    #[serde(default)]
    title: Option<String>,
    /// Whether the write is about the range from `base` to `change` and not
    /// about `change` alone. A range keeps a review of its own, apart from
    /// the review of its last change.
    #[serde(default)]
    range: bool,
}

impl ReviewTarget {
    /// The base of the review a write lands on. A write about one change
    /// lands on a review from that change's own base, and one about a range
    /// on a review from the range's. Reviews that only share the head are
    /// someone else's: a decision on the last change of a branch must not
    /// join the review of the whole branch.
    fn write_base(&self) -> &str {
        self.base.as_deref().unwrap_or(&self.change)
    }

    /// The base a range's reviews are read by, when this names a range.
    fn range_base(&self) -> Option<&str> {
        if self.range {
            self.base.as_deref()
        } else {
            None
        }
    }
}

#[derive(Deserialize)]
struct DecideBody {
    #[serde(flatten)]
    target: ReviewTarget,
    state: String,
    #[serde(default)]
    comment: Option<String>,
}

#[derive(Deserialize)]
struct NoteBody {
    #[serde(flatten)]
    target: ReviewTarget,
    body: String,
    /// The entity the note is about. A note with none is about the change.
    #[serde(default)]
    entity: Option<String>,
}

/// The one field of the daemon's reviews answer this server reads: which
/// review a write lands on. The rest is passed to the page as it was answered.
#[derive(Deserialize)]
struct ReviewsJson {
    reviews: Vec<StoredReview>,
}

#[derive(Deserialize)]
struct StoredReview {
    review_id: String,
}

#[derive(Serialize)]
struct ChangeRowJson {
    id: String,
    title: String,
    author: String,
    /// The agent that made the change, when an agent session did.
    #[serde(skip_serializing_if = "Option::is_none")]
    agent: Option<serde_json::Value>,
    timestamp: String,
    origin: &'static str,
    parents: usize,
    /// Entity deltas whose entity's own content moved.
    entity_deltas: usize,
    /// The deltas that count leaves out: ones that moved only a span or its
    /// source provenance. Apart from `entity_deltas`, never part of it.
    entity_deltas_unchanged: usize,
    relation_deltas: usize,
    /// Set when Kin recorded the change for itself and it changes no code.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    kin_checkpoint: bool,
}

#[derive(Serialize)]
struct ChangesJson {
    changes: Vec<ChangeRowJson>,
    /// Whether history holds more changes past the last one answered.
    truncated: bool,
    requested: usize,
    /// How many of the newest changes this page passed over.
    skip: usize,
}

#[derive(Deserialize)]
struct ChangesQuery {
    count: Option<usize>,
    /// How many of the newest changes to pass over, for the page after the
    /// ones already listed.
    #[serde(default)]
    skip: Option<usize>,
}

impl ChangesQuery {
    /// The history request this query reads: at most one page, from where
    /// the page asked.
    fn log_request(&self) -> super::log::LogRequest {
        super::log::LogRequest {
            count: self.count.unwrap_or(CHANGES_DEFAULT).clamp(1, CHANGES_MAX),
            skip: self.skip.unwrap_or(0),
        }
    }
}

#[derive(Deserialize)]
struct CountsQuery {
    /// Whole change ids, separated by commas.
    #[serde(default)]
    changes: String,
}

impl CountsQuery {
    /// The change ids asked about, or why the request is refused before the
    /// daemon is asked anything.
    fn changes(&self) -> std::result::Result<Vec<String>, String> {
        let changes: Vec<String> = self
            .changes
            .split(',')
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .collect();
        if changes.len() > super::review_guide::COUNTS_ASKED_MAX {
            return Err(format!(
                "{} changes were asked about at once; ask about at most {}",
                changes.len(),
                super::review_guide::COUNTS_ASKED_MAX
            ));
        }
        if let Some(short) = changes.iter().find(|id| !is_change_id(id)) {
            return Err(format!("'{short}' is not a full change id"));
        }
        Ok(changes)
    }
}

#[derive(Deserialize)]
struct GuideQuery {
    change: Option<String>,
    /// Present, with any value, to walk the working tree.
    working: Option<String>,
    base: Option<String>,
}

#[derive(Deserialize)]
struct SourceQuery {
    entity: String,
    change: String,
}

#[derive(Deserialize)]
struct HistoryQuery {
    entity: String,
    /// The change the guide being read ends at.
    #[serde(default)]
    at: Option<String>,
    /// A range's base, to mark which revisions belong to the range.
    #[serde(default)]
    since: Option<String>,
}

#[derive(Serialize)]
struct BranchJson {
    name: String,
    active: bool,
    default: bool,
}

/// A refusal the page shows as it was given. The text is the daemon's or the
/// CLI's own sentence, so the page never invents a cause.
pub(super) fn refusal(status: StatusCode, error: &anyhow::Error) -> Response {
    let mut response = Json(serde_json::json!({ "error": format!("{error:#}") })).into_response();
    *response.status_mut() = status;
    response
}

fn static_response(body: &'static str, content_type: &'static str) -> Response {
    let mut response = Response::new(body.into());
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
}

async fn serve_index(State(state): State<Arc<UiState>>) -> Response {
    let assets = if state.base_path.is_empty() {
        String::new()
    } else {
        format!("{}/review", state.base_path)
    };
    let workspace = if state.base_path.is_empty() {
        String::new()
    } else {
        format!("{}/workspace/", state.base_path)
    };
    let html = INDEX_HTML
        .replace("=\"/", &format!("=\"{assets}/"))
        .replace(
            "<body>",
            &format!(
                "<body data-api-base=\"{}\" data-workspace-url=\"{workspace}\">",
                state.base_path
            ),
        );
    axum::response::Html(html).into_response()
}

async fn serve_app_js() -> Response {
    static_response(APP_JS, "application/javascript; charset=utf-8")
}

async fn serve_style_css() -> Response {
    static_response(STYLE_CSS, "text/css; charset=utf-8")
}

async fn serve_diff_js() -> Response {
    static_response(DIFF_JS, "application/javascript; charset=utf-8")
}

async fn serve_map_js() -> Response {
    static_response(MAP_JS, "application/javascript; charset=utf-8")
}

async fn serve_highlight_js() -> Response {
    static_response(HIGHLIGHT_JS, "application/javascript; charset=utf-8")
}

async fn serve_mark() -> Response {
    static_response(MARK_SVG, "image/svg+xml")
}

async fn serve_session(State(state): State<Arc<UiState>>) -> Response {
    let (_, origin) = super::workspace_ui::display_identity(&state.layout);
    let (owner, host) = origin.map_or((None, None), |origin| (origin.owner, Some(origin.host)));
    Json(SessionJson {
        owner,
        host,
        repository_key: super::workspace_ui::repository_key(state.layout.working_dir()),
        repository: &state.repository,
        root: &state.root,
        editor: editor_hint(),
        initial_change: state.initial.change.as_deref(),
        initial_working: state.initial.working,
        initial_base: state.initial.base.as_deref(),
        version: env!("CARGO_PKG_VERSION"),
        actor: &state.actor,
        token: &state.token,
    })
    .into_response()
}

/// The editor to offer first, read from the shell `kin review open` runs in.
///
/// Only a hint. The page lets its reader pick another or none, and keeps
/// that choice in the browser.
fn editor_hint() -> Option<&'static str> {
    let named: Vec<String> = ["VISUAL", "EDITOR"]
        .iter()
        .filter_map(|key| std::env::var(key).ok())
        .collect();
    // `code --wait` and `/usr/local/bin/code` both name `code`.
    let names = |program: &str| {
        named.iter().any(|value| {
            value
                .split_whitespace()
                .next()
                .and_then(|first| first.rsplit('/').next())
                .is_some_and(|name| name.eq_ignore_ascii_case(program))
        })
    };
    if std::env::var_os("CURSOR_TRACE_ID").is_some() || names("cursor") {
        Some("cursor")
    } else if std::env::var_os("ZED_TERM").is_some() || names("zed") {
        Some("zed")
    } else if std::env::var("TERM_PROGRAM").is_ok_and(|program| program == "vscode")
        || names("code")
    {
        Some("vscode")
    } else {
        None
    }
}

/// The daemon is resolved per request, not held. A page left open across a
/// daemon restart keeps working, and one whose daemon is gone says so in the
/// daemon client's own words.
pub(super) async fn daemon(state: &UiState) -> Result<crate::daemon_client::DaemonClient> {
    crate::daemon_client::DaemonClient::connect_for_workspace("local UI", &state.layout).await
}

async fn serve_changes(
    State(state): State<Arc<UiState>>,
    Query(query): Query<ChangesQuery>,
) -> Response {
    let request = query.log_request();
    let (requested, skip) = (request.count, request.skip);
    let answer = async {
        let client = daemon(&state).await?;
        let response = client.log(&request).await?;
        response
            .report
            .context("the daemon answered the history request without a report")
    }
    .await;
    match answer {
        Ok(report) => Json(ChangesJson {
            truncated: report.truncated,
            requested,
            skip,
            changes: report
                .entries
                .into_iter()
                .map(|entry| ChangeRowJson {
                    kin_checkpoint: is_kin_checkpoint(&entry),
                    id: entry.change_id.to_string(),
                    title: entry
                        .message
                        .lines()
                        .next()
                        .unwrap_or_default()
                        .trim()
                        .to_string(),
                    author: author_display(&entry.author.to_string()),
                    agent: super::workspace_agents::agent_ref(&entry.author.to_string()),
                    timestamp: entry.timestamp.to_string(),
                    origin: match entry.origin {
                        kin_model::ChangeOrigin::Native => "native",
                        kin_model::ChangeOrigin::GitCommit { .. } => "git_commit",
                    },
                    parents: entry.parents.len(),
                    entity_deltas: entry.entity_delta_count,
                    entity_deltas_unchanged: entry.entity_deltas_unchanged,
                    relation_deltas: entry.relation_delta_count,
                })
                .collect(),
        })
        .into_response(),
        Err(error) => refusal(StatusCode::BAD_GATEWAY, &error),
    }
}

async fn serve_guide(
    State(state): State<Arc<UiState>>,
    Query(query): Query<GuideQuery>,
) -> Response {
    let named = |value: Option<String>| value.filter(|value| !value.trim().is_empty());
    let request = ReviewRequest::Guide {
        change: named(query.change),
        json: true,
        working: query.working.is_some(),
        base: named(query.base),
    };
    let working = query.working.is_some();
    let answer = async {
        // Validate this URL's repository and drop ambient session scope before
        // any admission. A daemon override must not receive a write first and
        // only then be discovered to serve a different workspace.
        let client = daemon(&state).await?;
        // Admitted first, as `kin diff` admits it: the page asks again as the
        // working tree changes, and an edit the watcher has not reached yet
        // must still be in the answer.
        let unmeasured = if working {
            admit_working_for_ui(&client).await
        } else {
            None
        };
        let mut response = client.review(&request).await?;
        if let Some(why) = unmeasured {
            super::review::mark_unmeasured(&mut response, &why)?;
        }
        response
            .json
            .context("the daemon answered the guide request without its JSON")
    }
    .await;
    match answer {
        Ok(json) => json_response(polish(json, polish_guide)),
        Err(error) => refusal(StatusCode::BAD_GATEWAY, &error),
    }
}

/// Admit the working copy before the page reads it. The admission names the
/// daemon as its actor, so a host with no Git identity still gets a measured
/// working copy; see [`super::admit::AdmitRequest::new`].
async fn admit_working_for_ui(client: &crate::daemon_client::DaemonClient) -> Option<String> {
    working_admission_reason(client.admit(&super::admit::AdmitRequest::new()).await)
}

fn working_admission_reason(dispatch: crate::daemon_client::AdmitDispatch) -> Option<String> {
    match dispatch {
        crate::daemon_client::AdmitDispatch::Answered(response) => {
            // HTTP success does not establish admission: policy refusals and
            // incomplete reports must keep the guide's unmeasured caveat.
            super::admit::admission_failure(&response)
        }
        crate::daemon_client::AdmitDispatch::Refused(error) => Some(format!(
            "this repository's daemon refused working-copy admission ({error:#})"
        )),
        crate::daemon_client::AdmitDispatch::Unanswered(error) => Some(format!(
            "working-copy admission did not answer ({error:#}); whether it ran is unknown and this request was not retried"
        )),
    }
}

/// The name a person is shown as, from an author as history recorded it.
///
/// A change imported from Git records the commit's whole signature line,
/// `Name <email> 1749933342 -0700`, and one whose bytes were not UTF-8
/// records them as `git-author-v1:` and hex. A page shows the name alone:
/// the address is not the reader's to see, and the time is the change's own
/// field. A native author, an actor label, is shown as recorded.
pub(super) fn author_display(raw: &str) -> String {
    if let Some(agent) = super::workspace_agents::agent_author(raw) {
        return agent.name;
    }
    let decoded = raw
        .strip_prefix("git-author-v1:")
        .and_then(|hex_text| hex::decode(hex_text.trim()).ok())
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
    let text = decoded.as_deref().unwrap_or(raw).trim();
    match text.find('<') {
        Some(open) if text[open..].contains('>') => {
            let name = text[..open].trim();
            if name.is_empty() {
                "Unnamed author".to_string()
            } else {
                name.to_string()
            }
        }
        _ => text.to_string(),
    }
}

/// How many places, in a sentence: `1 place that relies on it was` or
/// `3 places that rely on it were`.
fn places_that_use_it(count: usize) -> String {
    if count == 1 {
        "1 place that relies on it was".to_string()
    } else {
        format!("{count} places that rely on it were")
    }
}

fn recorded_entities(count: usize) -> String {
    if count == 1 {
        "1 entity".to_string()
    } else {
        format!("{count} entities")
    }
}

/// The count a recorded message carries between `before` and `after`.
fn count_between(message: &str, before: &str, after: &str) -> Option<usize> {
    message
        .strip_prefix(before)?
        .split_once(after)
        .and_then(|(count, _)| count.trim().parse().ok())
}

/// A finding's message in the words a reader of this page uses, when the
/// review recorded it in the words of its own analysis.
///
/// The counts and names come from the recorded message itself, so the
/// sentence says no more than the review found. A message this does not
/// recognize is shown as recorded.
pub(super) fn plain_finding(message: &str) -> Option<String> {
    for (before, after) in [("Modified entity `", "."), ("New public entity `", " yet.")] {
        if let Some(name) = message
            .strip_prefix(before)
            .and_then(|rest| rest.strip_suffix("` has no test coverage"))
        {
            return Some(format!("No test reaches `{name}`{after}"));
        }
    }
    const MIGRATED: &str =
        " graph-known consumer(s) were co-updated in this diff (migration verified in-range)";
    if let Some(count) = count_between(message, "Signature changed; all ", MIGRATED) {
        return Some(format!(
            "Its signature changed. {}{} updated in this change.",
            if count == 1 { "The " } else { "All " },
            places_that_use_it(count)
        ));
    }
    if let Some(count) = count_between(message, "Visibility reduced; all ", MIGRATED) {
        return Some(format!(
            "It was made less visible. {}{} updated in this change.",
            if count == 1 { "The " } else { "All " },
            places_that_use_it(count)
        ));
    }
    for (before, change) in [
        (
            "Breaking change: signature modification affects ",
            "the signature of",
        ),
        (
            "Breaking change: visibility reduced with ",
            "the visibility of",
        ),
    ] {
        if let Some(rest) = message.strip_prefix(before) {
            let (count, name) = rest.split_once(" direct external consumer(s) of ")?;
            let count: usize = count.trim().parse().ok()?;
            return Some(format!(
                "Breaking change: {change} {} changed, and {} not updated in this change.",
                name.trim(),
                places_that_use_it(count)
            ));
        }
    }
    if let Some(rest) = message.strip_prefix("Behavior of ") {
        let (name, rest) = rest.split_once(" changed with ")?;
        let count = rest.strip_suffix(" distinct non-test consumer entities")?;
        let count: usize = count.trim().parse().ok()?;
        return Some(format!(
            "The behavior of {name} changed, and {} outside tests {} it.",
            recorded_entities(count),
            if count == 1 { "uses" } else { "use" }
        ));
    }
    if let Some(rest) = message.strip_prefix("Behavior-preserving change to ") {
        let (name, rest) = rest.split_once(" reaches ")?;
        let (count, _) = rest.split_once(" distinct non-test consumer entities;")?;
        let count: usize = count.trim().parse().ok()?;
        return Some(format!(
            "{name} changed only in its docs, comments or formatting, so it behaves the same for the {} outside tests that {} it.",
            recorded_entities(count),
            if count == 1 { "uses" } else { "use" }
        ));
    }
    const RENAME: &str = " (parameter rename; all ";
    if message.starts_with("Signature changed: `") && message.ends_with("no runtime break)") {
        let (signatures, rest) = message.rsplit_once(RENAME)?;
        let count: usize = rest.split_once(' ')?.0.parse().ok()?;
        return Some(format!(
            "{signatures}. Only parameter names changed, and {} pass arguments by position, so none of them break.",
            if count == 1 {
                "the 1 call site".to_string()
            } else {
                format!("the {count} call sites")
            }
        ));
    }
    None
}

/// The guide as the page reads it: authors by name, and findings in the
/// page's words. The JSON is otherwise the daemon's own.
fn polish_guide(guide: &mut serde_json::Value) {
    if let Some(author) = guide["change"]["author"].as_str() {
        guide["change"]["author"] = author_display(author).into();
    }
    super::workspace_agents::attribute_change(&mut guide["change"]);
    let rewrite = |message: &mut serde_json::Value| {
        if let Some(plain) = message.as_str().and_then(plain_finding) {
            *message = plain.into();
        }
    };
    if let Some(chapters) = guide["chapters"].as_array_mut() {
        for step in chapters
            .iter_mut()
            .filter_map(|chapter| chapter["steps"].as_array_mut())
            .flatten()
        {
            if let Some(findings) = step["findings"].as_array_mut() {
                for finding in findings {
                    rewrite(&mut finding["message"]);
                }
            }
        }
    }
    if let Some(lists) = guide["findings"].as_object_mut() {
        for list in lists.values_mut().filter_map(|list| list.as_array_mut()) {
            list.iter_mut().for_each(rewrite);
        }
    }
}

fn polish_history(history: &mut serde_json::Value) {
    if let Some(revisions) = history["revisions"].as_array_mut() {
        for revision in revisions {
            if let Some(author) = revision["author"].as_str() {
                revision["author"] = author_display(author).into();
            }
        }
    }
}

/// Apply `edit` to an answer the daemon serialized. An answer that does not
/// read as JSON is passed through as it was answered.
fn polish(json: String, edit: fn(&mut serde_json::Value)) -> String {
    match serde_json::from_str::<serde_json::Value>(&json) {
        Ok(mut value) => {
            edit(&mut value);
            serde_json::to_string(&value).unwrap_or(json)
        }
        Err(_) => json,
    }
}

/// JSON the daemon already serialized, passed through as it was answered.
fn json_response(json: String) -> Response {
    let mut response = Response::new(json.into());
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

async fn serve_source(
    State(state): State<Arc<UiState>>,
    Query(query): Query<SourceQuery>,
) -> Response {
    let answer = async {
        let client = daemon(&state).await?;
        let response = client
            .review(&ReviewRequest::GuideSource {
                entity: query.entity,
                change: query.change,
            })
            .await?;
        response
            .json
            .context("the daemon answered the source request without its JSON")
    }
    .await;
    match answer {
        Ok(json) => json_response(json),
        Err(error) => refusal(StatusCode::BAD_GATEWAY, &error),
    }
}

/// The changes that changed one entity, for a step's history.
async fn serve_history(
    State(state): State<Arc<UiState>>,
    Query(query): Query<HistoryQuery>,
) -> Response {
    let answer = async {
        let client = daemon(&state).await?;
        client
            .review(&ReviewRequest::GuideHistory {
                entity: query.entity,
                at: query.at,
                since: query.since,
            })
            .await?
            .json
            .context("the daemon answered the history request without its JSON")
    }
    .await;
    match answer {
        Ok(json) => json_response(polish(json, polish_history)),
        Err(error) => refusal(StatusCode::BAD_GATEWAY, &error),
    }
}

/// The repository's branches by the names a range is asked with, so the page
/// can offer a branch against the default one without the reader typing it.
async fn serve_branches(State(state): State<Arc<UiState>>) -> Response {
    let answer = async {
        let client = daemon(&state).await?;
        let report = client
            .branch(&crate::commands::branch::BranchRequest::List)
            .await?
            .report
            .context("the daemon answered the branch list without its report")?;
        anyhow::Ok(
            report
                .branches
                .into_iter()
                .map(|branch| {
                    let full = branch.name.to_string();
                    BranchJson {
                        name: full
                            .strip_prefix("refs/heads/")
                            .unwrap_or(&full)
                            .to_string(),
                        active: branch.active,
                        default: branch.default,
                    }
                })
                .collect::<Vec<_>>(),
        )
    }
    .await;
    match answer {
        Ok(branches) => Json(serde_json::json!({ "branches": branches })).into_response(),
        Err(error) => refusal(StatusCode::BAD_GATEWAY, &error),
    }
}

/// The stored reviews of `change`, as the daemon answers them.
async fn reviews_json(
    client: &crate::daemon_client::DaemonClient,
    change: &str,
    range_base: Option<&str>,
) -> Result<String> {
    client
        .review(&ReviewRequest::GuideReviews {
            change: change.to_string(),
            range_base: range_base.map(str::to_string),
        })
        .await?
        .json
        .context("the daemon answered the reviews request without its JSON")
}

async fn serve_reviews(
    State(state): State<Arc<UiState>>,
    Query(query): Query<ReviewsQuery>,
) -> Response {
    let answer = async {
        let client = daemon(&state).await?;
        reviews_json(&client, &query.change, query.range_base.as_deref()).await
    }
    .await;
    match answer {
        Ok(json) => json_response(json),
        Err(error) => refusal(StatusCode::BAD_GATEWAY, &error),
    }
}

/// How many entities the guide of each asked change walks, for the list of
/// changes, so a change reads the same in the list and in its walk.
async fn serve_counts(
    State(state): State<Arc<UiState>>,
    Query(query): Query<CountsQuery>,
) -> Response {
    let changes = match query.changes() {
        Ok(changes) => changes,
        Err(reason) => return bad_request(&reason),
    };
    if changes.is_empty() {
        return Json(super::review_guide::GuideCounts::default()).into_response();
    }
    let answer = async {
        let client = daemon(&state).await?;
        client
            .review(&ReviewRequest::GuideCounts { changes })
            .await?
            .json
            .context("the daemon answered the count request without its JSON")
    }
    .await;
    match answer {
        Ok(json) => json_response(json),
        Err(error) => refusal(StatusCode::BAD_GATEWAY, &error),
    }
}

/// Where each reviewed change stands, for the list of changes.
async fn serve_standing(State(state): State<Arc<UiState>>) -> Response {
    let answer = async {
        let client = daemon(&state).await?;
        client
            .review(&ReviewRequest::GuideStanding {})
            .await?
            .json
            .context("the daemon answered the standing request without its JSON")
    }
    .await;
    match answer {
        Ok(json) => json_response(json),
        Err(error) => refusal(StatusCode::BAD_GATEWAY, &error),
    }
}

/// The review a write from the page lands on.
///
/// The newest stored review between the same two changes when there is one,
/// so a decision made here joins the history a terminal started. Otherwise a review of the
/// change is opened first, against the base the guide read it from, and the
/// write lands on that.
async fn review_for(
    client: &crate::daemon_client::DaemonClient,
    state: &UiState,
    target: &ReviewTarget,
) -> Result<String> {
    let _one_at_a_time = state.opening_review.lock().await;
    let stored = |json: &str| -> Result<Option<String>> {
        let reviews: ReviewsJson =
            serde_json::from_str(json).context("parse the daemon's reviews answer")?;
        Ok(reviews
            .reviews
            .into_iter()
            .next()
            .map(|review| review.review_id))
    };
    let written_from = Some(target.write_base());
    if let Some(review_id) = stored(&reviews_json(client, &target.change, written_from).await?)? {
        return Ok(review_id);
    }
    let title = target
        .title
        .as_deref()
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map_or_else(
            || format!("Change {}", &target.change[..12]),
            |title| title.chars().take(TITLE_MAX_CHARS).collect(),
        );
    client
        .review(&ReviewRequest::Create {
            title,
            // Both ends are written in the explicit change form, which the
            // ref grammar reads as a change and never as a branch of the same
            // spelling. A change with no parent has no base to name, so its
            // review is opened from the change to itself.
            base: format!("change:{}", target.write_base()),
            head: format!("change:{}", target.change),
            description: None,
            actor: Some(state.actor.clone()),
        })
        .await?;
    stored(&reviews_json(client, &target.change, written_from).await?)?
        .context("the review was opened and does not read back for this change")
}

/// The answer to a write the daemon has committed.
///
/// The write and the reread after it are two requests, and only the first one
/// decides whether anything was recorded. A reread that fails, because the
/// daemon restarted in between or for any other reason, leaves the record in
/// place, so the answer still says recorded and names the reread as what
/// failed. Answering that as a failed write would invite a retry, and the
/// retry would record the same decision twice.
fn recorded(reread: Result<String>) -> Response {
    let answer = reread
        .and_then(|json| {
            serde_json::from_str::<serde_json::Value>(&json)
                .context("parse the daemon's reviews answer")
        })
        .map(|mut answer| {
            answer["recorded"] = true.into();
            answer
        })
        .unwrap_or_else(
            |error| serde_json::json!({ "recorded": true, "reread_failed": format!("{error:#}") }),
        );
    Json(answer).into_response()
}

/// A full change id, the only form the page writes against.
fn is_change_id(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Why a write's target is refused before anything is asked of the daemon.
fn target_refusal(target: &ReviewTarget) -> Option<&'static str> {
    if !is_change_id(&target.change) {
        return Some("a decision or note is recorded against a full change id");
    }
    if target
        .base
        .as_deref()
        .is_some_and(|base| !is_change_id(base))
    {
        return Some("the base of a review is a full change id");
    }
    None
}

pub(super) fn bad_request(message: &str) -> Response {
    let mut response = Json(serde_json::json!({ "error": message })).into_response();
    *response.status_mut() = StatusCode::BAD_REQUEST;
    response
}

async fn serve_decide(
    State(state): State<Arc<UiState>>,
    headers: HeaderMap,
    Json(body): Json<DecideBody>,
) -> Response {
    if !may_write(&headers, &state) {
        return write_refused();
    }
    if let Some(reason) = target_refusal(&body.target) {
        return bad_request(reason);
    }
    if !matches!(body.state.as_str(), "approved" | "needs_work" | "blocked") {
        return bad_request("a decision is approved, needs_work or blocked");
    }
    let comment = body
        .comment
        .map(|comment| comment.trim().to_string())
        .filter(|comment| !comment.is_empty());
    if comment
        .as_ref()
        .is_some_and(|comment| comment.chars().count() > TEXT_MAX_CHARS)
    {
        return bad_request("a comment is at most 10,000 characters");
    }
    let written = async {
        let client = daemon(&state).await?;
        let review_id = review_for(&client, &state, &body.target).await?;
        client
            .review(&ReviewRequest::Decide {
                review_id,
                state: body.state.clone(),
                comment,
                actor: Some(state.actor.clone()),
            })
            .await?;
        Ok::<_, anyhow::Error>(client)
    }
    .await;
    match written {
        Ok(client) => {
            recorded(reviews_json(&client, &body.target.change, body.target.range_base()).await)
        }
        Err(error) => refusal(StatusCode::BAD_GATEWAY, &error),
    }
}

async fn serve_note(
    State(state): State<Arc<UiState>>,
    headers: HeaderMap,
    Json(body): Json<NoteBody>,
) -> Response {
    if !may_write(&headers, &state) {
        return write_refused();
    }
    if let Some(reason) = target_refusal(&body.target) {
        return bad_request(reason);
    }
    let text = body.body.trim().to_string();
    if text.is_empty() {
        return bad_request("a note needs some text");
    }
    if text.chars().count() > TEXT_MAX_CHARS {
        return bad_request("a note is at most 10,000 characters");
    }
    let scope = match body.entity.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(entity) => match uuid::Uuid::parse_str(entity) {
            Ok(id) => Some(format!("entity:{id}")),
            Err(_) => return bad_request("a note is scoped to an entity id"),
        },
    };
    let written = async {
        let client = daemon(&state).await?;
        let review_id = review_for(&client, &state, &body.target).await?;
        client
            .review(&ReviewRequest::Note {
                review_id,
                body: text,
                scope,
                actor: Some(state.actor.clone()),
            })
            .await?;
        Ok::<_, anyhow::Error>(client)
    }
    .await;
    match written {
        Ok(client) => {
            recorded(reviews_json(&client, &body.target.change, body.target.range_base()).await)
        }
        Err(error) => refusal(StatusCode::BAD_GATEWAY, &error),
    }
}

/// Whether `origin` is this server's own.
fn is_own_origin(origin: &str, port: u16) -> bool {
    origin
        .strip_prefix("http://")
        .is_some_and(|host| is_own_host(host, port))
}

/// Whether two byte strings are equal, without stopping at the first
/// difference, so how long a wrong token survives says nothing about it.
fn same_bytes(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .fold(0u8, |difference, (a, b)| difference | (a ^ b))
            == 0
}

/// Whether a request may write: it carries this run's token, and its origin is
/// this server's own whenever the browser names one.
pub(super) fn may_write(headers: &HeaderMap, state: &UiState) -> bool {
    let has_token = headers
        .get(WRITE_TOKEN_HEADER)
        .is_some_and(|sent| same_bytes(sent.as_bytes(), state.token.as_bytes()));
    let own_origin = match headers.get(header::ORIGIN) {
        None => true,
        Some(origin) => origin
            .to_str()
            .is_ok_and(|origin| is_own_origin(origin, state.port)),
    };
    has_token && own_origin
}

pub(super) fn write_refused() -> Response {
    (
        StatusCode::FORBIDDEN,
        "this page records a decision or a note only when its own page asks\n",
    )
        .into_response()
}

/// Whether `host` is this server's own loopback address.
fn is_own_host(host: &str, port: u16) -> bool {
    [
        format!("127.0.0.1:{port}"),
        format!("localhost:{port}"),
        format!("[::1]:{port}"),
    ]
    .iter()
    .any(|own| host.eq_ignore_ascii_case(own))
}

pub(super) async fn local_only(
    State(state): State<Arc<UiState>>,
    request: Request,
    next: Next,
) -> Response {
    let own = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|host| is_own_host(host, state.port));
    if !own {
        return (
            StatusCode::FORBIDDEN,
            "this page is served to its own loopback address only\n",
        )
            .into_response();
    }
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CONTENT_SECURITY_POLICY),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// Bind the page's port on loopback.
///
/// A port the caller named is theirs or the command fails, because a script
/// that asked for one port must not be handed another. The default walks
/// forward and then takes any free port, since a second repository's page or a
/// stray listener on the default is no reason to refuse to show a review.
pub(super) async fn bind(port: Option<u16>) -> Result<TcpListener> {
    let loopback = |port| SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    if let Some(port) = port {
        return TcpListener::bind(loopback(port))
            .await
            .with_context(|| format!("failed to bind 127.0.0.1:{port}"));
    }
    for candidate in DEFAULT_PORT..DEFAULT_PORT + PORT_ATTEMPTS {
        if let Ok(listener) = TcpListener::bind(loopback(candidate)).await {
            return Ok(listener);
        }
    }
    TcpListener::bind(loopback(0))
        .await
        .context("failed to bind any port on 127.0.0.1")
}

fn reader_routes() -> Router<Arc<UiState>> {
    Router::new()
        .route("/", get(serve_index))
        .route("/app.js", get(serve_app_js))
        .route("/style.css", get(serve_style_css))
        .route("/mark.svg", get(serve_mark))
        .route("/diff.js", get(serve_diff_js))
        .route("/map.js", get(serve_map_js))
        .route("/highlight.js", get(serve_highlight_js))
}

fn api_routes() -> Router<Arc<UiState>> {
    Router::new()
        .route("/api/session", get(serve_session))
        .route("/api/changes", get(serve_changes))
        .route("/api/guide", get(serve_guide))
        .route("/api/source", get(serve_source))
        .route("/api/reviews", get(serve_reviews))
        .route("/api/standing", get(serve_standing))
        .route("/api/counts", get(serve_counts))
        .route("/api/history", get(serve_history))
        .route("/api/branches", get(serve_branches))
        .route("/api/decide", post(serve_decide))
        .route("/api/note", post(serve_note))
        .route(
            "/api/change-agent",
            get(super::workspace_agents::serve_change_agent),
        )
}

fn router(state: Arc<UiState>) -> Router {
    reader_routes()
        .merge(api_routes())
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            local_only,
        ))
        .with_state(state)
}

pub(super) fn workspace_router(state: Arc<UiState>) -> Router {
    Router::new()
        .nest("/review/", reader_routes())
        .merge(api_routes())
        .merge(super::workspace_ui::routes())
        .route(
            "/api/workspace/graph",
            get(super::workspace_graph::serve_graph),
        )
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            local_only,
        ))
        .with_state(state)
}

/// `kin review open`: serve the guided review and, unless told not to, open it.
///
/// The daemon is reached before the port is bound, so a repository whose
/// daemon cannot answer refuses here with the daemon's own remedy instead of
/// opening a page that can only show an error.
pub async fn run(target: GuideTarget, port: Option<u16>, open_browser: bool) -> Result<()> {
    let layout = crate::commands::require_repository_layout()?;
    crate::daemon_client::DaemonClient::connect_for_command("review open", &layout).await?;

    let listener = bind(port).await?;
    let port = listener
        .local_addr()
        .context("failed to read the bound address")?
        .port();
    let repository = layout
        .working_dir()
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repository".to_string());
    let root = layout.working_dir().to_string_lossy().into_owned();
    let actor = page_actor(&layout);
    let state = Arc::new(UiState {
        layout,
        repository,
        root,
        initial: target,
        port,
        token: uuid::Uuid::new_v4().simple().to_string(),
        actor,
        opening_review: tokio::sync::Mutex::new(()),
        base_path: String::new(),
        catalog: None,
        agents: Default::default(),
    });

    let url = format!("http://127.0.0.1:{port}/");
    println!("Guided review at {url}");
    println!("Served from this machine only. Press Ctrl-C to stop.");
    if open_browser {
        let url_for_open = url.clone();
        tokio::task::spawn_blocking(move || {
            if let Err(error) = open::that(&url_for_open) {
                // The launcher's own error is for a log, not for a person.
                tracing::debug!(%error, "opening a browser failed");
                eprintln!("No browser opened here. Open {url_for_open} in your browser.");
            }
        });
    }

    axum::serve(listener, router(state))
        .await
        .context("kin review open server error")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serve the page on a free loopback port, the way `run` does, without a
    /// repository behind it. The routes under test never reach the daemon.
    async fn serve(initial: GuideTarget) -> u16 {
        let listener = bind(Some(0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = Arc::new(UiState {
            layout: kin_core::KinLayout::new(std::env::temp_dir().join("kin-review-ui-test")),
            repository: "fixture".to_string(),
            root: "/work/fixture".to_string(),
            initial,
            port,
            token: TEST_TOKEN.to_string(),
            actor: "ada".to_string(),
            opening_review: tokio::sync::Mutex::new(()),
            base_path: String::new(),
            catalog: None,
            agents: Default::default(),
        });
        tokio::spawn(async move {
            axum::serve(listener, router(state)).await.unwrap();
        });
        port
    }

    const TEST_TOKEN: &str = "0123456789abcdef0123456789abcdef";

    fn admission_response(admitted: bool) -> super::super::admit::AdmitResponse {
        use super::super::admit::{AdmitReport, AdmitResponse, ADMIT_SCHEMA};
        AdmitResponse {
            lines: Vec::new(),
            mutated: false,
            report: Some(AdmitReport {
                schema: ADMIT_SCHEMA.to_string(),
                repository_id: kin_model::RepositoryId::new("review-ui-fixture").unwrap(),
                tracked_before: 0,
                tracked_after: 0,
                entities_before: 0,
                entities_after: 0,
                embeddings_indexed: 0,
                embeddings_total: 0,
                reconcile: super::super::resources::ReconcileHealth::default(),
                tree_moved: Some(false),
                repository_authority_moved: Some(false),
                prior_admission_at: None,
                admitted,
                failure: (!admitted).then(|| "admission policy refused this pass".to_string()),
            }),
        }
    }

    #[test]
    fn working_admission_needs_a_successful_report_and_preserves_policy_refusals() {
        use crate::daemon_client::AdmitDispatch;
        assert!(
            working_admission_reason(AdmitDispatch::Answered(admission_response(true))).is_none()
        );
        assert!(
            working_admission_reason(AdmitDispatch::Answered(admission_response(false)))
                .unwrap()
                .contains("admission policy refused")
        );

        let mut missing = admission_response(true);
        missing.report = None;
        assert!(working_admission_reason(AdmitDispatch::Answered(missing))
            .unwrap()
            .contains("unknown"));

        let mut inconsistent = admission_response(true);
        inconsistent.lines.push(format!(
            "{}policy held the pass",
            super::super::admit::ADMIT_FAILURE_PREFIX
        ));
        assert_eq!(
            working_admission_reason(AdmitDispatch::Answered(inconsistent)).as_deref(),
            Some("policy held the pass")
        );
    }

    #[test]
    fn working_admission_refusals_and_unknown_outcomes_remain_distinct() {
        use crate::daemon_client::AdmitDispatch;
        let refused = working_admission_reason(AdmitDispatch::Refused(anyhow::anyhow!(
            "read-only authority"
        )))
        .unwrap();
        assert!(refused.contains("refused"));
        assert!(refused.contains("read-only authority"));
        let unknown = working_admission_reason(AdmitDispatch::Unanswered(anyhow::anyhow!(
            "response was lost"
        )))
        .unwrap();
        assert!(unknown.contains("unknown"));
        assert!(unknown.contains("not retried"));
        assert!(unknown.contains("response was lost"));
    }

    /// POST a write the way the page does, with whatever token and origin the
    /// case under test sends.
    async fn post_as(
        port: u16,
        path: &str,
        token: Option<&str>,
        origin: Option<&str>,
        body: serde_json::Value,
    ) -> reqwest::Response {
        let mut request = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}{path}"))
            .json(&body);
        if let Some(token) = token {
            request = request.header(WRITE_TOKEN_HEADER, token);
        }
        if let Some(origin) = origin {
            request = request.header(header::ORIGIN, origin);
        }
        request.send().await.unwrap()
    }

    async fn get_as(port: u16, path: &str, host: &str) -> reqwest::Response {
        reqwest::Client::new()
            .get(format!("http://127.0.0.1:{port}{path}"))
            .header(header::HOST, host)
            .send()
            .await
            .unwrap()
    }

    #[test]
    fn only_this_servers_loopback_names_are_its_own_host() {
        assert!(is_own_host("127.0.0.1:4221", 4221));
        assert!(is_own_host("localhost:4221", 4221));
        assert!(is_own_host("LOCALHOST:4221", 4221));
        assert!(!is_own_host("127.0.0.1:4222", 4221));
        assert!(!is_own_host("127.0.0.1", 4221));
        assert!(!is_own_host("rebound.example:4221", 4221));
    }

    #[tokio::test]
    async fn a_request_for_another_host_is_refused_before_any_route_runs() {
        let port = serve(GuideTarget::default()).await;
        for path in ["/", "/api/session", "/api/guide"] {
            let refused = get_as(port, path, &format!("rebound.example:{port}")).await;
            assert_eq!(refused.status(), StatusCode::FORBIDDEN, "{path}");
            assert!(refused
                .headers()
                .get(header::CONTENT_SECURITY_POLICY)
                .is_none());
        }
    }

    #[tokio::test]
    async fn the_page_is_served_whole_from_this_binary_and_loads_nothing_else() {
        let port = serve(GuideTarget::default()).await;
        let own = format!("127.0.0.1:{port}");
        let page = get_as(port, "/", &own).await;
        assert_eq!(page.status(), StatusCode::OK);
        assert_eq!(
            page.headers().get(header::CONTENT_SECURITY_POLICY).unwrap(),
            CONTENT_SECURITY_POLICY
        );
        assert_eq!(
            page.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-store"
        );
        // The policy forbids every other origin, so the page must not ask for
        // one either, or it would render broken instead of whole.
        for asset in [INDEX_HTML, APP_JS, STYLE_CSS, DIFF_JS, MAP_JS, HIGHLIGHT_JS] {
            assert!(
                !asset.contains("http://"),
                "the page names a network origin"
            );
            assert!(
                !asset.contains("https://"),
                "the page names a network origin"
            );
        }
        for path in [
            "/app.js",
            "/style.css",
            "/mark.svg",
            "/diff.js",
            "/map.js",
            "/highlight.js",
        ] {
            assert!(
                INDEX_HTML.contains(path),
                "{path} is not linked by the page"
            );
            let asset = get_as(port, path, &format!("localhost:{port}")).await;
            assert_eq!(asset.status(), StatusCode::OK, "{path}");
        }
    }

    #[tokio::test]
    async fn the_session_names_the_repository_and_the_change_asked_for() {
        let port = serve(GuideTarget {
            change: Some("abc123".to_string()),
            working: false,
            base: Some("main".to_string()),
        })
        .await;
        let session: serde_json::Value = get_as(port, "/api/session", &format!("127.0.0.1:{port}"))
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(session["repository"], "fixture");
        // The working tree's path, which the page turns a step's place into
        // an editor address with.
        assert_eq!(session["root"], "/work/fixture");
        assert_eq!(session["initial_change"], "abc123");
        assert_eq!(session["initial_base"], "main");
        assert!(session.get("initial_working").is_none());
        assert_eq!(session["actor"], "ada");
        assert_eq!(session["token"], TEST_TOKEN);
    }

    /// A write is refused unless this run's own page sent it, and the refusal
    /// comes before anything is asked of the repository.
    #[tokio::test]
    async fn a_write_needs_this_runs_token_and_its_own_origin() {
        let port = serve(GuideTarget::default()).await;
        let own = format!("http://127.0.0.1:{port}");
        let change = "a".repeat(64);
        let decision = serde_json::json!({ "change": change, "state": "approved" });
        let note = serde_json::json!({ "change": change, "body": "checked the panel" });

        for (path, body) in [("/api/decide", &decision), ("/api/note", &note)] {
            let no_token = post_as(port, path, None, Some(&own), body.clone()).await;
            assert_eq!(
                no_token.status(),
                StatusCode::FORBIDDEN,
                "{path} without a token"
            );
            let wrong = post_as(
                port,
                path,
                Some("0".repeat(32).as_str()),
                Some(&own),
                body.clone(),
            )
            .await;
            assert_eq!(
                wrong.status(),
                StatusCode::FORBIDDEN,
                "{path} with another token"
            );
            let foreign = post_as(
                port,
                path,
                Some(TEST_TOKEN),
                Some("http://rebound.example"),
                body.clone(),
            )
            .await;
            assert_eq!(
                foreign.status(),
                StatusCode::FORBIDDEN,
                "{path} from another origin"
            );
        }
    }

    /// A write its own page sends is still refused by shape before it reaches
    /// the repository: a short id, an unknown decision, an empty note.
    #[tokio::test]
    async fn a_malformed_write_is_refused_with_its_reason() {
        let port = serve(GuideTarget::default()).await;
        let own = format!("http://localhost:{port}");
        let change = "a".repeat(64);
        for (path, body, reason) in [
            (
                "/api/decide",
                serde_json::json!({ "change": "abc123", "state": "approved" }),
                "a decision or note is recorded against a full change id",
            ),
            (
                "/api/decide",
                serde_json::json!({ "change": change, "state": "maybe" }),
                "a decision is approved, needs_work or blocked",
            ),
            (
                "/api/note",
                serde_json::json!({ "change": change, "body": "   " }),
                "a note needs some text",
            ),
            (
                "/api/note",
                serde_json::json!({ "change": change, "body": "x", "entity": "listRun" }),
                "a note is scoped to an entity id",
            ),
            (
                "/api/note",
                serde_json::json!({ "change": change, "base": "main", "body": "x" }),
                "the base of a review is a full change id",
            ),
        ] {
            let refused = post_as(port, path, Some(TEST_TOKEN), Some(&own), body).await;
            assert_eq!(refused.status(), StatusCode::BAD_REQUEST, "{reason}");
            let answer: serde_json::Value = refused.json().await.unwrap();
            assert_eq!(answer["error"], reason);
        }
    }

    /// A write the daemon committed is answered as recorded even when the
    /// reread after it fails, so the page never invites a duplicate.
    #[tokio::test]
    async fn a_committed_write_is_recorded_whatever_the_reread_does() {
        let body = |response: Response| async move {
            assert_eq!(response.status(), StatusCode::OK);
            let bytes = axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap();
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()
        };
        let reread = body(recorded(Ok(r#"{"reviews":[]}"#.to_string()))).await;
        assert_eq!(reread["recorded"], true);
        assert!(reread["reviews"].as_array().is_some_and(Vec::is_empty));

        let stale = body(recorded(Err(anyhow::anyhow!("daemon restarted")))).await;
        assert_eq!(stale["recorded"], true);
        assert!(stale.get("reviews").is_none());
        assert_eq!(stale["reread_failed"], "daemon restarted");
    }

    #[test]
    fn a_token_is_compared_whole_and_an_origin_by_its_host() {
        assert!(same_bytes(b"token", b"token"));
        assert!(!same_bytes(b"token", b"tokem"));
        assert!(!same_bytes(b"token", b"toke"));
        assert!(is_own_origin("http://127.0.0.1:4221", 4221));
        assert!(is_own_origin("http://localhost:4221", 4221));
        assert!(!is_own_origin("https://127.0.0.1:4221", 4221));
        assert!(!is_own_origin("http://127.0.0.1:4222", 4221));
        assert!(!is_own_origin("null", 4221));
    }

    /// The list pages a history past one read: each page asks from where the
    /// last one ended, and no read answers more than one page.
    ///
    /// Breaking it: drop `skip` from the request and every page after the
    /// first repeats the newest changes.
    #[test]
    fn a_history_page_asks_from_where_the_list_ends() {
        let query = |count: Option<usize>, skip: Option<usize>| {
            let request = ChangesQuery { count, skip }.log_request();
            (request.count, request.skip)
        };
        assert_eq!(query(None, None), (CHANGES_DEFAULT, 0));
        assert_eq!(query(Some(480), Some(480)), (480, 480));
        assert_eq!(query(Some(9_000), Some(960)), (CHANGES_MAX, 960));
        assert_eq!(query(Some(0), None), (1, 0));
    }

    /// A count request is refused by its shape before the daemon is asked:
    /// a short id, or more changes than one page of history holds. An empty
    /// one is answered with no counts and asks nothing.
    #[tokio::test]
    async fn a_count_request_is_refused_by_shape_before_the_daemon_is_asked() {
        let port = serve(GuideTarget::default()).await;
        let own = format!("127.0.0.1:{port}");
        let short = get_as(port, "/api/counts?changes=abc123", &own).await;
        assert_eq!(short.status(), StatusCode::BAD_REQUEST);
        let answer: serde_json::Value = short.json().await.unwrap();
        assert_eq!(answer["error"], "'abc123' is not a full change id");

        let too_many = vec!["a".repeat(64); super::super::review_guide::COUNTS_ASKED_MAX + 1];
        let refused = get_as(
            port,
            &format!("/api/counts?changes={}", too_many.join(",")),
            &own,
        )
        .await;
        assert_eq!(refused.status(), StatusCode::BAD_REQUEST);

        let empty = get_as(port, "/api/counts?changes=", &own).await;
        assert_eq!(empty.status(), StatusCode::OK);
        let answer: serde_json::Value = empty.json().await.unwrap();
        assert_eq!(answer, serde_json::json!({ "counts": {} }));
    }

    /// History imported from Git records the whole signature line. The page
    /// names the person and never shows their address or the raw time.
    #[test]
    fn an_author_is_shown_by_name_alone() {
        assert_eq!(
            author_display("Grace Hopper <grace@example.com> 1749933342 -0700"),
            "Grace Hopper"
        );
        assert_eq!(author_display("Ada <ada@example.com>"), "Ada");
        assert_eq!(
            author_display("<anon@example.com> 1 +0000"),
            "Unnamed author"
        );
        assert_eq!(author_display("troyfortinjr"), "troyfortinjr");
        let undecodable = format!(
            "git-author-v1:{}",
            hex::encode(b"Ren\xe9 <r@example.com> 1 +0000")
        );
        assert_eq!(author_display(&undecodable), "Ren\u{fffd}");
    }

    /// The review's own vocabulary is rewritten from the counts and names
    /// its message carries, with no dash a reader of the page never sees.
    #[test]
    fn a_finding_reads_in_the_pages_words() {
        assert_eq!(
            plain_finding("Signature changed; all 3 graph-known consumer(s) were co-updated in this diff (migration verified in-range)").as_deref(),
            Some("Its signature changed. All 3 places that rely on it were updated in this change.")
        );
        assert_eq!(
            plain_finding("Visibility reduced; all 1 graph-known consumer(s) were co-updated in this diff (migration verified in-range)").as_deref(),
            Some("It was made less visible. The 1 place that relies on it was updated in this change.")
        );
        assert_eq!(
            plain_finding("Breaking change: signature modification affects 2 direct external consumer(s) of `Signer`").as_deref(),
            Some("Breaking change: the signature of `Signer` changed, and 2 places that rely on it were not updated in this change.")
        );
        let renamed = plain_finding("Signature changed: `def f(a)` \u{2192} `def f(b)` (parameter rename; all 4 graph-known call site(s) pass positionally \u{2014} no runtime break)").unwrap();
        assert_eq!(
            renamed,
            "Signature changed: `def f(a)` \u{2192} `def f(b)`. Only parameter names changed, and the 4 call sites pass arguments by position, so none of them break."
        );
        assert!(!renamed.contains('\u{2014}'));
        assert_eq!(
            plain_finding(
                "Behavior of `Signer` changed with 9 distinct non-test consumer entities"
            )
            .as_deref(),
            Some("The behavior of `Signer` changed, and 9 entities outside tests use it.")
        );
        assert_eq!(
            plain_finding("Behavior-preserving change to `Signer` reaches 1 distinct non-test consumer entities; the new body is provably equivalent to the old (docstring/comment/formatting), so this is informational").as_deref(),
            Some("`Signer` changed only in its docs, comments or formatting, so it behaves the same for the 1 entity outside tests that uses it.")
        );
        assert_eq!(
            plain_finding("Modified entity `signer` has no test coverage").as_deref(),
            Some("No test reaches `signer`.")
        );
        assert_eq!(
            plain_finding("New public entity `Signer.rotate` has no test coverage").as_deref(),
            Some("No test reaches `Signer.rotate` yet.")
        );
        assert_eq!(plain_finding("Signature changed; all many graph-known consumer(s) were co-updated in this diff (migration verified in-range)"), None);
    }

    #[test]
    fn a_guide_is_passed_on_with_authors_by_name_and_findings_in_plain_words() {
        let guide = serde_json::json!({
            "change": {"id": "c", "author": "Grace Hopper <grace@example.com> 1749933342 -0700"},
            "chapters": [{"steps": [{"findings": [
                {"kind": "breaking_migrated", "severity": "info", "message": "Signature changed; all 3 graph-known consumer(s) were co-updated in this diff (migration verified in-range)"},
                {"kind": "coverage_gap", "severity": "warning", "message": "Modified entity `x` has no test coverage"}
            ]}]}],
            "findings": {"breaking_changes": [], "notes": ["Visibility reduced; all 2 graph-known consumer(s) were co-updated in this diff (migration verified in-range)"]}
        });
        let polished: serde_json::Value =
            serde_json::from_str(&polish(guide.to_string(), polish_guide)).unwrap();
        assert_eq!(polished["change"]["author"], "Grace Hopper");
        let findings = &polished["chapters"][0]["steps"][0]["findings"];
        assert_eq!(
            findings[0]["message"],
            "Its signature changed. All 3 places that rely on it were updated in this change."
        );
        assert_eq!(findings[0]["kind"], "breaking_migrated");
        assert_eq!(findings[1]["message"], "No test reaches `x`.");
        assert_eq!(
            polished["findings"]["notes"][0],
            "It was made less visible. All 2 places that rely on it were updated in this change."
        );
        // An answer that does not read as JSON is passed on untouched.
        assert_eq!(polish("not json".into(), polish_guide), "not json");
        let history: serde_json::Value = serde_json::from_str(&polish(
            serde_json::json!({"revisions": [{"author": "Ada <ada@example.com> 1 +0000"}]})
                .to_string(),
            polish_history,
        ))
        .unwrap();
        assert_eq!(history["revisions"][0]["author"], "Ada");
    }

    #[tokio::test]
    async fn a_named_port_that_is_taken_fails_instead_of_moving() {
        let held = bind(Some(0)).await.unwrap();
        let taken = held.local_addr().unwrap().port();
        assert!(bind(Some(taken)).await.is_err());
    }
}
