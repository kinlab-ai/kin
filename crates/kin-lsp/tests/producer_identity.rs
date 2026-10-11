// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The enrichment producer identity moves when a source that decides a proof
//! changes, and only then.
//!
//! Every proof context carries the identity, so a store asks about every proof
//! again when it moves. These tests run the build script's own classification
//! (`build/producer_identity.rs`, included here as it is there) over a copy of
//! this crate's real sources and change one thing at a time.

use std::fs;
use std::path::{Path, PathBuf};

// `watched` is the build script's alone.
#[allow(dead_code)]
#[path = "../build/producer_identity.rs"]
mod producer_identity;

use producer_identity::{
    identity, without_tests, NOT_PROOF_DECIDING, PROOF_DECIDING, WORKSPACE_PRODUCERS,
};

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// A copy of this crate's sources, and of the workspace producers beside it
/// when this build has them, laid out as the real ones are.
struct SourceCopy {
    _root: tempfile::TempDir,
    manifest_dir: PathBuf,
}

impl SourceCopy {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let real = manifest_dir();
        let copied = root.path().join("kin-lsp");
        copy_tree(&real.join("src"), &copied.join("src"));
        for producer in WORKSPACE_PRODUCERS {
            let from = real.join(producer);
            if from.is_file() {
                let to = copied.join(producer);
                fs::create_dir_all(to.parent().unwrap()).unwrap();
                fs::copy(from, to).unwrap();
            }
        }
        Self {
            _root: root,
            manifest_dir: copied,
        }
    }

    fn identity(&self) -> String {
        identity(&self.manifest_dir).unwrap_or_else(|problems| panic!("{problems:#?}"))
    }

    fn source(&self, file: &str) -> PathBuf {
        self.manifest_dir.join("src").join(file)
    }

    /// Run `change` on one file and put the file back as it was.
    fn with_changed<T>(&self, path: &Path, change: &str, then: impl FnOnce() -> T) -> T {
        let before = fs::read(path).unwrap();
        let mut after = before.clone();
        after.extend_from_slice(change.as_bytes());
        fs::write(path, after).unwrap();
        let result = then();
        fs::write(path, before).unwrap();
        result
    }
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// A line that is code to the compiler and to the identity, in either
/// language the crate holds.
fn added_line(file: &str) -> &'static str {
    if file.ends_with(".rs") {
        "\npub const ADDED_BY_THE_IDENTITY_TEST: u8 = 1;\n"
    } else {
        "\n// added by the identity test\n"
    }
}

#[test]
fn the_build_embedded_the_identity_this_classification_gives() {
    assert_eq!(
        identity(&manifest_dir()).unwrap_or_else(|problems| panic!("{problems:#?}")),
        kin_lsp::proof_context::ENRICHMENT_PRODUCER_IDENTITY
    );
}

#[test]
fn every_proof_deciding_source_moves_the_identity() {
    let copy = SourceCopy::new();
    let base = copy.identity();
    for file in PROOF_DECIDING {
        let moved = copy.with_changed(&copy.source(file), added_line(file), || copy.identity());
        assert_ne!(moved, base, "a change to src/{file} must move the identity");
    }
    assert_eq!(copy.identity(), base, "every file was put back");
}

#[test]
fn no_other_source_moves_the_identity() {
    let copy = SourceCopy::new();
    let base = copy.identity();
    for (file, reason) in NOT_PROOF_DECIDING {
        let after = copy.with_changed(&copy.source(file), added_line(file), || copy.identity());
        assert_eq!(
            after, base,
            "src/{file} is listed as deciding no proof ({reason}), so changing it must leave \
             the identity as it was"
        );
    }
    // The change the classification was written for: a constant that times
    // the startup probe.
    let probe = copy.source("readiness_probe.rs");
    let text = fs::read_to_string(&probe).unwrap();
    let longer = text.replace(
        "pub const READINESS_REPEAT_TIMEOUT: Duration = Duration::from_secs(30);",
        "pub const READINESS_REPEAT_TIMEOUT: Duration = Duration::from_secs(45);",
    );
    assert_ne!(longer, text, "the probe still states its repeat limit");
    fs::write(&probe, longer).unwrap();
    assert_eq!(copy.identity(), base);
}

#[test]
fn a_test_inside_a_proof_deciding_source_does_not_move_the_identity() {
    let copy = SourceCopy::new();
    let base = copy.identity();
    let mut edited = 0;
    for file in PROOF_DECIDING.iter().filter(|file| file.ends_with(".rs")) {
        let path = copy.source(file);
        let text = fs::read_to_string(&path).unwrap();
        // Every top-level test module: a new test in each.
        let with_tests = text.replace(
            "\n#[cfg(test)]\nmod ",
            "\n#[cfg(test)]\nmod added_by_the_identity_test {\n    #[test]\n    fn added() {\n        \
             assert_eq!(\"}\", r#\"}\"#);\n    }\n}\n\n#[cfg(test)]\nmod ",
        );
        if with_tests != text {
            fs::write(&path, with_tests).unwrap();
            edited += 1;
        }
    }
    assert!(
        edited >= 40,
        "only {edited} sources carry a test module; the crate has changed shape and this test \
         no longer proves much"
    );
    assert_eq!(
        copy.identity(),
        base,
        "a test added beside each of {edited} test modules moved the identity"
    );

    // The same sources still move it through the code beside their tests.
    let enrichment = copy.source("enrichment.rs");
    let text = fs::read_to_string(&enrichment).unwrap();
    let anchor = "pub(crate) fn answer_names_entity(";
    assert!(text.contains(anchor));
    fs::write(
        &enrichment,
        text.replace(anchor, &format!("const ADDED: u8 = 1;\n{anchor}")),
    )
    .unwrap();
    assert_ne!(copy.identity(), base);
}

#[test]
fn the_workspace_producer_moves_the_identity_and_its_tests_do_not() {
    let copy = SourceCopy::new();
    let base = copy.identity();
    for producer in WORKSPACE_PRODUCERS {
        let path = copy.manifest_dir.join(producer);
        if !path.is_file() {
            // A packaged build of this crate alone holds no ledger beside it.
            continue;
        }
        let moved = copy.with_changed(&path, added_line(producer), || copy.identity());
        assert_ne!(moved, base, "{producer}");
        let tested = copy.with_changed(
            &path,
            "\n#[cfg(test)]\nmod added_by_the_identity_test {\n    #[test]\n    fn added() {}\n}\n",
            || copy.identity(),
        );
        assert_eq!(tested, base, "{producer}");
    }
}

#[test]
fn a_source_in_neither_list_and_a_listed_source_that_is_gone_are_refused() {
    let copy = SourceCopy::new();
    let unlisted = copy.source("analysis_env/new_module.rs");
    fs::write(&unlisted, "pub fn added() {}\n").unwrap();
    let problems = identity(&copy.manifest_dir).unwrap_err();
    assert_eq!(problems.len(), 1, "{problems:#?}");
    assert!(
        problems[0].starts_with("src/analysis_env/new_module.rs is in neither list"),
        "{problems:#?}"
    );
    fs::remove_file(&unlisted).unwrap();

    fs::remove_file(copy.source("pace.rs")).unwrap();
    fs::remove_file(copy.source("protocol.rs")).unwrap();
    let mut problems = identity(&copy.manifest_dir).unwrap_err();
    problems.sort();
    assert_eq!(
        problems,
        [
            "src/pace.rs is listed and does not exist",
            "src/protocol.rs is listed and does not exist",
        ]
    );
}

#[test]
fn the_lists_name_each_source_once_and_give_every_exclusion_a_reason() {
    let mut named: Vec<&str> = PROOF_DECIDING
        .iter()
        .copied()
        .chain(NOT_PROOF_DECIDING.iter().map(|(file, _)| *file))
        .collect();
    let listed = named.len();
    named.sort_unstable();
    named.dedup();
    assert_eq!(named.len(), listed, "a source is named twice");
    for (file, reason) in NOT_PROOF_DECIDING {
        assert!(
            reason.len() >= 5 && !reason.contains('\n'),
            "src/{file} needs one line saying why it decides no proof"
        );
    }
    // Tests are never hashed: every test file is excluded.
    for file in &named {
        if file.contains("test") {
            assert!(
                NOT_PROOF_DECIDING
                    .iter()
                    .any(|(excluded, _)| excluded == file),
                "src/{file} reads as a test and is hashed"
            );
        }
    }
}

#[test]
fn every_real_source_is_cut_cleanly_and_the_same_way_twice() {
    for file in PROOF_DECIDING.iter().filter(|file| file.ends_with(".rs")) {
        let text = fs::read_to_string(manifest_dir().join("src").join(file)).unwrap();
        let kept = without_tests(&text).unwrap_or_else(|reason| panic!("src/{file}: {reason}"));
        assert!(
            !kept.contains("\n#[cfg(test)]\n"),
            "src/{file} keeps a top-level test item"
        );
        assert_eq!(
            without_tests(&kept).as_deref(),
            Ok(kept.as_str()),
            "src/{file}"
        );
        assert!(kept.len() <= text.len());
    }
}

#[test]
fn top_level_test_items_are_cut_and_everything_else_is_kept() {
    let source = r##"use std::fmt;

/// Kept: compiled outside tests too.
#[cfg(any(test, feature = "test-support"))]
pub fn seam() {}

#[cfg(test)]
static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn kept(text: &str) -> usize {
    #[cfg(test)]
    let _nested = '{';
    text.matches(|c: char| c == '}' || c == '"').count()
}

#[cfg( test )]
#[derive(Debug)]
pub(crate) struct Fixture<'a>(&'a [u8; 4]);

#[cfg(test)]
mod tests {
    use super::*;

    const BRACE: char = '}';
    const QUOTE: char = '\'';
    const TEXT: &str = "} #[cfg(test)] mod inner { \" ";
    const RAW: &str = r#"
}
#[cfg(test)]
mod not_a_module {
"#;

    /* a comment with a brace } and /* a nested one } */ */
    #[test]
    fn runs<'a>(_: &'a str) {
        let _ = (BRACE, QUOTE, TEXT, RAW, b'}', br#"}"#);
    }
}

pub fn also_kept() {}

#[cfg(test)]
mod declared_elsewhere;

#[cfg(test)]
impl fmt::Debug for Hidden {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("}")
    }
}
"##;
    let kept = without_tests(source).unwrap();
    for stays in [
        "use std::fmt;",
        "#[cfg(any(test, feature = \"test-support\"))]\npub fn seam() {}",
        "pub fn kept(text: &str) -> usize {",
        "    #[cfg(test)]\n    let _nested = '{';",
        "pub fn also_kept() {}",
    ] {
        assert!(kept.contains(stays), "lost `{stays}` from:\n{kept}");
    }
    for goes in [
        "COUNTER",
        "Fixture",
        "mod tests",
        "not_a_module",
        "declared_elsewhere",
        "Hidden",
        "#[derive(Debug)]",
    ] {
        assert!(!kept.contains(goes), "kept `{goes}` in:\n{kept}");
    }

    // Changing only what was cut leaves the kept text as it was.
    let edited = source
        .replace("fn runs<'a>", "fn runs_differently<'a>")
        .replace("AtomicU64::new(0)", "AtomicU64::new(7)");
    assert_ne!(edited, source);
    assert_eq!(without_tests(&edited).unwrap(), kept);
}

#[test]
fn a_test_item_whose_end_cannot_be_found_is_an_error() {
    assert!(without_tests("#[cfg(test)]\nmod tests {\n    fn open() {\n").is_err());
    assert!(without_tests("pub fn f() {\n").is_err());
    assert!(without_tests("pub fn f() {}\n}\n").is_err());
    assert_eq!(
        without_tests("pub fn f() {}\n").as_deref(),
        Ok("pub fn f() {}")
    );
}

#[test]
fn a_test_and_the_lines_around_it_leave_no_trace() {
    let plain = without_tests("fn a() {}\n\nfn b() {}\n").unwrap();
    for with_a_test in [
        // The first test a file gets, at its end.
        "fn a() {}\n\nfn b() {}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n}\n",
        // One between two items, with its own comment and a second attribute.
        "fn a() {}\n\n/// What the tests below cover.\n#[allow(dead_code)]\n#[cfg(test)]\n\
         mod tests {}\n\n\n\nfn b() {}\n",
        // Several, and a test-only import ahead of everything.
        "#[cfg(test)]\nuse std::fmt;\nfn a() {}\n#[cfg(test)]\nmod one {}\n#[cfg(test)]\n\
         mod two {}\nfn b() {}",
    ] {
        assert_eq!(without_tests(with_a_test).unwrap(), plain, "{with_a_test}");
    }
    // What is kept is kept byte for byte, comments included.
    assert_ne!(
        without_tests("fn a() {}\n\n// why b exists\nfn b() {}\n").unwrap(),
        plain
    );
    assert_ne!(without_tests("fn a() { }\n\nfn b() {}\n").unwrap(), plain);
    // An inner attribute configures the file and stays, whatever follows it.
    assert_eq!(
        without_tests("#![allow(dead_code)]\n#[cfg(test)]\nmod tests {}\nfn a() {}\n").unwrap(),
        "#![allow(dead_code)]\nfn a() {}"
    );
}
