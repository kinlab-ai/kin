// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Which of this crate's sources decide what a proof says, and the identity
//! taken over them.
//!
//! The identity is part of every proof context, so when it moves every proof a
//! store holds is asked about again. It therefore has to move when the code
//! that decides a proof changes, and only then: a constant that times a
//! startup probe once moved it and re-asked every file of every store.
//!
//! Every file under `src/` is named here, in exactly one of two lists.
//! [`PROOF_DECIDING`] is hashed. [`NOT_PROOF_DECIDING`] is not, and each entry
//! says why in one line. A file in neither list, in both, or listed and
//! missing fails the build, so a new file forces the decision instead of
//! defaulting either way. When in doubt a file belongs in [`PROOF_DECIDING`]:
//! leaving out a file that does decide a proof lets stale proofs survive
//! silently, while keeping one that does not only costs a re-ask.
//!
//! Tests never move the identity. A test file is listed as not deciding, and
//! a hashed Rust file is hashed without its top-level `#[cfg(test)]` items
//! (see [`without_tests`]).
//!
//! `build.rs` embeds the identity, and `tests/producer_identity.rs` includes
//! this same file to prove the properties above against the real sources.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// Sources that decide what an answer proves or how it is recorded, relative
/// to `src/`.
pub const PROOF_DECIDING: &[&str] = &[
    // How each language's server is configured, and how its quirks are read.
    "adapters/clangd.rs",
    "adapters/contract.rs",
    "adapters/go.rs",
    "adapters/java.rs",
    "adapters/mod.rs",
    "adapters/python.rs",
    "adapters/repo_scan.rs",
    "adapters/rust_analyzer.rs",
    "adapters/tsserver_workspace_sources.js",
    "adapters/typescript.rs",
    // What the server resolves dependencies against: which packages a lock
    // yields, which files are taken for them and how they are laid out.
    "analysis_env/fetch.rs",
    "analysis_env/go/mod.rs",
    "analysis_env/go/modcache.rs",
    "analysis_env/go/sumdb.rs",
    "analysis_env/go/toolchain.rs",
    "analysis_env/js/layout.rs",
    "analysis_env/js/lockfile.rs",
    "analysis_env/js/mod.rs",
    "analysis_env/js/yaml.rs",
    "analysis_env/mod.rs",
    "analysis_env/python/index.rs",
    "analysis_env/python/interpreter.rs",
    "analysis_env/python/lockfile.rs",
    "analysis_env/python/markers.rs",
    "analysis_env/python/mod.rs",
    "analysis_env/python/plan.rs",
    "analysis_env/python/provision.rs",
    "analysis_env/python/store.rs",
    "analysis_env/python/tags.rs",
    "analysis_env/python/unlocked.rs",
    "analysis_env/rust/lockfile.rs",
    "analysis_env/rust/mod.rs",
    "analysis_env/rust/registry.rs",
    "analysis_env/rust/toolchain.rs",
    "analysis_env/unpack.rs",
    // Which stored result is reused in place of asking.
    "cache.rs",
    // What a question is, how its answer is read, and what is recorded.
    "call_sites.rs",
    "client.rs",
    "concurrency.rs",
    // Which server runs and with which arguments. The arguments are in no
    // configuration hash.
    "discovery.rs",
    "enrichment.rs",
    "error.rs",
    "external_symbols.rs",
    "file_enrichment.rs",
    // The initialize handshake and when a server counts as loaded.
    "lifecycle.rs",
    "proof.rs",
    "proof_context.rs",
    "protocol.rs",
    "registry.rs",
    "relation_identity.rs",
    "source_positions.rs",
    "typescript_call_hierarchy.rs",
];

/// Sources that decide no proof, relative to `src/`, each with the reason.
pub const NOT_PROOF_DECIDING: &[(&str, &str)] = &[
    (
        "concurrency_test_peer.py",
        "a scripted peer that only tests start",
    ),
    ("concurrency_tests.rs", "tests"),
    ("enrichment_integrity_tests.rs", "tests"),
    (
        "enrichment_test_peer.py",
        "a scripted peer that only tests start",
    ),
    (
        "lib.rs",
        "declares the crate's modules and re-exports their names; it holds no code of its own",
    ),
    (
        "pace.rs",
        "stretches how long a question may wait; one that runs out is recorded as a server \
         failure and asked again, never as a proof",
    ),
    (
        "readiness_probe.rs",
        "asks only whether a server starts on this host; it sends no question and records \
         nothing",
    ),
    (
        "server_process.rs",
        "owns and ends the server's process group; it never sees a question or an answer",
    ),
    (
        "start_failure.rs",
        "phrases why a server did not start; it sees no question and decides no answer",
    ),
];

/// Producer sources outside this crate, relative to its manifest directory.
/// Hashed when present: a packaged build of this crate alone has none.
///
/// The daemon's ledger gives every call site its state. Two parser sources
/// decide one of those states and leave no record a store could notice a
/// change by: which calls go through a name their own caller binds. A
/// language added to that reading adds its source here.
pub const WORKSPACE_PRODUCERS: &[&str] = &[
    "../kin-daemon/src/call_site_ledger.rs",
    "../kin-parser/src/languages/javascript_escape.rs",
    "../kin-parser/src/value_escape.rs",
];

/// The domain of the identity. The second version hashes the classified
/// sources without their tests, where the first hashed every Rust file whole.
const DOMAIN: &[u8] = b"kin.enrichment-producer.v2\0";

/// The files the identity reads, so a build script can watch each one.
pub fn watched(manifest_dir: &Path) -> Vec<PathBuf> {
    let mut paths = vec![manifest_dir.join("src")];
    paths.extend(
        WORKSPACE_PRODUCERS
            .iter()
            .map(|producer| manifest_dir.join(producer)),
    );
    paths
}

/// The identity of the proof-deciding sources under `manifest_dir`, as
/// `sha256:<hex>`, or every reason the classification does not hold.
pub fn identity(manifest_dir: &Path) -> Result<String, Vec<String>> {
    let source = manifest_dir.join("src");
    let mut present = Vec::new();
    collect(&source, &source, &mut present);
    let present: BTreeSet<String> = present.into_iter().collect();
    let deciding: BTreeSet<&str> = PROOF_DECIDING.iter().copied().collect();
    let excluded: BTreeSet<&str> = NOT_PROOF_DECIDING.iter().map(|(file, _)| *file).collect();

    let mut problems = Vec::new();
    if deciding.len() != PROOF_DECIDING.len() || excluded.len() != NOT_PROOF_DECIDING.len() {
        problems.push("a source file is listed twice in one list".to_string());
    }
    for file in deciding.intersection(&excluded) {
        problems.push(format!(
            "src/{file} is listed as proof-deciding and as not proof-deciding"
        ));
    }
    for file in &present {
        if !deciding.contains(file.as_str()) && !excluded.contains(file.as_str()) {
            problems.push(format!(
                "src/{file} is in neither list: add it to PROOF_DECIDING if it can change what \
                 an answer proves or how it is recorded, and when unsure; otherwise add it to \
                 NOT_PROOF_DECIDING with the reason"
            ));
        }
    }
    for file in deciding.union(&excluded) {
        if !present.contains(*file) {
            problems.push(format!("src/{file} is listed and does not exist"));
        }
    }
    for (file, reason) in NOT_PROOF_DECIDING {
        if reason.trim().is_empty() {
            problems.push(format!("src/{file} is excluded without a reason"));
        }
    }

    let mut inputs: Vec<(String, PathBuf)> = deciding
        .iter()
        .map(|file| (format!("src/{file}"), source.join(file)))
        .collect();
    for producer in WORKSPACE_PRODUCERS {
        let path = manifest_dir.join(producer);
        if path.is_file() {
            inputs.push(((*producer).to_string(), path));
        }
    }
    inputs.sort();

    let mut hasher = Sha256::new();
    hasher.update(DOMAIN);
    for (name, path) in &inputs {
        let Ok(bytes) = fs::read(path) else {
            // Reported above as listed and missing, or unreadable here.
            if path.exists() {
                problems.push(format!("{name} could not be read"));
            }
            continue;
        };
        let hashed = if name.ends_with(".rs") {
            match String::from_utf8(bytes) {
                Ok(text) => match without_tests(&text) {
                    Ok(kept) => kept.into_bytes(),
                    Err(reason) => {
                        problems.push(format!("{name}: {reason}"));
                        continue;
                    }
                },
                Err(_) => {
                    problems.push(format!("{name} is not UTF-8"));
                    continue;
                }
            }
        } else {
            bytes
        };
        hasher.update(name.as_bytes());
        hasher.update(b"\0");
        hasher.update((hashed.len() as u64).to_le_bytes());
        hasher.update(&hashed);
    }
    if !problems.is_empty() {
        return Err(problems);
    }
    let digest: String = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Ok(format!("sha256:{digest}"))
}

/// Every file under `directory`, as its path relative to `root` with `/`.
fn collect(root: &Path, directory: &Path, files: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(root, &path, files);
        } else if let Ok(relative) = path.strip_prefix(root) {
            files.push(relative.to_string_lossy().replace('\\', "/"));
        }
    }
}

/// `source` without its top-level `#[cfg(test)]` items: test modules, and the
/// functions, types, statics and declarations only tests compile.
///
/// The file is read as a run of top-level items, each with the comments and
/// attributes written above it, and the items that carry an attribute spelled
/// exactly `#[cfg(test)]` are left out. The rest are joined by one line break
/// each, so neither a test nor the blank lines around one leaves a trace. An
/// item under `#[cfg(any(test, ...))]` also compiles outside tests, and one
/// nested inside other code is a seam in that code, so both stay.
///
/// The text is read as Rust tokens (comments, string and raw string literals,
/// character literals and lifetimes), never by indentation, so a brace inside
/// a test's string ends nothing.
///
/// An item whose end cannot be found is an error, never a silent cut.
pub fn without_tests(source: &str) -> Result<String, String> {
    let mut kept = String::with_capacity(source.len());
    let mut at = skip_whitespace(source, 0);
    while at < source.len() {
        let item = top_level_item(source, at).ok_or_else(|| {
            format!(
                "could not find where the item that starts on line {} ends",
                source[..at].lines().count() + 1
            )
        })?;
        if !item.test_only {
            if !kept.is_empty() {
                kept.push('\n');
            }
            kept.push_str(&source[at..item.end]);
        }
        at = skip_whitespace(source, item.end);
    }
    Ok(kept)
}

/// One top-level item, from the first comment or attribute above it.
struct Item {
    /// Just past its last token.
    end: usize,
    /// Whether an attribute on it is exactly `#[cfg(test)]`.
    test_only: bool,
}

/// The item that starts at `at`, or `None` when it never ends.
///
/// An item ends after the `;` that ends it, or after the brace that closes
/// its body and a `;` right behind that. An inner attribute (`#![...]`) is an
/// item of its own. Comments with nothing after them are a last item.
fn top_level_item(source: &str, at: usize) -> Option<Item> {
    let bytes = source.as_bytes();
    let mut cursor = at;
    let mut test_only = false;
    // The comments and outer attributes above the item.
    loop {
        let token = skip_blank(source, cursor);
        if token >= bytes.len() {
            return Some(Item {
                end: bytes.len(),
                test_only,
            });
        }
        if bytes[token] != b'#' {
            cursor = token;
            break;
        }
        let opening = skip_blank(source, token + 1);
        let inner = bytes.get(opening) == Some(&b'!');
        let bracket = if inner {
            skip_blank(source, opening + 1)
        } else {
            opening
        };
        if bytes.get(bracket) != Some(&b'[') {
            cursor = token;
            break;
        }
        let (end, spelled) = attribute(source, token)?;
        if inner {
            // It configures the file, not the item below it.
            return Some(Item {
                end,
                test_only: false,
            });
        }
        test_only |= spelled == "#[cfg(test)]";
        cursor = end;
    }
    let (mut braces, mut parens, mut brackets) = (0usize, 0usize, 0usize);
    loop {
        cursor = skip_blank(source, cursor);
        let byte = *bytes.get(cursor)?;
        let next = skip_token(source, cursor);
        match byte {
            b'(' => parens += 1,
            b')' => parens = parens.checked_sub(1)?,
            b'[' => brackets += 1,
            b']' => brackets = brackets.checked_sub(1)?,
            b'{' => braces += 1,
            b'}' => {
                braces = braces.checked_sub(1)?;
                if braces == 0 && parens == 0 && brackets == 0 {
                    let behind = skip_blank(source, next);
                    let end = if bytes.get(behind) == Some(&b';') {
                        behind + 1
                    } else {
                        next
                    };
                    return Some(Item { end, test_only });
                }
            }
            b';' if braces == 0 && parens == 0 && brackets == 0 => {
                return Some(Item {
                    end: next,
                    test_only,
                });
            }
            _ => {}
        }
        cursor = next;
    }
}

/// The attribute that starts at the `#` at `at`: where it ends, and its text
/// without whitespace or comments.
fn attribute(source: &str, at: usize) -> Option<(usize, String)> {
    let bytes = source.as_bytes();
    let mut spelled = String::new();
    let mut cursor = at;
    let mut brackets = 0usize;
    loop {
        cursor = skip_blank(source, cursor);
        let byte = *bytes.get(cursor)?;
        let next = skip_token(source, cursor);
        spelled.push_str(&source[cursor..next]);
        match byte {
            b'[' => brackets += 1,
            b']' => {
                brackets = brackets.checked_sub(1)?;
                if brackets == 0 {
                    return Some((next, spelled));
                }
            }
            _ => {}
        }
        cursor = next;
    }
}

/// Past any whitespace at `at`.
fn skip_whitespace(source: &str, mut at: usize) -> usize {
    let bytes = source.as_bytes();
    while bytes.get(at).is_some_and(u8::is_ascii_whitespace) {
        at += 1;
    }
    at
}

/// Past any whitespace and comments at `at`.
fn skip_blank(source: &str, mut at: usize) -> usize {
    let bytes = source.as_bytes();
    loop {
        match bytes.get(at) {
            Some(byte) if byte.is_ascii_whitespace() => at += 1,
            Some(b'/') if matches!(bytes.get(at + 1), Some(b'/' | b'*')) => {
                at = skip_token(source, at);
            }
            _ => return at,
        }
    }
}

/// Past the token that starts at `at`: a comment, a string or character
/// literal, an identifier, or one other character.
fn skip_token(source: &str, at: usize) -> usize {
    let bytes = source.as_bytes();
    let rest = &source[at..];
    match bytes[at] {
        b'/' if rest.starts_with("//") => at + rest.find('\n').unwrap_or(rest.len()),
        b'/' if rest.starts_with("/*") => {
            let mut cursor = at + 2;
            let mut nesting = 1usize;
            while cursor < bytes.len() && nesting > 0 {
                if source[cursor..].starts_with("/*") {
                    nesting += 1;
                    cursor += 2;
                } else if source[cursor..].starts_with("*/") {
                    nesting -= 1;
                    cursor += 2;
                } else {
                    cursor += 1;
                }
            }
            cursor
        }
        b'"' => skip_string(source, at),
        b'\'' => skip_quote(source, at),
        byte if byte == b'_' || byte.is_ascii_alphabetic() => {
            let mut end = at + 1;
            while bytes
                .get(end)
                .is_some_and(|byte| *byte == b'_' || byte.is_ascii_alphanumeric())
            {
                end += 1;
            }
            let hashes = bytes[end..]
                .iter()
                .take_while(|byte| **byte == b'#')
                .count();
            match &source[at..end] {
                "r" | "br" | "cr" if bytes.get(end + hashes) == Some(&b'"') => {
                    skip_raw_string(source, end + hashes + 1, hashes)
                }
                "b" | "c" if bytes.get(end) == Some(&b'"') => skip_string(source, end),
                "b" if bytes.get(end) == Some(&b'\'') => skip_quote(source, end),
                _ => end,
            }
        }
        _ => at + rest.chars().next().map_or(1, char::len_utf8),
    }
}

/// Past the string literal whose opening quote is at `at`.
fn skip_string(source: &str, at: usize) -> usize {
    let bytes = source.as_bytes();
    let mut cursor = at + 1;
    while let Some(byte) = bytes.get(cursor) {
        match byte {
            b'\\' => cursor += 2,
            b'"' => return cursor + 1,
            _ => cursor += 1,
        }
    }
    bytes.len()
}

/// Past the raw string whose text starts at `at`, closed by a quote and
/// `hashes` hash signs.
fn skip_raw_string(source: &str, at: usize, hashes: usize) -> usize {
    let closing = format!("\"{}", "#".repeat(hashes));
    source[at..]
        .find(&closing)
        .map_or(source.len(), |found| at + found + closing.len())
}

/// Past the character literal at `at`, or past the quote alone when it opens
/// a lifetime or a label.
fn skip_quote(source: &str, at: usize) -> usize {
    let mut rest = source[at + 1..].chars();
    match rest.next() {
        Some('\\') => {
            // An escape: the escaped character, then everything to the quote.
            let escaped = rest.next().map_or(0, char::len_utf8);
            let from = at + 2 + escaped;
            source[from.min(source.len())..]
                .find('\'')
                .map_or(source.len(), |found| from + found + 1)
        }
        Some(character) if rest.next() == Some('\'') => at + 1 + character.len_utf8() + 1,
        _ => at + 1,
    }
}
