// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Seeded test stores built once per test binary and test run, and copied for
//! every test that asks for one.
//!
//! nextest runs each test in its own process, so a fixture that a whole family
//! of tests seeds identically (an init, a few admissions and a commit, or a Git
//! history and its import) was rebuilt once per test. A template is built by
//! the first process that asks for it, under a file lock, and published by a
//! single rename. Every caller, the first one included, then takes its own copy
//! at its own path and opens it afresh with `DaemonState::open`, so the open
//! still runs for real and no test can see another's writes.
//!
//! The cache is scoped to one nextest run (`NEXTEST_RUN_ID`) and to the exact
//! test binary (its path, length and modification time), so a rebuilt binary
//! or a later run never reads a template an earlier build wrote. Outside
//! nextest the scope is the process. Either way it lives under the run's
//! temporary directory, which the nextest setup script makes fresh per run.
//!
//! A copy is the same store only when nothing in it names where it was built.
//! An in-tree `.kin` serves its parent directory and records no path, and a Git
//! repository's `.git` names none either. A store kept outside its working tree
//! records that tree's path, so [`assert_in_tree_store`] refuses one. New
//! stores go to the central stores directory by default, so building or
//! reading a template pins this test process to the in-tree layout
//! ([`pin_in_tree_stores`]): copying a store to a new path is exactly what
//! only an in-tree store supports.

use std::future::Future;
use std::path::{Path, PathBuf};

/// One built template: a directory tree that is complete and that nothing is
/// still writing into.
pub(crate) struct Template {
    root: PathBuf,
}

impl Template {
    /// The template's root directory. Read it; never write into it.
    pub(crate) fn path(&self) -> &Path {
        &self.root
    }

    /// Copy the template member `member` (a path relative to the template
    /// root) into `destination`, an existing empty directory.
    pub(crate) fn copy_member_into(&self, member: &str, destination: &Path) {
        copy_tree(&self.root.join(member), destination);
    }

    /// A fresh temporary directory holding a copy of `member`, the way a
    /// fixture's `tempfile::tempdir()` holds the store it built.
    pub(crate) fn copy_member_to_tempdir(&self, member: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        self.copy_member_into(member, dir.path());
        dir
    }
}

/// The template `name`, built by `build` in an empty directory when no process
/// in this run has built it yet.
///
/// When `build` returns, everything it started must have stopped writing into
/// the directory it was given: drop every `DaemonState` and wait for anything
/// it spawned. A build that panics leaves its staging directory behind, and the
/// next caller removes it and builds again.
pub(crate) fn template(name: &str, build: impl FnOnce(&Path)) -> Template {
    pin_in_tree_stores();
    let (root, published) = template_paths(name);
    if published.is_dir() {
        return Template { root: published };
    }
    let _lock = TemplateLock::acquire(&root, name);
    if published.is_dir() {
        return Template { root: published };
    }
    let staging = fresh_staging_dir(&root, name);
    build(&staging);
    publish(&staging, &published);
    Template { root: published }
}

/// [`template`] for a build that has to await.
///
/// The lock is a blocking file lock taken on the calling thread. That is
/// deliberate: a test process waiting for another process to finish the build
/// has nothing else to run in the meantime.
pub(crate) async fn template_async<F, Fut>(name: &str, build: F) -> Template
where
    F: FnOnce(PathBuf) -> Fut,
    Fut: Future<Output = ()>,
{
    pin_in_tree_stores();
    let (root, published) = template_paths(name);
    if published.is_dir() {
        return Template { root: published };
    }
    let _lock = TemplateLock::acquire(&root, name);
    if published.is_dir() {
        return Template { root: published };
    }
    let staging = fresh_staging_dir(&root, name);
    build(staging.clone()).await;
    publish(&staging, &published);
    Template { root: published }
}

/// Keep every store this test process creates in its working tree.
///
/// A template is copied to a new path, which only an in-tree store survives,
/// so every template is built under this. Installed for the whole process,
/// because the seed functions open their stores on runtime threads where a
/// scoped environment guard cannot follow.
pub(crate) fn pin_in_tree_stores() {
    kin_core::test_env::install_process_wide(kin_core::layout::KIN_STORE_LAYOUT_ENV, "in-tree");
}

/// Refuse to template a store whose `.kin` is not inside `working_dir`. Such a
/// store records its working tree's path, so a copy would serve the original.
pub(crate) fn assert_in_tree_store(working_dir: &Path, layout: &kin_core::KinLayout) {
    assert!(
        !layout.is_external()
            && std::fs::canonicalize(layout.root()).unwrap()
                == std::fs::canonicalize(working_dir.join(".kin")).unwrap(),
        "only an in-tree store can be copied to a new path: {}",
        layout.root().display()
    );
}

/// Refuse a template tree that spells out the directory it was built in.
///
/// A copy is the same store only when nothing in it names where it was built.
/// This reads every regular file under `tree` for each spelling of `built_at`
/// (as given and canonical), so a store that recorded its location in plain
/// text is refused at build time instead of serving the template from every
/// copy. Run once per template, when it is built.
pub(crate) fn assert_names_no_path(tree: &Path, built_at: &Path) {
    let mut spellings = vec![built_at.as_os_str().as_encoded_bytes().to_vec()];
    if let Ok(canonical) = std::fs::canonicalize(built_at) {
        spellings.push(canonical.as_os_str().as_encoded_bytes().to_vec());
    }
    fn walk(directory: &Path, spellings: &[Vec<u8>]) {
        for entry in std::fs::read_dir(directory).unwrap() {
            let entry = entry.unwrap();
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                walk(&entry.path(), spellings);
            } else if kind.is_file() {
                let bytes = std::fs::read(entry.path()).unwrap();
                for spelling in spellings {
                    assert!(
                        !bytes
                            .windows(spelling.len())
                            .any(|window| window == spelling.as_slice()),
                        "template file {} names the directory it was built in, {}; \
                         a copy of it would serve the template",
                        entry.path().display(),
                        String::from_utf8_lossy(spelling)
                    );
                }
            }
        }
    }
    walk(tree, &spellings);
}

/// The layout of a copied in-tree store at `working_dir`, through the
/// canonical path `kin_core::init` would have recorded for it.
pub(crate) fn copied_layout(working_dir: &Path) -> kin_core::KinLayout {
    kin_core::KinLayout::new(std::fs::canonicalize(working_dir).unwrap().join(".kin"))
}

fn template_paths(name: &str) -> (PathBuf, PathBuf) {
    assert!(
        !name.is_empty()
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'),
        "a template name is one plain path component: {name:?}"
    );
    let root = cache_root();
    std::fs::create_dir_all(&root).unwrap();
    let published = root.join(name);
    (root, published)
}

/// This binary's template directory for this run.
fn cache_root() -> PathBuf {
    use std::hash::{Hash, Hasher};
    let scope = match std::env::var("NEXTEST_RUN_ID") {
        Ok(run) if !run.is_empty() => run,
        _ => format!("process-{}", std::process::id()),
    };
    let exe = std::env::current_exe().unwrap();
    let metadata = std::fs::metadata(&exe).unwrap();
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |since| since.as_nanos());
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    exe.hash(&mut hasher);
    metadata.len().hash(&mut hasher);
    modified.hash(&mut hasher);
    std::env::temp_dir()
        .join("kin-daemon-test-templates")
        .join(format!("{scope}-{:016x}", hasher.finish()))
}

fn fresh_staging_dir(root: &Path, name: &str) -> PathBuf {
    let staging = root.join(format!("{name}.staging"));
    match std::fs::remove_dir_all(&staging) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!(
            "cannot clear abandoned template staging {}: {error}",
            staging.display()
        ),
    }
    std::fs::create_dir(&staging).unwrap();
    staging
}

fn publish(staging: &Path, published: &Path) {
    std::fs::rename(staging, published).unwrap_or_else(|error| {
        panic!(
            "cannot publish template {} as {}: {error}",
            staging.display(),
            published.display()
        )
    });
}

/// An exclusive lock on one template's build, held by an open file.
struct TemplateLock {
    _file: std::fs::File,
}

impl TemplateLock {
    fn acquire(root: &Path, name: &str) -> Self {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(root.join(format!("{name}.lock")))
            .unwrap();
        fs2::FileExt::lock_exclusive(&file).unwrap();
        Self { _file: file }
    }
}

/// Copy every entry under `source` into the existing directory `destination`,
/// keeping file and directory permissions. `std::fs::copy` clones a file where
/// the filesystem can. A symbolic link is recreated with the same target, the
/// way Git checks a tracked link out; any other special member is refused
/// rather than guessed at.
pub(crate) fn copy_tree(source: &Path, destination: &Path) {
    for entry in std::fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let from = entry.path();
        let to = destination.join(entry.file_name());
        let kind = entry.file_type().unwrap();
        if kind.is_dir() {
            std::fs::create_dir(&to).unwrap();
            copy_tree(&from, &to);
            std::fs::set_permissions(&to, std::fs::metadata(&from).unwrap().permissions()).unwrap();
        } else if kind.is_file() {
            std::fs::copy(&from, &to).unwrap();
        } else if kind.is_symlink() {
            copy_symlink(&from, &to);
        } else {
            panic!("a template holds a special member at {}", from.display());
        }
    }
}

#[cfg(unix)]
fn copy_symlink(from: &Path, to: &Path) {
    std::os::unix::fs::symlink(std::fs::read_link(from).unwrap(), to).unwrap();
}

#[cfg(not(unix))]
fn copy_symlink(from: &Path, _to: &Path) {
    panic!("a template holds a symbolic link at {}", from.display());
}
