// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Public semantic records, projected only after the complete internal
//! evidence has determined the verdict and before records are fragmented.

use serde_json::{json, Map, Value};

pub const VERSION: u64 = 1;

/// This is a presentation boundary, never an input to authority evaluation.
/// Program text remains byte-for-byte data. Transport field addresses and
/// semantic relationship paths are not repository coordinates.
pub fn project(tool: &str, payload: &mut Value) {
    // Tool descriptions are typed registry records, maintained at registration.
    // They are not evidence records and must not be rewritten as one.
    if matches!(tool, "kin_tool_search" | "kin") {
        if let Some(text) = payload
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_owned)
        {
            payload["message"] = json!(diagnostic(&text));
        }
        return;
    }
    freeze_binding_reading(payload);
    project_record(payload, "");
    if let Some(envelope) = payload.get_mut("_kin").and_then(Value::as_object_mut) {
        envelope.insert("semantic_surface".into(), json!(VERSION));
    }
}

/// Last egress boundary, shared by every client presentation mode. Pages are
/// admitted only when the public projection preceded freezing and sizing.
pub fn present_result(result: &mut Value, tool: &str) {
    present_result_with(result, tool, false);
}

/// [`present_result`], and with `answer` the account of the answer attached
/// to its verdict as `_kin.verdict.answer` before the reply's public cost is
/// settled. A citable profile's bytes are an input to a result, so it is
/// presented without one.
pub fn present_result_with(result: &mut Value, tool: &str, answer: bool) {
    if let Some(content) = result.get_mut("content").and_then(Value::as_array_mut) {
        for block in content {
            let Some(text) = block.get("text").and_then(Value::as_str).map(str::to_owned) else {
                continue;
            };
            match serde_json::from_str::<Value>(&text) {
                Ok(payload) if crate::trace_pages::is_page(&payload) => {
                    if payload
                        .pointer("/_kin/semantic_surface")
                        .and_then(Value::as_u64)
                        != Some(VERSION)
                    {
                        *result = serde_json::to_value(crate::ToolCallResult::error(
                            "This continuation predates the semantic surface contract. Use matching current Kin binaries and restart the query without a cursor."
                        )).expect("tool refusal serializes");
                        return;
                    }
                }
                Ok(mut payload) => {
                    project(tool, &mut payload);
                    // The account of the answer rides its verdict, before
                    // the public cost of the reply is settled.
                    if answer {
                        crate::answer_account::attach_answer(tool, &mut payload);
                    }
                    if let Err(message) = settle_public_cost(&mut payload, &text, tool) {
                        *result = serde_json::to_value(crate::ToolCallResult::error(message))
                            .expect("tool refusal serializes");
                        return;
                    }
                    block["text"] = json!(crate::budget::render_in_format_of(&payload, &text)
                        .unwrap_or_else(|_| payload.to_string()));
                }
                Err(_) => {
                    block["text"] = json!(diagnostic(&text));
                }
            }
        }
    }
    // Structured content is attached only after this boundary.
    if let Some(object) = result.as_object_mut() {
        object.remove("structuredContent");
    }
}

// Private qualification has already finished. Recount only presentation, never
// recompute an absence verdict from the reduced public evidence.
fn settle_public_cost(payload: &mut Value, previous: &str, tool: &str) -> Result<(), &'static str> {
    if !previous.contains('\n') && payload.is_object() {
        payload[crate::budget::JSON_FORMAT_KEY] = json!("compact");
    }
    if let Some(max_chars) = payload
        .pointer("/_kin/response/max_chars")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
    {
        crate::budget::reconcile_residual(
            payload,
            &crate::budget::ResponseBudget {
                max_chars,
                compact: !previous.contains('\n'),
                ..Default::default()
            },
        );
    }
    let context = matches!(tool, "get_context_pack" | "trace_computation");
    if context && payload.get("answer_tokens").is_some() {
        payload["answer_tokens"] = json!(crate::budget::context_answer_tokens(payload));
    }
    for _ in 0..16 {
        let text = crate::budget::render_in_format_of(payload, previous)
            .map_err(|_| "Kin could not encode the semantic response.")?;
        let bytes = text.len();
        let tokens = kin_context::estimate_tokens(&text);
        let mut changed = false;
        if let Some(value) = payload.pointer_mut("/_kin/response/chars_after_budget") {
            if value.as_u64() != Some(bytes as u64) {
                *value = json!(bytes);
                changed = true;
            }
        }
        if context || tool == "explore_codebase" {
            if let Some(value) = payload.get_mut("tokens_used") {
                if value.as_u64() != Some(tokens as u64) {
                    *value = json!(tokens);
                    changed = true;
                }
            }
        }
        if tool == "explore_codebase" {
            if let Some(budget) = payload.get("token_budget").and_then(Value::as_u64) {
                let exceeds = tokens as u64 > budget;
                if payload.get("over_token_budget").and_then(Value::as_bool) != Some(exceeds) {
                    payload["over_token_budget"] = json!(exceeds);
                    changed = true;
                }
            }
        }
        if !changed {
            return Ok(());
        }
    }
    Err("Kin could not settle the semantic response accounting.")
}

/// Unknown backend diagnostics are not a public coordinate surface. Preserve
/// semantic diagnostics verbatim; an operator-only diagnostic stays internal.
pub fn diagnostic(text: &str) -> String {
    if physical_text(text) {
        // Private recovery details may include operator coordinates. Preserve
        // the actual stable refusal classification, including nested recovery
        // failures, without publishing those details or inventing success.
        let readings: Vec<_> = [
            ("request_recovery_required", "Kin cannot recover this request's durable state. Preserve its request_id and retry only after operator recovery."),
            ("transaction_recovery_required", "Kin cannot establish the transaction's durable state. Preserve its transaction_id and retry only after operator recovery."),
            ("transaction_persistence_failed", "Kin could not acknowledge the transaction update. Check its status before retrying."),
            ("semantic_operation_required", "Use a guarded entity patch, an entity source update, or an explicit Entity or Relation payload. No new operation was admitted."),
        ].into_iter().filter_map(|(code, meaning)| {
            text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == ':'))
                .any(|word| word == format!("{code}:"))
                .then(|| format!("{code}: {meaning}"))
        }).collect();
        if !readings.is_empty() {
            return readings.join(" ");
        }
        // A verdict's reasons are separate clauses, each under its own code.
        // One clause written in physical terms withholds that clause, never
        // the others: replacing the whole text erased every reason an answer
        // was qualified by, the spine and truncation clauses included.
        if let Some(projected) = project_clause_text(text) {
            return projected;
        }
        // A write the commit path discarded before it published anything
        // says so on a line of its own, which is the proof that nothing
        // moved. The reason is withheld like any other physical detail; the
        // line is kept, so the reply can still say that nothing was written
        // and what to do about it.
        if text.contains(crate::write_reply::PLAN_DISCARDED) {
            return format!("{WITHHELD_PLAN}{}", crate::write_reply::PLAN_DISCARDED);
        }
        "Kin could not provide this semantic result. The internal diagnostic requires operator inspection; no additional semantic conclusion is established.".into()
    } else {
        text.into()
    }
}

/// What a write refused before publication says in place of a reason that
/// names storage: that the reason exists and where it is kept, and that the
/// same change will be refused the same way.
const WITHHELD_PLAN: &str = "Kin refused this change while planning it, for a reason that names \
where code is stored and is kept in the daemon's log; sending it again unchanged will be refused \
again, so report this refusal";

/// What a withheld verdict clause says in place of its physical detail.
const WITHHELD_CLAUSE: &str =
    "detail withheld at the semantic boundary, and the code still limits this answer";

/// The text that introduces a verdict's clauses inside an advice sentence.
const LIMITING_FACTOR_LEAD: &str = "Limiting factor: ";

/// Project a verdict clause list, alone or after an advice sentence's
/// `Limiting factor: ` lead, one clause at a time. `None` when the text is not
/// such a list (every clause under a listed verdict or residue code) or its
/// lead itself is physical, so the caller withholds it whole.
fn project_clause_text(text: &str) -> Option<String> {
    let (lead, clauses) = match text.find(LIMITING_FACTOR_LEAD) {
        Some(at) => text.split_at(at + LIMITING_FACTOR_LEAD.len()),
        None => ("", text),
    };
    if physical_text(lead) {
        return None;
    }
    let mut projected = Vec::new();
    for clause in clauses.split(crate::verdict::CLAUSE_SEPARATOR) {
        let label = clause_label(clause)?;
        projected.push(if physical_text(clause) {
            let meaning = crate::verdict::CLAUSE_CODES
                .iter()
                .find(|entry| entry.code == label)
                .map(|entry| entry.meaning)
                .filter(|meaning| {
                    !physical_text(meaning) && !meaning.contains(crate::verdict::CLAUSE_SEPARATOR)
                })
                .unwrap_or(WITHHELD_CLAUSE);
            format!("{label}: {meaning}")
        } else {
            clause.to_string()
        });
    }
    Some(format!(
        "{lead}{}",
        projected.join(crate::verdict::CLAUSE_SEPARATOR)
    ))
}

/// A verdict clause's code: the label before its first colon, or the whole
/// clause when it is a bare code, and only when the verdict's closed code list
/// or a listed residue carries it.
fn clause_label(clause: &str) -> Option<&str> {
    let label = clause.split_once(':').map_or(clause, |(label, _)| label);
    (crate::verdict::CLAUSE_CODES
        .iter()
        .any(|entry| entry.code == label)
        || kin_model::call_site_reading::RESIDUE_CODES.contains(&label)
        || crate::declaration_coverage::RESIDUE_CODES.contains(&label))
    .then_some(label)
}

/// A scoped npm package with its version, such as `@kinlab/kin@0.8.3` in a
/// pinned `npx` command. Its slash separates a scope from a package name, and
/// the dot in its version is not a file extension, so it is no coordinate.
fn npm_package_spec(token: &str) -> bool {
    let name = |part: &str| {
        !part.is_empty()
            && part.chars().all(|c| {
                c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '.' | '_')
            })
    };
    token
        .strip_prefix('@')
        .and_then(|rest| rest.split_once('/'))
        .and_then(|(scope, rest)| Some((scope, rest.split_once('@')?)))
        .is_some_and(|(scope, (package, version))| {
            name(scope)
                && name(package)
                && version.starts_with(|c: char| c.is_ascii_alphanumeric())
                && version
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+'))
        })
}

/// Whether `text` reaches an agent as written. False when this surface would
/// withhold it as physical: a storage word or a coordinate in its prose.
pub fn is_semantic(text: &str) -> bool {
    !physical_text(text)
}

fn physical_text(text: &str) -> bool {
    // A word inside a backtick code span is code: a function named `file` or
    // a signature such as `fn file()` names an entity, not storage. A path is
    // a coordinate wherever it is written, so code spans are still read for
    // those.
    let mut in_code = false;
    text.split_whitespace().any(|word| {
        let code = in_code
            || word
                .trim_start_matches(['(', '[', '{', '"', '\''])
                .starts_with('`');
        if word.matches('`').count() % 2 == 1 {
            in_code = !in_code;
        }
        let token = word.trim_matches(|c: char| {
            matches!(
                c,
                '`' | '\'' | '"' | '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';'
            )
        });
        (!code
            && matches!(
                token.to_ascii_lowercase().trim_end_matches([':', '.']),
                "file"
                    | "files"
                    | "filesystem"
                    | "directory"
                    | "directories"
                    | "folder"
                    | "folders"
                    | "cross-file"
                    | "same-file"
            ))
            || token.starts_with("file://")
            || (crate::file_coordinate_query::looks_like_coordinate(token)
                && !npm_package_spec(token))
    })
}

/// What a redacted coordinate reads as inside text a person wrote.
const REDACTED_COORDINATE: &str = "[path redacted]";

/// Extensions that make a bare word a file name, such as `package.json` or
/// `ReactFiber.js`, in text a person wrote. A version such as `0.8.3`, a
/// member access such as `React.createElement` and an abbreviation such as
/// `e.g.` end in none of these.
const FILE_NAME_EXTENSIONS: &[&str] = &[
    "bash",
    "bazel",
    "bzl",
    "c",
    "cc",
    "cfg",
    "cjs",
    "clj",
    "cmake",
    "conf",
    "cpp",
    "cs",
    "css",
    "csv",
    "cts",
    "cxx",
    "dart",
    "dockerfile",
    "el",
    "env",
    "erl",
    "ex",
    "exs",
    "flow",
    "go",
    "gradle",
    "graphql",
    "h",
    "hh",
    "hpp",
    "hs",
    "htm",
    "html",
    "ini",
    "ipynb",
    "java",
    "jl",
    "js",
    "json",
    "jsonc",
    "jsx",
    "kt",
    "kts",
    "less",
    "lock",
    "log",
    "lua",
    "m",
    "md",
    "mdx",
    "mjs",
    "ml",
    "mm",
    "mts",
    "nix",
    "php",
    "pl",
    "plist",
    "proto",
    "ps1",
    "py",
    "pyi",
    "r",
    "rb",
    "rs",
    "sass",
    "scala",
    "scss",
    "sh",
    "snap",
    "sql",
    "svelte",
    "swift",
    "tf",
    "toml",
    "ts",
    "tsx",
    "txt",
    "vue",
    "wasm",
    "xml",
    "yaml",
    "yml",
    "zig",
    "zsh",
];

/// Whether a bare word names a file: a dotfile such as `.gitignore`, or a stem
/// and one of [`FILE_NAME_EXTENSIONS`].
fn bare_file_name(token: &str) -> bool {
    if let Some(dotfile) = token.strip_prefix('.') {
        return dotfile.starts_with(|c: char| c.is_ascii_alphabetic())
            && dotfile
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    }
    token.rsplit_once('.').is_some_and(|(stem, extension)| {
        !stem.is_empty()
            && stem
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '@' | '+'))
            && FILE_NAME_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str())
    })
}

/// A web address a person wrote, such as a pull request link. It names a page,
/// not a place on this host, so only a loopback address is a coordinate.
fn web_address(token: &str) -> bool {
    let Some(rest) = token
        .strip_prefix("https://")
        .or_else(|| token.strip_prefix("http://"))
    else {
        return false;
    };
    let host = rest.split(['/', ':', '?', '#']).next().unwrap_or_default();
    !host.is_empty()
        && !matches!(host, "localhost" | "127.0.0.1" | "0.0.0.0" | "[::1]")
        && !host.ends_with(".localhost")
}

/// Text a person wrote, such as a commit message, with only its physical parts
/// redacted in place: an absolute or relative path, a file name, a `file://`
/// address and a loopback endpoint. Every other word is the author's and
/// stays, the word "file" included, because withholding the whole message
/// lost what it said and put Kin's own internal-error sentence in its place,
/// which reads as a Kin failure. Idempotent.
pub(crate) fn redact_authored_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while !rest.is_empty() {
        let word_start = rest
            .find(|c: char| !c.is_whitespace())
            .unwrap_or(rest.len());
        out.push_str(&rest[..word_start]);
        rest = &rest[word_start..];
        let word_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let word = &rest[..word_end];
        rest = &rest[word_end..];
        let edge = |c: char| {
            matches!(
                c,
                '`' | '\''
                    | '"'
                    | '('
                    | ')'
                    | '['
                    | ']'
                    | '{'
                    | '}'
                    | '<'
                    | '>'
                    | ','
                    | ';'
                    | ':'
                    | '!'
                    | '?'
            )
        };
        let core = word.trim_start_matches(edge);
        let lead = &word[..word.len() - core.len()];
        let mut core = core.trim_end_matches(edge);
        // A sentence's own full stop is not part of the path it ends with.
        while core.ends_with('.') && core.len() > 1 && !core.ends_with("..") {
            core = &core[..core.len() - 1];
        }
        let trail = &word[lead.len() + core.len()..];
        let physical = !core.is_empty()
            && !web_address(core)
            && (core.starts_with("file://")
                || (crate::file_coordinate_query::looks_like_coordinate(core)
                    && !npm_package_spec(core))
                || bare_file_name(core));
        if physical {
            out.push_str(lead);
            out.push_str(REDACTED_COORDINATE);
            out.push_str(trail);
        } else {
            out.push_str(word);
        }
    }
    out
}

/// Whether a string field holds text a person wrote rather than Kin's own
/// prose: a change's commit message, under the name the plain reply gives it
/// or as the `message` of a change record. A change record names its parents
/// or its author and origin, which none of Kin's own diagnostics carries.
fn authored_text(key: &str, record: &Map<String, Value>) -> bool {
    key == "commit_message"
        || (key == "message"
            && (record.get("parents").is_some_and(Value::is_array)
                || (record.contains_key("author")
                    && record.get("origin").is_some_and(Value::is_object))))
}

/// Exact program text is data, including strings and imports that happen to
/// contain coordinates. These fields are never interpreted as diagnostics.
fn program_text(key: &str) -> bool {
    matches!(
        key,
        "body"
            | "original_body"
            | "code"
            | "source"
            | "source_text"
            | "expr"
            | "expression"
            | "snippet"
            | "excerpt"
            | "preview"
            | "body_preview"
            | "signature"
            | "header"
            | "documentation"
            | "doc"
            | "docstring"
            | "literal"
            | "utf8"
            | "receiver"
    )
}

// These are names supplied by semantic producers, not backend diagnostics.
// A function may be named `file` and an API entity may be named `/api/session`.
fn semantic_name(key: &str) -> bool {
    matches!(
        key,
        "name"
            | "entity_name"
            | "qualified_name"
            | "subject_name"
            | "owner_qualified_name"
            | "caller_name"
            | "callee"
            | "target_name"
            | "to_name"
            | "in_name"
            | "focal_name"
            // An external entity's symbol, such as `lib.es5.d.ts`/Array#map().
            // in a dependency package: its identity, not a repository path.
            | "symbol"
    )
}

// Only the closed identity beside its matching source ticket is semantic data.
// A field named repository_id elsewhere is not exempt from diagnostic rewriting.
fn typed_source_identity(record: &Map<String, Value>, key: &str) -> bool {
    let base_key = match key {
        "source_identity" => "source_base",
        "original_source_identity" => "original_source_base",
        _ => return false,
    };
    let Some(identity) = record
        .get(key)
        .and_then(Value::as_object)
        .filter(|identity| identity.len() == 3)
    else {
        return false;
    };
    let Some(ticket) = record
        .get(base_key)
        .filter(|base| crate::source_ticket::is_ticket(base))
        .and_then(|base| base[crate::source_ticket::TICKET_KEY].as_str())
        .and_then(|ticket| crate::source_ticket::parse(ticket).ok())
    else {
        return false;
    };
    identity
        .get("repository_id")
        .and_then(Value::as_str)
        .is_some_and(|id| kin_model::RepositoryId::new(id).is_ok())
        && identity
            .get("workspace_id")
            .and_then(Value::as_str)
            .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok())
        && identity
            .get("entity_id")
            .and_then(Value::as_str)
            .and_then(|id| uuid::Uuid::parse_str(id).ok())
            .is_some_and(|id| id.to_string() == ticket.entity_id)
}

fn artifact_scope(value: &Value) -> bool {
    value
        .as_str()
        .is_some_and(|scope| scope.starts_with("artifact:"))
        || value
            .as_object()
            .is_some_and(|scope| scope.contains_key("Artifact"))
}

fn freeze_binding_reading(value: &mut Value) {
    if let Some(envelope) = value.get_mut("_kin") {
        if envelope.get("reference_gap").is_none() {
            if let Some(gap) = crate::plain_reply::reference_binding_gap(envelope) {
                envelope["reference_gap"] = gap;
            }
        }
    }
    match value {
        Value::Object(record) => {
            for child in record.values_mut() {
                freeze_binding_reading(child);
            }
        }
        Value::Array(values) => {
            for child in values {
                freeze_binding_reading(child);
            }
        }
        _ => {}
    }
}

fn physical_field(key: &str, value: &Value, parent: &str) -> bool {
    // The graph-behind reading's path samples and its changed-path count are
    // the same host inventory as its unadmitted and underived counts.
    if parent == "behind" && matches!(key, "sample" | "changed_sample" | "changed_paths") {
        return true;
    }
    // Typed verdict inputs and completeness classes are predicates, not
    // coordinate records or inventory counts. Their states remain auditable.
    if matches!(parent, "inputs" | "classes") && value.is_string() {
        return false;
    }
    // A record fragment's path addresses a JSON field. A trace path is a
    // collection of semantic steps. Neither is a storage location.
    if key == "path" {
        return !matches!(parent, "record_fragment" | "compacted") && !value.is_array();
    }
    if key == "projection" {
        return value.is_object()
            || value
                .as_str()
                .is_some_and(crate::file_coordinate_query::looks_like_coordinate);
    }
    if key == "root" {
        return value
            .as_str()
            .is_some_and(crate::file_coordinate_query::looks_like_coordinate);
    }
    crate::file_coordinate_query::looks_like_coordinate(key)
        || key.split('_').any(|part| matches!(part, "file" | "files"))
        || matches!(
            key,
            "read_path"
                | "source_path"
                | "source_paths"
                | "paths"
                | "by_path"
                | "repo_root"
                | "repository_root"
                | "workspace_root"
                | "client_root"
                | "cwd"
                | "location"
                | "locations"
                | "source_span"
                | "span"
                | "start_line"
                | "end_line"
                | "start_col"
                | "end_col"
                | "line"
                | "lines"
                | "start_byte"
                | "end_byte"
                | "byte_span"
                | "artifact_id"
                | "quote_lines"
                | "full_adapter_sources"
                | "excluded_artifacts"
                | "source_inventory"
                | "unadmitted_paths"
                | "underived_paths"
                | "underived_sample"
                | "gap_paths"
                | "withheld_test_paths"
                | "graph_bodies"
                | "graph_body_gap_paths"
        )
}

fn project_identity(record: &mut Map<String, Value>) {
    let compatibility_container = ["kind", "entity_kind"].iter().any(|key| {
        record
            .get(*key)
            .and_then(Value::as_str)
            .is_some_and(|kind| kind.eq_ignore_ascii_case("file"))
    });
    if compatibility_container {
        // A retained import container has identity and relationships, but no
        // independently readable semantic declaration. Never reinterpret it as
        // a module or expose an aggregate source body under an entity label.
        for key in [
            "kind",
            "entity_kind",
            "name",
            "member_name",
            "qualified_name",
            "entity_name",
            "subject_name",
            "signature",
            "header",
            "body",
            "original_body",
            "code",
            "source",
            "source_text",
            "source_base",
            "original_source_base",
            "snippet",
            "excerpt",
            "preview",
            "body_preview",
        ] {
            record.remove(key);
        }
        record.insert("semantic_declaration_gap".into(), json!(
            "This recorded container has no independent semantic declaration; its recorded relationships remain available."
        ));
        record.insert("body_complete".into(), json!(false));
        record.insert("source_base_unavailable".into(), json!(true));
        return;
    }
    let origin = ["file_path", "file", "read_path", "entity_file"]
        .iter()
        .find_map(|key| record.get(*key).and_then(Value::as_str))
        .or_else(|| record.get("projection")?.get("path")?.as_str())
        .map(str::to_owned);
    let container = record
        .get("kind")
        .or_else(|| record.get("entity_kind"))
        .and_then(Value::as_str)
        .is_some_and(|kind| {
            matches!(
                kind.to_ascii_lowercase().as_str(),
                "module" | "package" | "file"
            )
        });
    if !container {
        return;
    }
    // Ranked full rows keep their recorded origin under provenance. Consult it
    // only for the exact generated container header, never to rewrite names or
    // strings in literal program text.
    let header_origin = origin
        .as_deref()
        .or_else(|| record.get("provenance")?.get("file")?.as_str())
        .map(str::to_owned);
    if let Some(origin) = header_origin {
        for key in ["signature", "header"] {
            let synthetic = record.get(key).and_then(Value::as_str).is_some_and(|text| {
                text == format!("module {origin}") || text == format!("package {origin}")
            });
            if synthetic {
                record.remove(key);
            }
        }
    }
    if let Some(origin) = origin {
        for key in ["name", "qualified_name"] {
            if record.get(key).and_then(Value::as_str) == Some(origin.as_str()) {
                record.remove(key);
            }
        }
        if !record.contains_key("name") && !record.contains_key("qualified_name") {
            record.insert(
                "semantic_identity_gap".into(),
                json!("A semantic name is not recorded for this entity; use its entity_id."),
            );
        }
    }
}

/// The public history qualifier describes the persisted admission evidence.
/// Recovery commands belong to the operator boundary, not an agent query.
fn project_partial_history(record: &mut Map<String, Value>) {
    let Ok(notice) = serde_json::from_value::<kin_core::history_state::PartialHistoryNotice>(
        Value::Object(record.clone()),
    ) else {
        return;
    };
    let mut sentence = match notice.state.as_str() {
        "partial" => "History is partial.".to_string(),
        "unreadable" => {
            "The history admission record is unreadable; completeness is unproven.".to_string()
        }
        _ => "History completeness is not established by this notice.".to_string(),
    };
    if let Some(commit) = notice.source_commit {
        sentence.push_str(&format!(" Source commit: {commit}."));
    }
    if let Some(admitted) = notice.admitted_commits {
        match notice.reachable_commits {
            Some(reachable) => {
                sentence.push_str(&format!(" Admitted commits: {admitted} of {reachable}."))
            }
            None => sentence.push_str(&format!(" Admitted commits: {admitted}.")),
        }
    }
    if let Some(derivation) = notice.derivation {
        sentence.push_str(&format!(" Derivation state: {derivation}."));
    }
    sentence.push_str(
        " Conclusions are limited to admitted history. An operator must inspect and complete \
         history admission before completeness can be established.",
    );
    record.insert("sentence".into(), json!(sentence));
}

fn project_record(value: &mut Value, parent: &str) {
    match value {
        Value::Array(values) => {
            for value in values {
                project_record(value, parent);
            }
        }
        Value::Object(record) => {
            if parent == "binding_coverage"
                && record.get("checked_scope").and_then(Value::as_str) == Some("selected_paths")
            {
                record.insert("checked_scope".into(), json!("selected_source_bindings"));
            }
            if parent == "partial_history" {
                project_partial_history(record);
            }
            project_identity(record);
            // Proximity candidates are not certified graph relationships.
            // Their public reading must disclose that gap without presenting
            // physical co-location as a semantic relation.
            if record.get("relation").and_then(Value::as_str) == Some("same_file_neighbor") {
                record.insert("relation".into(), json!("unproven_neighbor"));
            }
            if parent == "dependency_selection"
                && record.get("source").and_then(Value::as_str) == Some("same_file_fallback")
            {
                record.insert("source".into(), json!("uncertified_neighbor_selection"));
            }
            // Persisted operator scopes can predate the semantic input contract.
            // Exclude their typed physical variant, including extensionless names,
            // and disclose incomplete scope presentation without fabricating one.
            let mut omitted_scope = false;
            for key in ["scopes", "touched_scopes"] {
                if let Some(scopes) = record.get_mut(key).and_then(Value::as_array_mut) {
                    let before = scopes.len();
                    scopes.retain(|scope| !artifact_scope(scope));
                    omitted_scope |= scopes.len() != before;
                }
            }
            if record.get("scope").is_some_and(artifact_scope) {
                record.remove("scope");
                omitted_scope = true;
            }
            if omitted_scope {
                record.insert("semantic_scope_incomplete".into(), json!(true));
            }
            if record.get("matched_field").and_then(Value::as_str) == Some("file_surface_context") {
                for key in [
                    "matched_field",
                    "preview",
                    "excerpt",
                    "snippet",
                    "context",
                    "text",
                ] {
                    record.remove(key);
                }
                record.insert(
                    "semantic_gap".into(),
                    json!("entity_local_match_unavailable"),
                );
            }
            if let Some(repository) = record.get("repository").cloned() {
                if repository.get("root").is_some() {
                    let prior_warning = repository
                        .get("warning")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    let semantic =
                        serde_json::from_value::<crate::first_contact::RepositoryIdentity>(
                            repository,
                        )
                        .ok()
                        .and_then(|identity| {
                            crate::first_contact::semantic_repository_identity(&identity)
                        })
                        .and_then(|identity| serde_json::to_value(identity).ok());
                    match semantic {
                        Some(identity) => {
                            if let (Some(prior), Some(current), Some(advice)) = (
                                prior_warning.as_deref(),
                                identity.get("warning").and_then(Value::as_str),
                                record.get("advice").and_then(Value::as_str),
                            ) {
                                let rest = advice.replace(prior, "").trim().to_owned();
                                let advice = if rest.is_empty() {
                                    current.to_owned()
                                } else {
                                    format!("{current} {}", diagnostic(&rest))
                                };
                                record.insert("advice".into(), json!(advice));
                            }
                            record.insert("repository".into(), identity);
                        }
                        None => {
                            record.remove("repository");
                        }
                    }
                }
            }
            for base_key in ["source_base", "original_source_base"] {
                if let Some(base) = record
                    .get(base_key)
                    .filter(|base| base.is_object())
                    .cloned()
                {
                    if base.get(crate::source_ticket::TICKET_KEY).is_none() {
                        match crate::source_ticket::compact(&base) {
                            Some(ticket) => {
                                // Keep the source read's semantic identity beside its
                                // opaque ticket. Consumers must not borrow identity
                                // from a later status read to authorize this body.
                                let identity_key = if base_key == "source_base" {
                                    "source_identity"
                                } else {
                                    "original_source_identity"
                                };
                                record.insert(
                                    identity_key.into(),
                                    json!({
                                        "repository_id": base["context"]["repository_id"],
                                        "workspace_id": base["context"]["workspace_id"],
                                        "entity_id": base["entity_id"],
                                    }),
                                );
                                record.insert(base_key.into(), ticket);
                            }
                            None => {
                                record.remove(base_key);
                                record.insert("source_base_unavailable".into(), json!(true));
                            }
                        }
                    }
                }
            }
            if let Some(derivation) = record
                .get("source_derivation")
                .filter(|value| value.is_object())
                .cloned()
            {
                record.remove("source_derivation");
                let mut semantic = Map::new();
                for key in [
                    "scope",
                    "checked_scope",
                    "binding_scope",
                    "local_binding_requirement",
                ] {
                    if let Some(value) = derivation.get(key) {
                        semantic.insert(key.into(), value.clone());
                    }
                }
                if !semantic.is_empty() {
                    record.insert("binding_coverage".into(), Value::Object(semantic));
                }
            }
            record.retain(|key, value| !physical_field(key, value, parent));
            // Do not turn generated coordinate labels into semantic names.
            let container = record
                .get("kind")
                .or_else(|| record.get("entity_kind"))
                .and_then(Value::as_str)
                .is_some_and(|kind| {
                    matches!(
                        kind.to_ascii_lowercase().as_str(),
                        "module" | "package" | "file"
                    )
                });
            for key in ["name", "entity_name", "qualified_name", "subject_name"] {
                if container
                    && record
                        .get(key)
                        .and_then(Value::as_str)
                        .is_some_and(crate::file_coordinate_query::looks_like_coordinate)
                {
                    record.remove(key);
                    record.insert(
                        "semantic_identity_gap".into(),
                        json!("Use the recorded entity_id; a semantic name is unavailable."),
                    );
                }
            }
            let authored: Vec<String> = record
                .iter()
                .filter(|(key, value)| value.is_string() && authored_text(key, record))
                .map(|(key, _)| key.clone())
                .collect();
            let source_identities: Vec<_> = ["source_identity", "original_source_identity"]
                .into_iter()
                .filter(|key| typed_source_identity(record, key))
                .collect();
            for (key, value) in record {
                // Keep opaque repository IDs exact, including path-shaped IDs,
                // without exempting arbitrary records or diagnostic fields.
                if source_identities.contains(&key.as_str()) {
                    continue;
                }
                // Entity source and quoted expressions are scalar strings;
                // the walker never interprets or rewrites their contents.
                if program_text(key) && value.is_string() {
                    continue;
                }
                // A person's words are not Kin's diagnostic: their physical
                // parts are redacted in place and the rest is kept.
                if authored.contains(key) {
                    if let Value::String(text) = value {
                        *text = redact_authored_text(text);
                    }
                    continue;
                }
                if let Value::String(text) = value {
                    // Where an artifact is projected is the one coordinate
                    // a reply states, as a fact about the artifact and never
                    // as an address ([`crate::artifact_entity`]).
                    if !semantic_name(key)
                        && !matches!(key.as_str(), "ticket" | "next_cursor" | "cursor" | "path")
                        && key != crate::artifact_entity::PROJECTED_AT_KEY
                        && physical_text(text)
                    {
                        *text = diagnostic(text);
                    }
                } else {
                    project_record(value, key);
                }
            }
        }
        Value::String(text) if !program_text(parent) && parent != "path" => {
            *text = diagnostic(text);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_source_tickets_retain_the_identity_of_the_original_read() {
        let entity = "11111111-1111-4111-8111-111111111111";
        let workspace = "22222222-2222-4222-8222-222222222222";
        let base = json!({
            "schema": "kin.entity.source_base.v1",
            "context": {"repository_id": "org/repo.git", "workspace_id": workspace,
                "workspace_generation": 1, "workspace_head_hash": "a".repeat(64),
                "workspace_tree_hash": "b".repeat(64)},
            "entity_id": entity, "artifact_id": workspace, "source_blob_hash": "c".repeat(64),
            "start_byte": 0, "end_byte": 4, "body_hash": "d".repeat(64),
        });
        let ticket = crate::source_ticket::compact(&base).unwrap();
        let identity = json!({"repository_id": "org/repo.git", "workspace_id": workspace, "entity_id": entity});
        let mut value = json!({"source_base": base, "original_source_base": base});
        project("get_entity_source", &mut value);
        assert_eq!(value["source_base"], ticket);
        assert_eq!(value["original_source_base"], ticket);
        assert_eq!(value["source_identity"], identity);
        assert_eq!(value["original_source_identity"], identity);
        let first = value.clone();
        project("get_entity_source", &mut value);
        assert_eq!(
            value, first,
            "egress projection must preserve the captured identity"
        );
        let mut invalid = json!({"source_base": {"entity_id": entity}});
        project("get_entity_source", &mut invalid);
        assert!(invalid.get("source_base").is_none());
        assert!(invalid.get("source_identity").is_none());
    }

    #[test]
    fn source_identity_preservation_requires_a_closed_record_and_matching_ticket() {
        let entity = "11111111-1111-4111-8111-111111111111";
        let workspace = "22222222-2222-4222-8222-222222222222";
        let identity = json!({"repository_id": "org/repo.git",
            "workspace_id": workspace, "entity_id": entity});
        for version in ["sb1", "sb2"] {
            for (identity_key, base_key) in [
                ("source_identity", "source_base"),
                ("original_source_identity", "original_source_base"),
            ] {
                let ticket = json!({"ticket": format!("{version}.{entity}.1.{}", "a".repeat(32))});
                let mut value = json!({identity_key: identity, base_key: ticket,
                    "repository_id": "org/repo.git", "message": "cannot read org/repo.git",
                    "file_path": "org/repo.git"});
                project("get_entity_source", &mut value);
                assert_eq!(value[identity_key], identity);
                assert_eq!(value[base_key], ticket);
                assert_eq!(value["repository_id"], diagnostic("org/repo.git"));
                assert_eq!(value["message"], diagnostic("cannot read org/repo.git"));
                assert!(value.get("file_path").is_none());
                let once = value.clone();
                project("get_entity_source", &mut value);
                assert_eq!(value, once);

                for malformed in [
                    "missing_ticket",
                    "invalid_ticket",
                    "ticket_extra_field",
                    "wrong_entity",
                    "invalid_workspace",
                    "extra_field",
                    "array",
                ] {
                    let mut value = json!({identity_key: identity, base_key: ticket});
                    match malformed {
                        "missing_ticket" => {
                            value.as_object_mut().unwrap().remove(base_key);
                        }
                        "invalid_ticket" => value[base_key]["ticket"] = json!("sb2.invalid"),
                        "ticket_extra_field" => value[base_key]["note"] = json!("extra"),
                        "wrong_entity" => value[identity_key]["entity_id"] = json!(workspace),
                        "invalid_workspace" => {
                            value[identity_key]["workspace_id"] = json!("not-a-uuid")
                        }
                        "extra_field" => value[identity_key]["note"] = json!("org/repo.git"),
                        "array" => value[identity_key] = json!([identity]),
                        _ => unreachable!(),
                    }
                    project("get_entity_source", &mut value);
                    let projected = if malformed == "array" {
                        &value[identity_key][0]
                    } else {
                        &value[identity_key]
                    };
                    assert_eq!(
                        projected["repository_id"],
                        diagnostic("org/repo.git"),
                        "{version} {identity_key} {malformed}"
                    );
                    if malformed == "extra_field" {
                        assert_eq!(projected["note"], diagnostic("org/repo.git"));
                    }
                }
            }
        }
    }

    /// Where an artifact is projected is stated once, under its own key, as
    /// a fact about the artifact. The same place under any other key is a
    /// coordinate and is withheld as before.
    #[test]
    fn where_an_artifact_is_projected_is_the_one_place_a_reply_states() {
        let mut value = json!({
            "entity_id": "9c1e77a0-0000-4000-8000-000000000001",
            "name": "flask-logo.png", "module": "docs.static", "kind": "PNG image",
            "held_as": "bytes",
            "projected_at": "docs/_static/flask-logo.png",
            "note": "read it at docs/_static/flask-logo.png",
            "file_path": "docs/_static/flask-logo.png",
            "path": "docs/_static/flask-logo.png",
        });
        project("get_entity_source", &mut value);
        assert_eq!(value["projected_at"], "docs/_static/flask-logo.png");
        assert_eq!(value["name"], "flask-logo.png");
        assert_eq!(value["kind"], "PNG image");
        assert_eq!(value["held_as"], "bytes");
        assert!(value.get("file_path").is_none(), "{value}");
        assert!(value.get("path").is_none(), "{value}");
        assert!(
            !value["note"].as_str().unwrap().contains("docs/"),
            "{value}"
        );
    }

    #[test]
    fn coordinates_are_not_entity_identity_and_program_text_stays_exact() {
        let body = "def open_config():\n    return open('config/settings.json')\n";
        let mut value = json!({"_kin":{"verdict":{"state":"inconclusive"}},
            "focal_entity":{"entity_id":"target","name":"open_config","file_path":"src/config.py"},
            "body":body,"signature":"def open_config()", "line_in_entity":2,
            "source_derivation":{"checked_scope":"admitted_inventory","report":{"full_adapter_sources":23,"issues":[{"path":"tests/a.py"}]}},
            "references":[{"entity_id":"caller","name":"run","file":"src/main.py","line":90}],
            "path":[{"entity_id":"caller"},{"entity_id":"target"}],
            "record_fragment":{"path":"references[0].body","utf8":body,"byte_offset":0}});
        project("find_references", &mut value);
        assert_eq!(value["body"], body);
        assert_eq!(value["record_fragment"]["utf8"], body);
        assert_eq!(value["record_fragment"]["path"], "references[0].body");
        assert_eq!(value["path"].as_array().unwrap().len(), 2);
        assert_eq!(value["line_in_entity"], 2);
        assert_eq!(value["_kin"]["verdict"]["state"], "inconclusive");
        assert_eq!(value["references"][0]["entity_id"], "caller");
        assert!(!value.to_string().contains("src/config.py"));
        assert!(!value.to_string().contains("src/main.py"));
        assert!(!value.to_string().contains("tests/a.py"));
        assert!(value.get("source_derivation").is_none());
        assert_eq!(
            value["binding_coverage"]["checked_scope"],
            "admitted_inventory"
        );
    }

    #[test]
    fn selected_source_binding_scope_is_semantic_without_changing_qualification() {
        let qualification = json!({"version":1,"tool":"get_entity_source","scope":"live_head",
            "source":"no_limitation","relations":"no_limitation","history":"no_limitation"});
        let derivation = json!({"scope":"live_head","checked_scope":"selected_paths",
            "local_binding_requirement":"not_required"});
        let body = "return 'selected_paths; git fetch --unshallow'";
        let mut value = json!({"entity_id":"declared","name":"selected_paths","body":body,
            "source_derivation":derivation,"source_qualification":qualification,
            "_kin":{"source_derivation":derivation,"verdict":{"state":"certified"}},
            "prior":{"binding_coverage":{"checked_scope":"selected_paths","scope":"live_head"}}});
        project("get_entity_source", &mut value);
        for pointer in [
            "/binding_coverage/checked_scope",
            "/_kin/binding_coverage/checked_scope",
            "/prior/binding_coverage/checked_scope",
        ] {
            assert_eq!(
                value.pointer(pointer),
                Some(&json!("selected_source_bindings"))
            );
        }
        assert_eq!(value["binding_coverage"]["scope"], "live_head");
        assert_eq!(
            value["binding_coverage"]["local_binding_requirement"],
            "not_required"
        );
        assert_eq!(value["source_qualification"], qualification);
        assert_eq!(value["_kin"]["verdict"]["state"], "certified");
        assert_eq!(value["entity_id"], "declared");
        assert_eq!(value["name"], "selected_paths");
        assert_eq!(value["body"], body);
        let once = value.clone();
        project("get_entity_source", &mut value);
        assert_eq!(value, once);
    }

    #[test]
    fn partial_history_retains_evidence_and_uses_operator_admission_guidance() {
        use kin_core::history_state::{
            DerivationState, HistoryDerivationProgress, HistoryMode, HistoryStanding,
            HistoryStateRecord,
        };

        let partial = |underivable| HistoryStanding::Partial {
            record: HistoryStateRecord::new(
                HistoryMode::None,
                "0123456789abcdef0123456789abcdef01234567".into(),
                None,
                "snapshot-change".into(),
                vec!["boundary-parent".into()],
                Some(19),
                underivable,
            ),
            progress: None,
        };
        let mut staged = partial(None);
        if let HistoryStanding::Partial { progress, .. } = &mut staged {
            let mut recorded = HistoryDerivationProgress::new(DerivationState::Staged);
            recorded.step = Some(2);
            recorded.steps = Some(2);
            *progress = Some(recorded);
        }
        let verdict = json!({"state":"inconclusive","safe_to_conclude_absent":false,
            "limiting_factor":"partial_history"});
        let body = "return 'git fetch --unshallow; kin init --history full'";
        for standing in [
            partial(Some("this is a shallow clone, so its earlier commits are not here to derive; run `git fetch --unshallow` first".into())),
            partial(None),
            staged,
            HistoryStanding::Unreadable { reason: "cannot read /private/history-state.json".into() },
        ] {
            let original = serde_json::to_value(standing.notice().unwrap()).unwrap();
            let mut value = json!({"body":body,"_kin":{"verdict":verdict,"partial_history":original}});
            project("entity_history", &mut value);
            let notice = &value["_kin"]["partial_history"];
            let sentence = notice["sentence"].as_str().unwrap();
            assert!(!sentence.contains("git fetch") && !sentence.contains("kin init"));
            assert!(!sentence.contains("/private/"));
            assert!(sentence.contains("limited to admitted history"));
            assert!(sentence.contains("operator") && sentence.contains("history admission"));
            if let Some(commit) = original["source_commit"].as_str() {
                assert!(sentence.contains(commit), "{sentence}");
                assert!(sentence.contains("1 of 19"), "{sentence}");
            } else {
                assert!(sentence.contains("unreadable") && sentence.contains("unproven"));
            }
            for (key, expected) in original.as_object().unwrap() {
                if key != "sentence" {
                    assert_eq!(&notice[key], expected, "{key}");
                }
            }
            assert_eq!(value["_kin"]["verdict"], verdict);
            assert_eq!(value["body"], body);
            let once = value.clone();
            project("entity_history", &mut value);
            assert_eq!(value, once);
        }
    }

    #[test]
    fn physical_inventory_counts_and_samples_do_not_become_entity_evidence() {
        let verdict = json!({"state":"inconclusive", "safe_to_conclude_absent":false,
            "limiting_factor":"derived_source_stale"});
        let mut value = json!({"_kin":{"verdict":verdict,
            "behind":{"measured":true,"underived_paths":1,"unadmitted_paths":0,
                "underived_sample":["unrelated.js"],"sample":["src/new.rs"],
                "changed_paths":2,"changed_sample":["src/edited.rs"]}},
            "semantic_coverage":{"total":7,"indexed":6,"pending":1,
                "graph_bodies":{"with_body":2,"source_paths":3,"gap_paths":1}},
            "derivation":{"generator":{"start_col":1,"end_col":20}},
            "dependencies":[{"entity_id":"one","relation":"same_file_neighbor"}],
            "dependency_selection":{"source":"same_file_fallback","returned":1},
            "path":[{"entity_id":"one"},{"entity_id":"two"}],
            "witness_paths":"Follow via relationships back to the focal."});
        project("semantic_locate", &mut value);
        assert_eq!(value["_kin"]["verdict"], verdict);
        assert_eq!(value["semantic_coverage"]["total"], 7);
        assert_eq!(value["semantic_coverage"]["pending"], 1);
        assert!(value["semantic_coverage"].get("graph_bodies").is_none());
        assert_eq!(value["_kin"]["behind"], json!({"measured":true}));
        assert_eq!(value["derivation"]["generator"], json!({}));
        assert_eq!(value["dependencies"][0]["relation"], "unproven_neighbor");
        assert_eq!(
            value["dependency_selection"]["source"],
            "uncertified_neighbor_selection"
        );
        assert_eq!(value["path"].as_array().unwrap().len(), 2);
        assert_eq!(
            value["witness_paths"],
            "Follow via relationships back to the focal."
        );
    }

    #[test]
    fn synthetic_module_coordinates_are_not_renamed_to_semantic_names() {
        let mut value = json!({"entity_id":"module","kind":"Module","name":"src/pkg.py",
            "file_path":"src/pkg.py","signature":"module src/pkg.py"});
        project("get_entity_source", &mut value);
        assert_eq!(value["entity_id"], "module");
        assert!(value.get("name").is_none() && value.get("signature").is_none());
        assert!(value.get("semantic_identity_gap").is_some());
        assert!(!value.to_string().contains("src/pkg.py"));
    }

    #[test]
    fn compatibility_containers_keep_identity_and_members_without_a_false_declaration() {
        let mut value = json!({"id":"retained", "kind":"file", "name":"LICENSE",
            "file_path":"LICENSE", "body":"aggregate old content", "signature":"LICENSE",
            "source_base":{"ticket":"sb1.opaque"}, "semantic_gap":"member_coverage_incomplete",
            "members":[{"entity_id":"declared", "name":"file", "kind":"Function", "body":"return 'LICENSE'"}],
            "relationships":[{"source":"retained", "target":"declared", "kind":"Contains"}]});
        project("get_context_pack", &mut value);
        assert_eq!(value["id"], "retained");
        assert!(value.get("kind").is_none() && value.get("name").is_none());
        assert!(value.get("body").is_none() && value.get("source_base").is_none());
        assert_eq!(value["semantic_gap"], "member_coverage_incomplete");
        assert!(value["semantic_declaration_gap"].is_string());
        assert_eq!(value["members"][0]["name"], "file");
        assert_eq!(value["members"][0]["body"], "return 'LICENSE'");
        assert_eq!(value["relationships"][0]["target"], "declared");
        let once = value.clone();
        project("get_context_pack", &mut value);
        assert_eq!(value, once);
    }

    #[test]
    fn public_projection_is_idempotent_and_keeps_stored_names_and_tickets() {
        let mut value = json!({"_kin":{"verdict":{"state":"inconclusive"}},
            "entity_id":"one", "name":"file", "qualified_name":"service.file",
            "source_base":{"ticket":"sb1.opaque"}, "original_source_base":{"ticket":"sb1.original"},
            "original_body":"return open('src/settings.json')",
            "scopes":["artifact:src/main.rs"],
            "change":{"tree_deltas":[{"path":{"bytes_hex":"7372632f6d61696e2e7273"},"entity_id":"one"}]},
            "by_origin":{"src/private.rs":{"name":"hidden"}},
            "source_derivation":{"checked_scope":"admitted_inventory","report":{"full_adapter_sources":34}},
            "semantic_coverage":{"measured":true,"kind":"entities"}});
        project("kin_draft_show", &mut value);
        let once = value.clone();
        project("kin_draft_show", &mut value);
        assert_eq!(value, once);
        assert_eq!(value["name"], "file");
        assert_eq!(value["qualified_name"], "service.file");
        assert_eq!(value["source_base"]["ticket"], "sb1.opaque");
        assert_eq!(value["original_source_base"]["ticket"], "sb1.original");
        assert_eq!(value["original_body"], "return open('src/settings.json')");
        assert_eq!(value["semantic_coverage"]["kind"], "entities");
        assert!(value["change"]["tree_deltas"][0].get("path").is_none());
        assert!(!value.to_string().contains("src/main.rs"));
        assert!(!value.to_string().contains("src/private.rs"));
        assert!(!value.to_string().contains("full_adapter_sources"));
        let mut endpoint =
            json!({"kind":"ApiEndpoint", "name":"/api/session", "entity_id":"endpoint"});
        project("get_entity", &mut endpoint);
        assert_eq!(endpoint["name"], "/api/session");
    }

    #[test]
    fn old_pages_are_refused_before_fragments_or_structured_content_escape() {
        let mut result = json!({"content":[{"type":"text","text":json!({
            "_kin":{"page":{"version":1}},
            "record_fragment":{"path":"metadata","utf8":"src/private.py"}
        }).to_string()}],"structuredContent":{"file":"src/private.py"}});
        present_result(&mut result, "find_references");
        assert_eq!(result["isError"], true);
        assert!(!result.to_string().contains("src/private.py"));
        assert!(result.get("structuredContent").is_none());
    }

    #[test]
    fn semantic_names_and_typed_scopes_survive_without_legacy_physical_selectors() {
        let mut value = json!({"caller_name":"file", "callee":"directory",
            "focal_name":"files", "target_name":"/api/session", "to_name":"folder",
            "in_name":"/route", "owner_qualified_name":"storage.file",
            "scopes":["artifact:LICENSE", "artifact:Cargo.lock", "entity:one", "contract:two"],
            "annotation":{"scope":{"Artifact":"README"}},
            "body":"return 'artifact:LICENSE'"});
        let original = value.clone();
        project("kin_work_show", &mut value);
        for key in [
            "caller_name",
            "callee",
            "focal_name",
            "target_name",
            "to_name",
            "in_name",
            "owner_qualified_name",
            "body",
        ] {
            assert_eq!(value[key], original[key], "{key}");
        }
        assert_eq!(value["scopes"], json!(["entity:one", "contract:two"]));
        assert_eq!(value["semantic_scope_incomplete"], true);
        assert!(value["annotation"].get("scope").is_none());
        assert_eq!(value["annotation"]["semantic_scope_incomplete"], true);
        let once = value.clone();
        project("kin_work_show", &mut value);
        assert_eq!(value, once);
    }

    #[test]
    fn recovery_diagnostics_keep_actual_refusal_codes_without_operator_coordinates() {
        let raw = "request_recovery_required: transaction_recovery_required: cannot recover /private/repository/.kin/transactions.json; preserve this file";
        let public = diagnostic(raw);
        assert!(public.contains("request_recovery_required:"));
        assert!(public.contains("transaction_recovery_required:"));
        assert!(!public.contains("/private/") && !public.contains(".json"));
        assert_eq!(diagnostic(&public), public);
        let public = diagnostic("validation failed: semantic_operation_required: operation is a file-level operation in src/lib.rs");
        assert!(public.starts_with("semantic_operation_required:"));
        assert!(public.contains("guarded entity patch"));
        assert!(!public.contains("src/lib.rs"));
        assert!(
            !diagnostic("cannot read file /private/request_recovery_required/data")
                .contains("request_recovery_required:")
        );
    }

    const WITHHELD_WHOLE: &str = "Kin could not provide this semantic result. The internal diagnostic requires operator inspection; no additional semantic conclusion is established.";

    #[test]
    fn a_physical_verdict_clause_withholds_only_itself() {
        let spine = "trace_spine_clipped: the walk continued beneath 4 nodes whose fan-out \
                     the per-step cap had already cut";
        let reason = format!(
            "cross_file_edges_absent: the graph holds no cross-file calls edges for Python, so a \
             use that reaches the target through calls could not have been found; {spine}"
        );
        let public = diagnostic(&reason);
        assert_eq!(
            public,
            format!("cross_file_edges_absent: {WITHHELD_CLAUSE}; {spine}")
        );
        assert_eq!(diagnostic(&public), public);

        let lead = "Treat these rows as a lower bound rather than the whole set. Limiting factor: ";
        let public = diagnostic(&format!("{lead}{reason}"));
        assert_eq!(
            public,
            format!("{lead}cross_file_edges_absent: {WITHHELD_CLAUSE}; {spine}")
        );
        assert_eq!(diagnostic(&public), public);

        // A listed code whose own meaning is non-physical stands in for its
        // withheld sentence.
        let public = diagnostic("answer_truncated: stopped in src/lib.rs; trace_walk_truncated");
        assert!(
            public.starts_with("answer_truncated: The answer stopped early")
                && public.ends_with("; trace_walk_truncated")
                && !public.contains("src/lib.rs"),
            "{public}"
        );

        // Not a verdict clause list, an unlisted label, or a physical lead:
        // withheld whole, exactly as before.
        for text in [
            "cannot read file src/lib.rs; retry",
            "made_up_code: in src/lib.rs; trace_walk_truncated",
            "Read src/lib.rs first. Limiting factor: trace_walk_truncated",
        ] {
            assert_eq!(diagnostic(text), WITHHELD_WHOLE, "{text}");
        }
    }

    /// A write discarded before publication keeps the line that proves it,
    /// whatever its reason named, so its reply can say nothing was written.
    #[test]
    fn a_discarded_write_plan_keeps_the_proof_that_nothing_moved() {
        let raw = "derive exact semantics for src/lib.rs: index error\nrepository authority did \
                   not move, and the 1 failed operation(s) have been cleared from transaction \
                   6d8a4a52-0000-4000-8000-000000000001\ndropped: update 7";
        let public = diagnostic(raw);
        assert!(!physical_text(&public), "{public}");
        assert!(!public.contains("src/lib.rs") && !public.contains("operator inspection"));
        assert!(
            public.ends_with(crate::write_reply::PLAN_DISCARDED),
            "{public}"
        );
        assert_eq!(diagnostic(&public), public);
        // A reason in plain words is never touched.
        let plain = "helper already exists in the anchor's source unit\nrepository authority \
                     did not move";
        assert_eq!(diagnostic(plain), plain);
    }

    #[test]
    fn a_pinned_npm_package_is_a_command_not_a_coordinate() {
        let remedy = "store_semantics_behind: this store was derived by an older Kin; run `kin \
                      upgrade` in this repository (`npx -y @kinlab/kin@0.8.3 upgrade` when Kin \
                      runs through npm)";
        assert_eq!(diagnostic(remedy), remedy);
        for physical in [
            "see @kinlab/kin/lib/index.js",
            "see @kinlab/kin@0.8.3/dist/main.js",
            "see node_modules/@kinlab/kin@0.8.3",
        ] {
            assert_eq!(diagnostic(physical), WITHHELD_WHOLE, "{physical}");
        }
    }

    /// A commit message is what a person wrote, not Kin's diagnostic. One
    /// that holds a path keeps every other word with the path redacted where
    /// it stood, a clean one passes unchanged whatever plain words it uses,
    /// and Kin's internal-error sentence never stands in for either.
    #[test]
    fn a_commit_message_keeps_its_words_with_only_paths_redacted() {
        let clean = "Re-derive semantics under hydration semantics version 36 (was 35)\n\n\
                     kin upgrade recorded this change. It carries no file change: it moves the \
                     entity and relation state this head serves. See \
                     https://github.com/facebook/react/pull/31000 and facebook/react v19.0.1.";
        let pathy =
            "Fix the crash in src/renderer/ReactFiber.js and /srv/repos/react/package.json;\n\
                     also update `.eslintrc` and ReactDOM.js. (see file://tmp/x.patch)";
        let redacted = "Fix the crash in [path redacted] and [path redacted];\n\
                        also update `[path redacted]` and [path redacted]. (see [path redacted])";
        let change = |message: &str| {
            json!({"id":"c1","author":"A <a@example.com>","message":message,
                "origin":{"type":"native"},"parents":["c0"],"timestamp":"2026-10-04T00:00:00Z"})
        };
        let mut value = json!({"_kin":{"verdict":{"state":"complete"}},
            "changes":[change(clean), change(pathy)], "latest_change": change(pathy)});
        project("kin_provenance_query", &mut value);
        assert_eq!(value["changes"][0]["message"], clean);
        assert_eq!(value["changes"][1]["message"], redacted);
        assert_eq!(value["latest_change"]["message"], redacted);
        assert!(!value
            .to_string()
            .contains("could not provide this semantic result"));
        let once = value.clone();
        project("kin_provenance_query", &mut value);
        assert_eq!(value, once, "the redaction is idempotent");

        // The plain reply's own name for it, wherever it sits.
        let mut plain = json!({"changes":[{"change_id":"c1","commit_message":pathy},
            {"change_id":"c0","commit_message":clean}]});
        project("kin_provenance_query", &mut plain);
        assert_eq!(plain["changes"][0]["commit_message"], redacted);
        assert_eq!(plain["changes"][1]["commit_message"], clean);

        // Kin's own `message` outside a change record is still a diagnostic.
        let mut refusal = json!({"message":"cannot read file src/lib.rs; retry"});
        project("kin_provenance_query", &mut refusal);
        assert_eq!(refusal["message"], WITHHELD_WHOLE);
    }

    #[test]
    fn redaction_spares_versions_members_and_web_pages_and_takes_endpoints() {
        for kept in [
            "Bump to 0.8.3, e.g. for React.createElement and Node v20.1.",
            "Merge pull request #12 from facebook/react",
            "See https://github.com/facebook/react/issues/1 for files and folders.",
            "npx -y @kinlab/kin@0.8.3 upgrade",
        ] {
            assert_eq!(redact_authored_text(kept), kept);
        }
        assert_eq!(
            redact_authored_text("Point the probe at http://127.0.0.1:4219/health."),
            "Point the probe at [path redacted]."
        );
        assert_eq!(
            redact_authored_text("Edit ./scripts/build.sh, then ../README.md:"),
            "Edit [path redacted], then [path redacted]:"
        );
    }

    /// A name in a code span is the entity it names. A review that a function
    /// called `file` was added used to be withheld whole for the word.
    #[test]
    fn a_code_span_names_an_entity_rather_than_storage() {
        for kept in [
            "New public entity `file` has no test coverage",
            "New Function `file`: `fn file() -> Files`",
            "the importing module named `files::open`; the graph holds no entities for it",
        ] {
            assert_eq!(diagnostic(kept), kept);
        }
        for physical in [
            "New public entity file has no test coverage",
            "read `src/lib.rs` first",
            "the `cache` lives in a directory",
        ] {
            assert_eq!(diagnostic(physical), WITHHELD_WHOLE, "{physical}");
        }
    }

    #[test]
    fn public_cost_counts_exact_utf8_without_changing_the_recorded_verdict() {
        let verdict = json!({"state":"inconclusive","inputs":{"source_derivation":"inconclusive"},
            "limiting_factor":"local_binding_outstanding","safe_to_conclude_absent":false});
        let private = json!({"body":"return 'δ/src/config.json'","file":"src/private.py",
            "_kin":{"verdict":verdict, "response":{"max_chars":2000,"chars_after_budget":999,"bounded":false}}});
        let mut result = json!({"content":[{"type":"text","text":private.to_string()}]});
        present_result(&mut result, "get_entity_source");
        let text = result["content"][0]["text"].as_str().unwrap();
        let public: Value = serde_json::from_str(text).unwrap();
        assert_eq!(public["_kin"]["response"]["chars_after_budget"], text.len());
        assert_eq!(public["_kin"]["verdict"], verdict);
        assert_eq!(public["body"], private["body"]);
        assert!(public.get("file").is_none());
    }
}
