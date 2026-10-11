// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! What a guide says the consumers of a changed entity need, in a real
//! Python store, through the real binaries.
//!
//! A consumer binds to an entity's signature and runs its body. A comment or
//! a docstring asks nothing of it, a body change may change what it gets back,
//! and a signature change can break it. The guide reads which one happened
//! from the graph's fingerprint of the entity's tokens, which the parser
//! writes with comments and whitespace skipped, and from the behaviour
//! equivalence class the Python adapter records, which reads past a
//! docstring. A graph built by hand carries neither, so these drive `kin`
//! itself on a small package, one edit per recorded change, and read its JSON.

use serde_json::Value;
use std::fs;
use std::path::Path;
use tempfile::tempdir;

mod common;

use common::Command;

fn run_git(repo: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", kin_git::empty_global_git_config())
        .current_dir(repo)
        .output()
        .expect("run git")
}

fn require_git(repo: &Path, args: &[&str]) {
    let output = run_git(repo, args);
    assert!(
        output.status.success(),
        "git {args:?} failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn require_kin(
    runtime: &common::IsolatedDaemonRuntime,
    repo: &Path,
    args: &[&str],
) -> std::process::Output {
    let output = runtime
        .kin_command()
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", kin_git::empty_global_git_config())
        .env("KIN_DAEMON_BIN", runtime.daemon_bin())
        .env("KIN_DAEMON_DISABLE_LSP", "1")
        .current_dir(repo)
        .output()
        .expect("run kin");
    assert!(
        output.status.success(),
        "kin {args:?} failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn guide(runtime: &common::IsolatedDaemonRuntime, repo: &Path) -> Value {
    let output = require_kin(runtime, repo, &["review", "guide", "--json"]);
    let guide: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "kin review guide --json should print a guide ({error}): {}",
            String::from_utf8_lossy(&output.stdout)
        )
    });
    assert_eq!(guide["schema"], "kin.review.guide.v1");
    guide
}

/// The one step a guide walks for `want_bytes`.
fn want_bytes_step(guide: &Value) -> &Value {
    let steps: Vec<&Value> = guide["chapters"]
        .as_array()
        .expect("a guide lists its chapters")
        .iter()
        .flat_map(|chapter| {
            chapter["steps"]
                .as_array()
                .expect("a chapter lists its steps")
        })
        .filter(|step| step["name"] == "want_bytes")
        .collect();
    assert_eq!(steps.len(), 1, "want_bytes is one step: {guide:#}");
    steps[0]
}

fn count(value: &Value) -> u64 {
    value
        .as_u64()
        .unwrap_or_else(|| panic!("a count: {value:#}"))
}

/// What one edit should read as: the step's verdict and reason, which of the
/// three summary counts holds every consumer, and what the summary says the
/// consumers need.
struct Expected<'a> {
    verdict: &'a str,
    reason: &'a str,
    bucket: &'a str,
    headline: &'a str,
}

fn assert_reads_as(guide: &Value, expected: &Expected<'_>) {
    let step = want_bytes_step(guide);
    let summary = &guide["summary"];
    let consumers = count(&summary["recorded_consumers"]);
    assert!(
        consumers >= 2,
        "`sign` and `verify` reach want_bytes from outside the change: {summary:#}"
    );
    let verdict = &step["consumer_verdict"];
    assert_eq!(verdict["verdict"], expected.verdict, "{step:#}");
    assert_eq!(verdict["reason"], expected.reason, "{step:#}");
    assert_eq!(count(&verdict["consumers"]), consumers, "{step:#}");
    for bucket in [
        "consumers_not_updated",
        "consumers_may_need_review",
        "consumers_need_no_update",
    ] {
        let want = if bucket == expected.bucket {
            consumers
        } else {
            0
        };
        assert_eq!(count(&summary[bucket]), want, "{bucket}: {summary:#}");
    }
    let narration = summary["narration"].as_str().expect("a narration");
    assert!(
        narration.contains(expected.headline),
        "the headline should say `{}`: {narration}",
        expected.headline
    );
}

const ENCODING: &str = r#"def want_bytes(
    s: str | bytes, encoding: str = "utf-8", errors: str = "strict"
) -> bytes:
    """Encode a string to bytes."""
    if isinstance(s, str):
        s = s.encode(encoding, errors)

    return s
"#;

const SIGNER: &str = r#"from .encoding import want_bytes


def sign(value):
    return want_bytes(value) + b"."


def verify(value, sig):
    return want_bytes(value) == sig
"#;

const TEST: &str = r#"from pkg.encoding import want_bytes


def test_want_bytes():
    assert want_bytes("a") == b"a"
"#;

/// A comment inside the parameter list, as in the edit that found this: the
/// signature's recorded text takes the comment in, and the tokens say the
/// declaration is the same. Then a docstring, then the body, then the
/// signature, each recorded on its own, each read as what it asks of
/// `sign` and `verify`, which never change.
///
/// Falsify by reading the signature off its text in
/// `kin_review::ContractChange::of`: the comment then reads as a signature
/// change and its consumers as not updated, which is what the guide used to
/// say.
#[test]
fn each_edit_tells_its_consumers_what_they_need() {
    let root = tempdir().expect("temp root");
    let repo = root.path().join("repo");
    let runtime = common::IsolatedDaemonRuntime::new(&repo);
    fs::create_dir_all(repo.join("src/pkg")).expect("create package");
    fs::create_dir_all(repo.join("tests")).expect("create tests");
    require_git(&repo, &["init", "--initial-branch=main"]);
    require_git(&repo, &["config", "commit.gpgsign", "false"]);
    require_git(&repo, &["config", "user.name", "Ada Lovelace"]);
    require_git(&repo, &["config", "user.email", "ada@example.com"]);
    fs::write(repo.join("src/pkg/__init__.py"), b"").expect("write package");
    fs::write(repo.join("src/pkg/encoding.py"), ENCODING).expect("write encoding");
    fs::write(repo.join("src/pkg/signer.py"), SIGNER).expect("write signer");
    fs::write(repo.join("tests/test_encoding.py"), TEST).expect("write test");
    require_git(&repo, &["add", "--all"]);
    require_git(&repo, &["commit", "-m", "Add want_bytes and its callers"]);
    require_kin(&runtime, &repo, &["init", ".", "--json"]);

    let mut source = ENCODING.to_string();
    let mut edit = |from: &str, to: &str, message: &str| -> Value {
        assert!(source.contains(from), "the fixture holds `{from}`");
        source = source.replacen(from, to, 1);
        fs::write(repo.join("src/pkg/encoding.py"), &source).expect("edit want_bytes");
        require_kin(&runtime, &repo, &["commit", "-m", message]);
        guide(&runtime, &repo)
    };

    // A comment between the parameters.
    let commented = edit(
        "def want_bytes(\n",
        "def want_bytes(\n    # Bytes pass through unchanged.\n",
        "Say that bytes pass through",
    );
    assert_reads_as(
        &commented,
        &Expected {
            verdict: "needs_no_update",
            reason: "comments_or_layout_only",
            bucket: "consumers_need_no_update",
            headline: "None of them needs an update: what they rely on kept its signature \
                       and behaves as it did.",
        },
    );
    let step = want_bytes_step(&commented);
    let findings = step["findings"].as_array().expect("findings");
    assert!(
        findings
            .iter()
            .all(|finding| finding["kind"] != "breaking" && finding["kind"] != "signature_change"),
        "a comment breaks nothing and changes no signature: {findings:#?}"
    );
    assert!(
        step["narration"]
            .as_str()
            .is_some_and(|narration| narration.contains("They need no update.")
                && !narration.contains("Its signature changed")),
        "{:#}",
        step["narration"]
    );
    assert_ne!(
        commented["summary"]["risk"], "high",
        "{:#}",
        commented["summary"]
    );

    // The docstring.
    let documented = edit(
        "\"\"\"Encode a string to bytes.\"\"\"",
        "\"\"\"Return bytes, encoding a string first.\"\"\"",
        "Describe want_bytes by what it returns",
    );
    assert_reads_as(
        &documented,
        &Expected {
            verdict: "needs_no_update",
            reason: "behavior_equivalent",
            bucket: "consumers_need_no_update",
            headline: "None of them needs an update: what they rely on kept its signature \
                       and behaves as it did.",
        },
    );

    // The body, under a signature that holds.
    let body = edit(
        "s = s.encode(encoding, errors)",
        "s = s.encode(encoding, errors or \"strict\")",
        "Fall back to strict errors",
    );
    assert_reads_as(
        &body,
        &Expected {
            verdict: "may_need_review",
            reason: "body_changed",
            bucket: "consumers_may_need_review",
            headline: "None of them changed and the code they rely on did, so they may need \
                       review.",
        },
    );

    // The signature: `encoding` loses its default, and both callers pass only
    // the value.
    let signature = edit(
        "encoding: str = \"utf-8\",",
        "encoding: str,",
        "Make the encoding explicit",
    );
    assert_reads_as(
        &signature,
        &Expected {
            verdict: "not_updated",
            reason: "signature_changed",
            bucket: "consumers_not_updated",
            headline: "None of them was updated, and the change breaks what they rely on.",
        },
    );
}
