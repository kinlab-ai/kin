// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Which release a running daemon runs, read for the one-Kin-per-machine rule.
//!
//! The rule itself is [`kin_core::runtime_identity::succession`], a pure
//! function of the client, the installed pair and the daemon. This module
//! reads the daemon: `/runtime/identity` when it serves one, otherwise what
//! `/health` says, which every daemon since embedded build provenance sends,
//! and where its executable lives.

use super::*;
use kin_core::runtime_identity::{
    PeerLocation, PeerRelease, PeerReport, RuntimeIdentity, RuntimeMode, RuntimeNamespace,
    RuntimeServiceIdentity, Succession, SuccessionClient,
};

/// The parts of a `/health` answer that name a release.
#[derive(Debug, Deserialize)]
struct HealthRelease {
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    pid: Option<u32>,
    #[serde(default)]
    build: Option<HealthBuild>,
}

#[derive(Debug, Deserialize)]
struct HealthBuild {
    #[serde(default)]
    sha: Option<String>,
    #[serde(default)]
    dirty: bool,
    #[serde(default)]
    source_known: Option<bool>,
}

/// Why an executable's location makes it a development build, if it does.
///
/// Every installed release, managed, npm, Homebrew or cargo install, keeps
/// `kin-daemon` in a `bin` directory, and cargo never builds into one.
pub fn build_directory(executable: &Path) -> Option<&'static str> {
    if let Some(root) = std::env::var_os("KIN_DEV_TARGET_ROOT").filter(|root| !root.is_empty()) {
        if executable.starts_with(Path::new(&root)) {
            return Some("it runs from the development build root");
        }
    }
    if executable
        .components()
        .any(|component| component.as_os_str() == "target")
    {
        return Some("it runs from a cargo target directory");
    }
    let installed = executable
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|directory| directory == "bin");
    (!installed).then_some("it runs from a build directory, not an installed release")
}

/// Whether `executable` is the managed install's own `bin`, or an update's
/// backup of it, under `managed_home`: an older generation of the managed
/// install, whatever version it reports.
pub fn runs_from_managed_generation(executable: &Path, managed_home: &Path) -> bool {
    // A replaced daemon's executable may no longer exist, so a path that does
    // not resolve is resolved through its directory.
    let canonical = |path: &Path| {
        path.canonicalize()
            .or_else(|error| match (path.parent(), path.file_name()) {
                (Some(parent), Some(name)) => parent.canonicalize().map(|parent| parent.join(name)),
                _ => Err(error),
            })
            .unwrap_or_else(|_| path.to_path_buf())
    };
    let home = canonical(managed_home);
    let executable = canonical(executable);
    let Ok(within) = executable.strip_prefix(&home) else {
        return false;
    };
    let mut components = within.components();
    match components
        .next()
        .map(|first| first.as_os_str().to_string_lossy())
    {
        Some(first) if first == "bin" => components.clone().count() == 1,
        Some(first) => first.starts_with(".update-backup-"),
        None => false,
    }
}

/// Where the daemon at `executable` lives, as far as replacing it goes.
pub fn classify_peer_location(
    executable: Option<&Path>,
    managed_home: Option<&Path>,
) -> PeerLocation {
    let Some(executable) = executable else {
        return PeerLocation::Unknown;
    };
    if managed_home.is_some_and(|home| runs_from_managed_generation(executable, home)) {
        return PeerLocation::ManagedGeneration;
    }
    match build_directory(executable) {
        Some(why) => PeerLocation::BuildDirectory(why.to_string()),
        None => PeerLocation::Installed,
    }
}

/// Read the release of the daemon serving `base_url`, which must answer as
/// `pid`, within `budget`.
pub async fn read_peer_release(
    base_url: &str,
    pid: u32,
    managed_home: Option<&Path>,
    budget: Duration,
) -> std::result::Result<PeerRelease, String> {
    let executable = process_executable_path(pid).map(PathBuf::from);
    let location = classify_peer_location(executable.as_deref(), managed_home);
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .timeout(budget.clamp(Duration::from_millis(100), Duration::from_secs(5)))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| format!("could not build the release probe: {error}"))?;
    let base_url = base_url.trim_end_matches('/');
    let get = |path: &'static str| {
        let client = client.clone();
        let url = format!("{base_url}{path}");
        async move {
            let response = client
                .get(url)
                .send()
                .await
                .map_err(|error| format!("its {path} did not answer: {error}"))?;
            let status = response.status();
            let body = response
                .bytes()
                .await
                .map_err(|error| format!("its {path} answer did not complete: {error}"))?;
            Ok::<_, String>((status, body))
        }
    };
    let (status, body) = get("/runtime/identity").await?;
    let report = if status.is_success() {
        let identity: RuntimeServiceIdentity = serde_json::from_slice(&body)
            .map_err(|error| format!("its runtime identity is unreadable: {error}"))?;
        if identity.pid != pid {
            return Err(format!(
                "its endpoint answers for pid {}, not {pid}",
                identity.pid
            ));
        }
        PeerReport::Identified {
            identity: identity.identity,
            mode: identity.mode,
            namespace: identity.namespace,
        }
    } else if status == reqwest::StatusCode::NOT_FOUND {
        let (status, body) = get("/health").await?;
        if !status.is_success() {
            return Err(format!("its /health answered HTTP {status}"));
        }
        let health: HealthRelease = serde_json::from_slice(&body)
            .map_err(|error| format!("its /health answer is unreadable: {error}"))?;
        if let Some(other) = health.pid.filter(|other| *other != pid) {
            return Err(format!("its endpoint answers for pid {other}, not {pid}"));
        }
        let (commit, dirty, source_known) = match health.build {
            Some(build) => (build.sha, build.dirty, build.source_known),
            None => (None, false, None),
        };
        PeerReport::Legacy {
            version: health.version,
            commit,
            dirty,
            source_known,
        }
    } else {
        return Err(format!("its /runtime/identity answered HTTP {status}"));
    };
    Ok(PeerRelease {
        pid,
        executable,
        location,
        report,
    })
}

/// The managed pair installed on disk, read once per process and only when a
/// client first meets a daemon of another release. `None` when this process
/// is not a managed client or the pair cannot be read.
pub fn installed_runtime() -> Option<RuntimeIdentity> {
    #[cfg(test)]
    if let Some((_, installed)) = CLIENT_FOR_TEST.with(|cell| cell.borrow().clone()) {
        return installed;
    }
    static INSTALLED: std::sync::OnceLock<Option<RuntimeIdentity>> = std::sync::OnceLock::new();
    INSTALLED
        .get_or_init(|| {
            if RuntimeMode::current().ok() != Some(RuntimeMode::Managed) {
                return None;
            }
            let status = crate::commands::runtime::inspect(
                &crate::commands::runtime::StatusOptions::default(),
            );
            status.pair_matches.then(|| status.cli.identity()).flatten()
        })
        .clone()
}

/// This process as a client deciding about a daemon of another release.
pub fn current_succession_client() -> std::result::Result<SuccessionClient, String> {
    #[cfg(test)]
    if let Some((client, _)) = CLIENT_FOR_TEST.with(|cell| cell.borrow().clone()) {
        return Ok(client);
    }
    Ok(SuccessionClient {
        identity: RuntimeIdentity::current(),
        mode: RuntimeMode::current()?,
        namespace: RuntimeNamespace::current()
            .ok()
            .map(|namespace| namespace.token().to_owned()),
    })
}

/// What this process does about the daemon serving `base_url` as `pid`, which
/// refused it as another release.
pub async fn judge_running_daemon(base_url: &str, pid: u32, budget: Duration) -> Succession {
    let client = match current_succession_client() {
        Ok(client) => client,
        Err(error) => return Succession::Refuse(error),
    };
    let managed_home = crate::commands::runtime::managed_home().ok();
    match read_peer_release(base_url, pid, managed_home.as_deref(), budget).await {
        Ok(peer) => {
            kin_core::runtime_identity::succession(&client, installed_runtime().as_ref(), &peer)
        }
        Err(error) => Succession::Refuse(format!(
            "Kin daemon pid {pid} ({}) was left running: its release could not be read: {error}",
            process_executable_path(pid).unwrap_or_else(|| "executable unknown".to_string())
        )),
    }
}

#[cfg(test)]
type ClientForTest = Option<(SuccessionClient, Option<RuntimeIdentity>)>;

#[cfg(test)]
thread_local! {
    /// The client a test stands in for this process. Test builds are always
    /// development clients, and only a managed client replaces anything.
    static CLIENT_FOR_TEST: std::cell::RefCell<ClientForTest> =
        const { std::cell::RefCell::new(None) };
}

/// Stand in a managed client, and the installed pair, for this test's thread
/// until the guard drops.
#[cfg(all(test, unix))]
pub(crate) fn stand_in_client_for_test(
    client: SuccessionClient,
    installed: Option<RuntimeIdentity>,
) -> impl Drop {
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            CLIENT_FOR_TEST.with(|cell| cell.borrow_mut().take());
        }
    }
    CLIENT_FOR_TEST.with(|cell| *cell.borrow_mut() = Some((client, installed)));
    Restore
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_build_directory_is_told_apart_from_every_installed_release() {
        for executable in [
            "/Volumes/KinBuild/targets/some-branch/debug/kin-daemon",
            "/work/kin/target/release/kin-daemon",
            "/work/kin/target/aarch64-apple-darwin/release/kin-daemon",
        ] {
            assert!(
                build_directory(Path::new(executable)).is_some(),
                "{executable}"
            );
        }
        for executable in [
            "/home/someone/.kin/bin/kin-daemon",
            "/opt/homebrew/Cellar/kin/0.8.6/bin/kin-daemon",
            "/home/someone/.cargo/bin/kin-daemon",
        ] {
            assert_eq!(build_directory(Path::new(executable)), None, "{executable}");
        }
    }

    #[test]
    fn the_managed_install_and_its_update_backup_are_its_own_generations() {
        let home = tempfile::tempdir().unwrap();
        let bin = home.path().join("bin");
        let backup = home.path().join(".update-backup-1234").join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&backup).unwrap();
        for executable in [bin.join("kin-daemon"), backup.join("kin-daemon")] {
            assert_eq!(
                classify_peer_location(Some(&executable), Some(home.path())),
                PeerLocation::ManagedGeneration,
                "{}",
                executable.display()
            );
        }
        let elsewhere = Path::new("/opt/homebrew/bin/kin-daemon");
        assert_eq!(
            classify_peer_location(Some(elsewhere), Some(home.path())),
            PeerLocation::Installed
        );
        assert_eq!(
            classify_peer_location(None, Some(home.path())),
            PeerLocation::Unknown
        );
        assert!(!runs_from_managed_generation(
            &home.path().join("bin").join("nested").join("kin-daemon"),
            home.path()
        ));
    }
}
