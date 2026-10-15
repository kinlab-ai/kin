//! Published-release migration for an older managed installation.
//!
//! The incoming process is deliberately not authorized as the original managed
//! executable. Its independently authenticated release bytes and the original
//! full install generation form a separate, bounded bootstrap authority.

use super::*;
use kin_core::runtime_identity::RuntimeIdentity;

struct BootstrapReleasePins {
    release: ReleaseExpectation,
    target: String,
    archive_name: String,
}

impl BootstrapReleasePins {
    fn new(
        version: Version,
        commit_sha: String,
        archive_sha256: String,
        target: String,
        archive_name: String,
    ) -> Result<Self> {
        let release = ReleaseExpectation::from_options(
            Some(version),
            Some(commit_sha),
            Some(archive_sha256),
        )?
        .context("published bootstrap requires every release pin")?;
        if target.is_empty() || archive_name != current_platform_asset_name()? {
            anyhow::bail!(
                "published bootstrap target and archive must name this platform's release"
            );
        }
        Ok(Self {
            release,
            target,
            archive_name,
        })
    }

    fn validate_incoming(&self, identity: &RuntimeIdentity) -> Result<()> {
        identity.require_managed().map_err(anyhow::Error::msg)?;
        if identity.version != self.release.version.to_string()
            || identity.commit_sha != self.release.commit_sha
            || identity.target != self.target
        {
            anyhow::bail!(
                "incoming bootstrap executable does not match the pinned published version, source commit, and target"
            );
        }
        Ok(())
    }

    fn validate_prepared(&self, prepared: &PreparedPinnedRelease) -> Result<()> {
        self.release
            .validate_selected_release(&prepared.version, &prepared.commit_sha)?;
        self.release
            .validate_archive_bytes(&prepared.archive_bytes)?;
        if prepared.archive_name != self.archive_name
            || prepared.asset()?.name != self.archive_name
            || prepared.provenance.target != self.target
        {
            anyhow::bail!("verified release does not match the bootstrap target and archive pins");
        }
        Ok(())
    }
}

/// This type cannot authorize an ordinary update: it never implements or
/// constructs `UpdaterStartAuthority`, whose executing-object binding remains
/// the exact original managed CLI.
struct BootstrapInstallAuthority {
    original: ManagedBundleGeneration,
    #[cfg(unix)]
    original_notifier: Option<BootstrapNotifierGeneration>,
    incoming: ExecutingProcessAuthority,
    runtime: RuntimeIdentity,
}

#[cfg(unix)]
#[derive(Clone, Debug, PartialEq, Eq)]
struct BootstrapNotifierGeneration {
    identity: BundleIdentity,
    binding: PlatformObjectIdentity,
}

#[cfg(unix)]
fn bootstrap_notifier_generation(
    lib: &AnchoredDir,
    spec: &[ComponentSpec],
) -> Result<Option<BootstrapNotifierGeneration>> {
    if !spec_carries_notifier_bundle(spec) {
        return Ok(None);
    }
    let Some(identity) = bundle_identity_at(lib, NOTIFIER_BUNDLE_DIR)? else {
        return Ok(None);
    };
    let bundle = lib.open_child(NOTIFIER_BUNDLE_DIR)?;
    lib.ensure_child_binding(NOTIFIER_BUNDLE_DIR, &bundle)?;
    if bundle_identity_at(lib, NOTIFIER_BUNDLE_DIR)?.as_ref() != Some(&identity) {
        anyhow::bail!("managed notification bundle changed while bootstrap identity was read");
    }
    lib.ensure_child_binding(NOTIFIER_BUNDLE_DIR, &bundle)?;
    Ok(Some(BootstrapNotifierGeneration {
        identity,
        binding: PlatformObjectIdentity {
            namespace: bundle.dev,
            file: bundle.ino,
        },
    }))
}

#[cfg(unix)]
fn snapshot_bootstrap_notifier_generation(
    root: &Path,
    spec: &[ComponentSpec],
) -> Result<Option<BootstrapNotifierGeneration>> {
    let root_dir = AnchoredDir::open_ambient(root)?;
    let lib = root_dir.open_child("lib")?;
    root_dir.ensure_child_binding("lib", &lib)?;
    let generation = bootstrap_notifier_generation(&lib, spec)?;
    root_dir.ensure_child_binding("lib", &lib)?;
    Ok(generation)
}

impl BootstrapInstallAuthority {
    fn capture(requested_home: &Path, spec: &[ComponentSpec]) -> Result<Self> {
        Self::capture_with_incoming(
            requested_home,
            spec,
            ExecutingProcessAuthority::capture()?,
            RuntimeIdentity::current(),
        )
    }

    fn capture_with_incoming(
        requested_home: &Path,
        spec: &[ComponentSpec],
        incoming: ExecutingProcessAuthority,
        runtime: RuntimeIdentity,
    ) -> Result<Self> {
        runtime.require_managed().map_err(anyhow::Error::msg)?;
        let original = snapshot_managed_bundle_generation(requested_home, spec)?;
        #[cfg(unix)]
        let original_notifier = snapshot_bootstrap_notifier_generation(&original.root, spec)?;
        let cli_name = if cfg!(windows) { "kin.exe" } else { "kin" };
        let original_cli = original
            .components
            .get(cli_name)
            .and_then(Option::as_ref)
            .context("published bootstrap requires an existing managed CLI")?;
        if incoming.generation.binding == original_cli.binding
            || incoming.path.starts_with(&original.root)
        {
            anyhow::bail!(
                "published bootstrap must run a separate staged release executable outside the managed installation"
            );
        }
        incoming.verify_durable_identity()?;
        let confirmed = snapshot_managed_bundle_generation(requested_home, spec)?;
        if original != confirmed {
            anyhow::bail!("managed Kin bundle changed while bootstrap authority was captured");
        }
        #[cfg(unix)]
        if original_notifier != snapshot_bootstrap_notifier_generation(&confirmed.root, spec)? {
            anyhow::bail!(
                "managed notification bundle changed while bootstrap authority was captured"
            );
        }
        Ok(Self {
            original: confirmed,
            #[cfg(unix)]
            original_notifier,
            incoming,
            runtime,
        })
    }

    fn verify_locked(&self, lock: &InstallRootLock, spec: &[ComponentSpec]) -> Result<()> {
        self.incoming.verify_durable_identity()?;
        verify_managed_bundle_generation_locked(lock, spec, &self.original)?;
        #[cfg(unix)]
        {
            let install = lock.install()?;
            if bootstrap_notifier_generation(&install.lib, spec)? != self.original_notifier {
                anyhow::bail!("managed notification bundle changed during bootstrap preflight");
            }
            install.ensure_bound()?;
        }
        Ok(())
    }

    fn validate_legacy_target(
        &self,
        spec: &[ComponentSpec],
        incoming_version: &Version,
    ) -> Result<()> {
        let cli_name = if cfg!(windows) { "kin.exe" } else { "kin" };
        let installed_version = crate::commands::runtime::probe_legacy_cli_version(
            &self.original.root.join("bin").join(cli_name),
        )?;
        ensure_bootstrap_is_not_a_downgrade(&installed_version, incoming_version)?;
        if crate::commands::runtime::inspect(&crate::commands::runtime::StatusOptions::default())
            .ready
        {
            anyhow::bail!(
                "managed Kin already supports the current runtime contract; invoke its exact installed updater"
            );
        }
        self.incoming.verify_durable_identity()?;
        let confirmed = snapshot_managed_bundle_generation(&self.original.root, spec)?;
        if confirmed != self.original {
            anyhow::bail!("managed Kin generation changed during the legacy version probe");
        }
        #[cfg(unix)]
        if snapshot_bootstrap_notifier_generation(&self.original.root, spec)?
            != self.original_notifier
        {
            anyhow::bail!("managed notification bundle changed during the legacy version probe");
        }
        Ok(())
    }

    fn validate_published_binding(&self, prepared: &PreparedPinnedRelease) -> Result<()> {
        let cli_name = if cfg!(windows) { "kin.exe" } else { "kin" };
        let published_cli = prepared
            .provenance_identities
            .get(cli_name)
            .context("verified published archive has no CLI identity")?;
        validate_bootstrap_release_binding(
            &self.incoming.build,
            &self.runtime,
            &self.incoming.generation.identity,
            &prepared.version,
            &prepared.commit_sha,
            &prepared.provenance.target,
            &prepared.provenance.kin.embedded_dependency_provenance,
            published_cli,
        )
    }
}

fn ensure_bootstrap_is_not_a_downgrade(
    installed_version: &Version,
    incoming_version: &Version,
) -> Result<()> {
    if installed_version > incoming_version {
        anyhow::bail!(
            "published bootstrap cannot downgrade installed Kin {installed_version} to {incoming_version}"
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn validate_bootstrap_release_binding(
    embedded: &EmbeddedBuildIdentity,
    runtime: &RuntimeIdentity,
    mapped: &FileIdentity,
    version: &Version,
    commit_sha: &str,
    target: &str,
    dependency_provenance: &str,
    published_cli: &FileIdentity,
) -> Result<()> {
    runtime.require_managed().map_err(anyhow::Error::msg)?;
    embedded.require_published_release_identity()?;
    if embedded.version != version.to_string()
        || embedded.commit != commit_sha
        || embedded.dependency_provenance != dependency_provenance
        || runtime.version != embedded.version
        || runtime.commit_sha != embedded.commit
        || runtime.target != target
        || mapped != published_cli
    {
        anyhow::bail!(
            "incoming mapped bootstrap executable is not the exact CLI authenticated by the pinned published archive"
        );
    }
    Ok(())
}

/// Migrate an existing installation without relying on its older command API.
/// No managed byte, lock, preference, marker, or registration is written before
/// the incoming executable and every pinned remote input have been verified.
#[allow(clippy::too_many_arguments)]
pub async fn run_bootstrap_published(
    channel: Channel,
    version: Version,
    commit_sha: String,
    archive_sha256: String,
    target: String,
    archive_name: String,
    apply: bool,
) -> Result<()> {
    let pins =
        BootstrapReleasePins::new(version, commit_sha, archive_sha256, target, archive_name)?;
    pins.validate_incoming(&RuntimeIdentity::current())?;
    registry_authority_preflight()?;
    let requested_home = crate::commands::setup::kin_dir()?;
    let inspected_home = validate_existing_install_root(&requested_home)?;
    let spec = platform_bundle_spec(std::env::consts::OS)?;
    if !transaction_dirs(&inspected_home)?.is_empty() {
        anyhow::bail!(
            "an interrupted native update requires recovery before published bootstrap; the existing installation was not modified"
        );
    }
    refuse_restart_marker_before_remote_preflight(&inspected_home)?;
    ensure_no_active_runtimes_but_daemons(&inspected_home, spec).context(
        "published bootstrap requires managed VFS servers to be stopped before release preflight",
    )?;
    let authority = BootstrapInstallAuthority::capture(&inspected_home, spec)?;
    authority.validate_legacy_target(spec, &pins.release.version)?;

    let client = build_update_http_client()?;
    let prepared =
        prepare_pinned_release(&client, channel, &pins.release, &requested_home, spec).await?;
    pins.validate_prepared(&prepared)?;
    authority.validate_published_binding(&prepared)?;

    let lock = InstallRootLock::acquire_existing(&requested_home)?;
    refuse_new_update_while_restart_marker_exists(&lock)?;
    authority.verify_locked(&lock, spec)?;
    if !transaction_dirs(lock.root())?.is_empty() {
        anyhow::bail!("another native update interrupted the captured bootstrap generation");
    }
    ensure_no_active_runtimes_but_daemons(lock.root(), spec).context(
        "published bootstrap install phase requires managed VFS servers to remain stopped",
    )?;
    cleanup_stale_staging_dirs(&lock)?;
    authority.verify_locked(&lock, spec)?;
    let locked_channel =
        resolve_channel_locked(lock.root(), Some(channel), false, true, Some(&lock));
    ensure_pinned_channel_unchanged(channel, locked_channel)?;
    install_bootstrap_prepared_release(&lock, &prepared, &authority, spec).await?;
    drop(lock);
    // Only a fresh invocation of the exact installed CLI can clear the native
    // restart fence; the incoming bootstrap process never acknowledges itself.
    if apply {
        return run_chain_tail();
    }
    Ok(())
}

async fn install_bootstrap_prepared_release(
    lock: &InstallRootLock,
    prepared: &PreparedPinnedRelease,
    authority: &BootstrapInstallAuthority,
    spec: &[ComponentSpec],
) -> Result<()> {
    authority.verify_locked(lock, spec)?;
    authority.validate_published_binding(prepared)?;
    let latest = prepared.version.to_string();
    let staging = StagingDir::create(lock)?;
    stage_archive_locked(
        lock,
        &staging,
        &prepared.archive_bytes,
        &prepared.asset()?.name,
        spec,
    )?;
    validate_staged_artifact_provenance(
        staging.path(),
        spec,
        &prepared.provenance_identities,
        true,
    )?;
    validate_staged_static_build_identity(staging.path(), spec, &latest, &prepared.provenance)?;
    replace_daemons_for_release(lock.root(), &latest, &prepared.commit_sha).await?;
    authority.verify_locked(lock, spec)?;
    let pending = restart_pending_record(
        lock.root(),
        &latest,
        &prepared.provenance,
        &prepared.provenance_identities,
        spec,
    )?;
    let outcome = install_bootstrap_bundle_locked(
        lock,
        &staging,
        spec,
        &prepared.provenance_identities,
        &latest,
        &pending,
        authority,
    )?;
    report_successful_install(lock, lock.root(), &latest, outcome)
}

fn install_bootstrap_bundle_locked(
    lock: &InstallRootLock,
    staging: &StagingDir<'_>,
    spec: &[ComponentSpec],
    verified: &VerifiedStagedIdentities,
    version: &str,
    pending: &RestartPending,
    authority: &BootstrapInstallAuthority,
) -> Result<InstallOutcome> {
    if !std::ptr::eq(lock, staging.lock) {
        anyhow::bail!("bootstrap staging authority does not match the held install lock");
    }
    refuse_new_update_while_restart_marker_exists(lock)?;
    authority.verify_locked(lock, spec)?;
    #[cfg(unix)]
    {
        install_staged_bundle_unix_with_hooks(
            lock,
            staging.layout(),
            spec,
            verified,
            version,
            pending,
            |_, _| authority.incoming.verify_durable_identity(),
            |_, _| authority.incoming.verify_durable_identity(),
            |_| authority.incoming.verify_durable_identity(),
        )
    }
    #[cfg(not(unix))]
    {
        install_staged_bundle_locked_with_hooks(
            lock,
            staging.path(),
            spec,
            verified,
            version,
            pending,
            |_, _| authority.incoming.verify_durable_identity(),
            || authority.incoming.verify_durable_identity(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release_identity() -> (EmbeddedBuildIdentity, RuntimeIdentity, FileIdentity) {
        let sha = "a".repeat(40);
        (
            EmbeddedBuildIdentity {
                version: "0.8.7".into(),
                commit: sha.clone(),
                dirty: false,
                source_known: true,
                dependency_provenance: "provenance".into(),
            },
            RuntimeIdentity {
                version: "0.8.7".into(),
                commit_sha: sha,
                source_dirty: false,
                source_known: true,
                target: "test-target".into(),
                build_profile: "release".into(),
            },
            FileIdentity {
                size_bytes: 100,
                sha256: "b".repeat(64),
            },
        )
    }

    fn binding(
        embedded: &EmbeddedBuildIdentity,
        runtime: &RuntimeIdentity,
        mapped: &FileIdentity,
        published: &FileIdentity,
    ) -> Result<()> {
        validate_bootstrap_release_binding(
            embedded,
            runtime,
            mapped,
            &Version::parse("0.8.7").unwrap(),
            &"a".repeat(40),
            "test-target",
            "provenance",
            published,
        )
    }

    #[test]
    fn published_bootstrap_requires_exact_release_bytes_and_embedded_identity() {
        let (mut embedded, mut runtime, mapped) = release_identity();
        assert!(binding(&embedded, &runtime, &mapped, &mapped).is_ok());
        let mut other_bytes = mapped.clone();
        other_bytes.sha256 = "c".repeat(64);
        assert!(binding(&embedded, &runtime, &mapped, &other_bytes).is_err());
        other_bytes = mapped.clone();
        other_bytes.size_bytes += 1;
        assert!(binding(&embedded, &runtime, &mapped, &other_bytes).is_err());
        embedded.dependency_provenance = "another-source".into();
        assert!(binding(&embedded, &runtime, &mapped, &mapped).is_err());
        embedded.dependency_provenance = "provenance".into();
        runtime.target = "another-target".into();
        assert!(binding(&embedded, &runtime, &mapped, &mapped).is_err());
    }

    #[test]
    fn published_bootstrap_refuses_dirty_unknown_and_development_builds() {
        let (mut embedded, mut runtime, mapped) = release_identity();
        embedded.dirty = true;
        assert!(binding(&embedded, &runtime, &mapped, &mapped).is_err());
        embedded.dirty = false;
        embedded.source_known = false;
        assert!(binding(&embedded, &runtime, &mapped, &mapped).is_err());
        embedded.source_known = true;
        runtime.build_profile = "iteration".into();
        assert!(binding(&embedded, &runtime, &mapped, &mapped).is_err());
        runtime.build_profile = "release".into();
        runtime.source_dirty = true;
        assert!(binding(&embedded, &runtime, &mapped, &mapped).is_err());
        runtime.source_dirty = false;
        runtime.commit_sha = "a".repeat(12);
        assert!(binding(&embedded, &runtime, &mapped, &mapped).is_err());
    }

    #[test]
    fn published_bootstrap_refuses_an_already_newer_installation() {
        let incoming = Version::parse("0.8.7").unwrap();
        assert!(
            ensure_bootstrap_is_not_a_downgrade(&Version::parse("0.8.1").unwrap(), &incoming,)
                .is_ok()
        );
        assert!(ensure_bootstrap_is_not_a_downgrade(&incoming, &incoming).is_ok());
        assert!(
            ensure_bootstrap_is_not_a_downgrade(&Version::parse("0.8.8").unwrap(), &incoming,)
                .is_err()
        );
    }

    #[cfg(unix)]
    fn authority_fixture() -> (tempfile::TempDir, BootstrapInstallAuthority) {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("managed");
        fs::create_dir_all(home.join("bin")).unwrap();
        fs::create_dir_all(home.join("lib")).unwrap();
        fs::write(home.join("bin/kin"), b"old-cli").unwrap();
        fs::write(home.join("bin/kin-daemon"), b"old-daemon").unwrap();
        let incoming_path = temp.path().join("incoming-kin");
        fs::write(&incoming_path, b"verified-incoming-cli").unwrap();
        let incoming = ExecutingProcessAuthority::capture_test_file(&incoming_path).unwrap();
        let (_, runtime, _) = release_identity();
        let authority = BootstrapInstallAuthority::capture_with_incoming(
            &home,
            platform_bundle_spec(std::env::consts::OS).unwrap(),
            incoming,
            runtime,
        )
        .unwrap();
        (temp, authority)
    }

    #[cfg(unix)]
    #[test]
    fn published_bootstrap_fences_the_full_original_generation() {
        let (_temp, authority) = authority_fixture();
        let spec = platform_bundle_spec(std::env::consts::OS).unwrap();
        let lock = InstallRootLock::acquire_existing(&authority.original.root).unwrap();
        assert!(authority.verify_locked(&lock, spec).is_ok());
        fs::write(
            authority.original.root.join("bin/kin-daemon"),
            b"new-daemon",
        )
        .unwrap();
        assert!(authority.verify_locked(&lock, spec).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn published_bootstrap_refuses_identical_bytes_on_replaced_original_object() {
        let (_temp, authority) = authority_fixture();
        let spec = platform_bundle_spec(std::env::consts::OS).unwrap();
        let destination = authority.original.root.join("bin/kin");
        let replacement = authority.original.root.join("bin/replacement");
        fs::write(&replacement, b"old-cli").unwrap();
        fs::rename(replacement, destination).unwrap();
        let lock = InstallRootLock::acquire_existing(&authority.original.root).unwrap();
        assert!(authority.verify_locked(&lock, spec).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn published_bootstrap_rechecks_its_retained_incoming_image() {
        let (_temp, authority) = authority_fixture();
        let spec = platform_bundle_spec(std::env::consts::OS).unwrap();
        let lock = InstallRootLock::acquire_existing(&authority.original.root).unwrap();
        fs::write(&authority.incoming.path, b"changed-incoming-cli").unwrap();
        assert!(authority.verify_locked(&lock, spec).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn published_bootstrap_cannot_authorize_the_mapped_installed_cli() {
        let (_temp, authority) = authority_fixture();
        let incoming =
            ExecutingProcessAuthority::capture_test_file(&authority.original.root.join("bin/kin"))
                .unwrap();
        let (_, runtime, _) = release_identity();
        assert!(BootstrapInstallAuthority::capture_with_incoming(
            &authority.original.root,
            platform_bundle_spec(std::env::consts::OS).unwrap(),
            incoming,
            runtime,
        )
        .is_err());
    }

    #[cfg(unix)]
    #[test]
    fn published_bootstrap_fences_notification_bundle_presence_bytes_and_object() {
        for control in ["added", "changed", "replaced"] {
            let (_temp, prior) = authority_fixture();
            let bundle = prior.original.root.join("lib").join(NOTIFIER_BUNDLE_DIR);
            if control != "added" {
                fs::create_dir(&bundle).unwrap();
                fs::write(bundle.join("member"), b"original").unwrap();
            }
            let incoming =
                ExecutingProcessAuthority::capture_test_file(&prior.incoming.path).unwrap();
            let (_, runtime, _) = release_identity();
            let authority = BootstrapInstallAuthority::capture_with_incoming(
                &prior.original.root,
                MACOS_COMPONENTS,
                incoming,
                runtime,
            )
            .unwrap();
            match control {
                "added" => {
                    fs::create_dir(&bundle).unwrap();
                    fs::write(bundle.join("member"), b"added").unwrap();
                }
                "changed" => {
                    fs::write(bundle.join("member"), b"changed").unwrap();
                }
                "replaced" => {
                    let old = bundle.with_extension("old");
                    fs::rename(&bundle, old).unwrap();
                    fs::create_dir(&bundle).unwrap();
                    fs::write(bundle.join("member"), b"original").unwrap();
                }
                _ => unreachable!(),
            }
            let lock = InstallRootLock::acquire_existing(&authority.original.root).unwrap();
            assert!(authority.verify_locked(&lock, MACOS_COMPONENTS).is_err());
        }
    }
}
