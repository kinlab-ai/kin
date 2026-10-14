// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Where a repository stands against its remotes, and push and pull from the
//! local app.
//!
//! Reading moves nothing on either side. A push's standing comes from the
//! same negotiation `kin remote plan-push` runs in the daemon; a pull's from
//! the remote's own advertisement of its refs, and, for a peer daemon, from
//! the remote's own reading of the changes between the two heads. Counts are
//! changes and entities, read from the graph on whichever side holds them.
//! What Kin cannot ask, for want of a credential or an answer, is said as
//! that, never filled in.
//!
//! A push or a pull is sent once. Its answer names an outcome, and an answer
//! that is lost is reported as unknown and never sent again behind the page.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use kin_core::{KinConfig, KinLayout, RemoteHostKind, RemoteTransportKind};
use kin_model::{OperationId, RefName, RepositoryId, SemanticChangeId};
use kin_remote::repository_transfer::{RepositoryRefAdvertisement, RepositoryTransferError};
use kin_remote::repository_transfer_http::{
    HttpRepositoryTransferTransport, RepositoryTransferEndpoint,
};
use kin_remote::repository_transfer_negotiation::{
    RepositoryTransferDirection, RepositoryTransferPlan, RepositoryTransferTransport,
};
use serde::Deserialize;
use serde_json::{json, Value};

use super::review_ui::{self, UiState};
use super::transfer::{CommandTransferRequest, CommandTransferResponse, WorkspaceFollow};
use crate::daemon_client::{DaemonClient, TransferDispatch};

/// The schema every remotes answer carries.
pub(super) const REMOTES_SCHEMA: &str = "kin.workspace.remotes.v1";

/// How long one remote is given to answer a reading.
const REMOTE_READ_TIMEOUT: Duration = Duration::from_secs(15);

/// The most changes whose entities a reading counts. Past it the change count
/// is still given, and the entity count is said to be left uncounted.
const ENTITY_COUNT_MAX_CHANGES: usize = 1000;

/// How many sent operations a server remembers, so a second request with the
/// same operation is refused rather than sent again.
const SENT_KEPT: usize = 256;

/// How long an answer about Git on this machine is kept.
const GIT_READY_KEPT: Duration = Duration::from_secs(30);

pub(super) fn routes() -> Router<Arc<UiState>> {
    Router::new()
        .route("/api/workspace/remotes", get(serve_remotes))
        .route("/api/workspace/push", post(push))
        .route("/api/workspace/pull", post(pull))
        .route("/api/workspace/fetch", post(fetch))
}

/// The pushes and pulls one repository's page has sent.
pub(super) struct Transfers {
    /// Held while a push or a pull runs, so two from this app never race on
    /// one repository.
    running: Arc<tokio::sync::Mutex<()>>,
    /// Each operation sent, oldest first, with its outcome once it is known.
    sent: Mutex<VecDeque<(OperationId, Option<&'static str>)>>,
    /// Whether Git can carry a branch for this project, and when that was
    /// last asked, so a session that is read often starts Git seldom.
    git: Mutex<Option<(Instant, Result<(), String>)>>,
    /// Whether this branch can update from Git, as the daemon last said and
    /// when, and the ask running now, if one is. A session answers from what
    /// is known and never waits for an ask; see
    /// [`super::workspace_git_remote::update_ready_known`].
    update: Mutex<UpdateReadiness>,
}

/// What is known of whether this branch can update from Git.
#[derive(Default)]
struct UpdateReadiness {
    known: Option<(Instant, Result<(), String>)>,
    asking: Option<tokio::sync::watch::Receiver<Option<Result<(), String>>>>,
}

/// What a caller finds when it needs to know whether this branch can update
/// from Git.
pub(super) enum UpdateAsk {
    /// An ask is running: wait for its answer.
    Join(tokio::sync::watch::Receiver<Option<Result<(), String>>>),
    /// None is: this caller asks, and hands its answer to whoever joined.
    Lead(tokio::sync::watch::Sender<Option<Result<(), String>>>),
}

/// How long the daemon's answer on updating from Git is taken as current.
pub(super) const UPDATE_READY_KEPT: Duration = Duration::from_secs(30);

impl Default for Transfers {
    fn default() -> Self {
        Self {
            running: Arc::new(tokio::sync::Mutex::new(())),
            sent: Mutex::new(VecDeque::new()),
            git: Mutex::new(None),
            update: Mutex::new(UpdateReadiness::default()),
        }
    }
}

impl Transfers {
    fn sent(&self) -> std::sync::MutexGuard<'_, VecDeque<(OperationId, Option<&'static str>)>> {
        self.sent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Whether `operation` was sent before, and how it ended when known.
    fn earlier(&self, operation: OperationId) -> Option<Option<&'static str>> {
        self.sent()
            .iter()
            .find(|(sent, _)| *sent == operation)
            .map(|(_, outcome)| *outcome)
    }

    /// What Git last said about carrying a branch here, while it is fresh.
    pub(super) fn git_ready(&self) -> Option<Result<(), String>> {
        let known = self
            .git
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        known
            .as_ref()
            .filter(|(asked, _)| asked.elapsed() < GIT_READY_KEPT)
            .map(|(_, answer)| answer.clone())
    }

    pub(super) fn remember_git_ready(&self, answer: Result<(), String>) {
        *self
            .git
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((Instant::now(), answer));
    }

    fn update(&self) -> std::sync::MutexGuard<'_, UpdateReadiness> {
        self.update
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// What the daemon last said about updating this branch from Git, and
    /// how long ago, however old.
    pub(super) fn update_ready(&self) -> Option<(Duration, Result<(), String>)> {
        self.update()
            .known
            .as_ref()
            .map(|(asked, answer)| (asked.elapsed(), answer.clone()))
    }

    /// Whether an ask about updating from Git is running now.
    pub(super) fn update_asking(&self) -> bool {
        self.update()
            .asking
            .as_ref()
            .is_some_and(|asking| asking.has_changed().is_ok())
    }

    /// Join the ask about updating from Git that is running, or lead one.
    pub(super) fn ask_update(&self) -> UpdateAsk {
        let mut update = self.update();
        if let Some(asking) = &update.asking {
            // An ask whose asker went away without answering answers nobody.
            if asking.has_changed().is_ok() {
                return UpdateAsk::Join(asking.clone());
            }
        }
        let (sender, receiver) = tokio::sync::watch::channel(None);
        update.asking = Some(receiver);
        UpdateAsk::Lead(sender)
    }

    /// Keep the answer an ask led with [`Self::ask_update`] brought, and hand
    /// it to every caller that joined.
    pub(super) fn answer_update(
        &self,
        sender: tokio::sync::watch::Sender<Option<Result<(), String>>>,
        answer: Result<(), String>,
    ) {
        {
            let mut update = self.update();
            update.known = Some((Instant::now(), answer.clone()));
            update.asking = None;
        }
        sender.send_replace(Some(answer));
    }

    /// Record that `operation` is about to be sent.
    pub(super) fn mark_sent(&self, operation: OperationId) {
        let mut sent = self.sent();
        sent.push_back((operation, None));
        while sent.len() > SENT_KEPT {
            sent.pop_front();
        }
    }

    /// Record how `operation` ended.
    fn settle(&self, operation: OperationId, outcome: &'static str) {
        if let Some(entry) = self.sent().iter_mut().find(|(sent, _)| *sent == operation) {
            entry.1 = Some(outcome);
        }
    }
}

/// Where a credential for a remote came from. Never the credential itself.
#[derive(Clone)]
enum Credential {
    /// Another Kin repository on this machine answers at the remote's
    /// address, and its own local credential reaches it.
    ThisMachine {
        token: String,
        layout: KinLayout,
    },
    /// Named by the environment this app was started in.
    Environment(String),
    /// A saved credential that opens without a prompt.
    Saved(String),
    None,
}

impl Credential {
    fn label(&self) -> &'static str {
        match self {
            Self::ThisMachine { .. } => "this_machine",
            Self::Environment(_) => "environment",
            Self::Saved(_) => "saved",
            Self::None => "none",
        }
    }

    fn token(&self) -> Option<&str> {
        match self {
            Self::ThisMachine { token, .. } | Self::Environment(token) | Self::Saved(token) => {
                Some(token)
            }
            Self::None => None,
        }
    }
}

/// How Kin addresses one native remote's transfer seam.
#[derive(Clone)]
struct Peer {
    base_url: String,
    /// A hosted remote's organization. `None` is a peer daemon.
    organization_id: Option<String>,
    credential: Credential,
}

/// One remote this repository names, as its configuration records it.
#[derive(Clone)]
pub(super) struct Remote {
    pub(super) name: String,
    host: RemoteHostKind,
    native: bool,
    default: bool,
    /// The remote's address without any credential it carried.
    address: Option<String>,
    /// A native remote's transfer peer, or why Kin cannot address one. A Git
    /// remote has none: Git reaches it.
    peer: Result<Peer, String>,
}

/// The remotes of one repository, and the one a push or pull uses when the
/// page names none.
struct Remotes {
    remotes: Vec<Remote>,
    default: Result<String, String>,
}

/// An address fit to show: a URL without its credentials, or nothing.
fn shown_address(url: Option<&str>) -> Option<String> {
    let url = url.map(str::trim).filter(|url| !url.is_empty())?;
    kin_remote::repository_transfer::redacted_remote_address(url)
}

/// The credential Kin can use for `base_url` without asking anyone. Another
/// repository on this machine whose daemon records this loopback port comes
/// first: it is the most specific, and the daemon is checked to be that
/// repository's before its credential is sent.
fn credential_for(base_url: &str, this_machine: &[KinLayout]) -> Credential {
    if crate::daemon_client::endpoint_is_loopback(base_url) {
        let port = url::Url::parse(base_url)
            .ok()
            .and_then(|url| url.port_or_known_default());
        let found = this_machine.iter().find_map(|layout| {
            let (_, recorded) = crate::daemon_client::repo_daemon_recorded_endpoint(layout.root());
            if recorded.is_none() || recorded != port {
                return None;
            }
            let token = crate::daemon_client::resolve_daemon_auth_token_for_layout(layout)?;
            Some(Credential::ThisMachine {
                token,
                layout: layout.clone(),
            })
        });
        if let Some(found) = found {
            return found;
        }
    }
    if let Some(token) = super::remote::native_remote_bearer_token_from_environment(base_url) {
        return Credential::Environment(token);
    }
    match super::auth::saved_bearer_token_without_prompt(base_url) {
        Some(token) => Credential::Saved(token),
        None => Credential::None,
    }
}

/// Where a native remote's transfer seam is, as `kin push` resolves it.
fn native_peer(
    host: RemoteHostKind,
    url: Option<&str>,
    this_machine: &[KinLayout],
) -> Result<Peer, String> {
    let url = url
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .ok_or_else(|| "This remote has no transfer address.".to_string())?;
    let (base_url, located_organization) = super::transfer::peer_base_and_organization(url);
    let organization_id = match host {
        RemoteHostKind::KinLab => Some(
            located_organization
                .or_else(|| {
                    std::env::var("KIN_ORG_ID")
                        .ok()
                        .map(|value| value.trim().to_string())
                        .filter(|value| !value.is_empty())
                })
                .ok_or_else(|| {
                    "This KinLab remote's address names no organization, so Kin cannot address \
                     its repository."
                        .to_string()
                })?,
        ),
        _ => None,
    };
    if kin_remote::http_transport::validate_credential_url(&base_url).is_err() {
        return Err(
            "This remote's address is not one Kin sends a credential to: it must be HTTPS, or \
             HTTP on this machine's own loopback address."
                .to_string(),
        );
    }
    Ok(Peer {
        credential: credential_for(&base_url, this_machine),
        base_url,
        organization_id,
    })
}

/// Every remote the repository's configuration names. Blocking: it reads the
/// configuration and, for a loopback address, the daemon records of this
/// machine's other repositories.
fn read_remotes(layout: &KinLayout, this_machine: &[KinLayout]) -> Result<Remotes, String> {
    let config = KinConfig::load_or_default(&layout.config_path())
        .map_err(|error| format!("Kin could not read this repository's remotes: {error:#}"))?;
    let mut remotes = Vec::new();
    for remote in &config.remote.refs {
        let native = remote.transport == RemoteTransportKind::NativeKin;
        remotes.push(Remote {
            name: remote.name.clone(),
            host: remote.host,
            native,
            default: config.remote.default.as_deref() == Some(remote.name.as_str()),
            address: shown_address(remote.url.as_deref()),
            peer: if native {
                native_peer(remote.host, remote.url.as_deref(), this_machine)
            } else {
                Err(GIT_REACHES_IT.to_string())
            },
        });
    }
    for sealed in &config.git.remotes {
        if remotes.iter().any(|known| known.name == sealed.name) {
            continue;
        }
        let host = sealed.host_kind();
        remotes.push(Remote {
            name: sealed.name.clone(),
            host,
            native: false,
            default: false,
            address: shown_address(sealed.publish_url()),
            peer: Err(GIT_REACHES_IT.to_string()),
        });
    }
    let default = default_remote(config.remote.default.as_deref(), &remotes);
    Ok(Remotes { remotes, default })
}

/// The remote a push or pull uses when the page names none: the configured
/// default, else the only Kin remote, else the only Git remote.
fn default_remote(configured: Option<&str>, remotes: &[Remote]) -> Result<String, String> {
    if let Some(name) = configured {
        if remotes.iter().any(|remote| remote.name == name) {
            return Ok(name.to_string());
        }
    }
    let of_kind = |native: bool| -> Vec<&Remote> {
        remotes
            .iter()
            .filter(|remote| remote.native == native)
            .collect()
    };
    match of_kind(true).as_slice() {
        [] => {}
        [only] => return Ok(only.name.clone()),
        many => {
            return Err(format!(
                "{} Kin remotes are set up and none is the default, so name the one to use.",
                many.len()
            ))
        }
    }
    match of_kind(false).as_slice() {
        [] => Err("No remote is set up for this repository.".to_string()),
        [only] => Ok(only.name.clone()),
        many => Err(format!(
            "{} Git remotes are set up and none is the default, so name the one to use.",
            many.len()
        )),
    }
}

/// Why a Git remote has no transfer peer of Kin's.
const GIT_REACHES_IT: &str = "This is a Git remote, which Git reaches.";

/// Why a push or pull cannot run for this repository at all, or nothing.
///
/// A store that holds part of its history cannot send or take it as if it
/// were whole. What it holds is said as a count when the record has one, and
/// the rest is said to be on its way only while a derivation is actually
/// reading it or has read it, so a store whose reading has stopped, or never
/// started, is not described as filling in.
fn history_reason(standing: &kin_core::history_state::HistoryStanding) -> Option<String> {
    use kin_core::history_state::{DerivationState, HistoryStanding};
    standing.sentence()?;
    let (held, staged) = match standing {
        HistoryStanding::Partial { record, progress } => (
            match record.reachable_commits {
                Some(total) => format!(
                    "Kin holds {} of this repository's {} commits",
                    grouped(record.admitted_commits),
                    grouped(total)
                ),
                None => "Kin holds only part of this repository's history".to_string(),
            },
            progress
                .as_ref()
                .is_some_and(|progress| progress.state == DerivationState::Staged),
        ),
        _ => (
            "Kin could not read how much of this repository's history it holds".to_string(),
            false,
        ),
    };
    let rest = if super::daemon_activity::running_history(standing).is_some() {
        " and is reading the rest now"
    } else if staged {
        " and has read the rest, which it takes in at its next quiet point"
    } else {
        ""
    };
    Some(format!(
        "{held}{rest}, so it cannot be pushed or pulled until it holds all of it."
    ))
}

/// A count as a reader writes it: `4,673`.
fn grouped(count: u64) -> String {
    let digits = count.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// Whether a remote can be pushed to and pulled from as far as this machine
/// can tell without asking it, or why not.
fn usable(remote: &Remote, history: Option<&str>) -> Result<(), String> {
    let peer = remote.peer.as_ref().map_err(Clone::clone)?;
    if let Some(reason) = history {
        return Err(reason.to_string());
    }
    if matches!(peer.credential, Credential::None) {
        return Err(format!(
            "Kin holds no credential for {} that it can read without asking, so it cannot reach \
             it.",
            remote.name
        ));
    }
    Ok(())
}

/// Whether a push and a pull can run against `remote` as far as this machine
/// can tell without asking it, each with why not. A Git remote is published
/// to when Git can carry the branch, and updated only when exact local
/// Git-origin admission can advance it.
fn availability(
    remote: &Remote,
    history: Option<&str>,
    git: &Result<(), String>,
    git_pull: &Result<(), String>,
) -> (Result<(), String>, Result<(), String>) {
    if remote.native {
        let usable = usable(remote, history);
        return (usable.clone(), usable);
    }
    let push = match history {
        Some(reason) => Err(reason.to_string()),
        None => git.clone(),
    };
    let pull = match history {
        Some(reason) => Err(reason.to_string()),
        None => git
            .as_ref()
            .map_err(Clone::clone)
            .and_then(|_| git_pull.clone()),
    };
    (push, pull)
}

/// The session's `remote_push` and `remote_pull`: whether a push and a pull
/// can work for this repository, from its configuration, its credentials and
/// what Git said, each with why not. No remote is asked. Blocking.
pub(super) fn transfer_capability(
    layout: &KinLayout,
    history: &kin_core::history_state::HistoryStanding,
    this_machine: &[KinLayout],
    git: &Result<(), String>,
    git_pull: &Result<(), String>,
) -> (Result<(), String>, Result<(), String>, bool) {
    let remotes = match read_remotes(layout, this_machine) {
        Ok(remotes) => remotes,
        Err(reason) => return (Err(reason.clone()), Err(reason), false),
    };
    let history = history_reason(history);
    let none = || Err("No remote is set up for this repository.".to_string());
    let (mut push, mut pull) = (none(), none());
    let mut fetch = false;
    for (index, remote) in remotes.remotes.iter().enumerate() {
        let (can_push, can_pull) = availability(remote, history.as_deref(), git, git_pull);
        fetch |= remote.native && can_pull.is_ok();
        // The first remote's reason stands until some remote can.
        if can_push.is_ok() || (index == 0 && push.is_err()) {
            push = can_push;
        }
        if can_pull.is_ok() || (index == 0 && pull.is_err()) {
            pull = can_pull;
        }
    }
    (push, pull, fetch)
}

/// The repositories this machine can open, for a credential of their own.
async fn machine_repositories(state: &Arc<UiState>) -> Vec<KinLayout> {
    let Some(catalog) = state.catalog.clone() else {
        return Vec::new();
    };
    tokio::task::spawn_blocking(move || {
        catalog
            .read()
            .repositories
            .into_iter()
            .map(|listed| listed.layout)
            .collect()
    })
    .await
    .unwrap_or_default()
}

/// The branch this workspace is on, its head, and the repository's identity.
pub(super) struct LocalBranch {
    pub(super) repository_id: RepositoryId,
    /// `None` for a detached workspace.
    pub(super) branch: Option<RefName>,
    branches: Vec<(RefName, Option<SemanticChangeId>)>,
}

impl LocalBranch {
    pub(super) fn head_of(&self, name: &RefName) -> Option<SemanticChangeId> {
        self.branches
            .iter()
            .find(|(branch, _)| branch == name)
            .and_then(|(_, head)| *head)
    }
}

pub(super) async fn local_branch(client: &DaemonClient) -> anyhow::Result<LocalBranch> {
    use anyhow::Context as _;
    let report = client
        .branch(&super::branch::BranchRequest::List)
        .await?
        .report
        .context("the daemon answered the branch list without its report")?;
    let branch = match &report.workspace_head {
        kin_model::WorkspaceHead::Symbolic { target } if target.is_branch() => Some(target.clone()),
        _ => None,
    };
    Ok(LocalBranch {
        repository_id: report.repository_id,
        branch,
        branches: report
            .branches
            .into_iter()
            // A Git-origin branch targets its Git commit and a native one its
            // change; the daemon resolves either to the change it stands on.
            .map(|entry| (entry.name, entry.change_id))
            .collect(),
    })
}

pub(super) fn short_ref(name: &RefName) -> String {
    let full = name.to_string();
    full.strip_prefix("refs/heads/")
        .unwrap_or(&full)
        .to_string()
}

fn transfer_endpoint(peer: &Peer, timeout: Option<Duration>) -> RepositoryTransferEndpoint {
    let mut endpoint = RepositoryTransferEndpoint::new(peer.base_url.clone());
    if let Some(token) = peer.credential.token() {
        endpoint = endpoint.with_auth(token);
    }
    if let Some(organization) = &peer.organization_id {
        endpoint = endpoint.with_organization(organization.clone());
    }
    if let Some(timeout) = timeout {
        endpoint = endpoint.with_timeout(timeout.as_secs());
    }
    endpoint
}

/// The remote's own advertisement of its refs. Reads, moves nothing.
async fn advertisement(
    peer: &Peer,
    repository_id: &RepositoryId,
) -> Result<RepositoryRefAdvertisement, RepositoryTransferError> {
    let endpoint = transfer_endpoint(peer, Some(REMOTE_READ_TIMEOUT));
    let repository_id = repository_id.clone();
    tokio::task::spawn_blocking(move || {
        HttpRepositoryTransferTransport::new(endpoint).advertise_refs(&repository_id)
    })
    .await
    .unwrap_or_else(|error| {
        Err(RepositoryTransferError::Storage(format!(
            "the reading did not finish: {error}"
        )))
    })
}

fn advertised_head(
    advertisement: &RepositoryRefAdvertisement,
    name: &RefName,
) -> Option<SemanticChangeId> {
    advertisement
        .refs
        .iter()
        .find(|entry| &entry.name == name)
        .map(|entry| entry.head)
}

/// A remote's refusal or silence, in words.
fn remote_reason(error: &RepositoryTransferError) -> String {
    match error {
        RepositoryTransferError::Unauthenticated(_) => {
            "The remote refused the credential Kin sent.".to_string()
        }
        RepositoryTransferError::Storage(message) if message.contains("transport failed") => {
            "The remote could not be reached.".to_string()
        }
        RepositoryTransferError::Invalid(message) if message.contains("HTTP 404") => {
            "The remote does not hold this repository.".to_string()
        }
        other => format!("The remote did not answer the reading: {other}"),
    }
}

/// Why a pull cannot run when each side holds changes the other lacks.
const DIVERGED_PULL: &str = "This repository holds changes the remote does not, and the remote \
                             holds changes this repository does not. Kin moves only fast-forwards.";

/// The local daemon's refusal of a negotiation, in words.
fn daemon_reason(status: u16, body: &str) -> String {
    if body.contains("HTTP 401") {
        return "The remote refused the credential Kin sent.".to_string();
    }
    if body.contains("transport failed") {
        return "The remote could not be reached.".to_string();
    }
    if body.contains("(HTTP 404)") {
        return "The remote does not hold this repository.".to_string();
    }
    if body.contains("identical imported-Git authority") {
        return "This repository's history was converted from Git, and the remote's history did \
                not start from the same conversion. Kin moves history only between replicas that \
                share it."
            .to_string();
    }
    if body.contains("is not an ancestor of local head") {
        return "The remote holds changes this repository does not. Kin moves only \
                fast-forwards, so those changes have to come here first."
            .to_string();
    }
    if body.contains("is not an ancestor of source head") {
        return DIVERGED_PULL.to_string();
    }
    let body = body.trim();
    if body.is_empty() {
        format!("Kin refused with HTTP {status}.")
    } else {
        format!("Kin refused: {body}")
    }
}

/// Changes, entities and how the entities changed, from a semantic diff.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct RangeCount {
    pub(super) ahead: usize,
    pub(super) behind: usize,
    pub(super) entities: usize,
    added: usize,
    modified: usize,
    removed: usize,
}

fn range_count(diff: &Value) -> Option<RangeCount> {
    let entities = diff.get("entities")?.as_array()?;
    let kind = |wanted: &str| {
        entities
            .iter()
            .filter(|row| row.get("change_type").and_then(Value::as_str) == Some(wanted))
            .count()
    };
    Some(RangeCount {
        ahead: usize::try_from(diff.get("ahead")?.as_u64()?).ok()?,
        behind: usize::try_from(diff.get("behind")?.as_u64()?).ok()?,
        entities: entities.len(),
        added: kind("added"),
        modified: kind("modified"),
        removed: kind("removed"),
    })
}

pub(super) fn entity_changes(count: &RangeCount) -> Value {
    json!({"added":count.added,"modified":count.modified,"removed":count.removed})
}

/// One graph's reading of the range between two changes. `Err(None)` is a
/// graph that does not hold one of them, `Err(Some(reason))` one that did not
/// answer.
pub(super) type RangeReading = Result<RangeCount, Option<String>>;

/// The local graph's reading of what lies between two changes.
pub(super) async fn local_range_reading(
    client: &DaemonClient,
    repository_id: &RepositoryId,
    base: SemanticChangeId,
    head: SemanticChangeId,
) -> RangeReading {
    match client
        .repository_semantic_diff(repository_id.as_str(), &base.to_string(), &head.to_string())
        .await
    {
        Ok(Ok(diff)) => range_count(&diff).ok_or_else(|| {
            Some("This repository's daemon answered a reading Kin could not read.".to_string())
        }),
        // This graph could not resolve a read point: it does not hold it.
        Ok(Err((404 | 422, _))) => Err(None),
        Ok(Err((status, _))) => Err(Some(format!(
            "This repository's daemon answered HTTP {status}."
        ))),
        Err(_) => Err(Some(
            "This repository's daemon did not answer the reading.".to_string(),
        )),
    }
}

/// [`local_range_reading`] for a caller that only counts when it can.
pub(super) async fn local_range(
    client: &DaemonClient,
    repository_id: &RepositoryId,
    base: SemanticChangeId,
    head: SemanticChangeId,
) -> Option<RangeCount> {
    local_range_reading(client, repository_id, base, head)
        .await
        .ok()
}

/// What a pull would bring, as far as this repository's own graph settles
/// it, from its reading of the range from the remote's head to its own. A
/// remote head this graph holds is never something a pull brings. `None`
/// means this graph does not hold the remote's head, so the remote is asked.
fn pull_settled_here(here: &RangeReading) -> Option<Side> {
    match here {
        // The remote's head is an ancestor of this repository's.
        Ok(count) if count.behind == 0 => Some(Side::new("nothing_to_bring")),
        // Held here, and each head has changes the other lacks.
        Ok(_) => Some(Side::new("blocked").because(DIVERGED_PULL)),
        Err(Some(reason)) => Some(Side::new("unknown").because(reason.clone())),
        Err(None) => None,
    }
}

/// What a pull would bring, from the remote's reading of the range from this
/// repository's head to its own, once this repository is known not to hold
/// the remote's head.
fn pull_settled_there(there: RangeReading) -> Side {
    match there {
        Ok(count) if count.behind == 0 => {
            let mut side = Side::new("would_bring");
            side.changes = Some(count.ahead);
            side.entities = Some(count);
            side
        }
        // The remote does not hold this repository's head either, or holds
        // it off its own line.
        Ok(_) | Err(None) => Side::new("blocked").because(DIVERGED_PULL),
        Err(Some(reason)) => Side::new("unknown").because(reason),
    }
}

/// What a peer daemon's graph reads between two read points.
async fn remote_range(
    peer: &Peer,
    repository_id: &RepositoryId,
    base: SemanticChangeId,
    head: SemanticChangeId,
) -> RangeReading {
    let client = reqwest::Client::builder()
        .timeout(REMOTE_READ_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| {
            Some(format!(
                "Kin could not build a reader for the remote: {error}"
            ))
        })?;
    let mut request = client
        .get(format!(
            "{}/repos/{}/semantic-diff",
            peer.base_url,
            crate::daemon_client::urlencoding::encode(repository_id.as_str())
        ))
        .query(&[("base", base.to_string()), ("head", head.to_string())]);
    if let Some(token) = peer.credential.token() {
        request = request.bearer_auth(token);
    }
    let response = request
        .send()
        .await
        .map_err(|_| Some("The remote could not be reached.".to_string()))?;
    match response.status().as_u16() {
        200..=299 => {}
        401 => {
            return Err(Some(
                "The remote refused the credential Kin sent.".to_string(),
            ))
        }
        // The remote could not resolve a read point: it does not hold it.
        404 | 422 => return Err(None),
        status => return Err(Some(format!("The remote answered HTTP {status}."))),
    }
    let diff: Value = response
        .json()
        .await
        .map_err(|_| Some("The remote's answer could not be read.".to_string()))?;
    range_count(&diff).ok_or_else(|| Some("The remote's answer could not be read.".to_string()))
}

/// One side of a reading.
pub(super) struct Side {
    pub(super) state: &'static str,
    pub(super) changes: Option<usize>,
    pub(super) entities: Option<RangeCount>,
    pub(super) reason: Option<String>,
    /// What gets a blocked side moving again, and the branch it goes
    /// through: `fetch_then_merge` or `merge`.
    resolve: Option<(&'static str, String)>,
}

impl Side {
    pub(super) fn new(state: &'static str) -> Self {
        Self {
            state,
            changes: None,
            entities: None,
            reason: None,
            resolve: None,
        }
    }

    pub(super) fn because(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }

    pub(super) fn json(&self) -> Value {
        json!({
            "state": self.state,
            "changes": self.changes,
            "entities": self.entities.map(|count| count.entities),
            "entity_changes": self.entities.as_ref().map(entity_changes),
            "reason": self.reason,
            "resolve": self.resolve.as_ref().map(|(how, _)| *how),
            "resolve_branch": self.resolve.as_ref().map(|(_, branch)| branch.as_str()),
        })
    }
}

/// How the two sides sit, from what each would move.
pub(super) fn overall(push: &Side, pull: &Side) -> &'static str {
    match (push.state, pull.state) {
        ("would_send", "would_bring") | ("blocked", "blocked") => "diverged",
        ("would_send", "nothing_to_bring") => "ahead",
        (_, "would_bring") => "behind",
        ("nothing_to_send", "nothing_to_bring") => "up_to_date",
        ("blocked", "nothing_to_bring") => "blocked",
        _ => "unknown",
    }
}

fn summary(state: &str, push: &Side, pull: &Side) -> String {
    let counted = |side: &Side, verb: &str| match (side.changes, side.entities) {
        (Some(changes), Some(entities)) => format!(
            "{verb} {changes} {} touching {} {}",
            plural(changes, "change", "changes"),
            entities.entities,
            plural(entities.entities, "entity", "entities")
        ),
        (Some(changes), None) => {
            format!("{verb} {changes} {}", plural(changes, "change", "changes"))
        }
        _ => format!("{verb} changes"),
    };
    match state {
        "up_to_date" => "This branch matches the remote.".to_string(),
        "ahead" => format!("A push would {}.", counted(push, "send")),
        "behind" => format!("A pull would {}.", counted(pull, "bring")),
        "diverged" => "This repository and the remote each hold changes the other does not. Kin \
                       moves only fast-forwards, so neither a push nor a pull can run until they \
                       are brought together."
            .to_string(),
        _ => push
            .reason
            .clone()
            .or_else(|| pull.reason.clone())
            .unwrap_or_else(|| "Kin could not read where this branch stands.".to_string()),
    }
}

pub(super) fn plural<'a>(count: usize, one: &'a str, many: &'a str) -> &'a str {
    if count == 1 {
        one
    } else {
        many
    }
}

/// A standing nobody read, in the shape a read one has, with why.
pub(super) fn not_read(reason: impl Into<String>) -> Value {
    let reason = reason.into();
    json!({
        "read":"not_read",
        "state":"unknown",
        "summary":reason.clone(),
        "remote_head":null,
        "push":Side::new("unknown").because(reason.clone()).json(),
        "pull":Side::new("unknown").because(reason).json(),
    })
}

/// Where the current branch stands against one native remote. Moves nothing.
async fn native_standing(
    client: &DaemonClient,
    local: &LocalBranch,
    branch: &RefName,
    peer: &Peer,
    remote_name: &str,
) -> Value {
    if matches!(peer.credential, Credential::None) {
        return not_read(
            "Kin holds no credential for this remote that it can read without asking, so it was \
             not asked.",
        );
    }
    if let Credential::ThisMachine { layout, .. } = &peer.credential {
        if DaemonClient::from_running_workspace(peer.base_url.clone(), layout)
            .await
            .is_err()
        {
            return not_read(
                "The Kin repository on this machine that this address named does not answer \
                 there now, so it was not asked.",
            );
        }
    }
    let advertised = match advertisement(peer, &local.repository_id).await {
        Ok(advertised) => advertised,
        Err(error) => return not_read(remote_reason(&error)),
    };
    let remote_head = advertised_head(&advertised, branch);
    let local_head = local.head_of(branch);

    let mut ancestor_of_local = remote_head.is_some() && remote_head == local_head;
    // A branch with no change here has nothing to push, and the negotiation
    // would only refuse to plan one.
    let plan = match local_head {
        Some(_) => Some(
            client
                .command_transfer_plan_reading(&transfer_request(peer, branch))
                .await,
        ),
        None => None,
    };
    let push = match plan {
        None => Side::new("nothing_to_send")
            .because("This repository holds nothing on this branch yet."),
        Some(plan) => match plan {
            Ok(Ok(plan)) => match plan.plan.plan {
                RepositoryTransferPlan::UpToDate { .. } => Side::new("nothing_to_send"),
                RepositoryTransferPlan::FastForward {
                    destination_head,
                    source_head,
                    change_count,
                } => {
                    ancestor_of_local =
                        destination_head.is_some() && destination_head == remote_head;
                    let mut side = Side::new("would_send");
                    side.changes = change_count;
                    match destination_head {
                        Some(base) if change_count.unwrap_or(0) <= ENTITY_COUNT_MAX_CHANGES => {
                            side.entities =
                                local_range(client, &local.repository_id, base, source_head).await;
                        }
                        Some(_) => {
                            side.reason = Some(
                                "These changes are too many to count their entities here."
                                    .to_string(),
                            );
                        }
                        None => {
                            side.reason = Some(
                                "The remote holds nothing on this branch yet, so a push sends the \
                             whole branch."
                                    .to_string(),
                            );
                        }
                    }
                    side
                }
            },
            Ok(Err((409, body))) => Side::new("blocked").because(daemon_reason(409, &body)),
            Ok(Err((status, body))) => Side::new("unknown").because(daemon_reason(status, &body)),
            Err(_) => Side::new("unknown")
                .because("This repository's daemon did not answer the push reading."),
        },
    };

    let pull = match (remote_head, local_head) {
        (None, _) => {
            Side::new("nothing_to_bring").because("The remote holds nothing on this branch.")
        }
        (Some(remote), Some(local_head)) if remote == local_head => Side::new("nothing_to_bring"),
        _ if ancestor_of_local => Side::new("nothing_to_bring"),
        (Some(_), None) => Side::new("would_bring").because(
            "This repository holds nothing on this branch yet, so a pull brings the remote's \
                 whole branch.",
        ),
        (Some(remote), Some(local_head)) => {
            // This repository's own graph first: a remote head it holds is
            // nothing a pull brings, whatever the push reading came to.
            let here = local_range_reading(client, &local.repository_id, remote, local_head).await;
            match pull_settled_here(&here) {
                Some(side) => side,
                None if peer.organization_id.is_none() => pull_settled_there(
                    remote_range(peer, &local.repository_id, local_head, remote).await,
                ),
                None => Side::new("unknown").because(
                    "The remote holds changes this repository does not. It does not count them \
                     before a pull, so how many is known once they arrive.",
                ),
            }
        }
    };
    // A branch that has diverged moves again by bringing the remote's branch
    // in beside it and merging the two. Once it is in, only the merge is left.
    let mut pull = pull;
    if pull.state == "blocked" && pull.reason.as_deref() == Some(DIVERGED_PULL) {
        let beside = tracking_name(remote_name, branch);
        let held = RefName::branch(beside.as_bytes())
            .ok()
            .and_then(|beside| local.head_of(&beside));
        let how = if held.is_some() && held == remote_head {
            "merge"
        } else {
            "fetch_then_merge"
        };
        pull.resolve = Some((how, beside));
    }
    let push = if push.state == "blocked" && pull.state == "would_bring" {
        Side::new("blocked").because(
            "The remote holds changes this repository does not. Kin moves only fast-forwards, so \
             those changes have to come here first.",
        )
    } else {
        push
    };
    let state = overall(&push, &pull);
    json!({
        "read":"complete",
        "state":state,
        "summary":summary(state, &push, &pull),
        "remote_head":remote_head.map(|head| head.to_string()),
        "push":push.json(),
        "pull":pull.json(),
    })
}

/// Changes this branch holds that Kin recorded since the newest one it took
/// from Git, counted in changes and entities. `None` when none came from Git
/// within the newest page of history, or the reading failed.
pub(super) async fn since_git(
    client: &DaemonClient,
    local: &LocalBranch,
    branch: &RefName,
) -> Option<Value> {
    let head = local.head_of(branch)?;
    let log = client
        .log(&super::log::LogRequest {
            count: 200,
            skip: 0,
        })
        .await
        .ok()?
        .report?;
    let from_git = log
        .entries
        .iter()
        .find(|entry| matches!(entry.origin, kin_model::ChangeOrigin::GitCommit { .. }))?
        .change_id;
    if from_git == head {
        return Some(
            json!({"changes":0,"entities":0,"entity_changes":entity_changes(&RangeCount::default())}),
        );
    }
    let count = local_range(client, &local.repository_id, from_git, head).await?;
    Some(json!({
        "changes":count.ahead,
        "entities":count.entities,
        "entity_changes":entity_changes(&count),
    }))
}

/// The local branch a remote's branch is brought in as when the page names
/// none: the remote's name, then the branch's.
fn tracking_name(remote: &str, branch: &RefName) -> String {
    format!("{remote}/{}", short_ref(branch))
}

fn transfer_request(peer: &Peer, branch: &RefName) -> CommandTransferRequest {
    CommandTransferRequest {
        remote_base_url: peer.base_url.clone(),
        remote_token: peer.credential.token().map(str::to_string),
        remote_organization_id: peer.organization_id.clone(),
        repository_id: None,
        source_ref: Some(branch.clone()),
        destination_ref: Some(branch.clone()),
    }
}

/// `GET /api/workspace/remotes`: each remote, which one is the default, and
/// for the current branch what a push would send and what a pull would bring.
async fn serve_remotes(State(state): State<Arc<UiState>>) -> Response {
    let machine = machine_repositories(&state).await;
    let read = {
        let layout = state.layout.clone();
        tokio::task::spawn_blocking(move || {
            let history = kin_core::history_state::standing(&layout);
            (read_remotes(&layout, &machine), history_reason(&history))
        })
        .await
    };
    let (remotes, history) = match read {
        Ok((Ok(remotes), history)) => (remotes, history),
        Ok((Err(reason), _)) => {
            return review_ui::refusal(StatusCode::BAD_GATEWAY, &anyhow::anyhow!(reason))
        }
        Err(error) => {
            return review_ui::refusal(
                StatusCode::BAD_GATEWAY,
                &anyhow::anyhow!("the remotes reading did not finish: {error}"),
            )
        }
    };
    let any_native = remotes.remotes.iter().any(|remote| remote.native);
    // The daemon is asked only when there is something to compare against.
    let local = if remotes.remotes.is_empty() {
        Err("No remote is set up for this repository.".to_string())
    } else {
        match review_ui::daemon(&state).await {
            Ok(client) => match local_branch(&client).await {
                Ok(local) => Ok((client, local)),
                Err(_) => Err(
                    "This repository's daemon did not answer, so nothing was compared.".to_string(),
                ),
            },
            Err(_) => {
                Err("This repository's daemon did not answer, so nothing was compared.".to_string())
            }
        }
    };
    let branch = local
        .as_ref()
        .ok()
        .and_then(|(_, local)| local.branch.clone());
    // Git is asked about itself once, and only when a Git remote is shown.
    let git = if remotes.remotes.iter().any(|remote| !remote.native) {
        super::workspace_git_remote::ready(&state).await
    } else {
        Ok(())
    };
    let git_pull = if remotes.remotes.iter().any(|remote| !remote.native) {
        super::workspace_git_remote::update_ready_within(&state, Duration::from_secs(5)).await
    } else {
        Err("No Git remote is set up for this repository.".to_string())
    };
    // Every remote is read at once, so one that is slow to answer holds up
    // no other.
    let rows = futures_util::future::join_all(remotes.remotes.iter().map(|remote| {
        let (local, branch, history, git, git_pull, state) =
            (&local, &branch, &history, &git, &git_pull, &state);
        async move {
            let (can_push, can_pull) = availability(remote, history.as_deref(), git, git_pull);
            let late = || not_read("The remote did not answer in time, so nothing was compared.");
            let standing = match (local, branch, &remote.peer) {
                (Err(reason), _, _) => not_read(reason.as_str()),
                (Ok(_), None, _) => {
                    not_read("This workspace is on no branch, so there is nothing to compare.")
                }
                (Ok((client, local)), Some(branch), _) if !remote.native => tokio::time::timeout(
                    REMOTE_READ_TIMEOUT * 2,
                    super::workspace_git_remote::standing(
                        state, client, local, branch, remote, git, git_pull,
                    ),
                )
                .await
                .unwrap_or_else(|_| late()),
                (Ok((client, local)), Some(branch), Ok(peer)) => tokio::time::timeout(
                    REMOTE_READ_TIMEOUT * 2,
                    native_standing(client, local, branch, peer, &remote.name),
                )
                .await
                .unwrap_or_else(|_| late()),
                (Ok(_), Some(_), Err(reason)) => not_read(reason.as_str()),
            };
            json!({
                "name":remote.name,
                "kind":if remote.native { "native" } else { "git" },
                "host":remote.host.as_str(),
                "default":remote.default,
                "address":remote.address,
                "credential":match &remote.peer {
                    Ok(peer) => peer.credential.label(),
                    // Git reaches a Git remote, with the person's own credentials.
                    Err(_) if !remote.native => "git",
                    Err(_) => "not_applicable",
                },
                "can_push":can_push.is_ok(),
                "can_pull":can_pull.is_ok(),
                "can_fetch":remote.native && can_pull.is_ok(),
                "unavailable_reason":can_push.err(),
                "pull_unavailable_reason":can_pull.err(),
                "standing":standing,
            })
        }
    }))
    .await;
    let (default_remote, default_reason) = match &remotes.default {
        Ok(name) => (Some(name.clone()), None),
        Err(reason) => (None, Some(reason.clone())),
    };
    let head = match (&local, &branch) {
        (Ok((_, local)), Some(branch)) => local.head_of(branch).map(|head| head.to_string()),
        _ => None,
    };
    Json(json!({
        "schema":REMOTES_SCHEMA,
        "branch":branch.as_ref().map(short_ref),
        "ref":branch.as_ref().map(ToString::to_string),
        "head":head,
        "default_remote":default_remote,
        "default_remote_reason":default_reason,
        "history_complete":history.is_none(),
        "any_native":any_native,
        "remotes":rows,
        "basis":"Read for this answer from this repository's daemon and from each remote Kin could ask. Nothing moved on either side.",
    }))
    .into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TransferBody {
    #[serde(default)]
    remote: Option<String>,
    #[serde(default, rename = "ref")]
    reference: Option<String>,
    /// For a fetch, the local branch the remote's branch is brought in as.
    #[serde(default, rename = "as")]
    into: Option<String>,
    operation_id: OperationId,
}

async fn push(
    State(state): State<Arc<UiState>>,
    headers: HeaderMap,
    Json(body): Json<TransferBody>,
) -> Response {
    transfer(state, headers, body, Act::Push).await
}

async fn pull(
    State(state): State<Arc<UiState>>,
    headers: HeaderMap,
    Json(body): Json<TransferBody>,
) -> Response {
    transfer(state, headers, body, Act::Pull).await
}

/// `POST /api/workspace/fetch`: bring the remote's branch in as a local
/// branch of its own, moving nothing that is here. What a diverged branch
/// needs before the two can be merged.
async fn fetch(
    State(state): State<Arc<UiState>>,
    headers: HeaderMap,
    Json(body): Json<TransferBody>,
) -> Response {
    transfer(state, headers, body, Act::Fetch).await
}

/// What a transfer route was asked to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Act {
    Push,
    Pull,
    /// Bring the remote's branch in beside the local one, as a branch of its
    /// own. A pull onto another branch, and nothing here moves.
    Fetch,
}

impl Act {
    /// Which way history moves at the daemon.
    fn direction(self) -> RepositoryTransferDirection {
        match self {
            Self::Push => RepositoryTransferDirection::Push,
            Self::Pull | Self::Fetch => RepositoryTransferDirection::Pull,
        }
    }
}

fn verb(direction: Act) -> &'static str {
    match direction {
        Act::Push => "push",
        Act::Pull => "pull",
        Act::Fetch => "fetch",
    }
}

/// How one push or pull ended, as the page reads it.
#[derive(Debug)]
pub(super) struct Ended {
    pub(super) outcome: &'static str,
    pub(super) status: StatusCode,
    pub(super) body: Value,
}

/// Nothing was sent: the request stopped before the daemon was asked.
pub(super) fn not_sent(direction: Act, reason: impl Into<String>) -> Ended {
    Ended {
        outcome: "refused",
        status: StatusCode::CONFLICT,
        body: json!({
            "error":reason.into(),
            "remedy":format!("Nothing was sent. Correct what is reported, then {} again.", verb(direction)),
        }),
    }
}

/// The daemon answered with a refusal and a fresh reading shows nothing moved.
pub(super) fn refused(direction: Act, reason: String) -> Ended {
    Ended {
        outcome: "refused",
        status: StatusCode::CONFLICT,
        body: json!({
            "error":reason,
            "remedy":format!("The {} was refused and nothing moved. Correct what is reported, then try again.", verb(direction)),
        }),
    }
}

/// Nothing says how the request ended, or a reading shows that something
/// moved before a refusal.
pub(super) fn unknown(direction: Act, reason: String) -> Ended {
    let moved = match direction {
        Act::Push => {
            "The push may have published some or all of its changes. Read the remote's standing before pushing again."
        }
        Act::Pull => {
            "The pull may have admitted some or all of the remote's changes. Read the remote's standing and recent changes before pulling again."
        }
        Act::Fetch => {
            "The fetch may have brought in some or all of the remote's changes. Read the branches before fetching again."
        }
    };
    Ended {
        outcome: "unknown",
        status: StatusCode::BAD_GATEWAY,
        body: json!({
            "error":reason,
            "remedy":format!("{moved} This request was not retried."),
        }),
    }
}

/// What the page needs to know about how the working copy followed a pull.
pub(super) fn workspace_json(workspace: &WorkspaceFollow) -> Value {
    match workspace {
        WorkspaceFollow::Unreported => json!({"state":"unreported"}),
        WorkspaceFollow::NotApplicable { .. } => json!({"state":"not_applicable"}),
        WorkspaceFollow::AlreadyCurrent { .. } => json!({"state":"already_current"}),
        WorkspaceFollow::Advanced { .. } => json!({"state":"followed"}),
        WorkspaceFollow::Behind { detail } => json!({
            "state":"behind",
            "summary":"The pulled changes are recorded, and this working copy still shows what it showed before.",
            "detail":detail,
        }),
    }
}

fn reviews_json(response: &CommandTransferResponse) -> Value {
    use kin_remote::collaboration_transfer::CollaborationTransfer;
    match &response.outcome.collaboration {
        None => Value::Null,
        Some(CollaborationTransfer::InSync) => json!({"state":"in_sync"}),
        Some(CollaborationTransfer::PeerUnsupported { .. }) => json!({"state":"not_exchanged",
            "summary":"The remote does not exchange review records."}),
        Some(CollaborationTransfer::Skipped { .. }) => json!({"state":"not_exchanged"}),
        Some(CollaborationTransfer::Exchanged { carried, gaps, .. }) => json!({
            "state":"exchanged","carried":carried,"gaps":gaps.len(),
        }),
        Some(CollaborationTransfer::Refused { .. }) => json!({"state":"not_exchanged",
            "summary":"Review records were not exchanged. Any history this moved is durable."}),
    }
}

/// Read a settled answer into the outcome the page sees.
async fn answered(
    direction: Act,
    response: &CommandTransferResponse,
    client: &DaemonClient,
    repository_id: &RepositoryId,
    before: Option<SemanticChangeId>,
) -> Ended {
    let head = match (&response.outcome.plan, response.outcome.final_receipt()) {
        (_, Some(receipt)) => Some(receipt.destination_head),
        (RepositoryTransferPlan::UpToDate { head }, None) => *head,
        (RepositoryTransferPlan::FastForward { .. }, None) => None,
    };
    let moved = response.outcome.moved_history();
    let mut body = json!({
        "head":head.map(|head| head.to_string()),
        "packs":response.outcome.receipts.len(),
        "reviews":reviews_json(response),
    });
    if direction == Act::Pull {
        body["workspace"] = workspace_json(&response.workspace);
    }
    if !moved {
        return Ended {
            outcome: "nothing_to_send",
            status: StatusCode::OK,
            body,
        };
    }
    let counted = match (before, head) {
        (Some(before), Some(head)) => local_range(client, repository_id, before, head).await,
        _ => None,
    };
    let changes = match &response.outcome.plan {
        RepositoryTransferPlan::FastForward {
            change_count: Some(count),
            ..
        } => Some(*count),
        _ => counted.map(|count| count.ahead),
    };
    // Entities are counted along one line of history. Between two branches
    // that each moved, the difference is not what arrived.
    let counted = counted.filter(|count| count.behind == 0);
    body["changes"] = json!(changes);
    body["entities"] = json!(counted.map(|count| count.entities));
    body["entity_changes"] = counted.as_ref().map(entity_changes).unwrap_or(Value::Null);
    Ended {
        outcome: match direction {
            Act::Push => "pushed",
            Act::Pull => "pulled",
            Act::Fetch => "fetched",
        },
        status: StatusCode::OK,
        body,
    }
}

/// Why a sent transfer has no answer, in words. The transport's own error
/// is for a log: it names the daemon's records and the commands that inspect
/// them, and a page reader acts on neither.
fn lost_reason(direction: Act, error: &anyhow::Error) -> String {
    let timed_out = format!("{error:#}").contains("timed out");
    if timed_out {
        format!(
            "This repository's daemon did not answer the {} before the app stopped waiting.",
            verb(direction)
        )
    } else {
        format!(
            "The connection to this repository's daemon ended before it answered the {}.",
            verb(direction)
        )
    }
}

/// Settle a dispatch the daemon refused or whose answer was lost, from a
/// reading taken after it. `before` and `after` are the head the transfer
/// would move: the remote's for a push, this repository's for a pull or a
/// fetch. A lost
/// answer is never settled as refused, because the daemon may still be
/// running it.
fn settle_unanswered(
    direction: Act,
    dispatch: &TransferDispatch,
    before: Option<SemanticChangeId>,
    after: Option<Option<SemanticChangeId>>,
) -> Ended {
    match dispatch {
        TransferDispatch::Answered(_) => {
            unreachable!("an answered transfer is settled from its answer")
        }
        TransferDispatch::Lost(error) => unknown(direction, lost_reason(direction, error)),
        TransferDispatch::Refused { status, body } => match after {
            Some(after) if after == before => refused(direction, daemon_reason(*status, body)),
            Some(_) => unknown(
                direction,
                format!(
                    "The {} was refused after the branch had already moved: {}",
                    verb(direction),
                    daemon_reason(*status, body)
                ),
            ),
            None => unknown(
                direction,
                format!(
                    "The {} was refused, and whether anything moved first could not be read: {}",
                    verb(direction),
                    daemon_reason(*status, body)
                ),
            ),
        },
    }
}

/// The branch a transfer moves: the one the page names, or the current one.
fn transfer_ref(reference: Option<&str>, local: &LocalBranch) -> Result<RefName, String> {
    match reference.map(str::trim).filter(|value| !value.is_empty()) {
        Some(value) => {
            if value.len() > 512 {
                return Err("This branch name is too long.".to_string());
            }
            let name = super::repository_authority::parse_ref_name(value)
                .map_err(|error| format!("This is not a branch name: {error}"))?;
            if !name.is_branch() {
                return Err("Choose a branch below refs/heads/.".to_string());
            }
            Ok(name)
        }
        None => local.branch.clone().ok_or_else(|| {
            "This workspace is on no branch, so name the branch to move.".to_string()
        }),
    }
}

/// The branch a fetch lands on: the one the page names, or the remote's name
/// then the branch's. Never the branch it is fetched for, nor the one this
/// workspace is on: a pull is what moves those.
fn fetch_destination(
    named: Option<&str>,
    remote: &str,
    branch: &RefName,
    local: &LocalBranch,
) -> Result<RefName, String> {
    let name = match named.map(str::trim).filter(|name| !name.is_empty()) {
        Some(name) => name.to_string(),
        None => tracking_name(remote, branch),
    };
    if name.len() > 512 {
        return Err("This branch name is too long.".to_string());
    }
    let destination = super::repository_authority::parse_ref_name(&name)
        .map_err(|error| format!("This is not a branch name: {error}"))?;
    if !destination.is_branch() {
        return Err("Choose a branch below refs/heads/.".to_string());
    }
    if &destination == branch || local.branch.as_ref() == Some(&destination) {
        return Err(
            "A fetch brings the remote's branch in beside this one, so it needs a branch of \
             another name. A pull is what moves this branch."
                .to_string(),
        );
    }
    Ok(destination)
}

/// A push to a Git remote, or the refusal of anything else against one.
async fn git_transfer(
    state: &Arc<UiState>,
    body: &TransferBody,
    direction: Act,
    remote: &Remote,
    history: Option<&str>,
) -> Ended {
    if direction == Act::Fetch {
        return not_sent(direction, "Git updates advance the current branch. Fetching a Git branch beside it is not supported here.");
    }
    if let Some(reason) = history {
        return not_sent(direction, reason);
    }
    if let Err(reason) = super::workspace_git_remote::ready(state).await {
        return not_sent(direction, reason);
    }
    let client = match review_ui::daemon(state).await {
        Ok(client) => client,
        Err(_) => return not_sent(direction, "This repository's daemon did not answer."),
    };
    let local = match local_branch(&client).await {
        Ok(local) => local,
        Err(_) => {
            return not_sent(
                direction,
                "This repository's daemon did not answer the branch reading.",
            )
        }
    };
    let branch = match transfer_ref(body.reference.as_deref(), &local) {
        Ok(branch) => branch,
        Err(reason) => return not_sent(direction, reason),
    };
    let (mut ended, sent) = match direction {
        Act::Push => {
            super::workspace_git_remote::publish(
                state,
                body.operation_id,
                &client,
                &local,
                &branch,
                remote,
            )
            .await
        }
        Act::Pull => {
            super::workspace_git_remote::update(
                state,
                body.operation_id,
                &client,
                &local,
                &branch,
                remote,
            )
            .await
        }
        Act::Fetch => unreachable!("Git fetch refused before dispatch"),
    };
    ended.body["remote"] = json!(remote.name);
    ended.body["ref"] = json!(branch.to_string());
    ended.body["branch"] = json!(short_ref(&branch));
    ended.body["sent"] = json!(sent);
    ended
}

/// Run one push, pull or fetch to its end and say how it ended.
async fn run_transfer(state: &Arc<UiState>, body: &TransferBody, direction: Act) -> Ended {
    let machine = machine_repositories(state).await;
    let read = {
        let layout = state.layout.clone();
        tokio::task::spawn_blocking(move || {
            let history = kin_core::history_state::standing(&layout);
            (read_remotes(&layout, &machine), history_reason(&history))
        })
        .await
    };
    let (remotes, history) = match read {
        Ok((Ok(remotes), history)) => (remotes, history),
        Ok((Err(reason), _)) => return not_sent(direction, reason),
        Err(error) => {
            return not_sent(
                direction,
                format!("The remotes reading did not finish: {error}"),
            )
        }
    };
    let name = match body
        .remote
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        Some(name) => name.to_string(),
        None => match &remotes.default {
            Ok(name) => name.clone(),
            Err(reason) => return not_sent(direction, reason.clone()),
        },
    };
    let Some(remote) = remotes.remotes.iter().find(|remote| remote.name == name) else {
        return not_sent(
            direction,
            format!("No remote named {name} is set up for this repository."),
        );
    };
    if !remote.native {
        return git_transfer(state, body, direction, remote, history.as_deref()).await;
    }
    if let Err(reason) = usable(remote, history.as_deref()) {
        return not_sent(direction, reason);
    }
    let Ok(peer) = remote.peer.clone() else {
        return not_sent(direction, "This remote cannot be addressed.");
    };
    let client = match review_ui::daemon(state).await {
        Ok(client) => client,
        Err(_) => return not_sent(direction, "This repository's daemon did not answer."),
    };
    let local = match local_branch(&client).await {
        Ok(local) => local,
        Err(_) => {
            return not_sent(
                direction,
                "This repository's daemon did not answer the branch reading.",
            )
        }
    };
    let branch = match transfer_ref(body.reference.as_deref(), &local) {
        Ok(branch) => branch,
        Err(reason) => return not_sent(direction, reason),
    };
    // Where the remote's history lands here: on the branch itself, or for a
    // fetch on a branch beside it.
    let destination = match direction {
        Act::Fetch => {
            match fetch_destination(body.into.as_deref(), &remote.name, &branch, &local) {
                Ok(destination) => destination,
                Err(reason) => return not_sent(direction, reason),
            }
        }
        Act::Push | Act::Pull => branch.clone(),
    };
    if let Credential::ThisMachine { layout, .. } = &peer.credential {
        if DaemonClient::from_running_workspace(peer.base_url.clone(), layout)
            .await
            .is_err()
        {
            return not_sent(
                direction,
                "The Kin repository on this machine that this address named does not answer there now.",
            );
        }
    }
    // What the transfer would move, read before it is sent so a refusal can
    // be told apart from a transfer that moved something and then stopped.
    let remote_before = match advertisement(&peer, &local.repository_id).await {
        Ok(advertised) => advertised_head(&advertised, &branch),
        Err(error) => return not_sent(direction, remote_reason(&error)),
    };
    let local_before = local.head_of(&destination);
    let before = match direction {
        Act::Push => remote_before,
        Act::Pull | Act::Fetch => local_before,
    };
    let named = |mut ended: Ended, sent: bool| {
        ended.body["remote"] = json!(remote.name);
        ended.body["ref"] = json!(branch.to_string());
        ended.body["branch"] = json!(short_ref(&branch));
        if direction == Act::Fetch {
            ended.body["into"] = json!(destination.to_string());
            ended.body["into_branch"] = json!(short_ref(&destination));
        }
        ended.body["sent"] = json!(sent);
        ended
    };
    // A side that holds nothing on the branch has nothing to move, and the
    // negotiation would only refuse to plan it. Nothing is sent for either.
    let nothing_here = |summary: &str| Ended {
        outcome: "nothing_to_send",
        status: StatusCode::OK,
        body: json!({"head":local_before.map(|head| head.to_string()),"packs":0,"summary":summary}),
    };
    match direction {
        Act::Pull | Act::Fetch if remote_before.is_none() => {
            return named(
                nothing_here("The remote holds nothing on this branch."),
                false,
            );
        }
        Act::Push if local_before.is_none() => {
            let ended = if local.branch.as_ref() == Some(&branch) {
                nothing_here("This repository holds nothing on this branch yet.")
            } else {
                not_sent(
                    direction,
                    format!(
                        "This repository has no branch named {}.",
                        short_ref(&branch)
                    ),
                )
            };
            return named(ended, false);
        }
        _ => {}
    }

    state.transfers.mark_sent(body.operation_id);
    let mut request = transfer_request(&peer, &branch);
    request.destination_ref = Some(destination.clone());
    let dispatch = client.transfer_once(direction.direction(), &request).await;
    let ended = match &dispatch {
        TransferDispatch::Answered(response) => {
            // A branch brought in for the first time is counted against the
            // branch it was fetched for: what it holds that this one does not.
            let counted_from = match direction {
                Act::Fetch => before.or_else(|| local.head_of(&branch)),
                Act::Push | Act::Pull => before,
            };
            answered(
                direction,
                response,
                &client,
                &local.repository_id,
                counted_from,
            )
            .await
        }
        TransferDispatch::Lost(_) => settle_unanswered(direction, &dispatch, before, None),
        TransferDispatch::Refused { .. } => {
            let after = match direction {
                Act::Push => advertisement(&peer, &local.repository_id)
                    .await
                    .ok()
                    .map(|advertised| advertised_head(&advertised, &branch)),
                Act::Pull | Act::Fetch => local_branch(&client)
                    .await
                    .ok()
                    .map(|local| local.head_of(&destination)),
            };
            let settled = settle_unanswered(direction, &dispatch, before, after);
            // A pull refused because this repository already holds what the
            // remote has moved nothing and had nothing to move.
            let holds_remote = match (direction, settled.outcome, remote_before, before) {
                (Act::Pull | Act::Fetch, "refused", Some(remote), Some(here)) => {
                    local_range(&client, &local.repository_id, remote, here)
                        .await
                        .is_some_and(|count| count.behind == 0)
                }
                _ => false,
            };
            if holds_remote {
                Ended {
                    outcome: "nothing_to_send",
                    status: StatusCode::OK,
                    body: json!({"head":before.map(|head| head.to_string()),"packs":0,
                        "summary":"This repository already holds everything the remote has on this branch."}),
                }
            } else {
                settled
            }
        }
    };
    named(ended, true)
}

/// `POST /api/workspace/push`, `/pull` and `/fetch`.
async fn transfer(
    state: Arc<UiState>,
    headers: HeaderMap,
    body: TransferBody,
    direction: Act,
) -> Response {
    if !review_ui::may_write(&headers, &state) {
        return review_ui::write_refused();
    }
    let operation_id = body.operation_id;
    let answer = |ended: Ended| {
        let mut body = ended.body;
        body["operation_id"] = json!(operation_id);
        body["outcome"] = json!(ended.outcome);
        // Whether this request reached the daemon, said on every answer.
        if body.get("sent").is_none() {
            body["sent"] = json!(false);
        }
        (ended.status, Json(body)).into_response()
    };
    if operation_id.0.is_nil() {
        return answer(not_sent(direction, "A nonempty operation ID is required."));
    }
    if direction != Act::Fetch && body.into.is_some() {
        return answer(not_sent(
            direction,
            "Only a fetch names a branch to bring the remote's branch in as.",
        ));
    }
    let Ok(running) = state.transfers.running.clone().try_lock_owned() else {
        return answer(not_sent(
            direction,
            "Another push, pull or fetch is running for this repository.",
        ));
    };
    if let Some(earlier) = state.transfers.earlier(operation_id) {
        let mut ended = Ended {
            outcome: "refused",
            status: StatusCode::CONFLICT,
            body: json!({
                "error":"This operation was already sent once, and an operation is never sent twice.",
                "remedy":"Read the remote's standing to see how it ended, then start a new operation if there is more to move.",
                "already_sent":true,
            }),
        };
        ended.body["earlier_outcome"] = json!(earlier);
        return answer(ended);
    }
    // Run to its end whatever becomes of this request, holding the one-at-a-
    // time guard until then. A page that closes mid-transfer must not leave
    // the daemon's answer unread.
    let running_state = state.clone();
    let ended = tokio::spawn(async move {
        let _running = running;
        let ended = run_transfer(&running_state, &body, direction).await;
        if ended.body.get("sent").and_then(Value::as_bool) == Some(true) {
            running_state
                .transfers
                .settle(body.operation_id, ended.outcome);
        }
        ended
    })
    .await
    .unwrap_or_else(|error| {
        unknown(
            direction,
            format!("The {} did not finish: {error}", verb(direction)),
        )
    });
    answer(ended)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(byte: u8) -> SemanticChangeId {
        SemanticChangeId::from_hash(kin_model::Hash256::from_bytes([byte; 32]))
    }

    /// A partial history is said as what Kin holds, and the rest is said to
    /// be on its way only while a derivation reads it or has read it.
    #[test]
    fn a_partial_history_says_what_is_held_and_claims_no_reading_that_is_not_happening() {
        use kin_core::history_state::{
            DerivationState, HistoryDerivationProgress, HistoryMode, HistoryStanding,
            HistoryStateRecord,
        };
        let record = HistoryStateRecord::new(
            HistoryMode::Background,
            "a".repeat(40),
            Some("refs/heads/main".into()),
            "b".repeat(64),
            Vec::new(),
            Some(4_673),
            None,
        );
        let standing = |progress| HistoryStanding::Partial {
            record: record.clone(),
            progress,
        };
        let held = "Kin holds 1 of this repository's 4,673 commits";
        let idle = history_reason(&standing(None)).unwrap();
        assert_eq!(
            idle,
            format!("{held}, so it cannot be pushed or pulled until it holds all of it.")
        );
        assert!(!idle.contains("filled in"), "{idle}");
        // A derivation whose worker is gone is not reading anything.
        let mut stopped = HistoryDerivationProgress::new(DerivationState::Running);
        stopped.pid = Some(i32::MAX.unsigned_abs());
        assert_eq!(history_reason(&standing(Some(stopped))).unwrap(), idle);
        let mut running = HistoryDerivationProgress::new(DerivationState::Running);
        running.pid = Some(std::process::id());
        assert_eq!(
            history_reason(&standing(Some(running))).unwrap(),
            format!(
                "{held} and is reading the rest now, so it cannot be pushed or pulled until it \
                 holds all of it."
            )
        );
        let staged = history_reason(&standing(Some(HistoryDerivationProgress::new(
            DerivationState::Staged,
        ))))
        .unwrap();
        assert!(staged.contains("has read the rest"), "{staged}");
        assert_eq!(history_reason(&HistoryStanding::Complete), None);
        let uncounted = HistoryStanding::Partial {
            record: HistoryStateRecord {
                reachable_commits: None,
                ..record.clone()
            },
            progress: None,
        };
        assert!(history_reason(&uncounted)
            .unwrap()
            .starts_with("Kin holds only part of this repository's history,"));
        assert_eq!(
            [0, 999, 1_000, 1_234_567].map(grouped),
            ["0", "999", "1,000", "1,234,567"]
        );
    }

    #[test]
    fn an_operation_is_remembered_once_sent_and_its_outcome_once_settled() {
        let transfers = Transfers::default();
        let operation = OperationId::new();
        assert_eq!(transfers.earlier(operation), None);
        transfers.mark_sent(operation);
        assert_eq!(transfers.earlier(operation), Some(None));
        transfers.settle(operation, "pushed");
        assert_eq!(transfers.earlier(operation), Some(Some("pushed")));
        assert_eq!(transfers.earlier(OperationId::new()), None);
        // The memory is bounded, oldest first out.
        for _ in 0..SENT_KEPT {
            transfers.mark_sent(OperationId::new());
        }
        assert_eq!(transfers.earlier(operation), None);
    }

    /// A lost answer is unknown whatever a later reading says, because the
    /// daemon may still be running the transfer.
    #[test]
    fn a_lost_answer_is_unknown_and_never_refused() {
        for direction in [Act::Push, Act::Pull] {
            let lost = TransferDispatch::Lost(anyhow::anyhow!("connection reset"));
            for after in [None, Some(Some(head(1))), Some(None)] {
                let ended = settle_unanswered(direction, &lost, Some(head(1)), after);
                assert_eq!(ended.outcome, "unknown");
                assert_eq!(ended.status, StatusCode::BAD_GATEWAY);
                assert!(ended.body["remedy"]
                    .as_str()
                    .unwrap()
                    .contains("This request was not retried."));
                let error = ended.body["error"].as_str().unwrap();
                assert!(error.contains("ended before it answered"), "{error}");
                assert!(!error.contains("connection reset"), "{error}");
            }
        }
    }

    /// A refusal is refused only when a reading taken after it shows the head
    /// where it was. A moved head, or one that cannot be read, is unknown.
    #[test]
    fn a_refusal_is_refused_only_when_nothing_moved() {
        let refusal = TransferDispatch::Refused {
            status: 409,
            body: "remote head x on refs/heads/main is not an ancestor of local head y".into(),
        };
        let direction = Act::Push;
        let same = settle_unanswered(direction, &refusal, Some(head(1)), Some(Some(head(1))));
        assert_eq!(same.outcome, "refused");
        assert_eq!(same.status, StatusCode::CONFLICT);
        assert!(same.body["error"]
            .as_str()
            .unwrap()
            .contains("holds changes this repository does not"));
        let unborn = settle_unanswered(direction, &refusal, None, Some(None));
        assert_eq!(unborn.outcome, "refused");
        let moved = settle_unanswered(direction, &refusal, Some(head(1)), Some(Some(head(2))));
        assert_eq!(moved.outcome, "unknown");
        let unread = settle_unanswered(direction, &refusal, Some(head(1)), None);
        assert_eq!(unread.outcome, "unknown");
    }

    #[test]
    fn a_daemon_refusal_is_said_in_words_without_a_command() {
        let credential = daemon_reason(
            424,
            "repository transfer storage failed: remote refused ref advertisement (HTTP 401): \
             {\"error\":\"Authentication required\"}; send a bearer token ... run `kin auth login`",
        );
        assert_eq!(credential, "The remote refused the credential Kin sent.");
        assert!(!credential.contains('`'));
        assert_eq!(
            daemon_reason(
                424,
                "remote transfer status transport failed: connection refused"
            ),
            "The remote could not be reached."
        );
        assert_eq!(daemon_reason(500, "  "), "Kin refused with HTTP 500.");
        assert_eq!(
            daemon_reason(
                409,
                "remote refused transfer export with a repository-v6 conflict (HTTP 409): \
                 destination head a is not an ancestor of source head b"
            ),
            DIVERGED_PULL
        );
    }

    #[test]
    fn a_semantic_diff_counts_changes_and_entities() {
        let diff = json!({
            "ahead":2,"behind":0,
            "entities":[
                {"change_type":"added"},{"change_type":"modified"},
                {"change_type":"modified"},{"change_type":"removed"},
            ],
        });
        assert_eq!(
            range_count(&diff),
            Some(RangeCount {
                ahead: 2,
                behind: 0,
                entities: 4,
                added: 1,
                modified: 2,
                removed: 1,
            })
        );
        assert_eq!(range_count(&json!({"ahead":1})), None);
    }

    /// A remote head this repository holds is never something a pull
    /// brings, and only a head it does not hold is the remote's to count.
    #[test]
    fn a_pull_is_settled_here_before_the_remote_is_asked() {
        let count = |ahead, behind| RangeCount {
            ahead,
            behind,
            entities: 2,
            ..RangeCount::default()
        };
        // The remote is behind: its head is an ancestor of this one.
        assert_eq!(
            pull_settled_here(&Ok(count(3, 0))).unwrap().state,
            "nothing_to_bring"
        );
        assert_eq!(
            pull_settled_here(&Ok(count(1, 1))).unwrap().state,
            "blocked"
        );
        assert_eq!(
            pull_settled_here(&Err(Some("no answer".into())))
                .unwrap()
                .state,
            "unknown"
        );
        assert!(pull_settled_here(&Err(None)).is_none());

        let brings = pull_settled_there(Ok(count(2, 0)));
        assert_eq!(brings.state, "would_bring");
        assert_eq!(brings.changes, Some(2));
        assert_eq!(brings.entities.unwrap().entities, 2);
        assert_eq!(pull_settled_there(Ok(count(2, 1))).state, "blocked");
        assert_eq!(pull_settled_there(Err(None)).state, "blocked");
        assert_eq!(
            pull_settled_there(Err(Some("no answer".into()))).state,
            "unknown"
        );
    }

    #[test]
    fn the_two_sides_make_one_standing() {
        let side = |state| Side::new(state);
        assert_eq!(
            overall(&side("nothing_to_send"), &side("nothing_to_bring")),
            "up_to_date"
        );
        assert_eq!(
            overall(&side("would_send"), &side("nothing_to_bring")),
            "ahead"
        );
        assert_eq!(overall(&side("blocked"), &side("would_bring")), "behind");
        assert_eq!(overall(&side("blocked"), &side("blocked")), "diverged");
        assert_eq!(
            overall(&side("blocked"), &side("nothing_to_bring")),
            "blocked"
        );
        assert_eq!(
            overall(&side("unknown"), &side("nothing_to_bring")),
            "unknown"
        );
        let mut push = side("would_send");
        push.changes = Some(1);
        push.entities = Some(RangeCount {
            ahead: 1,
            entities: 3,
            ..RangeCount::default()
        });
        assert_eq!(
            summary("ahead", &push, &side("nothing_to_bring")),
            "A push would send 1 change touching 3 entities."
        );
    }

    #[test]
    fn the_default_is_the_configured_remote_then_the_only_kin_one_then_the_only_git_one() {
        let remote = |name: &str, native: bool| Remote {
            name: name.into(),
            host: RemoteHostKind::Peer,
            native,
            default: false,
            address: None,
            peer: Err(String::new()),
        };
        let both = [remote("origin", false), remote("peer", true)];
        assert_eq!(default_remote(Some("origin"), &both), Ok("origin".into()));
        assert_eq!(default_remote(Some("gone"), &both), Ok("peer".into()));
        assert_eq!(default_remote(None, &both), Ok("peer".into()));
        let two = [remote("a", true), remote("b", true)];
        assert_eq!(default_remote(Some("b"), &two), Ok("b".into()));
        assert!(default_remote(None, &two)
            .unwrap_err()
            .contains("none is the default"));
        assert_eq!(
            default_remote(None, &[remote("origin", false)]),
            Ok("origin".into())
        );
        assert!(
            default_remote(None, &[remote("origin", false), remote("fork", false)])
                .unwrap_err()
                .contains("2 Git remotes")
        );
        assert_eq!(
            default_remote(None, &[]),
            Err("No remote is set up for this repository.".into())
        );
    }

    /// A fetch lands beside the branch it is for, never on it or on the
    /// branch this workspace is on.
    #[test]
    fn a_fetch_lands_on_a_branch_of_another_name() {
        let main = RefName::branch(b"main").unwrap();
        let local = LocalBranch {
            repository_id: RepositoryId::new("fetch-destination").unwrap(),
            branch: Some(main.clone()),
            branches: vec![(main.clone(), Some(head(1)))],
        };
        assert_eq!(
            fetch_destination(None, "peer", &main, &local).unwrap(),
            RefName::branch(b"peer/main").unwrap()
        );
        assert_eq!(
            fetch_destination(Some(" theirs "), "peer", &main, &local).unwrap(),
            RefName::branch(b"theirs").unwrap()
        );
        for refused in ["main", "refs/heads/main"] {
            assert!(fetch_destination(Some(refused), "peer", &main, &local)
                .unwrap_err()
                .contains("a branch of another name"));
        }
        assert!(
            fetch_destination(Some("refs/tags/v1"), "peer", &main, &local)
                .unwrap_err()
                .contains("refs/heads/")
        );
        assert_eq!(Act::Fetch.direction(), RepositoryTransferDirection::Pull);
        assert_eq!(verb(Act::Fetch), "fetch");
    }

    /// What a blocked side can do next travels with it.
    #[test]
    fn a_side_says_how_it_moves_again() {
        let mut pull = Side::new("blocked").because(DIVERGED_PULL);
        assert_eq!(pull.json()["resolve"], Value::Null);
        pull.resolve = Some(("fetch_then_merge", "peer/main".into()));
        assert_eq!(pull.json()["resolve"], "fetch_then_merge");
        assert_eq!(pull.json()["resolve_branch"], "peer/main");
        assert_eq!(
            tracking_name("peer", &RefName::branch(b"topic/a").unwrap()),
            "peer/topic/a"
        );
    }

    #[test]
    fn an_address_is_shown_without_its_credentials() {
        assert_eq!(
            shown_address(Some("https://alice:s3cr3t@example.com/team")).as_deref(),
            Some("https://example.com/team")
        );
        assert_eq!(shown_address(Some("  ")), None);
        assert_eq!(shown_address(None), None);
    }

    #[test]
    fn a_peer_needs_an_address_kin_sends_a_credential_to() {
        assert!(native_peer(RemoteHostKind::Peer, None, &[])
            .err()
            .unwrap()
            .contains("no transfer address"));
        assert!(
            native_peer(RemoteHostKind::Peer, Some("http://peer.example:4219"), &[])
                .err()
                .unwrap()
                .contains("not one Kin sends a credential to")
        );
        let peer = native_peer(RemoteHostKind::Peer, Some("http://127.0.0.1:4010/"), &[]).unwrap();
        assert_eq!(peer.base_url, "http://127.0.0.1:4010");
        assert_eq!(peer.organization_id, None);
    }

    /// A fake daemon that counts the connections it is sent and answers each
    /// with `reply`, or with nothing when `reply` is `None`.
    async fn fake_daemon(
        reply: Option<&'static str>,
    ) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = seen.clone();
        let mut identity =
            kin_core::runtime_identity::RuntimeServiceIdentity::current("hosted-daemon").unwrap();
        identity.namespace = None;
        let identity_body = serde_json::to_string(&identity).unwrap();
        let identity_reply = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            identity_body.len(),
            identity_body
        );
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut buffer = vec![0_u8; 64 * 1024];
                let n = socket.read(&mut buffer).await.unwrap_or(0);
                if buffer[..n].starts_with(b"GET /runtime/identity ") {
                    let _ = socket.write_all(identity_reply.as_bytes()).await;
                    continue;
                }
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if let Some(reply) = reply {
                    let _ = socket.write_all(reply.as_bytes()).await;
                }
                // Dropping the socket with no reply is an answer lost after
                // the request was sent.
            }
        });
        (address, seen)
    }

    fn sample_request() -> CommandTransferRequest {
        CommandTransferRequest {
            remote_base_url: "http://127.0.0.1:9".into(),
            remote_token: None,
            remote_organization_id: None,
            repository_id: None,
            source_ref: Some(RefName::branch(b"main").unwrap()),
            destination_ref: Some(RefName::branch(b"main").unwrap()),
        }
    }

    /// The no-retry rule at the transport: a push whose answer is lost is
    /// sent exactly once and reported lost, never as a refusal, and a refusal
    /// is kept apart from a lost answer.
    #[tokio::test]
    async fn a_transfer_is_sent_once_and_a_lost_answer_is_not_sent_again() {
        let (address, seen) = fake_daemon(None).await;
        let client =
            DaemonClient::from_base_url_with_explicit_authority(address, None, None).unwrap();
        for direction in [Act::Push, Act::Pull] {
            let before = seen.load(std::sync::atomic::Ordering::SeqCst);
            let dispatch = client
                .transfer_once(direction.direction(), &sample_request())
                .await;
            assert!(
                matches!(dispatch, TransferDispatch::Lost(_)),
                "{dispatch:?}"
            );
            // Give a retry, were there one, the time to arrive.
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert_eq!(seen.load(std::sync::atomic::Ordering::SeqCst), before + 1);
        }

        let (address, seen) = fake_daemon(Some(
            "HTTP/1.1 409 Conflict\r\ncontent-type: text/plain\r\ncontent-length: 8\r\nconnection: close\r\n\r\nconflict",
        ))
        .await;
        let client =
            DaemonClient::from_base_url_with_explicit_authority(address, None, None).unwrap();
        let dispatch = client
            .transfer_once(Act::Push.direction(), &sample_request())
            .await;
        match dispatch {
            TransferDispatch::Refused { status, body } => {
                assert_eq!(status, 409);
                assert_eq!(body, "conflict");
            }
            other => panic!("a refusal is the daemon's answer, not a lost one: {other:?}"),
        }
        assert_eq!(seen.load(std::sync::atomic::Ordering::SeqCst), 1);

        // A success whose body cannot be read is lost, not answered.
        let (address, _) = fake_daemon(Some(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}",
        ))
        .await;
        let client =
            DaemonClient::from_base_url_with_explicit_authority(address, None, None).unwrap();
        let dispatch = client
            .transfer_once(Act::Pull.direction(), &sample_request())
            .await;
        assert!(
            matches!(dispatch, TransferDispatch::Lost(_)),
            "{dispatch:?}"
        );
    }

    /// The session's capability is read from configuration, credentials and
    /// what Git said: a repository with no remote can do neither, and one
    /// with only a Git remote can publish when Git can carry the branch, and
    /// update only while its current lineage supports exact Git admission.
    #[test]
    fn a_git_remote_can_update_only_when_exact_admission_is_available() {
        let complete = kin_core::history_state::HistoryStanding::Complete;
        let git_ready = Ok(());
        let temporary = tempfile::TempDir::new().unwrap();
        let layout = KinLayout::new(temporary.path().join(".kin"));
        let git_pull = Err(super::super::workspace_git_remote::GIT_PULL_UNAVAILABLE.to_string());
        let (push, pull, _) = transfer_capability(&layout, &complete, &[], &git_ready, &git_pull);
        for none in [push, pull] {
            assert_eq!(
                none,
                Err("No remote is set up for this repository.".to_string())
            );
        }

        std::fs::create_dir_all(layout.root()).unwrap();
        let mut config = KinConfig::default();
        config.git.remotes.push(kin_core::GitRemoteTransportConfig {
            name: "origin".into(),
            fetch_urls: vec!["https://github.invalid/example/demo.git".into()],
            push_urls: Vec::new(),
            fetch_refspecs: Vec::new(),
            push_refspecs: Vec::new(),
        });
        config.save(&layout.config_path()).unwrap();
        let remotes = read_remotes(&layout, &[]).unwrap();
        assert_eq!(remotes.remotes.len(), 1);
        assert!(!remotes.remotes[0].native);
        assert_eq!(
            remotes.remotes[0].address.as_deref(),
            Some("https://github.invalid/example/demo.git")
        );
        assert_eq!(remotes.default, Ok("origin".to_string()));

        let (push, pull, _) = transfer_capability(&layout, &complete, &[], &git_ready, &git_pull);
        assert_eq!(push, Ok(()));
        let never = pull.unwrap_err();
        assert!(never.contains("Kin-native changes"), "{never}");
        assert!(!never.contains('`'), "{never}");

        // Without Git, or without a Git repository at the project, nothing
        // can be published either, and the reason is Git's.
        let no_git = Err("Git is not on this machine.".to_string());
        let (push, _, _) = transfer_capability(&layout, &complete, &[], &no_git, &git_pull);
        assert_eq!(push, no_git);
        let (_, pull, fetch) = transfer_capability(&layout, &complete, &[], &git_ready, &Ok(()));
        assert_eq!(pull, Ok(()));
        assert!(!fetch);
    }

    /// A native remote is usable only with an address, a complete history
    /// and a credential Kin can read without asking.
    #[test]
    fn a_native_remote_needs_a_credential_and_a_complete_history() {
        let remote = |credential| Remote {
            name: "peer".into(),
            host: RemoteHostKind::Peer,
            native: true,
            default: true,
            address: Some("http://127.0.0.1:4010".into()),
            peer: Ok(Peer {
                base_url: "http://127.0.0.1:4010".into(),
                organization_id: None,
                credential,
            }),
        };
        assert!(usable(&remote(Credential::Environment("token".into())), None).is_ok());
        let missing = usable(&remote(Credential::None), None).unwrap_err();
        assert!(
            missing.contains("holds no credential for peer"),
            "{missing}"
        );
        let partial = usable(
            &remote(Credential::Environment("token".into())),
            Some("Kin holds 1 of this repository's 4,673 commits."),
        )
        .unwrap_err();
        assert!(partial.contains("Kin holds 1 of"));
        // A credential's label never carries the credential.
        assert_eq!(Credential::Saved("s3cr3t".into()).label(), "saved");
    }
}
