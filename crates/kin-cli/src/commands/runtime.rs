// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Executable-pair admission, independent of repository or daemon state.

use anyhow::{Context, Result};
use kin_core::runtime_identity::{RuntimeIdentity, RuntimeMode};
use kin_io::{Grant, HostDir, Spawner};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

const SCHEMA: &str = "kin.runtime-status.v1";
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const PROBE_OUTPUT_LIMIT: u64 = 65_536;

#[derive(Clone, Debug, Default)]
pub struct StatusOptions {
    pub development: bool,
    pub binary: Option<PathBuf>,
}

#[derive(Clone, Debug, Serialize)]
pub struct RuntimeComponent {
    pub path: PathBuf,
    pub version: Option<String>,
    pub commit_sha: Option<String>,
    pub source_dirty: Option<bool>,
    pub source_known: Option<bool>,
    pub target: Option<String>,
    pub build_profile: Option<String>,
    pub sha256: Option<String>,
}

impl RuntimeComponent {
    fn empty(path: PathBuf) -> Self {
        Self {
            path,
            version: None,
            commit_sha: None,
            source_dirty: None,
            source_known: None,
            target: None,
            build_profile: None,
            sha256: None,
        }
    }

    pub fn identity(&self) -> Option<RuntimeIdentity> {
        Some(RuntimeIdentity {
            version: self.version.clone()?,
            commit_sha: self.commit_sha.clone()?,
            source_dirty: self.source_dirty?,
            source_known: self.source_known?,
            target: self.target.clone()?,
            build_profile: self.build_profile.clone()?,
        })
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct RuntimeStatus {
    pub schema: &'static str,
    pub ready: bool,
    pub mode: String,
    pub kin_home: PathBuf,
    pub cli: RuntimeComponent,
    pub daemon: RuntimeComponent,
    pub pair_matches: bool,
    pub channel: Option<String>,
    pub release_kind: String,
    pub reason: Option<String>,
    pub detail: Option<String>,
    pub restart_needed: bool,
}

impl RuntimeStatus {
    fn refuse(&mut self, reason: &str, detail: impl Into<String>) {
        self.ready = false;
        self.reason = Some(reason.to_owned());
        self.detail = Some(detail.into());
    }
}

pub fn managed_home() -> Result<PathBuf> {
    let root = match explicit_home() {
        Some(root) => PathBuf::from(root),
        None => default_home()?,
    };
    // Preserve the requested path for the updater's anchored, no-follow root
    // validation. Canonicalization here would hide a caller-supplied symlink.
    requested_absolute_path(&root)
}

fn requested_absolute_path(root: &Path) -> Result<PathBuf> {
    if root.is_absolute() {
        Ok(root.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(root))
    }
}

fn explicit_home() -> Option<std::ffi::OsString> {
    std::env::var_os("KIN_HOME")
        .filter(|value| !value.is_empty())
        .or_else(|| std::env::var_os("KIN_DIR").filter(|value| !value.is_empty()))
}

fn default_home() -> Result<PathBuf> {
    let home = super::setup::home_dir().context("cannot resolve the user's home directory")?;
    Ok(home.join(".kin"))
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let canonical = absolute.canonicalize().unwrap_or(absolute);
    Ok(strip_windows_verbatim(canonical))
}

fn strip_windows_verbatim(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    if let Some(raw) = path.to_str() {
        if let Some(rest) = raw.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{rest}"));
        }
        if let Some(rest) = raw.strip_prefix(r"\\?\") {
            if rest.as_bytes().get(1) == Some(&b':') {
                return PathBuf::from(rest);
            }
        }
    }
    path
}

fn binary_name(name: &str) -> String {
    format!("{name}{}", std::env::consts::EXE_SUFFIX)
}

pub fn development_home(identity: &RuntimeIdentity, binary: &Path) -> Result<PathBuf> {
    let base = absolute_path(&default_home()?)?;
    if let Some(explicit) = explicit_home() {
        let explicit = absolute_path(Path::new(&explicit))?;
        anyhow::ensure!(
            explicit != base,
            "development mode cannot use the ordinary managed Kin home; set KIN_HOME to an isolated directory"
        );
        return Ok(explicit);
    }
    let parent = binary
        .parent()
        .context("runtime executable has no parent directory")?;
    Ok(identity.development_home(&base, parent))
}

/// Read both executable identities without opening a repository or starting a daemon.
pub fn inspect(options: &StatusOptions) -> RuntimeStatus {
    let mode = if options.development {
        RuntimeMode::Development
    } else {
        RuntimeMode::Managed
    };
    let home = managed_home()
        .and_then(|home| absolute_path(&home))
        .unwrap_or_else(|_| PathBuf::from("/"));
    let selected = options.binary.clone().unwrap_or_else(|| {
        if options.development {
            std::env::current_exe().unwrap_or_else(|_| home.join("bin").join(binary_name("kin")))
        } else {
            home.join("bin").join(binary_name("kin"))
        }
    });
    let selected = absolute_path(&selected).unwrap_or(selected);
    let daemon = selected
        .parent()
        .unwrap_or(&home)
        .join(binary_name("kin-daemon"));
    let mut report = RuntimeStatus {
        schema: SCHEMA,
        ready: false,
        mode: mode.as_str().to_owned(),
        kin_home: home,
        cli: RuntimeComponent::empty(selected),
        daemon: RuntimeComponent::empty(daemon),
        pair_matches: false,
        channel: (mode == RuntimeMode::Development).then(|| "development".to_owned()),
        release_kind: if options.development {
            "development"
        } else {
            "unknown"
        }
        .to_owned(),
        reason: None,
        detail: None,
        restart_needed: false,
    };
    if options.binary.is_some() && !options.development {
        report.refuse(
            "development_required",
            "--binary is only valid with --development",
        );
        return report;
    }
    if mode == RuntimeMode::Managed {
        if let Err(error) = validate_managed_paths() {
            report.refuse("managed_path_invalid", format!("{error:#}"));
            return report;
        }
    }
    if let Err(error) = probe_component(&mut report.cli, true, mode) {
        report.refuse("cli_probe_failed", format!("{error:#}"));
        return report;
    }
    if options.development {
        if let Some(identity) = report.cli.identity() {
            match development_home(&identity, &report.cli.path) {
                Ok(home) => report.kin_home = home,
                Err(error) => {
                    report.refuse("development_home_conflict", error.to_string());
                    return report;
                }
            }
        }
    }
    if let Err(error) = probe_component(&mut report.daemon, false, mode) {
        report.refuse("daemon_probe_failed", format!("{error:#}"));
        return report;
    }
    let (Some(cli), Some(daemon)) = (report.cli.identity(), report.daemon.identity()) else {
        report.refuse(
            "identity_incomplete",
            "both runtime components must expose their complete build identity",
        );
        return report;
    };
    if !cli.source_known
        || !daemon.source_known
        || !full_commit_sha(&cli.commit_sha)
        || !full_commit_sha(&daemon.commit_sha)
    {
        report.refuse(
            "source_identity_unknown",
            "both runtime components must identify their exact known source",
        );
        return report;
    }
    if mode == RuntimeMode::Managed {
        if let Err(error) = cli.require_managed().and_then(|_| daemon.require_managed()) {
            report.refuse("managed_identity_refused", error);
            return report;
        }
    }
    if cli != daemon {
        report.refuse("pair_source_mismatch", "CLI and daemon version, exact source, dirty state, target and build profile must agree");
        return report;
    }
    report.pair_matches = true;
    if mode == RuntimeMode::Development {
        report.ready = true;
        return report;
    }
    let metadata = HostDir::open(&report.kin_home, &installation_grant());
    let metadata = match metadata {
        Ok(metadata) => metadata,
        Err(error) => {
            report.refuse("install_metadata_unreadable", error.to_string());
            return report;
        }
    };
    match metadata.symlink_metadata("update.toml") {
        Ok(entry) if !entry.is_file() => {
            report.refuse(
                "install_metadata_unreadable",
                "recorded update policy must be a regular file",
            );
            return report;
        }
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            report.refuse("install_metadata_unreadable", error.to_string());
            return report;
        }
        _ => {}
    }
    match metadata.read("update.toml", 65_536) {
        Ok(Some(bytes)) => {
            let recorded = std::str::from_utf8(&bytes)
                .ok()
                .and_then(|text| toml::from_str::<toml::Value>(text).ok());
            match recorded {
                Some(recorded) => {
                    if let Some(channel) = recorded.get("channel").and_then(toml::Value::as_str) {
                        if matches!(channel, "stable" | "alpha") {
                            report.channel = Some(channel.to_owned());
                        } else {
                            report.refuse(
                                "update_policy_invalid",
                                "recorded update channel must be stable or alpha",
                            );
                            return report;
                        }
                    }
                }
                None => {
                    report.refuse(
                        "update_policy_invalid",
                        "recorded update policy is not valid UTF-8 TOML",
                    );
                    return report;
                }
            }
        }
        Ok(None) => {}
        Err(error) => {
            report.refuse("install_metadata_unreadable", error.to_string());
            return report;
        }
    }
    report.restart_needed = match metadata.symlink_metadata("update-restart-ack-required.json") {
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            report.refuse("install_metadata_unreadable", error.to_string());
            return report;
        }
    };
    if report.restart_needed {
        report.refuse("restart_required", "the native updater's durable restart fence requires acknowledgement with kin update --ack-restart");
        return report;
    }
    report.ready = true;
    report
}

fn full_commit_sha(sha: &str) -> bool {
    sha.len() == 40 && sha.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn validate_managed_paths() -> Result<()> {
    let home = managed_home()?;
    let entries = [
        home.clone(),
        home.join("bin"),
        home.join("bin").join(binary_name("kin")),
        home.join("bin").join(binary_name("kin-daemon")),
    ];
    for (index, entry) in entries.into_iter().enumerate() {
        let parent = entry
            .parent()
            .context("managed runtime path has no parent")?;
        let name = entry
            .file_name()
            .context("managed runtime path has no name")?;
        let directory = match HostDir::open(parent, &installation_grant()) {
            Ok(directory) => directory,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).context("cannot inspect the managed runtime directory")
            }
        };
        match directory.symlink_metadata(name) {
            Ok(metadata) => {
                anyhow::ensure!(
                    !metadata.file_type().is_symlink(),
                    "managed runtime path must not be a symbolic link: {}",
                    entry.display()
                );
                anyhow::ensure!(
                    if index < 2 {
                        metadata.is_dir()
                    } else {
                        metadata.is_file()
                    },
                    "managed runtime path has an invalid entry type: {}",
                    entry.display()
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("cannot inspect the managed runtime entry"),
        }
    }
    Ok(())
}

pub fn require_managed_pair() -> Result<RuntimeStatus> {
    let report = inspect(&StatusOptions::default());
    anyhow::ensure!(
        report.ready && report.pair_matches,
        "managed runtime admission refused ({}): {}",
        report.reason.as_deref().unwrap_or("unknown"),
        report.detail.as_deref().unwrap_or("runtime is not ready")
    );
    Ok(report)
}

/// Apply mode and home before repository profiles or clients can observe them.
pub fn prepare_environment() -> Result<RuntimeMode> {
    let mode = RuntimeMode::current().map_err(anyhow::Error::msg)?;
    let home = if mode == RuntimeMode::Development {
        let binary = absolute_path(&std::env::current_exe()?)?;
        development_home(&RuntimeIdentity::current(), &binary)?
    } else {
        managed_home()?
    };
    // SAFETY: process entry installs these before creating workers or loading
    // profiles. The chosen values persist for this process and its children.
    unsafe {
        if std::env::var_os("KIN_RUNTIME_MODE").as_deref()
            != Some(std::ffi::OsStr::new(mode.as_str()))
        {
            std::env::set_var("KIN_RUNTIME_MODE", mode.as_str());
        }
        if std::env::var_os("KIN_HOME").as_deref() != Some(home.as_os_str()) {
            std::env::set_var("KIN_HOME", home);
        }
    }
    Ok(mode)
}

/// A release found elsewhere on PATH may only enter the admitted managed pair.
/// Diagnostics remain usable when the installation needs repair.
pub fn route_startup(mode: RuntimeMode, exempt: bool) -> Result<Option<i32>> {
    if exempt {
        return Ok(None);
    }
    let invoked = absolute_path(&std::env::current_exe()?)?;
    let options = StatusOptions {
        development: mode == RuntimeMode::Development,
        binary: (mode == RuntimeMode::Development).then(|| invoked.clone()),
    };
    let report = inspect(&options);
    anyhow::ensure!(
        report.ready,
        "runtime admission refused ({}): {}. Run kin runtime status --json; repair the managed installation with kin update --apply, or select an isolated development runtime explicitly",
        report.reason.as_deref().unwrap_or("unknown"),
        report.detail.as_deref().unwrap_or("runtime is not ready")
    );
    // SAFETY: this is still the single-threaded process entry. Every subsequently
    // launched client and daemon inherits the admitted pair and the same home.
    unsafe {
        std::env::set_var("KIN_DAEMON_BIN", &report.daemon.path);
    }
    if mode == RuntimeMode::Development || invoked == report.cli.path {
        return Ok(None);
    }
    let mut child = Spawner::new(&installation_grant()).command(&report.cli.path);
    child.args(std::env::args_os().skip(1));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let error = child.exec();
        Err(error).context("cannot enter the admitted managed Kin executable")
    }
    #[cfg(not(unix))]
    {
        let status = child
            .status()
            .context("cannot enter the admitted managed Kin executable")?;
        Ok(Some(status.code().unwrap_or(1)))
    }
}

fn probe_component(component: &mut RuntimeComponent, cli: bool, mode: RuntimeMode) -> Result<()> {
    let before = executable_sha256(&component.path)?;
    component.sha256 = Some(before.clone());
    let args: &[&str] = if cli {
        &["bench-meta", "--json", "--runtime-identity-only"]
    } else {
        &["--compat-json"]
    };
    let payload = probe_json(&component.path, args, mode)?;
    let build = if cli {
        &payload
    } else {
        payload.get("build").unwrap_or(&payload)
    };
    let field = |cli_name, daemon_name| {
        build
            .get(if cli { cli_name } else { daemon_name })
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    component.version = if cli {
        field("kin_version", "version")
    } else {
        payload
            .get("version")
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    component.commit_sha = field("kin_commit", "sha");
    component.source_dirty = build
        .get(if cli { "kin_dirty" } else { "dirty" })
        .and_then(Value::as_bool);
    component.source_known = build
        .get(if cli {
            "kin_source_known"
        } else {
            "source_known"
        })
        .and_then(Value::as_bool);
    component.target = field("kin_target", "target");
    component.build_profile = field("kin_build_profile", "build_profile");
    anyhow::ensure!(
        executable_sha256(&component.path)? == before,
        "runtime executable changed during its identity probe"
    );
    Ok(())
}

fn executable_sha256(path: &Path) -> Result<String> {
    let parent = path
        .parent()
        .context("runtime executable has no directory")?;
    let name = path.file_name().context("runtime executable has no name")?;
    let directory = HostDir::open(parent, &installation_grant()).with_context(|| {
        format!(
            "cannot open runtime executable directory {}",
            parent.display()
        )
    })?;
    let file = directory
        .open_read_file(name)
        .with_context(|| format!("cannot read executable {}", path.display()))?;
    anyhow::ensure!(
        file.metadata()?.is_file(),
        "runtime executable is not a regular file"
    );
    let size = file.metadata()?.len();
    let mut file = file.take(size.saturating_add(1));
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 65_536];
    let mut hashed = 0_u64;
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hashed += count as u64;
        hasher.update(&buffer[..count]);
    }
    anyhow::ensure!(
        hashed == size,
        "runtime executable changed size while its identity was being hashed"
    );
    Ok(hex::encode(hasher.finalize()))
}

fn probe_json(binary: &Path, args: &[&str], mode: RuntimeMode) -> Result<Value> {
    let stdout = probe_output(binary, args, mode)?;
    serde_json::from_slice(&stdout).context("runtime identity probe did not emit a JSON object")
}

pub fn probe_legacy_cli_version(binary: &Path) -> Result<semver::Version> {
    let stdout = probe_output(binary, &["--version"], RuntimeMode::Managed)?;
    let text = std::str::from_utf8(&stdout).context("legacy CLI version is not UTF-8")?;
    let version = text
        .trim()
        .strip_prefix("kin ")
        .and_then(|text| text.split_whitespace().next())
        .context("legacy CLI did not emit its kin version")?;
    version.parse().context("legacy CLI version is not SemVer")
}

fn probe_output(binary: &Path, args: &[&str], mode: RuntimeMode) -> Result<Vec<u8>> {
    let mut command = Spawner::new(&installation_grant()).command(binary);
    command
        .args(args)
        .env("KIN_RUNTIME_MODE", mode.as_str())
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("cannot probe {}", binary.display()))?;
    let stdout = child.stdout.take().context("probe stdout is missing")?;
    let stderr = child.stderr.take().context("probe stderr is missing")?;
    let read = |pipe: Box<dyn Read + Send>| {
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let result = pipe
                .take(PROBE_OUTPUT_LIMIT)
                .read_to_end(&mut bytes)
                .map(|_| bytes);
            let _ = sender.send(result);
        });
        receiver
    };
    let stdout = read(Box::new(stdout));
    let stderr = read(Box::new(stderr));
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() >= PROBE_TIMEOUT {
            stop_probe(&mut child);
            let _ = child.wait();
            anyhow::bail!(
                "runtime identity probe exceeded {} seconds",
                PROBE_TIMEOUT.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let read_before_deadline =
        |receiver: std::sync::mpsc::Receiver<std::io::Result<Vec<u8>>>| -> Result<Vec<u8>> {
            let remaining = PROBE_TIMEOUT.saturating_sub(started.elapsed());
            receiver
                .recv_timeout(remaining)
                .map_err(|_| {
                    anyhow::anyhow!(
                        "runtime identity probe output exceeded {} seconds",
                        PROBE_TIMEOUT.as_secs()
                    )
                })?
                .map_err(Into::into)
        };
    let output = read_before_deadline(stdout)
        .and_then(|stdout| read_before_deadline(stderr).map(|stderr| (stdout, stderr)));
    let (stdout, stderr) = match output {
        Ok(output) => output,
        Err(error) => {
            stop_probe(&mut child);
            return Err(error);
        }
    };
    anyhow::ensure!(
        status.success(),
        "runtime identity probe exited {status}: {}",
        String::from_utf8_lossy(&stderr).trim()
    );
    Ok(stdout)
}

fn installation_grant() -> Grant {
    // Installation metadata and the explicitly selected executable pair are an
    // administrative boundary. This grant never reads repository source.
    Grant::ambient()
}

fn stop_probe(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        // The metadata subprocess owns a fresh group; its descendants can keep
        // capture pipes open after the immediate process has exited.
        if let Ok(pid) = i32::try_from(child.id()) {
            // SAFETY: this group was assigned only to the probe we spawned.
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
        }
    }
    let _ = child.kill();
}

pub fn run(json: bool, options: &StatusOptions) -> Result<i32> {
    let report = inspect(options);
    if json {
        crate::println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        crate::println!(
            "Kin runtime: {} ({})",
            if report.ready { "ready" } else { "not ready" },
            report.mode
        );
        crate::println!("Home: {}", report.kin_home.display());
        crate::println!("CLI: {}", report.cli.path.display());
        crate::println!("Daemon: {}", report.daemon.path.display());
        if let Some(detail) = &report.detail {
            crate::println!("{detail}");
        }
    }
    Ok(if report.ready { 0 } else { 1 })
}

pub fn run_identity_probe() -> Result<()> {
    let identity = RuntimeIdentity::current();
    crate::println!(
        "{}",
        serde_json::to_string(&serde_json::json!({
            "schema": "kin.runtime-build-identity.v1",
            "kin_version": identity.version,
            "kin_commit": identity.commit_sha,
            "kin_dirty": identity.source_dirty,
            "kin_source_known": identity.source_known,
            "kin_target": identity.target,
            "kin_build_profile": identity.build_profile,
        }))?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn managed_home_keeps_symlink_for_native_updater_root_validation() {
        let fixture = tempfile::tempdir().unwrap();
        let real = fixture.path().join("real-root");
        let alias = fixture.path().join("root-alias");
        std::fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let selected = requested_absolute_path(&alias);
        assert_eq!(selected.unwrap(), alias);
        assert!(std::fs::symlink_metadata(alias)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    fn installation_reader_stays_confined_and_streams_regular_files() {
        let fixture = tempfile::tempdir().unwrap();
        let home = fixture.path().join("install");
        std::fs::create_dir(&home).unwrap();
        std::fs::write(home.join("binary"), b"runtime bytes").unwrap();
        std::fs::write(fixture.path().join("outside"), b"outside bytes").unwrap();
        let directory = HostDir::open(&home, &installation_grant()).unwrap();
        let mut file = directory.open_read_file("binary").unwrap();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"runtime bytes");
        assert!(directory.open_read_file("../outside").is_err());
        assert!(directory
            .open_read_file(fixture.path().join("outside"))
            .is_err());
        assert!(directory.open_read_file(".").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn legacy_version_probe_only_runs_the_static_version_command() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = tempfile::tempdir().unwrap();
        let binary = fixture.path().join("kin");
        std::fs::write(
            &binary,
            "#!/bin/sh\n[ \"$1\" = '--version' ] || exit 2\nprintf '%s\\n' 'kin 0.8.1'\n",
        )
        .unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            probe_legacy_cli_version(&binary).unwrap(),
            semver::Version::new(0, 8, 1)
        );
        std::fs::write(&binary, "#!/bin/sh\nprintf '%s\\n' 'unversioned binary'\n").unwrap();
        assert!(probe_legacy_cli_version(&binary).is_err());
    }
}
