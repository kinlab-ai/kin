// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Exact process identity and the separation between managed and development services.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

pub const RUNTIME_MODE_ENV: &str = "KIN_RUNTIME_MODE";
pub const RUNTIME_IDENTITY_HEADER: &str = "x-kin-runtime-identity";
pub const RUNTIME_REPOSITORY_HEADER: &str = "x-kin-repository-identity";

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RuntimeRepositoryIdentity {
    pub repository_id: String,
    pub store_namespace: String,
}

impl RuntimeRepositoryIdentity {
    pub fn fingerprint(&self) -> String {
        let bytes = serde_json::to_vec(&(
            "kin.runtime-repository.v1",
            &self.repository_id,
            &self.store_namespace,
        ))
        .expect("repository identity is serializable");
        let mut digest = sha2::Sha256::new();
        digest.update(bytes);
        digest
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    pub fn validate_for(&self, expected: &RuntimeRepositoryBinding) -> Result<String, String> {
        expected.revalidate()?;
        if self != expected.identity() {
            return Err("repository identity differs from the selected repository; reconnect without changing the intended repository".into());
        }
        Ok(expected.fingerprint())
    }
}

#[derive(Debug, Clone)]
pub struct RuntimeRepositoryBinding {
    identity: RuntimeRepositoryIdentity,
    store: RuntimeNamespace,
}

impl RuntimeRepositoryBinding {
    pub fn capture(repository_id: &str, root: &Path) -> Result<Self, String> {
        Self::from_store_namespace(repository_id, RuntimeNamespace::capture(root)?)
    }

    fn from_store_namespace(repository_id: &str, store: RuntimeNamespace) -> Result<Self, String> {
        if repository_id.trim().is_empty() {
            return Err("selected repository has no logical identity".into());
        }
        Ok(Self {
            identity: RuntimeRepositoryIdentity {
                repository_id: repository_id.to_owned(),
                store_namespace: store.token().to_owned(),
            },
            store,
        })
    }

    pub fn for_store(root: &Path) -> Result<Self, String> {
        let store = RuntimeNamespace::capture(root)?;
        let repository_id = store.read_repository_id()?;
        store.revalidate()?;
        Self::from_store_namespace(&repository_id, store)
    }

    pub fn for_layout(layout: &crate::layout::KinLayout) -> Result<Self, String> {
        Self::for_store(layout.root())
    }

    pub fn identity(&self) -> &RuntimeRepositoryIdentity {
        &self.identity
    }

    pub fn fingerprint(&self) -> String {
        self.identity.fingerprint()
    }

    pub fn revalidate(&self) -> Result<(), String> {
        self.store.revalidate()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeMode {
    Managed,
    Development,
}

impl RuntimeMode {
    pub fn current() -> Result<Self, String> {
        Self::resolve(
            std::env::var(RUNTIME_MODE_ENV).ok().as_deref(),
            &RuntimeIdentity::current(),
        )
    }

    pub fn resolve(value: Option<&str>, identity: &RuntimeIdentity) -> Result<Self, String> {
        match value {
            Some("managed") => {
                identity.require_managed()?;
                Ok(Self::Managed)
            }
            Some("development") => Ok(Self::Development),
            Some(value) => Err(format!(
                "{RUNTIME_MODE_ENV} must be managed or development, got {value:?}"
            )),
            None if identity.require_managed().is_ok() => Ok(Self::Managed),
            None => Ok(Self::Development),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Managed => "managed",
            Self::Development => "development",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeIdentity {
    pub version: String,
    pub commit_sha: String,
    pub source_dirty: bool,
    pub source_known: bool,
    pub target: String,
    pub build_profile: String,
}

impl RuntimeIdentity {
    pub fn current() -> Self {
        let build = kin_buildinfo::get();
        Self {
            version: env!("CARGO_PKG_VERSION").to_string(),
            commit_sha: build.sha.to_string(),
            source_dirty: build.dirty,
            source_known: build.source_known,
            target: build.target.to_string(),
            build_profile: build.build_profile.to_string(),
        }
    }

    pub fn require_managed(&self) -> Result<(), String> {
        if !self.source_known
            || self.source_dirty
            || self.build_profile != "release"
            || self.commit_sha.len() != 40
            || !self
                .commit_sha
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            || self.target.is_empty()
        {
            return Err("managed runtime requires a clean release build with a full known source commit and target; select development mode for custom builds".into());
        }
        Ok(())
    }

    /// A version label alone cannot establish that these processes were built together.
    pub fn validate_service(
        &self,
        actual: &Self,
        expected_mode: RuntimeMode,
        actual_mode: RuntimeMode,
    ) -> Result<(), String> {
        if !self.source_known
            || !actual.source_known
            || self.commit_sha.len() != 40
            || actual.commit_sha.len() != 40
        {
            return Err("runtime pair has unknown source provenance; rebuild both components from known source".into());
        }
        if expected_mode != actual_mode {
            return Err(format!(
                "runtime mode mismatch: expected {}, service reports {}; the other service was left running",
                expected_mode.as_str(),
                actual_mode.as_str()
            ));
        }
        if expected_mode == RuntimeMode::Managed {
            self.require_managed()?;
            actual.require_managed()?;
        }
        if self != actual {
            return Err(format!(
                "runtime identity mismatch: CLI {} {} ({}, {}) and service {} {} ({}, {}); update the managed runtime together or select an isolated development runtime",
                self.version,
                self.commit_sha,
                self.target,
                self.build_profile,
                actual.version,
                actual.commit_sha,
                actual.target,
                actual.build_profile
            ));
        }
        Ok(())
    }

    /// CLI and daemon siblings share a development namespace; other build directories do not.
    pub fn development_key(&self, binary_directory: &Path) -> String {
        let mut digest = Sha256::new();
        digest.update(serde_json::to_vec(self).expect("runtime identity is serializable"));
        digest.update([0]);
        digest.update(binary_directory.as_os_str().to_string_lossy().as_bytes());
        digest
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    pub fn fingerprint(&self, mode: RuntimeMode, namespace: &str) -> String {
        let mut digest = Sha256::new();
        digest.update(serde_json::to_vec(self).expect("runtime identity is serializable"));
        digest.update([0]);
        digest.update(mode.as_str().as_bytes());
        digest.update([0]);
        digest.update(namespace.as_bytes());
        digest
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    pub fn development_home(
        &self,
        managed_home: &Path,
        binary_directory: &Path,
    ) -> std::path::PathBuf {
        managed_home
            .join("development")
            .join(self.development_key(binary_directory))
    }
}

/// A retained local home directory, independent of repository/store authority.
/// The token binds its canonical name and native object identity. Keeping the
/// directory open prevents an unlinked object's identity from being recycled
/// while this runtime is still using the old profile.
#[derive(Clone, Debug)]
pub struct RuntimeNamespace {
    selected_home: PathBuf,
    token: String,
    #[cfg(any(unix, windows))]
    _directory: Arc<cap_std::fs::Dir>,
}

static LOCAL_SERVICE_NAMESPACE: OnceLock<RuntimeNamespace> = OnceLock::new();

impl RuntimeNamespace {
    fn read_repository_id(&self) -> Result<String, String> {
        #[cfg(any(unix, windows))]
        {
            let directory = self._directory.try_clone().map_err(|error| {
                format!("cannot retain the selected repository directory: {error}")
            })?;
            let bytes = crate::store_records::read_retained(
                &self.selected_home,
                directory,
                Path::new("manifest.json"),
            )
            .map_err(|error| format!("selected repository identity is unavailable: {error}"))?
            .ok_or_else(|| "selected repository has no manifest record".to_string())?;
            let manifest: crate::manifest::KinManifest = serde_json::from_slice(&bytes)
                .map_err(|error| format!("selected repository manifest is invalid: {error}"))?;
            Ok(manifest.repo_id)
        }
        #[cfg(not(any(unix, windows)))]
        {
            Err("native repository identity is unsupported on this platform".into())
        }
    }

    pub fn current() -> Result<Self, String> {
        Self::capture(&crate::registry::managed_kin_home())
    }

    pub fn capture(home: &Path) -> Result<Self, String> {
        #[cfg(any(unix, windows))]
        {
            let refusal = |error: std::io::Error| {
                format!("cannot bind the selected Kin home runtime namespace: {error}")
            };
            let canonical = home.canonicalize().map_err(refusal)?;
            let directory = cap_std::fs::Dir::open_ambient_dir(home, cap_std::ambient_authority())
                .map_err(refusal)?;
            let object = runtime_directory_identity(&directory).map_err(refusal)?;
            let observed = cap_std::fs::Dir::open_ambient_dir(home, cap_std::ambient_authority())
                .map_err(refusal)?;
            if home.canonicalize().map_err(refusal)? != canonical
                || runtime_directory_identity(&observed).map_err(refusal)? != object
            {
                return Err(
                    "selected Kin home changed while its runtime namespace was captured".into(),
                );
            }
            let mut digest = Sha256::new();
            digest.update(canonical.as_os_str().as_encoded_bytes());
            digest.update([0]);
            digest.update(object);
            let token = digest
                .finalize()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            Ok(Self {
                selected_home: home.to_path_buf(),
                token,
                _directory: Arc::new(directory),
            })
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = home;
            Err("native runtime namespace identity is unsupported on this platform".into())
        }
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn revalidate(&self) -> Result<(), String> {
        let actual = Self::capture(&self.selected_home)?;
        if actual.token != self.token {
            return Err("selected Kin home runtime namespace was replaced; restart the runtime using its current profile".into());
        }
        Ok(())
    }

    /// Real local services call this before profile/model/store loading. Test
    /// routers can capture their own namespace without mutating global state.
    pub fn initialize_local_service() -> Result<(), String> {
        let home = crate::registry::managed_kin_home();
        crate::registry::create_kin_dir_all(&home)
            .map_err(|error| format!("cannot prepare selected Kin home: {error}"))?;
        let namespace = Self::capture(&home)?;
        if let Some(existing) = LOCAL_SERVICE_NAMESPACE.get() {
            existing.revalidate()?;
            if existing.token != namespace.token {
                return Err(
                    "local service runtime namespace was already captured for another home".into(),
                );
            }
            return Ok(());
        }
        LOCAL_SERVICE_NAMESPACE
            .set(namespace)
            .map_err(|_| "local service runtime namespace capture raced".to_string())
    }

    pub fn for_service() -> Result<Self, String> {
        match LOCAL_SERVICE_NAMESPACE.get() {
            Some(namespace) => {
                namespace.revalidate()?;
                Ok(namespace.clone())
            }
            None => Self::current(),
        }
    }
}

#[cfg(unix)]
fn runtime_directory_identity(directory: &cap_std::fs::Dir) -> std::io::Result<Vec<u8>> {
    use cap_std::fs::MetadataExt as _;
    let metadata = directory.dir_metadata()?;
    let mut identity = metadata.dev().to_le_bytes().to_vec();
    identity.extend_from_slice(&metadata.ino().to_le_bytes());
    Ok(identity)
}

#[cfg(windows)]
fn runtime_directory_identity(directory: &cap_std::fs::Dir) -> std::io::Result<Vec<u8>> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FileIdInfo, GetFileInformationByHandleEx, FILE_ID_INFO,
    };
    let mut info: FILE_ID_INFO = unsafe { std::mem::zeroed() };
    // SAFETY: the retained Dir handle is live; the output has the exact API layout.
    let inspected = unsafe {
        GetFileInformationByHandleEx(
            directory.as_raw_handle().cast(),
            FileIdInfo,
            (&raw mut info).cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if inspected == 0 {
        return Err(std::io::Error::last_os_error());
    }
    if info.VolumeSerialNumber == 0 || info.FileId.Identifier.iter().all(|byte| *byte == 0) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Windows runtime namespace returned a zero volume or FILE_ID_128 identity",
        ));
    }
    let mut identity = info.VolumeSerialNumber.to_le_bytes().to_vec();
    identity.extend_from_slice(&info.FileId.Identifier);
    Ok(identity)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeServiceIdentity {
    pub schema: String,
    pub identity: RuntimeIdentity,
    pub mode: RuntimeMode,
    pub component: String,
    pub pid: u32,
    #[serde(default)]
    pub namespace: Option<String>,
    #[serde(default)]
    pub repository: Option<RuntimeRepositoryIdentity>,
}

impl RuntimeServiceIdentity {
    pub fn current(component: &str) -> Result<Self, String> {
        Ok(Self {
            schema: "kin.runtime-service.v1".into(),
            identity: RuntimeIdentity::current(),
            mode: RuntimeMode::current()?,
            component: component.into(),
            pid: std::process::id(),
            namespace: if component == "hosted-daemon" {
                None
            } else {
                Some(RuntimeNamespace::for_service()?.token().to_owned())
            },
            repository: None,
        })
    }

    pub fn validate_for_current(&self, component: &str) -> Result<(), String> {
        self.validate_for(
            component,
            &RuntimeIdentity::current(),
            RuntimeMode::current()?,
            &RuntimeNamespace::current()?,
        )
    }

    /// Admit a daemon request under its declared local or hosted authority.
    /// Hosted release authority is independent of this machine's installation.
    pub fn admit_daemon_for(
        &self,
        identity: &RuntimeIdentity,
        mode: RuntimeMode,
        namespace: Option<&RuntimeNamespace>,
    ) -> Result<Option<String>, String> {
        if self.schema == "kin.runtime-service.v1"
            && self.component == "hosted-daemon"
            && self.namespace.is_none()
        {
            // Each release must be eligible under the same mode, while the
            // hosted service's source identity is independent of the client.
            identity.validate_service(identity, mode, mode)?;
            self.identity
                .validate_service(&self.identity, mode, self.mode)?;
            return Ok(None);
        }
        let namespace = namespace.ok_or_else(|| {
            "local runtime request has no admitted Kin home namespace".to_string()
        })?;
        self.validate_for("daemon", identity, mode, namespace)?;
        Ok(Some(identity.fingerprint(mode, namespace.token())))
    }

    fn validate_for(
        &self,
        component: &str,
        identity: &RuntimeIdentity,
        mode: RuntimeMode,
        namespace: &RuntimeNamespace,
    ) -> Result<(), String> {
        if self.schema != "kin.runtime-service.v1" || self.component != component {
            return Err(format!(
                "endpoint does not acknowledge the {component} runtime identity contract"
            ));
        }
        identity.validate_service(&self.identity, mode, self.mode)?;
        namespace.revalidate()?;
        if self.namespace.as_deref() != Some(namespace.token()) {
            return Err("local runtime namespace differs from the selected Kin home; the other service was left running".into());
        }
        Ok(())
    }
}

/// Where a running daemon's executable lives, as far as replacing it goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerLocation {
    /// The executable could not be read.
    Unknown,
    /// An installed release outside the managed home, such as an npm,
    /// Homebrew or cargo install.
    Installed,
    /// The managed home's own binaries, or an update's backup of them: an
    /// older generation of the managed install.
    ManagedGeneration,
    /// A build directory rather than an installed release, and why.
    BuildDirectory(String),
}

/// What a running daemon says about the release it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerReport {
    /// It serves `/runtime/identity`.
    Identified {
        identity: RuntimeIdentity,
        mode: RuntimeMode,
        namespace: Option<String>,
    },
    /// A release from before runtime identity, as its `/health` reads.
    Legacy {
        version: Option<String>,
        commit: Option<String>,
        dirty: bool,
        source_known: Option<bool>,
    },
}

/// A running daemon of some release, read before a client of another release
/// decides whether to replace it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerRelease {
    pub pid: u32,
    pub executable: Option<PathBuf>,
    pub location: PeerLocation,
    pub report: PeerReport,
}

impl PeerRelease {
    fn named(&self) -> String {
        format!(
            "Kin daemon pid {} ({})",
            self.pid,
            self.executable.as_deref().map_or_else(
                || "executable unknown".to_string(),
                |path| path.display().to_string()
            )
        )
    }

    fn version(&self) -> Option<&str> {
        match &self.report {
            PeerReport::Identified { identity, .. } => Some(&identity.version),
            PeerReport::Legacy { version, .. } => version.as_deref(),
        }
    }

    fn commit(&self) -> Option<&str> {
        match &self.report {
            PeerReport::Identified { identity, .. } => Some(&identity.commit_sha),
            PeerReport::Legacy { commit, .. } => commit.as_deref(),
        }
    }

    /// Why this daemon is a development build, when it is one.
    fn development(&self) -> Option<String> {
        let reported = match &self.report {
            PeerReport::Identified {
                mode: RuntimeMode::Development,
                ..
            } => Some("it runs in development mode".to_string()),
            PeerReport::Identified { identity, .. } => identity
                .require_managed()
                .err()
                .map(|_| "it is not a clean release build".to_string()),
            PeerReport::Legacy { dirty: true, .. } => {
                Some("its build has uncommitted source changes".to_string())
            }
            PeerReport::Legacy {
                source_known: Some(false),
                ..
            } => Some("its build does not know its source commit".to_string()),
            PeerReport::Legacy { .. } => None,
        };
        reported.or_else(|| match &self.location {
            PeerLocation::BuildDirectory(why) => Some(why.clone()),
            _ => None,
        })
    }
}

/// The client deciding whether a running daemon may stay, be attached to or
/// be replaced: its own release, mode and Kin home.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuccessionClient {
    pub identity: RuntimeIdentity,
    pub mode: RuntimeMode,
    /// The Kin home namespace it serves, when it has one to compare.
    pub namespace: Option<String>,
}

/// What a client does about a running daemon of a release that is not
/// exactly its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Succession {
    /// The daemon runs this client's release: use it.
    Attach,
    /// The daemon runs an older release this client supersedes: retire it and
    /// start this release's.
    Replace,
    /// A newer release. An older one never retires it.
    Newer(String),
    /// A development build. It is never replaced, only stopped by whoever
    /// runs it.
    Development(String),
    /// This client is not the installed release, a long-running client of a
    /// release an update replaced. It retires nothing and is told to restart.
    ClientSuperseded(String),
    /// Anything else that refuses, as before: a development client meeting a
    /// managed daemon, another Kin home, an unreadable release.
    Refuse(String),
}

impl Succession {
    /// The sentence a refusal is told in, or `None` for attach and replace.
    pub fn refusal(&self) -> Option<&str> {
        match self {
            Self::Attach | Self::Replace => None,
            Self::Newer(message)
            | Self::Development(message)
            | Self::ClientSuperseded(message)
            | Self::Refuse(message) => Some(message),
        }
    }
}

/// `Kin 0.8.7 (0123abc)`.
pub fn release_label(version: &str, commit: &str) -> String {
    let commit: String = commit.chars().take(7).collect();
    if commit.is_empty() {
        format!("Kin {version}")
    } else {
        format!("Kin {version} ({commit})")
    }
}

fn same_commit(left: &str, right: &str) -> bool {
    let (left, right) = (left.to_ascii_lowercase(), right.to_ascii_lowercase());
    left.len().min(right.len()) >= 7 && (left.starts_with(&right) || right.starts_with(&left))
}

/// Whether `client` attaches to, replaces or leaves the daemon `peer`.
/// `installed` is the managed pair on disk, when it was read: one Kin per
/// machine, and the client that is not it retires nothing.
///
/// Only an older release is ever replaced. A newer release, a development
/// build, a daemon whose release cannot be read, another Kin home, and every
/// daemon a development client meets are left running, and each refusal names
/// the daemon's pid and executable.
pub fn succession(
    client: &SuccessionClient,
    installed: Option<&RuntimeIdentity>,
    peer: &PeerRelease,
) -> Succession {
    if let PeerReport::Identified {
        identity,
        mode,
        namespace,
    } = &peer.report
    {
        if identity == &client.identity
            && *mode == client.mode
            && (client.namespace.is_none() || namespace == &client.namespace)
        {
            return Succession::Attach;
        }
    }
    let named = peer.named();
    if let Some(why) = peer.development() {
        return Succession::Development(format!(
            "{named} was left running: {why}, and a development build is never replaced. \
             Stop it with `kin daemon stop` in its repository"
        ));
    }
    if client.mode != RuntimeMode::Managed {
        return Succession::Refuse(format!(
            "{named} runs another Kin, and a development build never replaces a release; \
             it was left running"
        ));
    }
    if let PeerReport::Identified {
        namespace: Some(namespace),
        ..
    } = &peer.report
    {
        if client
            .namespace
            .as_ref()
            .is_some_and(|own| own != namespace)
        {
            return Succession::Refuse(format!(
                "{named} serves another Kin home and was left running"
            ));
        }
    }
    if let Some(installed) = installed.filter(|installed| **installed != client.identity) {
        return Succession::ClientSuperseded(format!(
            "Kin was updated to {}; restart this client, which is {}",
            release_label(&installed.version, &installed.commit_sha),
            release_label(&client.identity.version, &client.identity.commit_sha)
        ));
    }
    let Some(version) = peer.version() else {
        return Succession::Refuse(format!(
            "{named} does not report which release it runs and was left running"
        ));
    };
    let parse = |version: &str| semver::Version::parse(version.trim().trim_start_matches('v'));
    let (Ok(peer_version), Ok(client_version)) = (parse(version), parse(&client.identity.version))
    else {
        return Succession::Refuse(format!(
            "{named} reports release {version:?}, which cannot be ordered against this Kin; \
             it was left running"
        ));
    };
    let client_label = release_label(&client.identity.version, &client.identity.commit_sha);
    match peer_version.cmp(&client_version) {
        std::cmp::Ordering::Less => Succession::Replace,
        std::cmp::Ordering::Greater => Succession::Newer(format!(
            "{named} runs Kin {peer_version}, newer than {client_label}. An older release never \
             retires a newer one; run `kin update`"
        )),
        std::cmp::Ordering::Equal
            if peer
                .commit()
                .is_some_and(|commit| same_commit(commit, &client.identity.commit_sha)) =>
        {
            Succession::Attach
        }
        std::cmp::Ordering::Equal if peer.location == PeerLocation::ManagedGeneration => {
            Succession::Replace
        }
        std::cmp::Ordering::Equal => Succession::Refuse(format!(
            "{named} runs another build of Kin {peer_version}, and two builds of one version \
             cannot be ordered; it was left running"
        )),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn repository_binding_retained_manifest_keeps_identity_coherent_across_aba_alias() {
        let parent = tempfile::tempdir().unwrap();
        let selected = parent.path().join("selected");
        let replacement = parent.path().join("replacement");
        let retired = parent.path().join("retired");
        std::fs::create_dir(&selected).unwrap();
        std::fs::create_dir(&replacement).unwrap();
        let mut first = crate::manifest::KinManifest::new();
        first.repo_id = "repository-a".into();
        first.save(&selected.join("manifest.json")).unwrap();
        let mut second = crate::manifest::KinManifest::new();
        second.repo_id = "repository-b".into();
        second.save(&replacement.join("manifest.json")).unwrap();
        let retained = super::RuntimeNamespace::capture(&selected).unwrap();
        let original_namespace = retained.token().to_owned();

        std::fs::rename(&selected, &retired).unwrap();
        std::fs::rename(&replacement, &selected).unwrap();
        assert_eq!(
            crate::manifest::KinManifest::load(&selected.join("manifest.json"))
                .unwrap()
                .repo_id,
            "repository-b",
            "the mutable path must actually name B during this read"
        );
        let retained_id = retained.read_repository_id().unwrap();
        std::fs::rename(&selected, &replacement).unwrap();
        std::fs::rename(&retired, &selected).unwrap();
        retained.revalidate().unwrap();

        let binding =
            super::RuntimeRepositoryBinding::from_store_namespace(&retained_id, retained).unwrap();
        assert_eq!(binding.identity().repository_id, "repository-a");
        assert_eq!(binding.identity().store_namespace, original_namespace);
        assert_eq!(
            binding.identity(),
            super::RuntimeRepositoryBinding::for_store(&selected)
                .unwrap()
                .identity()
        );
    }

    use super::*;

    fn clean_identity() -> RuntimeIdentity {
        RuntimeIdentity {
            version: "0.8.6".into(),
            commit_sha: "a".repeat(40),
            source_dirty: false,
            source_known: true,
            target: "aarch64-apple-darwin".into(),
            build_profile: "release".into(),
        }
    }

    #[test]
    fn equal_versions_with_different_source_commits_are_refused() {
        let cli = clean_identity();
        let mut daemon = cli.clone();
        daemon.commit_sha = "b".repeat(40);
        assert!(cli
            .validate_service(&daemon, RuntimeMode::Managed, RuntimeMode::Managed)
            .unwrap_err()
            .contains("identity mismatch"));
    }

    #[test]
    fn development_service_cannot_answer_a_managed_caller() {
        let cli = clean_identity();
        let mut daemon = cli.clone();
        daemon.source_dirty = true;
        assert!(cli
            .validate_service(&daemon, RuntimeMode::Managed, RuntimeMode::Development)
            .unwrap_err()
            .contains("mode mismatch"));
        assert!(RuntimeMode::resolve(Some("managed"), &daemon).is_err());
        assert_eq!(
            RuntimeMode::resolve(None, &daemon).unwrap(),
            RuntimeMode::Development
        );
    }

    #[test]
    fn explicit_development_is_isolated_by_build_directory() {
        let mut identity = clean_identity();
        identity.build_profile = "debug".into();
        assert_eq!(
            RuntimeMode::resolve(Some("development"), &identity).unwrap(),
            RuntimeMode::Development
        );
        assert_ne!(
            identity.development_key(Path::new("/build/one")),
            identity.development_key(Path::new("/build/two"))
        );
        assert!(identity
            .validate_service(
                &identity,
                RuntimeMode::Development,
                RuntimeMode::Development
            )
            .is_ok());
    }

    #[test]
    fn same_build_different_home_metadata_is_refused() {
        let service_home = tempfile::tempdir().unwrap();
        let caller_home = tempfile::tempdir().unwrap();
        let service_namespace = RuntimeNamespace::capture(service_home.path()).unwrap();
        let caller_namespace = RuntimeNamespace::capture(caller_home.path()).unwrap();
        let identity = clean_identity();
        let service = RuntimeServiceIdentity {
            schema: "kin.runtime-service.v1".into(),
            identity: identity.clone(),
            mode: RuntimeMode::Managed,
            component: "daemon".into(),
            pid: std::process::id(),
            namespace: Some(service_namespace.token().to_owned()),
            repository: None,
        };
        service
            .validate_for(
                "daemon",
                &identity,
                RuntimeMode::Managed,
                &service_namespace,
            )
            .unwrap();
        assert!(service
            .validate_for("daemon", &identity, RuntimeMode::Managed, &caller_namespace)
            .unwrap_err()
            .contains("namespace differs"));
    }

    fn managed_client(version: &str) -> SuccessionClient {
        SuccessionClient {
            identity: RuntimeIdentity {
                version: version.into(),
                ..clean_identity()
            },
            mode: RuntimeMode::Managed,
            namespace: Some("home-a".into()),
        }
    }

    fn identified(version: &str, commit: &str, mode: RuntimeMode, home: &str) -> PeerRelease {
        PeerRelease {
            pid: 4242,
            executable: Some(PathBuf::from("/home/someone/.kin/bin/kin-daemon")),
            location: PeerLocation::ManagedGeneration,
            report: PeerReport::Identified {
                identity: RuntimeIdentity {
                    version: version.into(),
                    commit_sha: commit.into(),
                    ..clean_identity()
                },
                mode,
                namespace: Some(home.into()),
            },
        }
    }

    fn legacy(version: &str, location: PeerLocation) -> PeerRelease {
        PeerRelease {
            pid: 4242,
            executable: Some(PathBuf::from("/opt/homebrew/bin/kin-daemon")),
            location,
            report: PeerReport::Legacy {
                version: Some(version.into()),
                commit: Some("b".repeat(40)),
                dirty: false,
                source_known: Some(true),
            },
        }
    }

    /// Every row of the replacement rule, as the outcome each must reach.
    #[test]
    fn succession_follows_the_one_kin_per_machine_rule_row_by_row() {
        let client = managed_client("0.8.7");
        let installed = client.identity.clone();
        let a = "a".repeat(40);
        let c = "c".repeat(40);
        let rows: Vec<(
            &str,
            SuccessionClient,
            Option<RuntimeIdentity>,
            PeerRelease,
            &str,
        )> = vec![
            (
                "the daemon is this client's release",
                client.clone(),
                Some(installed.clone()),
                identified("0.8.7", &a, RuntimeMode::Managed, "home-a"),
                "attach",
            ),
            (
                "an older managed daemon of this home",
                client.clone(),
                Some(installed.clone()),
                identified("0.8.6", &c, RuntimeMode::Managed, "home-a"),
                "replace",
            ),
            (
                "an older clean release from before runtime identity",
                client.clone(),
                Some(installed.clone()),
                legacy("0.8.6", PeerLocation::Installed),
                "replace",
            ),
            (
                "another build of this version from the managed install",
                client.clone(),
                Some(installed.clone()),
                legacy("0.8.7", PeerLocation::ManagedGeneration),
                "replace",
            ),
            (
                "another build of this version from somewhere else",
                client.clone(),
                Some(installed.clone()),
                legacy("0.8.7", PeerLocation::Installed),
                "refuse",
            ),
            (
                "a newer release",
                client.clone(),
                Some(installed.clone()),
                legacy("0.9.0", PeerLocation::Installed),
                "newer",
            ),
            (
                "a daemon in development mode",
                client.clone(),
                Some(installed.clone()),
                identified("0.8.6", &c, RuntimeMode::Development, "home-a"),
                "development",
            ),
            (
                "a legacy daemon from a build directory",
                client.clone(),
                Some(installed.clone()),
                legacy(
                    "0.8.6",
                    PeerLocation::BuildDirectory("it runs from a cargo target directory".into()),
                ),
                "development",
            ),
            (
                "a client of a release an update replaced",
                managed_client("0.8.6"),
                Some(installed.clone()),
                legacy("0.8.5", PeerLocation::Installed),
                "client superseded",
            ),
            (
                "a development client meeting a managed daemon",
                SuccessionClient {
                    mode: RuntimeMode::Development,
                    ..client.clone()
                },
                None,
                legacy("0.8.6", PeerLocation::Installed),
                "refuse",
            ),
            (
                "a daemon of another Kin home",
                client.clone(),
                Some(installed.clone()),
                identified("0.8.6", &c, RuntimeMode::Managed, "home-b"),
                "refuse",
            ),
            (
                "a daemon that does not say its release",
                client.clone(),
                None,
                PeerRelease {
                    report: PeerReport::Legacy {
                        version: None,
                        commit: None,
                        dirty: false,
                        source_known: None,
                    },
                    ..legacy("0.0.0", PeerLocation::Installed)
                },
                "refuse",
            ),
        ];
        for (row, client, installed, peer, expected) in rows {
            let outcome = succession(&client, installed.as_ref(), &peer);
            let reached = match &outcome {
                Succession::Attach => "attach",
                Succession::Replace => "replace",
                Succession::Newer(_) => "newer",
                Succession::Development(_) => "development",
                Succession::ClientSuperseded(_) => "client superseded",
                Succession::Refuse(_) => "refuse",
            };
            assert_eq!(reached, expected, "{row}: {outcome:?}");
            if let Some(refusal) = outcome.refusal() {
                if !matches!(outcome, Succession::ClientSuperseded(_)) {
                    assert!(
                        refusal.contains("pid 4242") && refusal.contains("kin-daemon"),
                        "{row}: a refusal names the daemon's pid and executable: {refusal}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_superseded_client_is_told_which_release_replaced_it() {
        let client = managed_client("0.8.6");
        let installed = managed_client("0.8.7").identity;
        let Succession::ClientSuperseded(message) = succession(
            &client,
            Some(&installed),
            &legacy("0.8.5", PeerLocation::Installed),
        ) else {
            panic!("a client that is not the installed release retires nothing");
        };
        assert_eq!(
            message,
            "Kin was updated to Kin 0.8.7 (aaaaaaa); restart this client, which is Kin 0.8.6 (aaaaaaa)"
        );
        let Succession::Newer(newer) = succession(
            &managed_client("0.8.7"),
            None,
            &legacy("0.9.0", PeerLocation::Installed),
        ) else {
            panic!("a newer daemon is never retired");
        };
        assert!(newer.contains("run `kin update`"), "{newer}");
    }
}
