// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
};

use kin_model::graph::GraphStore;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::{Duration, Instant};

use crate::budget::ResponseBudget;
use crate::daemon_delegate;
use crate::envelope::{self, Envelope};
use crate::error::{McpError, Result};
use crate::handlers::{handle_tool_call, RequestRepositoryAuthority};
use crate::session::SessionRegistry;
use crate::startup_binding::{StartupBindingState, StartupDaemonBinding};
use crate::types::*;

/// MCP server configuration.
#[derive(Debug, Clone)]
pub struct McpServerConfig {
    pub server_name: String,
    pub server_version: String,
    pub allowed_tools: Option<HashSet<String>>,
    pub session_authority_mode: SessionAuthorityMode,
    pub snapshot_path: Option<PathBuf>,
    /// Startup-pinned local authority for the explicit offline runtime.
    ///
    /// Product daemon mode dispatches inside the daemon and supplies its own
    /// retained binding; it never populates this stdio-side field.
    pub repository_authority: Option<RequestRepositoryAuthority>,
    /// This server is the curated `agent-default` belt.
    ///
    /// Two behaviours hang off it, and they are one concept: the belt serves the
    /// short descriptions and trimmed schemas rather than the registered long
    /// forms, and it asks `semantic_locate` for the compact response shape on
    /// behalf of the agents it serves.
    ///
    /// Separate from `allowed_tools` because three profiles filter and only one
    /// of them is this belt. `benchmark` and `context-bench` keep the long forms
    /// and the shared payload deliberately: their bytes are an input to a
    /// citable result, and a benchmark number must not move because a
    /// description was rewritten or a payload was narrowed.
    pub agent_belt: bool,
    /// The routed surface this connection serves, when it serves one.
    ///
    /// A routed connection lists the one tool [`crate::routed::TOOL_NAME`] and
    /// reaches every other tool through its commands; `allowed_tools` holds that
    /// one name, so a named tool called directly is refused with the command
    /// that runs it here.
    pub routed: Option<crate::routed::RoutedSurface>,
    /// Whether entity bodies are served with each line marked by its offset in
    /// the entity, as [`crate::entity_lines`] presents them.
    ///
    /// Set only for a profile with no Kin write path, and cleared for any
    /// client that asks for exact bodies when it connects, as `kin agent run`
    /// does. Everything that restates a body as the base of an edit is served
    /// its exact bytes.
    pub number_entity_lines: bool,
    /// Whether answers are served in plain words, as [`crate::plain_reply`]
    /// renders them: one plain verdict, what is left to check, and none of
    /// the working the verdict was computed from.
    ///
    /// Set for the agent profiles, and cleared when the launcher is asked for
    /// machine replies ([`crate::plain_reply::MACHINE_REPLIES_ENV`]), which is
    /// how tests and acceptance scripts read the codes on these profiles. A
    /// citable connection is never rendered.
    pub plain_replies: bool,
    /// Opt-in trace evidence aliases on the three explicit compact query
    /// profiles. Independent of read-only permissions and source numbering;
    /// machine and citable replies always bypass this presentation.
    pub compact_trace_evidence: bool,
    /// Optional query detail choices remembered only for this connection's
    /// opaque cursors, with bounded storage and the page cache's lifetime.
    pub query_presentation: std::sync::Arc<crate::query_reply::QueryPresentation>,
    /// Whether this connection's bytes are an input to a citable result.
    ///
    /// `benchmark` and `context-bench` are served the instructions a published
    /// number was measured under, byte for byte, for the same reason
    /// `agent_belt` leaves their descriptions and payloads alone.
    pub citable: bool,
    /// The folder the client works in: its first workspace root once it names
    /// one, and until then the launch directory the launcher records here.
    ///
    /// An answer from a repository that is not this folder says so, and
    /// `kin_init` sets this folder up when it is named nothing else.
    pub client_root: Option<PathBuf>,
    /// How a path is resolved before two are compared, supplied by the
    /// launcher. Resolving symlinks reads the filesystem, which this crate
    /// leaves to its launcher; the default compares paths as given.
    pub canonicalize: fn(&Path) -> PathBuf,
    /// The write session this connection opens for its client on the first
    /// write that names none. See [`crate::connection_session`].
    pub connection_session: std::sync::Arc<crate::connection_session::ConnectionSession>,
    /// The MCP protocol version this connection negotiated at `initialize`,
    /// [`FALLBACK_PROTOCOL_VERSION`] until then.
    pub protocol_version: &'static str,
    /// Short handles for the UUIDs, cursors and tickets this connection
    /// serves, read back as the values on every call. Set by a long-lived
    /// stdio server on a plain-reply profile; see [`crate::session_handles`].
    pub session_handles: Option<std::sync::Arc<crate::session_handles::SessionHandles>>,
    /// How a plain reply is written: JSON, or compact text
    /// ([`crate::text_reply`]). Read only when `session_handles` is set.
    pub reply_format: crate::text_reply::ReplyFormat,
}

impl McpServerConfig {
    /// Serve this connection's plain replies with short session handles, in
    /// the reply format `KIN_MCP_REPLY_FORMAT` names. A long-lived stdio
    /// server calls this once; a one-shot `kin call` never does, since its
    /// handles could not be read back by a later process.
    pub fn serve_session_handles(&mut self) {
        if self.plain_replies && !self.citable {
            self.session_handles = Some(Default::default());
            self.reply_format = crate::text_reply::ReplyFormat::from_env();
        }
    }

    /// The handles to apply to this connection's replies, while its replies
    /// are plain.
    fn active_handles(&self) -> Option<&crate::session_handles::SessionHandles> {
        if !self.plain_replies || self.citable {
            return None;
        }
        self.session_handles.as_deref()
    }

    /// A `tools/call` message with every handle this connection served read
    /// back as its value; any other message unchanged.
    pub fn expand_handles(&self, message: String) -> String {
        match self.active_handles() {
            Some(handles) => handles.expand_request(&message).unwrap_or(message),
            None => message,
        }
    }

    /// Serve this connection exact entity bodies, whatever its profile would
    /// present. Called when a client asks for them at `initialize`.
    pub fn serve_exact_entity_bodies(&mut self) {
        self.number_entity_lines = false;
        if let Some(surface) = self.routed.as_mut() {
            surface.numbered = false;
        }
    }

    /// Serve this connection only the tools that read: the query half of its
    /// profile, or of the full surface, and the routed surface without its
    /// write path. Called when a client declares at `initialize` that it only
    /// reads, see [`READ_ONLY_CAPABILITY`].
    ///
    /// This decides what the connection lists and dispatches, never what a
    /// write may do: the store's permissions stay the authority for that on
    /// every connection, a read-only one included.
    pub fn serve_reads_only(&mut self) {
        if let Some(surface) = self.routed.as_mut() {
            surface.writes = false;
            return;
        }
        let surface = crate::tools::surface_definitions();
        let reads: std::collections::HashSet<String> = surface
            .tools
            .iter()
            .filter(|tool| tool.annotations.read_only_hint)
            .filter(|tool| {
                self.allowed_tools
                    .as_ref()
                    .is_none_or(|allowed| crate::verbs::allows(allowed, &tool.name))
            })
            .map(|tool| tool.name.clone())
            .collect();
        self.allowed_tools = Some(reads);
    }

    /// Read what an `initialize` request asks of the connection.
    fn apply_client_capabilities(&mut self, initialize: &serde_json::Value) {
        if client_declares_read_only(initialize) {
            self.serve_reads_only();
        }
        if crate::entity_lines::client_wants_exact_bodies(initialize) {
            self.serve_exact_entity_bodies();
        }
        if crate::plain_reply::client_wants_machine_replies(initialize) {
            self.plain_replies = false;
        }
        self.connection_session.set_client(
            crate::connection_session::ClientIdentity::from_initialize(initialize),
        );
        self.protocol_version = negotiate_protocol_version(
            initialize
                .pointer("/params/protocolVersion")
                .and_then(serde_json::Value::as_str),
        );
    }
}

/// How the stdio server should present session authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionAuthorityMode {
    /// The daemon must own session state. Local registry fallback is disabled.
    DaemonRequired,
    /// The local in-process registry is only a fallback for offline/test use.
    OfflineFallback,
}

impl SessionAuthorityMode {
    pub fn uses_daemon(self) -> bool {
        matches!(self, Self::DaemonRequired)
    }

    pub fn requires_daemon(self) -> bool {
        matches!(self, Self::DaemonRequired)
    }
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            server_name: "kin-mcp".into(),
            server_version: env!("CARGO_PKG_VERSION").into(),
            allowed_tools: None,
            session_authority_mode: SessionAuthorityMode::DaemonRequired,
            snapshot_path: None,
            repository_authority: None,
            agent_belt: false,
            routed: None,
            number_entity_lines: false,
            plain_replies: false,
            compact_trace_evidence: false,
            query_presentation: Default::default(),
            citable: false,
            client_root: None,
            canonicalize: Path::to_path_buf,
            connection_session: Default::default(),
            protocol_version: FALLBACK_PROTOCOL_VERSION,
            session_handles: None,
            reply_format: Default::default(),
        }
    }
}

pub trait PersistableMcpStore: GraphStore {
    /// A process-local selected-graph identity and completed truth revision.
    /// Stores without a revision cannot attest a resumable trace snapshot.
    fn trace_page_authority(&self) -> Option<serde_json::Value> {
        None
    }

    fn persist_primary_snapshot(&self, snapshot_path: Option<&Path>) -> Result<()> {
        let _ = snapshot_path;
        Ok(())
    }

    /// The UUID of every entity whose id, written as hex digits without
    /// hyphens, starts with `prefix`: how a short id an agent holds is read
    /// back with no connection state ([`crate::entity_address`]).
    ///
    /// The default lists every entity, which copies the graph once. A store
    /// that can walk its ids without copying owes its callers an override.
    fn entity_ids_with_prefix(&self, prefix: &str) -> std::result::Result<Vec<String>, String> {
        Ok(self
            .list_all_entities()
            .map_err(|error| error.to_string())?
            .into_iter()
            .map(|entity| entity.id.to_string())
            .filter(|id| crate::entity_address::id_starts_with(id, prefix))
            .collect())
    }
}

impl PersistableMcpStore for kin_db::InMemoryGraph {
    fn entity_ids_with_prefix(&self, prefix: &str) -> std::result::Result<Vec<String>, String> {
        let mut ids = Vec::new();
        self.for_each_entity(|entity| {
            let id = entity.id.to_string();
            if crate::entity_address::id_starts_with(&id, prefix) {
                ids.push(id);
            }
        });
        Ok(ids)
    }

    fn trace_page_authority(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "graph": format!("{self:p}"), "root": self.compute_root_hash(),
            "revision": self.truth_epoch(),
        }))
    }

    fn persist_primary_snapshot(&self, snapshot_path: Option<&Path>) -> Result<()> {
        let Some(snapshot_path) = snapshot_path else {
            return Ok(());
        };
        self.flush_text_index().map_err(McpError::graph)?;
        let snapshot = self.to_snapshot();
        let text_index_path = snapshot_path
            .parent()
            .map(|parent| parent.join("text-index"))
            .ok_or_else(|| McpError::Other("snapshot path has no parent directory".into()))?;
        let manager = kin_db::SnapshotManager::new(snapshot_path.to_path_buf());
        let graph = kin_db::InMemoryGraph::from_snapshot_with_text_index(snapshot, text_index_path)
            .map_err(McpError::graph)?;
        manager.swap(graph);
        manager.save().map_err(McpError::graph)?;
        Ok(())
    }
}

/// Run the in-process MCP server over stdio (stdin/stdout).
///
/// This is the explicit offline/test runtime. Product `kin mcp start` uses
/// [`run_stdio_daemon`], which never receives a graph store and cannot fall
/// through to local graph handlers.
pub async fn run_stdio<G: PersistableMcpStore + 'static>(
    store: G,
    mut config: McpServerConfig,
) -> Result<()> {
    if !config.session_authority_mode.requires_daemon() && config.repository_authority.is_none() {
        config.repository_authority =
            crate::handlers::repository_authority::discover_for_process()?;
    }
    let sessions = SessionRegistry::new();
    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let reader = BufReader::new(stdin);
    let mut reader = reader;

    tracing::info!("kin-mcp stdio server starting");
    match config.session_authority_mode {
        SessionAuthorityMode::DaemonRequired => {
            tracing::info!(
                "kin-mcp session authority: daemon-required; local registry fallback is disabled"
            );
        }
        SessionAuthorityMode::OfflineFallback => {
            tracing::info!(
                "kin-mcp session authority: explicit offline test mode; local registry is authoritative for this run"
            );
        }
    }

    while let Some((message, framed)) = read_stdio_message(&mut reader).await? {
        let message = config.expand_handles(message);
        if message.contains("\"initialize\"") {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&message) {
                if value.get("method").and_then(|method| method.as_str()) == Some("initialize") {
                    config.apply_client_capabilities(&value);
                }
            }
        }
        if let Some(response) = process_message(&message, &store, &config, &sessions).await {
            let response = present_for_client(response, &message, &config);
            let response_json = serde_json::to_string(&response).map_err(McpError::Json)?;
            write_stdio_message(&mut stdout, &response_json, framed).await?;
        }
    }

    tracing::info!("kin-mcp stdio server shutting down");
    Ok(())
}

/// The repository this MCP process serves: its working directory and the URL of
/// the daemon that owns its graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundRepo {
    /// Repository working directory (the parent of its `.kin/`).
    pub root: PathBuf,
    /// Daemon endpoint serving that repository.
    pub daemon_url: String,
}

/// What the binder made of the MCP client's advertised workspace roots.
///
/// The two failure shapes are kept apart because they call for opposite
/// behavior. A root the server can see and identify as a Kin repository is
/// evidence about which codebase the client is asking about; a root the server
/// cannot resolve at all is evidence about nothing, because a containerised or
/// remote server reaches its repository over a boundary the client does not
/// share and the client's paths never exist in the server's namespace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceBinding {
    /// The process is bound to this repository, either freshly or because the
    /// roots still contain the repository it already served.
    Bound(BoundRepo),
    /// The roots name Kin repositories this server can see, and it is not
    /// serving any of them: either an operator pin refuses to follow the
    /// client, or the daemon for the repository the client moved to could not
    /// be resolved. The client really is looking at another codebase.
    OtherRepository(Vec<PathBuf>),
    /// None of the roots names a Kin repository this server can see. The client
    /// and the server describe the filesystem in different namespaces, so the
    /// roots say nothing about which repository this server should serve.
    Unresolvable,
}

/// Binds — or re-binds — the repo daemon from the MCP client's advertised
/// workspace roots.
///
/// Receives the filesystem paths of the client's roots and binds the daemon for
/// the first one that is a Kin repository (setting `KIN_DAEMON_URL` as a side
/// effect), reporting what it found as a [`WorkspaceBinding`]. Supplied by the
/// kin-cli MCP command; when the binder itself is `None`, roots binding is
/// disabled (the client cannot serve roots).
///
/// The binder is invoked for every `roots/list` response, not only the first:
/// an editor that moves its window to another folder announces the change, and
/// a server that keeps its original binding answers from a repository the user
/// has left. A [`WorkspaceBinding::OtherRepository`] return while a repository
/// is already bound is therefore meaningful — it tells the server to refuse tool
/// calls rather than serve them from the previous repository's graph.
pub type RepoBinder = Box<
    dyn Fn(Vec<PathBuf>) -> Pin<Box<dyn Future<Output = WorkspaceBinding> + Send>> + Send + Sync,
>;

/// The JSON-RPC id the server uses for its own `roots/list` request so it can
/// recognize the matching response coming back from the client.
const ROOTS_REQUEST_ID: &str = "kin-mcp-roots-list";

/// Run the daemon-required MCP server over stdio.
///
/// This mode is intentionally graphless: every `tools/call` request is
/// forwarded to the repo daemon, which executes against its live graph and
/// session coordinator. The stdio process only handles JSON-RPC framing,
/// initialization, tool listing, allow-list checks, and transport errors.
///
/// When no repository was bound at startup (no `--repo`/`KIN_MCP_REPO` and the
/// launch cwd is not inside a Kin repo — the common case for editors that spawn
/// MCP servers from `$HOME`) and the client advertises the MCP `roots`
/// capability, the server requests `roots/list` after initialization and binds
/// the daemon to the open workspace via `repo_binder`. That is what lets Cursor,
/// Windsurf, and other editors reach whatever repository the user has open
/// without a hardcoded path in the MCP config.
///
/// The client's workspace can move afterwards, so a `roots/list_changed`
/// notification re-requests roots whether or not a repository is bound, and the
/// binder decides: it re-binds the process to the repository the client is now
/// looking at, or reports that the roots name a different Kin repository it will
/// not follow, in which case every `tools/call` is refused with a structured
/// repo-mismatch error until a later roots change resolves it. A bound server
/// never keeps answering from a repository the client has left — a confident
/// answer about the wrong codebase is worse than an error.
///
/// Roots this server cannot resolve at all are the other case, and it is not a
/// workspace change. A server registered as `docker exec -w <repo> ... kin mcp
/// start`, or reached over any other boundary, is bound to a repository by its
/// own cwd or `--repo`/`KIN_MCP_REPO` while the client announces host paths that
/// never exist in the server's namespace. Those roots are evidence about
/// nothing, so a server holding its own binding keeps serving it and records the
/// disagreement at info instead of refusing every call for the life of the
/// process.
pub async fn run_stdio_daemon(
    config: McpServerConfig,
    repo_binder: Option<RepoBinder>,
    startup: Option<std::sync::Arc<StartupDaemonBinding>>,
) -> Result<()> {
    daemon_delegate::capture_request_runtime();
    if !config.session_authority_mode.requires_daemon() {
        return Err(McpError::Other(
            "daemon stdio mode requires daemon session authority".to_string(),
        ));
    }

    let mut stdout = tokio::io::stdout();
    let mut reader = BufReader::new(client_input_until_stopped());

    // This process is an MCP session from here on: it holds an idle floor on
    // whichever daemon it forwards to, so an attached daemon started with a
    // shorter window cannot idle out between this session's tool calls.
    crate::session_idle_floor::enable();

    let served = run_stdio_daemon_over(
        &mut reader,
        &mut stdout,
        config,
        repo_binder,
        bound_daemon_url_from_env(),
        startup,
    )
    .await;

    // The client closed the session. Hand the daemon back its own idle policy
    // now, rather than one floor from now.
    crate::session_idle_floor::release().await;
    // Stopped by a signal: the session has ended, so leave now rather than
    // wait on the read of stdin the runtime would otherwise hold for.
    let stopped = STOP_CODE.load(std::sync::atomic::Ordering::Acquire);
    if stopped != 0 {
        std::process::exit(stopped);
    }
    served
}

/// The exit code a stop signal asked for, or 0 while none has arrived.
static STOP_CODE: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

/// The client's input, which ends when the client closes it or when this
/// process is asked to stop.
///
/// Most clients stop an MCP server with a signal rather than by closing its
/// input. Read as the end of input, a stop lets the loop end the connection's
/// session on its way out, as a closed connection does, so the agent is not
/// shown as connected after it has gone. A stop that cannot finish in a few
/// seconds, behind a tool call still running, exits as the signal would have.
fn client_input_until_stopped() -> tokio::io::DuplexStream {
    let (mut feed, input) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        let mut stdin = tokio::io::stdin();
        tokio::select! {
            _ = tokio::io::copy(&mut stdin, &mut feed) => {}
            code = stop_requested() => {
                tracing::info!("kin-mcp asked to stop; ending this connection's session");
                STOP_CODE.store(code, std::sync::atomic::Ordering::Release);
                // A thread of its own, so a runtime held up by a pending read
                // of stdin cannot keep the process from leaving.
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_secs(3));
                    std::process::exit(code);
                });
            }
        }
        // Dropping `feed` ends the loop's input.
    });
    input
}

/// Resolves with the exit code a stop signal asks for: SIGTERM, SIGHUP or
/// SIGINT. Never resolves where those signals cannot be watched.
async fn stop_requested() -> i32 {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let watch = |kind: SignalKind| signal(kind).ok();
        let (mut term, mut hup, mut int) = (
            watch(SignalKind::terminate()),
            watch(SignalKind::hangup()),
            watch(SignalKind::interrupt()),
        );
        async fn next(signal: &mut Option<tokio::signal::unix::Signal>) {
            match signal {
                Some(signal) => {
                    signal.recv().await;
                }
                None => std::future::pending::<()>().await,
            }
        }
        tokio::select! {
            _ = next(&mut term) => 128 + 15,
            _ = next(&mut hup) => 128 + 1,
            _ = next(&mut int) => 128 + 2,
        }
    }
    #[cfg(not(unix))]
    {
        std::future::pending::<i32>().await
    }
}

/// Transport-generic core of [`run_stdio_daemon`].
///
/// Split out so the binding lifecycle — the roots request, the response that
/// completes it, the re-request after a workspace change, and the refusal that
/// follows a change we cannot follow — is exercised over an in-memory transport
/// instead of only over the process's real stdin/stdout.
///
/// `bound_daemon_url` is the daemon this process was already bound to before the
/// loop started (from `--repo`/`KIN_MCP_REPO`/cwd), passed in rather than read
/// from the environment so the loop's binding decisions are a function of its
/// inputs.
async fn run_stdio_daemon_over<R, W>(
    reader: &mut R,
    writer: &mut W,
    mut config: McpServerConfig,
    repo_binder: Option<RepoBinder>,
    bound_daemon_url: Option<String>,
    startup: Option<std::sync::Arc<StartupDaemonBinding>>,
) -> Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    tracing::info!("kin-mcp daemon-proxy stdio server starting");

    // MCP `roots` binding state. Before a repository is bound we reach out for
    // workspace roots as soon as the client says it can serve them; afterwards
    // only a roots *change* triggers another request. An editor may initialize
    // the shared MCP process before opening a workspace, so only suppress a
    // request while one is actually in flight.
    let mut client_supports_roots = false;
    let mut roots_request_state = WorkspaceRootsRequestState::default();
    let mut binding = RepoBindingState::started_with(bound_daemon_url);
    // The folder the client works in before any roots arrive: the launch
    // directory the launcher recorded.
    let launch_root = config.client_root.clone();
    // The background open of the connection's session `initialize` started,
    // stopped if the client leaves before it finishes.
    let mut opening_session: Option<tokio::task::JoinHandle<()>> = None;

    while let Some((message, framed)) = read_stdio_message(&mut *reader).await? {
        let message = config.expand_handles(message);
        // Where this message, when it is a `tools/call` whose client asked for
        // progress, reports how far its wait for the daemon has come.
        let mut call_progress: Option<CallProgressNotifier> = None;
        // Whether this message is a `tools/call` the daemon answers, which is
        // the only kind that may open the connection's session on the way.
        let mut forwards_to_daemon = false;

        // The launcher's startup binding runs behind this loop so `initialize`
        // and `tools/list` are answered immediately. Once it settles with a
        // bound daemon, fold that into the roots-binding bookkeeping so a
        // later roots change compares against the repository actually served.
        if let Some(startup) = startup.as_ref() {
            if !binding.is_bound() {
                if let StartupBindingState::Bound(bound) = startup.snapshot() {
                    binding.bind(bound, BindingOrigin::Server);
                }
            }
        }

        // Peek at the raw JSON so we can distinguish the client's requests and
        // notifications from the `roots/list` response we may have sent: a
        // response carries no `method`, which the strongly-typed
        // `JsonRpcRequest` requires.
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&message) {
            let method = value.get("method").and_then(|m| m.as_str());
            if method == Some("tools/call") {
                call_progress = CallProgressNotifier::for_request(&value);
            }

            if method == Some("initialize") {
                client_supports_roots = value.pointer("/params/capabilities/roots").is_some();
                config.apply_client_capabilities(&value);
                if let Some(earlier) = opening_session.take() {
                    earlier.abort();
                }
                opening_session = open_session_at_initialize(&config, startup.clone());
            }

            // Whether `--repo`/`KIN_MCP_REPO` pinned this server's repository.
            // Read per message rather than once: the startup binding settles
            // behind this loop, so the pin is not knowable when it starts.
            let repo_pinned = startup
                .as_ref()
                .is_some_and(|startup| startup.pinned_by_operator());

            // A `tools/call` racing the launcher's startup binding waits for
            // the daemon to become ready, within the call's own readiness
            // budget, telling a client that asked for progress how far the
            // daemon has come while it waits. Only a budget that truly runs
            // out gets the honest still-starting answer, which says what is
            // still loading and how far along it is. Never a remedy-flavored
            // "no daemon" error while one is on its way up.
            //
            // The tool registry is exempt, because it reads no graph. Waiting on
            // the binding would answer "the daemon is still starting" to the one
            // question no daemon can answer better, and admitting a daemon spawn
            // for it would undo FIR-3099: an agent that asked only what tools
            // exist would open the store and schedule a full embedding pass.
            // A routed `describe`, or a routed call that will be refused, reads
            // no graph either.
            let call_reads_the_graph = value
                .pointer("/params/name")
                .and_then(|name| name.as_str())
                .map(crate::agent_belt::canonical_tool_name)
                != Some(crate::handlers::tool_search::TOOL_NAME)
                && !crate::routed::answers_locally(&value, config.routed);
            forwards_to_daemon = method == Some("tools/call") && call_reads_the_graph;
            if method == Some("tools/call") && call_reads_the_graph {
                if let Some(startup) = startup.as_ref() {
                    // The first ask for a graph answer, and the only thing that
                    // admits starting a daemon (FIR-3099). Before this line
                    // every bind path attaches to a daemon that is already
                    // serving and starts none, so a session that never calls a
                    // tool never opens the store and never runs an embedding
                    // pass. Set before the wait, because the wait is what the
                    // launcher's binding task is sitting on.
                    startup.admit_daemon_spawn();
                    if let Some(waited) = await_startup_binding(
                        startup,
                        daemon_delegate::readiness_budget(),
                        call_progress.as_mut(),
                        &mut *writer,
                        framed,
                    )
                    .await?
                    {
                        if let Some(response) = startup_pending_response(&value, startup, waited) {
                            let response = present_for_client(response, &message, &config);
                            let response_json =
                                serde_json::to_string(&response).map_err(McpError::Json)?;
                            write_stdio_message(&mut *writer, &response_json, framed).await?;
                        }
                        continue;
                    }
                    if !binding.is_bound() {
                        if let StartupBindingState::Bound(bound) = startup.snapshot() {
                            binding.bind(bound, BindingOrigin::Server);
                        }
                    }
                    // Roots the pre-call binder resolved to a Kin repository it
                    // was not yet allowed to start a daemon for. Put them
                    // through the binder again now that it is. This is the
                    // editor-launched-from-$HOME shape, where the client's roots
                    // are the only thing naming a repository and the launch
                    // directory settles unbound at once, so nothing was waited
                    // for above. After the settle, not before it, so a startup
                    // binding that is about to bind the same repository wins and
                    // no second daemon is raced for it.
                    if !binding.is_bound() {
                        let deferred = binding.deferred_roots();
                        if !deferred.is_empty() {
                            if let Some(binder) = repo_binder.as_ref() {
                                apply_workspace_roots(binder, deferred, &mut binding, repo_pinned)
                                    .await;
                            }
                        }
                    }
                }
            }

            // The client moved to a workspace we could not bind. Refuse every
            // tool call until the root becomes bindable: answering from the
            // repository the client left would return a confident, well-formed
            // result about the wrong codebase.
            //
            // The verdict is re-derived here rather than read out of the state
            // it was recorded in. It was computed once, when the roots changed,
            // and nothing about the announced root is fixed: `kin init` can run
            // there, a mount can appear, a container path can be made to exist.
            // A cached refusal outlives every one of those, so a server that
            // was right at roots-change time keeps refusing a workspace it
            // could now serve, and the only recovery is restarting the process.
            // Re-binding costs nothing that matters, because every call in this
            // state is being refused anyway.
            if method == Some("tools/call") && binding.is_mismatched() {
                if let Some(binder) = repo_binder.as_ref() {
                    let roots = binding.mismatched_roots();
                    if !roots.is_empty() {
                        apply_workspace_roots(binder, roots, &mut binding, repo_pinned).await;
                    }
                }
                if binding.is_mismatched() {
                    if let Some(response) = binding.repo_mismatch_response(&value) {
                        let response = present_for_client(response, &message, &config);
                        let response_json =
                            serde_json::to_string(&response).map_err(McpError::Json)?;
                        write_stdio_message(&mut *writer, &response_json, framed).await?;
                    }
                    continue;
                }
            }

            // Our own `roots/list` response returning from the client: bind the
            // daemon and swallow it (a response is never itself answered).
            if method.is_none()
                && value.get("id").and_then(|id| id.as_str()) == Some(ROOTS_REQUEST_ID)
            {
                roots_request_state.complete();
                let roots = parse_workspace_roots(&value);
                // The folder the client works in is its first root from here
                // on, which is what an answer from another repository is
                // compared to and what `kin_init` sets up by default.
                if !roots.is_empty() {
                    config.client_root = crate::first_contact::client_root(
                        &roots,
                        launch_root.as_deref(),
                        config.canonicalize,
                    );
                }
                if let Some(binder) = repo_binder.as_ref() {
                    apply_workspace_roots(binder, roots, &mut binding, repo_pinned).await;
                }
                continue;
            }

            // Ask the client for its workspace roots: while nothing is bound,
            // as soon as it finishes initializing or asks for the tool list
            // (retrying after an earlier empty response); and whenever it
            // reports a roots change, bound or not, because that is how an
            // editor announces that its window moved to another folder.
            if roots_request_state.begin_if_allowed(
                method,
                client_supports_roots,
                repo_binder.is_some(),
                binding.wants_workspace_roots(),
                Instant::now(),
            ) {
                let request = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": ROOTS_REQUEST_ID,
                    "method": "roots/list",
                });
                let request_json = serde_json::to_string(&request).map_err(McpError::Json)?;
                write_stdio_message(&mut *writer, &request_json, framed).await?;
                // Fall through: `initialized` has no response, and `tools/list`
                // is still answered normally below.
            }
        }

        // Every call this connection forwards names its session, so the
        // daemon records it against the agent that made it. A call that will
        // reach the daemon opens the session if initialize could not.
        let session_tag = if forwards_to_daemon {
            read_session(&config).await
        } else {
            config.connection_session.current().await
        }
        .map(daemon_delegate::ConnectionSessionTag::new);
        let response = daemon_delegate::with_connection_session(session_tag.clone(), async {
            match call_progress.as_mut() {
                Some(notifier) => {
                    forward_reporting_progress(&message, &config, notifier, &mut *writer, framed)
                        .await
                }
                None => Ok(process_daemon_message(&message, &config).await),
            }
        })
        .await?;
        // The daemon no longer holds the session: reaped after its idle
        // window, or lost to a restart. Open a new one now, so the agent is
        // shown again from its next call.
        if let Some(tag) = session_tag.filter(daemon_delegate::ConnectionSessionTag::gone) {
            config.connection_session.forget(tag.id()).await;
            // Detached: it finishes on its own, and a client that leaves first
            // leaves a session the daemon's idle rule reaps.
            let _reopening = open_session_in_background(&config);
        }
        if let Some(mut response) = response {
            if is_tools_call(&message) {
                stamp_client_folder(&mut response, binding.repo_root.as_deref(), &config);
            }
            let response = present_for_client(response, &message, &config);
            let response_json = serde_json::to_string(&response).map_err(McpError::Json)?;
            write_stdio_message(&mut *writer, &response_json, framed).await?;
        }
    }

    // The client closed the connection, so the session it wrote in ends with
    // it. A server that dies without getting here leaves the session to the
    // daemon's idle rule, once the process its session names is gone.
    if let Some(opening) = opening_session.take() {
        opening.abort();
    }
    end_connection_session(&config).await;
    tracing::info!("kin-mcp daemon-proxy stdio server shutting down");
    Ok(())
}

/// Whether the launcher named the session this process works in
/// (`KIN_SESSION_ID`, set by `kin with` and `kin exec`). That session is the
/// agent's, every forwarded call already names it, and no second one is
/// opened for reads.
fn launcher_names_session() -> bool {
    std::env::var("KIN_SESSION_ID").is_ok_and(|id| !id.trim().is_empty())
}

/// Ask the daemon to open a session with `start`, returning its id.
async fn open_daemon_session(
    start: HashMap<String, serde_json::Value>,
) -> std::result::Result<String, String> {
    let answer = daemon_delegate::forward_tool_call("kin_session_start", &start)
        .await?
        .ok_or_else(|| "no Kin daemon is serving this repository".to_string())?;
    let answer = serde_json::to_value(&answer).map_err(|error| error.to_string())?;
    crate::connection_session::started_session_id(&answer).ok_or_else(|| {
        let text = answer
            .pointer("/content/0/text")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        format!("the daemon opened no session: {text}")
    })
}

/// Open the connection's session for reads with `start`.
#[cfg(not(test))]
async fn open_read_session(
    session: &crate::connection_session::ConnectionSession,
    start: HashMap<String, serde_json::Value>,
) -> Option<String> {
    session.id_for_reads(|| open_daemon_session(start)).await
}

/// In a test, a connection that set ids is handed its next one and nothing
/// reaches a daemon; one that set none opens nothing.
#[cfg(test)]
async fn open_read_session(
    session: &crate::connection_session::ConnectionSession,
    _start: HashMap<String, serde_json::Value>,
) -> Option<String> {
    if !session.in_test_mode() {
        return None;
    }
    if let Some(held) = session.current().await {
        return Some(held);
    }
    let next = session.next_test_id();
    session
        .id_for_reads(|| async move { next.ok_or_else(|| "no test session id".to_string()) })
        .await
}

/// The connection's session for a call the daemon answers, opening it when
/// initialize could not. `None` when the launcher named the session, the
/// connection has no folder yet, or the daemon refused.
async fn read_session(config: &McpServerConfig) -> Option<String> {
    if launcher_names_session() {
        return None;
    }
    if let Some(held) = config.connection_session.current().await {
        return Some(held);
    }
    let cwd = config.client_root.as_deref()?.display().to_string();
    let start = config.connection_session.read_start_arguments(&cwd);
    open_read_session(&config.connection_session, start).await
}

/// Open the connection's session in the background, so nothing the client
/// waits on pays for it.
fn open_session_in_background(config: &McpServerConfig) -> Option<tokio::task::JoinHandle<()>> {
    if launcher_names_session() {
        return None;
    }
    let cwd = config.client_root.as_deref()?;
    let start = config
        .connection_session
        .read_start_arguments(&cwd.display().to_string());
    let session = std::sync::Arc::clone(&config.connection_session);
    Some(tokio::spawn(async move {
        open_read_session(&session, start).await;
    }))
}

/// Open the connection's session once the client has initialized, so an agent
/// that only reads is shown too.
///
/// In the background, so the handshake answers at once, and only against a
/// daemon that is already serving: this waits for the launcher's binding and
/// never admits starting a daemon, which only a call that asks for a graph
/// answer may do. If the binding settles with no daemon, the first such call
/// opens the session on its way.
fn open_session_at_initialize(
    config: &McpServerConfig,
    startup: Option<std::sync::Arc<StartupDaemonBinding>>,
) -> Option<tokio::task::JoinHandle<()>> {
    if launcher_names_session() {
        return None;
    }
    let Some(startup) = startup else {
        // No launcher binding to wait on, which only a test runs without; a
        // test that set no session ids opens nothing.
        return if cfg!(test) {
            open_session_in_background(config)
        } else {
            None
        };
    };
    let cwd = config.client_root.as_deref()?;
    let start = config
        .connection_session
        .read_start_arguments(&cwd.display().to_string());
    let session = std::sync::Arc::clone(&config.connection_session);
    Some(tokio::spawn(async move {
        loop {
            match startup.snapshot() {
                StartupBindingState::Bound(_) => break,
                StartupBindingState::Unbound { .. } => return,
                StartupBindingState::Pending => {
                    startup.wait_until_settled(Duration::from_secs(600)).await;
                }
            }
        }
        open_read_session(&session, start).await;
    }))
}

/// End the write session this connection opened, if it opened one.
async fn end_connection_session(config: &McpServerConfig) {
    let Some(session) = config.connection_session.current().await else {
        return;
    };
    let arguments = HashMap::from([("session_id".to_string(), serde_json::json!(session))]);
    if let Err(error) = daemon_delegate::forward_tool_call("kin_session_end", &arguments).await {
        tracing::debug!("ending the connection's session {session} failed: {error}");
    }
}

/// Whether a raw message is a `tools/call` request.
fn is_tools_call(message: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(message)
        .ok()
        .and_then(|value| {
            value
                .get("method")
                .and_then(|m| m.as_str())
                .map(str::to_string)
        })
        .as_deref()
        == Some("tools/call")
}

/// Make a tool answer on a connection whose client works in a folder other
/// than the repository this server is bound to say so, first.
///
/// An answer the daemon's health reached already carries `_kin.repository` and
/// the warning at the head of `_kin.advice`. Every other answer, a refusal, a
/// routed answer given here, a delegate error, gets both here, so no answer on
/// such a connection reads as if it came from the client's own folder. An
/// answer that is not JSON gets the warning as its first line. The same folder,
/// or no bound repository, leaves the answer as it is.
fn stamp_client_folder(
    response: &mut JsonRpcResponse,
    repo_root: Option<&Path>,
    config: &McpServerConfig,
) {
    let Some(block) = response
        .result
        .as_mut()
        .and_then(|result| result.get_mut("content"))
        .and_then(serde_json::Value::as_array_mut)
        .and_then(|content| content.first_mut())
    else {
        return;
    };
    let Some(text) = block.get("text").and_then(serde_json::Value::as_str) else {
        return;
    };
    let parsed = serde_json::from_str::<serde_json::Value>(text);
    if let Ok(payload) = &parsed {
        if crate::trace_pages::is_page(payload) {
            // The daemon budgeted this exact repository/client presentation.
            // Compare only with the connection's client context, never with a
            // later daemon/binder root or a newer health observation.
            let client = config.client_root.as_deref().map(config.canonicalize);
            let captured = payload
                .pointer("/_kin/repository")
                .cloned()
                .and_then(|value| {
                    serde_json::from_value::<crate::first_contact::SemanticRepositoryIdentity>(
                        value,
                    )
                    .ok()
                });
            if payload
                .pointer("/_kin/semantic_surface")
                .and_then(serde_json::Value::as_u64)
                == Some(1)
                && captured
                    .is_some_and(|identity| identity.matches_client_context(client.as_deref()))
            {
                return;
            }
            // An older or mismatched daemon did not budget the needed warning.
            // Appending one now would overrun the page and falsify accounting.
            // Refuse this page rather than forwarding data with wrong identity.
            response.result = Some(
                serde_json::to_value(ToolCallResult::error(
                    "The daemon page lacks the captured semantic repository and client-context binding \
                     required by this connection. Use matching current Kin binaries and restart \
                     the query without a cursor.",
                ))
                .expect("tool error serializes"),
            );
            return;
        }
    }
    let (Some(root), Some(client)) = (repo_root, config.client_root.as_deref()) else {
        return;
    };
    let private_identity = crate::first_contact::repository_identity(
        &(config.canonicalize)(root),
        Some(&(config.canonicalize)(client)),
    );
    let Some(identity) = crate::first_contact::semantic_repository_identity(&private_identity)
    else {
        return;
    };
    let Some(warning) = identity.warning.clone() else {
        return;
    };
    let stamped = match parsed {
        Ok(serde_json::Value::Object(mut payload)) => {
            let key = crate::envelope::ENVELOPE_KEY;
            let mut kin = match payload.remove(key) {
                Some(serde_json::Value::Object(kin)) => kin,
                Some(_) => return,
                None => serde_json::Map::new(),
            };
            if kin.get("repository") == serde_json::to_value(&identity).ok().as_ref() {
                return;
            }
            let advice = match kin.remove("advice") {
                Some(serde_json::Value::String(existing)) => {
                    let existing = private_identity
                        .warning
                        .as_deref()
                        .and_then(|old| existing.strip_prefix(old))
                        .unwrap_or(&existing)
                        .trim();
                    if existing.is_empty() {
                        warning
                    } else {
                        format!("{warning} {existing}")
                    }
                }
                _ => warning,
            };
            kin.insert(
                "repository".to_string(),
                serde_json::to_value(&identity).unwrap_or_default(),
            );
            // Built in reading order, advice first and the envelope first,
            // whichever order the map keeps.
            let mut envelope = serde_json::Map::new();
            envelope.insert("advice".to_string(), serde_json::Value::String(advice));
            envelope.extend(kin);
            let mut ordered = serde_json::Map::new();
            ordered.insert(key.to_string(), serde_json::Value::Object(envelope));
            ordered.extend(payload);
            // Keep the wire format the answer already had: re-indenting a
            // compact answer would undo the call's `compact` choice after its
            // size was settled.
            match crate::budget::render_in_format_of(&serde_json::Value::Object(ordered), text) {
                Ok(text) => text,
                Err(_) => return,
            }
        }
        _ => format!("{warning}\n\n{text}"),
    };
    block["text"] = serde_json::Value::String(stamped);
}

/// Feed a `roots/list` response through the binder and record what it means for
/// this process: a fresh binding, an unchanged one, or a workspace the server
/// cannot follow.
async fn apply_workspace_roots(
    binder: &RepoBinder,
    roots: Vec<PathBuf>,
    binding: &mut RepoBindingState,
    repo_pinned: bool,
) {
    if roots.is_empty() {
        // No open folder is not the same as a different folder: there is no
        // other repository for the client's calls to be about, so an existing
        // binding is left alone rather than torn down.
        tracing::info!("kin-mcp: client returned no workspace roots; binding is unchanged");
        return;
    }

    let previous_url = binding.daemon_url.clone();
    match binder(roots.clone()).await {
        WorkspaceBinding::Bound(bound) => {
            if previous_url.as_deref() != Some(bound.daemon_url.as_str()) {
                // A different daemon serves this process now. Drop the revival
                // override, which pins delegate calls at a daemon this process
                // started for the repository it is leaving.
                daemon_delegate::clear_daemon_url_override();
                tracing::info!(
                    repo = %bound.root.display(),
                    daemon = %bound.daemon_url,
                    "kin-mcp: bound repo daemon from the client's workspace roots"
                );
            }
            binding.bind(bound, BindingOrigin::ClientRoots);
        }
        WorkspaceBinding::OtherRepository(repos) if binding.is_bound() => {
            tracing::warn!(
                roots = ?roots,
                repositories = ?repos,
                "kin-mcp: the client's workspace roots name a different Kin repository this server \
                 does not serve; refusing tool calls instead of answering from the bound repository"
            );
            binding.mark_mismatch(roots, repo_pinned);
        }
        // The client's roots resolve to nothing here. That is the shape of every
        // containerised or remote registration: the client names host paths and
        // the server lives in another namespace, so the roots carry no evidence
        // that the client left the repository this server was pinned to. A
        // server holding its own binding — its launch cwd, `--repo`, or
        // `KIN_MCP_REPO` — keeps serving it. A server whose only authority was a
        // previous roots answer has had that authority withdrawn, so it refuses.
        WorkspaceBinding::Unresolvable
            if binding.is_bound() && binding.origin == Some(BindingOrigin::ClientRoots) =>
        {
            tracing::warn!(
                roots = ?roots,
                "kin-mcp: the client's workspace roots changed to a workspace with no bindable Kin \
                 repository; refusing tool calls instead of answering from the previous repository"
            );
            binding.mark_mismatch(roots, repo_pinned);
        }
        WorkspaceBinding::Unresolvable if binding.is_bound() => {
            tracing::info!(
                roots = ?roots,
                repo = ?binding.repo_root,
                "kin-mcp: none of the client's workspace roots names a Kin repository this server \
                 can resolve; keeping the repository this server was started with"
            );
        }
        // Nothing is bound and the client named Kin repositories this server can
        // see. Before FIR-3099 that could only mean their daemons refused to
        // start, so there was nothing to retry. Now it is also what a bind that
        // is not yet allowed to start one looks like, so the roots are kept for
        // the first `tools/call` to put through the binder again.
        WorkspaceBinding::OtherRepository(repos) => {
            tracing::info!(
                repositories = ?repos,
                "kin-mcp: the client's workspace roots name a Kin repository with no daemon bound \
                 yet; the first tool call binds it"
            );
            binding.defer_roots(roots);
        }
        WorkspaceBinding::Unresolvable => {
            tracing::info!("kin-mcp: no Kin repository among the client's workspace roots");
        }
    }
}

/// The daemon bound before the stdio loop started (`KIN_DAEMON_URL` unset or
/// empty means nothing was bound).
fn bound_daemon_url_from_env() -> Option<String> {
    std::env::var("KIN_DAEMON_URL")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Where a binding came from, which decides how much authority the client's
/// workspace roots carry over it.
///
/// A binding this server made for itself — its launch cwd, `--repo`, or
/// `KIN_MCP_REPO` — is an operator decision that survives roots it cannot
/// resolve. A binding that came from a previous `roots/list` answer exists only
/// because the client said so, so the client withdrawing it is decisive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BindingOrigin {
    /// Bound before or beside the stdio loop from this server's own inputs.
    Server,
    /// Bound from a `roots/list` answer.
    ClientRoots,
}

/// What the stdio loop knows about which repository it serves.
#[derive(Debug, Default)]
struct RepoBindingState {
    /// Daemon currently serving this process, if any.
    daemon_url: Option<String>,
    /// Repository that daemon serves. Unknown for a binding inherited from
    /// startup, which arrives as a URL with no accompanying root.
    repo_root: Option<PathBuf>,
    /// Where the current binding came from. `None` while nothing is bound.
    origin: Option<BindingOrigin>,
    /// Set when the client's workspace moved somewhere this server could not
    /// follow. Cleared by the next successful bind.
    mismatch: Option<WorkspaceMismatch>,
    /// Roots that named a Kin repository this server can see while nothing was
    /// bound. Kept so the first `tools/call` can put them through the binder
    /// again once starting a daemon is admitted. Cleared by the next successful
    /// bind.
    deferred_roots: Vec<PathBuf>,
}

/// A workspace change the server could not follow: it is still bound to
/// `bound_repo`, while the client now reports `requested_roots`.
#[derive(Debug)]
struct WorkspaceMismatch {
    bound_repo: Option<PathBuf>,
    requested_roots: Vec<PathBuf>,
    /// Whether `--repo`/`KIN_MCP_REPO` pinned this server's repository, which
    /// decides which remedies the refusal is allowed to offer.
    repo_pinned: bool,
}

impl RepoBindingState {
    fn started_with(daemon_url: Option<String>) -> Self {
        Self {
            origin: daemon_url.is_some().then_some(BindingOrigin::Server),
            daemon_url,
            ..Self::default()
        }
    }

    fn is_bound(&self) -> bool {
        self.daemon_url.is_some()
    }

    /// Whether the server should still be reaching for workspace roots on the
    /// ordinary triggers. A refusing server counts as needing one: it is bound
    /// to a repository the client is not looking at, which is no more useful
    /// than being unbound, so it keeps trying to bind rather than waiting only
    /// for the client to announce another change.
    fn wants_workspace_roots(&self) -> bool {
        !self.is_bound() || self.is_mismatched()
    }

    fn is_mismatched(&self) -> bool {
        self.mismatch.is_some()
    }

    fn bind(&mut self, bound: BoundRepo, origin: BindingOrigin) {
        self.daemon_url = Some(bound.daemon_url);
        self.repo_root = Some(bound.root);
        self.origin = Some(origin);
        self.mismatch = None;
        self.deferred_roots.clear();
    }

    /// Remember roots that resolved to a Kin repository nothing is serving yet.
    fn defer_roots(&mut self, roots: Vec<PathBuf>) {
        self.deferred_roots = roots;
    }

    /// The roots waiting for a tool call to admit starting their daemon.
    ///
    /// Cloned rather than taken, so a call whose bind still fails leaves them
    /// for the next one. That costs a bind attempt per refused call, which is
    /// the same trade the mismatch retry above already makes and for the same
    /// reason: every call in this state is being refused anyway.
    fn deferred_roots(&self) -> Vec<PathBuf> {
        self.deferred_roots.clone()
    }

    fn mark_mismatch(&mut self, requested_roots: Vec<PathBuf>, repo_pinned: bool) {
        self.mismatch = Some(WorkspaceMismatch {
            bound_repo: self.repo_root.clone(),
            requested_roots,
            repo_pinned,
        });
    }

    /// The roots the client announced that this server could not bind.
    ///
    /// Empty when nothing is mismatched. Handed back so a later call can put
    /// the same roots through the binder again instead of trusting a verdict
    /// reached before anything on the host had a chance to change.
    fn mismatched_roots(&self) -> Vec<PathBuf> {
        self.mismatch
            .as_ref()
            .map(|mismatch| mismatch.requested_roots.clone())
            .unwrap_or_default()
    }

    /// Structured refusal for a `tools/call` that arrived while the client's
    /// workspace and this server's binding disagree. `None` for a malformed
    /// call with no id, which has no response channel — the caller still drops
    /// the call rather than forwarding it.
    fn repo_mismatch_response(&self, request: &serde_json::Value) -> Option<JsonRpcResponse> {
        let mismatch = self.mismatch.as_ref()?;
        let id = request.get("id").filter(|id| !id.is_null())?.clone();
        let tool = request
            .pointer("/params/name")
            .and_then(|name| name.as_str())
            .unwrap_or("this tool");
        let result = ToolCallResult::error(mismatch.message(tool));
        let enveloped = envelope::finalize(result, Envelope::workspace_mismatch(), tool);
        Some(JsonRpcResponse::success(
            Some(id),
            serde_json::to_value(&enveloped).unwrap_or_default(),
        ))
    }
}

impl WorkspaceMismatch {
    fn message(&self, tool: &str) -> String {
        let bound = if self.bound_repo.is_some() {
            "the bound repository"
        } else {
            "the previously selected repository context"
        };
        format!(
            "kin-mcp refuses '{tool}': the client's requested repository context changed and \
             cannot be bound by this connection. Answering from {bound} would use the wrong \
             semantic scope. {}",
            self.remedy()
        )
    }

    /// The fixes that actually apply to this server.
    ///
    /// A repo-bound server is not offered `kin init .`, and the omission is the
    /// point rather than brevity. `--repo`/`KIN_MCP_REPO` is how a server is
    /// registered against a repository it reaches over a boundary the client
    /// does not share: a docker-exec registration, a remote checkout. The path
    /// the client announces is then a HOST path this process may never be able
    /// to see, so running `kin init` there does not repair the mismatch. It
    /// creates a second, empty repository beside the real one and leaves the
    /// refusal exactly where it was.
    fn remedy(&self) -> String {
        if self.repo_pinned {
            "This connection is pinned to a repository context. Reconnect the MCP server with \
             the intended repository selected."
                .to_string()
        } else {
            "Reconnect the MCP server with the intended repository selected.".to_string()
        }
    }
}

/// Wait for the launcher's startup binding to settle, for at most `budget`.
///
/// Returns `None` once it settles, bound or not, and `Some(waited)` when the
/// budget ran out first. The wait ends the instant the binding settles, so a
/// warm daemon that binds in milliseconds costs a call nothing, and a cold one
/// is waited for rather than reported on after a fixed few seconds. While it
/// waits it tells a client that asked for progress how far the daemon has
/// come, every [`daemon_delegate::PROGRESS_INTERVAL`], which also keeps a
/// client that resets its own timeout on progress from giving up first.
async fn await_startup_binding<W: AsyncWrite + Unpin>(
    startup: &StartupDaemonBinding,
    budget: Duration,
    mut progress: Option<&mut CallProgressNotifier>,
    writer: &mut W,
    framed: bool,
) -> Result<Option<Duration>> {
    let began = tokio::time::Instant::now();
    let deadline = began + budget;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if startup
            .wait_until_settled(remaining.min(daemon_delegate::PROGRESS_INTERVAL))
            .await
        {
            return Ok(None);
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(Some(began.elapsed()));
        }
        if let Some(progress) = progress.as_deref_mut() {
            let line = format!("waiting for the repo daemon: {}", startup.progress_line());
            progress.notify(writer, framed, &line, Some(budget)).await?;
        }
    }
}

/// Forward one message while relaying, as MCP progress notifications, what
/// any wait inside it reports: a daemon that is listening and still opening,
/// or a daemon being restarted after it stopped.
async fn forward_reporting_progress<W: AsyncWrite + Unpin>(
    message: &str,
    config: &McpServerConfig,
    notifier: &mut CallProgressNotifier,
    writer: &mut W,
    framed: bool,
) -> Result<Option<JsonRpcResponse>> {
    let (progress, mut lines) = daemon_delegate::CallProgress::channel();
    let call =
        daemon_delegate::with_call_progress(progress, process_daemon_message(message, config));
    tokio::pin!(call);
    loop {
        tokio::select! {
            biased;
            response = &mut call => return Ok(response),
            Some(line) = lines.recv() => notifier.notify(writer, framed, &line, None).await?,
        }
    }
}

/// The MCP progress notifications for one `tools/call` whose client sent a
/// `progressToken`.
///
/// Progress is the seconds this call has spent waiting so far, which only
/// grows, as the protocol requires of it. A call with no token gets no
/// notifications: the client did not ask for them.
struct CallProgressNotifier {
    token: serde_json::Value,
    began: tokio::time::Instant,
    last: Option<u64>,
}

impl CallProgressNotifier {
    fn for_request(request: &serde_json::Value) -> Option<Self> {
        let token = request.pointer("/params/_meta/progressToken")?;
        (token.is_string() || token.is_number()).then(|| Self {
            token: token.clone(),
            began: tokio::time::Instant::now(),
            last: None,
        })
    }

    async fn notify<W: AsyncWrite + Unpin>(
        &mut self,
        writer: &mut W,
        framed: bool,
        message: &str,
        total: Option<Duration>,
    ) -> Result<()> {
        let progress = self.began.elapsed().as_secs();
        if self.last.is_some_and(|last| progress <= last) {
            return Ok(());
        }
        self.last = Some(progress);
        let mut params = serde_json::json!({
            "progressToken": self.token,
            "progress": progress,
            "message": message,
        });
        if let Some(total) = total {
            params["total"] = serde_json::json!(total.as_secs());
        }
        let notification = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/progress",
            "params": params,
        });
        let text = serde_json::to_string(&notification).map_err(McpError::Json)?;
        write_stdio_message(writer, &text, framed).await
    }
}

/// Structured answer for a `tools/call` that arrived while the launcher's
/// startup binding is still pending. `None` for a malformed call with no id,
/// which has no response channel; the caller still drops the call rather than
/// forwarding it.
fn startup_pending_response(
    request: &serde_json::Value,
    startup: &StartupDaemonBinding,
    waited: Duration,
) -> Option<JsonRpcResponse> {
    let id = request.get("id").filter(|id| !id.is_null())?.clone();
    let tool = request
        .pointer("/params/name")
        .and_then(|name| name.as_str())
        .unwrap_or("this tool");
    let result = ToolCallResult::error(startup.starting_report(tool, waited));
    let enveloped = envelope::finalize(result, Envelope::daemon_unreachable(), tool);
    Some(JsonRpcResponse::success(
        Some(id),
        serde_json::to_value(&enveloped).unwrap_or_default(),
    ))
}

/// How long an unanswered `roots/list` suppresses another one. A client that
/// advertises the roots capability and then never answers would otherwise wedge
/// binding — and, after a workspace change, the refusal state — for the life of
/// the process.
const ROOTS_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Default)]
struct WorkspaceRootsRequestState {
    in_flight_since: Option<Instant>,
}

impl WorkspaceRootsRequestState {
    fn complete(&mut self) {
        self.in_flight_since = None;
    }

    fn request_in_flight(&self, now: Instant) -> bool {
        self.in_flight_since
            .is_some_and(|started| now.saturating_duration_since(started) < ROOTS_REQUEST_TIMEOUT)
    }

    fn begin_if_allowed(
        &mut self,
        method: Option<&str>,
        client_supports_roots: bool,
        has_repo_binder: bool,
        wants_binding: bool,
        now: Instant,
    ) -> bool {
        let should_begin = should_request_workspace_roots(
            method,
            client_supports_roots,
            self.request_in_flight(now),
            has_repo_binder,
            wants_binding,
        );
        if should_begin {
            self.in_flight_since = Some(now);
        }
        should_begin
    }
}

/// Decide whether an inbound client message should trigger a workspace-roots
/// request. Kept separate from the stdio loop so the retry semantics remain
/// deterministic and testable without mutating process-global daemon state.
///
/// A roots *change* is honored whether or not a repository is bound: it is the
/// only signal an editor sends when its window moves to another folder, and
/// ignoring it once bound is what leaves the server answering from the previous
/// repository. The other triggers are gated on `wants_binding` — unbound, or
/// bound to a repository the client has left — so a settled session does not
/// re-ask on every `tools/list`, while a refusing one keeps trying.
fn should_request_workspace_roots(
    method: Option<&str>,
    client_supports_roots: bool,
    request_in_flight: bool,
    has_repo_binder: bool,
    wants_binding: bool,
) -> bool {
    if !client_supports_roots || request_in_flight || !has_repo_binder {
        return false;
    }
    match method {
        Some("notifications/roots/list_changed") => true,
        Some("initialized") | Some("notifications/initialized") | Some("tools/list") => {
            wants_binding
        }
        _ => false,
    }
}

/// Extract filesystem paths from an MCP `roots/list` response
/// (`result.roots[].uri`), accepting both `file://` URIs and bare paths.
fn parse_workspace_roots(value: &serde_json::Value) -> Vec<PathBuf> {
    value
        .pointer("/result/roots")
        .and_then(|roots| roots.as_array())
        .map(|roots| {
            roots
                .iter()
                .filter_map(|root| root.get("uri").and_then(|uri| uri.as_str()))
                .filter_map(root_uri_to_path)
                .collect()
        })
        .unwrap_or_default()
}

/// Convert an MCP root's `uri` into a filesystem path. Accepts spec-compliant
/// `file://` URIs (decoding `%XX` escapes) and the bare absolute path some
/// clients (e.g. Cursor) send instead. Returns `None` for remote/non-file URIs.
fn root_uri_to_path(uri: &str) -> Option<PathBuf> {
    if uri
        .get(.."file://".len())
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("file://"))
    {
        let rest = &uri["file://".len()..];

        // Empty authority: `file:///abs/path` or `file:///C:/Users/me/repo`.
        if rest.starts_with('/') {
            return Some(file_uri_path(percent_decode(rest)));
        }

        let slash = rest.find('/')?;
        let authority = &rest[..slash];
        let path = percent_decode(&rest[slash..]);

        // `localhost` is the local machine, so only its path component matters.
        if authority.eq_ignore_ascii_case("localhost") {
            return Some(file_uri_path(path));
        }

        // A few Windows clients emit the non-canonical but common
        // `file://C:/Users/...` spelling, treating the drive as an authority.
        if is_windows_drive_authority(authority) {
            return Some(PathBuf::from(format!("{authority}{path}")));
        }

        // A non-local file authority is a Windows UNC share. Do not reinterpret
        // it as a local POSIX path on Unix hosts.
        #[cfg(windows)]
        {
            return Some(PathBuf::from(format!(
                r"\\{authority}{}",
                path.replace('/', "\\")
            )));
        }
        #[cfg(not(windows))]
        {
            return None;
        }
    }
    // Some clients (Cursor) send a bare absolute path rather than a file URI.
    if uri.starts_with('/') || is_windows_drive_path(uri) || uri.starts_with(r"\\") {
        return Some(PathBuf::from(uri));
    }
    None
}

/// Normalize the path component of a file URI. RFC 8089 spells a Windows drive
/// URI as `file:///C:/...`; the leading slash is URI syntax, not part of the
/// native Windows path.
fn file_uri_path(path: String) -> PathBuf {
    if path.starts_with('/') && is_windows_drive_path(&path[1..]) {
        PathBuf::from(&path[1..])
    } else {
        PathBuf::from(path)
    }
}

fn is_windows_drive_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'/' | b'\\')
}

fn is_windows_drive_authority(authority: &str) -> bool {
    let bytes = authority.as_bytes();
    bytes.len() == 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

/// Minimal percent-decoding for `file://` URI paths (handles `%20`, etc.).
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(decoded) = u8::from_str_radix(&input[i + 1..i + 3], 16) {
                out.push(decoded);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

async fn read_stdio_message<R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> Result<Option<(String, bool)>> {
    let mut first_line = String::new();
    loop {
        first_line.clear();
        let bytes = reader
            .read_line(&mut first_line)
            .await
            .map_err(McpError::Io)?;
        if bytes == 0 {
            return Ok(None);
        }
        if !first_line.trim().is_empty() {
            break;
        }
    }

    if let Some(content_length) = parse_content_length(&first_line) {
        let mut header_line = String::new();
        loop {
            header_line.clear();
            let bytes = reader
                .read_line(&mut header_line)
                .await
                .map_err(McpError::Io)?;
            if bytes == 0 {
                return Err(McpError::Protocol(
                    "unexpected EOF while reading MCP headers".into(),
                ));
            }
            if header_line == "\n" || header_line == "\r\n" {
                break;
            }
        }

        let mut payload = vec![0u8; content_length];
        reader
            .read_exact(&mut payload)
            .await
            .map_err(McpError::Io)?;
        let message = String::from_utf8(payload)
            .map_err(|e| McpError::Protocol(format!("invalid UTF-8 payload: {e}")))?;
        return Ok(Some((message, true)));
    }

    Ok(Some((first_line.trim().to_string(), false)))
}

async fn write_stdio_message<W: AsyncWrite + Unpin>(
    writer: &mut W,
    response_json: &str,
    framed: bool,
) -> Result<()> {
    if framed {
        let response_bytes = response_json.as_bytes();
        let header = format!("Content-Length: {}\r\n\r\n", response_bytes.len());
        writer
            .write_all(header.as_bytes())
            .await
            .map_err(McpError::Io)?;
        writer
            .write_all(response_bytes)
            .await
            .map_err(McpError::Io)?;
    } else {
        writer
            .write_all(response_json.as_bytes())
            .await
            .map_err(McpError::Io)?;
        writer.write_all(b"\n").await.map_err(McpError::Io)?;
    }
    writer.flush().await.map_err(McpError::Io)?;
    Ok(())
}

fn parse_content_length(line: &str) -> Option<usize> {
    let (name, value) = line.split_once(':')?;
    if !name.trim().eq_ignore_ascii_case("Content-Length") {
        return None;
    }
    value.trim().parse().ok()
}

/// Process a single JSON-RPC message and return a response.
pub async fn process_message<G: PersistableMcpStore>(
    message: &str,
    store: &G,
    config: &McpServerConfig,
    sessions: &SessionRegistry,
) -> Option<JsonRpcResponse> {
    let request: JsonRpcRequest = match serde_json::from_str(message) {
        Ok(req) => req,
        Err(e) => {
            return Some(JsonRpcResponse::error(
                None,
                -32700,
                format!("Parse error: {}", e),
            ));
        }
    };

    let id = request.id.clone();
    let is_notification = id.is_none();

    let response = match request.method.as_str() {
        "initialize" => Some(handle_initialize(id, &request.params, config)),
        "initialized" => None,
        "tools/list" => Some(handle_tools_list(id, config)),
        "tools/call" if config.session_authority_mode.requires_daemon() => {
            Some(handle_tools_call_daemon(id, &request.params, config).await)
        }
        "tools/call" => Some(handle_tools_call(id, &request.params, store, sessions, config).await),
        "prompts/list" => Some(JsonRpcResponse::success(
            id,
            crate::ask::dispatch::prompts_list(config),
        )),
        "prompts/get" => Some(prompt_answer(
            id,
            if config.session_authority_mode.requires_daemon() {
                crate::ask::dispatch::prompt_get_daemon(&request.params, config).await
            } else {
                crate::ask::dispatch::prompt_get_offline(&request.params, store, sessions, config)
                    .await
            },
        )),
        "ping" => Some(JsonRpcResponse::success(id, serde_json::json!({}))),
        _ => Some(JsonRpcResponse::error(
            id,
            -32601,
            format!("Method not found: {}", request.method),
        )),
    };

    if is_notification {
        None
    } else {
        response
    }
}

/// A `prompts/get` answer, or its refusal as invalid params.
fn prompt_answer(
    id: Option<serde_json::Value>,
    answer: std::result::Result<serde_json::Value, String>,
) -> JsonRpcResponse {
    match answer {
        Ok(result) => JsonRpcResponse::success(id, result),
        Err(message) => JsonRpcResponse::error(id, -32602, message),
    }
}

/// Process a single JSON-RPC message for daemon-backed product mode.
pub async fn process_daemon_message(
    message: &str,
    config: &McpServerConfig,
) -> Option<JsonRpcResponse> {
    let request: JsonRpcRequest = match serde_json::from_str(message) {
        Ok(req) => req,
        Err(e) => {
            return Some(JsonRpcResponse::error(
                None,
                -32700,
                format!("Parse error: {}", e),
            ));
        }
    };

    let id = request.id.clone();
    let is_notification = id.is_none();

    let response = match request.method.as_str() {
        "initialize" => Some(handle_initialize(id, &request.params, config)),
        "initialized" => None,
        "tools/list" => Some(handle_tools_list(id, config)),
        "tools/call" => Some(handle_tools_call_daemon(id, &request.params, config).await),
        "prompts/list" => Some(JsonRpcResponse::success(
            id,
            crate::ask::dispatch::prompts_list(config),
        )),
        "prompts/get" => Some(prompt_answer(
            id,
            crate::ask::dispatch::prompt_get_daemon(&request.params, config).await,
        )),
        "ping" => Some(JsonRpcResponse::success(id, serde_json::json!({}))),
        _ => Some(JsonRpcResponse::error(
            id,
            -32601,
            format!("Method not found: {}", request.method),
        )),
    };

    if is_notification {
        None
    } else {
        response
    }
}

/// The initialize capability, under `capabilities.experimental.kin`, with which
/// a client declares that it only reads.
///
/// An agent investigating code has no use for the write tools, and one that
/// sees them spends turns on them: in a measured run an agent started a
/// session for a question it answered by reading. A client that declares
/// `"readOnly": true` is served the tools that read and nothing else, see
/// [`McpServerConfig::serve_reads_only`].
pub const READ_ONLY_CAPABILITY: &str = "readOnly";

/// Whether an `initialize` request declares a client that only reads.
pub fn client_declares_read_only(initialize: &serde_json::Value) -> bool {
    initialize
        .pointer("/params/capabilities/experimental/kin")
        .and_then(|kin| kin.get(READ_ONLY_CAPABILITY))
        .and_then(serde_json::Value::as_bool)
        == Some(true)
}

/// Every MCP protocol version this server speaks, newest first.
///
/// 2025-03-26 is not among them: a server that negotiates it must accept
/// JSON-RPC batches, and this one reads one message at a time. A client that
/// asks for it is answered with [`FALLBACK_PROTOCOL_VERSION`], which it also
/// speaks, as it has been since this server began.
pub const SUPPORTED_PROTOCOL_VERSIONS: [&str; 3] = ["2025-11-25", "2025-06-18", "2024-11-05"];

/// The version a client is answered with when it names none, or one older
/// than the newest this server speaks and not among them.
const FALLBACK_PROTOCOL_VERSION: &str = "2024-11-05";

/// The first version with structured tool output: `outputSchema` on a listed
/// tool and `structuredContent` on its result.
const STRUCTURED_OUTPUT_SINCE: &str = "2025-06-18";

/// The version to answer an `initialize` that asked for `requested` with.
///
/// The client's own when this server speaks it. The newest this server speaks
/// when the client asked for a newer one, which the specification says a
/// client then accepts or disconnects from. Otherwise the fallback, because a
/// client that asked for an older unsupported version, or none, speaks the one
/// every client has been answered with to date.
pub fn negotiate_protocol_version(requested: Option<&str>) -> &'static str {
    let Some(requested) = requested else {
        return FALLBACK_PROTOCOL_VERSION;
    };
    if let Some(supported) = SUPPORTED_PROTOCOL_VERSIONS
        .iter()
        .find(|version| **version == requested)
    {
        return supported;
    }
    // Revisions are dates, so they order as strings.
    if requested > SUPPORTED_PROTOCOL_VERSIONS[0] {
        SUPPORTED_PROTOCOL_VERSIONS[0]
    } else {
        FALLBACK_PROTOCOL_VERSION
    }
}

/// Whether this connection serves each verb's `outputSchema` and a result's
/// `structuredContent`: when it negotiated a version that has them, and its
/// text content is the machine JSON the structured content restates.
///
/// Never on a connection that serves plain words. Claude Code, Codex and VS
/// Code show their model only the structured content when a result carries
/// it, so an agent profile's plain answer would be replaced by JSON there.
pub fn serves_structured_output(config: &McpServerConfig) -> bool {
    config.protocol_version >= STRUCTURED_OUTPUT_SINCE
        && (!config.plain_replies || config.citable)
        && config.routed.is_none()
}

/// The most bytes [`SERVER_INSTRUCTIONS`] may carry.
///
/// It is injected once per session and a client may spend it on the model's
/// context rather than on its own, so it is a budget rather than a page.
/// Grok delivers it as a 1,194-byte synthetic user message on turn one.
///
/// Held by the compile-time assertion below the string rather than by a test.
/// There is no state in which this crate should build with an instructions
/// string a client cannot afford to inject, and a test that fails after the
/// bytes are written is a slower way to learn the same thing.
pub(crate) const SERVER_INSTRUCTIONS_BUDGET: usize = 1_200;

/// Usage instructions returned at initialize time so a connecting agent knows
/// what this server is before its first tool call.
///
/// Written for the client shape where this string is ALL the model gets. A
/// client is free to hide the tool schemas: Grok never sends them, delivering
/// this string verbatim as a synthetic reminder on turn one and making the
/// model call its own `search_tool` with a query to learn any tool's name or
/// schema. Measured on 2026-09-15 with Kin attached beside Grok's own file and
/// shell tools, three local models made zero Kin calls across ten runs. The
/// string they read said "prefer semantic_locate" and "reach for grep only when
/// no graph-backed tool answers the question". Every one of the ten answered
/// from Grok's own file and shell tools instead, eight of them with grep, and
/// only three ever called `search_tool` at all.
///
/// So it names the tools a model reaches first by their exact served names,
/// because a name a model has read is a name it can search for, and it tells a
/// model whose client lists tools by search to search for this server before
/// its first file read.
///
/// Since 2026-09-22 it routes rather than lists: a list of what each tool does
/// left the choice to the model, and in the corrected rerun pilot of
/// 2026-09-22 the model explored with shell commands only and never called a
/// Kin tool. Since 2026-10-01 the routing leads, one line per question in the
/// words a question arrives in, and each verdict is defined as the action it
/// permits. Measured that day with a local model on tuned tasks, a surface that
/// led with the question each tool answers cut the model's generated tokens by
/// a third at the same exact answers, and the two lines it replaced ("Preserve
/// partial, stale, unproven, unresolved and runtime-name limits" and "Never
/// promote a gap to proof") were quoted by the model as its reason to keep
/// deliberating over a reply that had already answered. Every reply carries its
/// own verdict and its own limits, so the instructions say what to do with
/// each verdict rather than restating the limits.
///
/// Since 2026-10-06 the reach line says when to pass `scope`. A model asked
/// which functions under one package can end up calling a method traced the
/// whole repository four times and never passed it, then paged through 126
/// callers for the seven the question was about. The sentence took the place
/// of "Use verification tools only for builds and tests", which names tools
/// this profile does not serve, and of four characters on two other lines.
/// The routed profiles' strings have no room for it and are unchanged.
///
/// The routed profiles' wording is [`ROUTED_SERVER_INSTRUCTIONS`] and
/// [`ROUTED_QUERY_SERVER_INSTRUCTIONS`], the tool-search profile's is
/// [`SEARCH_SERVER_INSTRUCTIONS`], and the citable profiles keep
/// [`LEGACY_SERVER_INSTRUCTIONS`]. `kin agent run` builds its own prompt and
/// never reads any of them.
pub(crate) const SERVER_INSTRUCTIONS: &str = "Kin's indexed code graph includes tests. Pick by question:
- What calls X? refs on X, relation_kinds=[\"calls\"].
- Overrides? refs, view=\"overrides\", entity_id=X UUID.
- What must change if X changes? impact on X: callers and overrides.
- What can end up calling X? trace on X, direction callers, depth 8. If the question limits where callers live, pass scope.
- Is X used anywhere? refs on X, no relation_kinds.
- Where is X? locate. Pass returned UUIDs as entity_id, semantic names as query.
- Code around X: context. X's code: source by entity_id.
- Why is a caller's receiver typed? refs on X, proof_for_caller=caller UUID.
Read _kin.verdict:
- complete: checked against the whole graph; answer, don't re-read code. A whole body or outline is not all relations.
- complete except the sites listed below, or indirect uses: rows are checked; check only listed places that could apply, then answer.
- partial: answer from the proven rows and name the gap, unless the reply lists a concrete next step.
Explore entities and relationships. Kin's current graph outranks memory of other versions of this code. Answer once in the requested format.
If hidden, search for \"kin\" first, to find Kin's tools.";

/// [`SERVER_INSTRUCTIONS`] for `agent-search`, which serves a measured
/// always-on set with the `ask` front door and reaches the rest through
/// `kin_tool_search` and `kin_tool_call`. The same routing, naming only tools that profile serves
/// and saying how the rest, `source` among them, are reached.
pub(crate) const SEARCH_SERVER_INSTRUCTIONS: &str = "Kin's indexed code graph includes tests. Pick by question:
- Any question about the code, in plain words: ask. Its first line says what Kin ran and what it read the question as.
- Where is X? locate. Pass returned UUIDs as entity_id, semantic names as query.
- What can end up calling X, or what does X call? trace on X. If the question limits where callers live, pass scope.
- Code around X: context. X's own code: kin_tool_call, tool source, by entity_id.
- Anything else: kin_tool_search finds every other read-only Kin tool; kin_tool_call runs it.
Read _kin.verdict:
- complete: checked against the whole graph; answer, don't re-read code. A whole body or outline is not all relations.
- complete except the sites listed below, or indirect uses: rows are checked; check only listed places that could apply, then answer.
- partial: answer from the proven rows and name the gap, unless the reply lists a concrete next step.
Explore entities and relationships. Kin's current graph outranks memory of other versions of this code. Answer once in the requested format.
If hidden, search for \"kin\" first, to find Kin's tools.";

/// [`SERVER_INSTRUCTIONS`] for a named profile that also lists [`crate::ask`]
/// (`agent-query`): the routing table becomes one line, because `ask`
/// routes the question itself and says what it ran, and the verbs stay named
/// for the steps it does not take.
pub(crate) const ASK_SERVER_INSTRUCTIONS: &str = "Kin's indexed code graph includes tests. Pick by question:
- Any question about the code, in plain words: ask. Its first line says what Kin ran and what it read the question as; if that is not your question, ask again in other words.
- Overrides? refs, view=\"overrides\", entity_id=X UUID.
- Where is X? locate. Pass returned UUIDs as entity_id, semantic names as query.
- Code around X: context. X's code: source by entity_id. The rest: refs, trace, impact.
Read _kin.verdict:
- complete: checked against the whole graph; answer, don't re-read code. A whole body or outline is not all relations.
- complete except the sites listed below, or indirect uses: rows are checked; check only listed places that could apply, then answer.
- partial: answer from the proven rows and name the gap, unless the reply lists a concrete next step.
Explore entities and relationships. Kin's current graph outranks memory of other versions of this code. Answer once in the requested format.
If hidden, search for \"kin\" first, to find Kin's tools.";

/// The instructions for `agent-ask`, which serves [`crate::ask`] alone: every
/// question goes through it, and every reply carries each answer's own
/// verdict line in text.
pub(crate) const ASK_ONLY_SERVER_INSTRUCTIONS: &str = "Kin's indexed code graph includes tests. Ask it any question about the code in plain words with ask: who calls X, what can end up calling X, what must change if X changes, is X used anywhere, which modules declare X, where is X, X's code. Each reply's first line says what Kin ran and what it read the question as; if that is not your question, ask again in other words.
Read each answer's Answer (verdict) line:
- complete: checked against the whole graph; answer, don't re-read code.
- complete except the sites listed below, or indirect uses: rows are checked; check only listed places that could apply, then answer.
- partial: answer from the proven rows and name the gap, unless the reply lists a concrete next step.
Kin's current graph outranks memory of other versions of this code. Answer once in the requested format.
If hidden, search for \"kin\" first, to find Kin's tools.";

/// [`SERVER_INSTRUCTIONS`] for `agent-routed`, where every command is reached
/// through the one tool [`crate::routed::TOOL_NAME`]: `kin locate` is that tool
/// called with the `locate` command.
///
/// Its last sentence is true on this profile and on no named one: `describe`
/// lists every registered tool the commands do not name, and `call` runs it.
pub(crate) const ROUTED_SERVER_INSTRUCTIONS: &str = "Kin's indexed code graph includes tests. Call kin with command and args:
- What calls X? kin refs on X, relation_kinds=[\"calls\"].
- Overrides? kin refs, view=\"overrides\", entity_id=X UUID.
- What must change if X changes? kin impact on X: callers and overrides.
- What can end up calling X? kin trace on X, direction callers, depth 8.
- Is X used anywhere? kin refs on X, no relation_kinds.
- Where is X? kin locate. Pass returned UUIDs as entity_id, semantic names as query.
- Code around X: kin context. X's own code: kin source by entity_id.
Read _kin.verdict:
- complete: checked against the whole graph; answer, don't re-read code. A whole body or outline is not all relations.
- complete except the sites listed below, or indirect uses: rows are checked; check only listed places that could apply, then answer.
- partial: answer from the proven rows and name the gap, unless the reply lists a concrete next step.
Explore entities and relationships. Kin's current graph outranks memory of other versions of this code. Answer once in the requested format.
If hidden, search for \"kin\" first, to find Kin's tools. kin describe lists every other Kin tool, and kin call runs one.";

/// [`ROUTED_SERVER_INSTRUCTIONS`] for `agent-routed-query`, which serves the
/// same commands without a write path and reaches only read-only tools.
pub(crate) const ROUTED_QUERY_SERVER_INSTRUCTIONS: &str = "Kin's indexed code graph includes tests. Call kin with command and args:
- What calls X? kin refs on X, relation_kinds=[\"calls\"].
- Overrides? kin refs, view=\"overrides\", entity_id=X UUID.
- What must change if X changes? kin impact on X: callers and overrides.
- What can end up calling X? kin trace on X, direction callers, depth 8.
- Is X used anywhere? kin refs on X, no relation_kinds.
- Where is X? kin locate. Pass returned UUIDs as entity_id, semantic names as query.
- Code around X: kin context. X's own code: kin source by entity_id.
Read _kin.verdict:
- complete: checked against the whole graph; answer, don't re-read code. A whole body or outline is not all relations.
- complete except the sites listed below, or indirect uses: rows are checked; check only listed places that could apply, then answer.
- partial: answer from the proven rows and name the gap, unless the reply lists a concrete next step.
Explore entities and relationships. Kin's current graph outranks memory of other versions of this code. Answer once in the requested format.
If hidden, search for \"kin\" first, to find Kin's tools. kin describe lists every other read-only Kin tool, and kin call runs one.";

/// Historical instructions for the benchmark profiles, with retired file catalogs
/// removed from newly built servers. Earlier measurements remain bound to their
/// original binaries; changing this served surface requires a new measurement.
pub(crate) const LEGACY_SERVER_INSTRUCTIONS: &str = "Kin answers questions about this repository \
from a semantic graph. Address entities and relationships for questions about \
code and dependencies. Report missing semantic coverage as a gap.
If your client does not show you these tools up front and you have to search for a tool, search \
for \"kin\" first, to discover the semantic tools; that is also how you reach a tool this connection \
did not list.
semantic_locate: find code by describing what it does, when you do not know the name.
semantic_search: find declarations by exact name, kind or language.
get_context_pack: one token-bounded bundle of the code around an entity or a question.
find_references: who calls, imports or references one entity.
trace_data_flow: the ordered call chain out from one entity.
trace_path: how one entity reaches another.
impact_analysis: what a change to one entity could affect.
graph_neighborhood: dependencies and dependents of an entity.
Every answer carries a `_kin` envelope. Read `_kin.verdict` first: when its `state` is \
`inconclusive`, treat the counts as a lower bound and do not act on an absence in the answer.";

const _: () = assert!(
    SERVER_INSTRUCTIONS.len() <= SERVER_INSTRUCTIONS_BUDGET
        && ASK_SERVER_INSTRUCTIONS.len() <= SERVER_INSTRUCTIONS_BUDGET
        && ASK_ONLY_SERVER_INSTRUCTIONS.len() <= SERVER_INSTRUCTIONS_BUDGET
        && SEARCH_SERVER_INSTRUCTIONS.len() <= SERVER_INSTRUCTIONS_BUDGET
        && ROUTED_SERVER_INSTRUCTIONS.len() <= SERVER_INSTRUCTIONS_BUDGET
        && ROUTED_QUERY_SERVER_INSTRUCTIONS.len() <= SERVER_INSTRUCTIONS_BUDGET
        && LEGACY_SERVER_INSTRUCTIONS.len() <= SERVER_INSTRUCTIONS_BUDGET,
    "an instructions string is over SERVER_INSTRUCTIONS_BUDGET: a client injects it once per \
     session, and on a client that hides tool schemas it is the whole surface the model reads"
);

/// The tools [`SERVER_INSTRUCTIONS`] routes a model's questions to, in the order
/// it names them.
pub(crate) const NAMED_PROCEDURE_TOOLS: [&str; 6] = [
    crate::verbs::REFS,
    crate::verbs::IMPACT,
    crate::verbs::TRACE,
    crate::verbs::LOCATE,
    crate::verbs::CONTEXT,
    crate::verbs::SOURCE,
];

/// The instructions this connection's profile is served.
pub(crate) fn instructions_for(config: &McpServerConfig) -> &'static str {
    if config.citable {
        return LEGACY_SERVER_INSTRUCTIONS;
    }
    match config.routed {
        Some(surface) if surface.writes => ROUTED_SERVER_INSTRUCTIONS,
        Some(_) => ROUTED_QUERY_SERVER_INSTRUCTIONS,
        None => {
            let allowed = config.allowed_tools.as_ref();
            let serves_the_procedure = allowed.is_none_or(|allowed| {
                NAMED_PROCEDURE_TOOLS
                    .iter()
                    .all(|tool| crate::verbs::allows(allowed, tool))
            });
            let searches = !serves_the_procedure && crate::tool_invocation::enabled(allowed);
            // The named agent profiles that list `ask`. `full` serves it too and
            // keeps the whole routing table, as every unfiltered surface does.
            let lists_ask = allowed.is_some_and(|allowed| allowed.contains(crate::ask::TOOL_NAME));
            match (lists_ask, searches) {
                (_, true) => SEARCH_SERVER_INSTRUCTIONS,
                (true, false) if serves_the_procedure => ASK_SERVER_INSTRUCTIONS,
                (true, false) => ASK_ONLY_SERVER_INSTRUCTIONS,
                (false, false) => SERVER_INSTRUCTIONS,
            }
        }
    }
}

/// The instructions `initialize` serves: [`instructions_for`], and, when the
/// folder the client works in holds a store an older Kin build wrote, one
/// sentence naming `kin upgrade`.
///
/// Every answer over such a store is inconclusive until it is re-derived, and
/// the agent is the one reading those answers, so it is told why and what fixes
/// it before its first call. A citable connection keeps its instructions byte
/// for byte, because a published number was measured under them.
fn instructions_with_store_notice(config: &McpServerConfig) -> String {
    let mut instructions = speak_handles(instructions_for(config), config);
    let behind = !config.citable
        && config
            .client_root
            .as_deref()
            .is_some_and(crate::daemon_delegate::store_written_by_older_build);
    if behind {
        instructions.push_str(&format!(
            "\n\nAn older version of Kin built this repository's records, so every answer \
             stays partial until `kin upgrade` runs in it (`{}` through npm). Ask the user to \
             run it before relying on an answer here.",
            kin_core::hydration_semantics::npm_upgrade_command()
        ));
    }
    instructions
}

/// The tool listing this connection is served, exactly as `tools/list` writes
/// it: the routed tool on a routed connection, and otherwise the profile's
/// filtered, annotated and compacted list, with the source tools' description
/// saying so where bodies are numbered.
///
/// One function, so a test that measures the served bytes measures what the
/// server writes to the client rather than its own copy of the recipe.
pub fn served_tools_for(config: &McpServerConfig) -> ToolsListResult {
    if let Some(surface) = config.routed {
        return crate::routed::served_list(surface);
    }
    let mut tools =
        crate::tools::served_tools_list(config.allowed_tools.as_ref(), config.agent_belt);
    if config.number_entity_lines {
        crate::entity_lines::describe_numbered_bodies(&mut tools);
    }
    if config.active_handles().is_some() {
        for tool in &mut tools.tools {
            speak_handles_in(&mut tool.input_schema);
        }
    }
    tools
}

/// Wording for a connection that serves session handles: an id it hands
/// back is a short handle, not a UUID, and on a text connection a row's
/// `module :: name` is accepted wherever an id is.
fn speak_handles(text: &str, config: &McpServerConfig) -> String {
    if config.active_handles().is_none() {
        return text.to_string();
    }
    let mut text = text
        .replace(
            "Pass returned UUIDs as entity_id",
            "Pass returned ids as entity_id",
        )
        .replace("entity_id=X UUID", "entity_id=X's id")
        .replace("caller UUID", "caller id");
    if config.reply_format == crate::text_reply::ReplyFormat::Text {
        text = text
            .replace(
                "Pass returned ids as entity_id",
                "Pass a row's `module :: name`, or an id, as entity_id",
            )
            .replace(
                "Read _kin.verdict:",
                "Read the verdict on each reply's first line, Answer (verdict):",
            );
    }
    text
}

/// Every description in a served input schema, where an id is described as
/// a UUID, on a connection that serves short handles.
fn speak_handles_in(schema: &mut serde_json::Value) {
    match schema {
        serde_json::Value::Object(fields) => {
            for (key, value) in fields.iter_mut() {
                match value {
                    serde_json::Value::String(text) if key == "description" => {
                        *text = text.replace("UUIDs", "ids").replace("UUID", "id");
                    }
                    other => speak_handles_in(other),
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                speak_handles_in(item);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod handle_wording_tests {
    use super::*;

    fn agent(format: crate::text_reply::ReplyFormat) -> McpServerConfig {
        let mut config = McpServerConfig {
            allowed_tools: Some(crate::tool_name_set(
                crate::tools::agent_default_tool_names(),
            )),
            agent_belt: true,
            plain_replies: true,
            ..Default::default()
        };
        config.session_handles = Some(Default::default());
        config.reply_format = format;
        config
    }

    #[test]
    fn a_handle_connection_speaks_of_ids_and_a_text_one_of_names() {
        let plain = McpServerConfig {
            allowed_tools: Some(crate::tool_name_set(
                crate::tools::agent_default_tool_names(),
            )),
            agent_belt: true,
            plain_replies: true,
            ..Default::default()
        };
        assert!(instructions_with_store_notice(&plain).contains("Pass returned UUIDs"));
        let json = agent(crate::text_reply::ReplyFormat::Json);
        let instructions = instructions_with_store_notice(&json);
        assert!(
            instructions.contains("Pass returned ids as entity_id"),
            "{instructions}"
        );
        assert!(!instructions.contains("UUID"), "{instructions}");
        assert!(instructions.contains("Read _kin.verdict:"));
        let listed = serde_json::to_string(&served_tools_for(&json)).unwrap();
        assert!(!listed.contains("UUID"), "{listed}");
        let text = agent(crate::text_reply::ReplyFormat::Text);
        let instructions = instructions_with_store_notice(&text);
        assert!(
            instructions.contains("`module :: name`, or an id, as entity_id"),
            "{instructions}"
        );
        assert!(instructions.contains("Answer (verdict):"), "{instructions}");
    }
}

/// Present one tool's result the way this connection is served it, before the
/// envelope is attached.
///
/// An entity body over [`crate::source_pages::SOURCE_PAGE_BYTES`] is served as
/// an excerpt with a cursor to the rest, on every connection, unless the call
/// asked for `full_body`; a call that sends that cursor is served the next
/// excerpt. Internal source reads never pass through here, so the bases a
/// write resolves are read whole.
///
/// Then an entity's source body is marked with each line's
/// offset in the entity, on a connection that numbers. A connection that can
/// write through Kin keeps the exact bytes, because it restates them, and so
/// does any client that asked for them, `kin agent run` among them; the
/// benchmark profiles keep their payload bytes because those are an input to a
/// citable result.
fn present_result(
    config: &McpServerConfig,
    tool: &str,
    arguments: &HashMap<String, serde_json::Value>,
    result: &mut ToolCallResult,
) {
    crate::source_pages::present(tool, arguments, result);
    if config.number_entity_lines && crate::entity_lines::numbers_this_tool(tool) {
        crate::entity_lines::number_entity_body(result);
    }
}

fn handle_initialize(
    id: Option<serde_json::Value>,
    params: &serde_json::Value,
    config: &McpServerConfig,
) -> JsonRpcResponse {
    // Check if the client requests a newer protocol version than we support.
    // We respond with our supported version and include a warning — we never
    // error, to remain forward-compatible.
    let client_version = params.get("protocolVersion").and_then(|v| v.as_str());
    let negotiated = negotiate_protocol_version(client_version);

    let mut result = serde_json::to_value(&InitializeResult {
        protocol_version: negotiated.into(),
        capabilities: ServerCapabilities {
            tools: ToolsCapability {
                list_changed: false,
            },
            prompts: crate::ask::dispatch::serves_prompt(config).then_some(
                crate::types::PromptsCapability {
                    list_changed: false,
                },
            ),
        },
        server_info: ServerInfo {
            name: config.server_name.clone(),
            version: config.server_version.clone(),
        },
        instructions: Some(instructions_with_store_notice(config)),
    })
    .unwrap_or_default();

    // Add kinVersion to serverInfo for Kin-aware clients.
    if let Some(info) = result.get_mut("serverInfo") {
        info["kinVersion"] = serde_json::json!(config.server_version);
    }

    // Say so when the client is answered with a version it did not ask for.
    if let Some(client_version) = client_version.filter(|version| *version != negotiated) {
        result["_warning"] = serde_json::json!(format!(
            "client requested protocol version '{client_version}', server supports \
             '{negotiated}'; falling back to server version"
        ));
    }

    JsonRpcResponse::success(id, result)
}

fn handle_tools_list(id: Option<serde_json::Value>, config: &McpServerConfig) -> JsonRpcResponse {
    let tools = served_tools_for(config);
    let mut listing = serde_json::to_value(&tools).unwrap_or_default();
    if serves_structured_output(config) {
        advertise_output_schemas(&mut listing);
    }
    JsonRpcResponse::success(id, listing)
}

/// Add each typed verb's `outputSchema` to a served listing.
fn advertise_output_schemas(listing: &mut serde_json::Value) {
    let Some(tools) = listing
        .get_mut("tools")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return;
    };
    for tool in tools {
        let schema = tool
            .get("name")
            .and_then(serde_json::Value::as_str)
            .and_then(crate::replies::output_schema);
        if let (Some(schema), Some(tool)) = (schema, tool.as_object_mut()) {
            tool.insert("outputSchema".into(), schema);
        }
    }
}

/// Restate a successful verb answer's machine JSON as `structuredContent`,
/// beside the text that carries the same JSON, where this connection serves
/// structured output. A refusal carries none: the specification sets no rule
/// for one, and a client SDK that validates it would reject the refusal.
fn attach_structured_content(response: &mut JsonRpcResponse, request: &str) {
    let Ok(request) = serde_json::from_str::<serde_json::Value>(request) else {
        return;
    };
    if request.get("method").and_then(serde_json::Value::as_str) != Some("tools/call") {
        return;
    }
    let Some(tool) = request
        .pointer("/params/name")
        .and_then(serde_json::Value::as_str)
        .filter(|tool| crate::replies::output_schema(tool).is_some())
    else {
        return;
    };
    let Some(result) = response.result.as_mut() else {
        return;
    };
    if result.get("isError").and_then(serde_json::Value::as_bool) == Some(true) {
        return;
    }
    let text = result
        .pointer("/content/0/text")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let structured = match serde_json::from_str::<serde_json::Value>(text) {
        Ok(payload @ serde_json::Value::Object(_)) => payload,
        // A tool that answered in words still owes the structured content its
        // listing promised; the words are its one field.
        _ => serde_json::json!({ "text": text }),
    };
    if let Err(problem) = crate::replies::decode(tool, &structured) {
        tracing::warn!("{tool} answered outside its typed reply: {problem}");
    }
    result["structuredContent"] = structured;
}

async fn handle_tools_call<G: PersistableMcpStore>(
    id: Option<serde_json::Value>,
    params: &serde_json::Value,
    store: &G,
    sessions: &SessionRegistry,
    config: &McpServerConfig,
) -> JsonRpcResponse {
    let mut call_params: ToolCallParams = match serde_json::from_value(params.clone()) {
        Ok(p) => p,
        Err(e) => {
            return JsonRpcResponse::error(id, -32602, format!("Invalid params: {}", e));
        }
    };
    // A routed call becomes the named call it stands for before anything reads
    // its name, so everything below, the dispatcher and the envelope included,
    // treats it as that named call. `describe` and a call that did not validate
    // are answered here, through the same envelope.
    let routed_call = match crate::routed::route(&mut call_params, config.routed) {
        crate::routed::Routing::Answer(result) => {
            let budget = ResponseBudget::from_arguments(&call_params.arguments);
            return offline_envelope_success(id, result, crate::routed::TOOL_NAME, &budget);
        }
        crate::routed::Routing::Dispatch => true,
        crate::routed::Routing::NotRouted => false,
    };
    // The front door runs other verbs through this same path and composes
    // their answers; see `crate::ask`.
    if call_params.name == crate::ask::TOOL_NAME
        && (routed_call || crate::ask::dispatch::served(config))
    {
        let result =
            crate::ask::dispatch::answer_offline(&call_params.arguments, store, sessions, config)
                .await;
        return JsonRpcResponse::success(id, serde_json::to_value(&result).unwrap_or_default());
    }
    let overridden = agent_profile_overrides(&mut call_params, config);
    let mut response =
        dispatch_tools_call(id, &mut call_params, routed_call, store, sessions, config).await;
    if routed_call {
        present_routed_hints(&mut response, &call_params);
    }
    append_note(&mut response, overridden);
    response
}

/// Present a finished `tools/call` answer for this connection, then enforce
/// locate's final byte ceiling. Agent profiles use [`crate::plain_reply`].
///
/// Applied where an answer is written to the client, after everything that
/// reads the machine reply has run: paging, the response budget, and the
/// check that a page carries the repository and client folder it was
/// budgeted for. Locate's ceiling applies to every client presentation; other
/// tools retain their own budget policy. `kin call` applies this to the answer
/// it prints, so a shell is answered the way that profile's client is.
pub fn present_for_client(
    response: JsonRpcResponse,
    request: &str,
    config: &McpServerConfig,
) -> JsonRpcResponse {
    // A plain locate reply is cut to its budget as the reply that ships.
    if let Some(fit) = presented_fit(request, config) {
        return present_fitted(response, request, config, &fit);
    }
    // Read the normalized caller/profile budget before plain presentation can
    // remove its machine accounting. Size only the representation that ships.
    let locate_ceiling = finalized_locate_ceiling(&response, request, config);
    let reference_ceiling = presented_page_ceiling(request, config);
    let mut deferred_locate_details = None;
    let mut response = present_for_client_inner(
        response,
        request,
        config,
        locate_ceiling
            .is_some()
            .then_some(&mut deferred_locate_details),
    );
    if let (Some(max_chars), Some(result)) = (
        locate_ceiling.or(reference_ceiling),
        response.result.as_mut(),
    ) {
        crate::budget::enforce_final_content_ceiling(result, max_chars);
    }
    if let (Some(details), Some(result)) = (deferred_locate_details, response.result.as_ref()) {
        config
            .query_presentation
            .remember_delivered(result, "semantic_locate", details);
    }
    write_for_agent(&mut response, request, config);
    response
}

/// The last step of an agent reply on a connection that serves session
/// handles: the reply in compact text, or its JSON with every long opaque
/// value shortened. Runs after every byte budget, so it only ever shortens.
fn write_for_agent(response: &mut JsonRpcResponse, request: &str, config: &McpServerConfig) {
    write_for_agent_with(response, request, config, config.active_handles());
}

/// [`write_for_agent`] against a given handle table: the connection's own, or
/// a scratch copy of it for a reply that is only being sized.
fn write_for_agent_with(
    response: &mut JsonRpcResponse,
    request: &str,
    config: &McpServerConfig,
    handles: Option<&crate::session_handles::SessionHandles>,
) {
    let Some(handles) = handles else {
        return;
    };
    let Some(result) = response.result.as_mut() else {
        return;
    };
    let Ok(request) = serde_json::from_str::<serde_json::Value>(request) else {
        return;
    };
    if request.get("method").and_then(serde_json::Value::as_str) != Some("tools/call") {
        return;
    }
    if config.reply_format == crate::text_reply::ReplyFormat::Text {
        let tool = serde_json::from_value::<ToolCallParams>(
            request.get("params").cloned().unwrap_or_default(),
        )
        .ok()
        .map(|mut call| {
            let _ = crate::routed::route(&mut call, config.routed);
            let _ = crate::tool_invocation::expand(&mut call, config.allowed_tools.as_ref());
            crate::verbs::handler_name(&call.name, &call.arguments)
        })
        .unwrap_or_default();
        crate::text_reply::render_result(result, &tool, handles);
    } else {
        handles.shorten_result(result);
    }
}

/// The tools whose plain reply is cut to its budget as the reply that ships:
/// `locate` by name and by description.
const PRESENTATION_FITTED_TOOLS: [&str; 2] = ["semantic_locate", "semantic_search"];

/// Whether this connection's reply to `tool` is sized by what it delivers.
/// Machine replies and a citable connection keep the machine accounting.
fn fits_presentation(tool: &str, config: &McpServerConfig) -> bool {
    config.plain_replies && !config.citable && PRESENTATION_FITTED_TOOLS.contains(&tool)
}

/// Ask for the machine answer of a fitted reply at the widest budget.
///
/// The budget a caller names is a budget for the reply it reads. The machine
/// answer behind a plain reply is several times that reply's size: measured on
/// React, a name lookup with two declarations was 17,842 characters as a
/// machine answer and 1,418 as delivered, and the 12,000 default withheld one
/// of the two declarations over a form no reader receives. So the arms that
/// bound the machine answer are given the widest budget they serve, and
/// [`present_fitted`] holds the reply to the caller's.
fn widen_machine_budget(call: &mut ToolCallParams, config: &McpServerConfig) {
    if !fits_presentation(&call.name, config) && !pages_machine_locate(&call.name, config) {
        return;
    }
    call.arguments.remove("max_response_chars");
    call.arguments.insert(
        "max_chars".to_string(),
        serde_json::json!(crate::budget::RESPONSE_MAX_MAX_CHARS),
    );
}

/// A machine locate held to its caller's budget by paging its whole answer
/// ([`crate::locate_pages`]) rather than cutting rows from it. Its machine
/// answer is asked for whole, as a plain reply's is. A plain reply fits itself
/// ([`present_fitted`]), and a citable one keeps its exact bytes.
fn pages_machine_locate(tool: &str, config: &McpServerConfig) -> bool {
    tool == crate::locate_pages::TOOL && !config.plain_replies && !config.citable
}

/// What a frozen locate answer was read from, which a continuation must still
/// be reading to resume it. `None` when nothing names it, and then no
/// continuation is handed out.
fn locate_page_authority(graph: Option<serde_json::Value>) -> Option<serde_json::Value> {
    graph
        .filter(|graph| !graph.is_null())
        .map(|graph| serde_json::json!({ "graph": graph }))
}

/// The graph a daemon answer is read under, as its envelope names it.
fn daemon_graph_as_of(health: Option<&serde_json::Value>) -> Option<serde_json::Value> {
    health.and_then(|health| Envelope::daemon().with_health(health).graph_as_of)
}

/// A call whose reply [`present_fitted`] sizes, and the budget its caller
/// named or the profile gave it.
struct PresentedFit {
    tool: String,
    budget: ResponseBudget,
}

/// Read from the call as it was sent, the way [`presented_page_ceiling`]
/// reads its own, so the budget is the caller's and never the widened one.
fn presented_fit(request: &str, config: &McpServerConfig) -> Option<PresentedFit> {
    if !config.plain_replies || config.citable {
        return None;
    }
    let request = serde_json::from_str::<serde_json::Value>(request).ok()?;
    if request.get("method").and_then(serde_json::Value::as_str) != Some("tools/call") {
        return None;
    }
    let mut call = serde_json::from_value::<ToolCallParams>(request.get("params")?.clone()).ok()?;
    if matches!(
        crate::routed::route(&mut call, config.routed),
        crate::routed::Routing::Answer(_)
    ) || crate::tool_invocation::expand(&mut call, config.allowed_tools.as_ref()).is_err()
        || crate::verbs::resolve(&mut call.name, &mut call.arguments).is_err()
        || !fits_presentation(&call.name, config)
    {
        return None;
    }
    if config.agent_belt {
        crate::agent_belt::apply_belt_defaults(&call.name, &mut call.arguments);
    }
    Some(PresentedFit {
        budget: ResponseBudget::from_arguments(&call.arguments),
        tool: call.name,
    })
}

/// What a fitted reply sheds before it withholds a row, in this order.
///
/// Each step keeps what the one before it shed. Only when the reply with all
/// three shed is still over its budget is a row withheld.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum PresentedShed {
    /// The reply as it was built.
    Nothing,
    /// Why each `not_held` declaration is not held. Its module, name and kind
    /// stay, and so does the count in the answer line.
    NotHeldDetail,
    /// Everything beside the answer: any top-level block but the rows, the
    /// query, the total, the cursor and the not-held lists, and any line of
    /// the verdict block but the verdict, its meaning, what was delivered,
    /// what was searched and the fix.
    Explanations,
    /// Everything on a row but what names it: its entity id, module, name and
    /// kind. A signature or a body is read with `source` on the row.
    RowExtras,
}

impl PresentedShed {
    fn next(self) -> Option<Self> {
        match self {
            Self::Nothing => Some(Self::NotHeldDetail),
            Self::NotHeldDetail => Some(Self::Explanations),
            Self::Explanations => Some(Self::RowExtras),
            Self::RowExtras => None,
        }
    }
}

/// The top-level blocks of a plain locate reply that are its answer.
const PRESENTED_ANSWER_KEYS: [&str; 9] = [
    crate::envelope::ENVELOPE_KEY,
    "query",
    "results",
    "total",
    "next_cursor",
    "not_held",
    crate::plain_reply::NOT_HELD_ATTRIBUTES_KEY,
    "error",
    "message",
];

/// How many declarations a cut name lookup names in its one line of what it
/// did not show. The rest are counted.
pub(crate) const NAMED_NOT_SHOWN: usize = 12;

/// One row of a plain reply, as far as telling rows apart needs.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PresentedRow {
    id: String,
    /// The module the row sits in, or its name where the reply gives none.
    place: String,
}

fn presented_reply(
    result: &serde_json::Value,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    if result.get("isError").and_then(serde_json::Value::as_bool) == Some(true) {
        return None;
    }
    let text = result.pointer("/content/0/text")?.as_str()?;
    match serde_json::from_str::<serde_json::Value>(text).ok()? {
        serde_json::Value::Object(reply) => Some(reply),
        _ => None,
    }
}

fn presented_rows(response: &JsonRpcResponse) -> Vec<PresentedRow> {
    response
        .result
        .as_ref()
        .and_then(presented_reply)
        .and_then(|reply| {
            reply
                .get("results")
                .and_then(serde_json::Value::as_array)
                .cloned()
        })
        .unwrap_or_default()
        .iter()
        .map(|row| {
            let text = |key: &str| row.get(key).and_then(serde_json::Value::as_str);
            PresentedRow {
                id: text("entity_id")
                    .or(text("id"))
                    .unwrap_or_default()
                    .to_string(),
                place: text("module")
                    .or(text("name"))
                    .unwrap_or_default()
                    .to_string(),
            }
        })
        .collect()
}

/// Shed optional detail from a plain reply, through `shed`, and say so.
fn shed_presented(reply: &mut serde_json::Map<String, serde_json::Value>, shed: PresentedShed) {
    use serde_json::Value;
    let mut shed_something = false;
    if shed >= PresentedShed::NotHeldDetail {
        for row in reply
            .get_mut("not_held")
            .and_then(Value::as_array_mut)
            .into_iter()
            .flatten()
            .filter_map(Value::as_object_mut)
        {
            let before = row.len();
            row.retain(|key, _| matches!(key.as_str(), "module" | "name" | "kind"));
            shed_something |= row.len() != before;
        }
        if let Some(attributes) = reply
            .get_mut(crate::plain_reply::NOT_HELD_ATTRIBUTES_KEY)
            .and_then(Value::as_object_mut)
        {
            shed_something |= attributes.remove("why").is_some();
        }
    }
    if shed >= PresentedShed::Explanations {
        let before = reply.len();
        reply.retain(|key, _| PRESENTED_ANSWER_KEYS.contains(&key.as_str()));
        shed_something |= reply.len() != before;
        if let Some(said) = reply
            .get_mut(crate::envelope::ENVELOPE_KEY)
            .and_then(Value::as_object_mut)
        {
            let before = said.len();
            said.retain(|key, _| {
                matches!(
                    key.as_str(),
                    "verdict" | "meaning" | "delivered" | "searched" | "fix"
                )
            });
            shed_something |= said.len() != before;
        }
    }
    if shed >= PresentedShed::RowExtras {
        for row in reply
            .get_mut("results")
            .and_then(Value::as_array_mut)
            .into_iter()
            .flatten()
            .filter_map(Value::as_object_mut)
        {
            let before = row.len();
            row.retain(|key, _| {
                matches!(
                    key.as_str(),
                    "entity_id" | "id" | "artifact" | "module" | "name" | "kind"
                )
            });
            shed_something |= row.len() != before;
        }
    }
    if shed_something {
        say_delivered(
            reply,
            &format!(
                "Detail was left out to fit this reply; ask again with a larger {}, up to {}, to see it.",
                crate::request_schema::OUTPUT_BUDGET,
                crate::budget::RESPONSE_MAX_MAX_CHARS
            ),
        );
    }
}

/// Add one sentence to what a plain reply says it delivered.
fn say_delivered(reply: &mut serde_json::Map<String, serde_json::Value>, sentence: &str) {
    let Some(said) = reply
        .get_mut(crate::envelope::ENVELOPE_KEY)
        .and_then(serde_json::Value::as_object_mut)
    else {
        return;
    };
    let delivered = match said.get("delivered").and_then(serde_json::Value::as_str) {
        Some(already) if !already.is_empty() => format!("{already} {sentence}"),
        _ => sentence.to_string(),
    };
    said.insert(
        "delivered".to_string(),
        serde_json::Value::String(delivered),
    );
}

/// Name, in one line, the declarations of `whole` this reply does not show.
///
/// A name lookup has no cursor, so a declaration that did not fit is reached
/// only by asking again. The reply already says how many there are; this says
/// which, by module, so the rows shown are never read as the only ones.
fn name_rows_not_shown(
    reply: &mut serde_json::Map<String, serde_json::Value>,
    whole: &[PresentedRow],
) {
    let shown: HashSet<String> = reply
        .get("results")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|row| {
            row.get("entity_id")
                .or_else(|| row.get("id"))
                .and_then(serde_json::Value::as_str)
        })
        .map(str::to_string)
        .collect();
    // One entry a module, in the order its first declaration ranked, with how
    // many of its declarations are not shown when that is more than one.
    let mut modules: Vec<(&str, usize)> = Vec::new();
    for row in whole.iter().filter(|row| !shown.contains(&row.id)) {
        match modules.iter_mut().find(|(place, _)| *place == row.place) {
            Some((_, count)) => *count += 1,
            None => modules.push((row.place.as_str(), 1)),
        }
    }
    if modules.is_empty() {
        return;
    }
    let named: Vec<String> = modules
        .iter()
        .take(NAMED_NOT_SHOWN)
        .map(|(place, count)| match count {
            1 => (*place).to_string(),
            _ => format!("{place} ({count})"),
        })
        .collect();
    let more: usize = modules
        .iter()
        .skip(NAMED_NOT_SHOWN)
        .map(|(_, count)| count)
        .sum();
    let mut sentence = format!("Not shown, by module: {}", named.join(", "));
    if more > 0 {
        sentence.push_str(&format!(" and {more} more"));
    }
    sentence.push('.');
    say_delivered(reply, &sentence);
}

/// The bytes a client receives: every text block of the result.
fn delivered_bytes(response: &JsonRpcResponse) -> usize {
    response
        .result
        .as_ref()
        .and_then(|result| result.get("content"))
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|block| block.get("text").and_then(serde_json::Value::as_str))
        .fold(0usize, |sum, text| sum.saturating_add(text.len()))
}

/// A machine answer as this connection delivers it: presented plainly, shed
/// through `shed`, with the rows of `whole` it does not show named, and then
/// written in the connection's format against `handles`.
///
/// `remember` is for the one reply that ships: it records the detail choice
/// its cursor carries, which a reply that is only being sized must not.
fn deliver_presented(
    machine: JsonRpcResponse,
    request: &str,
    config: &McpServerConfig,
    shed: PresentedShed,
    whole: Option<&[PresentedRow]>,
    handles: Option<&crate::session_handles::SessionHandles>,
    remember: bool,
) -> JsonRpcResponse {
    let mut deferred_details = None;
    let mut response =
        present_for_client_inner(machine, request, config, Some(&mut deferred_details));
    if let Some(result) = response.result.as_mut() {
        if let Some(mut reply) = presented_reply(result) {
            let before = reply.clone();
            shed_presented(&mut reply, shed);
            if let Some(whole) = whole {
                name_rows_not_shown(&mut reply, whole);
            }
            if reply != before {
                if let Ok(text) = serde_json::to_string(&reply) {
                    result["content"][0]["text"] = serde_json::Value::String(text);
                }
            }
        }
        if let (true, Some(details)) = (remember, deferred_details) {
            config
                .query_presentation
                .remember_delivered(result, "semantic_locate", details);
        }
    }
    write_for_agent_with(&mut response, request, config, handles);
    response
}

/// Present a locate answer and hold the reply that ships to its budget.
///
/// The budget is a number of bytes the client receives, under this
/// connection's profile and format. The machine answer arrives built under
/// the widest budget ([`widen_machine_budget`]) and is cut here, in a fixed
/// order, measuring the delivered reply at every step:
///
/// 1. the reply as built;
/// 2. without why each not-held declaration is not held;
/// 3. without anything beside the answer;
/// 4. without anything on a row but what names it ([`PresentedShed`]);
/// 5. without rows, the farthest first. The machine answer is bounded again
///    under the widest budget whose delivered reply fits, so the cut is the
///    one every budget makes: disclosed, with a ranking's cursor moved back to
///    the first row not shown. A name lookup has no cursor, and names the
///    declarations it did not show by module.
///
/// So a row is withheld only when the delivered reply would not fit with it,
/// and never while optional detail is still on the page. A reply that cannot
/// fit even its first row is refused, as before.
fn present_fitted(
    machine: JsonRpcResponse,
    request: &str,
    config: &McpServerConfig,
    fit: &PresentedFit,
) -> JsonRpcResponse {
    let limit = fit.budget.max_chars;
    let scratch = config
        .active_handles()
        .map(crate::session_handles::SessionHandles::scratch);
    let sized = |machine: &JsonRpcResponse, shed, whole: Option<&[PresentedRow]>| {
        deliver_presented(
            machine.clone(),
            request,
            config,
            shed,
            whole,
            scratch.as_ref(),
            false,
        )
    };
    let ship = |machine: JsonRpcResponse, shed, whole: Option<&[PresentedRow]>| {
        deliver_presented(
            machine,
            request,
            config,
            shed,
            whole,
            config.active_handles(),
            true,
        )
    };

    let mut shed = PresentedShed::Nothing;
    loop {
        if delivered_bytes(&sized(&machine, shed, None)) <= limit {
            return ship(machine, shed, None);
        }
        match shed.next() {
            Some(next) => shed = next,
            None => break,
        }
    }

    let refuse = |mut response: JsonRpcResponse| {
        if let Some(result) = response.result.as_mut() {
            crate::budget::enforce_final_content_ceiling(result, limit);
        }
        response
    };
    let machine_text = machine
        .result
        .as_ref()
        .and_then(|result| result.pointer("/content/0/text"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let Some((text, payload)) = machine_text.and_then(|text| {
        let payload = serde_json::from_str::<serde_json::Value>(&text).ok()?;
        payload.is_object().then_some((text, payload))
    }) else {
        return refuse(sized(&machine, shed, None));
    };
    // The rows of the whole answer, read from the plain reply before it is
    // written in the connection's format.
    let whole = (fit.tool == "semantic_search").then(|| {
        presented_rows(&present_for_client_inner(
            machine.clone(),
            request,
            config,
            Some(&mut None),
        ))
    });
    let bounded = |max_chars: usize| {
        let mut cut = payload.clone();
        envelope::rebound(
            &mut cut,
            &fit.tool,
            &ResponseBudget {
                max_chars,
                explicit_max_chars: true,
                envelope_reserve: 0,
                ..fit.budget
            },
        );
        let mut candidate = machine.clone();
        if let (Some(result), Ok(rendered)) = (
            candidate.result.as_mut(),
            crate::budget::render_in_format_of(&cut, &text),
        ) {
            result["content"][0]["text"] = serde_json::Value::String(rendered);
        }
        candidate
    };
    let (mut low, mut high) = (
        crate::budget::RESPONSE_MIN_MAX_CHARS,
        crate::budget::measure(&payload).saturating_sub(1),
    );
    let mut fitted = None;
    while low <= high {
        let middle = low + (high - low) / 2;
        let candidate = bounded(middle);
        if delivered_bytes(&sized(&candidate, shed, whole.as_deref())) <= limit {
            fitted = Some(candidate);
            low = middle + 1;
        } else {
            high = middle - 1;
        }
    }
    match fitted {
        Some(candidate) => ship(candidate, shed, whole.as_deref()),
        None => refuse(sized(
            &bounded(crate::budget::RESPONSE_MIN_MAX_CHARS),
            shed,
            whole.as_deref(),
        )),
    }
}

/// The byte budget a plain `refs` or `trace` reply is held to where it
/// reaches the client: the one its pages were cut to, read from the call the
/// way it was dispatched, so the caller's `max_output_bytes`, or the
/// profile's default when it named none.
///
/// The pager sizes every reference and walk page so its plain presentation
/// fits this budget, and moves what does not fit to the next page under the
/// answer's own cursor. This is the guard behind it: a presentation that
/// still outgrows the budget is refused, never served over it. Machine
/// replies keep the pager's own accounting, a citable connection its exact
/// bytes, and the compact trace profile its own codec.
fn presented_page_ceiling(request: &str, config: &McpServerConfig) -> Option<usize> {
    if !config.plain_replies || config.citable {
        return None;
    }
    let request = serde_json::from_str::<serde_json::Value>(request).ok()?;
    if request.get("method").and_then(serde_json::Value::as_str) != Some("tools/call") {
        return None;
    }
    let mut call = serde_json::from_value::<ToolCallParams>(request.get("params")?.clone()).ok()?;
    if matches!(
        crate::routed::route(&mut call, config.routed),
        crate::routed::Routing::Answer(_)
    ) || crate::tool_invocation::expand(&mut call, config.allowed_tools.as_ref()).is_err()
        || crate::verbs::resolve(&mut call.name, &mut call.arguments).is_err()
        || !(call.name == "find_references"
            || call.name == "trace_data_flow" && !config.compact_trace_evidence)
    {
        return None;
    }
    if config.agent_belt {
        crate::agent_belt::apply_belt_defaults(&call.name, &mut call.arguments);
    }
    Some(ResponseBudget::from_arguments(&call.arguments).max_chars)
}

fn finalized_locate_ceiling(
    response: &JsonRpcResponse,
    request: &str,
    config: &McpServerConfig,
) -> Option<usize> {
    let request = serde_json::from_str::<serde_json::Value>(request).ok()?;
    if request.get("method").and_then(serde_json::Value::as_str) != Some("tools/call") {
        return None;
    }
    let mut call = serde_json::from_value::<ToolCallParams>(request.get("params")?.clone()).ok()?;
    if matches!(
        crate::routed::route(&mut call, config.routed),
        crate::routed::Routing::Answer(_)
    ) || crate::tool_invocation::expand(&mut call, config.allowed_tools.as_ref()).is_err()
        || crate::verbs::handler_name(&call.name, &call.arguments) != "semantic_locate"
    {
        return None;
    }
    let text = response
        .result
        .as_ref()?
        .pointer("/content/0/text")?
        .as_str()?;
    let payload = serde_json::from_str::<serde_json::Value>(text).ok()?;
    payload
        .pointer("/_kin/response/max_chars")?
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())
}

fn present_for_client_inner(
    mut response: JsonRpcResponse,
    request: &str,
    config: &McpServerConfig,
    deferred_locate_details: Option<&mut Option<bool>>,
) -> JsonRpcResponse {
    if let Ok(incoming) = serde_json::from_str::<serde_json::Value>(request) {
        if incoming.get("method").and_then(serde_json::Value::as_str) == Some("tools/call") {
            let effective = serde_json::from_value::<ToolCallParams>(
                incoming.get("params").cloned().unwrap_or_default(),
            )
            .ok()
            .map(|mut call| {
                let _ = crate::routed::route(&mut call, config.routed);
                let _ = crate::tool_invocation::expand(&mut call, config.allowed_tools.as_ref());
                crate::verbs::handler_name(&call.name, &call.arguments)
            })
            .unwrap_or_default();
            if let Some(result) = response.result.as_mut() {
                crate::semantic_surface::present_result_with(result, &effective, !config.citable);
            }
            if let Some(error) = response.error.as_mut() {
                error.message = crate::semantic_surface::diagnostic(&error.message);
            }
        }
    }
    if serves_structured_output(config) {
        attach_structured_content(&mut response, request);
    }
    if !config.plain_replies || config.citable {
        return response;
    }
    let Ok(request) = serde_json::from_str::<serde_json::Value>(request) else {
        return response;
    };
    if request.get("method").and_then(serde_json::Value::as_str) != Some("tools/call") {
        return response;
    }
    let params = request.get("params").cloned().unwrap_or_default();
    let name = params
        .get("name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let arguments: HashMap<String, serde_json::Value> = params
        .get("arguments")
        .and_then(|arguments| serde_json::from_value(arguments.clone()).ok())
        .unwrap_or_default();
    let tool = crate::verbs::handler_name(name, &arguments);
    // A details choice is read from the effective call, including one reached
    // through the routed or discovered surface. It never changes selection or
    // the authority of the frozen answer.
    let query_call = serde_json::from_value::<ToolCallParams>(params.clone())
        .ok()
        .and_then(|mut call| {
            let routing = crate::routed::route(&mut call, config.routed);
            if matches!(routing, crate::routed::Routing::Answer(_))
                || crate::tool_invocation::expand(&mut call, config.allowed_tools.as_ref()).is_err()
            {
                return None;
            }
            let effective = crate::verbs::handler_name(&call.name, &call.arguments);
            crate::query_reply::supported(&effective).then_some((effective, call.arguments))
        });
    if let Some(result) = response.result.as_mut() {
        let mut compact_trace_arguments = None;
        if config.compact_trace_evidence {
            let mut call: ToolCallParams = match serde_json::from_value(params) {
                Ok(call) => call,
                Err(_) => return response,
            };
            let routing = crate::routed::route(&mut call, config.routed);
            if !matches!(routing, crate::routed::Routing::Answer(_))
                && crate::tool_invocation::expand(&mut call, config.allowed_tools.as_ref()).is_ok()
            {
                let effective = crate::verbs::handler_name(&call.name, &call.arguments);
                if effective == "trace_data_flow" {
                    compact_trace_arguments = Some(call.arguments);
                }
            }
        }
        if let Some(arguments) = compact_trace_arguments {
            crate::trace_page_reply::present(result, &arguments);
        } else if let Some((tool, arguments)) = query_call {
            if let ("semantic_locate", Some(deferred)) = (tool.as_str(), deferred_locate_details) {
                *deferred = Some(
                    config
                        .query_presentation
                        .present_deferred(result, &tool, &arguments),
                );
            } else {
                config.query_presentation.present(result, &tool, &arguments);
            }
        } else {
            // An agent's impact answer that fits one page ends on what must
            // change and its open places.
            let answered_impact = tool == crate::impact_pages::TOOL
                && config.agent_belt
                && crate::reply_budget::impact_single_page(result);
            crate::plain_reply::present(result, &tool);
            if answered_impact {
                crate::reply_budget::present_answered_impact(result);
            }
            if tool == "graph_neighborhood" {
                crate::reply_budget::present_neighbors(result);
            }
        }
        crate::lean_reply::lean_result(result);
    }
    // A refused call names each tool the way this profile serves it too.
    if let Some(error) = response.error.as_mut() {
        error.message = crate::verbs::speak_verbs(&error.message);
    }
    response
}

/// What an agent profile does not take from this call, taken out before it is
/// dispatched, and the one line that says so. See
/// [`crate::agent_belt::agent_profile_overrides`]. Read against the registered
/// tool the call resolves to, so a verb and the name it replaced are alike.
fn agent_profile_overrides(call: &mut ToolCallParams, config: &McpServerConfig) -> Option<String> {
    if !config.agent_belt {
        return None;
    }
    let tool = crate::verbs::handler_name(&call.name, &call.arguments);
    crate::agent_belt::agent_profile_overrides(&tool, &mut call.arguments)
}

/// Add `note` to a tool answer as a line of its own, after the answer, so the
/// answer's own payload and envelope are served exactly as the tool wrote them.
fn append_note(response: &mut JsonRpcResponse, note: Option<String>) {
    let (Some(note), Some(result)) = (note, response.result.as_mut()) else {
        return;
    };
    if let Some(content) = result
        .get_mut("content")
        .and_then(|value| value.as_array_mut())
    {
        content.push(serde_json::json!({"type": "text", "text": note}));
    }
}

/// The offline route from a named call to its enveloped answer.
async fn dispatch_tools_call<G: PersistableMcpStore>(
    id: Option<serde_json::Value>,
    call_params: &mut ToolCallParams,
    routed_call: bool,
    store: &G,
    sessions: &SessionRegistry,
    config: &McpServerConfig,
) -> JsonRpcResponse {
    // A routed connection's dispatcher is the routed tool, so the discovery
    // dispatcher is answered as the named call it is, with the refusal that
    // names the routed command.
    let discovered_call = if config.routed.is_some() {
        false
    } else {
        match crate::tool_invocation::expand(call_params, config.allowed_tools.as_ref()) {
            Ok(value) => value,
            Err(error) => return JsonRpcResponse::error(id, -32602, error.to_string()),
        }
    };
    // Resolve the served name to the registered one here, once, before anything
    // keyed on a tool name reads it: the profile filter below, the dispatcher,
    // the response-budget shape, the negative-evidence spec and the envelope.
    // A verb becomes the registered tool that answers it, with that tool's
    // arguments, and a verb call that names no valid form is answered with the
    // one that works. The names the verbs replaced, and `find_declarations`,
    // are still accepted here; everything internal stays keyed on the
    // registered name.
    let served_name = call_params.name.clone();
    if let Err(refusal) = crate::verbs::resolve(&mut call_params.name, &mut call_params.arguments) {
        let budget = ResponseBudget::from_arguments(&call_params.arguments);
        let refusal = unserved_call(config, call_params, discovered_call, routed_call)
            .unwrap_or_else(|| ToolCallResult::error(refusal));
        return offline_envelope_success(id, refusal, &served_name, &budget);
    }
    // Refuse before forwarding too: an older daemon must not restore retired
    // file operations on an unfiltered connection.
    if let Some(message) = crate::tools::retired_file_operation(&call_params.name) {
        return JsonRpcResponse::error(id, -32602, message.into());
    }
    // The belt asks for the compact locate shape on its agents' behalf, only
    // when the caller named no surface. Applied here rather than in the daemon
    // because this is the one layer that knows which profile is being served.
    if config.agent_belt {
        crate::agent_belt::apply_belt_defaults(&call_params.name, &mut call_params.arguments);
    }
    // The caller's own budget, read before the machine answer is asked for
    // whole. A machine locate is held to it by paging.
    let caller_budget = ResponseBudget::from_arguments(&call_params.arguments);
    let locate_page = pages_machine_locate(&call_params.name, config);
    // The question a frozen answer is keyed by, read where a continuation of
    // it is read, before anything below adds to the call.
    let locate_arguments = locate_page.then(|| call_params.arguments.clone());
    if locate_page {
        if let Some(cursor) = crate::locate_pages::page_cursor(&call_params.arguments) {
            let authority = locate_page_authority(store.trace_page_authority().map(
                |store| serde_json::json!({"store": store, "snapshot_path": config.snapshot_path}),
            ));
            let result = match authority {
                Some(authority) => crate::locate_pages::resume(
                    cursor,
                    &crate::locate_pages::context(&call_params.arguments, &authority),
                    caller_budget.max_chars,
                )
                .map(|page| ToolCallResult::text(page.to_string()))
                .unwrap_or_else(ToolCallResult::error),
                None => ToolCallResult::error(
                    "this store cannot attest locate cursor revisions; restart without cursor",
                ),
            };
            return offline_envelope_success(id, result, &call_params.name, &caller_budget);
        }
    }
    // A plain locate reply is cut to its budget as the reply that ships, so
    // its machine answer is asked for whole.
    widen_machine_budget(call_params, config);
    let budget = ResponseBudget::from_arguments(&call_params.arguments);

    if let Some(allowed) = &config.allowed_tools {
        if !discovered_call && !routed_call && !crate::verbs::allows(allowed, &call_params.name) {
            let error_result = not_enabled_refusal(config, &served_name, &call_params.name);
            return offline_envelope_success(id, error_result, &call_params.name, &budget);
        }
    }

    // The "at least one of" rules the served schemas no longer carry: every
    // served schema is a plain object, because provider tool APIs drop a tool
    // whose schema opens with a combinator. Refused here, before anything
    // runs, with one call that works.
    if let Some(refusal) = crate::input_contract::refusal(&call_params.name, &call_params.arguments)
    {
        return offline_envelope_success(id, refusal, &call_params.name, &budget);
    }

    // An entity an argument names by its label, its short id or its name is
    // read from the graph, with no connection state. See `entity_address`.
    if let Err(refusal) =
        crate::entity_address::resolve_call(store, &call_params.name, &mut call_params.arguments)
    {
        return offline_envelope_success(
            id,
            ToolCallResult::error(refusal),
            &call_params.name,
            &budget,
        );
    }

    // A write that copied a source read's ticket carries the exact base the
    // ticket stands for, read here from the store this route serves. See
    // `source_ticket`.
    let authority = config.repository_authority.as_ref();
    if !config.session_authority_mode.uses_daemon() {
        if let Err(refusal) = crate::source_ticket::resolve_call(
            &call_params.name,
            &mut call_params.arguments,
            |entity_id| {
                let read = crate::handlers::entities::handle_get_entity_source(
                    &crate::source_ticket::source_arguments(&entity_id),
                    store,
                    authority,
                )
                .map_err(|error| error.to_string())
                .and_then(|result| crate::source_ticket::source_reply(&result));
                async move { read }
            },
        )
        .await
        {
            let budget = ResponseBudget::from_arguments(&call_params.arguments);
            return offline_envelope_success(
                id,
                ToolCallResult::error(refusal),
                &call_params.name,
                &budget,
            );
        }
    }

    if call_params.name == "kin_graph_status" {
        // Offline graph status still cannot claim daemon counters or source
        // authority. Its actual registry can independently disclose staged work.
        let result = (|| -> std::result::Result<ToolCallResult, String> {
            let request =
                crate::status_pages::StatusRequest::from_arguments(&call_params.arguments)?;
            let unavailable =
                crate::handlers::entities::handle_graph_status(&call_params.arguments, store)
                    .map_err(|error| error.to_string())?;
            let result = crate::status_pages::with_open_transactions(
                unavailable,
                &sessions.open_staged_transactions(),
                &serde_json::json!({"registry":sessions as *const SessionRegistry as usize,
                    "store":store as *const G as usize,"snapshot":config.snapshot_path}),
            )?;
            crate::status_pages::page(result, &request, crate::status_pages::offline_cursor_key())
        })()
        .unwrap_or_else(ToolCallResult::error);
        let result = envelope::finalize(result, Envelope::offline(), &call_params.name);
        let result = crate::status_pages::enforce_ceiling(result, budget.max_chars);
        return JsonRpcResponse::success(id, serde_json::to_value(result).unwrap_or_default());
    }

    if call_params.name == crate::handlers::tool_search::TOOL_NAME {
        let result = crate::handlers::tool_search::handle_tool_search_with_profile(
            &call_params.arguments,
            config.allowed_tools.as_ref(),
        )
        .unwrap_or_else(|error| ToolCallResult::error(error.to_string()));
        return offline_envelope_success(id, result, &call_params.name, &budget);
    }

    let reference_page = call_params.name == "find_references";
    if reference_page {
        if let Err(error) =
            crate::handlers::entities::validate_reference_proof_arguments(&call_params.arguments)
        {
            return offline_envelope_success(
                id,
                ToolCallResult::error(error.to_string()),
                &call_params.name,
                &budget,
            );
        }
    }
    let impact_page = call_params.name == crate::impact_pages::TOOL;
    let trace_authority = (call_params.name == "trace_data_flow" || reference_page || impact_page)
        .then(|| store.trace_page_authority())
        .flatten();
    let trace_context = trace_authority.as_ref().map(|authority| {
        let identity = serde_json::json!({
            "store": authority, "snapshot_path": config.snapshot_path,
        });
        if reference_page {
            crate::reference_pages::context(&call_params.arguments, &identity)
        } else if impact_page {
            crate::impact_pages::context(&call_params.arguments, &identity)
        } else {
            crate::trace_pages::Context::from_arguments(&call_params.arguments, &identity)
        }
    });
    if call_params.name == "trace_data_flow" || reference_page || impact_page {
        if call_params
            .arguments
            .get("cursor")
            .is_some_and(|value| !value.is_null() && !value.is_string())
        {
            return offline_envelope_success(
                id,
                ToolCallResult::error("cursor must be a string"),
                &call_params.name,
                &budget,
            );
        }
        if let Some(cursor) = call_params
            .arguments
            .get("cursor")
            .and_then(serde_json::Value::as_str)
        {
            let result = match trace_context.as_ref() {
                Some(context) => (if reference_page {
                    crate::reference_pages::resume(cursor, context, budget.max_chars)
                } else if impact_page {
                    crate::impact_pages::resume(cursor, context, budget.max_chars)
                } else {
                    crate::trace_pages::resume(cursor, context, budget.max_chars)
                })
                .and_then(|page| {
                    if store.trace_page_authority() != trace_authority {
                        return Err(
                            "trace graph changed while resuming; restart without cursor".into()
                        );
                    }
                    Ok(ToolCallResult::text(page.to_string()))
                })
                .unwrap_or_else(ToolCallResult::error),
                None => ToolCallResult::error(
                    "this store cannot attest trace cursor revisions; restart without cursor",
                ),
            };
            return offline_envelope_success(id, result, &call_params.name, &budget);
        }
    }

    let mut handler = std::pin::pin!(handle_tool_call(
        &call_params.name,
        &call_params.arguments,
        store,
        sessions,
        config.session_authority_mode,
        config.repository_authority.as_ref(),
        // No working copy on this route, and none is supposed to be here. It is
        // the explicit offline runtime, which serves a store rather than a
        // repository a daemon watches, and `kin mcp start` reaches the daemon
        // route instead (`session_authority_mode` is `DaemonRequired` there). A
        // probe built from this process's own working directory would be a
        // second, weaker guess at a repository this route never bound. So the
        // standing is `NotApplicable` rather than `Unchecked`: there is no disk
        // here that graph truth could be behind, which is a different fact from
        // a checkout nothing is watching.
        crate::working_copy::WorkingCopySurface::NotApplicable,
    ));
    let outcome = std::future::poll_fn(|cx| {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            std::future::Future::poll(handler.as_mut(), cx)
        })) {
            Ok(poll) => poll.map(Ok),
            Err(panic) => std::task::Poll::Ready(Err(panic)),
        }
    })
    .await;

    let call_result = match outcome {
        Ok(call_result) => call_result,
        Err(panic) => {
            let detail = panic
                .downcast_ref::<&str>()
                .map(|message| (*message).to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "tool handler panicked".to_string());
            return JsonRpcResponse::error(
                id,
                -32603,
                format!(
                    "Internal error: tool '{}' panicked: {detail}",
                    call_params.name
                ),
            );
        }
    };

    match call_result {
        Ok(mut result) => {
            present_result(
                config,
                &call_params.name,
                &call_params.arguments,
                &mut result,
            );
            if tool_requires_persist(&call_params.name) {
                if let Err(error) = store.persist_primary_snapshot(config.snapshot_path.as_deref())
                {
                    let error_result = ToolCallResult::error(format!(
                        "tool succeeded but snapshot persistence failed: {error}"
                    ));
                    return offline_envelope_success(id, error_result, &call_params.name, &budget);
                }
            }
            if let Some(context) = trace_context {
                if store.trace_page_authority() != trace_authority {
                    return offline_envelope_success(
                        id,
                        ToolCallResult::error(
                            "trace graph changed during the walk; restart without cursor",
                        ),
                        &call_params.name,
                        &budget,
                    );
                }
                let paged = if reference_page {
                    crate::reference_pages::finalize(result, Envelope::offline(), context, &budget)
                } else if impact_page {
                    crate::impact_pages::finalize(result, Envelope::offline(), context, &budget)
                } else {
                    crate::trace_pages::finalize(
                        result,
                        Envelope::offline(),
                        context,
                        budget.max_chars,
                    )
                };
                return JsonRpcResponse::success(
                    id,
                    serde_json::to_value(&paged).unwrap_or_default(),
                );
            }
            if locate_page {
                let enveloped = envelope::finalize_bounded(
                    result,
                    Envelope::offline(),
                    &call_params.name,
                    &budget,
                );
                let context = locate_page_authority(store.trace_page_authority().map(|store| {
                    serde_json::json!({"store": store, "snapshot_path": config.snapshot_path})
                }))
                .zip(locate_arguments.as_ref())
                .map(|(authority, arguments)| crate::locate_pages::context(arguments, &authority));
                let held = crate::locate_pages::finalize(enveloped, context, &caller_budget);
                return JsonRpcResponse::success(
                    id,
                    serde_json::to_value(&held).unwrap_or_default(),
                );
            }
            offline_envelope_success(id, result, &call_params.name, &budget)
        }
        Err(e) => offline_envelope_success(
            id,
            ToolCallResult::error(e.to_string()),
            &call_params.name,
            &budget,
        ),
    }
}

/// The refusal a call to a tool this profile does not serve gets. On a routed
/// connection it names the routed command that runs the tool there.
/// `served` is the name the caller sent and `registered` the tool it resolved
/// to: a named refusal repeats the caller's own spelling, and a routed one
/// names the command that runs the registered tool.
/// Fill in this connection's write session on a write that names none, opening
/// it on the first such write, and return the id used, or `None` when the call
/// named its own or needs none.
async fn adopt_connection_session(
    call: &mut ToolCallParams,
    config: &McpServerConfig,
) -> std::result::Result<Option<String>, ToolCallResult> {
    if !crate::connection_session::needs_connection_session(&call.name, &call.arguments) {
        return Ok(None);
    }
    let cwd = config.client_root.as_deref().ok_or_else(||
        ToolCallResult::error("This connection has no bound repository context for a session; nothing was written.")
    )?.display().to_string();
    let start = config.connection_session.start_arguments(&cwd);
    #[cfg(test)]
    let test_id = config.connection_session.next_test_id();
    let session = config
        .connection_session
        .id_or_start(|| async move {
            #[cfg(test)]
            if let Some(id) = test_id {
                return Ok(id);
            }
            open_daemon_session(start).await
        })
        .await
        .map_err(|error| {
            ToolCallResult::error(format!(
                "This write needs a session, and opening one for this connection failed: {error}. \
                 Nothing was written. Retry the write, or name a session_id from session start."
            ))
        })?;
    attach_process_for_writes(config, &session).await;
    call.arguments
        .insert("session_id".into(), serde_json::json!(session));
    Ok(Some(session))
}

/// Name this process on a session that was opened for reads, the first time
/// it writes, so the daemon keeps it while the process runs as it keeps a
/// session a write opened. Attempted once: a daemon that cannot attach it
/// leaves the session on the ordinary idle rule, and the write goes ahead.
async fn attach_process_for_writes(config: &McpServerConfig, session: &str) {
    if config.connection_session.names_process() {
        return;
    }
    config.connection_session.mark_names_process();
    #[cfg(test)]
    if config.connection_session.in_test_mode() {
        return;
    }
    if let Err(error) =
        daemon_delegate::attach_session_process(session, config.connection_session.process_id())
            .await
    {
        tracing::debug!("attaching this process to the connection's session failed: {error}");
    }
}

/// Whether `result` says the connection's session was reaped before the write
/// started anything; if so the session is reopened and put back on the call,
/// which may then be sent once more.
async fn reopen_reaped_session(
    call: &mut ToolCallParams,
    config: &McpServerConfig,
    session: Option<&str>,
    result: &ToolCallResult,
) -> bool {
    let Some(session) = session else {
        return false;
    };
    let answer = serde_json::to_value(result).unwrap_or_default();
    if !crate::connection_session::session_gone_before_start(&call.name, session, &answer) {
        return false;
    }
    config.connection_session.forget(session).await;
    call.arguments.remove("session_id");
    matches!(adopt_connection_session(call, config).await, Ok(Some(_)))
}

/// The refusal a call to a tool this connection does not serve gets, when its
/// arguments were refused too: the tool is not served, which is the first thing
/// the caller has to know, and its arguments are not read against a schema the
/// caller was never shown. `None` when the tool is served, so the argument
/// refusal stands.
fn unserved_call(
    config: &McpServerConfig,
    call: &ToolCallParams,
    discovered_call: bool,
    routed_call: bool,
) -> Option<ToolCallResult> {
    let allowed = config.allowed_tools.as_ref()?;
    if discovered_call || routed_call {
        return None;
    }
    let name = crate::agent_belt::canonical_tool_name(&call.name);
    if crate::verbs::allows(allowed, name) {
        return None;
    }
    let registered = crate::verbs::handler_name(&call.name, &call.arguments);
    Some(not_enabled_refusal(config, &call.name, &registered))
}

fn not_enabled_refusal(config: &McpServerConfig, served: &str, registered: &str) -> ToolCallResult {
    match config.routed {
        Some(surface) => crate::routed::refuse_named_call(registered, surface),
        None => ToolCallResult::error(format!(
            "tool '{served}' is not enabled in this MCP profile"
        )),
    }
}

/// Present a routed call's finished answer with its hints naming the routed
/// commands that reach what they name, inside the budget the call was bounded
/// to. Authority and budget-cut disclosures stay exactly as the named answer
/// carries them. Only the existing final byte/token counters follow the new
/// spelling; a spelling that cannot fit keeps the original qualified answer.
fn present_routed_hints(response: &mut JsonRpcResponse, call_params: &ToolCallParams) {
    let Some(value) = response.result.as_ref() else {
        return;
    };
    let Ok(mut result) = serde_json::from_value::<ToolCallResult>(value.clone()) else {
        return;
    };
    if result.content.first().is_some_and(|block| {
        let ContentBlock::Text { text } = block;
        serde_json::from_str::<serde_json::Value>(text)
            .is_ok_and(|payload| crate::trace_pages::is_page(&payload))
    }) {
        // Paging froze the semantic records and their original safety reading.
        // Rewriting whole records here but not their UTF-8 fragments would make
        // reconstructed content depend on the requested page size.
        return;
    }
    let budget = ResponseBudget::from_arguments(&call_params.arguments);
    let original = result.clone();
    crate::routed::rewrite_hints(&mut result, budget.max_chars);
    for (block, original_block) in result.content.iter_mut().zip(&original.content) {
        let ContentBlock::Text { text } = block;
        let ContentBlock::Text {
            text: original_text,
        } = original_block;
        if text == original_text {
            continue;
        }
        // A soft-budget residual already discloses an over-budget answer.
        // Presentation must not silently change that standing fact or trim
        // the evidence which required it. Keep that answer intact.
        let fitted = (original_text.len() <= budget.max_chars)
            .then(|| fit_routed_hint_accounting(text, &call_params.name, &budget))
            .flatten();
        *text = fitted.unwrap_or_else(|| original_text.clone());
    }
    if let Ok(presented) = serde_json::to_value(&result) {
        response.result = Some(presented);
    }
}

/// Settle only already-present accounting after hint presentation. This is not
/// another envelope pass: no authority, verdict, absence, cuts or payload facts
/// are recomputed. The context limit is the payload's effective tier, which may
/// be higher than the caller's requested tier. Failure keeps the original text.
fn fit_routed_hint_accounting(text: &str, tool: &str, budget: &ResponseBudget) -> Option<String> {
    let Ok(mut payload) = serde_json::from_str::<serde_json::Value>(text) else {
        return (text.len() <= budget.max_chars).then(|| text.to_string());
    };
    let bytes_pointer = "/_kin/response/chars_after_budget";
    let carries_bytes = match payload.pointer(bytes_pointer) {
        Some(value) => {
            value.as_u64()?;
            true
        }
        None => false,
    };
    let context = matches!(tool, "get_context_pack" | "trace_computation");
    let token_limit = if context {
        match payload.get("token_budget") {
            Some(value) => Some(value.as_u64()?),
            None => None,
        }
    } else {
        None
    };
    let carries_tokens = context && payload.get("tokens_used").is_some();
    if carries_tokens {
        payload.get("tokens_used")?.as_u64()?;
        token_limit?;
    }
    let pretty = text.starts_with("{\n");
    for _ in 0..16 {
        let rendered = if pretty {
            serde_json::to_string_pretty(&payload).ok()?
        } else {
            serde_json::to_string(&payload).ok()?
        };
        let bytes = rendered.len() as u64;
        let tokens = kin_context::estimate_tokens(&rendered) as u64;
        let bytes_match = !carries_bytes || payload.pointer(bytes_pointer)?.as_u64() == Some(bytes);
        let tokens_match = !carries_tokens || payload.get("tokens_used")?.as_u64() == Some(tokens);
        if bytes_match && tokens_match {
            return (rendered.len() <= budget.max_chars
                && token_limit.is_none_or(|limit| tokens <= limit))
            .then_some(rendered);
        }
        if carries_bytes {
            *payload.pointer_mut(bytes_pointer)? = serde_json::json!(bytes);
        }
        if carries_tokens {
            *payload.get_mut("tokens_used")? = serde_json::json!(tokens);
        }
    }
    None
}

/// Attach the offline/in-process response envelope and wrap the result as a
/// JSON-RPC success. The in-process path is the explicit offline runtime, so the
/// envelope honestly flags `offline_fallback` (not daemon-owned truth). The tool
/// name lets `finalize` synthesize the confidence-qualified negative for empty
/// retrieval results on this path too.
fn offline_envelope_success(
    id: Option<serde_json::Value>,
    result: ToolCallResult,
    tool_name: &str,
    budget: &ResponseBudget,
) -> JsonRpcResponse {
    let enveloped = envelope::finalize_bounded(result, Envelope::offline(), tool_name, budget);
    JsonRpcResponse::success(id, serde_json::to_value(&enveloped).unwrap_or_default())
}

async fn handle_tools_call_daemon(
    id: Option<serde_json::Value>,
    params: &serde_json::Value,
    config: &McpServerConfig,
) -> JsonRpcResponse {
    let mut call_params: ToolCallParams = match serde_json::from_value(params.clone()) {
        Ok(p) => p,
        Err(e) => {
            return JsonRpcResponse::error(id, -32602, format!("Invalid params: {}", e));
        }
    };
    // A routed call becomes the named call it stands for before anything reads
    // its name, so it is forwarded to the daemon as that named call and the
    // daemon never learns a routed tool exists. `describe` and a call that did
    // not validate read no graph and are answered here, through the same
    // envelope the tool registry is.
    let routed_call = match crate::routed::route(&mut call_params, config.routed) {
        crate::routed::Routing::Answer(result) => {
            let budget = ResponseBudget::from_arguments(&call_params.arguments);
            let enveloped = envelope::finalize_bounded(
                result,
                Envelope::daemon(),
                crate::routed::TOOL_NAME,
                &budget,
            );
            return JsonRpcResponse::success(
                id,
                serde_json::to_value(&enveloped).unwrap_or_default(),
            );
        }
        crate::routed::Routing::Dispatch => true,
        crate::routed::Routing::NotRouted => false,
    };
    // The front door runs other verbs through this same path and composes
    // their answers; the daemon is only ever asked for those verbs.
    if call_params.name == crate::ask::TOOL_NAME
        && (routed_call || crate::ask::dispatch::served(config))
    {
        let result = crate::ask::dispatch::answer_daemon(&call_params.arguments, config).await;
        return JsonRpcResponse::success(id, serde_json::to_value(&result).unwrap_or_default());
    }
    let overridden = agent_profile_overrides(&mut call_params, config);
    let mut response = forward_tools_call(id, &mut call_params, routed_call, config).await;
    if routed_call {
        present_routed_hints(&mut response, &call_params);
    }
    append_note(&mut response, overridden);
    response
}

/// Connection-owned presentation metadata, supplied before the daemon freezes
/// a trace/reference answer. It is not an agent argument or graph authority.
fn bind_paged_client_folder(call: &mut ToolCallParams, config: &McpServerConfig) {
    const ARGUMENT: &str = crate::trace_pages::CLIENT_ROOT_ARGUMENT;
    call.arguments.remove(ARGUMENT);
    if matches!(
        call.name.as_str(),
        "find_references" | "trace_data_flow" | crate::impact_pages::TOOL
    ) {
        if let Some(client) = config.client_root.as_deref() {
            call.arguments.insert(
                ARGUMENT.into(),
                serde_json::json!((config.canonicalize)(client).to_string_lossy()),
            );
        }
    }
}

/// The daemon route from a named call to its enveloped answer.
async fn forward_tools_call(
    id: Option<serde_json::Value>,
    call_params: &mut ToolCallParams,
    routed_call: bool,
    config: &McpServerConfig,
) -> JsonRpcResponse {
    // See `dispatch_tools_call`: on a routed connection the routed tool is the
    // dispatcher.
    let discovered_call = if config.routed.is_some() {
        false
    } else {
        match crate::tool_invocation::expand(call_params, config.allowed_tools.as_ref()) {
            Ok(value) => value,
            Err(error) => return JsonRpcResponse::error(id, -32602, error.to_string()),
        }
    };
    // Resolve the served name to the registered one here, once, before anything
    // keyed on a tool name reads it: the profile filter below, the dispatcher,
    // the response-budget shape, the negative-evidence spec and the envelope.
    // See `dispatch_tools_call`: a verb becomes the registered tool that
    // answers it, so the daemon is only ever asked for a registered name.
    let served_name = call_params.name.clone();
    if let Err(refusal) = crate::verbs::resolve(&mut call_params.name, &mut call_params.arguments) {
        let budget = ResponseBudget::from_arguments(&call_params.arguments);
        let refusal = unserved_call(config, call_params, discovered_call, routed_call)
            .unwrap_or_else(|| ToolCallResult::error(refusal));
        let enveloped =
            envelope::finalize_bounded(refusal, Envelope::daemon(), &served_name, &budget);
        return JsonRpcResponse::success(id, serde_json::to_value(&enveloped).unwrap_or_default());
    }
    // Refuse before forwarding too: an older daemon must not restore retired
    // file operations on an unfiltered connection.
    if let Some(message) = crate::tools::retired_file_operation(&call_params.name) {
        return JsonRpcResponse::error(id, -32602, message.into());
    }
    // The authoritative daemon expands source tickets after replay checks and
    // before first admission. Reading a public source reply here cannot recover
    // a private base and would incorrectly reject a successful keyed retry.
    // The belt asks for the compact locate shape on its agents' behalf, only
    // when the caller named no surface. Applied here rather than in the daemon
    // because this is the one layer that knows which profile is being served.
    if config.agent_belt {
        crate::agent_belt::apply_belt_defaults(&call_params.name, &mut call_params.arguments);
    }
    // The caller's own budget, read before the machine answer is asked for
    // whole. A machine locate is held to it by paging.
    let caller_budget = ResponseBudget::from_arguments(&call_params.arguments);
    let locate_page = pages_machine_locate(&call_params.name, config);
    // The question a frozen answer is keyed by, read where a continuation of
    // it is read, before anything below adds to the call.
    let locate_arguments = locate_page.then(|| call_params.arguments.clone());
    if locate_page {
        if let Some(cursor) = crate::locate_pages::page_cursor(&call_params.arguments) {
            // The pages were frozen from the graph this daemon was serving.
            // A continuation resumes only while it still serves that graph.
            let health = daemon_delegate::fetch_health_snapshot().await;
            let result = match locate_page_authority(daemon_graph_as_of(health.as_ref())) {
                Some(authority) => crate::locate_pages::resume(
                    cursor,
                    &crate::locate_pages::context(&call_params.arguments, &authority),
                    caller_budget.max_chars,
                )
                .map(|page| ToolCallResult::text(page.to_string()))
                .unwrap_or_else(ToolCallResult::error),
                None => ToolCallResult::error(
                    "locate cursor cannot be resumed while the daemon's graph cannot be read; \
                     restart without cursor",
                ),
            };
            let enveloped = envelope::finalize_bounded(
                result,
                Envelope::daemon(),
                &call_params.name,
                &caller_budget,
            );
            return JsonRpcResponse::success(
                id,
                serde_json::to_value(&enveloped).unwrap_or_default(),
            );
        }
    }
    // A plain locate reply is cut to its budget as the reply that ships, so
    // its machine answer is asked for whole.
    widen_machine_budget(call_params, config);
    let budget = ResponseBudget::from_arguments(&call_params.arguments);

    if let Some(allowed) = &config.allowed_tools {
        if !discovered_call && !routed_call && !crate::verbs::allows(allowed, &call_params.name) {
            let error_result = not_enabled_refusal(config, &served_name, &call_params.name);
            let enveloped = envelope::finalize_bounded(
                error_result,
                Envelope::daemon(),
                &call_params.name,
                &budget,
            );
            return JsonRpcResponse::success(
                id,
                serde_json::to_value(&enveloped).unwrap_or_default(),
            );
        }
    }

    // See `dispatch_tools_call`: the rules the plain served schemas no longer
    // carry are refused here, before the daemon is asked anything. Not in the
    // handlers, which the daemon also calls with arguments it has rewritten.
    if let Some(refusal) = crate::input_contract::refusal(&call_params.name, &call_params.arguments)
    {
        let enveloped =
            envelope::finalize_bounded(refusal, Envelope::daemon(), &call_params.name, &budget);
        return JsonRpcResponse::success(id, serde_json::to_value(&enveloped).unwrap_or_default());
    }

    // A short cursor or ticket this connection did not read back belongs to
    // one that is gone. Said here, before any route is asked, because a
    // write's ticket never reaches the daemon's tool route. The daemon reads
    // an entity's label or short id from the graph itself; see
    // `entity_address`.
    if let Some(refusal) = crate::entity_address::session_bound_refusal(&call_params.arguments) {
        let enveloped = envelope::finalize_bounded(
            ToolCallResult::error(refusal),
            Envelope::daemon(),
            &call_params.name,
            &budget,
        );
        return JsonRpcResponse::success(id, serde_json::to_value(&enveloped).unwrap_or_default());
    }

    if matches!(
        call_params.name.as_str(),
        "kin_session_start" | "register_session"
    ) {
        let Some(context) = config.client_root.as_deref() else {
            let refusal = ToolCallResult::error(
                "This connection has no bound repository context for a session; nothing ran.",
            );
            let result =
                envelope::finalize_bounded(refusal, Envelope::daemon(), &call_params.name, &budget);
            return JsonRpcResponse::success(id, serde_json::to_value(result).unwrap_or_default());
        };
        // Dedicated session transport consumes this private connection fact.
        // Public selectors were validated before it was attached.
        call_params.arguments.insert(
            "cwd".into(),
            serde_json::json!((config.canonicalize)(context)),
        );
    }

    // The tool registry is answered here rather than forwarded. It is the
    // registry compiled into THIS binary, which is what `tools/list` on this
    // server is built from, so a daemon on a different build would hand an agent
    // a schema this server cannot dispatch. The answer reads no graph, so there
    // is nothing the daemon could add. The envelope is the daemon one because on
    // this route the SERVER is daemon-backed, which is what `runtime` reports;
    // `Envelope::daemon_unreachable` keeps the same value for the same reason.
    if call_params.name == crate::handlers::tool_search::TOOL_NAME {
        let result = crate::handlers::tool_search::handle_tool_search_with_profile(
            &call_params.arguments,
            config.allowed_tools.as_ref(),
        )
        .unwrap_or_else(|error| ToolCallResult::error(error.to_string()));
        let enveloped =
            envelope::finalize_bounded(result, Envelope::daemon(), &call_params.name, &budget);
        return JsonRpcResponse::success(id, serde_json::to_value(&enveloped).unwrap_or_default());
    }

    // A write that names no session runs in this connection's own, opened on
    // the first write that needs it. Authorization stays the daemon's: the
    // session declares no capabilities, so the store's permissions decide.
    let connection_session = match adopt_connection_session(call_params, config).await {
        Ok(session) => session,
        Err(refusal) => {
            let enveloped =
                envelope::finalize_bounded(refusal, Envelope::daemon(), &call_params.name, &budget);
            return JsonRpcResponse::success(
                id,
                serde_json::to_value(&enveloped).unwrap_or_default(),
            );
        }
    };

    // The one-shot adapter preserves the existing unkeyed begin/commit path.
    // Keyed requests travel intact under a versioned internal name so an old
    // daemon refuses the durable contract before any transaction is started.
    if call_params.name == "kin_mutate" {
        let mut result = crate::handlers::sessions::mutate_through_daemon(&call_params.arguments)
            .await
            .unwrap_or_else(|error| ToolCallResult::error(error.to_string()));
        if reopen_reaped_session(call_params, config, connection_session.as_deref(), &result).await
        {
            result = crate::handlers::sessions::mutate_through_daemon(&call_params.arguments)
                .await
                .unwrap_or_else(|error| ToolCallResult::error(error.to_string()));
        }
        let enveloped =
            envelope::finalize_bounded(result, Envelope::daemon(), &call_params.name, &budget);
        return JsonRpcResponse::success(id, serde_json::to_value(&enveloped).unwrap_or_default());
    }

    // Routing and discovery have resolved the real tool and arguments. Never
    // let a supplied internal value impersonate this connection's folder.
    bind_paged_client_folder(call_params, config);

    // Graph status carries its own selected-graph coverage observation. Mark
    // the beginning before forwarding so a refusal published during the call
    // cannot be discharged by counters that may have preceded it.
    let graph_status_observation_started_at_unix =
        (call_params.name == "kin_graph_status").then(daemon_delegate::current_unix_seconds);
    let mut forwarded =
        daemon_delegate::forward_tool_call(&call_params.name, &call_params.arguments).await;
    let reopened = match &forwarded {
        Ok(Some(result)) => {
            let result = result.clone();
            reopen_reaped_session(call_params, config, connection_session.as_deref(), &result).await
        }
        _ => false,
    };
    if reopened {
        forwarded =
            daemon_delegate::forward_tool_call(&call_params.name, &call_params.arguments).await;
    }
    let (mut result, mut base_env) = match forwarded {
        Ok(Some(result)) => (result, Envelope::daemon()),
        // Which gap it was decides the envelope: a working directory that is no
        // repository has no daemon to be unreachable.
        Ok(None) => daemon_delegate::daemon_unavailable_tool_result(&call_params.name).await,
        Err(error) => {
            let base = envelope_for_delegate_error(
                &error,
                daemon_delegate::recorded_daemon_kill().as_ref(),
            );
            (ToolCallResult::error(error), base)
        }
    };
    // Current daemons validate the private selected-graph observation before
    // publishing its semantic presentation. Preserve that captured verdict;
    // a later health reading belongs to a different observation.
    if call_params.name == "kin_graph_status" {
        match daemon_delegate::validate_semantic_graph_status_result(&result) {
            Ok(true) => {
                let result = crate::status_pages::enforce_ceiling(result, budget.max_chars);
                return JsonRpcResponse::success(
                    id,
                    serde_json::to_value(&result).unwrap_or_default(),
                );
            }
            Err(error) => {
                return JsonRpcResponse::success(
                    id,
                    serde_json::to_value(ToolCallResult::error(error)).unwrap_or_default(),
                );
            }
            Ok(false) => {}
        }
    }
    present_result(
        config,
        &call_params.name,
        &call_params.arguments,
        &mut result,
    );

    // Stamped on every answer, not only on the ones that failed. A suspended
    // sweep is a standing fact about what this store's graph can contain, so
    // the call it changes the reading of is the one that SUCCEEDS and returns
    // nothing: an agent certifying that absence needs to know the producer that
    // would have filled it is switched off.
    base_env = base_env.with_suspended_sweep(daemon_delegate::suspended_sweep().as_ref());
    // Stamped for the same reason and on the same calls. Work Kin declined
    // because the machine had no room is work no producer is doing, and the
    // answer it changes the reading of is the one that succeeds and returns
    // nothing. Generic tools reconcile an embedding refusal against the exact
    // selected-graph coverage returned by `/commands/resources`. Graph status
    // is handled below from the typed report it returns, so it never mixes two
    // independently sampled graph observations.
    if call_params.name != "kin_graph_status" {
        let pressure_refusal =
            daemon_delegate::outstanding_memory_pressure_refusal(&call_params.arguments).await;
        base_env = base_env.with_memory_pressure(pressure_refusal.as_ref());
    }
    // Stamped on every answer for the same reason, and it is the one of the
    // three that qualifies an answer which came back looking complete. A graph
    // short of its own last verified-good census returns rows that are all true
    // and a set that is not whole, and nothing in the payload can tell the two
    // apart.
    base_env = base_env.with_relation_census_loss(daemon_delegate::relation_census_hold().as_ref());
    // Stamped on every answer because a replay-version gap qualifies a graph
    // that otherwise looks complete. Historical deltas are not re-derived in
    // place, so the answer affected most is a successful absence whose missing
    // row may reflect replay semantics the store cannot show match this build.
    base_env = base_env.with_hydration_semantics_observation(
        daemon_delegate::hydration_semantics_standing().as_ref(),
    );
    // A store holding only its checked-out commit answers current questions in
    // full and history questions in part; every answer carries which.
    base_env = base_env.with_partial_history(daemon_delegate::history_standing().as_ref());
    // Stamped on every answer for the same reason as the three above, and it
    // qualifies the same kind of answer the census-loss flag does: one that came
    // back looking complete. A sweep that offered relations the graph does not
    // hold leaves an absence an agent may certify, and nothing in the payload
    // distinguishes it from an absence that is simply true.
    base_env = base_env.with_enrichment_shortfall(daemon_delegate::enrichment_shortfall().as_ref());

    // `kin_graph_status` already reports the exact graph view selected by the
    // daemon, including temporal-session scope. Generic `/health` is HEAD-only:
    // borrowing its entity count or generation here would mix two authorities
    // in one response. Build the standard `_kin` envelope from the report
    // itself and validate the fully annotated stdio payload instead.
    // Enrich the envelope with honest degraded/freshness signals from the daemon
    // `/health` body when the daemon is actually reachable. When it was already
    // determined unreachable, skip the probe — there is nothing to ask.
    //
    // Fetched before the graph-status branch below, not after it. That branch
    // used to return first and so never reached this probe at all, which took
    // the working-copy reading away from it along with the counts it is right to
    // refuse, and left it publishing "0 uncommitted" over a repository holding a
    // module the graph had never met (FIR-2820).
    let health = if base_env.degraded.daemon_unreachable == Some(true)
        || base_env.degraded.no_repository == Some(true)
    {
        None
    } else {
        daemon_delegate::fetch_health_snapshot().await
    };

    if call_params.name == "kin_graph_status" {
        // Two narrow lifts off the one health probe above, and nothing else
        // from it. Vector persistence is a property of this daemon's storage
        // backend rather than a HEAD graph observation, and the working-copy
        // reading is a fact about the disk rather than about the selected
        // graph; selected-graph finalization below still replaces every graph
        // field from the report itself.
        if let Some(health) = health.as_ref() {
            base_env = base_env.with_repository(
                health,
                config.client_root.as_deref(),
                config.canonicalize,
            );
            base_env = base_env.with_working_copy_health(health);
            // The third lift: which daemon answered, and how long it had been up.
            // A status read from a daemon that began serving a moment ago is the
            // answer most in need of saying so.
            base_env = base_env.with_answering_daemon(health);
            if let Some(unavailable) = health
                .get("embed_persistence_unavailable")
                .and_then(serde_json::Value::as_bool)
            {
                base_env = base_env.with_embed_persistence_unavailable(unavailable);
            }
        }
        let pressure_refusals = daemon_delegate::recorded_memory_pressure_refusals();
        let enveloped = finalize_daemon_graph_status(
            result,
            base_env,
            &pressure_refusals,
            graph_status_observation_started_at_unix
                .expect("graph status records its observation start before forwarding"),
        );
        let enveloped = crate::status_pages::enforce_ceiling(enveloped, budget.max_chars);
        return JsonRpcResponse::success(id, serde_json::to_value(&enveloped).unwrap_or_default());
    }

    if let Some(health) = health.as_ref() {
        base_env = base_env.with_health(health);
        base_env =
            base_env.with_repository(health, config.client_root.as_deref(), config.canonicalize);
    }

    let locate_graph = locate_page
        .then(|| daemon_graph_as_of(health.as_ref()))
        .flatten();
    let enveloped = envelope::finalize_bounded(result, base_env, &call_params.name, &budget);
    if locate_page {
        let context = locate_page_authority(locate_graph)
            .zip(locate_arguments.as_ref())
            .map(|(authority, arguments)| crate::locate_pages::context(arguments, &authority));
        let held = crate::locate_pages::finalize(enveloped, context, &caller_budget);
        return JsonRpcResponse::success(id, serde_json::to_value(&held).unwrap_or_default());
    }
    JsonRpcResponse::success(id, serde_json::to_value(&enveloped).unwrap_or_default())
}

/// The envelope for a delegate error, from the error itself.
///
/// A delegate error that ends with the daemon gone is exactly the condition
/// `daemon_unreachable` exists to state, and it was the one shape that never
/// set it: the flag was reserved for the case where no daemon endpoint was
/// resolved at all, so a session whose daemon was killed under it received an
/// empty `degraded` object, which a client cannot tell from a healthy answer
/// without parsing prose. Where the store has recorded why its daemons keep
/// dying, that is stamped beside it.
///
/// Every other delegate error is left alone. A live daemon rejecting a bad
/// argument has not become unreachable, and a store's kill history is not a
/// fact about that call.
fn envelope_for_delegate_error(
    error: &str,
    record: Option<&kin_daemon_spawn::DaemonKillRecord>,
) -> Envelope {
    if daemon_delegate::is_daemon_loss_error(error) {
        Envelope::daemon_unreachable().with_recorded_daemon_kill(record)
    } else {
        Envelope::daemon()
    }
}

/// Validate and qualify a private selected-graph observation at its producer.
/// The public semantic report is validated separately and never reconstructed
/// as a private report or graded against a later graph observation.
pub fn finalize_daemon_graph_status(
    result: ToolCallResult,
    base_env: Envelope,
    pressure_refusals: &[kin_core::memory_pressure::PressureRefusal],
    observation_started_at_unix: u64,
) -> ToolCallResult {
    match daemon_delegate::validate_semantic_graph_status_result(&result) {
        Ok(true) => return result,
        Err(error) => return ToolCallResult::error(error),
        Ok(false) => {}
    }
    let unobserved_pressure_refusal = pressure_refusals.last();
    let mut report = match daemon_delegate::parse_graph_status_report(&result) {
        Ok(Some(report)) => report,
        Ok(None) => {
            return envelope::finalize(
                result,
                base_env.with_memory_pressure(unobserved_pressure_refusal),
                "kin_graph_status",
            );
        }
        Err(error) => {
            return envelope::finalize(
                ToolCallResult::error(error),
                base_env.with_memory_pressure(unobserved_pressure_refusal),
                "kin_graph_status",
            );
        }
    };

    let embedding_coverage = kin_core::memory_pressure::EmbeddingCoverage {
        pending: report.embeddings_pending,
        indexed: report.embeddings_indexed,
        total: report.embeddings_total,
    };
    let pressure_refusal = daemon_delegate::pressure_refusal_for_selected_graph(
        pressure_refusals,
        embedding_coverage,
        observation_started_at_unix,
    );

    let counts = (
        u64::try_from(report.entity_count),
        u64::try_from(report.embeddings_indexed),
        u64::try_from(report.embeddings_pending),
        u64::try_from(report.embeddings_total),
        u64::try_from(report.relation_count),
    );
    let (Ok(entity_count), Ok(indexed), Ok(pending), Ok(total), Ok(relation_count)) = counts else {
        return envelope::finalize(
            ToolCallResult::error(
                "daemon kin_graph_status counters do not fit the stdio response envelope",
            ),
            base_env.with_memory_pressure(pressure_refusal.as_ref()),
            "kin_graph_status",
        );
    };

    // Rebuild from the strict typed observation before public presentation,
    // stripping an existing private envelope so annotation cannot preserve
    // additive or unscoped metadata from a different observation.
    report.response_envelope = None;
    let result = match serde_json::to_string_pretty(&report) {
        Ok(text) => ToolCallResult::text(text),
        Err(error) => {
            return envelope::finalize(
                ToolCallResult::error(format!(
                    "stdio kin_graph_status could not serialize the validated daemon report: \
                     {error}"
                )),
                base_env.with_memory_pressure(pressure_refusal.as_ref()),
                "kin_graph_status",
            );
        }
    };
    let mut selected_env = base_env
        .with_selected_graph_observation(
            envelope::DurabilityCounts {
                live_entities: entity_count,
                durable_entities: report.durable_entity_count,
                live_relations: Some(relation_count),
                durable_relations: report.durable_relation_count,
            },
            indexed,
            pending,
            total,
        )
        .with_memory_pressure(pressure_refusal.as_ref());
    if let Some(stale) = report.stale.as_ref() {
        selected_env = selected_env.with_selected_graph_staleness(
            stale.reason.as_str(),
            stale.settled_age_ms,
            stale.observed_authority_epoch,
            stale.live_attempts,
        );
    }
    let enveloped = envelope::finalize(result, selected_env, "kin_graph_status");
    if let Err(error) = daemon_delegate::parse_graph_status_report(&enveloped) {
        return envelope::finalize(
            ToolCallResult::error(format!(
                "stdio kin_graph_status contract validation failed after envelope annotation: \
                 {error}"
            )),
            Envelope::daemon(),
            "kin_graph_status",
        );
    }
    enveloped
}

fn tool_requires_persist(name: &str) -> bool {
    crate::handlers::review::is_review_mutation(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::{ENVELOPE_KEY, ENVELOPE_VERSION};
    use kin_db::InMemoryGraph;

    /// Run a `tools/call` through the real in-process chokepoint and return the
    /// parsed tool payload (the JSON inside the single text content block).
    async fn call_tool_payload(tool: &str, arguments: serde_json::Value) -> serde_json::Value {
        let mut config = McpServerConfig::default();
        config.session_authority_mode = SessionAuthorityMode::OfflineFallback;
        let sessions = SessionRegistry::new();
        let store = InMemoryGraph::default();
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": tool, "arguments": arguments },
        })
        .to_string();
        let resp = process_message(&msg, &store, &config, &sessions)
            .await
            .expect("response");
        assert!(resp.error.is_none(), "transport error for {tool}");
        let result: ToolCallResult =
            serde_json::from_value(resp.result.expect("result")).expect("tool call result");
        let ContentBlock::Text { text } = result.content.first().expect("one content block");
        serde_json::from_str(text)
            .unwrap_or_else(|e| panic!("envelope-annotated payload for {tool} is not JSON: {e}"))
    }

    #[tokio::test]
    async fn offline_reference_and_trace_reject_non_string_cursors_before_reading() {
        for tool in ["find_references", "trace_data_flow"] {
            for cursor in [serde_json::json!(7), serde_json::json!({"token":"x"})] {
                let payload =
                    call_tool_payload(tool, serde_json::json!({"query":"absent", "cursor":cursor}))
                        .await;
                assert!(
                    payload.to_string().contains("cursor must be a string"),
                    "{tool}: {payload}"
                );
                assert!(
                    !payload.to_string().contains("entity not found"),
                    "invalid transport must not become a graph answer"
                );
            }
        }
    }

    /// Assert the offline envelope is present and well-formed on a payload.
    fn assert_offline_envelope(payload: &serde_json::Value, tool: &str) {
        let env = payload
            .get(ENVELOPE_KEY)
            .unwrap_or_else(|| panic!("tool {tool} response is missing the _kin envelope"));
        assert_eq!(
            env["envelope_version"], ENVELOPE_VERSION,
            "tool {tool} envelope version"
        );
        assert_eq!(
            env["runtime"], "offline-in-process",
            "tool {tool} runtime should report the in-process fallback"
        );
        // Honesty: the offline path flags itself as a non-daemon fallback.
        assert_eq!(env["degraded"]["offline_fallback"], true, "tool {tool}");
    }

    async fn offline_review_call(
        store: &InMemoryGraph,
        config: &McpServerConfig,
        tool: &str,
        arguments: serde_json::Value,
    ) -> ToolCallResult {
        let message = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": tool, "arguments": arguments }
        })
        .to_string();
        let response = process_message(&message, store, config, &SessionRegistry::new())
            .await
            .unwrap();
        assert!(response.error.is_none(), "{response:?}");
        serde_json::from_value(response.result.unwrap()).unwrap()
    }

    fn review_payload(result: &ToolCallResult) -> serde_json::Value {
        let ContentBlock::Text { text } = &result.content[0];
        serde_json::from_str(text).unwrap()
    }

    fn assigned_reviewers(store: &InMemoryGraph, review: &kin_model::ReviewId) -> Vec<String> {
        let mut names: Vec<_> = kin_review::assignments::current_assignments(store, review)
            .unwrap()
            .into_iter()
            .map(|assignment| assignment.reviewer.name)
            .collect();
        names.sort();
        names
    }

    /// A named call that breaks an "at least one of" rule the plain served
    /// schema no longer carries is refused by the server on both call routes,
    /// before anything runs: offline, and on the daemon route with no daemon
    /// to reach at all.
    #[tokio::test]
    async fn both_call_routes_refuse_a_call_missing_every_alternative() {
        let message = |tool: &str, arguments: serde_json::Value| {
            serde_json::json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": { "name": tool, "arguments": arguments }
            })
            .to_string()
        };
        let refused_text = |response: JsonRpcResponse| -> serde_json::Value {
            assert!(response.error.is_none(), "{response:?}");
            let result: ToolCallResult = serde_json::from_value(response.result.unwrap()).unwrap();
            assert_eq!(result.is_error, Some(true), "{result:?}");
            let ContentBlock::Text { text } = &result.content[0];
            serde_json::from_str(text).unwrap()
        };
        let offline = McpServerConfig {
            session_authority_mode: SessionAuthorityMode::OfflineFallback,
            ..Default::default()
        };
        let daemon = McpServerConfig {
            allowed_tools: Some(crate::tools::name_set(
                crate::tools::agent_default_tool_names(),
            )),
            agent_belt: true,
            ..Default::default()
        };
        let store = InMemoryGraph::new();
        for (tool, arguments, needs) in [
            (
                "semantic_locate",
                serde_json::json!({"limit": 3}),
                "query or cursor",
            ),
            (
                "get_context_pack",
                serde_json::json!({"depth": 1}),
                "entity_id or entities or question",
            ),
            (
                crate::handlers::lexical::TOOL_NAME,
                serde_json::json!({"kind": "function"}),
                "literal or cursor",
            ),
        ] {
            let answers = [
                refused_text(
                    process_message(
                        &message(tool, arguments.clone()),
                        &store,
                        &offline,
                        &SessionRegistry::new(),
                    )
                    .await
                    .unwrap(),
                ),
                refused_text(
                    process_daemon_message(&message(tool, arguments.clone()), &daemon)
                        .await
                        .unwrap(),
                ),
            ];
            for answer in answers {
                assert!(
                    answer["message"].as_str().unwrap().contains(needs),
                    "{tool}: {answer}"
                );
                assert_eq!(answer["example"]["name"], tool);
            }
        }
        // The review rules hold on the offline route, which serves them.
        for (tool, arguments) in [
            ("kin_review_create", serde_json::json!({"title": "t"})),
            (
                "kin_review_create",
                serde_json::json!({"title": "t", "base": "working-tree"}),
            ),
            (
                "kin_review_assign",
                serde_json::json!({"review_id": "r", "requested_reviewers": ["a"]}),
            ),
        ] {
            let answer = refused_text(
                process_message(
                    &message(tool, arguments.clone()),
                    &store,
                    &offline,
                    &SessionRegistry::new(),
                )
                .await
                .unwrap(),
            );
            assert_eq!(
                answer["error"], "missing_arguments",
                "{tool} {arguments}: {answer}"
            );
        }
    }

    #[tokio::test]
    async fn offline_review_unassign_survives_snapshot_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot = dir.path().join("graph.kindb");
        let config = McpServerConfig {
            snapshot_path: Some(snapshot.clone()),
            session_authority_mode: SessionAuthorityMode::OfflineFallback,
            allowed_tools: None,
            ..Default::default()
        };
        let store = InMemoryGraph::new();
        let created = offline_review_call(
            &store,
            &config,
            "kin_review_create",
            // base and head as the handler would default them, which the
            // create call's "at least one of" rule asks the caller to name.
            serde_json::json!({"title":"Persist assignments", "base":"working-tree",
                "head":"working-tree", "reviewers":["Alice", "Bob"]}),
        )
        .await;
        assert_ne!(created.is_error, Some(true), "{created:?}");
        let review = kin_model::ReviewId(
            uuid::Uuid::parse_str(review_payload(&created)["review_id"].as_str().unwrap()).unwrap(),
        );
        let original = kin_db::SnapshotManager::open_without_text_index(&snapshot).unwrap();
        assert_eq!(
            assigned_reviewers(original.graph().as_ref(), &review),
            vec!["Alice", "Bob"]
        );
        drop(original);
        for _ in 0..2 {
            let result = offline_review_call(
                &store,
                &config,
                "kin_review_unassign",
                serde_json::json!({"review_id":review.to_string(), "reviewer":"Alice"}),
            )
            .await;
            assert_ne!(result.is_error, Some(true), "{result:?}");
            assert_eq!(review_payload(&result)["unassigned"], true);
            assert_eq!(assigned_reviewers(&store, &review), vec!["Bob"]);
            let reopened = kin_db::SnapshotManager::open_without_text_index(&snapshot).unwrap();
            assert_eq!(
                assigned_reviewers(reopened.graph().as_ref(), &review),
                vec!["Bob"],
                "a successful unassign must survive reopening the configured snapshot"
            );
        }
    }

    #[tokio::test]
    async fn offline_review_unassign_reports_snapshot_failure() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot = dir.path().join("graph.kindb");
        let mut config = McpServerConfig {
            snapshot_path: Some(snapshot.clone()),
            session_authority_mode: SessionAuthorityMode::OfflineFallback,
            allowed_tools: None,
            ..Default::default()
        };
        let store = InMemoryGraph::new();
        let created = offline_review_call(
            &store,
            &config,
            "kin_review_create",
            serde_json::json!({"title":"Refuse false durable success", "base":"working-tree",
                "head":"working-tree", "reviewers":["Alice"]}),
        )
        .await;
        assert_ne!(created.is_error, Some(true), "{created:?}");
        let review = kin_model::ReviewId(
            uuid::Uuid::parse_str(review_payload(&created)["review_id"].as_str().unwrap()).unwrap(),
        );
        let blocked_parent = dir.path().join("not-a-directory");
        std::fs::write(&blocked_parent, b"owned fixture").unwrap();
        config.snapshot_path = Some(blocked_parent.join("graph.kindb"));
        let result = offline_review_call(
            &store,
            &config,
            "kin_review_unassign",
            serde_json::json!({"review_id":review.to_string(), "reviewer":"Alice"}),
        )
        .await;
        assert_eq!(result.is_error, Some(true), "{result:?}");
        assert!(review_payload(&result)["message"]
            .as_str()
            .unwrap()
            .contains("snapshot persistence failed"));
        // The existing offline contract applies in memory before persistence.
        // Failure must disclose this, not claim rollback or durable success.
        assert!(assigned_reviewers(&store, &review).is_empty());
        let reopened = kin_db::SnapshotManager::open_without_text_index(&snapshot).unwrap();
        assert_eq!(
            assigned_reviewers(reopened.graph().as_ref(), &review),
            vec!["Alice"]
        );
    }

    // ── D.8: every tool family carries the unified response envelope ──────────

    #[tokio::test]
    async fn envelope_present_on_entities_family() {
        // semantic_search returns an object payload; the envelope must ride
        // alongside the original `results` key without displacing it.
        let payload =
            call_tool_payload("semantic_search", serde_json::json!({ "query": "foo" })).await;
        assert_offline_envelope(&payload, "semantic_search");
        assert!(
            payload.get("results").is_some(),
            "semantic_search payload must keep its `results` key where agents expect it"
        );
    }

    #[tokio::test]
    async fn envelope_present_on_work_family() {
        let payload = call_tool_payload("kin_work_list", serde_json::json!({})).await;
        assert_offline_envelope(&payload, "kin_work_list");
    }

    #[tokio::test]
    async fn envelope_present_on_verification_family() {
        let payload = call_tool_payload("kin_coverage_summary", serde_json::json!({})).await;
        assert_offline_envelope(&payload, "kin_coverage_summary");
    }

    #[tokio::test]
    async fn envelope_present_on_error_results() {
        // semantic_locate errors offline (vector search needs the daemon). The
        // envelope must still be attached so degraded states are surfaced, and
        // the human message preserved alongside it.
        let payload = call_tool_payload(
            "semantic_locate",
            serde_json::json!({ "query": "where is auth handled" }),
        )
        .await;
        assert_offline_envelope(&payload, "semantic_locate");
        let message = payload["message"]
            .as_str()
            .expect("wrapped error message present");
        assert!(
            message.contains("requires the Kin daemon"),
            "original error message preserved, got: {message}"
        );
    }

    fn direct_graph_status_result_with_coverage(
        pending: usize,
        indexed: usize,
        total: usize,
    ) -> ToolCallResult {
        ToolCallResult::text(
            serde_json::json!({
                "schema": "kin.graph-status.v1",
                "view": "daemon_selected_graph",
                "scope": "temporal_session",
                "authority": "repo-daemon",
                "sampling": "point_in_time_selected_graph",
                "authority_epoch": 42,
                "entity_count": 2,
                "relation_count": 1,
                "embedding_source": "selected_graph",
                "embeddings_indexed": indexed,
                "embeddings_pending": pending,
                "embeddings_total": total,
                "completion_attested": false
            })
            .to_string(),
        )
    }

    fn direct_graph_status_result() -> ToolCallResult {
        direct_graph_status_result_with_coverage(1, 1, 2)
    }

    #[test]
    fn enrichment_status_final_envelope_budget_preserves_rows_or_refuses() {
        let mut raw: serde_json::Value =
            serde_json::from_str(match &direct_graph_status_result().content[0] {
                ContentBlock::Text { text } => text,
            })
            .unwrap();
        let enrichment = serde_json::json!({
            "schema":"kin.enrichment-status.v1","scope":{"kind":"committed_graph","change":"selected-revision"},
            "current":true,"truth_epoch":4,"snapshot_id":"a".repeat(64),
            "proof_scope":"recorded_call_site_census","all_relationships_attested":false,
            "files":[{"projection_path":kin_model::RepoPath::from_utf8("src/held.py").unwrap(),"proof":"unverified","current_completion":"unavailable_in_selected_scope","reason":"λ".repeat(1000)}],
        });
        raw["enrichment"] = enrichment.clone();
        let wrapped = finalize_daemon_graph_status(
            ToolCallResult::text(raw.to_string()),
            Envelope::daemon(),
            &[],
            0,
        );
        assert_ne!(wrapped.is_error, Some(true));
        let generous = crate::status_pages::enforce_ceiling(wrapped.clone(), 60000);
        let ContentBlock::Text { text } = &generous.content[0];
        let value: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(value["enrichment"], enrichment);
        assert!(value.get("_kin").is_some());
        assert!(text.len() <= 60000);
        let floor = crate::status_pages::enforce_ceiling(wrapped, 2048);
        assert_eq!(floor.is_error, Some(true));
        let ContentBlock::Text { text } = &floor.content[0];
        assert!(text.len() <= 2048);
        assert!(text.contains("budget"));
    }

    /// The same report with its graph emptied, the shape a daemon answers with in
    /// the moment after it begins serving and before its graph is loaded.
    fn direct_graph_status_result_with_entities(entity_count: u64) -> ToolCallResult {
        let result = direct_graph_status_result();
        let ContentBlock::Text { text } = result.content.first().expect("one content block");
        let mut report: serde_json::Value = serde_json::from_str(text).expect("report json");
        report["entity_count"] = serde_json::json!(entity_count);
        report["relation_count"] = serde_json::json!(0);
        ToolCallResult::text(report.to_string())
    }

    /// A status answer from a daemon whose graph holds nothing goes out as a
    /// graph gap with a verdict, and names the daemon that gave it, never as a
    /// bare zero.
    #[test]
    fn an_empty_selected_graph_goes_out_as_a_graph_gap_naming_its_daemon() {
        let health = serde_json::json!({
            "pid": 98031,
            "repo_root": "/work/umbrella",
            "repo_id": "local-4705badf13d922e2",
            "uptime_seconds": 1,
            "version": "0.7.9",
        });
        let base = Envelope::daemon()
            .with_working_copy_health(&health)
            .with_answering_daemon(&health);
        let enveloped =
            finalize_daemon_graph_status(direct_graph_status_result_with_entities(0), base, &[], 2);
        assert_ne!(enveloped.is_error, Some(true), "{enveloped:?}");
        let report = daemon_delegate::parse_graph_status_report(&enveloped)
            .unwrap()
            .expect("successful internal status report");
        let envelope = report.response_envelope.expect("internal status envelope");
        assert_eq!(envelope.graph_state.entity_count, Some(0));
        let verdict = envelope.verdict.expect("empty graph verdict");
        assert_eq!(
            verdict["inputs"]["graph_empty"], "inconclusive",
            "{verdict}"
        );
        assert!(
            verdict["limiting_factor"]
                .as_str()
                .is_some_and(|factor| factor.split("; ").any(|code| code == "graph_empty")),
            "the verdict must name the gap: {verdict}"
        );
        let daemon = envelope.answered_by.expect("internal answering authority");
        assert_eq!(daemon.pid, 98031);
        assert_eq!(daemon.repo_id, "local-4705badf13d922e2");
        assert_eq!(daemon.repo_root, "/work/umbrella");
        assert_eq!(daemon.uptime_seconds, 1);

        let response = present_for_client(
            JsonRpcResponse::success(
                Some(serde_json::json!(1)),
                serde_json::to_value(enveloped).unwrap(),
            ),
            r#"{"method":"tools/call","params":{"name":"kin_graph_status","arguments":{}}}"#,
            &McpServerConfig {
                plain_replies: false,
                ..Default::default()
            },
        );
        let result = response.result.unwrap();
        let text = result["content"][0]["text"].as_str().unwrap();
        let public: serde_json::Value = serde_json::from_str(text).unwrap();
        assert!(public["_kin"]["answered_by"].get("repo_root").is_none());
        assert!(!text.contains("/work/umbrella"));
        assert_eq!(public["_kin"]["semantic_surface"], 1);

        // The control: a populated graph carries no graph gap.
        let populated = finalize_daemon_graph_status(
            direct_graph_status_result(),
            Envelope::daemon().with_answering_daemon(&health),
            &[],
            2,
        );
        assert_ne!(populated.is_error, Some(true), "{populated:?}");
        let populated = daemon_delegate::parse_graph_status_report(&populated)
            .unwrap()
            .expect("successful internal status report")
            .response_envelope
            .expect("internal status envelope");
        assert!(
            !serde_json::to_string(&populated)
                .unwrap()
                .contains("graph_empty"),
            "a populated graph is not a gap: {populated:?}"
        );
    }

    #[test]
    fn graph_status_stdio_envelope_is_derived_from_the_selected_graph() {
        // Even if a caller accidentally supplies a HEAD-derived base envelope,
        // finalization must replace every graph-specific field with the
        // temporal report's own observation.
        let head_health = serde_json::json!({
            "graph_entity_count": 999,
            "graph_generation": 77,
            "initialized": true,
            "graph_loaded": true,
            "reconciliation_status": "head-only",
            "embed_worker_failed": true,
            "embed_persistence_unavailable": true
        });
        let base = Envelope::daemon().with_health(&head_health);
        let enveloped = finalize_daemon_graph_status(direct_graph_status_result(), base, &[], 2);
        let report = daemon_delegate::parse_graph_status_report(&enveloped)
            .unwrap()
            .expect("successful stdio status report");
        let response_env = report
            .response_envelope
            .expect("stdio status carries the standard envelope");

        assert_eq!(
            report.scope,
            crate::handlers::entities::GraphStatusScope::TemporalSession
        );
        assert_eq!(report.entity_count, 2);
        assert_eq!(response_env.graph_state.entity_count, Some(2));
        assert!(response_env.graph_as_of.is_none());
        assert!(response_env.graph_state.loaded.is_none());
        assert!(response_env.graph_state.initialized.is_none());
        assert!(response_env.graph_state.reconciliation_status.is_none());
        assert!(response_env.degraded.embed_worker_failed.is_none());
        assert_eq!(
            response_env.degraded.embed_persistence_unavailable,
            Some(true),
            "an incomplete selected graph keeps the daemon storage blocker"
        );
        let coverage = response_env
            .semantic_coverage
            .expect("selected-graph embedding coverage");
        assert_eq!(coverage.indexed, 1);
        assert_eq!(coverage.pending, 1);
        assert_eq!(coverage.total, 2);
        assert!(!coverage.complete);
    }

    /// A replayed status sample must own the envelope's freshness reading.
    ///
    /// The first-contact run received counters from 359601 ms earlier beside
    /// `_kin.freshness.state=recorded` and `age_seconds=6`, because the latter
    /// came from HEAD admission health. A reader scanning the standard envelope
    /// therefore saw a fresh-looking status over an empty past graph.
    #[test]
    fn replayed_graph_status_marks_the_standard_envelope_stale() {
        let stale_result = ToolCallResult::text(
            serde_json::json!({
                "schema": "kin.graph-status.v1",
                "view": "daemon_selected_graph",
                "scope": "head",
                "authority": "repo-daemon",
                "sampling": "last_settled_selected_graph",
                "authority_epoch": 7,
                "entity_count": 0,
                "durable_entity_count": 0,
                "relation_count": 0,
                "embedding_source": "selected_graph",
                "embeddings_indexed": 0,
                "embeddings_pending": 0,
                "embeddings_total": 0,
                "completion_attested": false,
                "stale": {
                    "reason": "embedding_coverage_changing",
                    "settled_age_ms": 359601,
                    "observed_authority_epoch": 8,
                    "live_attempts": 3,
                    "note": "the live sample was abandoned while embedding coverage changed"
                }
            })
            .to_string(),
        );
        let admission_health = serde_json::json!({
            "reconcile": {
                "untracked_path_count": 0,
                "untracked_observed_age_seconds": 0,
                "last_admission_success_at": "2026-09-04T14:00:00Z",
                "last_admission_success_age_seconds": 6
            }
        });
        let enveloped = finalize_daemon_graph_status(
            stale_result,
            Envelope::daemon().with_working_copy_health(&admission_health),
            &[],
            2,
        );
        let report = daemon_delegate::parse_graph_status_report(&enveloped)
            .expect("the stdio contract validates")
            .expect("the status call succeeds");
        let freshness = serde_json::to_value(
            report
                .response_envelope
                .expect("stdio status carries an envelope")
                .freshness
                .expect("a replayed sample carries freshness"),
        )
        .unwrap();
        assert_eq!(freshness["state"], "stale");
        assert_eq!(freshness["basis"], "selected_graph_sample");
        assert_eq!(freshness["settled_age_ms"], 359601);
        assert_eq!(freshness["live_attempts"], 3);
        assert!(
            freshness.get("age_seconds").is_none(),
            "the unrelated six-second admission clock must not survive: {freshness}"
        );
    }

    /// A replayed status sample over a level graph and a level working copy.
    ///
    /// This is the payload a daemon served while admitting a module it had just
    /// picked up: the working copy reading no longer counted the file as
    /// unadmitted, and the replayed counters predated its entities. The
    /// durability block read `recorded`, zero live-only entities and no
    /// `behind`, over a working copy holding an uncommitted module. The
    /// counters stay as observed and the state must not certify them.
    #[test]
    fn a_replayed_graph_status_does_not_publish_a_durability_all_clear() {
        let level_report = serde_json::json!({
            "schema": "kin.graph-status.v1",
            "view": "daemon_selected_graph",
            "scope": "head",
            "authority": "repo-daemon",
            "sampling": "point_in_time_selected_graph",
            "authority_epoch": 0,
            "entity_count": 6,
            "durable_entity_count": 6,
            "relation_count": 13,
            "durable_relation_count": 13,
            "embedding_source": "selected_graph",
            "embeddings_indexed": 0,
            "embeddings_pending": 12,
            "embeddings_total": 12,
            "completion_attested": false
        });
        let mut replayed_report = level_report.clone();
        replayed_report["sampling"] = serde_json::json!("last_settled_selected_graph");
        replayed_report["stale"] = serde_json::json!({
            "reason": "selected_graph_changing",
            "settled_age_ms": 2023,
            "live_attempts": 3,
            "note": "the live sample was abandoned while graph authority moved"
        });
        let stale_result = ToolCallResult::text(replayed_report.to_string());
        let level_working_copy = serde_json::json!({
            "reconcile": {
                "untracked_path_count": 0,
                "untracked_observed_age_seconds": 0,
                "last_admission_success_at": "2026-09-27T00:35:27Z",
                "last_admission_success_age_seconds": 0
            }
        });
        let enveloped = finalize_daemon_graph_status(
            stale_result,
            Envelope::daemon().with_working_copy_health(&level_working_copy),
            &[],
            2,
        );
        let report = daemon_delegate::parse_graph_status_report(&enveloped)
            .expect("the stdio contract validates")
            .expect("the status call succeeds");
        let envelope = report
            .response_envelope
            .expect("stdio status carries an envelope");
        assert!(
            envelope.behind.is_none(),
            "the working copy reading is level"
        );
        let durability = envelope
            .durability
            .expect("graph status reports durability");
        assert_eq!(durability.state, "unknown", "{durability:?}");
        assert_eq!(durability.live_only_entities, None, "{durability:?}");
        assert_eq!(durability.live_only_relations, None, "{durability:?}");
        assert_eq!(durability.live_entities, Some(6), "{durability:?}");
        assert_eq!(durability.durable_entities, Some(6), "{durability:?}");

        // The control: the same counters sampled live keep the all-clear, so
        // this does not qualify every status answer.
        let live = finalize_daemon_graph_status(
            ToolCallResult::text(level_report.to_string()),
            Envelope::daemon().with_working_copy_health(&level_working_copy),
            &[],
            2,
        );
        let live = daemon_delegate::parse_graph_status_report(&live)
            .expect("the stdio contract validates")
            .expect("the status call succeeds")
            .response_envelope
            .expect("stdio status carries an envelope")
            .durability
            .expect("graph status reports durability");
        assert_eq!(live.state, "recorded", "{live:?}");
        assert_eq!(live.live_only_entities, Some(0), "{live:?}");
    }

    #[test]
    fn graph_status_stdio_schema_rejects_a_mixed_head_envelope() {
        let enveloped =
            finalize_daemon_graph_status(direct_graph_status_result(), Envelope::daemon(), &[], 2);
        let ContentBlock::Text { text } = &enveloped.content[0];
        let mut payload: serde_json::Value = serde_json::from_str(text).unwrap();
        payload["_kin"]["graph_state"]["entity_count"] = serde_json::json!(999);
        payload["_kin"]["graph_as_of"] = serde_json::json!({ "generation": 77 });

        let error = serde_json::from_value::<crate::handlers::entities::GraphStatusReport>(payload)
            .expect_err("HEAD metadata must not validate beside a temporal selected graph");
        assert!(
            error.to_string().contains("_kin graph_as_of")
                || error.to_string().contains("_kin graph_state"),
            "{error}"
        );
    }

    #[test]
    fn graph_status_stdio_replaces_daemon_supplied_envelope_and_validates_coverage_note() {
        let enveloped =
            finalize_daemon_graph_status(direct_graph_status_result(), Envelope::daemon(), &[], 2);
        let ContentBlock::Text { text } = &enveloped.content[0];
        let mut payload: serde_json::Value = serde_json::from_str(text).unwrap();
        payload["_kin"]["graph_state"]["head_generation"] = serde_json::json!(77);

        let sanitized = finalize_daemon_graph_status(
            ToolCallResult::text(payload.to_string()),
            Envelope::daemon(),
            &[],
            2,
        );
        let ContentBlock::Text { text } = &sanitized.content[0];
        let sanitized_payload: serde_json::Value = serde_json::from_str(text).unwrap();
        assert!(
            sanitized_payload["_kin"]["graph_state"]
                .get("head_generation")
                .is_none(),
            "daemon-supplied additive envelope metadata must be stripped"
        );
        daemon_delegate::parse_graph_status_report(&sanitized)
            .expect("sanitized stdio report must validate")
            .expect("sanitized stdio report remains a success");

        // Envelope v2 sends no coverage sentence, so what the report validates is
        // the disclosure a reader acts on: `complete` has to agree with the
        // counters it is derived from.
        let mut disagreeing = sanitized_payload;
        disagreeing["_kin"]["semantic_coverage"]
            .as_object_mut()
            .unwrap()
            .insert("complete".to_string(), serde_json::json!(true));
        let error =
            serde_json::from_value::<crate::handlers::entities::GraphStatusReport>(disagreeing)
                .expect_err("incomplete coverage may not report itself complete");
        assert!(
            error
                .to_string()
                .contains("semantic_coverage disagrees with selected-graph status"),
            "{error}"
        );
    }

    fn response_pressure_refusal(work: &str) -> kin_core::memory_pressure::PressureRefusal {
        kin_core::memory_pressure::PressureRefusal {
            work: work.to_string(),
            level: "constrained".to_string(),
            reason: format!("{work} was refused"),
            at_unix: 1,
            from_budget: false,
        }
    }

    fn finalized_graph_status_with_pressure(
        work: &str,
        pending: usize,
        indexed: usize,
        total: usize,
    ) -> serde_json::Value {
        let refusal = response_pressure_refusal(work);
        // Production order: the typed report and durable refusal meet at the
        // graph-status finalizer. No second resources sample participates.
        let result = finalize_daemon_graph_status(
            direct_graph_status_result_with_coverage(pending, indexed, total),
            Envelope::daemon(),
            std::slice::from_ref(&refusal),
            2,
        );
        let ContentBlock::Text { text } = result.content.first().expect("one content block");
        serde_json::from_str(text).expect("annotated graph-status response")
    }

    #[test]
    fn graph_status_preserves_only_pressure_for_work_the_selected_graph_still_owes() {
        for (name, payload) in [
            (
                "incomplete selected embedding coverage",
                finalized_graph_status_with_pressure("embed-batch", 1, 8, 9),
            ),
            (
                "live embedding backlog",
                finalized_graph_status_with_pressure("embed-batch", 1, 9, 9),
            ),
            (
                "language-server work",
                finalized_graph_status_with_pressure("lsp-sweep", 0, 9, 9),
            ),
            (
                "future work",
                finalized_graph_status_with_pressure("future-heavy-work", 0, 9, 9),
            ),
        ] {
            assert_eq!(
                payload[ENVELOPE_KEY]["degraded"]["memory_pressure"],
                serde_json::json!(true),
                "{name} must survive selected-graph finalization: {payload}"
            );
        }

        let complete = finalized_graph_status_with_pressure("embed-batch", 0, 9, 9);
        assert!(
            complete[ENVELOPE_KEY]["degraded"]
                .get("memory_pressure")
                .is_none(),
            "exactly complete embedding coverage retires the response-level signal: {complete}"
        );
    }

    #[test]
    fn graph_status_qualifies_unavailable_persistence_by_selected_coverage() {
        let incomplete = finalize_daemon_graph_status(
            direct_graph_status_result_with_coverage(1, 8, 9),
            Envelope::daemon().with_embed_persistence_unavailable(true),
            &[],
            2,
        );
        let ContentBlock::Text { text } = incomplete.content.first().expect("one content block");
        let incomplete: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(
            incomplete[ENVELOPE_KEY]["degraded"]["embed_persistence_unavailable"],
            serde_json::json!(true),
            "incomplete selected coverage has no producer that can fill it"
        );

        let complete = finalize_daemon_graph_status(
            direct_graph_status_result_with_coverage(0, 9, 9),
            Envelope::daemon().with_embed_persistence_unavailable(true),
            &[],
            2,
        );
        let ContentBlock::Text { text } = complete.content.first().expect("one content block");
        let complete: serde_json::Value = serde_json::from_str(text).unwrap();
        assert!(
            complete[ENVELOPE_KEY]["degraded"]
                .get("embed_persistence_unavailable")
                .is_none(),
            "a fully covered selected graph has no embedding work for the unavailable producer"
        );
    }

    fn finalized_empty_locate_with_pressure(
        work: &str,
        embedding_coverage: Option<kin_core::memory_pressure::EmbeddingCoverage>,
    ) -> serde_json::Value {
        let refusal = daemon_delegate::pressure_refusal_for_coverage(
            response_pressure_refusal(work),
            embedding_coverage,
        );
        let env = Envelope::daemon()
            .with_selected_graph_observation(
                envelope::DurabilityCounts {
                    live_entities: 1,
                    durable_entities: Some(1),
                    live_relations: Some(0),
                    durable_relations: Some(0),
                },
                1,
                0,
                1,
            )
            .with_memory_pressure(refusal.as_ref());
        let result = envelope::finalize(
            ToolCallResult::text(
                serde_json::json!({
                    "query": "missing_authenticator",
                    "results": [],
                    "total_ranked": 0,
                })
                .to_string(),
            ),
            env,
            "semantic_locate",
        );
        let ContentBlock::Text { text } = result.content.first().expect("one content block");
        serde_json::from_str(text).expect("annotated locate response")
    }

    #[test]
    fn memory_pressure_qualifies_verdicts_only_for_outstanding_work() {
        let coverage = |pending, indexed, total| kin_core::memory_pressure::EmbeddingCoverage {
            pending,
            indexed,
            total,
        };
        let completed_embed =
            finalized_empty_locate_with_pressure("embed-batch", Some(coverage(0, 9, 9)));
        assert!(
            completed_embed[ENVELOPE_KEY]["degraded"]
                .get("memory_pressure")
                .is_none(),
            "completed embedding work must not leave a degradation: {completed_embed}"
        );
        assert!(!completed_embed["negative"]["degraded_signals"]
            .as_array()
            .expect("negative degradation labels")
            .iter()
            .any(|label| label.as_str() == Some("memory_pressure")));
        assert_eq!(
            completed_embed["negative"]["trust"], "authoritative",
            "whole coverage must not poison an otherwise exact semantic absence"
        );
        assert_eq!(
            completed_embed[ENVELOPE_KEY]["verdict"]["inputs"]["absence_gate"],
            "certified"
        );

        // An outstanding sweep stays visible on every answer and bounds only
        // the relation answers it feeds. This one ranks vectors over whole
        // embeddings, so it certifies and names the refusal it considered.
        let held_sweep = finalized_empty_locate_with_pressure("lsp-sweep", Some(coverage(0, 9, 9)));
        assert_eq!(
            held_sweep[ENVELOPE_KEY]["degraded"]["memory_pressure"], true,
            "a held sweep stays visible: {held_sweep}"
        );
        assert!(
            held_sweep["negative"]["degraded_signals"]
                .as_array()
                .expect("negative degradation labels")
                .iter()
                .any(|label| label.as_str() == Some("memory_pressure")),
            "a held sweep reaches the response-level negative: {held_sweep}"
        );
        assert_eq!(
            held_sweep["negative"]["trust"], "authoritative",
            "a held sweep does not bound a vector ranking: {held_sweep}"
        );
        assert!(
            held_sweep["negative"]["trust_reason"]
                .as_str()
                .unwrap_or_default()
                .contains(
                    "the disclosed signals [memory_pressure] were considered and are not \
                     load-bearing for this claim"
                ),
            "{held_sweep}"
        );
        assert_eq!(
            held_sweep[ENVELOPE_KEY]["verdict"]["inputs"]["absence_gate"],
            "certified"
        );

        for (work, observed) in [
            ("embed-batch", Some(coverage(1, 9, 9))),
            ("embed-batch", Some(coverage(0, 8, 9))),
            ("embed-batch", None),
            ("future-heavy-work", Some(coverage(0, 9, 9))),
        ] {
            let response = finalized_empty_locate_with_pressure(work, observed);
            assert_eq!(
                response[ENVELOPE_KEY]["degraded"]["memory_pressure"], true,
                "{work} with coverage {observed:?} stays visible: {response}"
            );
            assert!(
                response["negative"]["degraded_signals"]
                    .as_array()
                    .expect("negative degradation labels")
                    .iter()
                    .any(|label| label.as_str() == Some("memory_pressure")),
                "{work} must reach the response-level negative: {response}"
            );
            assert_eq!(response["negative"]["trust"], "inconclusive");
            assert_eq!(
                response[ENVELOPE_KEY]["verdict"]["inputs"]["absence_gate"],
                "inconclusive"
            );
        }
    }

    // ── Track C: confidence-qualified negatives ride the envelope through the
    //    real dispatch chokepoint, on the offline path, across tool groups. ──────

    /// Assert the additive `negative` contract is present and shaped, without
    /// disturbing the envelope or the original payload keys.
    fn assert_negative(payload: &serde_json::Value, tool: &str, kind: &str) -> serde_json::Value {
        assert_offline_envelope(payload, tool);
        let negative = payload
            .get("negative")
            .unwrap_or_else(|| panic!("tool {tool} empty result must carry a `negative` contract"));
        assert_eq!(negative["kind"], kind, "tool {tool} negative kind");
        // Offline is a fallback surface: absence is never authoritative here.
        assert_eq!(
            negative["safe_to_conclude_absent"], false,
            "tool {tool} offline absence must be inconclusive"
        );
        assert_eq!(negative["trust"], "inconclusive", "tool {tool}");
        assert!(
            negative["advice"].as_str().is_some_and(|a| !a.is_empty()),
            "tool {tool} negative must carry human advice"
        );
        negative.clone()
    }

    #[tokio::test]
    async fn negative_contract_on_empty_object_payload_search() {
        // Object payload: `negative` is added beside `_kin` and the untouched
        // `results` key.
        let payload = call_tool_payload(
            "semantic_search",
            serde_json::json!({ "query": "nonexistent" }),
        )
        .await;
        let negative = assert_negative(&payload, "semantic_search", "no_entity_match");
        assert_eq!(negative["result_count"], 0);
        assert_eq!(negative["interpretation"], "absent_as_indexed");
        // Back-compat: the original result collection still lives where agents
        // expect it, empty.
        assert_eq!(
            payload["results"].as_array().map(|a| a.len()),
            Some(0),
            "negative must not displace the original `results` key"
        );
    }

    #[tokio::test]
    async fn negative_contract_on_empty_bare_array_dead_code() {
        // Bare-array payload: the annotator wraps it under `result`; `negative`
        // rides alongside.
        let payload = call_tool_payload("dead_code", serde_json::json!({})).await;
        let negative = assert_negative(&payload, "dead_code", "no_dead_code");
        assert_eq!(negative["result_count"], 0);
        assert!(
            payload["result"].is_array(),
            "bare-array dead_code payload is wrapped under `result`"
        );
    }

    #[tokio::test]
    async fn no_negative_on_non_retrieval_tool() {
        // Work-graph listing is not a code-retrieval negative surface: an empty
        // work list must NOT be dressed up as a confidence-qualified absence.
        let payload = call_tool_payload("kin_work_list", serde_json::json!({})).await;
        assert_offline_envelope(&payload, "kin_work_list");
        assert!(
            payload.get("negative").is_none(),
            "non-retrieval tools must not carry a `negative` contract"
        );
    }

    #[test]
    fn default_config_requires_daemon_session_authority() {
        let config = McpServerConfig::default();
        assert_eq!(
            config.session_authority_mode,
            SessionAuthorityMode::DaemonRequired
        );
        assert!(config.session_authority_mode.requires_daemon());
    }

    #[tokio::test]
    async fn offline_graph_status_discloses_staged_work_without_inventing_graph_counters() {
        let store = InMemoryGraph::new();
        let sessions = SessionRegistry::new();
        sessions.register("owner", "test");
        let transaction = sessions
            .begin_transaction("owner", "selected-entity")
            .unwrap();
        sessions
            .stage_transaction(
                &transaction.transaction_id,
                vec![crate::session::McpMutationOperation {
                    verb: "update".into(),
                    target: "selected-entity".into(),
                    payload: None,
                    body: Some("private draft must not escape".into()),
                    description: "edit".into(),
                    destination: None,
                }],
            )
            .unwrap();
        let config = McpServerConfig {
            session_authority_mode: SessionAuthorityMode::OfflineFallback,
            allowed_tools: None,
            ..Default::default()
        };
        let message = serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"kin_graph_status","arguments":{"max_chars":60000}}})
        .to_string();
        let response = process_message(&message, &store, &config, &sessions)
            .await
            .unwrap();
        let result: ToolCallResult = serde_json::from_value(response.result.unwrap()).unwrap();
        assert_eq!(result.is_error, Some(true));
        let crate::types::ContentBlock::Text { text } = &result.content[0];
        let value: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(value["graph_status"], "unavailable");
        assert_eq!(
            value["open_transactions"]["items"][0]["transaction_id"],
            transaction.transaction_id
        );
        assert!(value.get("entity_count").is_none());
        assert!(value.get("completion_attested").is_none());
        assert!(!text.contains("private draft must not escape"));
        assert_ne!(value["_kin"]["verdict"]["safe_to_conclude_absent"], true);
        sessions
            .abort_transaction(&transaction.transaction_id)
            .unwrap();
        let response = process_message(&message, &store, &config, &sessions)
            .await
            .unwrap();
        let result: ToolCallResult = serde_json::from_value(response.result.unwrap()).unwrap();
        let crate::types::ContentBlock::Text { text } = &result.content[0];
        let value: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(value["open_transactions"]["items"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn process_initialize() {
        let config = McpServerConfig::default();
        let sessions = SessionRegistry::new();
        let store = InMemoryGraph::default();

        let msg = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
        let resp = process_message(msg, &store, &config, &sessions)
            .await
            .unwrap();
        assert!(resp.result.is_some());
        assert!(resp.error.is_none());

        let result = resp.result.unwrap();
        assert_eq!(result["serverInfo"]["name"], "kin-mcp");
        // P2-2.3: kinVersion must be present in serverInfo
        assert!(result["serverInfo"]["kinVersion"].is_string());
        // The spec's instructions field must reach the wire with usable content.
        // What that content has to hold is asserted once, in
        // `agent_belt::tests::the_server_instructions_name_only_tools_the_profile_serves`,
        // against the served profiles. This asserts only that the string
        // survives the round trip, since a client that hides tool schemas has
        // nothing else to give the model.
        let instructions = result["instructions"].as_str().unwrap();
        assert_eq!(instructions, SERVER_INSTRUCTIONS);
        assert!(instructions.contains("locate"));
        // The one verdict has to be named on the wire, or an agent learns the
        // contract from whichever block it happens to read first.
        assert!(instructions.contains("_kin.verdict"));
    }

    #[tokio::test]
    async fn initialize_with_newer_protocol_version_includes_warning() {
        let config = McpServerConfig::default();
        let sessions = SessionRegistry::new();
        let store = InMemoryGraph::default();

        let msg = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2099-01-01"}}"#;
        let resp = process_message(msg, &store, &config, &sessions)
            .await
            .unwrap();
        assert!(resp.result.is_some());
        assert!(resp.error.is_none());

        let result = resp.result.unwrap();
        // A client newer than this server is answered with its newest version.
        assert_eq!(result["protocolVersion"], SUPPORTED_PROTOCOL_VERSIONS[0]);
        // Warning is present
        assert!(result["_warning"].is_string());
        assert!(result["_warning"].as_str().unwrap().contains("2099-01-01"));
    }

    #[tokio::test]
    async fn initialize_with_matching_protocol_version_no_warning() {
        let config = McpServerConfig::default();
        let sessions = SessionRegistry::new();
        let store = InMemoryGraph::default();

        let msg = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05"}}"#;
        let resp = process_message(msg, &store, &config, &sessions)
            .await
            .unwrap();
        let result = resp.result.unwrap();
        assert!(result.get("_warning").is_none());
    }

    #[tokio::test]
    async fn process_tools_list() {
        let config = McpServerConfig::default();
        let sessions = SessionRegistry::new();
        let store = InMemoryGraph::default();

        let msg = r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#;
        let resp = process_message(msg, &store, &config, &sessions)
            .await
            .unwrap();
        assert!(resp.result.is_some());

        let tools = &resp.result.unwrap()["tools"];
        assert!(tools.is_array());
        let tools = tools.as_array().unwrap();
        assert!(!tools.is_empty());

        // The annotations have to survive the serving path, not just the
        // registry: a client reads them from this response and from nowhere
        // else.
        for tool in tools {
            assert!(
                tool["annotations"]["title"].is_string(),
                "{} reached the wire with no title",
                tool["name"]
            );
            assert!(tool["annotations"]["readOnlyHint"].is_boolean());
        }

        let names: Vec<&str> = tools
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted, "tools/list must serve a stable name order");
    }

    #[tokio::test]
    async fn process_tools_call_semantic_search() {
        let mut config = McpServerConfig::default();
        config.session_authority_mode = SessionAuthorityMode::OfflineFallback;
        let sessions = SessionRegistry::new();
        let store = InMemoryGraph::default();

        let msg = r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"semantic_search","arguments":{"query":"foo"}}}"#;
        let resp = process_message(msg, &store, &config, &sessions)
            .await
            .unwrap();
        assert!(resp.result.is_some());
        assert!(resp.error.is_none());
    }

    #[tokio::test]
    async fn process_tools_call_semantic_locate_requires_daemon_offline() {
        // End-to-end dispatch check: semantic_locate must reach
        // handle_semantic_locate and report the daemon requirement rather than
        // silently degrading to a metadata filter when no daemon graph is
        // present.
        let mut config = McpServerConfig::default();
        config.session_authority_mode = SessionAuthorityMode::OfflineFallback;
        let sessions = SessionRegistry::new();
        let store = InMemoryGraph::default();

        let msg = r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"semantic_locate","arguments":{"query":"where is auth handled"}}}"#;
        let resp = process_message(msg, &store, &config, &sessions)
            .await
            .unwrap();
        assert!(resp.error.is_none());
        let result: ToolCallResult = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert_eq!(result.is_error, Some(true));
        let text = match result.content.first().unwrap() {
            ContentBlock::Text { text } => text,
        };
        assert!(
            text.contains("requires the Kin daemon"),
            "expected daemon-required message, got: {text}"
        );
    }

    #[tokio::test]
    async fn daemon_required_tools_do_not_use_local_handlers() {
        // Bind this end-to-end dispatch test to a Kin repository explicitly.
        // Without a store marker, the production delegate correctly reports the
        // distinct "not inside a kin repository" state before it can prove the
        // daemon-required branch this test is intended to lock down.
        //
        // The repository is private to this test, and the delegate is pointed
        // at it rather than at the process working directory. That directory is
        // shared by every test in this binary and the work handlers' tests move
        // it into repositories of their own, so a `.kin` made beside the crate
        // was sometimes looked for from another test's repository, or from one
        // already deleted, and this test read "not inside a kin repository".
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir(repo.path().join(".kin")).unwrap();
        std::fs::write(repo.path().join(".kin/version"), "2\n").unwrap();
        let _working_dir = crate::daemon_delegate::TestWorkingDir::enter(repo.path());
        let _daemon_url = kin_core::test_env::EnvVarGuard::unset("KIN_DAEMON_URL");
        let config = McpServerConfig::default();
        let sessions = SessionRegistry::new();
        let store = InMemoryGraph::default();

        let msg = r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"semantic_search","arguments":{"query":"foo"}}}"#;
        let resp = process_message(msg, &store, &config, &sessions)
            .await
            .unwrap();
        // Exercise the actual stdio egress, after private authority grading.
        let resp = present_for_client(resp, msg, &config);
        assert!(resp.error.is_none());
        let result: ToolCallResult = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert_eq!(result.is_error, Some(true));
        let text = match result.content.first().unwrap() {
            ContentBlock::Text { text } => text,
        };
        // The public error preserves the selected authority and the gap,
        // while its private transport diagnostic is not agent knowledge.
        let payload: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(payload["_kin"]["runtime"], "repo-daemon", "{payload}");
        assert_eq!(
            payload["_kin"]["degraded"]["daemon_unreachable"], true,
            "{payload}"
        );
        assert!(
            payload.get("results").is_none(),
            "no local result may replace the required authority: {payload}"
        );
        assert!(!text.contains(repo.path().to_str().unwrap()), "{text}");
    }

    #[tokio::test]
    async fn process_tools_call_register_session() {
        let mut config = McpServerConfig::default();
        config.session_authority_mode = SessionAuthorityMode::OfflineFallback;
        let sessions = SessionRegistry::new();
        let store = InMemoryGraph::default();

        let msg = r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"register_session","arguments":{"assistant_name":"claude-code","session_id":"test-123"}}}"#;
        let resp = process_message(msg, &store, &config, &sessions)
            .await
            .unwrap();
        assert!(resp.result.is_some());
        assert_eq!(sessions.count(), 1);
    }

    #[tokio::test]
    async fn process_daemon_message_handles_transport_methods_without_store() {
        let config = McpServerConfig::default();
        let msg = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
        let resp = process_daemon_message(msg, &config).await.unwrap();
        assert!(resp.result.is_some());
        assert!(resp.error.is_none());
    }

    #[tokio::test]
    async fn resource_uris_cannot_bypass_entity_only_source_tools() {
        let config = McpServerConfig::default();
        let init = handle_initialize(Some(serde_json::json!(1)), &serde_json::json!({}), &config);
        assert!(init.result.unwrap()["capabilities"]
            .get("resources")
            .is_none());
        for method in [
            "resources/list",
            "resources/templates/list",
            "resources/read",
        ] {
            for uri in ["file:///README.md", "kin://artifact/README.md"] {
                let request = serde_json::json!({"jsonrpc":"2.0","id":1,"method":method,"params":{"uri":uri}}).to_string();
                for daemon in [false, true] {
                    let response = if daemon {
                        process_daemon_message(&request, &config).await
                    } else {
                        process_message(
                            &request,
                            &InMemoryGraph::default(),
                            &config,
                            &SessionRegistry::new(),
                        )
                        .await
                    }
                    .unwrap();
                    assert_eq!(response.error.unwrap().code, -32601);
                    assert!(response.result.is_none());
                }
            }
        }
    }

    #[tokio::test]
    async fn retired_operations_are_absent_and_refused_on_every_profile_and_transport() {
        let mut configs = vec![
            McpServerConfig::default(),
            default_config(),
            query_config(),
            with_writes(),
            read_only(),
        ];
        for names in [
            crate::tools::agent_search_tool_names(),
            crate::tools::benchmark_tool_names(),
            crate::tools::context_bench_tool_names(),
        ] {
            configs.push(McpServerConfig {
                allowed_tools: Some(crate::tools::name_set(names)),
                ..Default::default()
            });
        }
        for retired in [
            "kin_artifact_list",
            "list_file_entities",
            "init",
            "kin_init",
            "exec",
            "kin_session_exec",
            "kin_todo_import",
        ] {
            for config in &configs {
                let listed = handle_tools_list(Some(serde_json::json!(1)), config);
                assert!(!listed.result.as_ref().unwrap()["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|tool| tool["name"] == retired));
                for daemon in [false, true] {
                    for (name, args) in [
                        (retired, serde_json::json!({"path":"README.md"})),
                        (
                            "kin_tool_call",
                            serde_json::json!({"tool":retired,"arguments":{"path":"README.md"}}),
                        ),
                        (
                            "kin",
                            serde_json::json!({"command":retired,"args":{"path":"README.md"}}),
                        ),
                        (
                            "kin",
                            serde_json::json!({"command":format!("kin {retired}"),"args":{"path":"README.md"}}),
                        ),
                        (
                            "kin",
                            serde_json::json!({"command":"call","args":{"tool":retired,"arguments":{"path":"README.md"}}}),
                        ),
                    ] {
                        if name == "kin" && config.routed.is_none() {
                            continue;
                        }
                        let request = tools_call(name, args);
                        let response = if daemon {
                            process_daemon_message(&request, config).await
                        } else {
                            process_message(
                                &request,
                                &InMemoryGraph::default(),
                                config,
                                &SessionRegistry::new(),
                            )
                            .await
                        }
                        .unwrap();
                        assert!(
                            response.error.is_some()
                                || response
                                    .result
                                    .as_ref()
                                    .is_some_and(|r| r["isError"] == true),
                            "{retired} through {name}: {:?}",
                            response.result
                        );
                        // An unfiltered daemon connection must refuse locally, not
                        // contact an older daemon that still knows this operation.
                        if name == retired && config.allowed_tools.is_none() {
                            let refusal = response
                                .error
                                .as_ref()
                                .map(|error| error.message.as_str())
                                .or_else(|| {
                                    response
                                        .result
                                        .as_ref()
                                        .and_then(|result| result["content"][0]["text"].as_str())
                                })
                                .unwrap();
                            assert!(
                                refusal.contains("unavailable on the semantic agent surface"),
                                "{refusal}"
                            );
                            assert!(!refusal.contains("README.md"), "{refusal}");
                        }
                    }
                }
            }
            let discovery =
                crate::handlers::tool_search::handle_tool_search(&std::collections::HashMap::new())
                    .unwrap();
            let ContentBlock::Text { text } = &discovery.content[0];
            let discovery: serde_json::Value = serde_json::from_str(text).unwrap();
            assert!(!discovery["matched_names"]
                .as_array()
                .unwrap()
                .iter()
                .any(|name| name == retired));
            assert!(!LEGACY_SERVER_INSTRUCTIONS.contains(&format!("`{retired}`")));
        }
    }

    #[tokio::test]
    async fn whole_artifact_read_is_absent_and_refused_on_every_profile_and_transport() {
        let mut configs = vec![
            McpServerConfig::default(),
            default_config(),
            query_config(),
            with_writes(),
            read_only(),
        ];
        for names in [
            crate::tools::agent_search_tool_names(),
            crate::tools::benchmark_tool_names(),
            crate::tools::context_bench_tool_names(),
        ] {
            configs.push(McpServerConfig {
                allowed_tools: Some(crate::tools::name_set(names)),
                ..Default::default()
            });
        }
        for config in configs {
            let listed = handle_tools_list(Some(serde_json::json!(1)), &config);
            assert!(!serde_json::to_string(&listed.result)
                .unwrap()
                .contains("kin_artifact_read"));
            for daemon in [false, true] {
                for (name, args) in [
                    ("kin_artifact_read", serde_json::json!({"path":"README.md"})),
                    (
                        "kin_tool_call",
                        serde_json::json!({"tool":"kin_artifact_read","arguments":{"path":"README.md"}}),
                    ),
                    (
                        "kin",
                        serde_json::json!({"command":"read","args":{"path":"README.md"}}),
                    ),
                    (
                        "kin",
                        serde_json::json!({"command":"kin_artifact_read","args":{"path":"README.md"}}),
                    ),
                    (
                        "kin",
                        serde_json::json!({"command":"call","args":{"tool":"kin_artifact_read","arguments":{"path":"README.md"}}}),
                    ),
                ] {
                    // The routed tool is meaningful only on a routed profile.
                    if name == "kin" && config.routed.is_none() {
                        continue;
                    }
                    let request = tools_call(name, args);
                    let response = if daemon {
                        process_daemon_message(&request, &config).await
                    } else {
                        process_message(
                            &request,
                            &InMemoryGraph::default(),
                            &config,
                            &SessionRegistry::new(),
                        )
                        .await
                    }
                    .unwrap();
                    assert!(
                        response.error.is_some()
                            || response
                                .result
                                .as_ref()
                                .is_some_and(|r| r["isError"] == true),
                        "{name}: {:?}",
                        response.result
                    );
                }
            }
        }
        let discovery =
            crate::handlers::tool_search::handle_tool_search(&std::collections::HashMap::new())
                .unwrap();
        assert!(!serde_json::to_string(&discovery)
            .unwrap()
            .contains("kin_artifact_read"));
    }

    #[tokio::test]
    async fn discovery_reports_profile_eligibility_without_activating_tools() {
        for daemon_route in [false, true] {
            for filtered in [false, true] {
                let config = McpServerConfig {
                    allowed_tools: filtered
                        .then(|| crate::tools::name_set(crate::tools::agent_search_tool_names())),
                    agent_belt: filtered,
                    session_authority_mode: SessionAuthorityMode::OfflineFallback,
                    ..McpServerConfig::default()
                };
                let store = InMemoryGraph::default();
                let sessions = SessionRegistry::new();
                let before = handle_tools_list(Some(serde_json::json!(1)), &config);
                let listed = before.result.as_ref().unwrap()["tools"].as_array().unwrap();
                let search_description = listed
                    .iter()
                    .find(|tool| tool["name"] == "kin_tool_search")
                    .unwrap()["description"]
                    .as_str()
                    .unwrap();
                assert!(search_description.contains("Discovery does not"));
                assert!(!search_description.contains("callable definition"));
                for tool in listed {
                    assert!(!tool["description"]
                        .as_str()
                        .unwrap()
                        .contains("then call it on the next turn"));
                }
                let search = r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"kin_tool_search","arguments":{"need":"impact","limit":1}}}"#;
                let response = if daemon_route {
                    process_daemon_message(search, &config).await
                } else {
                    process_message(search, &store, &config, &sessions).await
                }
                .expect("search response");
                let result = response.result.expect("search result");
                let payload: serde_json::Value =
                    serde_json::from_str(result["content"][0]["text"].as_str().expect("JSON text"))
                        .unwrap();
                assert_eq!(payload["matches"][0]["name"], crate::verbs::IMPACT);
                assert_eq!(
                    payload["invocation"]["profile_enabled"][crate::verbs::IMPACT],
                    !filtered
                );
                assert_eq!(payload["invocation"]["discovery_changes_profile"], false);
                assert_eq!(payload["invocation"]["normal_authorization_required"], true);
                let after = handle_tools_list(Some(serde_json::json!(3)), &config);
                assert_eq!(before.result, after.result);

                if filtered {
                    let invoke = r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"impact","arguments":{}}}"#;
                    let response = if daemon_route {
                        process_daemon_message(invoke, &config).await
                    } else {
                        process_message(invoke, &store, &config, &sessions).await
                    }
                    .unwrap();
                    let result = response.result.unwrap();
                    assert_eq!(result["isError"], true);
                    assert!(result["content"][0]["text"]
                        .as_str()
                        .unwrap()
                        .contains("not enabled in this MCP profile"));
                }
            }
        }
    }

    /// A connection that serves plain replies renders every answer at the
    /// last step: one plain verdict and none of the verdict codes. The same
    /// connection asked for machine replies at `initialize` serves the codes.
    #[tokio::test]
    async fn an_agent_profile_answers_in_plain_words_unless_asked_for_machine_replies() {
        let mut plain = McpServerConfig {
            allowed_tools: Some(crate::tools::name_set(
                crate::tools::agent_default_tool_names(),
            )),
            agent_belt: true,
            plain_replies: true,
            session_authority_mode: SessionAuthorityMode::OfflineFallback,
            ..McpServerConfig::default()
        };
        let store = InMemoryGraph::default();
        let sessions = SessionRegistry::new();
        let call = serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {"name": crate::verbs::LOCATE, "arguments": {"query": "nothing_here"}}
        })
        .to_string();
        let text = |response: JsonRpcResponse| {
            response.result.expect("a result")["content"][0]["text"]
                .as_str()
                .expect("a text block")
                .to_string()
        };
        let answered = text(present_for_client(
            process_message(&call, &store, &plain, &sessions)
                .await
                .unwrap(),
            &call,
            &plain,
        ));
        for code in [
            "limiting_factor",
            "safe_to_conclude_absent",
            "inconclusive",
            "negative",
        ] {
            assert!(
                !answered.contains(code),
                "{code} reached an agent: {answered}"
            );
        }
        let reply: serde_json::Value = serde_json::from_str(&answered).unwrap();
        assert!(reply["_kin"]["meaning"].is_string(), "{reply}");

        plain.apply_client_capabilities(&serde_json::json!({"params": {"capabilities": {
            "experimental": {"kin": {"machineReplies": true}}
        }}}));
        let machine = text(present_for_client(
            process_message(&call, &store, &plain, &sessions)
                .await
                .unwrap(),
            &call,
            &plain,
        ));
        let reply: serde_json::Value = serde_json::from_str(&machine).unwrap();
        assert!(reply["_kin"]["verdict"]["state"].is_string(), "{reply}");
    }

    /// An agent profile answers a reference call in the answer-only shape even
    /// when the call asks for detail, by a verb or by the name it replaced, and
    /// adds one line after the answer saying what it did not take. The answer
    /// block itself is the tool's own. `full` takes the toggle and adds nothing.
    #[tokio::test]
    async fn an_agent_profile_answers_answer_only_and_says_what_it_did_not_take() {
        let belt = McpServerConfig {
            allowed_tools: Some(crate::tools::name_set(
                crate::tools::agent_default_tool_names(),
            )),
            agent_belt: true,
            session_authority_mode: SessionAuthorityMode::OfflineFallback,
            ..McpServerConfig::default()
        };
        let full = McpServerConfig {
            session_authority_mode: SessionAuthorityMode::OfflineFallback,
            ..McpServerConfig::default()
        };
        let store = InMemoryGraph::default();
        let sessions = SessionRegistry::new();
        let call = |name: &str, arguments: serde_json::Value| {
            serde_json::json!({
                "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                "params": {"name": name, "arguments": arguments}
            })
            .to_string()
        };
        let blocks = |response: JsonRpcResponse| {
            response.result.expect("a result")["content"]
                .as_array()
                .expect("content blocks")
                .clone()
        };
        for name in [crate::verbs::REFS, "find_references"] {
            let detailed = serde_json::json!({"query": "nothing_here", "answer_only": false});
            let answered = blocks(
                process_message(&call(name, detailed.clone()), &store, &belt, &sessions)
                    .await
                    .unwrap(),
            );
            assert_eq!(answered.len(), 2, "{name}: {answered:?}");
            let note = answered[1]["text"].as_str().unwrap();
            assert!(
                note.contains("answer_only") && note.contains("always gives the short answer"),
                "{note}"
            );
            let plain = blocks(
                process_message(
                    &call(name, serde_json::json!({"query": "nothing_here"})),
                    &store,
                    &belt,
                    &sessions,
                )
                .await
                .unwrap(),
            );
            assert_eq!(
                plain.len(),
                1,
                "{name}: a call that asked for nothing earns no note"
            );
            let on_full = blocks(
                process_message(&call(name, detailed), &store, &full, &sessions)
                    .await
                    .unwrap(),
            );
            assert_eq!(on_full.len(), 1, "{name}: full takes the toggle");
        }
    }

    #[tokio::test]
    async fn discovered_calls_reach_real_handlers_without_expanding_the_profile() {
        let config = McpServerConfig {
            allowed_tools: Some(crate::tools::name_set(
                crate::tools::agent_search_tool_names(),
            )),
            agent_belt: true,
            session_authority_mode: SessionAuthorityMode::OfflineFallback,
            ..McpServerConfig::default()
        };
        let store = InMemoryGraph::default();
        let sessions = SessionRegistry::new();
        let bytes = b"fn dispatched_symbol() {}\n";
        let kin_index::IndexedAny::EntitySource(indexed) = kin_index::IndexPipeline::new()
            .index_any_content(
                &kin_model::FilePathId::new("src/lib.rs"),
                bytes,
                kin_blobs::digest(bytes),
            )
            .unwrap()
        else {
            panic!("source fixture");
        };
        for entity in indexed.entities {
            kin_model::EntityStore::upsert_entity(&store, &entity).unwrap();
        }
        store.flush_text_index().unwrap();
        let before = handle_tools_list(Some(serde_json::json!(1)), &config);
        let message = |name: &str, arguments: serde_json::Value| {
            serde_json::json!({
            "jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":name,"arguments":arguments}
        }).to_string()
        };
        // `refs` is a read-only verb agent-search does not serve.
        let searched = process_message(
            &message(
                "kin_tool_search",
                serde_json::json!({"need":"refs","limit":1}),
            ),
            &store,
            &config,
            &sessions,
        )
        .await
        .unwrap()
        .result
        .unwrap();
        let discovery: serde_json::Value =
            serde_json::from_str(searched["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(discovery["matches"][0]["name"], crate::verbs::REFS);
        assert_eq!(
            discovery["invocation"]["profile_enabled"][crate::verbs::REFS],
            false
        );
        assert_eq!(
            discovery["invocation"]["callable_via_dispatcher"][crate::verbs::REFS],
            true
        );
        let args = serde_json::json!({"query":"dispatched_symbol"});
        for withheld in [crate::verbs::REFS, "find_references"] {
            let refused =
                process_message(&message(withheld, args.clone()), &store, &config, &sessions)
                    .await
                    .unwrap();
            assert_eq!(refused.result.unwrap()["isError"], true, "{withheld}");
        }
        let full = McpServerConfig {
            allowed_tools: None,
            ..config.clone()
        };
        let direct = process_message(
            &message(crate::verbs::REFS, args.clone()),
            &store,
            &full,
            &sessions,
        )
        .await
        .unwrap();
        let wrapped = process_message(
            &message(
                "kin_tool_call",
                serde_json::json!({"tool":crate::verbs::REFS,"arguments":args}),
            ),
            &store,
            &config,
            &sessions,
        )
        .await
        .unwrap();
        assert!(wrapped.error.is_none());
        let text = wrapped.result.as_ref().unwrap()["content"][0]["text"]
            .as_str()
            .unwrap();
        assert!(
            text.contains("dispatched_symbol") && !text.contains("src/lib.rs"),
            "{text}"
        );
        assert_ne!(wrapped.result.as_ref().unwrap()["isError"], true);
        assert_eq!(
            direct.result, wrapped.result,
            "target answer, negative evidence and envelope must be unchanged"
        );
        assert_eq!(
            before.result,
            handle_tools_list(Some(serde_json::json!(3)), &config).result
        );
    }

    /// One guarded whole-entity update as `kin_mutate` takes it: the new body
    /// beside a well-formed `source_base`, whose entity id is the target.
    ///
    /// No route these tests drive compares a base with repository bytes, so a
    /// base that passes `EntitySourceBase::validate` is all they need. A body
    /// without one is refused as `source_base_required` before the behaviour
    /// these tests assert about could run.
    fn guarded_update_operation(body: &str) -> serde_json::Value {
        let base = crate::source_base::EntitySourceBase {
            schema: crate::source_base::SourceBaseSchema::V1,
            context: crate::source_base::SourceBaseContext {
                repository_id: "server-test".into(),
                workspace_id: uuid::Uuid::new_v4().to_string(),
                workspace_generation: 1,
                workspace_head_hash: "a".repeat(64),
                workspace_tree_hash: "b".repeat(64),
            },
            entity_id: kin_model::EntityId::new(),
            artifact_id: kin_model::ArtifactId::new(),
            source_blob_hash: "c".repeat(64),
            start_byte: 0,
            end_byte: 12,
            body_hash: "d".repeat(64),
        };
        serde_json::json!({
            "verb": "update",
            "target": base.entity_id.to_string(),
            "payload": { "EntitySourceBase": crate::source_ticket::compact(&serde_json::to_value(base).unwrap()).unwrap() },
            "body": body,
            "description": "edit",
        })
    }

    #[tokio::test]
    async fn discovered_calls_keep_profile_and_mutation_authority_refusals() {
        // An admissible mutation, so the refusals below are the profile's and
        // the session authority's rather than the operation's.
        let call = serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{
            "name":"kin_tool_call","arguments":{"tool":"kin_mutate","arguments":{
                "operations":[guarded_update_operation("fn widget() {}")]
            }}
        }})
        .to_string();
        let store = InMemoryGraph::default();
        let sessions = SessionRegistry::new();
        for names in [
            crate::tools::agent_default_tool_names(),
            crate::tools::agent_query_tool_names(),
            crate::tools::context_bench_tool_names(),
        ] {
            let config = McpServerConfig {
                allowed_tools: Some(crate::tools::name_set(names)),
                session_authority_mode: SessionAuthorityMode::OfflineFallback,
                ..McpServerConfig::default()
            };
            for daemon in [false, true] {
                let response = if daemon {
                    process_daemon_message(&call, &config).await
                } else {
                    process_message(&call, &store, &config, &sessions).await
                }
                .unwrap();
                assert!(response.error.unwrap().message.contains("not enabled"));
            }
        }
        let config = McpServerConfig {
            allowed_tools: Some(crate::tools::name_set(
                crate::tools::agent_search_tool_names(),
            )),
            agent_belt: true,
            session_authority_mode: SessionAuthorityMode::OfflineFallback,
            ..McpServerConfig::default()
        };
        for daemon in [false, true] {
            let response = if daemon {
                process_daemon_message(&call, &config).await
            } else {
                process_message(&call, &store, &config, &sessions).await
            }
            .unwrap();
            assert!(response.error.unwrap().message.contains("read-only"));
        }
        assert_eq!(sessions.count(), 0);
    }

    /// The tool registry is answered by this binary on the daemon route, from
    /// the definitions `tools/list` is built from.
    ///
    /// The daemon route forwards every name it does not special-case to the
    /// daemon's generic MCP endpoint. Forwarding this one would answer from
    /// whatever registry the daemon build carries, which is how an agent gets a
    /// schema for a tool this server cannot dispatch. There is no daemon in this
    /// test, so a forwarded call could not answer at all.
    #[tokio::test]
    async fn the_tool_registry_is_answered_locally_on_the_daemon_route() {
        let config = McpServerConfig {
            allowed_tools: Some(crate::tools::name_set(
                crate::tools::agent_search_tool_names(),
            )),
            agent_belt: true,
            ..McpServerConfig::default()
        };

        let msg = format!(
            r#"{{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{{"name":"{}","arguments":{{"need":"impact"}}}}}}"#,
            crate::handlers::tool_search::TOOL_NAME
        );
        let resp = process_daemon_message(&msg, &config).await.unwrap();
        let result = resp.result.expect("the registry call is answered");
        assert_ne!(
            result.get("isError"),
            Some(&serde_json::json!(true)),
            "the registry call came back an error: {result:#?}"
        );
        let text = result["content"][0]["text"]
            .as_str()
            .expect("a text content block")
            .to_string();
        let discovery: serde_json::Value = serde_json::from_str(&text).unwrap();
        let matched = discovery["matches"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == crate::verbs::IMPACT)
            .unwrap_or_else(|| {
                panic!("a withheld tool was not reachable through the served search: {text}")
            });
        let registry = crate::tools::surface_definitions();
        let definition = registry
            .tools
            .iter()
            .find(|tool| tool.name == crate::verbs::IMPACT)
            .unwrap();
        assert_eq!(
            *matched,
            serde_json::to_value(definition).unwrap(),
            "discovery must return this binary's complete public schema"
        );
        assert_eq!(
            discovery["invocation"]["profile_enabled"][crate::verbs::IMPACT],
            false
        );

        // The control: a tool this profile does not serve is still refused, so
        // the short circuit above did not become a way around the profile.
        let refused = process_daemon_message(
            r#"{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"impact_analysis","arguments":{}}}"#,
            &config,
        )
        .await
        .unwrap()
        .result
        .expect("a refusal is still a result frame");
        assert!(
            refused["content"][0]["text"]
                .as_str()
                .unwrap_or_default()
                .contains("not enabled in this MCP profile"),
            "the profile filter stopped refusing withheld tools: {refused:#?}"
        );
    }

    /// A mutation is expanded by this binary on the daemon route, not forwarded.
    ///
    /// This is the one that cost a debugging session. Everything the daemon
    /// route does not special-case is handed to the daemon under the name the
    /// caller used, and the daemon runs what arrives under
    /// `SessionAuthorityMode::OfflineFallback`, because inside the daemon its
    /// own registry IS the authority. A forwarded `kin_mutate` therefore reaches
    /// `handle_mutate` with `uses_daemon()` FALSE, takes the in-process branch,
    /// and is refused by a commit path that has no projection. The daemon routes
    /// exactly one name to its exact-commit path, `kin_transaction_commit`, so a
    /// one-shot under any other name never reaches it.
    ///
    /// There is no daemon in this test, which is what makes the two answers
    /// distinguishable: the local expansion refuses for its own missing
    /// `session_id` before it ever reaches the wire, and a call that did reach
    /// the wire comes back as the daemon being unavailable.
    #[tokio::test]
    async fn a_mutation_is_expanded_locally_on_the_daemon_route() {
        let config = McpServerConfig::default();
        // Guarded, so the expansion's check on the operations passes and the
        // only thing left for it to refuse is the missing session.
        let edit = guarded_update_operation("pub fn widget() {}");

        // Keyed, because an unkeyed write that names no session runs in the
        // connection's own (`crate::connection_session`); a keyed one binds
        // its request to the session its caller retries under, so it still
        // has to name one, and the local expansion refuses it if it does not.
        let unsessioned = process_daemon_message(
            &serde_json::json!({"jsonrpc":"2.0","id":11,"method":"tools/call","params":{
                "name":"kin_mutate","arguments":{"operations":[edit], "request_id": "k1"}
            }})
            .to_string(),
            &config,
        )
        .await
        .unwrap()
        .result
        .expect("the mutation call is answered");
        let text = unsessioned["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        assert!(
            text.contains("session_id") && text.contains("kin_session_start"),
            "a locally expanded mutate refuses for its own missing session before the wire; \
             a forwarded one could not have produced this: {text}"
        );

        // The discriminating half. With a session named, the expansion has
        // nothing left to refuse of its own and goes on to forward its begin.
        // Without this, the assertion above would pass just as well against an
        // expansion that refused everything.
        //
        // What the wire then says is deliberately not asserted. This test runs
        // wherever the checkout sits, and whether a daemon answers depends on
        // whether a store exists above that directory, which is a fact about the
        // machine rather than about this route. The assertion that carries the
        // guard is the one above: only a locally expanded mutate names
        // `kin_session_start`, and a forwarded one cannot, under any daemon.
        let sessioned = process_daemon_message(
            &serde_json::json!({"jsonrpc":"2.0","id":12,"method":"tools/call","params":{
                "name":"kin_mutate","arguments":{
                    "session_id":"11111111-1111-4111-8111-111111111111",
                    "operations":[edit]
                }
            }})
            .to_string(),
            &config,
        )
        .await
        .unwrap()
        .result
        .expect("the mutation call is answered");
        let text = sessioned["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        assert!(
            !text.contains("kin_session_start"),
            "a named session was refused as missing: {text}"
        );
        assert!(
            !text.contains("source_base_required"),
            "the expansion refused the operations it should have forwarded: {text}"
        );
        assert_eq!(
            sessioned.get("isError"),
            Some(&serde_json::json!(true)),
            "a mutation with no daemon behind it still has to fail: {sessioned:#?}"
        );
    }

    #[tokio::test]
    async fn process_unknown_method() {
        let config = McpServerConfig::default();
        let sessions = SessionRegistry::new();
        let store = InMemoryGraph::default();

        let msg = r#"{"jsonrpc":"2.0","id":5,"method":"unknown/method","params":{}}"#;
        let resp = process_message(msg, &store, &config, &sessions)
            .await
            .unwrap();
        assert!(resp.error.is_some());
        assert_eq!(resp.error.unwrap().code, -32601);
    }

    #[tokio::test]
    async fn process_invalid_json() {
        let config = McpServerConfig::default();
        let sessions = SessionRegistry::new();
        let store = InMemoryGraph::default();

        let resp = process_message("not json", &store, &config, &sessions)
            .await
            .unwrap();
        assert!(resp.error.is_some());
        assert_eq!(resp.error.unwrap().code, -32700);
    }

    #[tokio::test]
    async fn process_ping() {
        let config = McpServerConfig::default();
        let sessions = SessionRegistry::new();
        let store = InMemoryGraph::default();

        let msg = r#"{"jsonrpc":"2.0","id":6,"method":"ping","params":{}}"#;
        let resp = process_message(msg, &store, &config, &sessions)
            .await
            .unwrap();
        assert!(resp.result.is_some());
        assert!(resp.error.is_none());
    }

    #[tokio::test]
    async fn process_initialized_notification_has_no_response() {
        let config = McpServerConfig::default();
        let sessions = SessionRegistry::new();
        let store = InMemoryGraph::default();

        let msg = r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#;
        let resp = process_message(msg, &store, &config, &sessions).await;
        assert!(resp.is_none());
    }

    #[test]
    fn parse_content_length_header() {
        assert_eq!(parse_content_length("Content-Length: 123\r\n"), Some(123));
        assert_eq!(parse_content_length("content-length: 7\n"), Some(7));
        assert_eq!(parse_content_length("X-Test: 1"), None);
        assert_eq!(parse_content_length("Content-Length: nope"), None);
    }

    #[test]
    fn root_uri_to_path_handles_file_uris_and_bare_paths() {
        assert_eq!(
            root_uri_to_path("file:///Users/me/proj"),
            Some(PathBuf::from("/Users/me/proj"))
        );
        // `file://host/path` drops the authority, leaving the absolute path.
        assert_eq!(
            root_uri_to_path("file://localhost/Users/me/proj"),
            Some(PathBuf::from("/Users/me/proj"))
        );
        // Percent-escaped characters (e.g. spaces) decode.
        assert_eq!(
            root_uri_to_path("file:///Users/me/My%20Repo"),
            Some(PathBuf::from("/Users/me/My Repo"))
        );
        // RFC 8089 Windows drive URIs drop the URI-only leading slash.
        assert_eq!(
            root_uri_to_path("file:///C:/Users/me/My%20Repo"),
            Some(PathBuf::from("C:/Users/me/My Repo"))
        );
        assert_eq!(
            root_uri_to_path("file://localhost/C:/Users/me/kin"),
            Some(PathBuf::from("C:/Users/me/kin"))
        );
        // Accept the non-canonical spelling emitted by some Windows clients.
        assert_eq!(
            root_uri_to_path("file://C:/Users/me/kin"),
            Some(PathBuf::from("C:/Users/me/kin"))
        );
        // Some clients (Cursor) send a bare absolute path, not a file:// URI.
        assert_eq!(
            root_uri_to_path("/Users/me/kin"),
            Some(PathBuf::from("/Users/me/kin"))
        );
        assert_eq!(
            root_uri_to_path(r"C:\Users\me\kin"),
            Some(PathBuf::from(r"C:\Users\me\kin"))
        );
        assert_eq!(
            root_uri_to_path("C:/Users/me/kin"),
            Some(PathBuf::from("C:/Users/me/kin"))
        );
        assert_eq!(root_uri_to_path("C:relative\\kin"), None);
        #[cfg(windows)]
        assert_eq!(
            root_uri_to_path("file://server/share/kin"),
            Some(PathBuf::from(r"\\server\share\kin"))
        );
        #[cfg(not(windows))]
        assert_eq!(root_uri_to_path("file://server/share/kin"), None);
        // Non-file schemes (e.g. remote workspaces) are skipped.
        assert_eq!(root_uri_to_path("vscode-remote://host/x"), None);
        assert_eq!(root_uri_to_path("https://example.com/x"), None);
    }

    #[test]
    fn parse_workspace_roots_extracts_local_paths() {
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": ROOTS_REQUEST_ID,
            "result": {
                "roots": [
                    {"uri": "file:///Users/me/kin", "name": "kin"},
                    {"uri": "vscode-remote://host/y"},
                    {"uri": "/Users/me/bare", "name": "bare"},
                    {"uri": "file:///Users/me/other"}
                ]
            }
        });
        assert_eq!(
            parse_workspace_roots(&response),
            vec![
                PathBuf::from("/Users/me/kin"),
                PathBuf::from("/Users/me/bare"),
                PathBuf::from("/Users/me/other"),
            ]
        );
        // A response carrying no roots yields an empty list, never a panic.
        assert!(parse_workspace_roots(&serde_json::json!({ "result": {} })).is_empty());
    }

    #[test]
    fn workspace_roots_retry_after_empty_response_or_root_change() {
        for method in [
            "initialized",
            "notifications/initialized",
            "notifications/roots/list_changed",
            "tools/list",
        ] {
            assert!(should_request_workspace_roots(
                Some(method),
                true,
                false,
                true,
                true,
            ));
        }
    }

    #[test]
    fn workspace_roots_request_stays_serialized() {
        assert!(!should_request_workspace_roots(
            Some("notifications/roots/list_changed"),
            true,
            true,
            true,
            true,
        ));
        assert!(!should_request_workspace_roots(
            Some("tools/list"),
            true,
            true,
            true,
            true,
        ));
    }

    #[test]
    fn workspace_roots_request_state_reopens_after_completion() {
        let now = Instant::now();
        let mut state = WorkspaceRootsRequestState::default();
        assert!(state.begin_if_allowed(Some("initialized"), true, true, true, now));
        assert!(!state.begin_if_allowed(
            Some("notifications/roots/list_changed"),
            true,
            true,
            true,
            now,
        ));

        state.complete();
        assert!(state.begin_if_allowed(
            Some("notifications/roots/list_changed"),
            true,
            true,
            true,
            now,
        ));
    }

    #[test]
    fn workspace_roots_request_retries_after_an_unanswered_request_times_out() {
        let start = Instant::now();
        let mut state = WorkspaceRootsRequestState::default();
        assert!(state.begin_if_allowed(Some("initialized"), true, true, true, start));

        // A client that advertises roots and never answers must not wedge
        // binding for the life of the process.
        let just_before = start + ROOTS_REQUEST_TIMEOUT - Duration::from_millis(1);
        assert!(!state.begin_if_allowed(Some("tools/list"), true, true, true, just_before));
        let after = start + ROOTS_REQUEST_TIMEOUT;
        assert!(state.begin_if_allowed(Some("tools/list"), true, true, true, after));
    }

    #[test]
    fn workspace_roots_request_requires_capability_binder_and_unbound_daemon() {
        let trigger = Some("tools/list");
        assert!(!should_request_workspace_roots(
            trigger, false, false, true, true,
        ));
        assert!(!should_request_workspace_roots(
            trigger, true, false, false, true,
        ));
        assert!(!should_request_workspace_roots(
            trigger, true, false, true, false,
        ));
        assert!(!should_request_workspace_roots(
            Some("tools/call"),
            true,
            false,
            true,
            true,
        ));
    }

    #[test]
    fn a_refusing_binding_still_wants_workspace_roots() {
        let mut binding = RepoBindingState::default();
        assert!(
            binding.wants_workspace_roots(),
            "unbound wants a repository"
        );

        binding.bind(
            bound_repo("/repo/a", "http://127.0.0.1:4111"),
            BindingOrigin::ClientRoots,
        );
        assert!(
            !binding.wants_workspace_roots(),
            "a settled binding must not re-ask on every trigger"
        );

        binding.mark_mismatch(vec![PathBuf::from("/elsewhere")], false);
        assert!(
            binding.wants_workspace_roots(),
            "bound to a repository the client left is no better than unbound"
        );

        binding.bind(
            bound_repo("/repo/b", "http://127.0.0.1:4222"),
            BindingOrigin::ClientRoots,
        );
        assert!(!binding.wants_workspace_roots());
        assert!(!binding.is_mismatched(), "binding again clears the refusal");
    }

    #[test]
    fn workspace_roots_change_is_honored_after_a_repository_is_already_bound() {
        // The defect this locks down: with a daemon already bound, the roots
        // change that an editor sends when its window moves to another folder
        // was dropped, leaving the server answering from the old repository.
        assert!(should_request_workspace_roots(
            Some("notifications/roots/list_changed"),
            true,
            false,
            true,
            false,
        ));
        // Everything else stays gated on an unbound daemon so a settled session
        // does not re-ask on every tools/list.
        assert!(!should_request_workspace_roots(
            Some("notifications/initialized"),
            true,
            false,
            true,
            false,
        ));
        // A roots change still needs a binder and the client capability, and
        // still never overlaps an in-flight request.
        assert!(!should_request_workspace_roots(
            Some("notifications/roots/list_changed"),
            true,
            true,
            true,
            false,
        ));
        assert!(!should_request_workspace_roots(
            Some("notifications/roots/list_changed"),
            true,
            false,
            false,
            false,
        ));
        assert!(!should_request_workspace_roots(
            Some("notifications/roots/list_changed"),
            false,
            false,
            true,
            false,
        ));
    }

    // ── Workspace-switch integration over the real stdio loop ──────────────

    /// Records every roots list the server hands the binder and replies with a
    /// scripted outcome per call, so a test can drive a bind and then a switch.
    struct ScriptedBinder {
        outcomes: std::sync::Mutex<std::collections::VecDeque<WorkspaceBinding>>,
        calls: std::sync::Arc<std::sync::Mutex<Vec<Vec<PathBuf>>>>,
    }

    impl ScriptedBinder {
        fn install(
            outcomes: Vec<WorkspaceBinding>,
        ) -> (
            RepoBinder,
            std::sync::Arc<std::sync::Mutex<Vec<Vec<PathBuf>>>>,
        ) {
            let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let scripted = std::sync::Arc::new(ScriptedBinder {
                outcomes: std::sync::Mutex::new(outcomes.into()),
                calls: std::sync::Arc::clone(&calls),
            });
            let binder: RepoBinder = Box::new(
                move |roots: Vec<PathBuf>| -> Pin<Box<dyn Future<Output = WorkspaceBinding> + Send>> {
                    let scripted = std::sync::Arc::clone(&scripted);
                    Box::pin(async move {
                        scripted.calls.lock().unwrap().push(roots);
                        scripted
                            .outcomes
                            .lock()
                            .unwrap()
                            .pop_front()
                            // A binder called more times than the script covers
                            // resolves nothing rather than binding something the
                            // test never named.
                            .unwrap_or(WorkspaceBinding::Unresolvable)
                    })
                },
            );
            (binder, calls)
        }
    }

    /// The binder outcome for a client that moved to a Kin repository this
    /// server does not serve.
    fn other_repository(root: &str) -> WorkspaceBinding {
        WorkspaceBinding::OtherRepository(vec![PathBuf::from(root)])
    }

    /// The binder outcome for a root this server cannot resolve at all: the
    /// container and remote shape, where the client's path does not exist in
    /// this process's namespace.
    fn unresolvable() -> WorkspaceBinding {
        WorkspaceBinding::Unresolvable
    }

    fn bound_repo(root: &str, daemon_url: &str) -> BoundRepo {
        BoundRepo {
            root: PathBuf::from(root),
            daemon_url: daemon_url.to_string(),
        }
    }

    /// Drive the daemon stdio loop over an in-memory transport with a scripted
    /// client session, and return every message the server wrote back.
    ///
    /// The config carries an empty tool allow-list so a `tools/call` that is
    /// *not* refused for a repo mismatch is answered locally by the allow-list
    /// branch. That keeps every assertion offline and deterministic: no test
    /// here ever reaches the daemon delegate, which on a machine with a live
    /// `KIN_DAEMON_URL` would open a connection and could spawn a daemon.
    /// FIR-3031, graded through the handler a client actually reaches rather
    /// than through the helper it calls.
    ///
    /// The helper has its own tests in `tools`. This one exists because a
    /// correct helper nobody calls is the same observable surface as no fix at
    /// all: deleting the call in `handle_tools_list` leaves every helper test
    /// green.
    #[test]
    fn tools_list_says_where_a_withheld_tool_is() {
        let config = McpServerConfig {
            allowed_tools: Some(
                crate::agent_default_tool_names()
                    .iter()
                    .map(|name| (*name).to_string())
                    .collect(),
            ),
            ..McpServerConfig::default()
        };
        let response = handle_tools_list(Some(serde_json::json!(1)), &config);
        let value = serde_json::to_value(&response).unwrap();
        let tools = value["result"]["tools"].as_array().expect("a tools array");
        assert_eq!(tools.len(), crate::agent_default_tool_names().len());

        let find_references = tools
            .iter()
            .find(|tool| tool["name"] == crate::verbs::REFS)
            .expect("refs is served by the default profile");
        let description = find_references["description"].as_str().unwrap();
        assert!(
            description.contains("bulk_check_references"),
            "the batch advice survives: {description}"
        );
        assert!(
            description.contains("not served by this tool profile"),
            "and the served answer says the default profile withholds it: {description}"
        );

        // The control: the same handler with no profile filter must annotate
        // nothing, because nothing is withheld.
        let unfiltered = handle_tools_list(Some(serde_json::json!(2)), &McpServerConfig::default());
        let unfiltered = serde_json::to_value(&unfiltered).unwrap();
        for tool in unfiltered["result"]["tools"].as_array().unwrap() {
            assert!(
                !tool["description"]
                    .as_str()
                    .unwrap()
                    .contains("not served by this tool profile"),
                "an unfiltered surface withholds nothing: {}",
                tool["name"]
            );
        }
    }

    /// FIR-3099. The whole handshake, and a workspace-roots answer beside it,
    /// must leave the launcher's binding task still waiting: none of them is a
    /// caller asking Kin for a graph answer, and starting a daemon for one costs
    /// the store open and a full embedding pass.
    ///
    /// The roots answer is in the script on purpose. It is the second spawn
    /// door and the only one that fires when the server was launched outside a
    /// repository, so a fix that closed the startup binding alone would pass a
    /// version of this test that omitted it.
    #[tokio::test]
    async fn a_handshake_and_a_roots_answer_admit_no_daemon_spawn() {
        let startup = StartupDaemonBinding::new();
        let seen_roots = std::sync::Arc::new(std::sync::Mutex::new(Vec::<PathBuf>::new()));
        let recorder = std::sync::Arc::clone(&seen_roots);
        let binder: RepoBinder = Box::new(move |roots: Vec<PathBuf>| {
            let recorder = std::sync::Arc::clone(&recorder);
            Box::pin(async move {
                recorder.lock().unwrap().extend(roots);
                // What an attach-only bind reports for a repository this server
                // can see with no daemon serving it yet.
                WorkspaceBinding::OtherRepository(vec![PathBuf::from("/work/repo")])
            })
        });

        drive_daemon_loop_with_startup(
            &[
                initialize_with_roots_capability(),
                serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
                serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
                roots_response(&["/work/repo"]),
            ],
            Some(binder),
            None,
            Some(std::sync::Arc::clone(&startup)),
        )
        .await;

        assert!(
            !startup.daemon_spawn_admitted(),
            "initialize, initialized, tools/list and a roots answer must not admit starting a \
             daemon: every one of them arrives before a caller has asked Kin anything"
        );
        assert!(
            !seen_roots.lock().unwrap().is_empty(),
            "the binder must still have been consulted, or this test would pass on a server \
             that ignores workspace roots entirely"
        );
    }

    /// The other direction, which is what keeps the test above from passing on
    /// a server that can never start a daemon at all.
    #[tokio::test]
    async fn the_first_tool_call_admits_the_daemon_spawn() {
        let startup = StartupDaemonBinding::new();
        startup.resolve_unbound("no repository at the launch directory");

        drive_daemon_loop_with_startup(
            &[
                initialize_with_roots_capability(),
                tool_call(3, "semantic_locate"),
            ],
            None,
            None,
            Some(std::sync::Arc::clone(&startup)),
        )
        .await;

        assert!(
            startup.daemon_spawn_admitted(),
            "a tools/call is a caller asking for a graph answer, and must admit the spawn the \
             answer needs"
        );
    }

    /// A client that initializes is given its session in the background once a
    /// daemon is serving, so an agent that only reads is shown. The handshake
    /// waits on none of it, nothing admits starting a daemon, and the session
    /// names no process, so a connection that never does anything is left to
    /// the daemon's ordinary idle rule.
    #[tokio::test]
    async fn initialize_opens_the_connections_session_once_a_daemon_serves() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        if launcher_names_session() {
            return;
        }
        let startup = StartupDaemonBinding::new();
        let config = McpServerConfig {
            allowed_tools: Some(HashSet::new()),
            client_root: Some(PathBuf::from("/semantic-test-context")),
            ..McpServerConfig::default()
        };
        config.connection_session.open_with(&["read-session"]);
        let session = std::sync::Arc::clone(&config.connection_session);

        let (client, server) = tokio::io::duplex(64 * 1024);
        let (server_read, mut server_write) = tokio::io::split(server);
        let (client_read, mut client_write) = tokio::io::split(client);
        let served = tokio::spawn({
            let startup = std::sync::Arc::clone(&startup);
            async move {
                let mut reader = BufReader::new(server_read);
                run_stdio_daemon_over(
                    &mut reader,
                    &mut server_write,
                    config,
                    None,
                    None,
                    Some(startup),
                )
                .await
            }
        });
        let mut replies = BufReader::new(client_read).lines();
        let initialize = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"clientInfo": {"name": "claude-code", "version": "2"}},
        });
        client_write
            .write_all(format!("{initialize}\n").as_bytes())
            .await
            .unwrap();
        let answered = tokio::time::timeout(Duration::from_secs(5), replies.next_line())
            .await
            .expect("initialize is answered at once")
            .unwrap()
            .unwrap();
        assert!(answered.contains("\"id\":1"), "{answered}");

        // No daemon is serving yet. Nothing was opened, and nothing admitted
        // starting one.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(session.test_opens(), 0);
        assert!(!startup.daemon_spawn_admitted());

        // A daemon somebody else started comes up.
        startup.resolve_bound(
            BoundRepo {
                root: PathBuf::from("/semantic-test-context"),
                daemon_url: "http://127.0.0.1:9".to_string(),
            },
            false,
        );
        let opened = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(id) = session.current().await {
                    return id;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the session opens once a daemon serves");
        assert_eq!(opened, "read-session");
        assert_eq!(session.test_opens(), 1);
        assert!(
            !session.names_process(),
            "opened for reads, the session names no process"
        );
        assert!(!startup.daemon_spawn_admitted());

        // Closing the client's half of the transport is the client leaving.
        client_write.shutdown().await.unwrap();
        drop(replies);
        tokio::time::timeout(Duration::from_secs(30), served)
            .await
            .expect("the loop ends with its client")
            .unwrap()
            .unwrap();
    }

    /// A call the daemon answers opens the session when initialize could not,
    /// and every later call reuses it.
    #[tokio::test]
    async fn a_forwarded_call_opens_the_session_initialize_could_not() {
        if launcher_names_session() {
            return;
        }
        let config = McpServerConfig {
            client_root: Some(PathBuf::from("/semantic-test-context")),
            ..McpServerConfig::default()
        };
        assert_eq!(
            read_session(&config).await,
            None,
            "a test that set no ids reaches no daemon"
        );
        config.connection_session.open_with(&["lazy-session"]);
        assert_eq!(read_session(&config).await.as_deref(), Some("lazy-session"));
        assert_eq!(read_session(&config).await.as_deref(), Some("lazy-session"));
        assert_eq!(config.connection_session.test_opens(), 1);
    }

    /// Roots a pre-call bind could not follow are kept, not discarded, so the
    /// first tool call binds the repository the client named instead of
    /// reporting no daemon for a repository Kin can see.
    #[tokio::test]
    async fn deferred_roots_are_rebound_on_the_first_tool_call() {
        let startup = StartupDaemonBinding::new();
        startup.resolve_unbound("no repository at the launch directory");
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = std::sync::Arc::clone(&calls);
        let binder: RepoBinder = Box::new(move |_roots: Vec<PathBuf>| {
            let counter = std::sync::Arc::clone(&counter);
            Box::pin(async move {
                // First pass is the attach-only bind, which found no daemon.
                // Second pass runs with the spawn admitted and binds.
                if counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                    WorkspaceBinding::OtherRepository(vec![PathBuf::from("/work/repo")])
                } else {
                    WorkspaceBinding::Bound(bound_repo("/work/repo", "http://127.0.0.1:4311"))
                }
            })
        });

        drive_daemon_loop_with_startup(
            &[
                initialize_with_roots_capability(),
                roots_response(&["/work/repo"]),
                tool_call(3, "semantic_locate"),
            ],
            Some(binder),
            None,
            Some(std::sync::Arc::clone(&startup)),
        )
        .await;

        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "the tool call must put the deferred roots through the binder again, now that \
             starting their daemon is admitted"
        );
    }

    async fn drive_daemon_loop(
        client_messages: &[serde_json::Value],
        repo_binder: Option<RepoBinder>,
        bound_daemon_url: Option<String>,
    ) -> Vec<serde_json::Value> {
        drive_daemon_loop_with_startup(client_messages, repo_binder, bound_daemon_url, None).await
    }

    async fn drive_daemon_loop_with_startup(
        client_messages: &[serde_json::Value],
        repo_binder: Option<RepoBinder>,
        bound_daemon_url: Option<String>,
        startup: Option<std::sync::Arc<StartupDaemonBinding>>,
    ) -> Vec<serde_json::Value> {
        let mut input = String::new();
        for message in client_messages {
            input.push_str(&message.to_string());
            input.push('\n');
        }
        let mut reader = BufReader::new(input.as_bytes());
        let mut written: Vec<u8> = Vec::new();
        let config = McpServerConfig {
            allowed_tools: Some(HashSet::new()),
            ..McpServerConfig::default()
        };

        run_stdio_daemon_over(
            &mut reader,
            &mut written,
            config,
            repo_binder,
            bound_daemon_url,
            startup,
        )
        .await
        .expect("stdio loop must drain the scripted client session");

        String::from_utf8(written)
            .expect("server output is UTF-8")
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).expect("server output is JSON-RPC"))
            .collect()
    }

    fn roots_response(roots: &[&str]) -> serde_json::Value {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": ROOTS_REQUEST_ID,
            "result": {
                "roots": roots
                    .iter()
                    .map(|root| serde_json::json!({ "uri": format!("file://{root}") }))
                    .collect::<Vec<_>>(),
            },
        })
    }

    fn initialize_with_roots_capability() -> serde_json::Value {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "capabilities": { "roots": { "listChanged": true } } },
        })
    }

    fn tool_call(id: u32, name: &str) -> serde_json::Value {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": { "name": name, "arguments": {} },
        })
    }

    fn roots_requests(responses: &[serde_json::Value]) -> Vec<&serde_json::Value> {
        responses
            .iter()
            .filter(|value| value.get("method").and_then(|m| m.as_str()) == Some("roots/list"))
            .collect()
    }

    /// The `_kin.degraded` object an answered tool call carries. Reads it out of
    /// the annotated content block rather than the transport frame, which is
    /// where an agent reads it.
    fn tool_error_degraded(response: &serde_json::Value) -> serde_json::Value {
        let text = tool_error_text(response);
        let payload: serde_json::Value =
            serde_json::from_str(&text).expect("an annotated tool result is JSON");
        payload
            .get(ENVELOPE_KEY)
            .and_then(|envelope| envelope.get("degraded"))
            .cloned()
            .unwrap_or_else(|| panic!("the response carries no _kin degraded object: {text}"))
    }

    fn tool_error_text(response: &serde_json::Value) -> String {
        let result: ToolCallResult =
            serde_json::from_value(response.get("result").cloned().expect("tool result"))
                .expect("tool result payload");
        assert_eq!(result.is_error, Some(true), "expected an error result");
        match result.content.first().expect("error content block") {
            ContentBlock::Text { text } => text.clone(),
        }
    }

    #[tokio::test]
    async fn roots_change_after_a_bind_rebinds_to_the_new_workspace() {
        let (binder, calls) = ScriptedBinder::install(vec![
            WorkspaceBinding::Bound(bound_repo("/repo/a", "http://127.0.0.1:4111")),
            WorkspaceBinding::Bound(bound_repo("/repo/b", "http://127.0.0.1:4222")),
        ]);

        let responses = drive_daemon_loop(
            &[
                initialize_with_roots_capability(),
                serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
                roots_response(&["/repo/a"]),
                serde_json::json!({"jsonrpc": "2.0", "method": "notifications/roots/list_changed"}),
                roots_response(&["/repo/b"]),
            ],
            Some(binder),
            None,
        )
        .await;

        // Two `roots/list` requests: the late bind, then the workspace switch.
        // On the pre-fix server the second never leaves the process.
        assert_eq!(
            roots_requests(&responses).len(),
            2,
            "a roots change after a successful bind must re-request roots: {responses:#?}"
        );
        let calls = calls.lock().unwrap().clone();
        assert_eq!(
            calls,
            vec![
                vec![PathBuf::from("/repo/a")],
                vec![PathBuf::from("/repo/b")]
            ],
            "the binder must be re-invoked with the client's new roots"
        );
    }

    #[tokio::test]
    async fn tool_calls_fail_loud_when_the_new_workspace_cannot_be_bound() {
        // Bind repo A from the client's own roots, then switch to a workspace
        // with no bindable Kin repo. A binding that exists only because the
        // client announced it does not survive the client withdrawing it, so
        // this server refuses rather than answering from the repository it was
        // sent to and then sent away from. The third outcome is the tool call's
        // own re-check, which must find the root still unbindable and leave the
        // refusal exactly as it was: re-checking changes when the verdict is
        // taken, never what it is.
        let (binder, calls) = ScriptedBinder::install(vec![
            WorkspaceBinding::Bound(bound_repo("/repo/a", "http://127.0.0.1:4111")),
            unresolvable(),
            unresolvable(),
        ]);

        let responses = drive_daemon_loop(
            &[
                initialize_with_roots_capability(),
                serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
                roots_response(&["/repo/a"]),
                serde_json::json!({"jsonrpc": "2.0", "method": "notifications/roots/list_changed"}),
                roots_response(&["/elsewhere/not-a-kin-repo"]),
                tool_call(7, "semantic_locate"),
            ],
            Some(binder),
            None,
        )
        .await;

        let answer = responses
            .iter()
            .find(|value| value.get("id").and_then(|id| id.as_u64()) == Some(7))
            .expect("the tool call must be answered, not dropped");
        let text = tool_error_text(answer);
        assert!(
            text.contains("semantic_locate")
                && text.contains("requested repository context changed"),
            "refusal must name the tool and the semantic scope mismatch: {text}"
        );
        assert!(
            !text.contains("/repo/a") && !text.contains("/elsewhere/not-a-kin-repo"),
            "refusal must not expose private coordinates: {text}"
        );
        assert_eq!(
            calls.lock().unwrap().len(),
            3,
            "the tool call must re-check the root before refusing"
        );
    }

    /// The refusal was computed when the roots changed and then cached, so a
    /// root that became bindable afterwards kept being refused for the life of
    /// the process. The reported shape: a container registration whose
    /// announced host path was made to exist, with no way to restart the
    /// client's MCP server.
    #[tokio::test]
    async fn a_root_that_becomes_bindable_self_heals_on_the_next_tool_call() {
        let (binder, calls) = ScriptedBinder::install(vec![
            WorkspaceBinding::Bound(bound_repo("/repo/a", "http://127.0.0.1:4111")),
            unresolvable(),
            WorkspaceBinding::Bound(bound_repo("/host/path", "http://127.0.0.1:4222")),
        ]);

        let responses = drive_daemon_loop(
            &[
                initialize_with_roots_capability(),
                serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
                roots_response(&["/repo/a"]),
                serde_json::json!({"jsonrpc": "2.0", "method": "notifications/roots/list_changed"}),
                roots_response(&["/host/path"]),
                // No roots change and no restart in between: only the host
                // changed under a root the client already announced.
                tool_call(21, "semantic_locate"),
            ],
            Some(binder),
            None,
        )
        .await;

        let calls = calls.lock().unwrap().clone();
        assert_eq!(
            calls.len(),
            3,
            "the tool call must put the recorded roots through the binder again: {calls:?}"
        );
        assert_eq!(
            calls[2],
            vec![PathBuf::from("/host/path")],
            "the re-check must use the roots the client announced, not invent new ones"
        );

        let answer = responses
            .iter()
            .find(|value| value.get("id").and_then(|id| id.as_u64()) == Some(21))
            .expect("the tool call must be answered");
        let text = tool_error_text(answer);
        assert!(
            text.contains("not enabled in this MCP profile"),
            "a root that became bindable must clear the refusal without a restart: {text}"
        );
    }

    /// Drive the loop with a repository bound at startup, then move the client
    /// to a second Kin repository this server does not serve, and return the
    /// refusal text.
    async fn pinned_workspace_refusal(repo_pinned: bool) -> String {
        let startup = StartupDaemonBinding::new();
        startup.resolve_bound(
            bound_repo("/repo/pinned", "http://127.0.0.1:4111"),
            repo_pinned,
        );
        // Twice: the roots change that records the mismatch, and the tool call's
        // own re-check.
        let (binder, _calls) = ScriptedBinder::install(vec![
            other_repository("/repo/second"),
            other_repository("/repo/second"),
        ]);

        let responses = drive_daemon_loop_with_startup(
            &[
                initialize_with_roots_capability(),
                serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
                roots_response(&["/repo/second"]),
                tool_call(31, "semantic_locate"),
            ],
            Some(binder),
            None,
            Some(startup),
        )
        .await;

        let answer = responses
            .iter()
            .find(|value| value.get("id").and_then(|id| id.as_u64()) == Some(31))
            .expect("the tool call must be answered");
        tool_error_text(answer)
    }

    /// `kin init .` is wrong advice for a repo-bound server, and wrong in the
    /// expensive direction: the repository the client moved to already exists,
    /// so following the hint writes a second store into it and still leaves the
    /// pinned server refusing.
    #[tokio::test]
    async fn a_repo_bound_refusal_does_not_suggest_kin_init() {
        let pinned = pinned_workspace_refusal(true).await;
        assert!(
            !pinned.contains("kin init"),
            "a repo-bound server must not send the user to create a second repository: {pinned}"
        );
        assert!(
            pinned.contains("pinned to a repository context") && pinned.contains("Reconnect"),
            "the applicable semantic remedy must survive: {pinned}"
        );
        assert!(
            pinned.contains("semantic_locate") && !pinned.contains("/repo/"),
            "the refusal must name the tool without private coordinates: {pinned}"
        );

        // An unpinned connection reports the same scope failure without
        // claiming the connection is pinned or suggesting physical setup.
        let unpinned = pinned_workspace_refusal(false).await;
        assert!(
            !unpinned.contains("pinned to a repository context") && unpinned.contains("Reconnect"),
            "the unpinned remedy must describe the actual connection: {unpinned}"
        );
    }

    /// Drive a server that bound its own repository before the loop started —
    /// the `docker exec -w <repo> ... kin mcp start` and plain per-repo
    /// registration shape — through a roots answer the binder reports as
    /// `resolution`, then two tool calls, and return their answers.
    async fn server_bound_run(
        resolution: WorkspaceBinding,
        root: &str,
    ) -> (serde_json::Value, serde_json::Value) {
        let startup = StartupDaemonBinding::new();
        // `pinned_by_operator: false` is the reported registration: the cwd of
        // the `docker exec` bound the repository, with no --repo/KIN_MCP_REPO.
        startup.resolve_bound(bound_repo("/work/express", "http://127.0.0.1:4111"), false);
        // Three: the roots answer, and each tool call's own re-check if the
        // first one recorded a mismatch.
        let (binder, _calls) =
            ScriptedBinder::install(vec![resolution.clone(), resolution.clone(), resolution]);

        let responses = drive_daemon_loop_with_startup(
            &[
                initialize_with_roots_capability(),
                serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
                roots_response(&[root]),
                tool_call(41, "semantic_locate"),
                tool_call(42, "semantic_locate"),
            ],
            Some(binder),
            None,
            Some(startup),
        )
        .await;

        let answer_for = |id: u64| {
            responses
                .iter()
                .find(|value| value.get("id").and_then(|value| value.as_u64()) == Some(id))
                .unwrap_or_else(|| panic!("tool call {id} must be answered"))
                .clone()
        };
        (answer_for(41), answer_for(42))
    }

    /// The reported shape (FIR-2405): a server reached through `docker exec`
    /// serves `/work/express` while the client announces the host path
    /// `/private/tmp/.../work`, which never exists inside the container. That
    /// root is not evidence that the client left the repository this server was
    /// started for, so the server must keep answering. Before the fix exactly
    /// one call survived each respawn: the one that arrived before the roots
    /// answer did, after which every call was refused.
    #[tokio::test]
    async fn a_server_bound_repository_survives_roots_it_cannot_resolve() {
        let (first, second) =
            server_bound_run(unresolvable(), "/private/tmp/kin-dogfood/brown/work").await;

        for (id, answer) in [(41, &first), (42, &second)] {
            let text = tool_error_text(answer);
            assert!(
                !text.contains("kin-mcp refuses"),
                "call {id} must not be refused for a root this server cannot resolve: {text}"
            );
            assert!(
                text.contains("not enabled in this MCP profile"),
                "call {id} must reach ordinary handling: {text}"
            );
        }
    }

    /// Falsification of the test above, over the same helper: when the client's
    /// roots name a Kin repository this server can see and does not serve, the
    /// refusal is still owed. Answering would return a confident result about
    /// the wrong codebase, which is the failure the refusal exists to prevent.
    #[tokio::test]
    async fn a_server_bound_repository_still_refuses_a_second_kin_repository() {
        let (first, second) =
            server_bound_run(other_repository("/work/requests"), "/work/requests").await;

        for (id, answer) in [(41, &first), (42, &second)] {
            let text = tool_error_text(answer);
            assert!(
                text.contains("kin-mcp refuses") && text.contains("semantic_locate"),
                "call {id} must be refused and name the tool: {text}"
            );
            assert!(
                text.contains("wrong semantic scope") && !text.contains("/work/"),
                "call {id} must explain the scope mismatch without coordinates: {text}"
            );
        }
    }

    /// The refusal reported a reachability problem it did not have: the bound
    /// daemon answered `/health` 200 throughout, so an agent reading
    /// `daemon_unreachable` went and checked a daemon that was fine.
    #[tokio::test]
    async fn a_workspace_refusal_is_not_stamped_daemon_unreachable() {
        let (refused, _) =
            server_bound_run(other_repository("/work/requests"), "/work/requests").await;
        let degraded = tool_error_degraded(&refused);

        assert_eq!(
            degraded.get("workspace_mismatch"),
            Some(&serde_json::Value::Bool(true)),
            "the refusal must carry its own degradation: {degraded}"
        );
        assert!(
            degraded.get("daemon_unreachable").is_none(),
            "a reachable daemon must not be reported unreachable: {degraded}"
        );

        // Falsification: the same assertion run against a server that keeps
        // serving proves the flag tracks the refusal rather than riding every
        // response out of this loop.
        let (served, _) = server_bound_run(unresolvable(), "/host/only/path").await;
        assert!(
            tool_error_degraded(&served)
                .get("workspace_mismatch")
                .is_none(),
            "a served call must carry no workspace mismatch"
        );
    }

    #[tokio::test]
    async fn a_bindable_workspace_switch_clears_an_earlier_refusal() {
        // A → unbindable workspace → B: the refusal must not outlive the switch
        // that resolves it.
        let (binder, _calls) = ScriptedBinder::install(vec![
            WorkspaceBinding::Bound(bound_repo("/repo/a", "http://127.0.0.1:4111")),
            unresolvable(),
            WorkspaceBinding::Bound(bound_repo("/repo/b", "http://127.0.0.1:4222")),
        ]);

        let responses = drive_daemon_loop(
            &[
                initialize_with_roots_capability(),
                serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
                roots_response(&["/repo/a"]),
                serde_json::json!({"jsonrpc": "2.0", "method": "notifications/roots/list_changed"}),
                roots_response(&["/elsewhere/not-a-kin-repo"]),
                serde_json::json!({"jsonrpc": "2.0", "method": "notifications/roots/list_changed"}),
                roots_response(&["/repo/b"]),
                tool_call(9, "semantic_locate"),
            ],
            Some(binder),
            None,
        )
        .await;

        let answer = responses
            .iter()
            .find(|value| value.get("id").and_then(|id| id.as_u64()) == Some(9))
            .expect("the tool call must be answered");
        let text = tool_error_text(answer);
        assert!(
            text.contains("not enabled in this MCP profile"),
            "a successful re-bind must clear the refusal and let the call through: {text}"
        );
    }

    #[tokio::test]
    async fn a_refusing_server_keeps_trying_to_bind_on_ordinary_triggers() {
        // Bind A, switch to a workspace that cannot be bound, then send only a
        // plain `tools/list`. A refusing server is no more useful than an
        // unbound one, so it must reach for roots again instead of waiting for
        // the client to announce another change — and a bindable answer there
        // clears the refusal.
        let (binder, calls) = ScriptedBinder::install(vec![
            WorkspaceBinding::Bound(bound_repo("/repo/a", "http://127.0.0.1:4111")),
            unresolvable(),
            WorkspaceBinding::Bound(bound_repo("/repo/b", "http://127.0.0.1:4222")),
        ]);

        let responses = drive_daemon_loop(
            &[
                initialize_with_roots_capability(),
                serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
                roots_response(&["/repo/a"]),
                serde_json::json!({"jsonrpc": "2.0", "method": "notifications/roots/list_changed"}),
                roots_response(&["/elsewhere/not-a-kin-repo"]),
                serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
                roots_response(&["/repo/b"]),
                tool_call(13, "semantic_locate"),
            ],
            Some(binder),
            None,
        )
        .await;

        assert_eq!(
            roots_requests(&responses).len(),
            3,
            "a refusing server must re-request roots on an ordinary trigger: {responses:#?}"
        );
        assert_eq!(
            calls.lock().unwrap().len(),
            3,
            "the third roots response must reach the binder"
        );
        let answer = responses
            .iter()
            .find(|value| value.get("id").and_then(|id| id.as_u64()) == Some(13))
            .expect("the tool call must be answered");
        let text = tool_error_text(answer);
        assert!(
            text.contains("not enabled in this MCP profile"),
            "binding again must clear the refusal: {text}"
        );
    }

    #[tokio::test]
    async fn a_startup_bound_server_still_follows_a_workspace_switch() {
        // The reported shape: a repository bound before the loop starts (from
        // --repo/KIN_MCP_REPO/cwd) used to suppress roots handling entirely.
        let (binder, calls) = ScriptedBinder::install(vec![WorkspaceBinding::Bound(bound_repo(
            "/repo/b",
            "http://127.0.0.1:4222",
        ))]);

        let responses = drive_daemon_loop(
            &[
                initialize_with_roots_capability(),
                serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
                serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
                serde_json::json!({"jsonrpc": "2.0", "method": "notifications/roots/list_changed"}),
                roots_response(&["/repo/b"]),
            ],
            Some(binder),
            Some("http://127.0.0.1:4111".to_string()),
        )
        .await;

        // `initialized` and `tools/list` must stay quiet for an already-bound
        // server; only the roots change reaches out.
        let requests = roots_requests(&responses);
        assert_eq!(
            requests.len(),
            1,
            "only the roots change may re-request roots once bound: {responses:#?}"
        );
        assert_eq!(
            calls.lock().unwrap().clone(),
            vec![vec![PathBuf::from("/repo/b")]],
            "the switch must reach the binder even though startup already bound a repo"
        );
    }

    #[tokio::test]
    async fn an_empty_roots_response_leaves_an_existing_binding_alone() {
        // No open folder is not a different folder: there is no other
        // repository the client's calls could be about, so a binding survives
        // and tool calls are not refused.
        let (binder, calls) = ScriptedBinder::install(vec![WorkspaceBinding::Bound(bound_repo(
            "/repo/a",
            "http://127.0.0.1:4111",
        ))]);

        let responses = drive_daemon_loop(
            &[
                initialize_with_roots_capability(),
                serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
                roots_response(&["/repo/a"]),
                serde_json::json!({"jsonrpc": "2.0", "method": "notifications/roots/list_changed"}),
                roots_response(&[]),
                tool_call(11, "semantic_locate"),
            ],
            Some(binder),
            None,
        )
        .await;

        assert_eq!(
            calls.lock().unwrap().len(),
            1,
            "an empty roots list must not be handed to the binder"
        );
        let answer = responses
            .iter()
            .find(|value| value.get("id").and_then(|id| id.as_u64()) == Some(11))
            .expect("the tool call must be answered");
        let text = tool_error_text(answer);
        assert!(
            text.contains("not enabled in this MCP profile"),
            "an empty roots list must not be treated as a workspace switch: {text}"
        );
    }

    /// The answer-early contract (FIR-2316): the handshake never waits on the
    /// daemon. With the startup binding pending forever (the shape of a cold
    /// flagship-scale daemon start), `initialize` and `tools/list` are still
    /// answered from this process.
    #[tokio::test]
    async fn initialize_and_tools_list_answer_while_the_startup_binding_is_pending() {
        let startup = StartupDaemonBinding::new();

        let responses = drive_daemon_loop_with_startup(
            &[
                serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}),
                serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
                serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
            ],
            None,
            None,
            Some(startup),
        )
        .await;

        let initialize = responses
            .iter()
            .find(|value| value.get("id").and_then(|id| id.as_u64()) == Some(1))
            .expect("initialize must be answered while the daemon binding is pending");
        assert!(
            initialize.pointer("/result/serverInfo/name").is_some(),
            "initialize must carry the ordinary result: {initialize:#?}"
        );
        let tools_list = responses
            .iter()
            .find(|value| value.get("id").and_then(|id| id.as_u64()) == Some(2))
            .expect("tools/list must be answered while the daemon binding is pending");
        assert!(
            tools_list.pointer("/result/tools").is_some(),
            "tools/list must carry the ordinary result: {tools_list:#?}"
        );
    }

    /// A `tools/call` racing a binding that never settles gets the honest
    /// still-starting answer once the bounded grace elapses: not silence, and
    /// not the fall-through refusal this config would otherwise produce (the
    /// empty allow-list answers "not enabled in this MCP profile", so reaching
    /// that text would prove the guard never ran).
    #[tokio::test(start_paused = true)]
    async fn a_tool_call_during_a_pending_binding_answers_that_the_daemon_is_starting() {
        let startup = StartupDaemonBinding::new();

        let responses = drive_daemon_loop_with_startup(
            &[
                serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}),
                tool_call(3, "kin_graph_status"),
            ],
            None,
            None,
            Some(startup),
        )
        .await;

        let answer = responses
            .iter()
            .find(|value| value.get("id").and_then(|id| id.as_u64()) == Some(3))
            .expect("a tool call during startup binding must be answered, not dropped");
        let text = tool_error_text(answer);
        assert!(
            text.contains("still starting") && text.contains("kin_graph_status"),
            "the answer must say the daemon is starting and name the tool: {text}"
        );
        assert!(
            text.contains("retry"),
            "the remedy is retrying, not remediation: {text}"
        );
        assert!(
            !text.contains("not enabled in this MCP profile"),
            "the still-starting guard must answer before the fall-through path: {text}"
        );
    }

    /// Asking what tools exist neither waits for the daemon nor starts one.
    ///
    /// Two properties in one session, both of which the registry search would
    /// break if it were treated as an ordinary graph call. It must not be
    /// answered "the daemon is still starting", because no daemon can answer it
    /// better; and it must not admit a daemon spawn, because that is the
    /// FIR-3099 property that keeps a session which asked for no graph answer
    /// from opening the store and scheduling a full embedding pass.
    ///
    /// This config carries an empty allow-list, so a call that reaches ordinary
    /// handling answers "not enabled in this MCP profile". Reaching that text is
    /// what proves the exemption ran rather than the wait, and `kin_graph_status`
    /// beside it is the control that must still be told the daemon is starting.
    #[tokio::test(start_paused = true)]
    async fn the_tool_registry_neither_waits_for_the_daemon_nor_starts_one() {
        let startup = StartupDaemonBinding::new();

        let responses = drive_daemon_loop_with_startup(
            &[
                serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}),
                tool_call(2, crate::handlers::tool_search::TOOL_NAME),
                tool_call(3, "kin_graph_status"),
            ],
            None,
            None,
            Some(std::sync::Arc::clone(&startup)),
        )
        .await;

        let search = responses
            .iter()
            .find(|value| value.get("id").and_then(|id| id.as_u64()) == Some(2))
            .expect("the registry call must be answered, not dropped");
        let search_text = tool_error_text(search);
        assert!(
            !search_text.contains("still starting"),
            "the registry call waited on a daemon it does not read: {search_text}"
        );
        assert!(
            search_text.contains("not enabled in this MCP profile"),
            "the registry call did not reach ordinary handling, so the exemption is untested: \
             {search_text}"
        );

        let status = responses
            .iter()
            .find(|value| value.get("id").and_then(|id| id.as_u64()) == Some(3))
            .expect("the control call must be answered");
        assert!(
            tool_error_text(status).contains("still starting"),
            "the control lost its still-starting answer, so this test would pass with the wait \
             removed for every tool: {}",
            tool_error_text(status)
        );

        // The spawn admission, on its own session, because the control call
        // above admits one by design and would mask this.
        let search_only = StartupDaemonBinding::new();
        drive_daemon_loop_with_startup(
            &[
                serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}),
                tool_call(2, crate::handlers::tool_search::TOOL_NAME),
            ],
            None,
            None,
            Some(std::sync::Arc::clone(&search_only)),
        )
        .await;
        assert!(
            !search_only.daemon_spawn_admitted(),
            "asking what tools exist admitted a daemon spawn, which opens the store and \
             schedules an embedding pass for a session that asked for no graph answer"
        );

        // And the control for that half: a graph call in the same shape does
        // admit one, so the assertion above is about this tool rather than
        // about a path nothing reaches.
        let graph_call = StartupDaemonBinding::new();
        drive_daemon_loop_with_startup(
            &[
                serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}),
                tool_call(2, "kin_graph_status"),
            ],
            None,
            None,
            Some(std::sync::Arc::clone(&graph_call)),
        )
        .await;
        assert!(
            graph_call.daemon_spawn_admitted(),
            "no tool admits a daemon spawn any more, so the exemption above proves nothing"
        );
    }

    /// Once the binding settles without a daemon, tool calls fall through to
    /// the ordinary handling instead of reporting a startup that is over.
    #[tokio::test]
    async fn a_tool_call_after_the_binding_settles_unbound_falls_through() {
        let startup = StartupDaemonBinding::new();
        startup.resolve_unbound("not a Kin repository");

        let responses = drive_daemon_loop_with_startup(
            &[
                serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}),
                tool_call(4, "semantic_locate"),
            ],
            None,
            None,
            Some(startup),
        )
        .await;

        let answer = responses
            .iter()
            .find(|value| value.get("id").and_then(|id| id.as_u64()) == Some(4))
            .expect("the tool call must be answered");
        let text = tool_error_text(answer);
        assert!(
            !text.contains("still starting"),
            "a settled binding must not be reported as still starting: {text}"
        );
        assert!(
            text.contains("not enabled in this MCP profile"),
            "a settled-unbound binding falls through to the ordinary handling: {text}"
        );
    }

    /// A startup binding that settles bound folds into the roots bookkeeping:
    /// the server then behaves as bound, so it does not keep asking a
    /// roots-capable client for a workspace it already serves.
    #[tokio::test]
    async fn a_bound_startup_binding_folds_into_the_roots_bookkeeping() {
        let startup = StartupDaemonBinding::new();
        startup.resolve_bound(bound_repo("/repo/a", "http://127.0.0.1:4111"), false);
        let (binder, calls) = ScriptedBinder::install(vec![]);

        let responses = drive_daemon_loop_with_startup(
            &[
                initialize_with_roots_capability(),
                serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
                serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
            ],
            Some(binder),
            None,
            Some(startup),
        )
        .await;

        assert!(
            roots_requests(&responses).is_empty(),
            "a server whose startup binding bound must not keep requesting workspace roots: \
             {responses:#?}"
        );
        assert!(
            calls.lock().unwrap().is_empty(),
            "the binder must not run while the startup binding already bound"
        );
    }

    /// Every daemon-loss message the delegate mints must go out stamped unreachable.
    ///
    /// The endpoints were each guarded and the join between them was not. The delegate
    /// mints three messages for a daemon that stopped answering; the classifier keyed on
    /// one prefix; and the third, a connection that broke while it was carrying the
    /// request, went out with an empty `degraded` object, which a client reads as a live
    /// daemon. That shape is what an OOM kill mid-answer looks like from here and it is
    /// the first error a caller meets.
    ///
    /// So the cases are BUILT from the delegate's own constructors rather than written out
    /// again as strings. A fourth shape, or a reworded prefix, has to be handled here
    /// rather than merely matched, which is the whole difference between this test and two
    /// correct tests that jointly guard nothing.
    #[test]
    fn every_daemon_loss_message_the_delegate_mints_is_stamped_unreachable() {
        let record = kin_daemon_spawn::DaemonKillRecord {
            kills: 4,
            memory_kills: 4,
            first_unix: 4_320,
            last_unix: 4_800,
            last_pid: Some(41),
            last_cause: kin_daemon_spawn::DaemonKillCause::MemoryLimit {
                kernel_oom_kills: 1,
            },
            limit_bytes: Some(12 * 1024 * 1024 * 1024),
            last_rss_bytes: None,
        };
        for record in [None, Some(&record)] {
            let minted = [
                daemon_delegate::revival_failed_message(
                    "tool semantic_locate",
                    "http://127.0.0.1:42231",
                    "error sending request",
                    "daemon exited during startup with status signal: 9 (SIGKILL)",
                    record,
                ),
                daemon_delegate::revived_retry_failed_message(
                    "tool semantic_locate",
                    "http://127.0.0.1:42232",
                    "timed out",
                    record,
                ),
                daemon_delegate::transport_dropped_message(
                    "MCP tool call",
                    "error sending request for url (http://127.0.0.1:42231/mcp/tools/call)",
                    record,
                ),
            ];
            for message in minted {
                assert_eq!(
                    envelope_for_delegate_error(&message, record)
                        .degraded
                        .daemon_unreachable,
                    Some(true),
                    "a daemon-loss message went out reading as a live daemon: {message}"
                );
            }
        }

        // The control that has to stay silent. A live daemon refusing a call has not
        // become unreachable, and a flag set on everything says nothing.
        let ordinary = envelope_for_delegate_error("no entity named `HTTPAdapter.send`", None);
        assert_eq!(ordinary.degraded.daemon_unreachable, None);
    }

    /// The dead-daemon error is the shape that never set a degraded flag, and a
    /// client cannot tell an empty `degraded` object from a healthy answer
    /// without parsing prose. An ordinary tool error from a live daemon is left
    /// exactly as it was.
    #[test]
    fn a_dead_daemon_error_is_stamped_unreachable_and_an_ordinary_one_is_not() {
        let gone = envelope_for_delegate_error(
            "repo daemon exited; restart required: tool find_references: daemon at \
             http://127.0.0.1:32881 is not responding",
            None,
        );
        assert_eq!(gone.degraded.daemon_unreachable, Some(true));

        let ordinary = envelope_for_delegate_error("no entity named `HTTPAdapter.send`", None);
        assert_eq!(
            ordinary.degraded.daemon_unreachable, None,
            "a live daemon rejecting a call has not become unreachable"
        );
        assert_eq!(
            serde_json::to_value(&ordinary.degraded).unwrap(),
            serde_json::json!({}),
            "an ordinary tool error carries the same empty degraded object it always did"
        );
    }
    /// A binding that settles long after the old fixed grace is waited for:
    /// the call is answered from the settled binding, not told the daemon is
    /// still starting.
    ///
    /// The shape of the defect this replaces: a cold daemon on a large store
    /// took minutes to bind, the first call gave up at 45 s and every later one
    /// at 10 s, each with the same still-starting reply. The binding here
    /// settles unbound so the call falls through to ordinary handling, which on
    /// this config's empty allow-list answers "not enabled in this MCP
    /// profile"; reaching that text proves the call waited for the settle.
    #[tokio::test(start_paused = true)]
    async fn a_call_waits_for_a_binding_that_settles_after_the_old_grace() {
        let startup = StartupDaemonBinding::new();
        let settler = std::sync::Arc::clone(&startup);
        let settle_after = Duration::from_secs(120);
        assert!(settle_after < daemon_delegate::readiness_budget());
        tokio::spawn(async move {
            tokio::time::sleep(settle_after).await;
            settler.resolve_unbound("settled after a cold start");
        });

        let began = tokio::time::Instant::now();
        let responses = drive_daemon_loop_with_startup(
            &[
                serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}),
                tool_call(3, "semantic_locate"),
            ],
            None,
            None,
            Some(startup),
        )
        .await;

        let answer = responses
            .iter()
            .find(|value| value.get("id").and_then(|id| id.as_u64()) == Some(3))
            .expect("the tool call must be answered");
        let text = tool_error_text(answer);
        assert!(
            !text.contains("still starting"),
            "a binding that settles inside the call's budget must be waited for: {text}"
        );
        assert!(
            text.contains("not enabled in this MCP profile"),
            "the call must be answered from the settled binding: {text}"
        );
        assert!(
            began.elapsed() >= settle_after,
            "the answer came before the binding settled, so nothing was waited for"
        );
    }

    /// Every call gets the whole readiness budget, not only the first one.
    ///
    /// The first call here spends its whole budget and says so. The binding
    /// then settles thirty seconds into the second call, three times longer
    /// than a repeat call used to be allowed to wait, and the second call is
    /// answered from it.
    #[tokio::test(start_paused = true)]
    async fn a_later_call_waits_as_long_as_the_first_one_did() {
        let budget = daemon_delegate::readiness_budget();
        let startup = StartupDaemonBinding::new();
        let settler = std::sync::Arc::clone(&startup);
        tokio::spawn(async move {
            tokio::time::sleep(budget + Duration::from_secs(30)).await;
            settler.resolve_unbound("settled during the second call");
        });

        let responses = drive_daemon_loop_with_startup(
            &[
                serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}),
                tool_call(3, "kin_graph_status"),
                tool_call(4, "semantic_locate"),
            ],
            None,
            None,
            Some(startup),
        )
        .await;

        let first = responses
            .iter()
            .find(|value| value.get("id").and_then(|id| id.as_u64()) == Some(3))
            .expect("the first call must be answered");
        let first_text = tool_error_text(first);
        assert!(
            first_text.contains("still starting")
                && first_text.contains(&format!("waited {}s", budget.as_secs())),
            "a call whose budget runs out says so, with the budget it spent: {first_text}"
        );

        let second = responses
            .iter()
            .find(|value| value.get("id").and_then(|id| id.as_u64()) == Some(4))
            .expect("the second call must be answered");
        let second_text = tool_error_text(second);
        assert!(
            !second_text.contains("still starting")
                && second_text.contains("not enabled in this MCP profile"),
            "a repeat call must wait for the daemon too, not give up after a few seconds: \
             {second_text}"
        );
    }

    /// A call whose client sent a progress token hears how far the daemon has
    /// come while it waits, and a call without one hears nothing extra.
    #[tokio::test(start_paused = true)]
    async fn a_waiting_call_reports_progress_to_a_client_that_asked_for_it() {
        let startup = StartupDaemonBinding::new();
        startup.set_phase_probe(Box::new(|| crate::startup_binding::StartupProgress {
            phase: "phase: the daemon process is up and loading the repository graph",
            last_open: Some(Duration::from_secs(400)),
        }));
        let settler = std::sync::Arc::clone(&startup);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(22)).await;
            settler.resolve_unbound("settled");
        });

        let mut asked = tool_call(3, "kin_graph_status");
        asked["params"]["_meta"] = serde_json::json!({"progressToken": "call-3"});
        let responses = drive_daemon_loop_with_startup(
            &[
                serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}),
                asked,
            ],
            None,
            None,
            Some(startup),
        )
        .await;

        let notes: Vec<&serde_json::Value> = responses
            .iter()
            .filter(|value| {
                value.get("method").and_then(|m| m.as_str()) == Some("notifications/progress")
            })
            .collect();
        assert!(
            notes.len() >= 3,
            "a 22 s wait reported every 5 s must send several notifications: {responses:#?}"
        );
        let mut last = None;
        for note in &notes {
            assert_eq!(note["params"]["progressToken"], "call-3");
            assert_eq!(
                note["params"]["total"],
                daemon_delegate::readiness_budget().as_secs()
            );
            let progress = note["params"]["progress"]
                .as_u64()
                .expect("numeric progress");
            assert!(
                last.is_none_or(|last| progress > last),
                "progress must only grow: {notes:#?}"
            );
            last = Some(progress);
            let message = note["params"]["message"].as_str().unwrap_or_default();
            assert!(
                message.contains("loading the repository graph")
                    && message.contains("of the 400s the last open of this store took"),
                "each notification says what is loading and how far along it is: {message}"
            );
        }
        let answer_at = responses
            .iter()
            .position(|value| value.get("id").and_then(|id| id.as_u64()) == Some(3))
            .expect("the call must be answered");
        assert!(
            responses[..answer_at]
                .iter()
                .filter(|value| value.get("method").is_some())
                .all(|value| value["method"] == "notifications/progress"),
            "only progress notifications may precede the answer"
        );
        assert!(
            responses[answer_at..]
                .iter()
                .all(|value| value["method"] != "notifications/progress"),
            "nothing may be reported for a call after it has been answered"
        );

        // The same wait without a token sends nothing but the answer.
        let quiet = StartupDaemonBinding::new();
        let settler = std::sync::Arc::clone(&quiet);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(22)).await;
            settler.resolve_unbound("settled");
        });
        let responses = drive_daemon_loop_with_startup(
            &[
                serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}),
                tool_call(3, "kin_graph_status"),
            ],
            None,
            None,
            Some(quiet),
        )
        .await;
        assert!(
            responses
                .iter()
                .all(|value| value["method"] != "notifications/progress"),
            "a client that asked for no progress must get none: {responses:#?}"
        );
    }

    /// When the budget truly runs out the answer is bounded and honest: it
    /// says what is still loading and how far along it is, names the knob that
    /// widens the wait, and still marks the daemon unreachable, never ready.
    #[tokio::test(start_paused = true)]
    async fn a_call_whose_budget_runs_out_says_what_is_loading_and_how_far_along() {
        let startup = StartupDaemonBinding::new();
        startup.set_phase_probe(Box::new(|| crate::startup_binding::StartupProgress {
            phase: "phase: the daemon process is up and loading the repository graph",
            last_open: Some(Duration::from_secs(900)),
        }));

        let began = tokio::time::Instant::now();
        let responses = drive_daemon_loop_with_startup(
            &[
                serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}),
                tool_call(3, "kin_graph_status"),
            ],
            None,
            None,
            Some(startup),
        )
        .await;
        let budget = daemon_delegate::readiness_budget();
        assert!(
            began.elapsed() >= budget,
            "the call must spend its whole budget before giving up"
        );

        let answer = responses
            .iter()
            .find(|value| value.get("id").and_then(|id| id.as_u64()) == Some(3))
            .expect("the call must be answered");
        let text = tool_error_text(answer);
        for expected in [
            "still starting",
            &format!("waited {}s", budget.as_secs()),
            "whole readiness budget",
            "loading the repository graph",
            "of the 900s the last open of this store took",
            daemon_delegate::DAEMON_PATIENCE_ENV,
            "retry",
        ] {
            assert!(text.contains(expected), "missing {expected:?}: {text}");
        }
        let payload: serde_json::Value =
            serde_json::from_str(&text).expect("the answer carries its envelope");
        assert_eq!(
            payload["_kin"]["degraded"]["daemon_unreachable"], true,
            "a daemon that never became ready must not be reported as one that did: {payload:#?}"
        );
    }

    // ── The routed profiles ─────────────────────────────────────────────────

    /// A routed profile's config, served the way `kin mcp start` serves it.
    fn routed_config(surface: crate::routed::RoutedSurface) -> McpServerConfig {
        McpServerConfig {
            allowed_tools: Some(crate::tools::name_set(
                crate::tools::agent_routed_tool_names(),
            )),
            agent_belt: true,
            routed: Some(surface),
            number_entity_lines: surface.numbered,
            session_authority_mode: SessionAuthorityMode::OfflineFallback,
            ..McpServerConfig::default()
        }
    }

    fn with_writes() -> McpServerConfig {
        routed_config(crate::routed::RoutedSurface::WITH_WRITES)
    }

    fn read_only() -> McpServerConfig {
        routed_config(crate::routed::RoutedSurface::READ_ONLY)
    }

    /// The belt with its write half, which `agent-routed` must answer as.
    fn default_config() -> McpServerConfig {
        McpServerConfig {
            allowed_tools: Some(crate::tools::name_set(
                crate::tools::agent_default_tool_names(),
            )),
            agent_belt: true,
            session_authority_mode: SessionAuthorityMode::OfflineFallback,
            ..McpServerConfig::default()
        }
    }

    /// The query belt, which `agent-routed-query` must answer as.
    fn query_config() -> McpServerConfig {
        McpServerConfig {
            allowed_tools: Some(crate::tools::name_set(
                crate::tools::agent_query_tool_names(),
            )),
            agent_belt: true,
            number_entity_lines: true,
            session_authority_mode: SessionAuthorityMode::OfflineFallback,
            ..McpServerConfig::default()
        }
    }

    fn tools_call(name: &str, arguments: serde_json::Value) -> String {
        serde_json::json!({
            "jsonrpc": "2.0", "id": 7, "method": "tools/call",
            "params": { "name": name, "arguments": arguments },
        })
        .to_string()
    }

    /// One change against `focal`'s history, with an id computed from its
    /// content, the shape the history handler's own tests build.
    fn history_change_for(
        parents: Vec<kin_model::SemanticChangeId>,
        deltas: Vec<kin_model::change::EntityDelta>,
        message: &str,
        second: usize,
    ) -> kin_model::change::SemanticChange {
        let root = parents.is_empty();
        let mut change = kin_model::change::SemanticChange {
            id: kin_model::SemanticChangeId::from_hash(kin_model::Hash256::from_bytes([0; 32])),
            origin: kin_model::change::ChangeOrigin::Native,
            parents,
            timestamp: serde_json::from_value(serde_json::json!(format!(
                "2026-09-22T21:{:02}:{:02}Z",
                second / 60,
                second % 60
            )))
            .unwrap(),
            author: kin_model::AuthorId::new("History parity"),
            message: message.into(),
            entity_deltas: deltas,
            relation_deltas: vec![],
            tree_deltas: vec![],
            admission_policy_delta: root.then(|| {
                kin_model::AdmissionPolicyDelta::initialize(
                    kin_model::SharedAdmissionPolicy::empty(0),
                )
            }),
            projected_files: vec![],
            spec_link: None,
            evidence: vec![],
            risk_summary: None,
            external_reference_deltas: vec![],
            resolution_record_deltas: Vec::new(),
        };
        change.id = kin_model::compute_semantic_change_id(&change).unwrap();
        change
    }

    /// The bounded entity history answers through the routed tool exactly as
    /// the named `entity_history` tool answers on a profile that serves it:
    /// the same default page of 20 and ceiling of 100, the same
    /// `next_offset`, `change_count` and `latest_change_id`, the same
    /// `max_chars` trimming, and the same structured refusals when the
    /// metadata cannot fit. Reached through `call` and by the tool's own name,
    /// on both routed surfaces, byte for byte once the named answer's hints
    /// name the routed commands.
    #[tokio::test]
    async fn routed_entity_history_pages_and_bounds_like_the_named_tool() {
        use kin_model::change::EntityDelta;
        use kin_model::graph::ChangeStore;
        let (store, entities) = routed_fixture();
        let focal = entities
            .iter()
            .find(|entity| entity.name == "routed_helper")
            .expect("the fixture's helper")
            .clone();
        let bystander = entities
            .iter()
            .find(|entity| entity.name == "routed_caller")
            .expect("the fixture's caller")
            .clone();
        // 35 changes to the helper, with long messages so a budget has to trim.
        let mut parent = Vec::new();
        let mut previous = focal.clone();
        for n in 0..35 {
            let delta = if n == 0 {
                EntityDelta::Added { new: focal.clone() }
            } else {
                let mut revised = previous.clone();
                revised.signature = format!("fn routed_helper() -> u32 /* revision {n} */");
                let old = std::mem::replace(&mut previous, revised.clone());
                EntityDelta::Modified { old, new: revised }
            };
            let change = history_change_for(
                parent.clone(),
                vec![delta],
                &format!("{n}:{}", "界".repeat(150)),
                n,
            );
            store.create_change(&change).unwrap();
            parent = vec![change.id];
        }
        // The caller's history: one change whose ancestry is too long to
        // carry, and one whose metadata no page of max_chars 2,000 can hold.
        let fake_parent = |seed: usize| {
            let mut bytes = [0u8; 32];
            bytes[..8].copy_from_slice(&(seed as u64).to_le_bytes());
            bytes[31] = 0x5a;
            kin_model::SemanticChangeId::from_hash(kin_model::Hash256::from_bytes(bytes))
        };
        let caller_root = history_change_for(
            vec![],
            vec![EntityDelta::Added {
                new: bystander.clone(),
            }],
            "caller root",
            100,
        );
        store.create_change(&caller_root).unwrap();
        let mut revised = bystander.clone();
        revised.signature = "fn routed_caller() -> u32 /* merged */".into();
        let wide_merge = history_change_for(
            std::iter::once(caller_root.id)
                .chain((0..40).map(fake_parent))
                .collect(),
            vec![EntityDelta::Modified {
                old: bystander.clone(),
                new: revised.clone(),
            }],
            "a merge with forty parents",
            101,
        );
        store.create_change(&wide_merge).unwrap();

        let focal_id = focal.id.to_string();
        let caller_id = bystander.id.to_string();
        let named = McpServerConfig {
            allowed_tools: None,
            agent_belt: true,
            session_authority_mode: SessionAuthorityMode::OfflineFallback,
            ..McpServerConfig::default()
        };
        let cases: Vec<serde_json::Value> = vec![
            serde_json::json!({"entity_id": focal_id}),
            serde_json::json!({"entity_id": focal_id, "offset": 20, "limit": 10}),
            serde_json::json!({"entity_id": focal_id, "limit": 100}),
            serde_json::json!({"entity_id": focal_id, "offset": 30}),
            serde_json::json!({"entity_id": focal_id, "max_chars": 8000}),
            serde_json::json!({"entity_id": caller_id, "max_chars": 2000}),
            serde_json::json!({"entity_id": caller_id}),
        ];
        let mut seen_codes = Vec::new();
        let mut seen_next = Vec::new();
        for arguments in &cases {
            let sessions = SessionRegistry::new();
            let named_answer = process_message(
                &tools_call("entity_history", arguments.clone()),
                &store,
                &named,
                &sessions,
            )
            .await
            .expect("a named response");
            let named_answer = presented_as_routed(named_answer, "entity_history", arguments);
            for surface in [with_writes(), read_only()] {
                for routed_args in [
                    serde_json::json!({"command": "call", "args": {"tool": "entity_history", "arguments": arguments}}),
                    serde_json::json!({"command": "entity_history", "args": arguments}),
                ] {
                    let routed_answer = process_message(
                        &tools_call(crate::routed::TOOL_NAME, routed_args.clone()),
                        &store,
                        &surface,
                        &sessions,
                    )
                    .await
                    .expect("a routed response");
                    assert_eq!(
                        serde_json::to_string(&routed_answer.result).unwrap(),
                        serde_json::to_string(&named_answer.result).unwrap(),
                        "{routed_args} on {:?} answered differently from entity_history",
                        surface.routed
                    );
                }
            }
            let text = named_answer.result.as_ref().unwrap()["content"][0]["text"]
                .as_str()
                .unwrap()
                .to_string();
            let payload: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
            if arguments.get("entity_id") == Some(&serde_json::json!(focal_id))
                && arguments.get("max_chars").is_none()
            {
                let rows = payload["result"].as_array().expect("a real history page");
                let offset = arguments["offset"].as_u64().unwrap_or(0);
                let limit = arguments["limit"].as_u64().unwrap_or(20);
                let returned = rows.len() as u64;
                assert!(returned > 0 && returned <= limit && offset + returned <= 35);
                assert_eq!(payload["returned"], rows.len());
                assert_eq!(
                    payload["next_offset"],
                    if offset + returned < 35 {
                        serde_json::json!(offset + returned)
                    } else {
                        serde_json::Value::Null
                    }
                );
                if limit == 100 {
                    assert!(returned < 35, "the ceiling case must exercise a budget cut");
                }
                assert!(
                    rows.iter().all(|row| row.get("detail_summary").is_none()),
                    "tail rows must be withheld before retained focal details"
                );
            }
            if let Some(code) = payload
                .pointer("/error/code")
                .or_else(|| payload.pointer("/_kin/error/code"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
                .or_else(|| {
                    [
                        "history_metadata_exceeds_budget",
                        "history_ancestry_exceeds_limit",
                    ]
                    .into_iter()
                    .find(|code| text.contains(code))
                    .map(str::to_string)
                })
            {
                seen_codes.push(code);
            }
            if arguments.get("entity_id") == Some(&serde_json::json!(focal_id))
                && arguments.get("max_chars").is_none()
            {
                seen_next.push((
                    arguments.clone(),
                    payload["limit"].clone(),
                    payload["returned"].clone(),
                    payload["next_offset"].clone(),
                    payload["change_count"].clone(),
                    payload["latest_change_id"].is_string(),
                ));
            }
        }
        // The controls: the pages are the bounded ones, and the refusal was
        // reached, so the comparison covered it. The ceiling page gives up tail
        // rows to preserve retained detail. Its size is budget-dependent;
        // next_offset must follow the rows actually returned on both routes.
        let returned = |case: usize| seen_next[case].2.as_u64().expect("returned row count");
        let page = |case: usize, limit: u64, returned: u64, next: serde_json::Value| {
            (
                cases[case].clone(),
                serde_json::json!(limit),
                serde_json::json!(returned),
                next,
                serde_json::json!(35),
                true,
            )
        };
        assert_eq!(
            seen_next,
            vec![
                page(0, 20, returned(0), serde_json::json!(returned(0))),
                page(1, 10, returned(1), serde_json::json!(20 + returned(1))),
                page(2, 100, returned(2), serde_json::json!(returned(2))),
                page(3, 20, 5, serde_json::Value::Null),
            ],
        );
        assert!(
            seen_codes.contains(&"history_metadata_exceeds_budget".to_string()),
            "{seen_codes:?}"
        );

        // Outside the schema's bounds, or the wrong type, both routes refuse,
        // and in the same sentence: the router before dispatch, the named
        // handler itself. Neither clamps.
        for (field, value, sentence) in [
            (
                "limit",
                serde_json::json!(500),
                "entity_history: limit must be at most 100.",
            ),
            (
                "limit",
                serde_json::json!(0),
                "entity_history: limit must be at least 1.",
            ),
            (
                "limit",
                serde_json::json!("ten"),
                "entity_history: limit must be an integer.",
            ),
            (
                "max_chars",
                serde_json::json!(1999),
                "entity_history: max_chars must be at least 2000.",
            ),
            (
                "max_chars",
                serde_json::json!(60001),
                "entity_history: max_chars must be at most 60000.",
            ),
            (
                "offset",
                serde_json::json!(-1),
                "entity_history: offset must be at least 0.",
            ),
        ] {
            let mut arguments = serde_json::json!({"entity_id": focal_id});
            arguments[field] = value.clone();
            let named_answer = process_message(
                &tools_call("entity_history", arguments.clone()),
                &store,
                &named,
                &SessionRegistry::new(),
            )
            .await
            .expect("a named response");
            let named_text = serde_json::to_string(&named_answer.result).unwrap();
            assert_eq!(
                named_answer.result.as_ref().unwrap()["isError"],
                true,
                "{field} {value}: {named_text}"
            );
            assert!(
                named_text.contains(sentence),
                "named {field} {value}: {named_text}"
            );
            for surface in [with_writes(), read_only()] {
                let routed_answer = process_message(
                    &tools_call(
                        crate::routed::TOOL_NAME,
                        serde_json::json!({"command": "call", "args": {"tool": "entity_history", "arguments": arguments}}),
                    ),
                    &store,
                    &surface,
                    &SessionRegistry::new(),
                )
                .await
                .expect("a routed response");
                let routed_text = serde_json::to_string(&routed_answer.result).unwrap();
                assert_eq!(
                    routed_answer.result.as_ref().unwrap()["isError"],
                    true,
                    "{field} {value}: {routed_text}"
                );
                assert!(
                    routed_text.contains(sentence),
                    "routed {field} {value}: {routed_text}"
                );
            }
        }
    }

    /// A store holding one small Rust file whose second function calls its
    /// first, so a reference, a chain and a route all have something to find.
    fn routed_fixture() -> (InMemoryGraph, Vec<kin_model::Entity>) {
        let store = InMemoryGraph::default();
        let bytes = b"fn routed_helper() -> u32 {\n    7\n}\n\nfn routed_caller() -> u32 {\n    routed_helper() + 1\n}\n";
        let kin_index::IndexedAny::EntitySource(indexed) = kin_index::IndexPipeline::new()
            .index_any_content(
                &kin_model::FilePathId::new("src/routed.rs"),
                bytes,
                kin_blobs::digest(bytes),
            )
            .unwrap()
        else {
            panic!("source fixture");
        };
        for entity in &indexed.entities {
            kin_model::EntityStore::upsert_entity(&store, entity).unwrap();
        }
        for relation in &indexed.relations {
            kin_model::EntityStore::upsert_relation(&store, relation).unwrap();
        }
        store.flush_text_index().unwrap();
        (store, indexed.entities)
    }

    /// `text` with every UUID replaced by one placeholder.
    fn mask_uuids(text: &str) -> String {
        let bytes = text.as_bytes();
        let shape = |at: usize| {
            at + 36 <= bytes.len()
                && bytes[at..at + 36].iter().enumerate().all(|(index, byte)| {
                    if matches!(index, 8 | 13 | 18 | 23) {
                        *byte == b'-'
                    } else {
                        byte.is_ascii_hexdigit()
                    }
                })
        };
        let mut out = String::with_capacity(text.len());
        let mut index = 0;
        while index < bytes.len() {
            if shape(index) {
                out.push_str("<uuid>");
                index += 36;
            } else {
                let character = text[index..].chars().next().unwrap();
                out.push(character);
                index += character.len_utf8();
            }
        }
        out
    }

    /// The named answer as a routed connection presents it: the same bytes
    /// with its hints naming the routed commands.
    fn presented_as_routed(
        mut response: JsonRpcResponse,
        tool: &str,
        arguments: &serde_json::Value,
    ) -> JsonRpcResponse {
        let params: ToolCallParams =
            serde_json::from_value(serde_json::json!({"name": tool, "arguments": arguments}))
                .unwrap();
        present_routed_hints(&mut response, &params);
        response
    }

    fn finalized_hint_context(
        tool: &str,
        arguments: &serde_json::Value,
        hint: &str,
        body: &str,
    ) -> JsonRpcResponse {
        let params: ToolCallParams = serde_json::from_value(serde_json::json!({
            "name": tool, "arguments": arguments
        }))
        .unwrap();
        let payload = serde_json::json!({
            "token_budget": 8000, "tokens_used": 0,
            "focal_entity": {"id": "focal", "name": "focal", "body": body, "body_complete": true},
            "dependencies": [{"id": "dependency", "name": "dependency", "body_unavailable": hint}],
            "dependents": [],
            "degradations": [{"reason": "references_are_incomplete"}]
        });
        let result = envelope::finalize_bounded(
            ToolCallResult::text(payload.to_string()),
            Envelope::daemon(),
            tool,
            &ResponseBudget::from_arguments(&params.arguments),
        );
        assert_ne!(result.is_error, Some(true));
        JsonRpcResponse::success(
            Some(serde_json::json!(1)),
            serde_json::to_value(result).unwrap(),
        )
    }

    fn presented_text(response: &JsonRpcResponse) -> &str {
        response.result.as_ref().unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
    }

    /// Exercise the supported final presentation boundary after real envelope
    /// qualification, including shortening, lengthening and equal-byte hints.
    #[test]
    fn routed_hint_accounting_matches_final_context_wire_without_changing_facts() {
        let body = "fn café() {\r\n    execute();\r\n}";
        let hints = [
            (
                "no source body was read for this row: it was not priced by this request's source projection; read it with get_entity_source",
                "no source body was read for this row: it was not priced by this request's source projection; read it with kin source",
            ),
            ("use graph_neighborhood", "use kin call graph_neighborhood"),
            ("use trace_data_flow", "use kin trace-data-flow"),
            ("use kin_graph_status", "use kin graph status"),
        ];
        for tool in ["get_context_pack", "trace_computation"] {
            for compact in [true, false] {
                let args = serde_json::json!({"compact": compact, "max_chars": 60000});
                for (hint, routed_hint) in hints {
                    let before = finalized_hint_context(tool, &args, hint, body);
                    let mut expected: serde_json::Value =
                        serde_json::from_str(presented_text(&before)).unwrap();
                    let after = presented_as_routed(before, tool, &args);
                    let text = presented_text(&after);
                    let actual: serde_json::Value = serde_json::from_str(text).unwrap();
                    assert_eq!(actual["dependencies"][0]["body_unavailable"], routed_hint);
                    assert_eq!(actual["_kin"]["response"]["chars_after_budget"], text.len());
                    assert_eq!(actual["tokens_used"], kin_context::estimate_tokens(text));
                    assert_eq!(text.contains('\n'), !compact);
                    assert!(text.len() <= 60000);
                    assert!(kin_context::estimate_tokens(text) <= 8000);
                    // Every fact, source byte, standing limitation, negative and
                    // authority field stays equal. Only these three values vary.
                    expected["dependencies"][0]["body_unavailable"] =
                        serde_json::json!(routed_hint);
                    expected["_kin"]["response"]["chars_after_budget"] =
                        serde_json::json!(text.len());
                    expected["tokens_used"] = serde_json::json!(kin_context::estimate_tokens(text));
                    assert_eq!(actual, expected, "{tool}, compact={compact}, {hint}");
                }
            }
        }
    }

    /// A routed spelling can be shorter in bytes and still require more of the
    /// context token estimate. It must not escape the effective context tier.
    #[test]
    fn routed_hint_accounting_keeps_original_at_effective_token_limit() {
        let args = serde_json::json!({"token_budget": 4000, "max_chars": 60000});
        for tool in ["get_context_pack", "trace_computation"] {
            let mut words = 5000;
            let mut at_limit = None;
            for _ in 0..8 {
                let body = "word ".repeat(words);
                let before = finalized_hint_context(tool, &args, "use get_entity_source", &body);
                let text = presented_text(&before);
                let payload: serde_json::Value = serde_json::from_str(text).unwrap();
                assert_eq!(payload["focal_entity"]["body"], body);
                let tokens = kin_context::estimate_tokens(text);
                assert!(tokens <= 8000);
                if tokens == 8000 {
                    at_limit = Some(before);
                    break;
                }
                words += ((8000 - tokens) * 7 / 8).max(1);
            }
            let before = at_limit.expect("the real finalizer reaches the effective token ceiling");
            // The same payload with room still takes the routed spelling even
            // though its effective tier exceeds the caller's requested tier.
            let roomy = finalized_hint_context(
                tool,
                &args,
                "use get_entity_source",
                &"word ".repeat(words - 100),
            );
            assert!(kin_context::estimate_tokens(presented_text(&roomy)) > 4000);
            let roomy_after = presented_as_routed(roomy, tool, &args);
            let payload: serde_json::Value =
                serde_json::from_str(presented_text(&roomy_after)).unwrap();
            assert_eq!(
                payload["dependencies"][0]["body_unavailable"],
                "use kin source"
            );
            let original = before.result.clone();
            let after = presented_as_routed(before, tool, &args);
            assert_eq!(after.result, original, "the named spelling fits exactly");
            assert_eq!(kin_context::estimate_tokens(presented_text(&after)), 8000);
        }
    }

    /// Recounting the final byte counter itself can add a digit. The old hint
    /// byte guard alone admitted this rewrite with stale four-digit accounting.
    #[test]
    fn routed_hint_accounting_checks_ceiling_after_counter_growth() {
        let args = serde_json::json!({"max_chars": 10000});
        let mut padding = 6000;
        let mut exact = None;
        for _ in 0..8 {
            let body = "x".repeat(padding);
            let before =
                finalized_hint_context("get_context_pack", &args, "use graph_neighborhood", &body);
            let text = presented_text(&before);
            let payload: serde_json::Value = serde_json::from_str(text).unwrap();
            assert_eq!(payload["focal_entity"]["body"], body);
            if text.len() == 9991 {
                exact = Some(before);
                break;
            }
            padding = padding
                .checked_add_signed(9991 - text.len() as isize)
                .unwrap();
        }
        let before = exact.expect("the finalized fixture is nine bytes below its ceiling");
        let original = before.result.clone();
        let after = presented_as_routed(before, "get_context_pack", &args);
        assert_eq!(after.result, original);
        assert_eq!(presented_text(&after).len(), 9991);
    }

    #[test]
    fn routed_hint_accounting_preserves_existing_soft_budget_residual() {
        let args = serde_json::json!({"max_chars": 2000});
        let params: ToolCallParams = serde_json::from_value(serde_json::json!({
            "name": "find_references", "arguments": args
        }))
        .unwrap();
        let result = envelope::finalize_bounded(
            ToolCallResult::text(
                serde_json::json!({
                    "references": [], "total": 0, "observation": "x".repeat(4000),
                    "hint": "use get_entity_source"
                })
                .to_string(),
            ),
            Envelope::daemon(),
            "find_references",
            &ResponseBudget::from_arguments(&params.arguments),
        );
        let before = JsonRpcResponse::success(
            Some(serde_json::json!(1)),
            serde_json::to_value(result).unwrap(),
        );
        assert!(presented_text(&before).len() > 2000);
        assert!(presented_text(&before).contains("response_over_budget"));
        let original = before.result.clone();
        let after = presented_as_routed(before, "find_references", &args);
        assert_eq!(after.result, original);
    }

    /// `graph source` and `kin graph source`, the CLI's spellings, answer a
    /// routed call byte for byte as `source` does on both routed profiles:
    /// payload, `_kin` envelope and any refusal. The Kin block `kin setup`
    /// writes for a routed client names source that way, so the same words
    /// run through the routed tool and in a shell.
    #[tokio::test]
    async fn graph_source_answers_exactly_as_source_does() {
        async fn answer(
            store: &InMemoryGraph,
            config: &McpServerConfig,
            command: &str,
            entity_id: &str,
        ) -> serde_json::Value {
            let request = tools_call(
                crate::routed::TOOL_NAME,
                serde_json::json!({"command": command, "args": {"entity_id": entity_id}}),
            );
            let response = process_message(&request, store, config, &SessionRegistry::new())
                .await
                .expect("a routed response");
            assert!(response.error.is_none(), "{command} was a transport error");
            response.result.expect("a result")
        }

        let (store, entities) = routed_fixture();
        let helper = entities
            .iter()
            .find(|entity| entity.name == "routed_helper")
            .expect("the fixture has routed_helper")
            .id
            .to_string();
        for config in [with_writes(), read_only()] {
            let source = answer(&store, &config, "source", &helper).await;
            let text = source["content"][0]["text"].as_str().expect("a text block");
            let payload: serde_json::Value =
                serde_json::from_str(text).expect("an enveloped payload is JSON");
            assert!(payload.get(ENVELOPE_KEY).is_some(), "{payload}");
            assert!(
                payload.get("example").is_none(),
                "source stopped at the router: {payload}"
            );
            for spelling in ["graph source", "kin graph source"] {
                assert_eq!(
                    answer(&store, &config, spelling, &helper).await,
                    source,
                    "{spelling} on {:?}",
                    config.routed
                );
            }
        }
    }

    /// Every routed command answers with exactly what its named tool answers
    /// on the named profile it stands in for, for the same arguments: payload,
    /// negative evidence and `_kin` envelope, byte for byte once the named
    /// answer's hints name the routed commands, including where the named tool
    /// itself refuses. `agent-routed` answers as `agent-default`, writes
    /// included, and `agent-routed-query` as `agent-query`.
    #[tokio::test]
    async fn each_routed_command_returns_its_named_tool_payload() {
        // Both answers read the process environment, and `KIN_SOURCE_ROOT`
        // decides every read path in them: set, the fixture's file reads as
        // `.kin/source-root/src/routed.rs`, unset as `src/routed.rs`. Other
        // tests in this binary set it through the environment guard while this
        // one runs, and one that set or restored it between a routed call and
        // its named call made the two answers differ in `hits[].file` and in
        // the response size accounting that follows from it. Holding the
        // guard's domain keeps the environment still for the whole comparison.
        let _environment = kin_core::test_env::EnvVarGuard::new();
        let (store, entities) = routed_fixture();
        let id_of = |name: &str| {
            entities
                .iter()
                .find(|entity| entity.name == name)
                .unwrap_or_else(|| panic!("the fixture has no {name}"))
                .id
                .to_string()
        };
        let (helper, caller) = (id_of("routed_helper"), id_of("routed_caller"));
        let reads: Vec<(serde_json::Value, &str, serde_json::Value)> = vec![
            (
                serde_json::json!({"command": "locate", "args": {"query": "routed helper"}}),
                "semantic_locate",
                serde_json::json!({"query": "routed helper"}),
            ),
            (
                serde_json::json!({"command": "search", "args": {"query": "routed_helper"}}),
                "semantic_search",
                serde_json::json!({"query": "routed_helper"}),
            ),
            (
                serde_json::json!({"command": "search", "args": {"literal": "routed_helper()"}}),
                "lexical_lookup",
                serde_json::json!({"literal": "routed_helper()"}),
            ),
            (
                serde_json::json!({"command": "context", "args": {"entity_id": caller}}),
                "get_context_pack",
                serde_json::json!({"entity_id": caller}),
            ),
            (
                serde_json::json!({"command": "refs", "args": {"entity_id": helper}}),
                "find_references",
                serde_json::json!({"entity_id": helper}),
            ),
            (
                serde_json::json!({"command": "trace", "args": {"focal": caller}}),
                "trace_data_flow",
                serde_json::json!({"focal": caller}),
            ),
            (
                serde_json::json!({"command": "path", "args": {"from": caller, "to": helper}}),
                "trace_path",
                serde_json::json!({"from": caller, "to": helper}),
            ),
            (
                serde_json::json!({"command": "impact", "args": {"entity_ids": [helper]}}),
                "impact_analysis",
                serde_json::json!({"entity_ids": [helper]}),
            ),
            (
                serde_json::json!({"command": "source", "args": {"entity_id": helper}}),
                "get_entity_source",
                serde_json::json!({"entity_id": helper}),
            ),
            (
                serde_json::json!({"command": "status"}),
                "kin_graph_status",
                serde_json::json!({}),
            ),
            (
                serde_json::json!({"command": "call", "args": {"tool": "graph_neighborhood", "arguments": {"entity_id": helper}}}),
                "graph_neighborhood",
                serde_json::json!({"entity_id": helper}),
            ),
            (
                serde_json::json!({"command": "graph_neighborhood", "args": {"entity_id": helper}}),
                "graph_neighborhood",
                serde_json::json!({"entity_id": helper}),
            ),
        ];
        // A mutate the handler refuses before it changes anything, so both
        // calls meet the same store: kin_mutate's own refusal, the same way
        // whichever name reached it. The update names an entity the store does
        // not hold and carries its source base, so it passes the semantic check
        // and opens a transaction before the in-process commit refuses its body.
        let edit = guarded_update_operation("fn x() {}");
        let writes: Vec<(serde_json::Value, &str, serde_json::Value)> = vec![(
            serde_json::json!({"command": "mutate", "args": {"operations": [edit]}}),
            "kin_mutate",
            serde_json::json!({"operations": [edit]}),
        )];
        let mut answered: Vec<&str> = Vec::new();
        let cases = reads
            .iter()
            .map(|case| (case, true))
            .chain(writes.iter().map(|case| (case, false)));
        for ((routed_args, tool, named_args), read) in cases {
            let pairs: Vec<(McpServerConfig, McpServerConfig)> = if read {
                vec![
                    (with_writes(), default_config()),
                    (read_only(), query_config()),
                ]
            } else {
                vec![(with_writes(), default_config())]
            };
            for (routed, named) in pairs {
                let sessions = SessionRegistry::new();
                let observed_start = kin_model::Timestamp::now();
                let routed_answer = process_message(
                    &tools_call(crate::routed::TOOL_NAME, routed_args.clone()),
                    &store,
                    &routed,
                    &sessions,
                )
                .await
                .expect("a routed response");
                let named_answer = process_message(
                    &tools_call(tool, named_args.clone()),
                    &store,
                    &named,
                    &sessions,
                )
                .await
                .expect("a named response");
                assert!(
                    routed_answer.error.is_none(),
                    "{routed_args} was a transport error"
                );
                let named_answer = presented_as_routed(named_answer, tool, named_args);
                let observed_end = kin_model::Timestamp::now();
                // A write opens its own transaction, so the two answers carry
                // different transaction ids and nothing else.
                let comparable = |result: &Option<serde_json::Value>| {
                    let mut result = result.clone();
                    // Independent traversals record their own duration. Normalize
                    // only the measured elapsed_ms, never semantic payload data.
                    if let Some(contents) = result
                        .as_mut()
                        .and_then(|v| v.get_mut("content"))
                        .and_then(serde_json::Value::as_array_mut)
                    {
                        for block in contents {
                            if let Some(text) =
                                block.get("text").and_then(serde_json::Value::as_str)
                            {
                                if let Ok(mut payload) =
                                    serde_json::from_str::<serde_json::Value>(text)
                                {
                                    if *tool == "kin_graph_status" {
                                        // Independent status calls observe the same live work
                                        // at different instants. Check both actual timestamps,
                                        // then compare every stable item and cursor identity.
                                        let observed = payload
                                            .pointer_mut("/open_transactions/observed_at")
                                            .expect("status carries its observation time");
                                        let timestamp: kin_model::Timestamp =
                                            serde_json::from_value(observed.clone())
                                                .expect("valid observation timestamp");
                                        assert!(
                                            timestamp >= observed_start
                                                && timestamp <= observed_end
                                        );
                                        *observed = serde_json::to_value(&observed_start).unwrap();
                                    }
                                    let mut timed = false;
                                    if let Some(explored) = payload
                                        .get_mut("explored")
                                        .and_then(serde_json::Value::as_array_mut)
                                    {
                                        for row in explored {
                                            if row.get("elapsed_ms").is_some() {
                                                row["elapsed_ms"] = serde_json::json!(0);
                                                timed = true;
                                            }
                                        }
                                    }
                                    // The envelope measured the reply with its
                                    // real timings, whose digit count varies
                                    // between two runs under load.
                                    if timed {
                                        if let Some(response) = payload
                                            .pointer_mut("/_kin/response")
                                            .and_then(serde_json::Value::as_object_mut)
                                        {
                                            response.remove("chars_before_budget");
                                            response.remove("chars_after_budget");
                                        }
                                    }
                                    block["text"] =
                                        serde_json::json!(serde_json::to_string(&payload).unwrap());
                                }
                            }
                        }
                    }
                    let text = serde_json::to_string(&result).unwrap();
                    if read {
                        text
                    } else {
                        mask_uuids(&text)
                    }
                };
                assert_eq!(
                    comparable(&routed_answer.result),
                    comparable(&named_answer.result),
                    "{routed_args} on {:?} answered differently from {tool}",
                    routed.routed
                );
                let result = routed_answer.result.expect("a result");
                let text = result["content"][0]["text"].as_str().expect("a text block");
                let payload: serde_json::Value =
                    serde_json::from_str(text).expect("an enveloped payload is JSON");
                assert!(
                    payload.get(ENVELOPE_KEY).is_some(),
                    "{routed_args} came back without the envelope: {payload}"
                );
                if result.get("isError") != Some(&serde_json::json!(true)) {
                    if routed.routed.is_some_and(|surface| surface.writes) {
                        answered.push(tool);
                    }
                    continue;
                }
                // The in-process route has no daemon and no pinned repository
                // authority, so some tools refuse here. Their refusal has to be
                // the HANDLER's own, which proves the routed call reached the
                // handler rather than stopping at the router.
                assert!(
                    payload.get("example").is_none(),
                    "{tool} came back with the router's refusal: {payload}"
                );
            }
        }
        // The control: the graph-only tools answer for real here, so the
        // comparison covers real payloads and not only matching refusals.
        answered.sort_unstable();
        assert!(
            answered.len() >= 5
                && [
                    "impact_analysis",
                    "semantic_search",
                    "trace_data_flow",
                    "trace_path"
                ]
                .iter()
                .all(|tool| answered.contains(tool)),
            "the commands that answer in-process moved: {answered:?}"
        );
    }

    /// A write on the read-only routed surface is refused before anything
    /// runs, and the refusal names the profile that carries it.
    #[tokio::test]
    async fn the_read_only_routed_surface_refuses_every_write() {
        let store = InMemoryGraph::default();
        let sessions = SessionRegistry::new();
        for arguments in [
            serde_json::json!({"command": "mutate", "args": {"operations": []}}),
            serde_json::json!({"command": "session", "args": {"vendor": "v", "client_name": "c", "cwd": "/"}}),
            serde_json::json!({"command": "kin_mutate", "args": {"operations": []}}),
            serde_json::json!({"command": "call", "args": {"tool": "kin_transaction_begin", "arguments": {}}}),
        ] {
            let result = process_message(
                &tools_call(crate::routed::TOOL_NAME, arguments.clone()),
                &store,
                &read_only(),
                &sessions,
            )
            .await
            .unwrap()
            .result
            .unwrap();
            assert_eq!(result["isError"], true, "{arguments}");
            let payload: serde_json::Value =
                serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
            let message = payload["message"].as_str().unwrap();
            assert!(
                message.contains("read-only agent-routed-query")
                    && message.contains("agent-routed profile carries"),
                "{arguments}: {message}"
            );
            assert!(payload.get(ENVELOPE_KEY).is_some());
        }
        assert!(
            sessions.list_agent_sessions().is_empty(),
            "a refused write still opened a session"
        );
        // Named directly, the write tool is refused the same way.
        let direct = process_message(
            &tools_call("kin_mutate", serde_json::json!({"operations": []})),
            &store,
            &read_only(),
            &sessions,
        )
        .await
        .unwrap()
        .result
        .unwrap();
        assert!(direct["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("agent-routed profile carries"));
    }

    /// `describe` and a refusal are answered by this server, with the
    /// envelope, on both routes, and the daemon route needs no daemon for
    /// either because neither reads a graph.
    #[tokio::test]
    async fn routed_describe_and_refusals_carry_the_envelope_on_both_routes() {
        let store = InMemoryGraph::default();
        let sessions = SessionRegistry::new();
        for surface in [
            crate::routed::RoutedSurface::WITH_WRITES,
            crate::routed::RoutedSurface::READ_ONLY,
        ] {
            let offline = routed_config(surface);
            let mut daemon_config = routed_config(surface);
            daemon_config.session_authority_mode = SessionAuthorityMode::DaemonRequired;
            for (arguments, is_error) in [
                (
                    serde_json::json!({"command": "describe", "args": {"command": "search"}}),
                    false,
                ),
                (serde_json::json!({"command": "describe"}), false),
                (
                    serde_json::json!({"command": "locate", "args": {"q": "retries"}}),
                    true,
                ),
            ] {
                let message = tools_call(crate::routed::TOOL_NAME, arguments.clone());
                for response in [
                    process_message(&message, &store, &offline, &sessions).await,
                    process_daemon_message(&message, &daemon_config).await,
                ] {
                    let result = response.expect("a response").result.expect("a result");
                    assert_eq!(
                        result.get("isError") == Some(&serde_json::json!(true)),
                        is_error,
                        "{arguments}: {result}"
                    );
                    let payload: serde_json::Value =
                        serde_json::from_str(result["content"][0]["text"].as_str().unwrap())
                            .unwrap();
                    assert!(payload.get(ENVELOPE_KEY).is_some(), "{payload}");
                    if is_error {
                        let message = payload["message"].as_str().expect("a refusal says why");
                        assert!(message.contains("query (string)"), "{message}");
                        assert_eq!(payload["example"]["command"], "locate");
                    } else if arguments.get("args").is_some() {
                        assert_eq!(payload["variants"].as_array().map(Vec::len), Some(2));
                    } else {
                        assert!(payload["other_tools"]
                            .as_array()
                            .is_some_and(|rows| !rows.is_empty()));
                    }
                }
            }
            // The control: a named tool the routed profile does not serve is
            // still refused, so routing did not become a way around the
            // profile, and the refusal names the command that runs it here.
            let refused = process_message(
                &tools_call("semantic_locate", serde_json::json!({"query": "x"})),
                &store,
                &offline,
                &sessions,
            )
            .await
            .unwrap()
            .result
            .unwrap();
            let text = refused["content"][0]["text"].as_str().unwrap();
            assert!(text.contains("not enabled in this MCP profile"), "{text}");
            assert!(text.contains("command locate"), "{text}");
        }
    }

    /// Initialize serves the question routing worded for the surface the
    /// profile serves.
    #[tokio::test]
    async fn initialize_serves_the_procedure_for_the_profile() {
        let store = InMemoryGraph::default();
        let sessions = SessionRegistry::new();
        let message = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
        for (config, expected, names) in [
            (with_writes(), ROUTED_SERVER_INSTRUCTIONS, "kin locate"),
            (read_only(), ROUTED_QUERY_SERVER_INSTRUCTIONS, "kin locate"),
            (
                query_config(),
                ASK_SERVER_INSTRUCTIONS,
                "Any question about the code, in plain words: ask.",
            ),
            (default_config(), SERVER_INSTRUCTIONS, "Where is X? locate"),
            (
                McpServerConfig::default(),
                SERVER_INSTRUCTIONS,
                "Where is X? locate",
            ),
        ] {
            let result = process_message(message, &store, &config, &sessions)
                .await
                .unwrap()
                .result
                .unwrap();
            let instructions = result["instructions"].as_str().unwrap();
            assert_eq!(instructions, expected);
            assert!(instructions.contains(names));
            let routes = if expected == ASK_SERVER_INSTRUCTIONS {
                "- Any question about the code, in plain words: ask."
            } else {
                "- What calls X? "
            };
            for line in [routes, "- complete: ", "- partial: "] {
                assert!(
                    instructions.contains(&format!("\n{line}")),
                    "{line}: {instructions}"
                );
            }
        }
    }

    /// Each profile's `tools/list` is what its surface says: one routed tool on
    /// the routed profiles, read-only on the read-only one, and the source
    /// tool described the way the connection serves bodies.
    #[tokio::test]
    async fn each_profile_lists_what_its_surface_serves() {
        let store = InMemoryGraph::default();
        let sessions = SessionRegistry::new();
        let list = r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#;
        let listed = |config: McpServerConfig| {
            let store = &store;
            let sessions = &sessions;
            async move {
                process_message(list, store, &config, sessions)
                    .await
                    .unwrap()
                    .result
                    .unwrap()["tools"]
                    .as_array()
                    .unwrap()
                    .clone()
            }
        };
        let routed = listed(with_writes()).await;
        assert_eq!(routed.len(), 1);
        assert_eq!(routed[0]["name"], crate::routed::TOOL_NAME);
        assert_eq!(routed[0]["annotations"]["readOnlyHint"], false);
        let commands = routed[0]["inputSchema"]["properties"]["command"]["enum"].clone();
        assert!(commands
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("mutate")));

        let query_routed = listed(read_only()).await;
        assert_eq!(query_routed.len(), 1);
        assert_eq!(query_routed[0]["annotations"]["readOnlyHint"], true);
        let commands = query_routed[0]["inputSchema"]["properties"]["command"]["enum"].clone();
        assert!(!commands
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("mutate")));
        assert!(query_routed[0]["description"]
            .as_str()
            .unwrap()
            .contains("+N offsets"));

        let source = |tools: &[serde_json::Value]| {
            tools
                .iter()
                .find(|tool| tool["name"] == crate::verbs::SOURCE)
                .map(|tool| tool["description"].as_str().unwrap().to_string())
                .unwrap()
        };
        assert_eq!(
            source(&listed(query_config()).await),
            crate::entity_lines::NUMBERED_SOURCE_DESCRIPTION
        );
        assert_ne!(
            source(&listed(default_config()).await),
            crate::entity_lines::NUMBERED_SOURCE_DESCRIPTION
        );
    }

    async fn drive_daemon_loop_with_config(
        client_messages: &[serde_json::Value],
        config: McpServerConfig,
    ) -> Vec<serde_json::Value> {
        let mut input = String::new();
        for message in client_messages {
            input.push_str(&message.to_string());
            input.push('\n');
        }
        let mut reader = BufReader::new(input.as_bytes());
        let mut written: Vec<u8> = Vec::new();
        run_stdio_daemon_over(&mut reader, &mut written, config, None, None, None)
            .await
            .expect("stdio loop must drain the scripted client session");
        String::from_utf8(written)
            .expect("server output is UTF-8")
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).expect("server output is JSON-RPC"))
            .collect()
    }

    /// Setting a folder up is a command a person runs: no named, discovered or
    /// routed MCP call reaches it, full and writable profiles included.
    #[tokio::test]
    async fn retired_init_is_refused_on_every_profile() {
        let mut configs = vec![
            McpServerConfig::default(),
            default_config(),
            query_config(),
            with_writes(),
            read_only(),
        ];
        for names in [
            crate::tools::agent_search_tool_names(),
            crate::tools::benchmark_tool_names(),
            crate::tools::context_bench_tool_names(),
        ] {
            configs.push(McpServerConfig {
                allowed_tools: Some(crate::tools::name_set(names)),
                ..Default::default()
            });
        }
        for mut config in configs {
            config.session_authority_mode = SessionAuthorityMode::DaemonRequired;
            config.client_root = Some(PathBuf::from("/work/app"));
            let calls = [
                ("init", serde_json::json!({})),
                ("kin_init", serde_json::json!({"path":"sub"})),
                (
                    "kin_tool_call",
                    serde_json::json!({"tool":"kin_init","arguments":{}}),
                ),
                ("kin", serde_json::json!({"command":"init"})),
                ("kin", serde_json::json!({"command":"kin_init"})),
                (
                    "kin",
                    serde_json::json!({"command":"call","args":{"tool":"kin_init","arguments":{}}}),
                ),
            ];
            let requests: Vec<serde_json::Value> = calls.into_iter().enumerate().map(|(id, (name, arguments))| serde_json::json!({
                "jsonrpc":"2.0", "id":id, "method":"tools/call", "params":{"name":name,"arguments":arguments}
            })).collect();
            let answers = drive_daemon_loop_with_config(&requests, config).await;
            assert_eq!(answers.len(), requests.len());
            for answer in answers {
                assert!(
                    answer.get("error").is_some() || answer["result"]["isError"] == true,
                    "{answer}"
                );
            }
        }
    }

    #[test]
    fn no_profile_lists_retired_initialization_or_execution() {
        let mut configs = vec![
            McpServerConfig::default(),
            default_config(),
            query_config(),
            with_writes(),
            read_only(),
        ];
        for names in [
            crate::tools::agent_search_tool_names(),
            crate::tools::benchmark_tool_names(),
            crate::tools::context_bench_tool_names(),
        ] {
            configs.push(McpServerConfig {
                allowed_tools: Some(crate::tools::name_set(names)),
                ..Default::default()
            });
        }
        for config in configs {
            let listed = served_tools_for(&config);
            for name in [
                "init",
                "kin_init",
                "exec",
                "kin_session_exec",
                "kin_todo_import",
            ] {
                assert!(!listed.tools.iter().any(|tool| tool.name == name));
                for tool in &listed.tools {
                    if let Some(commands) = tool
                        .input_schema
                        .pointer("/properties/command/enum")
                        .and_then(serde_json::Value::as_array)
                    {
                        assert!(!commands.iter().any(|command| command == name));
                    }
                }
            }
        }
        // Retiring operator commands does not remove targeted writes.
        assert!(
            !served_tools_for(&with_writes()).tools[0]
                .annotations
                .read_only_hint
        );
        assert!(
            served_tools_for(&read_only()).tools[0]
                .annotations
                .read_only_hint
        );
    }

    /// Every answer on a connection whose client works in a folder inside the
    /// bound repository leads with which repository answered, whatever
    /// produced it; an answer from the client's own folder is left as it is.
    #[test]
    fn every_answer_for_a_nested_folder_leads_with_the_repository_that_answered() {
        let answer = |text: &str| {
            JsonRpcResponse::success(
                Some(serde_json::json!(1)),
                serde_json::json!({"content": [{"type": "text", "text": text}]}),
            )
        };
        let text_of = |response: &JsonRpcResponse| -> String {
            response.result.as_ref().unwrap()["content"][0]["text"]
                .as_str()
                .unwrap()
                .to_string()
        };
        let nested = McpServerConfig {
            client_root: Some(PathBuf::from("/work/repo/app")),
            ..McpServerConfig::default()
        };
        let root = Path::new("/work/repo");

        // A routed refusal answered here, with an envelope and no repository,
        // in each wire format. The stamp keeps the format the answer already
        // had, and the warning leads the envelope in both.
        let refusal = serde_json::json!({"_kin": {"envelope_version": 2}, "message": "no"});
        let pretty = serde_json::to_string_pretty(&refusal).unwrap();
        let mut refused = answer(&pretty);
        stamp_client_folder(&mut refused, Some(root), &nested);
        let text = text_of(&refused);
        let first = text.lines().nth(2).unwrap().trim_start();
        assert!(
            first.starts_with(&format!(
                "\"advice\": \"{}",
                crate::first_contact::SEMANTIC_SCOPE_WARNING
            )),
            "{text}"
        );
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        let expected_identity = crate::first_contact::semantic_repository_identity(
            &crate::first_contact::repository_identity(root, nested.client_root.as_deref()),
        )
        .unwrap();
        assert_eq!(
            value["_kin"]["repository"],
            serde_json::to_value(&expected_identity).unwrap()
        );
        assert!(!text.contains("/work/"));
        assert_eq!(value["message"], "no");
        let mut compact = answer(&refusal.to_string());
        stamp_client_folder(&mut compact, Some(root), &nested);
        let compact_text = text_of(&compact);
        assert!(!compact_text.contains('\n'), "{compact_text}");
        assert!(
            compact_text.starts_with(&format!(
                "{{\"_kin\":{{\"advice\":\"{}",
                crate::first_contact::SEMANTIC_SCOPE_WARNING
            )),
            "{compact_text}"
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&compact_text).unwrap(),
            value
        );

        // An answer with advice of its own keeps it, after the warning.
        let mut advised = answer(r#"{"_kin": {"advice": "Missing: Go references."}, "hits": []}"#);
        stamp_client_folder(&mut advised, Some(root), &nested);
        let value: serde_json::Value = serde_json::from_str(&text_of(&advised)).unwrap();
        let advice = value["_kin"]["advice"].as_str().unwrap();
        assert!(
            advice.starts_with(crate::first_contact::SEMANTIC_SCOPE_WARNING)
                && advice.ends_with("Missing: Go references."),
            "{advice}"
        );

        // An answer the daemon's health already stamped is not stamped twice.
        let health = serde_json::json!({"repo_root": "/work/repo"});
        let stamped_env = crate::envelope::Envelope::daemon().with_repository(
            &health,
            Some(Path::new("/work/repo/app")),
            Path::to_path_buf,
        );
        let finalized = crate::envelope::finalize(
            ToolCallResult::text(serde_json::json!({"references": []}).to_string()),
            stamped_env,
            "find_references",
        );
        let mut from_health = present_for_client(
            JsonRpcResponse::success(
                Some(serde_json::json!(1)),
                serde_json::to_value(&finalized).unwrap(),
            ),
            r#"{"method":"tools/call","params":{"name":"find_references","arguments":{}}}"#,
            &nested,
        );
        let before = text_of(&from_health);
        stamp_client_folder(&mut from_health, Some(root), &nested);
        assert_eq!(text_of(&from_health), before);
        let health_value: serde_json::Value = serde_json::from_str(&before).unwrap();
        assert_eq!(
            health_value["_kin"]["advice"]
                .as_str()
                .unwrap()
                .matches(crate::first_contact::SEMANTIC_SCOPE_WARNING)
                .count(),
            1,
            "{before}"
        );

        // Plain text gets the warning as its first line.
        let mut plain = answer("tool 'x' is not enabled in this MCP profile");
        stamp_client_folder(&mut plain, Some(root), &nested);
        assert!(text_of(&plain).starts_with(crate::first_contact::SEMANTIC_SCOPE_WARNING));
        assert!(!text_of(&plain).contains("/work/"));

        // The client's own folder, or no bound repository: nothing changes.
        let own = McpServerConfig {
            client_root: Some(PathBuf::from("/work/repo")),
            ..McpServerConfig::default()
        };
        let mut same = answer(r#"{"message": "ok"}"#);
        stamp_client_folder(&mut same, Some(root), &own);
        assert_eq!(text_of(&same), r#"{"message": "ok"}"#);
        let mut unbound = answer(r#"{"message": "ok"}"#);
        stamp_client_folder(&mut unbound, None, &nested);
        assert_eq!(text_of(&unbound), r#"{"message": "ok"}"#);
    }

    #[test]
    fn routed_hints_preserve_frozen_full_reading_and_fragment_pages() {
        let hint = "Read it with get_entity_source.";
        for tool in ["find_references", "trace_data_flow"] {
            let call: ToolCallParams = serde_json::from_value(serde_json::json!({
                "name": tool, "arguments": {"max_chars": 2000}
            }))
            .unwrap();
            for payload in [
                serde_json::json!({"_kin":{"page":{"version":1,"complete":true}},"hint":hint}),
                serde_json::json!({"_kin":{"page":{"version":1,"complete":false}},
                    "readings":[{"key":"_kin","value":{"advice":hint}}]}),
                serde_json::json!({"_kin":{"page":{"version":1,"complete":false}},
                    "record_fragment":{"collection":"readings","index":0,"key":"_kin",
                        "field":"advice","encoding":"utf8","byte_offset":0,
                        "total_bytes":hint.len(),"field_complete":true,"record_complete":true,"text":hint}}),
            ] {
                let text = serde_json::to_string_pretty(&payload).unwrap();
                let mut response = JsonRpcResponse::success(
                    Some(serde_json::json!(1)),
                    serde_json::to_value(ToolCallResult::text(text)).unwrap(),
                );
                let before = serde_json::to_vec(&response).unwrap();
                present_routed_hints(&mut response, &call);
                assert_eq!(serde_json::to_vec(&response).unwrap(), before);
            }
            let mut ordinary = JsonRpcResponse::success(
                Some(serde_json::json!(1)),
                serde_json::to_value(ToolCallResult::text(
                    serde_json::json!({
                        "hint":hint,"_kin":{"advice":hint}
                    })
                    .to_string(),
                ))
                .unwrap(),
            );
            present_routed_hints(&mut ordinary, &call);
            let payload: serde_json::Value = serde_json::from_str(
                ordinary.result.as_ref().unwrap()["content"][0]["text"]
                    .as_str()
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(payload["hint"], "Read it with kin source.");
            assert_eq!(payload["_kin"]["advice"], hint);
        }
    }

    #[test]
    fn paged_client_folder_forwarding_uses_only_the_canonical_connection() {
        fn canonicalize(path: &Path) -> PathBuf {
            if path == Path::new("/link") {
                PathBuf::from("/work/repo/app")
            } else {
                path.to_path_buf()
            }
        }
        for tool in ["find_references", "trace_data_flow", "get_entity"] {
            for client_root in [None, Some(PathBuf::from("/link"))] {
                let mut call: ToolCallParams = serde_json::from_value(serde_json::json!({
                    "name": tool,
                    "arguments": {"query": "Target", "__kin_client_root": "/spoofed"}
                }))
                .unwrap();
                let config = McpServerConfig {
                    client_root: client_root.clone(),
                    canonicalize,
                    ..McpServerConfig::default()
                };
                bind_paged_client_folder(&mut call, &config);
                assert_eq!(call.arguments["query"], "Target");
                let expected = (client_root.is_some() && tool != "get_entity")
                    .then(|| serde_json::json!("/work/repo/app"));
                assert_eq!(
                    call.arguments.get(crate::trace_pages::CLIENT_ROOT_ARGUMENT),
                    expected.as_ref()
                );
            }
        }
    }

    #[test]
    fn paged_client_folder_preserves_frozen_identity_and_exact_wire_bytes() {
        let _cache_test = crate::trace_pages::cache_test_guard();
        const CEILING: usize = 4000;
        for tool in ["find_references", "trace_data_flow", "impact_analysis"] {
            for client in ["/work/repo", "/work/repo/app"] {
                for count in [0, 1, 24] {
                    let config = McpServerConfig {
                        client_root: Some(PathBuf::from(client)),
                        ..McpServerConfig::default()
                    };
                    let identity = crate::first_contact::semantic_repository_identity(
                        &crate::first_contact::repository_identity(
                            Path::new("/work/repo"),
                            Some(Path::new(client)),
                        ),
                    )
                    .unwrap();
                    let primary = match tool {
                        "find_references" => "references",
                        "impact_analysis" => "affected_callers",
                        _ => "chain",
                    };
                    let rows: Vec<_> = (0..count)
                        .map(|index| {
                            serde_json::json!({
                                "entity_id": format!("row-{index}"), "step": index + 1,
                                "parent_step": 0, "body": "x".repeat(300)
                            })
                        })
                        .collect();
                    let mut payload =
                        serde_json::json!({"negative": {"safe_to_conclude_absent": false}});
                    payload[primary] = serde_json::json!(rows);
                    let context = crate::trace_pages::Context::new(
                        &serde_json::json!({"tool": tool, "client": client, "count": count}),
                        &serde_json::json!({"repo": "/work/repo", "generation": 7}),
                    );
                    let envelope = crate::Envelope::daemon().with_repository(
                        &serde_json::json!({"repo_root": "/work/repo"}),
                        Some(Path::new(client)),
                        Path::to_path_buf,
                    );
                    let result = ToolCallResult::text(payload.to_string());
                    let result = if tool == "find_references" {
                        crate::reference_pages::finalize(
                            result,
                            envelope,
                            context.clone(),
                            &crate::budget::ResponseBudget {
                                max_chars: CEILING,
                                ..Default::default()
                            },
                        )
                    } else if tool == "impact_analysis" {
                        crate::impact_pages::finalize(
                            result,
                            envelope,
                            context.clone(),
                            &crate::budget::ResponseBudget {
                                max_chars: CEILING,
                                ..Default::default()
                            },
                        )
                    } else {
                        crate::trace_pages::finalize(result, envelope, context.clone(), CEILING)
                    };
                    assert_ne!(result.is_error, Some(true), "{result:?}");
                    let crate::ContentBlock::Text { text } = &result.content[0];
                    let mut page: serde_json::Value = serde_json::from_str(text).unwrap();
                    let mut pages = 0;
                    loop {
                        pages += 1;
                        assert!(pages < 100);
                        let before = page.to_string();
                        assert!(before.len() <= CEILING);
                        assert_eq!(page["_kin"]["response"]["chars_after_budget"], before.len());
                        assert_eq!(
                            page["_kin"]["repository"],
                            serde_json::to_value(&identity).unwrap()
                        );
                        assert_eq!(page["_kin"]["semantic_surface"], 1);
                        assert!(!before.contains("/work/"));
                        let mut response = JsonRpcResponse::success(
                            Some(serde_json::json!(1)),
                            serde_json::to_value(ToolCallResult::text(before.clone())).unwrap(),
                        );
                        // A later binding must neither replace captured identity nor add bytes.
                        stamp_client_folder(&mut response, Some(Path::new("/later/repo")), &config);
                        assert_eq!(
                            response.result.as_ref().unwrap()["content"][0]["text"],
                            before
                        );
                        let Some(cursor) = page["next_cursor"].as_str() else {
                            break;
                        };
                        page = crate::trace_pages::resume(cursor, &context, CEILING).unwrap();
                    }
                    if count == 24 {
                        assert!(pages > 1);
                    }
                }
            }
        }
    }

    #[test]
    fn profiled_pages_preserve_captured_identity_through_stdio_presentation() {
        let _cache_test = crate::trace_pages::cache_test_guard();
        use serde_json::json;

        for tool in ["find_references", "trace_data_flow"] {
            for agent_belt in [false, true] {
                for client in ["/work/repo", "/work/repo/app"] {
                    for count in [0, 3] {
                        for requested_ceiling in [None, Some(2000), Some(8000), Some(60_000)] {
                            let config = McpServerConfig {
                                agent_belt,
                                number_entity_lines: agent_belt,
                                client_root: Some(PathBuf::from(client)),
                                ..McpServerConfig::default()
                            };
                            let mut call: ToolCallParams = serde_json::from_value(json!({
                                "name":tool, "arguments":{"query":"Context.pop"}
                            }))
                            .unwrap();
                            if let Some(ceiling) = requested_ceiling {
                                call.arguments.insert("max_chars".into(), json!(ceiling));
                            }
                            if config.agent_belt {
                                crate::agent_belt::apply_belt_defaults(tool, &mut call.arguments);
                            }
                            bind_paged_client_folder(&mut call, &config);
                            let budget = ResponseBudget::from_arguments(&call.arguments);
                            assert_eq!(budget.answer_only, agent_belt && tool == "find_references");
                            let context = crate::reference_pages::context(
                                &call.arguments,
                                &json!({"repo":"/work/repo", "generation":7}),
                            );
                            let mut envelope = Envelope::daemon().with_repository(
                                &json!({"repo_root":"/work/repo"}),
                                Some(Path::new(client)),
                                Path::to_path_buf,
                            );
                            envelope.graph_as_of =
                                Some(json!({"generation":7, "graph_root":"held"}));
                            envelope.answered_by =
                                crate::envelope::AnsweringDaemon::from_health(&json!({
                                    "pid":42, "repo_root":"/work/repo", "repo_id":"held-repository",
                                    "uptime_seconds":100, "version":"test"
                                }));
                            let mut public_envelope = json!({"_kin": &envelope});
                            crate::semantic_surface::project(tool, &mut public_envelope);
                            // A real resolved focal enters answer-only projection. The older
                            // identity test omitted it and therefore never exercised that branch.
                            let mut payload = json!({
                                "focal_entity":{"entity_id":"focal", "name":"Context.pop"},
                                "total_upstream":count,
                                "call_sites":{
                                    "candidate_count":2,
                                    "candidates":[
                                        {"caller":"other", "callee":"pop", "reason":kin_model::call_site_reading::REACH_CALLEE_SPELLS,
                                         "expr": format!("pop({})", "unverified_argument, ".repeat(450))},
                                        {"caller":"another", "callee":"pop", "reason":kin_model::call_site_reading::REACH_CALLEE_SPELLS}
                                    ],
                                    "clauses":["proof_context_unverified: recorded proof is unverified"]
                                },
                                "source_derivation":crate::source_derivation::SourceDerivationObservation::unavailable(
                                    &"source observation remains unproven ".repeat(220)
                                ),
                                "negative":{"safe_to_conclude_absent":false}
                            });
                            let primary = if tool == "find_references" {
                                "references"
                            } else {
                                "chain"
                            };
                            payload[primary] = json!((0..count)
                                .map(|index| json!({
                                    "entity_id":format!("row-{index}"), "step":index+1,
                                    "parent_step":0, "resolution":"type_resolved",
                                    "signature":"held signature", "body":"semantic body"
                                }))
                                .collect::<Vec<_>>());
                            let raw = ToolCallResult::text(payload.to_string());
                            let page_result = if tool == "find_references" {
                                crate::reference_pages::finalize(
                                    raw,
                                    envelope.clone(),
                                    context.clone(),
                                    &budget,
                                )
                            } else {
                                crate::trace_pages::finalize(
                                    raw,
                                    envelope.clone(),
                                    context.clone(),
                                    budget.max_chars,
                                )
                            };
                            assert_ne!(page_result.is_error, Some(true), "{page_result:?}");
                            let ContentBlock::Text { text } = &page_result.content[0];
                            let mut page: serde_json::Value = serde_json::from_str(text).unwrap();
                            let mut pages = 0;
                            let mut saw_fragment = false;
                            loop {
                                pages += 1;
                                assert!(pages < 500, "continuation did not terminate");
                                saw_fragment |= page.get("record_fragment").is_some();
                                let before = page.to_string();
                                assert!(before.len() <= budget.max_chars);
                                assert_eq!(
                                    page["_kin"]["response"]["chars_after_budget"],
                                    before.len()
                                );
                                for key in ["repository", "graph_as_of", "answered_by", "runtime"] {
                                    assert_eq!(
                                        page["_kin"][key], public_envelope["_kin"][key],
                                        "{tool}/{agent_belt}/{key}"
                                    );
                                }
                                assert_eq!(page["_kin"]["semantic_surface"], 1);
                                assert!(!before.contains("/work/"));
                                assert_eq!(
                                    page["_kin"]["verdict"]["safe_to_conclude_absent"],
                                    false
                                );
                                if page["_kin"]["page"]["complete"] == false {
                                    assert!(
                                        page["_kin"].get("source_derivation").is_none(),
                                        "an unavailable inventory has no measured counts to repeat"
                                    );
                                }
                                if page["_kin"]["page"]["complete"] == true {
                                    if budget.answer_only {
                                        assert_eq!(page["_kin"]["shape"], "answer_only");
                                        // Every named leftover rides the reply.
                                        assert_eq!(
                                            page["call_site_candidates"]["named"]
                                                .as_array()
                                                .unwrap()
                                                .len(),
                                            2
                                        );
                                        assert!(page["call_site_candidates"]
                                            .get("named_withheld")
                                            .is_none());
                                    }
                                    assert_eq!(page[primary].as_array().unwrap().len(), count);
                                    assert!(page["_kin"].get("source_derivation").is_none());
                                    let candidate = if budget.answer_only {
                                        &page["call_site_candidates"]["named"][0]
                                    } else {
                                        &page["call_sites"]["candidates"][0]
                                    };
                                    assert_eq!(
                                        candidate["expr"],
                                        payload["call_sites"]["candidates"][0]["expr"]
                                    );
                                }
                                // The real stdio sequence must not strip, rewrite or requalify
                                // any frozen field, including fragments, safety readings and counts.
                                let mut result = ToolCallResult::text(before.clone());
                                present_result(&config, tool, &HashMap::new(), &mut result);
                                result = envelope::finalize_bounded(
                                    result,
                                    Envelope::daemon_unreachable(),
                                    tool,
                                    &budget,
                                );
                                let mut response = JsonRpcResponse::success(
                                    Some(json!(1)),
                                    serde_json::to_value(result).unwrap(),
                                );
                                stamp_client_folder(
                                    &mut response,
                                    Some(Path::new("/later/repository")),
                                    &config,
                                );
                                let response = present_for_client(
                                    response,
                                    &json!({"method":"tools/call", "params":{"name":tool,"arguments":{}}}).to_string(),
                                    &config,
                                );
                                assert_ne!(response.result.as_ref().unwrap()["isError"], true);
                                assert_eq!(
                                    response.result.as_ref().unwrap()["content"][0]["text"],
                                    before
                                );
                                let Some(cursor) = page["next_cursor"].as_str() else {
                                    break;
                                };
                                page =
                                    crate::trace_pages::resume(cursor, &context, budget.max_chars)
                                        .unwrap_or_else(|error| {
                                            panic!(
                                                "{tool}/{agent_belt}/{client}/{count}/{requested_ceiling:?}, \
                                                 after page {pages}: {error}"
                                            )
                                        });
                            }
                            if requested_ceiling == Some(60_000) {
                                assert_eq!(pages, 1, "complete-envelope control must fit");
                            } else if requested_ceiling == Some(2000) {
                                assert!(
                                    pages > 1,
                                    "floor control must traverse continuation pages"
                                );
                                assert!(
                                    saw_fragment,
                                    "floor control must preserve a fragmented safety reading"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn paged_client_folder_refuses_missing_or_stale_presentation_without_growth() {
        let config = McpServerConfig {
            client_root: Some(PathBuf::from("/work/repo/app")),
            ..McpServerConfig::default()
        };
        let public_identity = |client: &str| {
            serde_json::to_value(
                crate::first_contact::semantic_repository_identity(
                    &crate::first_contact::repository_identity(
                        Path::new("/work/repo"),
                        Some(Path::new(client)),
                    ),
                )
                .unwrap(),
            )
            .unwrap()
        };
        let valid = public_identity("/work/repo/app");
        let mut unknown_field = valid.clone();
        unknown_field["root"] = serde_json::json!("/work/repo");
        let mut wrong_warning = valid.clone();
        wrong_warning["warning"] = serde_json::json!("/private/coordinate");
        let mut invalid_digest = valid.clone();
        invalid_digest["authority_context"] = serde_json::json!("not-a-context-digest");
        for (repository, version) in [
            (serde_json::Value::Null, Some(1)),
            (serde_json::json!({"root": "/work/repo"}), Some(1)),
            (
                serde_json::to_value(crate::first_contact::repository_identity(
                    Path::new("/work/repo"),
                    Some(Path::new("/work/repo/app")),
                ))
                .unwrap(),
                Some(1),
            ),
            (public_identity("/other/client"), Some(1)),
            (unknown_field, Some(1)),
            (wrong_warning, Some(1)),
            (invalid_digest, Some(1)),
            (valid.clone(), None),
            (valid, Some(2)),
        ] {
            let mut page = serde_json::json!({
                "_kin": {"page": {"version": 1}, "repository": repository},
                "references": [{"body": "x".repeat(1600)}], "next_cursor": "held"
            });
            if let Some(version) = version {
                page["_kin"]["semantic_surface"] = serde_json::json!(version);
            }
            let mut response = JsonRpcResponse::success(
                Some(serde_json::json!(1)),
                serde_json::to_value(ToolCallResult::text(page.to_string())).unwrap(),
            );
            stamp_client_folder(&mut response, Some(Path::new("/work/repo/app")), &config);
            let result = response.result.unwrap();
            assert_eq!(result["isError"], true);
            let text = result["content"][0]["text"].as_str().unwrap();
            assert!(text.len() < 2000);
            assert!(text.contains("restart the query without a cursor"));
            assert!(!text.contains("held"));
        }
    }

    /// A client that asks for exact entity bodies when it connects, as `kin
    /// agent run` does, is served them on a profile that would otherwise
    /// number, and its listing says so; a client that does not ask is served
    /// the numbered form.
    #[tokio::test]
    async fn a_client_that_asks_for_exact_bodies_is_served_them() {
        let initialize = |capabilities: serde_json::Value| {
            serde_json::json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {"protocolVersion": "2024-11-05", "capabilities": capabilities,
                           "clientInfo": {"name": "kin-agent", "version": "0"}},
            })
        };
        let list = serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"});
        let exact = serde_json::json!({"experimental": {"kin": {"exactEntityBodies": true}}});
        let mut daemon_query = query_config();
        daemon_query.session_authority_mode = SessionAuthorityMode::DaemonRequired;
        let mut daemon_routed = read_only();
        daemon_routed.session_authority_mode = SessionAuthorityMode::DaemonRequired;
        for (capabilities, numbered) in [(serde_json::json!({}), true), (exact, false)] {
            let responses = drive_daemon_loop_with_config(
                &[initialize(capabilities.clone()), list.clone()],
                daemon_query.clone(),
            )
            .await;
            let tools = responses[1]["result"]["tools"].as_array().unwrap();
            let source = tools
                .iter()
                .find(|tool| tool["name"] == crate::verbs::SOURCE)
                .unwrap();
            assert_eq!(
                source["description"] == crate::entity_lines::NUMBERED_SOURCE_DESCRIPTION,
                numbered,
                "{capabilities}"
            );
            let responses = drive_daemon_loop_with_config(
                &[initialize(capabilities.clone()), list.clone()],
                daemon_routed.clone(),
            )
            .await;
            let description = responses[1]["result"]["tools"][0]["description"]
                .as_str()
                .unwrap()
                .to_string();
            assert_eq!(
                description.contains("+N offsets"),
                numbered,
                "{description}"
            );
        }
        let mut config = query_config();
        config.serve_exact_entity_bodies();
        assert!(!config.number_entity_lines);
        let mut routed = read_only();
        routed.serve_exact_entity_bodies();
        assert!(!routed.number_entity_lines && !routed.routed.unwrap().numbered);
    }

    /// A routed dispatch's hints name the spellings that reach each tool
    /// through the routed tool and in a shell, and its `_kin` envelope is
    /// exactly the named answer's. The session command has no CLI spelling,
    /// so its tool is named with `kin call`.
    #[tokio::test]
    async fn a_routed_answer_names_routed_commands_in_its_hints() {
        let store = InMemoryGraph::default();
        let sessions = SessionRegistry::new();
        // Guarded, so the refusal whose hints are under test is the missing
        // session's and not the operation's. Keyed, so the call has to name its
        // session rather than run in the connection's own.
        let arguments = serde_json::json!({
            "operations": [guarded_update_operation("pub fn value() {}")],
            "request_id": "k1"
        });
        let named = process_message(
            &tools_call("kin_mutate", arguments.clone()),
            &store,
            &McpServerConfig {
                session_authority_mode: SessionAuthorityMode::DaemonRequired,
                ..default_config()
            },
            &sessions,
        )
        .await
        .unwrap()
        .result
        .unwrap();
        let mut daemon_routed = with_writes();
        daemon_routed.session_authority_mode = SessionAuthorityMode::DaemonRequired;
        let routed = process_daemon_message(
            &tools_call(
                crate::routed::TOOL_NAME,
                serde_json::json!({"command": "mutate", "args": arguments}),
            ),
            &daemon_routed,
        )
        .await
        .unwrap()
        .result
        .unwrap();
        let text = |result: &serde_json::Value| {
            serde_json::from_str::<serde_json::Value>(
                result["content"][0]["text"].as_str().unwrap(),
            )
            .unwrap()
        };
        let (named_payload, routed_payload) = (text(&named), text(&routed));
        let named_message = named_payload["message"].as_str().unwrap();
        let routed_message = routed_payload["message"].as_str().unwrap();
        assert!(
            named_message.contains("kin_session_start"),
            "{named_message}"
        );
        assert_eq!(
            routed_message,
            named_message.replace("kin_session_start", "kin call kin_session_start"),
            "{routed_message}"
        );
        assert_eq!(routed_payload[ENVELOPE_KEY], named_payload[ENVELOPE_KEY]);
    }

    /// An entity's body is marked with each line's offset where the
    /// connection numbers, and served exactly everywhere else: a profile that
    /// can write through Kin, the full surface, the citable benchmark
    /// profiles, and a client that asked. A whole-file read is never numbered.
    #[test]
    fn entity_bodies_are_numbered_only_where_the_connection_numbers() {
        let record = serde_json::json!({
            "id": "e1", "file_path": "src/lib.rs", "start_line": 40, "end_line": 42,
            "body": "fn a() {\n    b();\n}",
        });
        let read = serde_json::json!({"path_label": "src/lib.rs", "text_utf8": "fn a() {\n}\n"});
        let presented = |config: &McpServerConfig, tool: &str, payload: &serde_json::Value| {
            let mut result = ToolCallResult::text(payload.to_string());
            present_result(config, tool, &HashMap::new(), &mut result);
            let crate::types::ContentBlock::Text { text } = &result.content[0];
            serde_json::from_str::<serde_json::Value>(text).unwrap()
        };
        let named = |names: Option<&[&str]>, belt: bool| McpServerConfig {
            allowed_tools: names.map(crate::tools::name_set),
            agent_belt: belt,
            ..McpServerConfig::default()
        };
        let search = McpServerConfig {
            number_entity_lines: true,
            ..named(Some(crate::tools::agent_search_tool_names()), true)
        };
        for config in [read_only(), query_config(), search] {
            for tool in ["get_entity_source", "get_entity_body"] {
                let numbered = presented(&config, tool, &record);
                assert_eq!(numbered["body"], "+0\tfn a() {\n+1\t    b();\n+2\t}");
                assert_eq!(
                    numbered[crate::entity_lines::NUMBERING_KEY],
                    crate::entity_lines::NUMBERING_NOTE
                );
                assert_eq!(numbered["start_line"], 40, "the file location moved");
                assert_eq!(numbered["end_line"], 42, "the file location moved");
            }
            assert_eq!(presented(&config, "unregistered_tool", &read), read);
        }
        let mut asked = query_config();
        asked.serve_exact_entity_bodies();
        for config in [
            with_writes(),
            default_config(),
            named(Some(crate::tools::agent_default_tool_names()), true),
            named(None, false),
            named(Some(crate::tools::benchmark_tool_names()), false),
            named(Some(crate::tools::context_bench_tool_names()), false),
            asked,
        ] {
            assert_eq!(presented(&config, "get_entity_source", &record), record);
        }
    }

    #[test]
    fn protocol_negotiation_answers_the_clients_version_or_the_right_fallback() {
        for (requested, answered) in [
            (Some("2025-11-25"), "2025-11-25"),
            (Some("2025-06-18"), "2025-06-18"),
            (Some("2024-11-05"), "2024-11-05"),
            // Newer than this server: its newest, which the client accepts or
            // disconnects from.
            (Some("2026-07-28"), "2025-11-25"),
            // Older and unsupported: 2025-03-26 obliges a server to take
            // batches, so its client is answered as every client was before.
            (Some("2025-03-26"), "2024-11-05"),
            (Some("2024-10-07"), "2024-11-05"),
            (None, "2024-11-05"),
        ] {
            assert_eq!(
                negotiate_protocol_version(requested),
                answered,
                "{requested:?}"
            );
        }
    }

    /// Initialize a connection as a client asking for `version` would.
    fn initialized(mut config: McpServerConfig, version: &str) -> McpServerConfig {
        config.apply_client_capabilities(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": version, "capabilities": {},
                       "clientInfo": {"name": "structured-test", "version": "1"}}
        }));
        config
    }

    #[tokio::test]
    async fn structured_output_restates_each_verb_reply_under_its_output_schema() {
        let _environment = kin_core::test_env::EnvVarGuard::new();
        let (store, entities) = routed_fixture();
        let id_of = |name: &str| {
            entities
                .iter()
                .find(|entity| entity.name == name)
                .unwrap_or_else(|| panic!("the fixture has no {name}"))
                .id
                .to_string()
        };
        let (helper, caller) = (id_of("routed_helper"), id_of("routed_caller"));
        let config = initialized(
            McpServerConfig {
                session_authority_mode: SessionAuthorityMode::OfflineFallback,
                ..McpServerConfig::default()
            },
            "2025-06-18",
        );
        assert!(serves_structured_output(&config));
        let sessions = SessionRegistry::new();

        let listed = process_message(
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#,
            &store,
            &config,
            &sessions,
        )
        .await
        .unwrap()
        .result
        .unwrap();
        let schemas: HashMap<String, serde_json::Value> = listed["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|tool| {
                Some((
                    tool["name"].as_str()?.to_string(),
                    tool.get("outputSchema")?.clone(),
                ))
            })
            .collect();
        for verb in crate::verbs::VERBS {
            let schema = schemas
                .get(verb)
                .unwrap_or_else(|| panic!("{verb} is listed with no outputSchema"));
            assert_eq!(schema["type"], "object", "{verb}");
        }

        let mut answered = 0;
        for (verb, arguments) in [
            (
                crate::verbs::LOCATE,
                serde_json::json!({"query": "routed_helper"}),
            ),
            (
                crate::verbs::LOCATE,
                serde_json::json!({"query": "routed_helper()", "mode": "literal"}),
            ),
            (crate::verbs::TRACE, serde_json::json!({"focal": caller})),
            (
                crate::verbs::TRACE,
                serde_json::json!({"focal": caller, "to": helper}),
            ),
            (
                crate::verbs::NEIGHBORS,
                serde_json::json!({"entity_id": helper}),
            ),
            (
                crate::verbs::IMPACT,
                serde_json::json!({"entity_ids": [helper]}),
            ),
            (
                crate::verbs::HISTORY,
                serde_json::json!({"entity_id": helper}),
            ),
            (crate::verbs::REFS, serde_json::json!({"entity_id": helper})),
            (
                crate::verbs::CONTEXT,
                serde_json::json!({"entity_id": caller}),
            ),
            (
                crate::verbs::SOURCE,
                serde_json::json!({"entity_id": helper}),
            ),
            (crate::verbs::STATUS, serde_json::json!({})),
            (crate::verbs::INIT, serde_json::json!({})),
        ] {
            let request = tools_call(verb, arguments.clone());
            let response = process_message(&request, &store, &config, &sessions)
                .await
                .unwrap();
            let response = present_for_client(response, &request, &config);
            let result = response.result.expect("a tool result");
            let refused = result["isError"].as_bool() == Some(true);
            match result.get("structuredContent") {
                None => assert!(
                    refused,
                    "{verb} {arguments} answered with no structuredContent"
                ),
                Some(structured) => {
                    assert!(!refused, "{verb} {arguments} restated a refusal");
                    answered += 1;
                    let text: serde_json::Value =
                        serde_json::from_str(result["content"][0]["text"].as_str().unwrap())
                            .unwrap();
                    assert_eq!(structured, &text, "{verb}: the text carries the same JSON");
                    let problems = crate::schema_check::errors(&schemas[verb], structured);
                    assert!(
                        problems.is_empty(),
                        "{verb} {arguments} does not fit its outputSchema: {problems:?}"
                    );
                    crate::replies::decode(verb, structured).unwrap_or_else(|problem| {
                        panic!("{verb} does not decode as its typed reply: {problem}")
                    });
                }
            }
        }
        assert!(
            answered >= 6,
            "only {answered} verbs answered in the fixture"
        );
    }

    #[tokio::test]
    async fn plain_words_and_older_clients_are_served_no_structured_output() {
        let store = InMemoryGraph::default();
        let sessions = SessionRegistry::new();
        let agent = initialized(
            McpServerConfig {
                plain_replies: true,
                ..default_config()
            },
            "2025-11-25",
        );
        let older = initialized(McpServerConfig::default(), "2024-11-05");
        for config in [agent, older] {
            assert!(!serves_structured_output(&config));
            let listed = process_message(
                r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#,
                &store,
                &config,
                &sessions,
            )
            .await
            .unwrap()
            .result
            .unwrap();
            assert!(listed["tools"]
                .as_array()
                .unwrap()
                .iter()
                .all(|tool| tool.get("outputSchema").is_none()));
        }
    }

    #[tokio::test]
    async fn a_client_that_declares_it_only_reads_is_served_the_read_verbs() {
        let declare = |config: McpServerConfig, read_only: bool| {
            let mut config = config;
            config.apply_client_capabilities(&serde_json::json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {"protocolVersion": "2025-06-18",
                           "capabilities": {"experimental": {"kin": {"readOnly": read_only}}}}
            }));
            config
        };
        let names = |config: &McpServerConfig| -> std::collections::BTreeSet<String> {
            served_tools_for(config)
                .tools
                .into_iter()
                .map(|tool| tool.name)
                .collect()
        };
        // `agent-query` also lists the front door, which the default profile
        // does not serve until a benchmark gate on the default profile says
        // it should, so the default's query half is the rest of that list.
        let query: std::collections::BTreeSet<String> = crate::tools::agent_query_tool_names()
            .iter()
            .filter(|name| **name != crate::ask::TOOL_NAME)
            .map(|name| name.to_string())
            .collect();

        // The default profile narrows to exactly its query half, so the
        // listing costs no more than what `agent-query` is measured at.
        let reader = declare(default_config(), true);
        assert_eq!(names(&reader), query);
        let bytes = serde_json::to_string(&served_tools_for(&reader))
            .unwrap()
            .len();
        assert!(
            bytes <= crate::tools::AGENT_QUERY_LIST_CEILING_BYTES,
            "{bytes}"
        );
        // A client that declares nothing, or that it writes, keeps the profile.
        assert_eq!(
            names(&declare(default_config(), false)).len(),
            crate::tools::agent_default_tool_names().len()
        );

        // The full surface keeps every tool that reads, and no write.
        let full = declare(McpServerConfig::default(), true);
        let listed = served_tools_for(&full);
        assert!(listed.tools.len() > query.len());
        assert!(listed
            .tools
            .iter()
            .all(|tool| tool.annotations.read_only_hint));
        for write in [
            crate::verbs::MUTATE,
            crate::verbs::SESSION,
            crate::verbs::TRANSACTION,
        ] {
            assert!(
                !listed.tools.iter().any(|tool| tool.name == write),
                "{write}"
            );
        }

        // A write sent anyway is refused as a tool this connection does not
        // serve; the store's permissions are never consulted for it here.
        let store = InMemoryGraph::default();
        let refused = process_message(
            &tools_call(crate::verbs::MUTATE, serde_json::json!({"operations": []})),
            &store,
            &McpServerConfig {
                session_authority_mode: SessionAuthorityMode::OfflineFallback,
                ..reader
            },
            &SessionRegistry::new(),
        )
        .await
        .unwrap()
        .result
        .unwrap();
        assert!(refused["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("not enabled in this MCP profile"));

        // The routed surface loses its write path.
        let routed = declare(with_writes(), true);
        assert_eq!(routed.routed.map(|surface| surface.writes), Some(false));
    }

    /// The default profile's write path needs no `session start`: a `mutate`
    /// that names no session runs in the connection's own, opened on the first
    /// write and reused by the next. The daemon's session start is stood in for
    /// by the test's ids; what is proved is that the call reaches the write
    /// path carrying a session, and is never refused for lacking one.
    #[tokio::test]
    async fn a_write_on_the_default_profile_opens_the_connections_session() {
        let config = McpServerConfig {
            client_root: Some(PathBuf::from("/semantic-test-context")),
            allowed_tools: Some(crate::tools::name_set(
                crate::tools::agent_default_tool_names(),
            )),
            agent_belt: true,
            ..McpServerConfig::default()
        };
        assert!(config.session_authority_mode.requires_daemon());
        config
            .connection_session
            .open_with(&["connection-session-1", "connection-session-2"]);
        let listed: Vec<String> = served_tools_for(&config)
            .tools
            .into_iter()
            .map(|tool| tool.name)
            .collect();
        assert!(
            !listed.iter().any(|name| name == crate::verbs::SESSION),
            "the default profile lists no session tool: {listed:?}"
        );
        assert!(
            !listed.iter().any(|name| name == crate::verbs::EXEC),
            "operator execution is not a public capability: {listed:?}"
        );

        let edit = guarded_update_operation("pub fn widget() {}");
        for id in [21, 22] {
            let answer = process_daemon_message(
                &serde_json::json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{
                    "name": crate::verbs::MUTATE, "arguments": {"operations": [edit.clone()]}
                }})
                .to_string(),
                &config,
            )
            .await
            .unwrap()
            .result
            .expect("the write is answered");
            let text = answer["content"][0]["text"].as_str().unwrap_or_default();
            assert!(
                !text.contains("Missing required parameter: 'session_id'")
                    && !text.contains("needs a session"),
                "a default-profile write was refused for its session: {text}"
            );
        }
        let held = config.connection_session.current().await;
        assert!(
            matches!(
                held.as_deref(),
                Some("connection-session-1" | "connection-session-2")
            ),
            "{held:?}"
        );
        // One open for the first write, reused by the second, unless a daemon
        // this machine happens to run reaped the stand-in id and it was
        // reopened once.
        assert!(
            (1..=2).contains(&config.connection_session.test_opens()),
            "{}",
            config.connection_session.test_opens()
        );
    }
}

#[cfg(test)]
mod minimal_query_client_tests {
    use super::*;
    use serde_json::json;

    fn response(payload: serde_json::Value) -> JsonRpcResponse {
        JsonRpcResponse::success(
            Some(json!(1)),
            json!({"content":[{"type":"text","text":payload.to_string()}]}),
        )
    }

    fn refs() -> serde_json::Value {
        json!({"focal_entity":{"id":"target","name":"Cache.__setitem__","file":"cache.py","signature":"def __setitem__(self, key, value)"},
            "references":[{"entity_id":"caller","name":"audit_manifest","file":"publisher.py","kind":"Function",
                "relation_kinds":["calls"],"resolution":"type_resolved","sites":[{"expr":"cache[key] = value","line_in_entity":1}]}],
            "relation_kinds":["calls"],"total_upstream":1,"next_cursor":"opaque",
            "_kin":{"verdict":{"state":"inconclusive","limiting_factor":"call_sites_owed: more callers could be unchecked","safe_to_conclude_absent":false}}})
    }

    fn body(response: &JsonRpcResponse) -> serde_json::Value {
        serde_json::from_str(
            response.result.as_ref().unwrap()["content"][0]["text"]
                .as_str()
                .unwrap(),
        )
        .unwrap()
    }

    fn request(name: &str, arguments: serde_json::Value) -> String {
        json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":arguments}}).to_string()
    }

    fn query() -> McpServerConfig {
        McpServerConfig {
            agent_belt: true,
            plain_replies: true,
            allowed_tools: Some(crate::tool_name_set(crate::agent_query_tool_names())),
            ..McpServerConfig::default()
        }
    }

    #[test]
    fn named_routed_and_discovered_query_surfaces_share_minimal_and_detail_modes() {
        for details in [false, true] {
            let args = json!({"query":"Cache.__setitem__","details":details});
            let calls = [
                (query(), request("refs", args.clone())),
                (
                    McpServerConfig {
                        routed: Some(crate::routed::RoutedSurface::READ_ONLY),
                        allowed_tools: Some(crate::tool_name_set(crate::agent_routed_tool_names())),
                        ..query()
                    },
                    request("kin", json!({"command":"refs","args":args.clone()})),
                ),
                (
                    McpServerConfig {
                        allowed_tools: Some(crate::tool_name_set(crate::agent_search_tool_names())),
                        ..query()
                    },
                    request("kin_tool_call", json!({"tool":"refs","arguments":args})),
                ),
            ];
            let mut expected = None;
            for (config, req) in calls {
                let served = present_for_client(response(refs()), &req, &config);
                let answer = body(&served);
                assert!(answer["references"][0].get("file").is_none());
                assert_eq!(answer["references"][0]["entity_id"], "caller");
                assert_eq!(
                    answer["references"][0]["sites"].is_array(),
                    details,
                    "{req}"
                );
                assert_eq!(answer["entity"]["signature"].is_string(), details, "{req}");
                assert_eq!(answer["_kin"]["verdict"], "partial");
                if let Some(expected) = &expected {
                    assert_eq!(&answer, expected, "{req}");
                } else {
                    expected = Some(answer);
                }
            }
        }
    }

    #[test]
    fn mcp_continuations_inherit_details_and_separate_cli_calls_can_repeat_them() {
        let config = query();
        present_for_client(
            response(refs()),
            &request("refs", json!({"query":"Cache.__setitem__","details":true})),
            &config,
        );
        let mcp = present_for_client(
            response(refs()),
            &request("refs", json!({"cursor":"opaque"})),
            &config,
        );
        assert!(body(&mcp)["references"][0]["sites"].is_array());
        // `kin call` uses this same presenter but starts a new config per
        // process. Repeating the flag restores the exact inherited view.
        let cli = present_for_client(
            response(refs()),
            &request("refs", json!({"cursor":"opaque","details":true})),
            &query(),
        );
        assert_eq!(body(&cli), body(&mcp));
        let fresh = present_for_client(
            response(refs()),
            &request("refs", json!({"cursor":"opaque"})),
            &query(),
        );
        assert!(body(&fresh)["references"][0].get("sites").is_none());
        let changed = present_for_client(
            response(refs()),
            &request("refs", json!({"cursor":"opaque","details":false})),
            &config,
        );
        assert_eq!(body(&changed), body(&fresh));
    }

    #[test]
    fn descriptive_locate_continuations_keep_their_detail_mode() {
        let config = query();
        let machine = json!({"query":"stores a cached value", "entities":[{"id":"target","name":"Cache.__setitem__","file":"cache.py","signature":"def __setitem__(self, key, value)","line":4}],
            "next_cursor":"opaque-ranked", "ranked_by":"entity cards and the graph", "total_ranked":1,
            "_kin":{"verdict":{"state":"certified","safe_to_conclude_absent":false}}});
        present_for_client(
            response(machine.clone()),
            &request(
                "locate",
                json!({"query":"stores a cached value","details":true}),
            ),
            &config,
        );
        let continued = present_for_client(
            response(machine),
            &request("locate", json!({"cursor":"opaque-ranked"})),
            &config,
        );
        assert!(body(&continued)["results"][0]["signature"].is_string());
    }

    #[test]
    fn full_machine_and_citable_queries_preserve_semantic_evidence() {
        let mut machine = query();
        machine.apply_client_capabilities(
            &json!({"params":{"capabilities":{"experimental":{"kin":{"machineReplies":true}}}}}),
        );
        for config in [
            McpServerConfig::default(),
            McpServerConfig {
                citable: true,
                ..query()
            },
            machine,
        ] {
            let served = present_for_client(
                response(refs()),
                &request("refs", json!({"query":"Cache.__setitem__"})),
                &config,
            );
            let public = body(&served);
            assert_eq!(public["focal_entity"]["id"], "target");
            assert_eq!(public["references"][0]["entity_id"], "caller");
            assert_eq!(
                public["references"][0]["sites"],
                refs()["references"][0]["sites"]
            );
            // A paged answer's verdict is the frozen whole's, so the page
            // carries it unchanged and no account of its own.
            assert_eq!(public["_kin"]["verdict"], refs()["_kin"]["verdict"]);
            // An answer delivered whole carries its account beside the
            // machine verdict, on every presentation that is not citable.
            let mut whole = refs();
            whole.as_object_mut().unwrap().remove("next_cursor");
            let served_whole = present_for_client(
                response(whole.clone()),
                &request("refs", json!({"query":"Cache.__setitem__"})),
                &config,
            );
            let mut verdict = body(&served_whole)["_kin"]["verdict"].clone();
            let answer = verdict.as_object_mut().unwrap().remove("answer");
            assert_eq!(verdict, whole["_kin"]["verdict"]);
            if config.citable {
                assert_eq!(answer, None);
            } else {
                assert_eq!(
                    answer,
                    Some(json!({"standing": "partial", "open": ["call_sites_owed"]}))
                );
            }
            assert!(public["focal_entity"].get("file").is_none());
            assert!(public["references"][0].get("file").is_none());
            if let Some(structured) = served.result.as_ref().unwrap().get("structuredContent") {
                assert_eq!(structured, &public);
            }
        }
    }

    #[test]
    fn literal_status_and_errors_keep_the_existing_plain_contract() {
        for (name, args, machine) in [
            (
                "locate",
                json!({"query":"cache[key]","mode":"literal"}),
                json!({"literal":"cache[key]","matches":[],"_kin":{"verdict":{"state":"certified","safe_to_conclude_absent":false}}}),
            ),
            (
                "status",
                json!({}),
                json!({"entity_count":3,"relation_count":2,"_kin":{}}),
            ),
        ] {
            let req = request(name, args.clone());
            let effective =
                crate::verbs::handler_name(name, &serde_json::from_value(args).unwrap());
            let mut expected = response(machine.clone());
            crate::plain_reply::present(expected.result.as_mut().unwrap(), &effective);
            let served = present_for_client(response(machine), &req, &query());
            assert_eq!(body(&served), body(&expected), "{req}");
        }
        let result = json!({"isError":true,"content":[{"type":"text","text":json!({"error":"bad cursor","remediation":"refetch refs","_kin":{"verdict":{"state":"inconclusive"}}}).to_string()}]});
        let mut expected = result.clone();
        crate::plain_reply::present(&mut expected, "find_references");
        let served = present_for_client(
            JsonRpcResponse::success(Some(json!(1)), result),
            &request("refs", json!({"cursor":"stale","details":true})),
            &query(),
        );
        assert_eq!(served.result.unwrap(), expected);
    }
}

#[cfg(test)]
mod compact_trace_client_tests {
    use super::*;
    use crate::trace_compact::tests::{body, expand, result};
    use serde_json::json;

    fn fixture() -> serde_json::Value {
        let mut value = crate::trace_compact::tests::fixture();
        for row in value["reach"]["frontier"].as_array_mut().unwrap() {
            row.as_object_mut().unwrap().remove("file");
            row.as_object_mut().unwrap().remove("line");
        }
        for row in value["chain"].as_array_mut().unwrap() {
            for site in row["sites"].as_array_mut().unwrap() {
                site.as_object_mut().unwrap().remove("file");
                site.as_object_mut().unwrap().remove("line");
            }
        }
        value["_kin"]["semantic_surface"] = json!(1);
        value
    }

    fn response() -> JsonRpcResponse {
        JsonRpcResponse::success(Some(json!(1)), result(&fixture()))
    }
    fn request(name: &str, arguments: serde_json::Value) -> String {
        json!({"jsonrpc":"2.0", "id":1, "method":"tools/call", "params":{"name":name,"arguments":arguments}}).to_string()
    }
    fn arguments() -> serde_json::Value {
        json!({"focal":"00000000-0000-0000-0000-000000000001", "direction":"callers", "depth":8})
    }
    fn query() -> McpServerConfig {
        McpServerConfig {
            agent_belt: true,
            plain_replies: true,
            compact_trace_evidence: true,
            allowed_tools: Some(crate::tool_name_set(crate::agent_query_tool_names())),
            ..McpServerConfig::default()
        }
    }

    #[test]
    fn compact_query_named_routed_and_discovered_calls_expand_the_same_proof() {
        let named = query();
        let routed = McpServerConfig {
            routed: Some(crate::routed::RoutedSurface::READ_ONLY),
            allowed_tools: Some(crate::tool_name_set(crate::agent_routed_tool_names())),
            ..query()
        };
        let search = McpServerConfig {
            allowed_tools: Some(crate::tool_name_set(crate::agent_search_tool_names())),
            ..query()
        };
        for (config, req) in [
            (named, request("trace", arguments())),
            (
                routed,
                request("kin", json!({"command":"trace", "args":arguments()})),
            ),
            (
                search,
                request(
                    "kin_tool_call",
                    json!({"tool":"trace", "arguments":arguments()}),
                ),
            ),
        ] {
            let response = present_for_client(response(), &req, &config);
            let encoded = body(response.result.as_ref().unwrap());
            assert!(encoded.get("trace_encoding").is_some(), "{req}");
            assert_eq!(expand(encoded), fixture());
        }
    }

    #[test]
    fn default_full_citable_and_machine_capability_leave_representation_unchanged() {
        let mut machine = query();
        machine.apply_client_capabilities(
            &json!({"params":{"capabilities":{"experimental":{"kin":{"machineReplies":true}}}}}),
        );
        assert!(machine.compact_trace_evidence && !machine.plain_replies);
        for config in [
            McpServerConfig {
                compact_trace_evidence: false,
                ..query()
            },
            McpServerConfig {
                plain_replies: false,
                compact_trace_evidence: false,
                ..query()
            },
            McpServerConfig {
                citable: true,
                ..query()
            },
            McpServerConfig {
                plain_replies: false,
                ..query()
            },
            machine,
        ] {
            let before = response();
            let served = present_for_client(response(), &request("trace", arguments()), &config);
            assert_eq!(
                serde_json::to_value(served).unwrap(),
                serde_json::to_value(before).unwrap()
            );
        }
    }

    #[test]
    fn read_only_does_not_opt_default_in_and_exact_bodies_do_not_opt_query_out() {
        let mut default = McpServerConfig {
            plain_replies: true,
            agent_belt: true,
            allowed_tools: Some(crate::tool_name_set(crate::agent_default_tool_names())),
            ..McpServerConfig::default()
        };
        default.serve_reads_only();
        assert!(!default.compact_trace_evidence);
        let mut exact = query();
        exact.number_entity_lines = true;
        exact.serve_exact_entity_bodies();
        assert!(exact.compact_trace_evidence && !exact.number_entity_lines);
        let served = present_for_client(response(), &request("trace", arguments()), &exact);
        assert!(body(served.result.as_ref().unwrap())
            .get("trace_encoding")
            .is_some());
    }

    #[test]
    fn trace_path_other_tools_and_refused_wrappers_do_not_receive_aliases() {
        let routed = McpServerConfig {
            routed: Some(crate::routed::RoutedSurface::READ_ONLY),
            ..query()
        };
        for (config, req) in [
            (query(), request("trace_path", json!({"from":"a","to":"b"}))),
            (query(), request("refs", arguments())),
            (
                routed,
                request("kin", json!({"command":"trace", "args":{"invalid":true}})),
            ),
        ] {
            let before = response();
            let served = present_for_client(response(), &req, &config);
            assert_eq!(
                serde_json::to_value(served).unwrap(),
                serde_json::to_value(before).unwrap()
            );
        }
    }

    #[test]
    fn profile_initialize_instructions_and_schema_are_unchanged_by_codec_opt_in() {
        let mut off = query();
        off.compact_trace_evidence = false;
        assert_eq!(instructions_for(&off), instructions_for(&query()));
        assert_eq!(
            serde_json::to_value(served_tools_for(&off)).unwrap(),
            serde_json::to_value(served_tools_for(&query())).unwrap()
        );
    }
}
