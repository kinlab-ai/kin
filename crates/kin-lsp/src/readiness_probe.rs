// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The startup probe: whether a language is actually served on this host.
//!
//! A probe starts a server, waits for its initialize handshake and drops it.
//! It sends no question and records nothing, so nothing here decides what an
//! answer proves, and this file is outside the enrichment producer identity
//! (see `build/producer_identity.rs`). How long a probe waits can then change
//! without every stored proof being asked about again.

use std::path::Path;
use std::time::Duration;

use kin_model::LanguageId;

use crate::adapters::{LspAdapter, ServerLaunch};
use crate::lifecycle::LspServer;
use crate::registry::{
    BinaryFinder, ProviderGap, ProviderGapReason, ProviderProbe, ProviderRegistry,
    SystemBinaryFinder,
};

/// How long a readiness probe waits for a server to complete the handshake.
///
/// Deliberately far below the 30 s ceiling [`LspServer::start`] allows an
/// enrichment run. A probe answers an operator or a startup path, and a
/// surface that blocks for half a minute to render one status row has traded
/// one bad answer for a worse experience.
pub const READINESS_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a readiness probe waits when it is a repeat of one whose server
/// was started and did not finish the handshake.
///
/// Running out of [`READINESS_PROBE_TIMEOUT`] does not show a server
/// unusable: on a loaded host a working server's handshake has been seen to
/// take longer than that limit. A repeat runs in the background, where no
/// status row waits on it, so it is given the ceiling [`LspServer::start`]
/// allows an enrichment run.
pub const READINESS_REPEAT_TIMEOUT: Duration = Duration::from_secs(30);

/// Ask whether a language is actually served on this host, and answer with one
/// of three states rather than two.
///
/// `Ok` is usable: the server resolved, completed the initialize handshake, and
/// reported the capabilities in the returned [`ProviderProbe`]. `Err` is a
/// [`ProviderGap`] whose reason separates the two ways a language can fail to
/// be served: no binary at all, versus a binary that is present and refuses to
/// initialize. Those need different repairs, which is why the caller is given
/// the distinction instead of a boolean.
///
/// This exists because a binary on `PATH` is not a working language server.
/// Resolution alone answered the weaker question, and a host whose server was
/// installed but unusable was reported as served by every surface that asked.
///
/// It spawns a process, so it belongs to lifecycle paths that may spawn
/// (daemon start, install verification, a diagnostic command) and never to a
/// query path.
pub async fn probe_readiness(
    registry: &ProviderRegistry,
    language: LanguageId,
    workspace_root: &Path,
    initialization_options: Option<serde_json::Value>,
) -> std::result::Result<ProviderProbe, ProviderGap> {
    probe_readiness_with(
        registry,
        language,
        workspace_root,
        initialization_options,
        &SystemBinaryFinder,
    )
    .await
}

/// [`probe_readiness`] with an injected [`BinaryFinder`], so the three states
/// can be exercised against fixture servers instead of whatever the host
/// happens to have installed.
pub async fn probe_readiness_with(
    registry: &ProviderRegistry,
    language: LanguageId,
    workspace_root: &Path,
    initialization_options: Option<serde_json::Value>,
    finder: &dyn BinaryFinder,
) -> std::result::Result<ProviderProbe, ProviderGap> {
    let resolved = registry.resolve_with(language, finder)?;
    let command = resolved.command.display().to_string();
    let args: Vec<&str> = resolved.args.iter().map(String::as_str).collect();

    // The handshake limit is enforced inside the launch, so a server that
    // runs it out still has its stderr read into the reason.
    let started = LspServer::launch_within(
        &command,
        &args,
        workspace_root,
        &ServerLaunch::with_initialization_options(initialization_options),
        // A probe only checks readiness and never enriches, so it proves no
        // TypeScript binding initializer and needs no grammar.
        None,
        READINESS_PROBE_TIMEOUT,
    )
    .await;

    match started {
        Err(error) => Err(ProviderGap {
            language,
            reason: ProviderGapReason::ServerUnusable {
                message: error.to_string(),
            },
            tried: vec![resolved.id.clone()],
        }),
        Ok(server) => {
            let probed_capabilities = server.probed_capabilities();
            // Dropped rather than shut down politely: a drop ends the server's
            // whole process group at once, and a probe has no session worth
            // closing.
            drop(server);
            Ok(ProviderProbe {
                resolved,
                probed_capabilities,
            })
        }
    }
}

/// What one adapter's server can do on this host, as [`probe_adapter`]
/// finds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdapterProbe {
    /// It started and completed the initialize handshake.
    Usable,
    /// Its program is not on `PATH`. The reason names it and where it was
    /// looked for.
    NotInstalled { reason: String },
    /// Its program is there and the server did not start. The reason is the
    /// whole of why (see [`LspServer::launch_within`]).
    Unusable { reason: String },
}

/// Start `adapter`'s server for `workspace_root` with the launch the adapter
/// configures for it, and wait up to `limit` for its handshake.
///
/// The probe a surface runs when it must answer on its own, without a
/// daemon. It starts what the daemon starts, with the same configuration, so
/// the two reach the same verdict: a TypeScript server given its TypeScript
/// by the launch starts here exactly as it starts in a sweep, where a probe
/// started without the launch failed where the daemon succeeded.
pub async fn probe_adapter(
    adapter: &dyn LspAdapter,
    workspace_root: &Path,
    limit: Duration,
    finder: &dyn BinaryFinder,
) -> AdapterProbe {
    let command = adapter.server_command();
    let Some(program) = finder.find_on_path(command) else {
        return AdapterProbe::NotInstalled {
            reason: format!("no `{command}` on PATH (looked in {})", finder.searched()),
        };
    };
    let launch = adapter.launch(workspace_root);
    let args = adapter.server_args();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match LspServer::launch_within(
        &program.to_string_lossy(),
        &args,
        workspace_root,
        &launch,
        None,
        limit,
    )
    .await
    {
        Ok(server) => {
            drop(server);
            AdapterProbe::Usable
        }
        Err(error) => AdapterProbe::Unusable {
            reason: error.to_string(),
        },
    }
}
