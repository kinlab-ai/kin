// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The verdict's codes are one closed list, and two things hold it closed.
//!
//! `docs/mcp-tools.md` carries the list's table row for row, so the page a
//! reader looks a code up in cannot drift from the list the verdict reads. And
//! every clause label a producer in kin-mcp or kin-core writes is on the list,
//! so a new reason cannot reach a response as a code nobody can look up. The
//! verdict sends an unlisted label as `unlisted_clause` rather than mint it; this
//! scan is what keeps that fallback from ever being needed.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::verdict::{CLAUSE_CODES, UNLISTED_CLAUSE_CODE};

/// Labels the scan finds that are not verdict clauses, each with where it lives
/// and what it is instead. Every entry must still be found by the scan, so a
/// stale allowance fails rather than quietly widening what passes.
const NOT_CLAUSES: &[(&str, &str)] = &[
    (
        "auto",
        "verbs.rs: a locate mode description in the tool schema, not a retrieval verdict",
    ),
    (
        "derived_member_body_unavailable",
        "handlers/common.rs: a body_unavailable diagnostic for a candidate with no independent body; source_body_reading grades generator-source availability separately",
    ),
    (
        "durable_request_daemon_required",
        "handlers/sessions.rs: a keyed mutation transport-capability refusal, not a retrieval verdict",
    ),
    (
        "durable_request_outcome_unknown",
        "handlers/sessions.rs: a keyed mutation transport outcome error, not a retrieval verdict",
    ),
    (
        "entity_create_required",
        "session.rs: refusal of a raw Entity payload used to create source, not a retrieval verdict",
    ),
    (
        "invalid_request_id",
        "handlers/sessions.rs: a mutation request-identity validation error",
    ),
    (
        "kin_mutate_not_started",
        "handlers/sessions.rs: a mutation begin refusal marker for session recovery, not a retrieval verdict",
    ),
    (
        "module_body_unavailable",
        "handlers/common.rs: a body_unavailable diagnostic refusing aggregate container source; module_members_reading grades member-coverage gaps separately",
    ),
    (
        "parse_hole",
        "kin-core reference_coverage.rs: a census label no MCP verdict carries",
    ),
    ("phase", "startup_binding.rs: a startup progress label"),
    (
        "read_only_session",
        "session.rs: a write refusal for a session that declared itself read-only",
    ),
    (
        "scope",
        "handlers/review.rs: a replacement parameter example in path-anchor deprecations",
    ),
    (
        "semantic_operation_required",
        "session.rs: refusal of file-shaped agent mutations, not a retrieval verdict",
    ),
    (
        "source_base_required",
        "session.rs: refusal of an unguarded entity replacement, not a retrieval verdict",
    ),
    (
        "source_base_changed",
        "source_ticket.rs: a write refused because its edit ticket no longer matches the entity's current source, not a retrieval verdict",
    ),
    (
        "source_base_ticket_invalid",
        "source_ticket.rs: a write refused because its edit ticket was not copied exactly, not a retrieval verdict",
    ),
    (
        "source_base_unavailable",
        "source_base.rs: a source read's unavailable edit-base status, not a retrieval verdict",
    ),
    (
        "section_outline",
        "artifact_entity/parts.rs: why the outline of a text held with parts carries no edit base, not a retrieval verdict",
    ),
    (
        "source_cursor_invalid",
        "source_pages.rs: a source continuation refused because its cursor was not copied exactly, not a retrieval verdict",
    ),
    (
        "source_cursor_stale",
        "source_pages.rs: a source continuation refused because the body changed since its first excerpt, not a retrieval verdict",
    ),
    (
        "source_excerpt",
        "source_pages.rs: why an excerpt carries no edit base, not a retrieval verdict",
    ),
    (
        "spine_unavailable",
        "handlers/review.rs: the cross_repo_impact_status payload field",
    ),
    (
        "staged_byte_limit_exceeded",
        "session.rs: a transaction refusal code",
    ),
    (
        "staged_operation_limit_exceeded",
        "session.rs: a transaction refusal code",
    ),
    (
        "trace_computation",
        "handlers/entities.rs: a tool error message prefix",
    ),
    (
        "trace_data_flow",
        "handlers/entities.rs: a tool error message prefix",
    ),
    (
        "transaction_limit_exceeded",
        "session.rs: a session refusal code",
    ),
    ("warning", "kin-core init_attempt.rs: a CLI warning line"),
];

/// A label written with a placeholder, and every code it can expand to.
const PATTERNS: &[(&str, &[&str])] =
    &[("substrate_{}", &["substrate_partial", "substrate_unknown"])];

/// Listed codes no literal names, each with the source that produces it.
const SOURCED_ELSEWHERE: &[(&str, &str)] = &[
    (
        "binding_unproven",
        "kin-model call_site_reading.rs CallSiteTally::clauses, carried by the call_sites block",
    ),
    (
        "call_sites_not_in_build",
        "kin-model call_site_reading.rs CallSiteTally::clauses, carried by the call_sites block",
    ),
    (
        "call_sites_owed",
        "kin-model call_site_reading.rs CallSiteTally::clauses, carried by the call_sites block",
    ),
    (
        "call_sites_server_failed",
        "kin-model call_site_reading.rs CallSiteTally::clauses, carried by the call_sites block",
    ),
    (
        "call_sites_unproven_no_resolver",
        "kin-model call_site_reading.rs CallSiteTally::clauses, carried by the call_sites block",
    ),
    (
        "call_sites_unresolved",
        "kin-model call_site_reading.rs CallSiteTally::clauses, carried by the call_sites block",
    ),
    (
        "proof_context_stale",
        "kin-model call_site_reading.rs CallSiteTally::clauses, carried by the call_sites block",
    ),
    (
        "proof_context_unverified",
        "kin-model call_site_reading.rs CallSiteTally::clauses, carried by the call_sites block",
    ),
    (
        "cross_repo_unavailable",
        "negative.rs cross_repo_unavailable_qualifier, the label for a spine answer naming no code",
    ),
    (
        "spine_candidate_representation_gap",
        "kin-spine SPINE_CANDIDATE_REPRESENTATION_GAP, the code a daemon's spine refusal opens its reason with, carried as the label",
    ),
    (
        "spine_initialization_deferred",
        "handlers/entities.rs SPINE_INITIALIZATION_DEFERRED, a spine's code carried as the label",
    ),
    (
        "spine_root_stale",
        "handlers/entities.rs SPINE_ROOT_STALE, a spine's code carried as the label",
    ),
    (
        "named_leftovers",
        "kin-model call_site_reading.rs NAMED_LEFTOVERS, written by call_sites.rs named_clauses, carried by the call_sites block",
    ),
    (
        "value_calls_unmeasured",
        "kin-model call_site_reading.rs VALUE_CALLS_UNMEASURED, written by call_sites.rs named_clauses, carried by the call_sites block",
    ),
    (
        "escape_residue_listed",
        "kin-model call_site_reading.rs ESCAPE_RESIDUE_LISTED, written by call_sites.rs named_clauses, carried by the call_sites block",
    ),
    (
        "constructor_dispatch_unlisted",
        "negative.rs constructor_dispatch_gap, the method gate for a constructor whose named scope is complete",
    ),
    (
        "decorator_dispatch_listed",
        "kin-model call_site_reading.rs DECORATOR_DISPATCH_LISTED, written by call_sites.rs named_clauses, carried by the call_sites block",
    ),
    (
        "unkeyed_lookups_counted",
        "kin-model call_site_reading.rs UNKEYED_LOOKUPS_COUNTED, written by call_sites.rs named_clauses and trace_named_scope.rs block, carried by the call_sites block",
    ),
    (
        "declaration_census_unmeasured",
        "declaration_coverage.rs CENSUS_UNMEASURED_CODE, carried by the declarations block",
    ),
    (
        "declarations_unadmitted_listed",
        "declaration_coverage.rs UNADMITTED_LISTED_CODE, carried by the declarations block",
    ),
    (
        "declarations_unparsed_files_listed",
        "declaration_coverage.rs UNPARSED_FILES_LISTED_CODE, carried by the declarations block",
    ),
    (
        "reference_completion_unmeasured",
        "kin-model call_site_reading.rs REFERENCE_COMPLETION_UNMEASURED, written by verdict.rs reference_completion_gap",
    ),
    (
        "reference_mentions_unmatched",
        "kin-model call_site_reading.rs REFERENCE_MENTIONS_UNMATCHED, written by verdict.rs reference_completion_gap from the census's residue.mentions",
    ),
    (
        "reference_sites_undecided",
        "kin-model call_site_reading.rs REFERENCE_SITES_UNDECIDED, written by verdict.rs undecided_reference_sites_gap from the undecided_reference_sites block",
    ),
    (
        "reference_sites_undecided_listed",
        "kin-model call_site_reading.rs REFERENCE_SITES_UNDECIDED_LISTED, written by verdict.rs undecided_reference_sites_gap from the undecided_reference_sites block",
    ),
    ("unlisted_clause", "verdict.rs, the fallback itself"),
];

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .unwrap_or_else(|error| panic!("read {}: {error}", dir.display()))
        .map(|entry| entry.expect("directory entry").path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// The production half of a source file: everything before its test module,
/// with line comments dropped, so a doc example or a test fixture cannot put a
/// label on the list or take one off it.
fn production_source(text: &str) -> String {
    let body = match text.find("\n#[cfg(test)]") {
        Some(cut) => &text[..cut],
        None => text,
    };
    body.lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Whether an attribute requires the bare `test` predicate.
///
/// A direct `test` or an `all` with a direct `test` counts. `not(test)` and
/// `any(test, feature = "test-support")` can enable production source and
/// must stay in the scan. More complex predicates stay in conservatively.
fn cfg_names_test(line: &str) -> bool {
    let Some(predicate) = line
        .strip_prefix("#[cfg(")
        .and_then(|rest| rest.strip_suffix(")]"))
    else {
        return false;
    };
    if predicate.trim() == "test" {
        return true;
    }
    predicate
        .strip_prefix("all(")
        .and_then(|rest| rest.strip_suffix(')'))
        .is_some_and(|terms| {
            !terms.contains('(')
                && !terms.contains(')')
                && terms.split(',').any(|term| term.trim() == "test")
        })
}

/// The file an `#[path = "..."]` attribute names.
fn path_attribute(line: &str) -> Option<&str> {
    line.strip_prefix("#[path = \"")?.split('"').next()
}

/// The files a crate includes only under `#[cfg(test)]`, as the walk names
/// them.
///
/// A whole-file test module's declaration carries `#[cfg(test)]`, while its
/// own text need not. Without reading that declaration, a fixture byte string
/// would count as a producer writing a label. Both ordinary module paths and
/// explicit `#[path]` declarations are checked; an ungated module stays in
/// the scan.
fn test_only_modules(files: &[PathBuf]) -> BTreeSet<PathBuf> {
    let mut excluded = BTreeSet::new();
    for file in files {
        let (Some(directory), Ok(text)) = (file.parent(), std::fs::read_to_string(file)) else {
            continue;
        };
        let module_directory = if matches!(
            file.file_name().and_then(|name| name.to_str()),
            Some("lib.rs" | "main.rs" | "mod.rs")
        ) {
            directory.to_path_buf()
        } else {
            directory.join(file.file_stem().expect("a Rust source file has a stem"))
        };
        let mut test_gated = false;
        let mut explicit_path = false;
        for line in text.lines() {
            let line = line.trim();
            // A doc comment or a blank line sits inside the attribute stack it
            // documents, so neither opens nor closes one.
            if line.is_empty() || line.starts_with("//") {
                continue;
            }
            if let Some(name) = path_attribute(line) {
                explicit_path = true;
                if test_gated {
                    excluded.insert(directory.join(name));
                }
                continue;
            }
            if test_gated && !explicit_path {
                let declaration = line
                    .strip_prefix("pub(crate) ")
                    .or_else(|| line.strip_prefix("pub(super) "))
                    .or_else(|| line.strip_prefix("pub(self) "))
                    .or_else(|| line.strip_prefix("pub "))
                    .unwrap_or(line);
                if let Some(name) = declaration
                    .strip_prefix("mod ")
                    .and_then(|rest| rest.strip_suffix(';'))
                    .filter(|name| {
                        !name.is_empty()
                            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                    })
                {
                    excluded.insert(module_directory.join(format!("{name}.rs")));
                    excluded.insert(module_directory.join(name).join("mod.rs"));
                }
            }
            if cfg_names_test(line) {
                test_gated = true;
            } else if !line.starts_with("#[") {
                test_gated = false;
                explicit_path = false;
            }
        }
    }
    excluded
}

#[test]
fn whole_file_test_modules_are_excluded_without_hiding_producers() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("lib.rs");
    std::fs::write(
        &root,
        "#[cfg(test)]\nmod ordinary_tests;\n\
         #[cfg(all(test, unix))]\n#[path = \"custom_tests.rs\"]\nmod custom;\n\
         #[cfg(feature = \"testing\")]\nmod production;\n\
         #[cfg(not(test))]\nmod non_test_production;\n\
         #[cfg(any(test, feature = \"test-support\"))]\nmod shared_production;\n\
         #[cfg(all(any(unix, test, windows), feature = \"test-support\"))]\nmod nested_production;\n\
         #[cfg(test)]\npub(crate) mod visible_tests;\n\
         #[path = \"other_production.rs\"]\nmod other;\n",
    )
    .unwrap();
    let owner = dir.path().join("owner.rs");
    std::fs::write(&owner, "#[cfg(test)]\nmod tests;\n").unwrap();
    let excluded = test_only_modules(&[root, owner]);
    assert!(excluded.contains(&dir.path().join("ordinary_tests.rs")));
    assert!(excluded.contains(&dir.path().join("ordinary_tests/mod.rs")));
    assert!(excluded.contains(&dir.path().join("custom_tests.rs")));
    assert!(!excluded.contains(&dir.path().join("custom.rs")));
    assert!(!excluded.contains(&dir.path().join("production.rs")));
    assert!(!excluded.contains(&dir.path().join("non_test_production.rs")));
    assert!(!excluded.contains(&dir.path().join("shared_production.rs")));
    assert!(!excluded.contains(&dir.path().join("nested_production.rs")));
    assert!(excluded.contains(&dir.path().join("visible_tests.rs")));
    assert!(excluded.contains(&dir.path().join("owner/tests.rs")));
    assert!(!excluded.contains(&dir.path().join("tests.rs")));
    assert!(!excluded.contains(&dir.path().join("other_production.rs")));
}

/// Every label a string literal opens with (`"label: ...`), and every value of
/// a `*_LIMITING_FACTOR` constant.
fn labels_in(source: &str) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    let bytes = source.as_bytes();
    for (index, _) in source.match_indices('"') {
        let rest = &source[index + 1..];
        let label: String = rest
            .chars()
            .take_while(|c| {
                c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '{' | '}')
            })
            .collect();
        let opens_clause = !label.is_empty()
            && label.as_bytes()[0].is_ascii_lowercase()
            && rest[label.len()..].starts_with(": ")
            && (index == 0 || bytes[index - 1] != b'\\');
        if opens_clause {
            found.insert(label);
        }
    }
    for (index, _) in source.match_indices("_LIMITING_FACTOR: &str = \"") {
        let value_start = index + "_LIMITING_FACTOR: &str = \"".len();
        if let Some(end) = source[value_start..].find('"') {
            found.insert(source[value_start..value_start + end].to_string());
        }
    }
    found
}

fn scanned_labels() -> BTreeSet<String> {
    let mut files = Vec::new();
    rust_files(&crate_dir().join("src"), &mut files);
    rust_files(&crate_dir().join("../kin-core/src"), &mut files);
    assert!(
        files.len() > 20,
        "the scan read {} files, so it read almost nothing",
        files.len()
    );
    let test_only = test_only_modules(&files);
    let mut labels = BTreeSet::new();
    for file in files {
        if test_only.contains(&file) {
            continue;
        }
        let text = std::fs::read_to_string(&file)
            .unwrap_or_else(|error| panic!("read {}: {error}", file.display()));
        labels.extend(labels_in(&production_source(&text)));
    }
    labels
}

#[test]
fn every_clause_label_a_producer_writes_is_listed() {
    let scanned = scanned_labels();
    // The control: a label the verdict is known to write, and enough of them
    // that a scanner which matched nothing cannot pass.
    assert!(
        scanned.contains("edge_coverage_unknown"),
        "the scan missed a known producer: {scanned:?}"
    );
    assert!(
        scanned.len() >= 50,
        "the scan found only {} labels: {scanned:?}",
        scanned.len()
    );

    let listed: BTreeSet<&str> = CLAUSE_CODES.iter().map(|entry| entry.code).collect();
    let not_clauses: BTreeSet<&str> = NOT_CLAUSES.iter().map(|(label, _)| *label).collect();
    let mut unlisted = Vec::new();
    let mut produced: BTreeSet<String> = BTreeSet::new();
    for label in &scanned {
        if not_clauses.contains(label.as_str()) {
            continue;
        }
        if label.contains('{') {
            match PATTERNS.iter().find(|(pattern, _)| pattern == label) {
                Some((_, codes)) => produced.extend(codes.iter().map(|code| code.to_string())),
                None => unlisted.push(format!(
                    "{label} (a placeholder label with no PATTERNS entry)"
                )),
            }
            continue;
        }
        if listed.contains(label.as_str()) {
            produced.insert(label.clone());
        } else {
            unlisted.push(label.clone());
        }
    }
    assert!(
        unlisted.is_empty(),
        "a producer writes clause labels the closed list does not carry, so the verdict would send \
         them as {UNLISTED_CLAUSE_CODE}: {unlisted:?}. List each in verdict.rs CLAUSE_CODES and in \
         docs/mcp-tools.md, or name it in NOT_CLAUSES with what it is instead."
    );

    let stale_allowances: Vec<&str> = NOT_CLAUSES
        .iter()
        .map(|(label, _)| *label)
        .filter(|label| !scanned.contains(*label))
        .collect();
    assert!(
        stale_allowances.is_empty(),
        "NOT_CLAUSES names labels no source writes: {stale_allowances:?}"
    );

    let elsewhere: BTreeSet<&str> = SOURCED_ELSEWHERE.iter().map(|(code, _)| *code).collect();
    let never_produced: Vec<&str> = listed
        .iter()
        .copied()
        .filter(|code| !produced.contains(*code) && !elsewhere.contains(code))
        .collect();
    assert!(
        never_produced.is_empty(),
        "CLAUSE_CODES lists codes no producer writes, so the docs promise codes a reader will never \
         see: {never_produced:?}"
    );
}

#[test]
fn the_docs_code_table_is_the_closed_list() {
    let path = crate_dir().join("../../docs/mcp-tools.md");
    let docs = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    let begin = docs
        .find("<!-- clause-codes:begin -->")
        .expect("the docs carry the table's begin marker");
    let end = docs
        .find("<!-- clause-codes:end -->")
        .expect("the docs carry the table's end marker");
    let rows: Vec<&str> = docs[begin..end]
        .lines()
        .filter(|line| line.starts_with("| `"))
        .collect();
    let expected: Vec<String> = CLAUSE_CODES
        .iter()
        .map(|entry| format!("| `{}` | {} |", entry.code, entry.meaning))
        .collect();
    assert_eq!(
        rows,
        expected.iter().map(String::as_str).collect::<Vec<_>>(),
        "docs/mcp-tools.md and CLAUSE_CODES disagree; the rows the docs should carry are:\n{}",
        expected.join("\n")
    );
}

#[test]
fn the_list_is_sorted_unique_and_holds_no_separator() {
    let codes: Vec<&str> = CLAUSE_CODES.iter().map(|entry| entry.code).collect();
    let mut sorted = codes.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(
        codes, sorted,
        "CLAUSE_CODES must be in code order with no duplicate"
    );
    for entry in CLAUSE_CODES {
        assert!(
            entry
                .code
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
            "a code is snake_case and so can never hold the separator: {}",
            entry.code
        );
        assert!(
            !entry.meaning.contains('|'),
            "a meaning with a pipe breaks the docs table: {}",
            entry.code
        );
    }
}

/// Every listed code's meaning reaches an agent as written. The semantic
/// surface stands a clause's meaning in for a clause it withholds, so a
/// meaning that is itself physical left the agent a withheld note where the
/// reason belonged.
#[test]
fn every_listed_meaning_reaches_an_agent_as_written() {
    let mut withheld = Vec::new();
    for entry in CLAUSE_CODES {
        let clause = format!("{}: {}", entry.code, entry.meaning);
        if !crate::semantic_surface::is_semantic(entry.meaning)
            || crate::semantic_surface::diagnostic(&clause) != clause
        {
            withheld.push(entry.code);
        }
    }
    assert!(
        withheld.is_empty(),
        "the semantic surface withholds these codes' meanings, so an agent reads a withheld \
         note in their place; say it in terms of entities, modules and relationships: \
         {withheld:?}"
    );
}

/// The `const NAME: &str = "code";` declarations that name a listed code, so
/// a clause opened with `{NAME}: ` is read as that code's clause.
fn code_constants(sources: &[String]) -> std::collections::BTreeMap<String, String> {
    let listed: BTreeSet<&str> = CLAUSE_CODES.iter().map(|entry| entry.code).collect();
    let mut constants = std::collections::BTreeMap::new();
    for source in sources {
        for line in source.lines() {
            let Some(rest) = line
                .trim_start()
                .trim_start_matches("pub(crate) ")
                .trim_start_matches("pub ")
                .strip_prefix("const ")
            else {
                continue;
            };
            let Some((name, value)) = rest.split_once(": &str = \"") else {
                continue;
            };
            let value = value.trim_end_matches("\";");
            if listed.contains(value) {
                constants.insert(name.to_string(), value.to_string());
            }
        }
    }
    constants
}

/// The text of the string literal that opens at `rest`, its line
/// continuations and escaped quotes resolved.
fn literal_text(rest: &str) -> String {
    let mut text = String::new();
    let mut chars = rest.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => break,
            '\\' => match chars.next() {
                Some('\n') => {
                    while chars.peek().is_some_and(|c| c.is_whitespace()) {
                        chars.next();
                    }
                }
                Some('n') => text.push('\n'),
                Some(other) => text.push(other),
                None => break,
            },
            other => text.push(other),
        }
    }
    text
}

/// Every clause literal in `source` that opens with a listed code, or with a
/// constant naming one, as `(code, text)`, its placeholders read as a neutral
/// word: what a placeholder holds is data, and this test reads the wording.
fn clause_literals_in(
    source: &str,
    constants: &std::collections::BTreeMap<String, String>,
) -> Vec<(String, String)> {
    let listed: BTreeSet<&str> = CLAUSE_CODES.iter().map(|entry| entry.code).collect();
    let mut found = Vec::new();
    for (index, _) in source.match_indices('"') {
        if index > 0 && source.as_bytes()[index - 1] == b'\\' {
            continue;
        }
        let rest = &source[index + 1..];
        let label: String = if rest.starts_with('{') {
            rest.chars()
                .take_while(|c| {
                    c.is_ascii_uppercase() || c.is_ascii_digit() || matches!(c, '_' | '{' | '}')
                })
                .collect()
        } else {
            rest.chars()
                .take_while(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '_')
                .collect()
        };
        if label.is_empty() || !rest[label.len()..].starts_with(": ") {
            continue;
        }
        let label = label.as_str();
        let code = if let Some(name) = label.strip_prefix('{').and_then(|l| l.strip_suffix('}')) {
            constants.get(name).cloned()
        } else {
            listed.contains(label).then(|| label.to_string())
        };
        let Some(code) = code else {
            continue;
        };
        let mut text = literal_text(rest);
        // Placeholders, never the doubled braces a format string escapes.
        text = text.replace("{{", "\u{1}").replace("}}", "\u{2}");
        let mut neutral = String::new();
        let mut depth = 0usize;
        for c in text.chars() {
            match c {
                '{' => {
                    depth += 1;
                    if depth == 1 {
                        neutral.push('x');
                    }
                }
                '}' if depth > 0 => depth -= 1,
                _ if depth > 0 => {}
                '\u{1}' => neutral.push('{'),
                '\u{2}' => neutral.push('}'),
                other => neutral.push(other),
            }
        }
        found.push((code, neutral));
    }
    found
}

/// Every clause a producer writes under a listed code reaches an agent in its
/// own words. A clause the surface withholds still reaches the agent as its
/// code's meaning, but the producer's own sentence, with the language, the
/// classes and the counts it names, is lost, so a producer writes its clause
/// in terms of entities, modules and relationships, never storage.
#[test]
fn every_clause_a_producer_writes_reaches_an_agent_as_written() {
    let mut files = Vec::new();
    rust_files(&crate_dir().join("src"), &mut files);
    rust_files(&crate_dir().join("../kin-core/src"), &mut files);
    let test_only = test_only_modules(&files);
    let sources: Vec<String> = files
        .iter()
        .filter(|file| !test_only.contains(*file))
        .map(|file| {
            production_source(
                &std::fs::read_to_string(file)
                    .unwrap_or_else(|error| panic!("read {}: {error}", file.display())),
            )
        })
        .collect();
    let constants = code_constants(&sources);
    assert!(
        constants.contains_key("UNMEASURED_ARRIVAL_LIMITING_FACTOR"),
        "the constant scan missed a known clause constant: {constants:?}"
    );
    let clauses: Vec<(String, String)> = sources
        .iter()
        .flat_map(|source| clause_literals_in(source, &constants))
        .collect();
    assert!(
        clauses
            .iter()
            .any(|(code, _)| code == "cross_file_edges_absent")
            && clauses.len() >= 40,
        "the scan found only {} clause literals: {clauses:?}",
        clauses.len()
    );
    let withheld: Vec<&(String, String)> = clauses
        .iter()
        .filter(|(_, text)| !crate::semantic_surface::is_semantic(text))
        .collect();
    assert!(
        withheld.is_empty(),
        "the semantic surface withholds these producers' clauses: {withheld:#?}"
    );
}

#[test]
fn the_clause_literal_scan_reads_continuations_constants_and_placeholders() {
    let constants = std::collections::BTreeMap::from([(
        "ARRIVAL".to_string(),
        "caller_arrival_unmeasured".to_string(),
    )]);
    let source = "let a = format!(\"cross_file_edges_absent: the graph holds no {missing} \\\n    edges for {{x}} {language}\");\nlet b = format!(\"{ARRIVAL}: across files\");\nlet c = \"not_a_code: ignored\";";
    let found = clause_literals_in(source, &constants);
    assert_eq!(
        found,
        vec![
            (
                "cross_file_edges_absent".to_string(),
                "cross_file_edges_absent: the graph holds no x edges for {x} x".to_string()
            ),
            (
                "caller_arrival_unmeasured".to_string(),
                "x: across files".to_string()
            ),
        ]
    );
    assert!(!crate::semantic_surface::is_semantic(&found[1].1));
}
