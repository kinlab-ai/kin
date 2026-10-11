// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Deriving a partial store's Git history, in the foreground or behind it.
//!
//! A store `kin init --history background` created holds the checked-out
//! commit and answers at once. Its history is derived afterwards by a worker
//! process that runs the complete admission ladder into a stage beside the
//! repository, at low priority and with a bounded share of the machine, and
//! then attaches the staged store once no daemon is serving the partial one.
//! `kin init --history full` over a partial store does the same work in the
//! foreground and attaches at once.
//!
//! The worker is a separate process on purpose. The ladder's peak memory is a
//! conversion's, which is far larger than a serving daemon's, and holding it
//! inside the daemon would count against the daemon's own memory budget and
//! share its threads. A process of its own can be niced, capped, killed and
//! restarted without touching what serves queries.
//!
//! Resuming after a restart works at two grains. A derivation stopped part way
//! through the ladder starts the ladder again, because the ladder holds its
//! work in memory and proves it as one unit. A derivation that finished staging
//! is never repeated: the stage is recorded in the progress file, and the next
//! worker or the next daemon start attaches it.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use kin_core::history_attach::{self, AttachOutcome};
use kin_core::history_state::{
    self, DerivationState, HistoryDerivationProgress, HistoryMode, HistoryStanding,
};
use kin_core::KinLayout;

/// The lock one derivation holds for its whole life, so two never run for one
/// store.
const WORKER_LOCK: &str = "history-derivation.lock";
/// The worker's own log, so a background failure is readable after the fact.
const WORKER_LOG: &str = "history-derivation.log";

/// Threads a background derivation may use: a quarter of this machine's, and
/// at least one.
pub(crate) fn background_threads() -> usize {
    std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1)
        .div_ceil(4)
        .max(1)
}

/// Start the background worker for the store at `layout`, detached, and say
/// whether one was started.
///
/// The worker takes the store's derivation lock itself and exits at once when
/// another worker holds it, so starting one is idempotent.
pub(crate) fn spawn_background_worker(layout: &KinLayout) -> Result<Option<u32>> {
    let standing = history_state::standing(layout);
    let HistoryStanding::Partial { record, .. } = &standing else {
        return Ok(None);
    };
    if record.mode != HistoryMode::Background || record.underivable.is_some() {
        return Ok(None);
    }
    let program = std::env::current_exe().context("find this kin binary")?;
    spawn_worker_with(&program, layout)
}

/// [`spawn_background_worker`] with the `kin` binary named, for a caller that
/// is not `kin` itself.
pub fn spawn_worker_with(program: &Path, layout: &KinLayout) -> Result<Option<u32>> {
    let log_path = layout.logs_dir().join(WORKER_LOG);
    std::fs::create_dir_all(layout.logs_dir())
        .with_context(|| format!("create {}", layout.logs_dir().display()))?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("open {}", log_path.display()))?;
    let mut command = std::process::Command::new(program);
    command
        .arg("derive-history")
        .arg(layout.working_dir())
        .arg("--background")
        .current_dir(layout.working_dir())
        .env("RAYON_NUM_THREADS", background_threads().to_string())
        .env_remove("KIN_DAEMON_URL")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(log);
    // No memory ceiling is passed. The worker judges itself against its share
    // of the machine once it knows it is a background worker, and an
    // operator's own `KIN_INIT_MEMORY_CEILING_BYTES` reaches it unchanged.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        // Its own process group, so a terminal's interrupt meant for the
        // command that started it does not reach it.
        command.process_group(0);
    }
    let child = command
        .spawn()
        .with_context(|| format!("start {} derive-history", program.display()))?;
    Ok(Some(child.id()))
}

/// Lower this process to background priority: the lowest CPU niceness, and on
/// macOS the utility disk-I/O tier, below anything interactive.
///
/// Not macOS's background band. That band also throttles disk I/O to a crawl,
/// and a derivation is mostly small reads and writes: measured on psf/requests
/// on a loaded machine, its first phase took 30 seconds in the band against
/// 0.1 seconds out of it, so the history would never have arrived. Linux
/// derives a process's I/O priority from its niceness by default, so niceness
/// alone is the same trade there.
fn lower_own_priority() {
    #[cfg(unix)]
    // SAFETY: setpriority takes plain integers and only affects this process.
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, 0, 19);
    }
    #[cfg(target_os = "macos")]
    {
        // From <sys/resource.h>; the libc crate does not carry these.
        const IOPOL_TYPE_DISK: libc::c_int = 0;
        const IOPOL_SCOPE_PROCESS: libc::c_int = 0;
        const IOPOL_UTILITY: libc::c_int = 4;
        extern "C" {
            fn setiopolicy_np(
                iotype: libc::c_int,
                scope: libc::c_int,
                policy: libc::c_int,
            ) -> libc::c_int;
        }
        // SAFETY: a documented libSystem call taking plain integers; it only
        // changes this process's own disk I/O policy.
        unsafe {
            setiopolicy_np(IOPOL_TYPE_DISK, IOPOL_SCOPE_PROCESS, IOPOL_UTILITY);
        }
    }
}

/// The store `dir` holds itself, never an enclosing one.
fn own_layout(dir: &Path) -> Result<KinLayout> {
    kin_core::layout::locate_store(dir)
        .ok_or_else(|| anyhow!("{} holds no Kin store", dir.display()))
}

/// Holds a store's derivation lock until dropped.
struct DerivationLock {
    _file: std::fs::File,
}

fn try_lock(layout: &KinLayout, wait: bool) -> Result<Option<DerivationLock>> {
    use fs2::FileExt as _;
    let path = layout.root().join(WORKER_LOCK);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    if wait {
        file.lock_exclusive()
            .with_context(|| format!("lock {}", path.display()))?;
        return Ok(Some(DerivationLock { _file: file }));
    }
    match file.try_lock_exclusive() {
        Ok(()) => Ok(Some(DerivationLock { _file: file })),
        Err(_) => Ok(None),
    }
}

/// Write progress, never failing the derivation for it.
fn report(layout: &KinLayout, progress: &HistoryDerivationProgress) {
    if let Err(error) = history_state::write_progress(layout, progress) {
        tracing::warn!(%error, "could not record history derivation progress");
    }
}

/// A stage a previous attempt finished, still on disk.
fn finished_stage(progress: Option<&HistoryDerivationProgress>) -> Option<PathBuf> {
    let progress = progress?;
    if progress.state != DerivationState::Staged {
        return None;
    }
    let stage = PathBuf::from(progress.detail.as_deref()?);
    history_attach::staged_store(&stage)
        .join("manifest.json")
        .is_file()
        .then_some(stage)
}

/// How a derivation ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DerivationOutcome {
    /// The store was already complete.
    AlreadyComplete,
    /// Another worker holds the lock.
    AlreadyRunning,
    /// The store asks for no derivation, or cannot have one.
    NotAsked(String),
    /// Derived and staged; attached when no daemon serves the store.
    Staged { stage: PathBuf },
}

/// Derive the store's complete history into a stage, reusing a stage an
/// earlier attempt finished.
///
/// `background` lowers this process's priority first and never waits for a
/// lock another derivation holds.
fn derive_to_stage(
    layout: &KinLayout,
    background: bool,
    say: &dyn Fn(&str),
) -> Result<(DerivationOutcome, Option<DerivationLock>)> {
    let record = match history_state::standing(layout) {
        HistoryStanding::Complete => return Ok((DerivationOutcome::AlreadyComplete, None)),
        HistoryStanding::Unreadable { reason } => bail!(
            "this store's history record cannot be read ({reason}), so its history cannot be \
             derived safely"
        ),
        HistoryStanding::Partial { record, .. } => record,
    };
    if let Some(reason) = &record.underivable {
        return Ok((DerivationOutcome::NotAsked(reason.clone()), None));
    }
    if background && record.mode != HistoryMode::Background {
        return Ok((
            DerivationOutcome::NotAsked(format!(
                "this store records history mode {}, which derives nothing in the background",
                record.mode
            )),
            None,
        ));
    }
    let Some(lock) = try_lock(layout, !background)? else {
        return Ok((DerivationOutcome::AlreadyRunning, None));
    };
    // Read again under the lock: a foreground derivation may have waited out a
    // background one that staged or attached the history meanwhile.
    let progress = match history_state::standing(layout) {
        HistoryStanding::Complete => return Ok((DerivationOutcome::AlreadyComplete, Some(lock))),
        HistoryStanding::Partial { progress, .. } => progress,
        HistoryStanding::Unreadable { reason } => bail!(
            "this store's history record cannot be read ({reason}), so its history cannot be \
             derived safely"
        ),
    };
    if background {
        lower_own_priority();
        // A third of the machine, spent out of what is free, so this
        // derivation, the daemon it feeds and the operator's own work all fit.
        kin_core::init_budget::spend_a_share_of_the_machine(
            kin_core::init_budget::BACKGROUND_SHARE_DIVISOR,
        );
    }
    if let Some(stage) = finished_stage(progress.as_ref()) {
        return Ok((DerivationOutcome::Staged { stage }, Some(lock)));
    }

    let manifest = kin_core::KinManifest::load(&layout.manifest_path())?;
    let repository_id = kin_model::RepositoryId::new(manifest.repo_id.clone())
        .map_err(|error| anyhow!("invalid repository identity: {error}"))?;
    let working_dir = layout.working_dir().to_path_buf();
    history_attach::reap_abandoned_stages(&working_dir, &manifest.repo_id, None);
    let stage = history_attach::new_stage_parent(&working_dir, &manifest.repo_id)?;

    let started = chrono::Utc::now();
    let attempts = progress.as_ref().map_or(0, |progress| progress.attempts) + 1;
    let running = move |step: Option<(usize, usize)>| {
        let mut progress = HistoryDerivationProgress::new(DerivationState::Running);
        progress.attempts = attempts;
        progress.pid = Some(std::process::id());
        progress.started_at = Some(started);
        progress.commits_total = record.reachable_commits;
        if let Some((step, steps)) = step {
            progress.step = u32::try_from(step).ok();
            progress.steps = u32::try_from(steps).ok();
            progress.phase = kin_core::COMPLETE_ADMISSION_PHASE_LABELS
                .get(step.saturating_sub(1))
                .map(|label| kin_core::admission_phase_shown(label).to_string());
        }
        progress
    };
    report(layout, &running(None));
    say("Deriving the full Git history (the store keeps answering meanwhile)");

    // The ladder reports each step it enters; each becomes a progress record
    // every history answer can quote.
    let observer: kin_core::AdmissionStepObserver = {
        let layout = layout.clone();
        let running = running.clone();
        std::sync::Arc::new(move |step, steps| report(&layout, &running(Some((step, steps)))))
    };
    kin_core::compact_admission_progress(Some(observer));
    let derived = kin_core::derive_complete_history_into_stage(
        &working_dir,
        layout.root(),
        &repository_id,
        &stage,
    );
    kin_core::compact_admission_progress(None);
    match derived {
        Ok(_) => {
            let mut staged = HistoryDerivationProgress::new(DerivationState::Staged);
            staged.attempts = attempts;
            staged.started_at = Some(started);
            staged.commits_total = record.reachable_commits;
            staged.detail = Some(stage.display().to_string());
            report(layout, &staged);
            Ok((DerivationOutcome::Staged { stage }, Some(lock)))
        }
        Err(error) => {
            let _ = std::fs::remove_dir_all(&stage);
            let mut failed = HistoryDerivationProgress::new(DerivationState::Failed);
            failed.attempts = attempts;
            failed.started_at = Some(started);
            failed.detail = Some(first_line(&error.to_string()));
            report(layout, &failed);
            Err(anyhow!(error).context("derive this repository's Git history"))
        }
    }
}

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or(text).trim().to_string()
}

/// Attach a staged store, stopping the daemon first when `stop_daemon`, and
/// otherwise only when no daemon serves the store.
async fn attach(layout: &KinLayout, stage: &Path, stop_daemon: bool) -> Result<bool> {
    if stop_daemon {
        super::daemon::stop_current_repo_quiet(layout.root())
            .await
            .context("stop this repository's daemon before attaching its history")?;
    } else if crate::daemon_client::resolve_daemon_url_if_running_async(layout)
        .await
        .is_some()
    {
        return Ok(false);
    }
    let budget = if stop_daemon {
        kin_daemon_spawn::REPOSITORY_RUNTIME_AUTHORITY_RETRY_BUDGET
    } else {
        std::time::Duration::ZERO
    };
    let Some(runtime) =
        crate::daemon_client::acquire_repository_runtime_authority_within(layout.root(), budget)
            .with_context(|| {
                format!(
                    "acquire repository runtime authority for {}",
                    layout.root().display()
                )
            })?
    else {
        if stop_daemon {
            bail!(
                "another Kin process holds this repository's runtime authority ({}), so its \
                 history cannot be attached now; run `kin daemon stop`, then `kin init --history \
                 full` again",
                layout.root().display()
            );
        }
        return Ok(false);
    };
    let outcome = history_attach::attach_staged_history(layout, stage);
    drop(runtime);
    match outcome? {
        AttachOutcome::Attached | AttachOutcome::AlreadyComplete => Ok(true),
    }
}

/// `kin derive-history [PATH] [--background]`: the worker.
///
/// Exit 0 whenever the store is left in a state its own records describe,
/// including a derivation another worker is running, and 1 when the ladder
/// failed, which the progress record says too.
pub async fn run_worker(path: Option<String>, background: bool) -> Result<i32> {
    let dir = path
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().expect("cannot determine current directory"));
    let layout = own_layout(&dir)?;
    let say = |line: &str| {
        let _ = writeln!(std::io::stderr(), "{line}");
    };
    // Blocking work on a runtime thread would stall the runtime; the ladder is
    // one long blocking call.
    let derive_layout = layout.clone();
    let (outcome, lock) = tokio::task::spawn_blocking(move || {
        derive_to_stage(&derive_layout, background, &|line| {
            let _ = writeln!(std::io::stderr(), "{line}");
        })
    })
    .await
    .map_err(|error| anyhow!("history derivation stopped: {error}"))??;
    match outcome {
        DerivationOutcome::Staged { stage } => {
            if attach(&layout, &stage, !background).await? {
                let _ = history_state::clear_progress(&layout);
                drop(lock);
                say("Attached the full Git history; this store's history is complete");
                prepare_attached_store(&layout);
            } else {
                drop(lock);
                say(
                    "Derived the full Git history; it replaces this store when its daemon is \
                     next idle",
                );
            }
            Ok(0)
        }
        DerivationOutcome::AlreadyComplete => {
            say("This store's history is already complete");
            Ok(0)
        }
        DerivationOutcome::AlreadyRunning => {
            say("A history derivation is already running for this store");
            Ok(0)
        }
        DerivationOutcome::NotAsked(reason) => {
            say(&format!("No history derivation ran: {reason}"));
            Ok(0)
        }
    }
}

/// What a foreground derivation leaves a person to read.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct ForegroundDerivation {
    /// `complete`, `staged` or `unchanged`.
    pub history: &'static str,
    pub detail: String,
}

/// Derive and attach in the foreground, for `kin init --history full` over a
/// partial store. Stops the daemon only for the attach itself.
pub(crate) async fn derive_now(layout: &KinLayout, quiet: bool) -> Result<ForegroundDerivation> {
    let derive_layout = layout.clone();
    let (outcome, lock) = tokio::task::spawn_blocking(move || {
        derive_to_stage(&derive_layout, false, &|line| {
            if !quiet {
                let _ = writeln!(std::io::stderr(), "{line}");
            }
        })
    })
    .await
    .map_err(|error| anyhow!("history derivation stopped: {error}"))??;
    match outcome {
        DerivationOutcome::Staged { stage } => {
            let attached = attach(layout, &stage, true).await?;
            drop(lock);
            if !attached {
                bail!("the full history was derived but could not be attached");
            }
            let _ = history_state::clear_progress(layout);
            prepare_attached_store(layout);
            Ok(ForegroundDerivation {
                history: "complete",
                detail: "Attached the full Git history; this store's history is complete. The \
                         next daemon re-links cross-file references and rebuilds the search \
                         index for it"
                    .to_string(),
            })
        }
        DerivationOutcome::AlreadyComplete => Ok(ForegroundDerivation {
            history: "complete",
            detail: "This store's history is already complete".to_string(),
        }),
        DerivationOutcome::AlreadyRunning => Ok(ForegroundDerivation {
            history: "unchanged",
            detail: "A history derivation is already running for this store".to_string(),
        }),
        DerivationOutcome::NotAsked(reason) => Ok(ForegroundDerivation {
            history: "unchanged",
            detail: format!("No history derivation ran: {reason}"),
        }),
    }
}

/// The first-reopen section a newly attached store is read from, prepared the
/// way `kin init` prepares a new store's. Best effort: a daemon builds what is
/// missing on its own.
fn prepare_attached_store(layout: &KinLayout) {
    if let Err(error) = super::repository_authority::materialize_workspace_base_offline(layout) {
        tracing::warn!(%error, "could not prepare the attached store's first graph reopen");
    }
}
