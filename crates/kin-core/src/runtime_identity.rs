// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Exact process identity and the separation between managed and development services.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

pub const RUNTIME_MODE_ENV: &str = "KIN_RUNTIME_MODE";
pub const RUNTIME_IDENTITY_HEADER: &str = "x-kin-runtime-identity";

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

#[cfg(test)]
mod tests {
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
}
