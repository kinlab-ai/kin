// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Cooperative dev/test runtime ownership, backed by the real group guardian.
//!
//! The on-disk records are receipts, never authority to signal a numeric PID.
//! Only the live `ProcessGroupGuardian` owns signalling and its pinned sentinel.

use crate::{ProcessGroupGuardian, ProcessGroupGuardianLauncher};
use serde_json::{json, Value};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

pub const ROOT_ENV: &str = "KIN_TEST_RUNTIME_ROOT";
pub const EXECUTABLE_ENV: &str = "KIN_TEST_RUNTIME_GUARDIAN_EXECUTABLE";
pub const OWNER_ENV: &str = "KIN_TEST_RUNTIME_OWNER_TOKEN";
pub const GROUP_ENV: &str = "KIN_TEST_RUNTIME_CONTAINMENT_PROCESS_GROUP";
static NEXT_GROUP: AtomicU64 = AtomicU64::new(0);

/// Read-only check for a group named by this runtime's own receipt. This never
/// grants signalling authority and treats uncertainty as not quiescent.
pub fn recorded_group_is_quiescent(group: libc::pid_t) -> bool {
    group > 1
        && matches!(
            crate::process_group_containment(group),
            crate::ProcessGroupContainment::Empty | crate::ProcessGroupContainment::OnlyExited
        )
}

pub fn durable_json(path: &Path, value: &Value) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("receipt has no parent"))?;
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    let mut f = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&tmp)?;
    serde_json::to_writer(&mut f, value)?;
    f.write_all(b"\n")?;
    kin_blobs::durable::sync_all(&f)?;
    std::fs::rename(&tmp, path)?;
    kin_blobs::durable::sync_all(&std::fs::File::open(parent)?)
}

/// A per-command or per-language-server guardian with durable ownership records.
/// Readiness is established before a caller may launch executable work.
pub struct TestRuntimeGroup {
    guardian: Option<ProcessGroupGuardian>,
    pub directory: PathBuf,
    owner: String,
    pub group: libc::pid_t,
    pub watcher: u32,
}

impl TestRuntimeGroup {
    pub fn new(executable: &Path, directory: &Path, owner: &str) -> io::Result<Self> {
        std::fs::create_dir(directory)?;
        durable_json(
            &directory.join("intent.json"),
            &json!({
                "version": 1, "owner_token": owner, "owner_pid": std::process::id(),
                "guardian_executable": executable, "payload_admitted": false
            }),
        )?;
        let mut guardian = ProcessGroupGuardianLauncher::product(executable)
            .with_cleanup_timeout(Duration::from_secs(5))
            .spawn_with(
                &directory.join("guardian.ready"),
                Instant::now() + Duration::from_secs(10),
                |_| {},
            )?;
        let group = guardian.process_group();
        let watcher = guardian
            .watcher_id()
            .ok_or_else(|| io::Error::other("guardian has no watcher"))?;
        if let Err(e) = durable_json(
            &directory.join("ready.json"),
            &json!({
                "version": 1, "owner_token": owner, "owner_pid": std::process::id(),
                "group": group, "watcher_pid": watcher, "guardian_executable": executable
            }),
        ) {
            guardian.request_cleanup();
            let _ = guardian.reap_until(Instant::now() + Duration::from_secs(10));
            return Err(e);
        }
        Ok(Self {
            guardian: Some(guardian),
            directory: directory.to_owned(),
            owner: owner.into(),
            group,
            watcher,
        })
    }

    /// Validate the actual runtime group and its owner record before creating a
    /// subordinate LSP guardian. Ordinary product callers get `None` unchanged.
    pub fn language_server_from_env() -> io::Result<Option<Self>> {
        let directory = std::env::var_os(ROOT_ENV);
        let executable = std::env::var_os(EXECUTABLE_ENV);
        if directory.is_none() && executable.is_none() {
            return Ok(None);
        }
        let directory =
            PathBuf::from(directory.ok_or_else(|| io::Error::other("runtime root missing"))?);
        let executable = PathBuf::from(
            executable.ok_or_else(|| io::Error::other("runtime guardian executable missing"))?,
        );
        let owner =
            std::env::var(OWNER_ENV).map_err(|_| io::Error::other("runtime owner missing"))?;
        let declared: libc::pid_t = std::env::var(GROUP_ENV)
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|g| *g > 1)
            .ok_or_else(|| io::Error::other("runtime group missing"))?;
        if unsafe { libc::getpgrp() } != declared || owner.is_empty() {
            return Err(io::Error::other(
                "runtime ownership does not match the actual group",
            ));
        }
        let manifest: Value =
            serde_json::from_slice(&std::fs::read(directory.join("owner.json"))?)?;
        if manifest["owner_token"].as_str() != Some(&owner)
            || manifest["guardian_executable"].as_str() != executable.to_str()
            || manifest["group"].as_i64() != Some(i64::from(declared))
        {
            return Err(io::Error::other("runtime owner record mismatch"));
        }
        let sequence = NEXT_GROUP.fetch_add(1, Ordering::Relaxed);
        let child = directory
            .join("lsp")
            .join(format!("{}-{sequence}", std::process::id()));
        Self::new(&executable, &child, &owner).map(Some)
    }

    pub fn spawn(&mut self, command: std::process::Command) -> io::Result<std::process::Child> {
        self.guardian
            .as_mut()
            .ok_or_else(|| io::Error::other("runtime group finalized"))?
            .spawn(command)
    }

    pub fn spawn_tokio(
        &mut self,
        command: tokio::process::Command,
    ) -> io::Result<tokio::process::Child> {
        self.guardian
            .as_mut()
            .ok_or_else(|| io::Error::other("runtime group finalized"))?
            .spawn_tokio(command)
    }

    pub fn request_cleanup(&mut self) {
        if let Some(guardian) = self.guardian.as_mut() {
            guardian.request_cleanup();
        }
    }

    /// Call after reaping directly owned executable children. Success includes
    /// the guardian's exact process-group quiescence proof, not just exit status.
    pub fn finish_cleanup(&mut self) -> io::Result<()> {
        let Some(mut guardian) = self.guardian.take() else {
            return Ok(());
        };
        guardian.request_cleanup();
        let result = guardian.reap_until(Instant::now() + Duration::from_secs(10));
        let proof = json!({"version": 1, "owner_token": self.owner, "group": self.group,
            "watcher_pid": self.watcher, "cleanup_verified": result.is_ok(),
            "error": result.as_ref().err().map(ToString::to_string)});
        let write = durable_json(&self.directory.join("done.json"), &proof);
        result?;
        write
    }
}

impl Drop for TestRuntimeGroup {
    fn drop(&mut self) {
        if self.guardian.is_some() {
            if let Err(error) = self.finish_cleanup() {
                tracing::warn!(%error, "test runtime group cleanup could not be verified");
            }
        }
    }
}
