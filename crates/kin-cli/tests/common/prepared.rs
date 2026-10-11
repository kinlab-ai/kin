// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

// Prepared fixture repositories: built once per test binary, copied per test.
//
// Most integration tests here start the same way: write a Git repository,
// commit it, and convert it with `kin init`. The conversion is the expensive
// part, and it is only the subject of the tests that assert what `kin init`
// does. The others need a converted store to act on, so a test binary builds
// each such fixture once, under a file lock every test process of that binary
// shares, and hands each test its own copy of the whole directory: working
// tree, `.git` and `.kin` together.
//
// A copied store is a supported product path, not a test shortcut. `kin init`
// reuses a prepared store by copying it (see `reuse_copy` in the CLI's
// recovery carrier). The copy here follows the same rule that reuse does: the
// store travels, and the donor's own running and record-keeping (daemon
// runtime files, logs, kill records, session records) stay behind. An in-tree
// store names no working tree of its own, and nothing else in it names its
// location, so a copy of the whole directory serves its new one. That is not
// true of a fixture with Git remotes, whose configuration records their URLs:
// build those per test, or give the template remotes that travel with it.
//
// On APFS the copy is one `clonefile(2)` call, which shares blocks until a test
// writes; elsewhere it is a plain recursive copy that keeps modification times,
// so a daemon's catch-up over paths modified since the last admission finds
// nothing to do, exactly as it would in the tree the store was built in.
//
// Each copy carries the repository and workspace identity of its template.
// Every test owns its own `IsolatedDaemonRuntime`, with its own registry,
// `KIN_HOME` and daemons, so copies in different tests never meet. A test that
// needs two distinct repositories must build them itself or from two template
// names; two copies of one template are one repository.

/// Bumped whenever the template layout or the copy rule changes, so a shared
/// `TMPDIR` never hands a test a template built under the old rule.
const PREPARED_TEMPLATE_FORMAT: u32 = 1;

/// How long an unused template directory survives in a shared `TMPDIR`. The
/// nextest setup script already gives each run a fresh `TMPDIR`; this bounds a
/// plain `cargo test` run, which inherits the developer's.
const PREPARED_TEMPLATE_RETENTION: Duration = Duration::from_secs(12 * 60 * 60);

/// Daemon runtime files, as the CLI's reuse rule names them.
const PREPARED_RUNTIME_FILES: &[&str] = &[
    "daemon.pid",
    "daemon.port",
    "daemon.owner",
    "daemon.pid.tmp",
    "daemon.port.tmp",
    "daemon.owner.tmp",
    "daemon.token",
    "daemon.lock",
    "daemon.lifecycle",
    "daemon.start.lock",
];

/// Top-level store files that record the donor's own sessions and sweeps.
const PREPARED_DONOR_RECORDS: &[&str] = &[
    "lsp-sweep-interruptions",
    "coordination_events.jsonl",
    "mcp_transactions.json",
    "pending-release.json",
    "pending-release.lock",
];

/// Store directories whose contents are the donor's own history.
const PREPARED_DONOR_RECORD_DIRS: &[&str] = &["logs", "runs", "bench", "backups", "projections"];

/// One test's own copy of a prepared fixture.
///
/// Dropping it removes the copy. Declare it before the test's
/// `IsolatedDaemonRuntime`, as a fixture's `tempdir()` is, so the runtime stops
/// its daemons before the directory they serve goes away.
pub struct PreparedRepository {
    workspace: PathBuf,
    repo: PathBuf,
    _parent: tempfile::TempDir,
}

impl PreparedRepository {
    /// The copied repository: the working tree holding `.git` and `.kin`.
    pub fn repo(&self) -> &Path {
        &self.repo
    }

    /// The copied fixture root, the directory the builder was handed.
    pub fn root(&self) -> &Path {
        &self.workspace
    }
}

/// This test's own copy of the fixture `name`, built by `build` the first time
/// any test of this binary asks for it.
///
/// `build` receives an empty root directory, creates the fixture under it
/// exactly as a test would (Git repository, `kin init` through its own
/// `IsolatedDaemonRuntime`), lets that runtime stop its daemons, and returns the
/// repository path. `name` must identify one builder within the binary: two
/// builders sharing a name would hand one test the other's fixture.
pub fn prepared_repository<F>(name: &str, build: F) -> PreparedRepository
where
    F: FnOnce(&Path) -> PathBuf,
{
    let template = prepared_template(name, build);
    let parent = tempfile::Builder::new()
        .prefix("kin-prep-")
        .tempdir()
        .unwrap_or_else(|error| panic!("create a directory for a copy of {name}: {error}"));
    let workspace = parent.path().join("w");
    clone_fixture_tree(&template.root, &workspace).unwrap_or_else(|error| {
        panic!(
            "copy the prepared fixture {name} from {} to {}: {error}",
            template.root.display(),
            workspace.display()
        )
    });
    PreparedRepository {
        repo: workspace.join(&template.repo_relative),
        workspace,
        _parent: parent,
    }
}

struct PreparedTemplate {
    root: PathBuf,
    repo_relative: PathBuf,
}

fn prepared_template<F>(name: &str, build: F) -> PreparedTemplate
where
    F: FnOnce(&Path) -> PathBuf,
{
    assert!(
        !name.is_empty()
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'),
        "a prepared fixture name is a plain identifier, not {name:?}"
    );
    let directory = prepared_template_directory();
    let root = directory.join(name);
    let ready = directory.join(format!("{name}.ready"));
    if let Some(template) = read_ready_template(&ready, &root) {
        return template;
    }

    let lock_path = directory.join(format!("{name}.lock"));
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .unwrap_or_else(|error| panic!("open {}: {error}", lock_path.display()));
    // Held until this function returns. A builder that panics releases it by
    // unwinding or by process exit, leaves no ready marker, and the next test
    // builds again rather than copying a half-built fixture.
    lock.lock()
        .unwrap_or_else(|error| panic!("lock {}: {error}", lock_path.display()));
    if let Some(template) = read_ready_template(&ready, &root) {
        return template;
    }

    match std::fs::remove_dir_all(&root) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!("remove the partial fixture {}: {error}", root.display()),
    }
    std::fs::create_dir_all(&root)
        .unwrap_or_else(|error| panic!("create {}: {error}", root.display()));
    let repo = build(&root);
    let repo_relative = repo
        .strip_prefix(&root)
        .unwrap_or_else(|_| {
            panic!(
                "the builder of {name} returned {}, which is not under the root it was handed, {}",
                repo.display(),
                root.display()
            )
        })
        .to_path_buf();
    let store = repo.join(".kin");
    if store.is_dir() {
        leave_donor_runtime_behind(&store);
    }

    let staged = directory.join(format!("{name}.ready.tmp"));
    std::fs::write(&staged, repo_relative.to_string_lossy().as_bytes())
        .unwrap_or_else(|error| panic!("write {}: {error}", staged.display()));
    std::fs::rename(&staged, &ready)
        .unwrap_or_else(|error| panic!("publish {}: {error}", ready.display()));
    drop(lock);
    PreparedTemplate {
        root,
        repo_relative,
    }
}

fn read_ready_template(ready: &Path, root: &Path) -> Option<PreparedTemplate> {
    let relative = std::fs::read_to_string(ready).ok()?;
    Some(PreparedTemplate {
        root: root.to_path_buf(),
        repo_relative: PathBuf::from(relative),
    })
}

/// The directory this binary's templates live in, named by what built them.
///
/// The test executable, `kin` and `kin-daemon` together decide what a
/// template holds, so a rebuild of any of them names a new directory and no
/// test copies a store an older build converted.
fn prepared_template_directory() -> PathBuf {
    static DIRECTORY: OnceLock<PathBuf> = OnceLock::new();
    DIRECTORY
        .get_or_init(|| {
            use sha2::Digest as _;

            let executable = std::env::current_exe().expect("locate this test executable");
            let kin = PathBuf::from(env!("CARGO_BIN_EXE_kin"));
            let daemon =
                kin.with_file_name(format!("kin-daemon{}", std::env::consts::EXE_SUFFIX));
            let mut hasher = sha2::Sha256::new();
            hasher.update(PREPARED_TEMPLATE_FORMAT.to_le_bytes());
            for path in [&executable, &kin, &daemon] {
                hasher.update(path.as_os_str().as_encoded_bytes());
                hasher.update([0]);
                if let Ok(metadata) = std::fs::metadata(path) {
                    hasher.update(metadata.len().to_le_bytes());
                    if let Ok(modified) = metadata.modified() {
                        let since = modified
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default();
                        hasher.update(since.as_nanos().to_le_bytes());
                    }
                }
            }
            let identity = hex::encode(&hasher.finalize()[..8]);
            let stem = executable
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_else(|| "tests".to_string());
            let parent = std::env::temp_dir().join("kin-templates");
            let directory = parent.join(format!("{stem}-{identity}"));
            std::fs::create_dir_all(&directory)
                .unwrap_or_else(|error| panic!("create {}: {error}", directory.display()));
            // Marks this directory as in use, so pruning by another binary
            // measures idleness rather than age.
            let _ = filetime::set_file_mtime(&directory, filetime::FileTime::now());
            prune_idle_templates(&parent, &directory);
            directory
        })
        .clone()
}

fn prune_idle_templates(parent: &Path, current: &Path) {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path == current {
            continue;
        }
        let idle = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|idle| idle > PREPARED_TEMPLATE_RETENTION);
        if idle {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

/// Remove from a copied store what the CLI's reuse rule leaves behind: the
/// donor's daemon runtime files and its own records, keeping the repository.
///
/// A store whose reconciliation directory still holds a transaction or eject
/// journal is bound to the path it was written at, and the product refuses to
/// copy it. A template in that state is a builder that did not let its daemon
/// finish, so it fails here rather than handing tests an unrecoverable copy.
pub fn leave_donor_runtime_behind(store: &Path) {
    if let Ok(entries) = std::fs::read_dir(store.join("reconciliation")) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            assert!(
                !name.starts_with("tx-") && name != "exact-eject-journal.json",
                "the prepared store {} holds path-bound reconciliation recovery ({name}); \
                 its builder must stop the daemon before returning",
                store.display()
            );
        }
    }
    let entries = std::fs::read_dir(store)
        .unwrap_or_else(|error| panic!("read {}: {error}", store.display()));
    for entry in entries {
        let entry = entry.unwrap_or_else(|error| panic!("read {}: {error}", store.display()));
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
        if is_dir {
            if PREPARED_DONOR_RECORD_DIRS.contains(&name.as_str()) {
                std::fs::remove_dir_all(&path)
                    .and_then(|()| std::fs::create_dir(&path))
                    .unwrap_or_else(|error| panic!("empty {}: {error}", path.display()));
            }
            continue;
        }
        let left_behind = PREPARED_RUNTIME_FILES
            .iter()
            .any(|runtime| name.eq_ignore_ascii_case(runtime))
            || name.starts_with("daemon")
            || PREPARED_DONOR_RECORDS.contains(&name.as_str())
            || name == kin_core::layout::WORKING_TREE_RECORD;
        if left_behind {
            std::fs::remove_file(&path)
                .unwrap_or_else(|error| panic!("remove {}: {error}", path.display()));
        }
    }
}

/// Copy the directory tree at `source` to `destination`, which must not exist.
///
/// One `clonefile(2)` on APFS; otherwise a recursive copy that keeps
/// permissions, symbolic links and modification times.
pub fn clone_fixture_tree(source: &Path, destination: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::ffi::OsStrExt as _;

        let from = std::ffi::CString::new(source.as_os_str().as_bytes())?;
        let to = std::ffi::CString::new(destination.as_os_str().as_bytes())?;
        // SAFETY: both arguments are valid NUL-terminated paths that outlive
        // the call, and clonefile does not retain them.
        if unsafe { libc::clonefile(from.as_ptr(), to.as_ptr(), 0) } == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        // Another filesystem, or one without clones: fall back to copying.
        // Anything else, such as an existing destination, is a real failure.
        if !matches!(error.raw_os_error(), Some(libc::ENOTSUP | libc::EXDEV)) {
            return Err(error);
        }
    }
    copy_fixture_tree(source, destination)
}

fn copy_fixture_tree(source: &Path, destination: &Path) -> std::io::Result<()> {
    let metadata = std::fs::symlink_metadata(source)?;
    let kind = metadata.file_type();
    if kind.is_symlink() {
        let target = std::fs::read_link(source)?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, destination)?;
        #[cfg(windows)]
        {
            if std::fs::metadata(source).is_ok_and(|resolved| resolved.is_dir()) {
                std::os::windows::fs::symlink_dir(&target, destination)?;
            } else {
                std::os::windows::fs::symlink_file(&target, destination)?;
            }
        }
        return Ok(());
    }
    if kind.is_dir() {
        std::fs::create_dir(destination)?;
        for entry in std::fs::read_dir(source)? {
            let entry = entry?;
            copy_fixture_tree(&entry.path(), &destination.join(entry.file_name()))?;
        }
        std::fs::set_permissions(destination, metadata.permissions())?;
    } else {
        // `fs::copy` carries permissions, and clones where the platform can.
        std::fs::copy(source, destination)?;
    }
    filetime::set_file_times(
        destination,
        filetime::FileTime::from_last_access_time(&metadata),
        filetime::FileTime::from_last_modification_time(&metadata),
    )
}
