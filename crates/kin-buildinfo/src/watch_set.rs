// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Which paths the build script watches, shared with the library's tests.
//!
//! `build.rs` includes this file by path, because a build script cannot link
//! the crate it builds. Keep it free of dependencies.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Whether edits below the source trees, or to the Git index, can still change
/// the build identity.
///
/// A clean identity turns dirty on the first edit or staged change, so it must
/// watch them. An identity that could not be read must watch them too, so that
/// the next readable state replaces it. A known dirty identity keeps its sha,
/// branch and dirty bit through any further edit or staging; a commit, a
/// checkout or a reset is what changes it, and those move HEAD or write its
/// reflog, which the build script watches regardless. A dirty tree returned to
/// exactly HEAD without moving HEAD (an editor's undo, `git restore`) keeps
/// the binary saying dirty: the fail-closed direction for any cache keyed by
/// the identity.
pub fn watches_source_trees(dirty: bool, source_identity_known: bool) -> bool {
    !(dirty && source_identity_known)
}

/// Each tracked top-level entry of the repository, except that an entry holding
/// a blocked path is replaced by its tracked children, and so on down.
///
/// Cargo reruns a build script when any file below a watched directory is newer
/// than its last run, ignored files included. Blocked paths are therefore this
/// build's own output directory and every path Git ignores: a target directory,
/// a test runner's report under it, Python bytecode, an editor's scratch file.
/// None of them is source, and each one would otherwise rerun the build script
/// on every test run and rebuild every crate that embeds the identity.
/// Untracked directories never appear, because entries come from tracked files.
pub fn source_watch_set(root: &Path, tracked: &[&Path], blocked: &[PathBuf]) -> BTreeSet<PathBuf> {
    let mut watched = BTreeSet::new();
    collect_source_watches(root, Path::new(""), tracked, blocked, &mut watched);
    watched
}

fn collect_source_watches(
    root: &Path,
    prefix: &Path,
    tracked: &[&Path],
    blocked: &[PathBuf],
    watched: &mut BTreeSet<PathBuf>,
) {
    let children: BTreeSet<&OsStr> = tracked
        .iter()
        .filter_map(|file| file.strip_prefix(prefix).ok())
        .filter_map(|rest| rest.components().next())
        .map(|component| component.as_os_str())
        .collect();
    for child in children {
        if prefix.as_os_str().is_empty() && child == OsStr::new("target") {
            continue;
        }
        let relative = prefix.join(child);
        let path = root.join(&relative);
        if blocked.iter().any(|blocked| blocked == &path) {
            // A tracked file Git also reports as ignored (a force-added file
            // matching an ignore rule) is still source.
            if tracked.iter().any(|file| *file == relative) {
                watched.insert(path);
            }
        } else if blocked.iter().any(|blocked| blocked.starts_with(&path)) {
            collect_source_watches(root, &relative, tracked, blocked, watched);
        } else {
            watched.insert(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracked() -> Vec<&'static Path> {
        [
            "README.md",
            "bin/kin-dev",
            "open/kin/Cargo.toml",
            "open/kin/crates/kin-cli/src/main.rs",
            "open/kin/crates/kin-cli/tests/cli.rs",
            "open/kin-editor/package.json",
            "target/tracked-by-mistake.txt",
            "tooling/dev/local_loop.py",
            "tooling/dev/loop-core.json",
        ]
        .into_iter()
        .map(Path::new)
        .collect()
    }

    fn watched(blocked: &[&str]) -> Vec<String> {
        let root = Path::new("/repo");
        let blocked: Vec<PathBuf> = blocked.iter().map(|path| root.join(path)).collect();
        source_watch_set(root, &tracked(), &blocked)
            .into_iter()
            .map(|path| path.strip_prefix(root).unwrap().display().to_string())
            .collect()
    }

    #[test]
    fn with_nothing_blocked_each_tracked_top_level_entry_is_watched_except_target() {
        assert_eq!(watched(&[]), ["README.md", "bin", "open", "tooling"]);
    }

    #[test]
    fn a_test_report_or_bytecode_below_a_source_tree_is_never_watched() {
        // `cargo nextest` writes its JUnit report below open/kin/target, and a
        // Python tool writes bytecode beside the module it imports.
        let set = watched(&[
            "open/kin/target",
            "tooling/dev/__pycache__",
            "bin/__pycache__",
        ]);
        assert_eq!(
            set,
            [
                "README.md",
                "bin/kin-dev",
                "open/kin/Cargo.toml",
                "open/kin/crates",
                "open/kin-editor",
                "tooling/dev/local_loop.py",
                "tooling/dev/loop-core.json",
            ]
        );
        for blocked in [
            "open/kin/target",
            "tooling/dev/__pycache__",
            "bin/__pycache__",
        ] {
            assert!(
                set.iter().all(|path| !blocked.starts_with(path.as_str())),
                "{blocked} sits below a watched path in {set:?}"
            );
        }
    }

    #[test]
    fn an_output_directory_deep_in_a_tree_keeps_its_siblings_watched() {
        let set = watched(&["open/kin/crates/kin-cli/tests/__pycache__"]);
        assert!(set.contains(&"open/kin/crates/kin-cli/src".to_string()));
        assert!(set.contains(&"open/kin/crates/kin-cli/tests/cli.rs".to_string()));
        assert!(set.contains(&"open/kin-editor".to_string()));
        assert!(!set.contains(&"open".to_string()));
    }

    #[test]
    fn a_tracked_file_that_git_also_ignores_stays_watched() {
        let set = watched(&["tooling/dev/loop-core.json"]);
        assert!(set.contains(&"tooling/dev/loop-core.json".to_string()));
        assert!(set.contains(&"tooling/dev/local_loop.py".to_string()));
    }

    #[test]
    fn only_a_known_dirty_identity_stops_watching_the_source_trees() {
        assert!(watches_source_trees(false, true));
        assert!(watches_source_trees(false, false));
        assert!(watches_source_trees(true, false));
        assert!(!watches_source_trees(true, true));
    }
}
