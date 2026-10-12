// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Literal membership over graph-owned stored entity fields.
//!
//! Enumerates the complete kind-filtered entity set, then checks the six
//! stored fields directly. This is independent of BM25 tokenization, candidate
//! limits, and derived-index commit/rebuild state. It scans and materializes the
//! scoped graph on every page; the page size bounds the response, not the work.
//!
//! A cursor names the canonical contents of the matching entity set, not a
//! whole-graph revision. Exact source-line enrichment is best effort and only
//! accepts a source body whose coherence with the entity span is verified.
//! No filesystem search or structural-reference claim is involved.

use std::collections::HashMap;

use base64::Engine;
use serde_json::{json, Value};

use kin_model::entity::{Entity, EntityRole};
use kin_model::graph::{EntityFilter, GraphStore};

use super::common::{
    entity_presentation_end_line, entity_presentation_start_line, entity_read_path,
    parse_kind_filter, presentation_line, read_entity_source_exact, HeldSourceAuthority,
    SpanCoherence,
};
use super::repository_authority::RequestRepositoryAuthority;
use crate::error::{McpError, Result};
use crate::types::ToolCallResult;

/// The name this tool is registered and dispatched under.
pub const TOOL_NAME: &str = "lexical_lookup";

pub const LEXICAL_LOOKUP_DESC: &str = "\
Find exact literals, including punctuation, in stored graph fields: entity name, signature, \
doc summary, source body preview, and file import/surface context. Matching folds ASCII \
case only. This enumerates entities in the requested kind and test-role scope and checks those fields \
directly; it does not depend on the lexical index. Previews may be sampled or truncated, \
file paths and unadmitted source are excluded; whole-file module nodes expose only identity metadata, and a miss cannot prove repository absence. \
A hit is lexical evidence, never a resolved call or reference; use find_references or \
trace_data_flow for structural relationships. Pass the bare literal, not a question about it. \
Hits carry the matched field and excerpt; exact source lines require verified span/source \
coherence, otherwise only the stored entity span is supplied. Results are ordered by entity \
id, without a relevance score. limit bounds one page; total_matching is the exact count in \
these stored fields. Every page scans and materializes the scoped graph. A cursor binds the \
literal, kind, include_tests and matching entity contents; changed results require a fresh lookup. \
Test-role entities are excluded by default; include_tests true includes them. kind test selects \
only test-role entities and cannot be combined with include_tests false.";

/// The response key carrying this tool's disclosure: what stored fields do and
/// do not cover, and the caution that a hit is not a resolved reference.
pub const DISCLOSURE_KEY: &str = "disclosure";

/// The most hits one page may hold.
const DEFAULT_LIMIT: usize = 20;
const MAX_LIMIT: usize = 100;

const MAX_LITERAL_BYTES: usize = 4096;
const MAX_CURSOR_BYTES: usize = 65_536;
/// The version of the base64 JSON cursor earlier builds minted. It still
/// decodes; this build mints the short handle [`HANDLE_PREFIX`] opens.
const LEGACY_CURSOR_VERSION: u64 = 1;

/// How a cursor this build mints begins: `lit.<offset>.<total>.<matches>.<kind>.
/// <literal>.<include_tests>.<check>`, with the literal in unpadded base64url and `check` the
/// first [`HANDLE_CHECK_HEX`] hex digits of the digest of everything before
/// it. Short, because an agent copies it by hand, and checked, because a copy
/// that changed one character must be refused rather than read as another
/// cursor.
const HANDLE_PREFIX: &str = "lit";

/// How a cursor of a text search over held content begins, in the same form.
const TEXT_HANDLE_PREFIX: &str = "txt";
const BOUNDED_TEXT_HANDLE_PREFIX: &str = "txt2";
use super::text_match::Options;

/// Hex digits of the matching-set fingerprint a cursor carries.
const MATCHING_SET_HEX: usize = 12;

/// Hex digits of a handle's check.
const HANDLE_CHECK_HEX: usize = 6;

/// What a caller is told when a cursor does not read back exactly as minted.
pub(crate) const CURSOR_UNREADABLE: &str = "the cursor was not copied exactly; resend \
     next_cursor unchanged as cursor alone, or omit cursor and repeat the query and mode to \
     start again at the first match.";

/// Byte ceiling passed to [`read_entity_source_exact`] when resolving a body
/// match's exact line. Generous: the body is read and thrown away after one
/// `find`, never returned, so the cost is paid once per hit on one page.
const EXACT_LINE_READ_MAX_BYTES: usize = 2_000_000;

/// The metadata key `kin-parser/src/extract.rs::embedding_body_preview` writes
/// its output under. Not re-exported at `kin_parser`'s crate root (only
/// `kin_parser::DECLARATION_LINE_KEY` and the file-context keys are), so this
/// is the same private local copy `kin-db/src/search/text.rs` keeps for the
/// same reason.
const EMBEDDING_BODY_PREVIEW_KEY: &str = "embedding_body_preview";

/// Which of an entity's own stored fields carried the literal, in the exact
/// priority order `kin-db/src/search/text.rs::entity_fields` weights them
/// (name highest, then signature, doc summary, body preview, import context,
/// surface context).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MatchedField {
    Name,
    Signature,
    DocSummary,
    BodyPreview,
    FileImportContext,
    FileSurfaceContext,
}

const FIELD_PRIORITY: [MatchedField; 6] = [
    MatchedField::Name,
    MatchedField::Signature,
    MatchedField::DocSummary,
    MatchedField::BodyPreview,
    MatchedField::FileImportContext,
    MatchedField::FileSurfaceContext,
];

impl MatchedField {
    fn as_str(self) -> &'static str {
        match self {
            MatchedField::Name => "name",
            MatchedField::Signature => "signature",
            MatchedField::DocSummary => "doc_summary",
            MatchedField::BodyPreview => "body_preview",
            MatchedField::FileImportContext => "file_import_context",
            MatchedField::FileSurfaceContext => "file_surface_context",
        }
    }

    /// Whether a match on this field sits at the entity's own declaration
    /// line, so [`entity_presentation_start_line`] answers "the line where the
    /// literal occurs" exactly, with no source read. `false` for the three
    /// fields that are built from the entity's whole body or from file-level
    /// context rather than from its declaration line alone.
    fn anchored_at_declaration(self) -> bool {
        matches!(
            self,
            MatchedField::Name | MatchedField::Signature | MatchedField::DocSummary
        )
    }
}

/// One entity's own copy of the field `field` names, or `None` when the
/// entity carries nothing under that key.
fn field_text(entity: &Entity, field: MatchedField) -> Option<&str> {
    // Stored module previews can predate declaration-span validation and carry
    // sibling bodies. Keep identity matching; module content must come through
    // verified source authority, not unverified retrieval metadata.
    if (kin_model::is_file_module_surface(entity)
        || matches!(
            entity.kind,
            kin_model::EntityKind::Module | kin_model::EntityKind::Package
        ))
        && !matches!(field, MatchedField::Name | MatchedField::Signature)
    {
        return None;
    }
    match field {
        MatchedField::Name => Some(entity.name.as_str()).filter(|text| !text.is_empty()),
        MatchedField::Signature => Some(entity.signature.as_str()).filter(|text| !text.is_empty()),
        MatchedField::DocSummary => entity.doc_summary.as_deref(),
        MatchedField::BodyPreview => entity
            .metadata
            .extra
            .get(EMBEDDING_BODY_PREVIEW_KEY)
            .and_then(Value::as_str),
        MatchedField::FileImportContext => entity
            .metadata
            .extra
            .get(kin_parser::FILE_IMPORT_CONTEXT_KEY)
            .and_then(Value::as_str),
        MatchedField::FileSurfaceContext => entity
            .metadata
            .extra
            .get(kin_parser::FILE_SURFACE_CONTEXT_KEY)
            .and_then(Value::as_str),
    }
}

/// The byte offset of `needle`'s first case-insensitive occurrence in
/// `haystack`, or `None`.
///
/// Folds ASCII letters only. Comparing only the ASCII range means the byte length of
/// `haystack` never shifts under folding the way a full Unicode lowercasing
/// pass can for some scripts, so the returned offset stays a valid index into
/// the ORIGINAL string, not a copy of it.
fn find_ascii_insensitive(haystack: &str, needle: &str) -> Option<usize> {
    let hay = haystack.as_bytes();
    let need = needle.as_bytes();
    if need.is_empty() || need.len() > hay.len() {
        return None;
    }
    (0..=(hay.len() - need.len())).find(|&start| {
        need.iter()
            .enumerate()
            .all(|(i, &b)| hay[start + i].eq_ignore_ascii_case(&b))
    })
}

/// The first stored field carrying the whole literal, in field priority order.
fn matched_field(entity: &Entity, literal: &str) -> Option<MatchedField> {
    FIELD_PRIORITY.into_iter().find(|&field| {
        field_text(entity, field)
            .is_some_and(|text| find_ascii_insensitive(text, literal).is_some())
    })
}

/// A short excerpt of the matched field's own text, centered on the literal.
fn excerpt(entity: &Entity, field: MatchedField, literal: &str) -> Option<String> {
    const RADIUS: usize = 80;
    let text = field_text(entity, field)?;
    let start = find_ascii_insensitive(text, literal)?;

    let mut lo = start.saturating_sub(RADIUS);
    while lo > 0 && !text.is_char_boundary(lo) {
        lo -= 1;
    }
    let mut hi = (start + literal.len() + RADIUS).min(text.len());
    while hi < text.len() && !text.is_char_boundary(hi) {
        hi += 1;
    }

    let mut out = String::new();
    if lo > 0 {
        out.push_str("... ");
    }
    // One line, whatever the field held: the excerpt is the matching line a
    // hit is shown by.
    out.push_str(
        &text[lo..hi]
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" "),
    );
    if hi < text.len() {
        out.push_str(" ...");
    }
    Some(out)
}

/// The longest matching line a hit serves, in characters. A longer one keeps
/// the text around the match, with `...` where it was cut.
const MATCHING_LINE_MAX_CHARS: usize = 160;

/// `line` trimmed, and cut around the first place it holds `literal` when it
/// is longer than [`MATCHING_LINE_MAX_CHARS`].
pub(crate) fn matching_line(line: &str, literal: &str) -> String {
    let line = line.trim();
    if line.chars().count() <= MATCHING_LINE_MAX_CHARS {
        return line.to_string();
    }
    let at = find_ascii_insensitive(line, literal).unwrap_or(0);
    let radius = MATCHING_LINE_MAX_CHARS.saturating_sub(literal.len()) / 2;
    let mut lo = at.saturating_sub(radius);
    while lo > 0 && !line.is_char_boundary(lo) {
        lo -= 1;
    }
    let mut hi = (at + literal.len() + radius).min(line.len());
    while hi < line.len() && !line.is_char_boundary(hi) {
        hi += 1;
    }
    let mut out = String::new();
    if lo > 0 {
        out.push_str("... ");
    }
    out.push_str(line[lo..hi].trim());
    if hi < line.len() {
        out.push_str(" ...");
    }
    out
}

/// Best-effort exact line for a match outside the declaration (a body
/// preview, or file import/surface context), or `None` when it cannot be
/// confirmed.
///
/// The body preview stored on the entity has its whitespace collapsed to
/// single spaces at extraction time (`kin-parser/src/extract.rs`), which
/// means it carries no newlines to count: the technique that recovers a line
/// from a span's OWN bytes cannot be applied to that preview directly. This
/// instead reads the entity's real span bytes through the same graph-owned,
/// content-addressed source authority `get_entity_source` already reads
/// from, finds the literal's byte offset in that unmodified text, and counts
/// newlines up to it. Soft-fail throughout: no local authority binding, no
/// span, a literal the exact body does not confirm verbatim, or any read
/// error all read as "not confirmable" rather than failing the call --
/// resolving a hit's exact line is this tool's own best effort, never a
/// requirement of answering.
fn resolve_exact_line<G: GraphStore>(
    held: &HeldSourceAuthority<'_, G>,
    entity: &Entity,
    literal: &str,
) -> Option<(u32, String)> {
    let span = entity.span.as_ref()?;
    let exact = read_entity_source_exact(held, entity, EXACT_LINE_READ_MAX_BYTES).ok()??;
    if exact.span_coherence == SpanCoherence::Unverified {
        return None;
    }
    let offset = find_ascii_insensitive(&exact.body, literal)?;
    if !exact.body.is_char_boundary(offset) {
        return None;
    }
    let newlines_before = exact.body[..offset].matches('\n').count() as u32;
    // The line the match sits on, as written: what a text search is read for.
    let line_start = exact.body[..offset].rfind('\n').map_or(0, |at| at + 1);
    let line_end = exact.body[offset..]
        .find('\n')
        .map_or(exact.body.len(), |at| offset + at);
    let text = matching_line(&exact.body[line_start..line_end], literal);
    Some((presentation_line(span.start_line + newlines_before), text))
}

/// The line to report for one hit, and how sure this tool is of it.
fn hit_line<G: GraphStore>(
    held: &HeldSourceAuthority<'_, G>,
    entity: &Entity,
    field: MatchedField,
    literal: &str,
) -> (Option<u32>, &'static str, Option<String>) {
    if field.anchored_at_declaration() {
        return (entity_presentation_start_line(entity), "declaration", None);
    }
    match resolve_exact_line(held, entity, literal) {
        Some((line, text)) => (Some(line), "exact", Some(text)),
        None => (None, "entity_span_only", None),
    }
}

/// One verified occurrence from the graph-owned entity snapshot.
struct Hit {
    entity: Entity,
    field: MatchedField,
}

fn build_hit<G: GraphStore>(held: &HeldSourceAuthority<'_, G>, hit: &Hit, literal: &str) -> Value {
    let (line, line_confidence, line_text) = hit_line(held, &hit.entity, hit.field, literal);
    json!({
        "entity_id": hit.entity.id,
        "name": hit.entity.name,
        "kind": hit.entity.kind,
        "language": hit.entity.language,
        "file": entity_read_path(&hit.entity),
        "start_line": entity_presentation_start_line(&hit.entity),
        "end_line": entity_presentation_end_line(&hit.entity),
        "line": line,
        "line_confidence": line_confidence,
        "matched_field": hit.field.as_str(),
        "score": null,
        // The matching source line when the exact line was confirmed, and the
        // stored field's text around the match otherwise.
        "excerpt": line_text.or_else(|| excerpt(&hit.entity, hit.field, literal)),
    })
}

/// Scope and cost travel with every answer, including a zero-match answer.
fn disclosure(total_matching: usize) -> Value {
    json!({
        "answered_by": "stored_graph_fields",
        "matching": "ascii_case_insensitive_literal",
        "complete_for": "matching entities in the requested kind and test-role scope over these six stored fields",
        "detail": "Reads stored graph fields: name, signature, doc summary, source body preview, \
                   and module import and surface context. Body previews collapse whitespace and \
                   may be sampled beyond 8000 characters. Repository coordinates and unadmitted \
                   source are excluded. \
                   No derived text index is required. Exact source-line enrichment is accepted \
                   only when its bytes are verified against the entity span's provenance.",
        "cost": "Every page scans and materializes the scoped graph; limit bounds returned hits, not scan work.",
        "caution": if total_matching == 0 {
            "No matching stored graph field was found in this kind and test-role scope. This is not proof of \
             repository absence: previews are bounded, paths are excluded, and unadmitted source \
             is outside this enumeration."
        } else {
            "These are lexical occurrences in stored graph fields, not resolved references. A hit \
             does not mean the entity calls, imports, or is called by anything; use \
             find_references or trace_data_flow for that claim."
        },
    })
}

/// A continuation of one literal lookup: where the next page starts in the
/// entity-id order every page shares, how many matched when it was minted, and
/// a fingerprint of which entities matched.
///
/// It is bound to the literal, kind and test-role scope, and to that set of
/// matching entities. When the set changed, a page cut at its offset could
/// repeat or skip a match, so the cursor is refused with the counts and a
/// restart. An edit to a matching entity's other fields, such as a background
/// enrichment, moves no row and changes nothing.
struct LexicalCursor {
    /// The search reads held module text rather than stored fields.
    held_text: bool,
    options: Options,
    authority: Option<String>,
    literal: String,
    kind: Option<String>,
    include_tests: bool,
    offset: usize,
    total: usize,
    /// The matching-set fingerprint, or `None` for a version 1 token, whose
    /// digest covered whole records and is not comparable.
    matching: Option<String>,
    /// The module path a text search was confined to, as the graph spells it.
    module_scope: Option<String>,
}

/// Base64 in either alphabet, padded or not: the same bytes either way.
fn decode_base64(token: &str) -> Option<Vec<u8>> {
    use base64::engine::general_purpose::{STANDARD_NO_PAD, URL_SAFE_NO_PAD};
    let bare = token.trim_end_matches('=');
    URL_SAFE_NO_PAD
        .decode(bare)
        .or_else(|_| STANDARD_NO_PAD.decode(bare))
        .ok()
}

/// The token as sent, without the whitespace or quotes a copy wrapped it in.
fn unwrapped(token: &str) -> &str {
    token
        .trim()
        .trim_matches(|c: char| c == '"' || c == '\'' || c == '`')
}

/// The check a handle ends with, over everything before it.
fn handle_check(body: &str) -> String {
    kin_blobs::digest(body.as_bytes())
        .to_string()
        .chars()
        .take(HANDLE_CHECK_HEX)
        .collect()
}

fn is_hex(text: &str, len: usize) -> bool {
    text.len() == len && text.bytes().all(|b| b.is_ascii_hexdigit())
}

impl LexicalCursor {
    fn encode(&self) -> String {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        if self.held_text {
            let mut body = format!(
                "{}.{}.{}.{}.{}.{}.{}.{}.{}.{}",
                BOUNDED_TEXT_HANDLE_PREFIX,
                self.offset,
                self.total,
                self.matching.as_deref().unwrap_or_default(),
                self.kind.as_deref().unwrap_or_default(),
                URL_SAFE_NO_PAD.encode(self.literal.as_bytes()),
                u8::from(self.include_tests),
                u8::from(self.options.pattern),
                u8::from(self.options.ignore_case),
                self.authority.as_deref().unwrap_or_default()
            );
            // A confined search carries its module path, so a page continues
            // inside it; a handle without one searched every module.
            if let Some(module) = &self.module_scope {
                body.push('.');
                body.push_str(&URL_SAFE_NO_PAD.encode(module.as_bytes()));
            }
            return format!("{body}.{}", handle_check(&body));
        }
        let body = format!(
            "{}.{}.{}.{}.{}.{}.{}",
            if self.held_text {
                TEXT_HANDLE_PREFIX
            } else {
                HANDLE_PREFIX
            },
            self.offset,
            self.total,
            self.matching.as_deref().unwrap_or_default(),
            self.kind.as_deref().unwrap_or_default(),
            URL_SAFE_NO_PAD.encode(self.literal.as_bytes()),
            u8::from(self.include_tests),
        );
        let check = handle_check(&body);
        format!("{body}.{check}")
    }

    fn decode(token: &str) -> Option<Self> {
        if token.len() > MAX_CURSOR_BYTES {
            return None;
        }
        let token = unwrapped(token);
        let cursor = if (token.starts_with(HANDLE_PREFIX) || token.starts_with(TEXT_HANDLE_PREFIX))
            && token.contains('.')
        {
            Self::decode_handle(token)?
        } else {
            Self::decode_legacy(token)?
        };
        if cursor.literal.is_empty()
            || (!cursor.held_text && cursor.literal.trim().is_empty())
            || cursor.literal.len() > MAX_LITERAL_BYTES
            || cursor
                .kind
                .as_ref()
                .is_some_and(|kind| kind.is_empty() || kind.len() > 64)
            || cursor.offset == 0
            || cursor.offset >= cursor.total
        {
            return None;
        }
        Some(cursor)
    }

    fn decode_handle(token: &str) -> Option<Self> {
        let (body, check) = token.rsplit_once('.')?;
        if !is_hex(check, HANDLE_CHECK_HEX) || handle_check(body) != check {
            return None;
        }
        let parts: Vec<&str> = body.split('.').collect();
        let (parts, module_scope) = match parts.as_slice() {
            [BOUNDED_TEXT_HANDLE_PREFIX, rest @ ..] if rest.len() == 10 => {
                let module = base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .decode(rest[9])
                    .ok()
                    .and_then(|bytes| String::from_utf8(bytes).ok())
                    .filter(|module| !module.is_empty())?;
                (&parts[..10], Some(module))
            }
            _ => (parts.as_slice(), None),
        };
        if let [BOUNDED_TEXT_HANDLE_PREFIX, offset, total, matching, kind, literal, tests, pattern, ignore_case, authority] =
            parts
        {
            let flag = |v: &str| match v {
                "0" => Some(false),
                "1" => Some(true),
                _ => None,
            };
            if !is_hex(matching, MATCHING_SET_HEX) || !is_hex(authority, 64) {
                return None;
            }
            return Some(Self {
                held_text: true,
                options: Options {
                    pattern: flag(pattern)?,
                    ignore_case: flag(ignore_case)?,
                },
                authority: Some((*authority).to_string()),
                literal: String::from_utf8(
                    base64::engine::general_purpose::URL_SAFE_NO_PAD
                        .decode(literal)
                        .ok()?,
                )
                .ok()?,
                kind: (!kind.is_empty()).then(|| (*kind).to_string()),
                include_tests: flag(tests)?,
                offset: offset.parse().ok()?,
                total: total.parse().ok()?,
                matching: Some((*matching).to_string()),
                module_scope,
            });
        }
        // Earlier handles searched every role. Retain that scope when a
        // caller continues one rather than applying the new request default.
        let (fields, include_tests) = match parts {
            [prefix, offset, total, matching, kind, literal] => {
                ([*prefix, *offset, *total, *matching, *kind, *literal], true)
            }
            [prefix, offset, total, matching, kind, literal, tests] => (
                [*prefix, *offset, *total, *matching, *kind, *literal],
                match *tests {
                    "0" => false,
                    "1" => true,
                    _ => return None,
                },
            ),
            _ => return None,
        };
        let [prefix, offset, total, matching, kind, literal] = fields;
        let held_text = match prefix {
            HANDLE_PREFIX => false,
            TEXT_HANDLE_PREFIX => true,
            _ => return None,
        };
        if !is_hex(matching, MATCHING_SET_HEX) {
            return None;
        }
        let literal = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(literal)
            .ok()?;
        Some(Self {
            held_text,
            options: Options::default(),
            authority: None,
            literal: String::from_utf8(literal).ok()?,
            kind: (!kind.is_empty()).then(|| kind.to_string()),
            include_tests,
            offset: offset.parse().ok()?,
            total: total.parse().ok()?,
            matching: Some(matching.to_string()),
            module_scope: None,
        })
    }

    fn decode_legacy(token: &str) -> Option<Self> {
        let value: Value = serde_json::from_slice(&decode_base64(token)?).ok()?;
        let object = value.as_object()?;
        if object.len() != 6 || value.get("v")?.as_u64()? != LEGACY_CURSOR_VERSION {
            return None;
        }
        let snapshot = value.get("s")?.as_str()?;
        if !is_hex(snapshot, 64) {
            return None;
        }
        Some(Self {
            held_text: false,
            options: Options::default(),
            authority: None,
            literal: value.get("l")?.as_str()?.to_string(),
            kind: match value.get("k")? {
                Value::Null => None,
                Value::String(kind) => Some(kind.clone()),
                _ => return None,
            },
            include_tests: true,
            offset: usize::try_from(value.get("o")?.as_u64()?).ok()?,
            total: usize::try_from(value.get("t")?.as_u64()?).ok()?,
            matching: None,
            module_scope: None,
        })
    }
}

/// Whether `token` has the form of a literal lookup's cursor, so a cursor sent
/// without a mode reaches the literal search, whose check then says whether
/// it was copied exactly, rather than the description search, whose cursors
/// look like neither form.
pub(crate) fn is_literal_cursor(token: &str) -> bool {
    let token = unwrapped(token);
    token.starts_with(&format!("{HANDLE_PREFIX}."))
        || token.starts_with(&format!("{TEXT_HANDLE_PREFIX}."))
        || token.starts_with(&format!("{BOUNDED_TEXT_HANDLE_PREFIX}."))
        || decode_base64(token)
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .is_some_and(|value| value.get("l").is_some_and(Value::is_string))
}

/// Continue immediately after the rows the response budget actually retained,
/// including a nominal final page that originally needed no continuation.
pub(crate) fn cursor_after_withheld(payload: &Value, kept: usize) -> Option<String> {
    let start = usize::try_from(payload.get("page_offset")?.as_u64()?).ok()?;
    let total = usize::try_from(payload.get("total_matching")?.as_u64()?).ok()?;
    let offset = start.checked_add(kept)?;
    if kept == 0 || offset >= total {
        return None;
    }
    let cursor = LexicalCursor {
        held_text: payload.get(HELD_TEXT_KEY).and_then(Value::as_bool) == Some(true),
        options: Options {
            pattern: payload
                .get("pattern")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            ignore_case: payload
                .get("ignore_case")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        },
        authority: payload
            .get("text_authority")
            .and_then(Value::as_str)
            .map(str::to_string),
        literal: payload.get("literal")?.as_str()?.to_string(),
        kind: match payload.get("kind")? {
            Value::Null => None,
            Value::String(kind) => Some(kind.clone()),
            _ => return None,
        },
        include_tests: payload
            .get("include_tests")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        offset,
        total,
        matching: payload
            .get("matching_snapshot")
            .and_then(Value::as_str)
            .map(|matching| matching.chars().take(MATCHING_SET_HEX).collect()),
        module_scope: payload
            .pointer(&format!(
                "/{}/module",
                crate::handlers::review::IMPACT_ROW_SCOPE_KEY
            ))
            .and_then(Value::as_str)
            .map(str::to_string),
    };
    let token = cursor.encode();
    LexicalCursor::decode(&token).map(|_| token)
}

fn optional_string(
    args: &HashMap<String, Value>,
    key: &str,
    max_bytes: usize,
) -> Result<Option<String>> {
    args.get(key)
        .map(|value| {
            let text = value
                .as_str()
                .ok_or_else(|| McpError::InvalidParams(format!("{key} must be a string")))?
                .trim();
            if text.is_empty() || text.len() > max_bytes {
                return Err(McpError::InvalidParams(format!(
                    "{key} must contain 1..{max_bytes} bytes"
                )));
            }
            Ok(text.to_string())
        })
        .transpose()
}

/// A short fingerprint of which entities match, in page order, which is what a
/// page's offset depends on. A background edit to a matching entity's other
/// fields moves no row, so it leaves the fingerprint, and every cursor minted
/// against it, as it was.
fn matching_snapshot(hits: &[Hit], literal: &str, kind: Option<&str>) -> Result<String> {
    let ids: Vec<String> = hits.iter().map(|hit| hit.entity.id.to_string()).collect();
    matching_snapshot_of("kin-lexical-matching-set-v2", ids, literal, kind)
}

fn matching_snapshot_of(
    label: &str,
    ids: Vec<String>,
    literal: &str,
    kind: Option<&str>,
) -> Result<String> {
    let bytes = serde_json::to_vec(&json!([label, literal, kind, ids])).map_err(McpError::Json)?;
    Ok(kin_blobs::digest(&bytes)
        .to_string()
        .chars()
        .take(MATCHING_SET_HEX)
        .collect())
}

/// Where a lookup says it searched held module text: `locate` with mode
/// `text`. Its cursor carries the same.
pub const HELD_TEXT_KEY: &str = "held_text";

pub fn handle_lexical_lookup<G: GraphStore>(
    args: &HashMap<String, Value>,
    store: &G,
    repository_authority: Option<&RequestRepositoryAuthority>,
) -> Result<ToolCallResult> {
    lookup(args, store, repository_authority, None)
}

/// The lookup. `text_of` stands in for the request's held content where a
/// caller holds the bytes itself; `None` reads them through the request's
/// source authority.
pub(crate) fn lookup<G: GraphStore>(
    args: &HashMap<String, Value>,
    store: &G,
    repository_authority: Option<&RequestRepositoryAuthority>,
    text_of: Option<super::held_text::TextOf<'_>>,
) -> Result<ToolCallResult> {
    let cursor_token = optional_string(args, "cursor", MAX_CURSOR_BYTES)?;
    let cursor = cursor_token
        .as_deref()
        .map(|token| {
            LexicalCursor::decode(token)
                .ok_or_else(|| McpError::InvalidParams(CURSOR_UNREADABLE.into()))
        })
        .transpose()?;
    let requested_literal = if args.get(HELD_TEXT_KEY).and_then(Value::as_bool) == Some(true)
        || cursor.as_ref().is_some_and(|c| c.held_text)
    {
        args.get("literal")
            .map(|value| {
                let text = value
                    .as_str()
                    .ok_or_else(|| McpError::InvalidParams("literal must be a string".into()))?;
                if text.is_empty() || text.len() > MAX_LITERAL_BYTES {
                    return Err(McpError::InvalidParams(format!(
                        "literal must contain 1..{MAX_LITERAL_BYTES} bytes"
                    )));
                }
                Ok(text.to_string())
            })
            .transpose()?
    } else {
        optional_string(args, "literal", MAX_LITERAL_BYTES)?
    };
    let requested_kind = optional_string(args, "kind", 64)?.map(|kind| kind.to_ascii_lowercase());
    let requested_tests = args
        .get("include_tests")
        .filter(|value| !value.is_null())
        .map(|value| {
            value
                .as_bool()
                .ok_or_else(|| McpError::InvalidParams("include_tests must be a boolean".into()))
        })
        .transpose()?;
    // Every population filter is inherited by cursor-only continuation and
    // must agree when repeated. `limit` may change from page to page.
    if let Some(cursor) = &cursor {
        if requested_tests.is_some_and(|include_tests| include_tests != cursor.include_tests) {
            return Err(McpError::InvalidParams(format!(
                "cursor continues a search with include_tests={}, and this call changes it. \
                 Send cursor alone to continue, or omit cursor to start a new search.",
                cursor.include_tests
            )));
        }
        if let Some(literal) = requested_literal
            .as_ref()
            .filter(|literal| *literal != &cursor.literal)
        {
            return Err(McpError::InvalidParams(format!(
                "cursor continues the search for {:?}, and this call asks for {literal:?}. Send \
                 cursor alone to continue, or omit cursor to start a new search.",
                cursor.literal
            )));
        }
        if let Some(kind) = requested_kind
            .as_ref()
            .filter(|kind| Some(*kind) != cursor.kind.as_ref())
        {
            return Err(McpError::InvalidParams(match &cursor.kind {
                Some(was) => format!(
                    "cursor continues a search of kind {was:?}, and this call asks for kind \
                     {kind:?}. Send cursor alone to continue, or omit cursor to start a new search."
                ),
                None => format!(
                    "cursor continues a search of every kind, and this call asks for kind \
                     {kind:?}. Send cursor alone to continue, or omit cursor to start a new search."
                ),
            }));
        }
    }
    let requested_held_text = args.get(HELD_TEXT_KEY).and_then(Value::as_bool);
    if let (Some(asked), Some(cursor)) = (requested_held_text, &cursor) {
        if asked != cursor.held_text {
            return Err(McpError::InvalidParams(format!(
                "cursor continues a {} search, and this call asks for mode {}. Send cursor alone \
                 to continue, or omit cursor to start a new search.",
                if cursor.held_text { "text" } else { "literal" },
                if asked { "text" } else { "literal" },
            )));
        }
    }
    let held_text = requested_held_text
        .or_else(|| cursor.as_ref().map(|c| c.held_text))
        .unwrap_or(false);
    if held_text && args.get("language").is_some_and(|value| !value.is_null()) {
        return Err(McpError::InvalidParams(
            "language filtering is not supported in mode text; omit language".into(),
        ));
    }
    let option = |key: &str, previous: bool| -> Result<bool> {
        let asked = args
            .get(key)
            .map(|value| {
                value
                    .as_bool()
                    .ok_or_else(|| McpError::InvalidParams(format!("{key} must be a boolean")))
            })
            .transpose()?;
        if !held_text && asked.is_some() {
            return Err(McpError::InvalidParams(format!(
                "{key} is only supported in mode text"
            )));
        }
        if cursor.is_some() && asked.is_some_and(|value| value != previous) {
            return Err(McpError::InvalidParams(format!(
                "cursor fixes {key}={previous}; send cursor alone or start a new search"
            )));
        }
        Ok(asked.unwrap_or(previous))
    };
    let options = Options {
        pattern: option(
            "pattern",
            cursor.as_ref().is_some_and(|c| c.options.pattern),
        )?,
        ignore_case: option(
            "ignore_case",
            cursor.as_ref().is_some_and(|c| c.options.ignore_case),
        )?,
    };
    let literal = requested_literal
        .or_else(|| cursor.as_ref().map(|c| c.literal.clone()))
        .ok_or_else(|| McpError::InvalidParams("missing required parameter: literal".into()))?;
    if held_text && !options.pattern && literal.chars().count() < 2 {
        return Err(McpError::InvalidParams(
            "mode text needs at least two characters to find".into(),
        ));
    }
    let kind = requested_kind.or_else(|| cursor.as_ref().and_then(|c| c.kind.clone()));
    // An exact search is after every place the text is written, so test code
    // is searched unless the caller leaves it out.
    let include_tests = requested_tests
        .or_else(|| cursor.as_ref().map(|c| c.include_tests))
        .unwrap_or(held_text || kind.as_deref() == Some("test"));
    if kind.as_deref() == Some("test") && !include_tests {
        return Err(McpError::InvalidParams(
            "kind test selects test-role entities and cannot be combined with include_tests=false"
                .into(),
        ));
    }
    let mut scope = EntityFilter::default();
    if let Some(kind) = kind.as_deref() {
        if kind == "test" {
            scope.roles = Some(vec![EntityRole::Test]);
        } else {
            scope.kinds = Some(parse_kind_filter(kind).ok_or_else(|| {
                McpError::InvalidParams(format!("unsupported entity kind: {kind}"))
            })?);
        }
    }
    let limit = match args.get("limit") {
        Some(value) => value
            .as_u64()
            .ok_or_else(|| McpError::InvalidParams("limit must be a nonnegative integer".into()))?,
        None => DEFAULT_LIMIT as u64,
    }
    .clamp(1, MAX_LIMIT as u64) as usize;
    let offset = cursor.as_ref().map_or(0, |c| c.offset);

    // A text search may be confined to a module path; a continuation keeps
    // the one its search began with.
    let requested_scope = optional_string(args, crate::trace_scope::SCOPE_ARGUMENT, 512)?;
    if let (Some(asked), Some(was)) = (
        &requested_scope,
        cursor
            .as_ref()
            .and_then(|cursor| cursor.module_scope.as_ref()),
    ) {
        if !asked.eq_ignore_ascii_case(was) {
            return Err(McpError::InvalidParams(format!(
                "cursor continues a search inside {was:?}, and this call asks for {asked:?}. \
                 Send cursor alone to continue, or omit cursor to start a new search."
            )));
        }
    }
    let module_scope = requested_scope.or_else(|| {
        cursor
            .as_ref()
            .and_then(|cursor| cursor.module_scope.clone())
    });
    if module_scope.is_some() && !held_text {
        return Err(McpError::InvalidParams(
            "scope confines a text search; a stored-field lookup reads every module.".into(),
        ));
    }
    let within = match &module_scope {
        Some(module) => crate::trace_scope::RowScope::from_arguments(
            store,
            &HashMap::from([(
                crate::trace_scope::SCOPE_ARGUMENT.to_string(),
                Value::String(module.clone()),
            )]),
        )
        .map_err(McpError::InvalidParams)?,
        None => None,
    };
    if held_text {
        return held_text_lookup(
            store,
            repository_authority,
            text_of,
            HeldTextQuery {
                within,
                options,
                literal,
                kind,
                scope,
                include_tests,
                offset,
                limit,
                cursor,
            },
        );
    }

    let scoped: Vec<_> = store
        .query_entities(&scope)
        .map_err(McpError::graph)?
        .into_iter()
        .filter(|entity| include_tests || entity.role != EntityRole::Test)
        .collect();
    let scope_languages = crate::edge_coverage::languages_of(&scoped);
    let scope_count = scoped.len();
    let mut verified: Vec<Hit> = scoped
        .into_iter()
        .filter_map(|entity| matched_field(&entity, &literal).map(|field| Hit { entity, field }))
        .collect();
    verified.sort_by_key(|hit| hit.entity.id);
    let snapshot = matching_snapshot(&verified, &literal, kind.as_deref())?;
    if let Some(cursor) = &cursor {
        if cursor.total != verified.len()
            || cursor
                .matching
                .as_ref()
                .is_some_and(|matching| matching != &snapshot)
        {
            return Err(McpError::InvalidParams(format!(
                "the entities that match changed since the page that gave this cursor ({} \
                 then, {} now), so its place among them is gone; omit cursor to start again at \
                 the first match.",
                cursor.total,
                verified.len()
            )));
        }
    }

    let total_matching = verified.len();
    let held = HeldSourceAuthority::new(store, repository_authority);
    let page: Vec<Value> = verified
        .iter()
        .skip(offset)
        .take(limit)
        .map(|hit| build_hit(&held, hit, &literal))
        .collect();
    let served_through = offset + page.len();
    let truncated = served_through < total_matching;

    let mut payload = json!({
        "literal": literal,
        "kind": kind,
        "include_tests": include_tests,
        "hits": page,
        "total_matching": total_matching,
        "truncated": truncated,
        "matching_snapshot": snapshot,
        "order": "entity_id",
        "page_offset": offset,
    });
    if truncated {
        payload["next_cursor"] = json!(LexicalCursor {
            held_text: false,
            options: Options::default(),
            authority: None,
            literal: literal.clone(),
            kind: kind.clone(),
            include_tests,
            offset: served_through,
            total: total_matching,
            matching: Some(snapshot.clone()),
            module_scope: None,
        }
        .encode());
    }
    payload[DISCLOSURE_KEY] = disclosure(total_matching);

    if total_matching == 0 {
        payload[crate::edge_coverage::EDGE_COVERAGE_KEY] =
            crate::edge_coverage::observe_absence_scope(&scope_languages, Some(scope_count));
    }

    let json_text = serde_json::to_string_pretty(&payload).map_err(McpError::Json)?;
    Ok(ToolCallResult::text(json_text))
}

/// One text search, as the request and its cursor settle it.
struct HeldTextQuery {
    /// The module path the rows are confined to, when the caller named one.
    within: Option<crate::trace_scope::RowScope>,
    options: Options,
    literal: String,
    kind: Option<String>,
    scope: EntityFilter,
    include_tests: bool,
    offset: usize,
    limit: usize,
    cursor: Option<LexicalCursor>,
}

/// `locate` with mode `text`: every entity whose own text Kin holds the
/// literal in, exactly, with the first line in it that does, numbered within
/// the entity. See [`super::held_text`].
fn held_text_lookup<G: GraphStore>(
    store: &G,
    repository_authority: Option<&RequestRepositoryAuthority>,
    text_of: Option<super::held_text::TextOf<'_>>,
    query: HeldTextQuery,
) -> Result<ToolCallResult> {
    let HeldTextQuery {
        within,
        literal,
        options,
        kind,
        scope,
        include_tests,
        offset,
        limit,
        cursor,
    } = query;
    let held = HeldSourceAuthority::new(store, repository_authority);
    // Sample authority before searching. The request-local content cache cannot
    // substitute for the current repository/workspace authority.
    let sample = if text_of.is_none() {
        held.workspace_sample().ok()
    } else {
        None
    };
    let tree = match sample {
        Some(sample) => sample.tree.clone(),
        None => store
            .resolved_tree_snapshot()
            .map_err(McpError::graph)?
            .ok_or_else(|| {
                McpError::Context("text search coverage gap: no held workspace tree".into())
            })?,
    };
    let authority = match sample {
        Some(sample) => Some(
            kin_blobs::digest(
                &serde_json::to_vec(&json!([
                    "kin-held-text-authority-v2",
                    sample.source_base_context,
                    sample.label,
                    sample.committed,
                    sample.generation,
                    sample.tree_hash,
                    sample.source_change_id,
                ]))
                .map_err(McpError::Json)?,
            )
            .to_string(),
        ),
        // Fixture readers have an explicit content provider but no durable
        // authority. Never use this branch for an external request.
        None if text_of.is_some() => {
            Some(kin_blobs::digest(&serde_json::to_vec(&tree).map_err(McpError::Json)?).to_string())
        }
        None => None,
    };
    if cursor
        .as_ref()
        .is_some_and(|c| c.authority.is_none() || c.authority != authority)
    {
        return Err(McpError::InvalidParams("text cursor belongs to a different or unavailable workspace authority; omit cursor to start a new search".into()));
    }
    let from_authority = |digest| {
        if let Some(read_fixture) = text_of {
            return Ok(read_fixture(digest));
        }
        match held.tree_content(digest) {
            Ok(bytes) => Ok(Some(bytes)),
            // A budget refusal is a property of this request, not a hole in
            // repository coverage. Discard any rows already accumulated.
            Err(error @ McpError::SourceBudgetExhausted(_)) => Err(error),
            Err(_) => Ok(None),
        }
    };
    let (rows, coverage, budget) =
        super::held_text::search(store, &tree, &literal, options, &from_authority)?;
    let mut left_out_tests = 0usize;
    let mut scoped_out = 0usize;
    let rows: Vec<_> = rows
        .into_iter()
        .filter(|row| {
            if let Some(entity) = row.entity() {
                let test = entity.role == EntityRole::Test;
                if test && !include_tests {
                    left_out_tests += 1;
                }
                (include_tests || !test)
                    && scope
                        .roles
                        .as_ref()
                        .is_none_or(|roles| roles.contains(&entity.role))
                    && scope
                        .kinds
                        .as_ref()
                        .is_none_or(|kinds| kinds.contains(&entity.kind))
            } else {
                // An unparsed artifact has no declaration kind or test role.
                // Explicit parsed-kind scopes cannot invent either fact.
                scope.roles.is_none() && scope.kinds.is_none()
            }
        })
        // Confined to a module path: a code row by its module records, an
        // unparsed row by the module it is placed in. A row whose place is
        // unknown is kept, never dropped unseen.
        .filter(|row| {
            let Some(within) = &within else {
                return true;
            };
            let inside = match (row.entity(), row.unparsed_module()) {
                (Some(entity), _) => within.lists(entity),
                (None, Some(Some(module))) => {
                    let module = module.to_lowercase();
                    let scope = within.module().to_lowercase();
                    module == scope || module.starts_with(&format!("{scope}."))
                }
                _ => true,
            };
            if !inside {
                scoped_out += 1;
            }
            inside
        })
        .map(|row| {
            budget.check()?;
            Ok(row.hit(store))
        })
        .collect::<Result<Vec<_>>>()?;
    budget.check()?;
    let snapshot = kin_blobs::digest(
        &serde_json::to_vec(&json!([
            "kin-held-text-matching-set-v2",
            authority,
            literal,
            kind,
            include_tests,
            options.pattern,
            options.ignore_case,
            coverage,
            within.as_ref().map(|within| within.module()),
            rows,
        ]))
        .map_err(McpError::Json)?,
    )
    .to_string()
    .chars()
    .take(MATCHING_SET_HEX)
    .collect::<String>();
    budget.check()?;
    let total_matching = rows.len();
    if cursor
        .as_ref()
        .is_some_and(|c| c.total != total_matching || c.matching.as_ref() != Some(&snapshot))
    {
        return Err(McpError::InvalidParams("text matches or coverage changed since this cursor; omit cursor to start again at the first match".into()));
    }
    let page: Vec<_> = rows.into_iter().skip(offset).take(limit).collect();
    let served_through = offset + page.len();
    let truncated = served_through < total_matching;
    let mut payload = json!({
        "literal": literal, HELD_TEXT_KEY: true, "kind": kind,
        "pattern": options.pattern, "ignore_case": options.ignore_case,
        "include_tests": include_tests, "hits": page, "total_matching": total_matching,
        "truncated": truncated, "matching_snapshot": snapshot, "text_authority": authority,
        "order": "module_position", "page_offset": offset,
        HELD_TEXT_COVERAGE_KEY: {
            "modules": coverage.modules, "modules_unread": coverage.unread,
            "occurrences_outside_entities": coverage.outside_entities,
            "modules_without_complete_spans": coverage.modules_without_complete_spans,
            "modules_with_unverified_parse": coverage.modules_with_unverified_parse,
            "artifacts_text": coverage.artifacts_text, "artifacts_unread": coverage.artifacts_unread,
            "artifacts_bytes_skipped": coverage.artifacts_bytes_skipped,
            "unaddressable": coverage.unaddressable, "authority_unavailable": authority.is_none(),
            "tests_left_out": left_out_tests,
        },
    });
    if let Some(within) = &within {
        payload[crate::handlers::review::IMPACT_ROW_SCOPE_KEY] =
            json!({ "module": within.module(), "outside": scoped_out });
    }
    if truncated && authority.is_some() {
        payload["next_cursor"] = json!(LexicalCursor {
            held_text: true,
            options,
            authority,
            literal: literal.clone(),
            kind: kind.clone(),
            include_tests,
            offset: served_through,
            total: total_matching,
            matching: Some(snapshot.clone()),
            module_scope: within.as_ref().map(|within| within.module().to_string()),
        }
        .encode());
    }
    payload[DISCLOSURE_KEY] = json!({
        "answered_by": "held_text",
        "matching": if options.pattern { "regex (inline flags apply)" }
            else if options.ignore_case { "literal_unicode_case_insensitive" } else { "exact_bytes_case_sensitive" },
        "complete_for": "bounded matches at held semantic addresses in the reported scope; gaps do not prove absence",
        "detail": "Code matches belong to the innermost complete entity span. Unparsed text matches retain their artifact UUID and namespace, without claiming a semantic binding. Bytes, symbolic links and submodule pointers are excluded. No source body is returned. A text match is not a resolved reference.",
        "limits": {"query_bytes":4096,"compiled_pattern_bytes":1048576,"unit_bytes":8388608,
            "total_bytes":67108864,"units":10000,"matches":100000,"elapsed_ms":2000,"excerpt_chars":160},
    });
    budget.check()?;
    let serialized = serde_json::to_string_pretty(&payload).map_err(McpError::Json)?;
    budget.check()?;
    Ok(ToolCallResult::text(serialized))
}

/// Where a text search says what it read: the parsed modules in the tree,
/// those whose content could not be read back, occurrences no entity spans,
/// and rows left out as test code.
pub const HELD_TEXT_COVERAGE_KEY: &str = "held_text_coverage";

#[cfg(test)]
mod tests {
    use super::*;
    use kin_model::entity::{EntityRole, SemanticFingerprint, SourceSpan, Visibility};
    use kin_model::ids::FilePathId;
    use kin_model::{EntityId, EntityKind};
    use kin_model::{EntityMetadata, FingerprintAlgorithm, Hash256, LanguageId};
    use std::collections::HashSet;

    /// A confined text search's cursor carries its module path, so the next
    /// page stays inside it; a handle without one still decodes as a search
    /// of every module.
    #[test]
    fn a_text_cursor_carries_its_module_scope() {
        let cursor = LexicalCursor {
            held_text: true,
            options: Options::default(),
            authority: Some("a".repeat(64)),
            literal: "SESSION_REFRESH".to_string(),
            kind: None,
            include_tests: true,
            offset: 12,
            total: 30,
            matching: Some("0".repeat(MATCHING_SET_HEX)),
            module_scope: Some("flask.sessions".to_string()),
        };
        let decoded = LexicalCursor::decode(&cursor.encode()).expect("decodes");
        assert_eq!(decoded.module_scope.as_deref(), Some("flask.sessions"));
        assert_eq!(decoded.offset, 12);
        let unscoped = LexicalCursor {
            module_scope: None,
            ..cursor
        };
        let decoded = LexicalCursor::decode(&unscoped.encode()).expect("decodes");
        assert_eq!(decoded.module_scope, None);
    }

    fn fingerprint() -> SemanticFingerprint {
        SemanticFingerprint {
            algorithm: FingerprintAlgorithm::V1TreeSitter,
            ast_hash: Hash256::from_bytes([0; 32]),
            signature_hash: Hash256::from_bytes([0; 32]),
            behavior_hash: Hash256::from_bytes([0; 32]),
            equivalence_hash: Hash256::from_bytes([0; 32]),
            stability_score: 1.0,
        }
    }

    fn entity_with_body(name: &str, body_preview: &str, start_line: u32) -> Entity {
        let mut metadata = EntityMetadata::default();
        metadata.extra.insert(
            EMBEDDING_BODY_PREVIEW_KEY.to_string(),
            Value::String(body_preview.to_string()),
        );
        Entity {
            id: EntityId::new(),
            kind: EntityKind::Function,
            name: name.to_string(),
            language: LanguageId::Go,
            fingerprint: fingerprint(),
            file_origin: Some(FilePathId::new("pkg/cmd/codespace/codespace_selector.go")),
            span: Some(SourceSpan {
                file: FilePathId::new("pkg/cmd/codespace/codespace_selector.go"),
                start_byte: 0,
                end_byte: body_preview.len(),
                start_line,
                start_col: 0,
                end_line: start_line + 10,
                end_col: 0,
            }),
            signature: format!("func {name}()"),
            visibility: Visibility::Public,
            role: EntityRole::Source,
            doc_summary: None,
            metadata,
            lineage_parent: None,
            created_in: None,
            superseded_by: None,
        }
    }

    #[test]
    fn whole_file_module_previews_are_not_lexical_source_excerpts() {
        let file = kin_model::FilePathId::new("value.go");
        let adapter = kin_parser::GoAdapter;
        use kin_parser::LanguageAdapter;
        let source = b"package demo\nfunc Value() int { return 728194; }\n";
        let tree = adapter.parse(source).unwrap();
        let parsed = adapter.extract(&tree, source, &file).unwrap();
        let entities: Vec<_> = parsed
            .entities
            .into_iter()
            .map(|e| e.into_entity_with_source(kin_model::LanguageId::Go, &file, Some(source)))
            .collect();
        let wrapper = entities
            .iter()
            .find(|e| kin_model::is_file_module_surface(e))
            .unwrap();
        assert!(wrapper
            .metadata
            .extra
            .contains_key(EMBEDDING_BODY_PREVIEW_KEY));
        assert!(field_text(wrapper, MatchedField::BodyPreview).is_none());
        assert!(field_text(wrapper, MatchedField::Signature).is_some());
        let function = entities
            .iter()
            .find(|e| e.kind == kin_model::EntityKind::Function)
            .unwrap();
        assert!(field_text(function, MatchedField::BodyPreview)
            .unwrap()
            .contains("728194"));
    }

    #[test]
    fn legacy_declared_module_previews_cannot_bypass_source_span_refusal() {
        let adapter = kin_parser::RustAdapter;
        use kin_parser::LanguageAdapter;
        let file = kin_model::FilePathId::new("src/defaults.rs");
        let source = b"pub mod defaults {}\npub fn outside() { sibling_only(); }\n";
        let parsed = adapter
            .extract(&adapter.parse(source).unwrap(), source, &file)
            .unwrap();
        // The declared `mod defaults`, which is its own entity beside the
        // file's module even where the two share a name.
        let mut module = parsed
            .entities
            .into_iter()
            .map(|e| e.into_entity_with_source(kin_model::LanguageId::Rust, &file, Some(source)))
            .find(|e| {
                e.kind == kin_model::EntityKind::Module && !kin_model::is_file_module_surface(e)
            })
            .unwrap();
        module.span.as_mut().unwrap().end_byte = source.len();
        module.metadata.extra.insert(
            EMBEDDING_BODY_PREVIEW_KEY.into(),
            json!(std::str::from_utf8(source).unwrap()),
        );
        assert!(
            !kin_model::is_file_module_surface(&module),
            "real signature reproduces legacy classifier gap"
        );
        assert_eq!(matched_field(&module, "sibling_only"), None);
        assert!(field_text(&module, MatchedField::BodyPreview).is_none());
        assert_eq!(matched_field(&module, "defaults"), Some(MatchedField::Name));
    }

    #[test]
    fn matched_field_requires_the_whole_literal_not_one_camel_case_part() {
        // The index would retrieve this entity for a query of "fetchCodespaces"
        // on the "fetch" token alone (camelCase splitting), but its body never
        // carries the whole literal, so a literal-lookup tool must not report
        // it as a hit.
        let entity = entity_with_body("Select", "fetchAll(ctx) then codespaces.List(ctx)", 30);
        assert_eq!(matched_field(&entity, "fetchCodespaces"), None);
    }

    #[test]
    fn matched_field_finds_a_real_occurrence_case_insensitively() {
        let entity = entity_with_body("Select", "cs.FETCHCODESPACES(ctx)", 30);
        assert_eq!(
            matched_field(&entity, "fetchCodespaces"),
            Some(MatchedField::BodyPreview)
        );
    }

    #[test]
    fn matched_field_prefers_name_over_body() {
        let entity = entity_with_body("fetchCodespaces", "return fetchCodespaces(ctx)", 30);
        assert_eq!(
            matched_field(&entity, "fetchCodespaces"),
            Some(MatchedField::Name)
        );
    }

    #[test]
    fn declaration_anchored_fields_never_call_resolve_exact_line() {
        // A name/signature match's line comes from the entity's own span, no
        // source read. This is exercised indirectly: entity_with_body's span
        // has no real backing store, so if `hit_line` tried to resolve an
        // exact line for a name match it would panic or misbehave, not
        // silently succeed.
        assert!(MatchedField::Name.anchored_at_declaration());
        assert!(MatchedField::Signature.anchored_at_declaration());
        assert!(MatchedField::DocSummary.anchored_at_declaration());
        assert!(!MatchedField::BodyPreview.anchored_at_declaration());
        assert!(!MatchedField::FileImportContext.anchored_at_declaration());
        assert!(!MatchedField::FileSurfaceContext.anchored_at_declaration());
    }

    #[test]
    fn excerpt_centers_on_the_match_and_marks_truncation() {
        let long_prefix = "x".repeat(200);
        let body = format!("{long_prefix} cs.fetchCodespaces(ctx) {}", "y".repeat(200));
        let entity = entity_with_body("Select", &body, 30);
        let text = excerpt(&entity, MatchedField::BodyPreview, "fetchCodespaces").expect("excerpt");
        assert!(text.contains("fetchCodespaces"));
        assert!(text.starts_with("..."));
        assert!(text.ends_with("..."));
    }

    /// A hit shows the line it matched on: one line, cut around the match only
    /// when it is very long.
    #[test]
    fn a_matching_line_is_one_line_around_the_match() {
        assert_eq!(
            matching_line("        ctx.pop(error)  ", ".pop("),
            "ctx.pop(error)"
        );
        let long = format!("{}ctx.pop(error){}", "a".repeat(300), "b".repeat(300));
        let line = matching_line(&long, ".pop(");
        assert!(line.contains("ctx.pop(error)"), "{line}");
        assert!(line.starts_with("... ") && line.ends_with(" ..."), "{line}");
        assert!(
            line.chars().count() <= MATCHING_LINE_MAX_CHARS + 8,
            "{line}"
        );
        let entity = entity_with_body("Doc", "first line\n   second pop( line", 1);
        let text = excerpt(&entity, MatchedField::BodyPreview, "pop(").unwrap();
        assert!(!text.contains('\n'), "{text}");
    }

    /// The response budget never cuts a hit's matching line: it sheds the
    /// placement fields first, then withholds whole hits behind the cursor.
    #[test]
    fn the_budget_keeps_every_hit_s_matching_line() {
        let entities: Vec<_> = (0..40)
            .map(|i| {
                entity_with_body(
                    &format!("{}Caller{i}", "long_name_".repeat(20)),
                    "x.crowdneedle(y)",
                    i,
                )
            })
            .collect();
        let graph = store_with_entities(&entities);
        let args = HashMap::from([
            ("literal".into(), json!("crowdneedle")),
            ("limit".into(), json!(40)),
        ]);
        let mut payload = payload_of(&handle_lexical_lookup(&args, &graph, None).unwrap());
        let budget = crate::budget::ResponseBudget {
            max_chars: 4_000,
            ..Default::default()
        };
        crate::budget::enforce(&mut payload, TOOL_NAME, &budget).expect("budgeted");
        let hits = payload["hits"].as_array().unwrap();
        assert!(hits.len() < 40, "the fixture must cut hits");
        assert!(payload["next_cursor"].is_string(), "{payload}");
        for hit in hits {
            assert!(
                hit["excerpt"]
                    .as_str()
                    .is_some_and(|line| line.contains("crowdneedle")),
                "{hit}"
            );
            assert!(hit.get("body_elided").is_none(), "{hit}");
        }
    }

    #[test]
    fn find_ascii_insensitive_matches_regardless_of_case() {
        assert_eq!(
            find_ascii_insensitive("cs.FetchCodespaces(ctx)", "fetchcodespaces"),
            Some(3)
        );
        assert_eq!(find_ascii_insensitive("no match here", "zzz"), None);
        assert_eq!(find_ascii_insensitive("short", "much longer needle"), None);
    }

    #[test]
    fn cursor_round_trips_literal_kind_and_offset() {
        let cursor = LexicalCursor {
            held_text: false,
            options: Options::default(),
            authority: None,
            literal: "fetchCodespaces".to_string(),
            kind: Some("function".to_string()),
            include_tests: false,
            offset: 20,
            total: 57,
            matching: Some("a".repeat(MATCHING_SET_HEX)),
            module_scope: None,
        };
        let decoded = LexicalCursor::decode(&cursor.encode()).expect("decodes");
        assert!(
            cursor.encode().starts_with("lit.20.57."),
            "{}",
            cursor.encode()
        );
        assert_eq!(decoded.literal, "fetchCodespaces");
        assert_eq!(decoded.kind, Some("function".to_string()));
        assert!(!decoded.include_tests);
        assert_eq!(decoded.offset, 20);
        assert_eq!(decoded.total, 57);
        assert_eq!(decoded.matching, Some("a".repeat(MATCHING_SET_HEX)));
    }

    /// An agent copies a cursor by hand. The one this build mints is short and
    /// reads back only exactly as minted: whitespace or quotes around it are
    /// not part of it, and a copy that changed any one character is refused,
    /// never read as another cursor.
    #[test]
    fn a_cursor_is_short_and_reads_back_only_as_minted() {
        let token = LexicalCursor {
            held_text: false,
            options: Options::default(),
            authority: None,
            literal: ".pop(".to_string(),
            kind: None,
            include_tests: true,
            offset: 14,
            total: 45,
            matching: Some("0123456789ab".to_string()),
            module_scope: None,
        }
        .encode();
        assert!(token.len() <= 48, "{} characters: {token}", token.len());
        for copy in [
            token.clone(),
            format!("\"{token}\""),
            format!("  `{token}`  "),
        ] {
            assert_eq!(LexicalCursor::decode(&copy).unwrap().offset, 14, "{copy}");
            assert!(is_literal_cursor(&copy), "{copy}");
        }
        for at in 0..token.len() {
            let original = token.as_bytes()[at];
            let changed = if original == b'1' { '2' } else { '1' };
            let mut copy = token.clone();
            copy.replace_range(at..at + 1, &changed.to_string());
            assert!(
                LexicalCursor::decode(&copy).is_none(),
                "one changed character at {at} read as a cursor: {copy}"
            );
        }
        // A version 1 token, minted before this, still continues its lookup.
        let legacy = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            json!({"v": 1, "l": ".pop(", "k": null, "o": 14, "t": 45, "s": "b".repeat(64)})
                .to_string(),
        );
        assert_eq!(LexicalCursor::decode(&legacy).unwrap().offset, 14);
        assert!(is_literal_cursor(&legacy));
    }

    /// A damaged cursor is refused with what to do about it.
    #[test]
    fn a_cursor_that_was_not_copied_exactly_says_so() {
        let graph = store_with_entities(&[
            entity_with_body("Caller0", "crowdneedle", 0),
            entity_with_body("Caller1", "crowdneedle", 1),
        ]);
        let cursor = regression_first_cursor(&graph);
        let damaged = format!("{}0", &cursor[..cursor.len() - 1]);
        let damaged = if damaged == cursor {
            format!("{}1", &cursor[..cursor.len() - 1])
        } else {
            damaged
        };
        let args = HashMap::from([("cursor".into(), json!(damaged))]);
        let error = handle_lexical_lookup(&args, &graph, None)
            .expect_err("a damaged cursor is refused")
            .to_string();
        assert!(error.contains("not copied exactly"), "{error}");
        assert!(error.contains("unchanged"), "{error}");
    }

    #[test]
    fn cursor_rejects_a_token_it_did_not_mint() {
        assert!(LexicalCursor::decode("not-a-real-cursor").is_none());
    }

    #[test]
    fn disclosure_names_a_miss_as_unconfirmed_never_as_absent() {
        let block = disclosure(0);
        let caution = block["caution"].as_str().unwrap();
        assert!(caution.contains("not proof"));
    }

    #[test]
    fn disclosure_on_a_hit_still_denies_being_a_reference_edge() {
        let block = disclosure(3);
        let caution = block["caution"].as_str().unwrap();
        assert!(
            caution.contains("not resolved references") || caution.contains("not mean the entity")
        );
    }

    // ── End-to-end: the handler against a real InMemoryGraph + text index ──

    fn store_with_entities(entities: &[Entity]) -> kin_db::InMemoryGraph {
        let graph = kin_db::InMemoryGraph::new();
        for entity in entities {
            kin_model::graph::EntityStore::upsert_entity(&graph, entity).expect("upsert entity");
        }
        graph.flush_text_index().expect("flush text index");
        graph
    }

    fn payload_of(result: &ToolCallResult) -> Value {
        let crate::types::ContentBlock::Text { text } = &result.content[0];
        serde_json::from_str(text).expect("handler must return valid JSON")
    }

    #[test]
    fn handle_lexical_lookup_reports_a_real_hit_with_file_and_matched_field() {
        let _lock = super::super::tests::ENV_MUTEX
            .get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let hit_entity = entity_with_body("Select", "return cs.fetchCodespaces(ctx)", 30);
        let hit_id = hit_entity.id;
        let miss_entity = entity_with_body("Other", "nothing relevant in here", 5);
        let graph = store_with_entities(&[hit_entity, miss_entity]);

        let mut args: HashMap<String, Value> = HashMap::new();
        args.insert("literal".to_string(), json!("fetchCodespaces"));
        let result = handle_lexical_lookup(&args, &graph, None).expect("handler succeeds");
        let payload = payload_of(&result);

        assert_eq!(payload["total_matching"], json!(1), "{payload}");
        let hits = payload["hits"].as_array().expect("hits array");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["entity_id"], json!(hit_id));
        assert_eq!(hits[0]["matched_field"], json!("body_preview"));
        assert_eq!(
            hits[0]["file"],
            json!("pkg/cmd/codespace/codespace_selector.go")
        );
        assert!(payload[DISCLOSURE_KEY].is_object(), "{payload}");
    }

    #[test]
    fn handle_lexical_lookup_a_name_match_reports_its_declaration_line_exactly() {
        let entity = entity_with_body("fetchCodespaces", "unrelated body text", 82);
        let graph = store_with_entities(&[entity]);

        let mut args: HashMap<String, Value> = HashMap::new();
        args.insert("literal".to_string(), json!("fetchCodespaces"));
        let result = handle_lexical_lookup(&args, &graph, None).expect("handler succeeds");
        let payload = payload_of(&result);

        let hits = payload["hits"].as_array().expect("hits array");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["matched_field"], json!("name"));
        assert_eq!(hits[0]["line_confidence"], json!("declaration"));
        // `entity_with_body` sets `start_line: 82` (0-based, graph convention);
        // the presentation line is 1-based.
        assert_eq!(hits[0]["line"], json!(83));
    }

    #[test]
    fn handle_lexical_lookup_pages_with_a_cursor_and_the_second_page_finishes() {
        let entities: Vec<Entity> = (0..3)
            .map(|i| {
                entity_with_body(
                    &format!("Caller{i}"),
                    "cs.fetchCodespaces(ctx)",
                    10 + i as u32 * 20,
                )
            })
            .collect();
        let graph = store_with_entities(&entities);

        let mut args: HashMap<String, Value> = HashMap::new();
        args.insert("literal".to_string(), json!("fetchCodespaces"));
        args.insert("limit".to_string(), json!(2));
        let first = handle_lexical_lookup(&args, &graph, None).expect("handler succeeds");
        let first_payload = payload_of(&first);
        assert_eq!(first_payload["total_matching"], json!(3), "{first_payload}");
        assert_eq!(first_payload["hits"].as_array().unwrap().len(), 2);
        assert_eq!(first_payload["truncated"], json!(true));
        let cursor = first_payload["next_cursor"]
            .as_str()
            .expect("a truncated page carries a cursor")
            .to_string();

        let mut second_args: HashMap<String, Value> = HashMap::new();
        second_args.insert("cursor".to_string(), json!(cursor));
        let second = handle_lexical_lookup(&second_args, &graph, None).expect("handler succeeds");
        let second_payload = payload_of(&second);
        assert_eq!(second_payload["hits"].as_array().unwrap().len(), 1);
        assert_eq!(second_payload["truncated"], json!(false));
        assert!(second_payload.get("next_cursor").is_none());
    }

    #[test]
    fn handle_lexical_lookup_on_a_miss_says_so_and_attaches_edge_coverage() {
        let entity = entity_with_body("Unrelated", "nothing to see here", 1);
        let graph = store_with_entities(&[entity]);

        let mut args: HashMap<String, Value> = HashMap::new();
        args.insert("literal".to_string(), json!("fetchCodespaces"));
        let result = handle_lexical_lookup(&args, &graph, None).expect("handler succeeds");
        let payload = payload_of(&result);

        assert_eq!(payload["total_matching"], json!(0));
        assert_eq!(payload["hits"], json!([]));
        let caution = payload[DISCLOSURE_KEY]["caution"].as_str().unwrap();
        assert!(caution.contains("not proof"));
        assert!(
            payload
                .get(crate::edge_coverage::EDGE_COVERAGE_KEY)
                .is_some(),
            "{payload}"
        );
    }

    #[test]
    fn lexical_regression_all_501_matches_are_reachable() {
        let entities: Vec<_> = (0..501)
            .map(|i| entity_with_body(&format!("Caller{i}"), "crowdneedle", i))
            .collect();
        let graph = store_with_entities(&entities);
        let mut args = HashMap::from([
            ("literal".into(), json!("crowdneedle")),
            ("limit".into(), json!(100)),
        ]);
        let mut found = HashSet::new();
        for _ in 0..6 {
            let payload = payload_of(&handle_lexical_lookup(&args, &graph, None).unwrap());
            assert_eq!(payload["total_matching"], json!(501));
            for hit in payload["hits"].as_array().unwrap() {
                assert!(
                    found.insert(hit["entity_id"].as_str().unwrap().to_string()),
                    "duplicate hit"
                );
            }
            if let Some(cursor) = payload.get("next_cursor") {
                args = HashMap::from([
                    ("cursor".into(), cursor.clone()),
                    ("limit".into(), json!(100)),
                ]);
            } else {
                assert_eq!(payload["truncated"], json!(false));
                break;
            }
        }
        assert_eq!(found.len(), 501);
    }

    #[test]
    fn lexical_regression_punctuation_literal_is_searchable() {
        let entity = entity_with_body("arrow", "const next = value => value + 1", 0);
        let graph = store_with_entities(std::slice::from_ref(&entity));
        let args = HashMap::from([("literal".into(), json!("=>"))]);
        let payload = payload_of(&handle_lexical_lookup(&args, &graph, None).unwrap());
        assert_eq!(payload["total_matching"], json!(1));
        assert_eq!(payload["hits"][0]["entity_id"], json!(entity.id));
    }

    #[test]
    fn lexical_regression_kind_filter_does_not_starve_after_500_candidates() {
        let mut entities: Vec<_> = (0..501)
            .map(|i| entity_with_body(&format!("crowdneedle{i}"), "crowdneedle", i))
            .collect();
        let mut target = entity_with_body(
            "Target",
            &format!("{} crowdneedle", "filler ".repeat(1000)),
            0,
        );
        target.kind = EntityKind::Class;
        let id = target.id;
        entities.push(target);
        let graph = store_with_entities(&entities);
        let args = HashMap::from([
            ("literal".into(), json!("crowdneedle")),
            ("kind".into(), json!("class")),
        ]);
        let payload = payload_of(&handle_lexical_lookup(&args, &graph, None).unwrap());
        assert_eq!(payload["total_matching"], json!(1));
        assert_eq!(payload["hits"][0]["entity_id"], json!(id));
    }

    fn regression_first_cursor(graph: &kin_db::InMemoryGraph) -> String {
        let args = HashMap::from([
            ("literal".into(), json!("crowdneedle")),
            ("limit".into(), json!(1)),
        ]);
        payload_of(&handle_lexical_lookup(&args, graph, None).unwrap())["next_cursor"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[test]
    fn lexical_regression_cursor_rejects_changed_literal_or_kind() {
        let entities: Vec<_> = (0..3)
            .map(|i| entity_with_body(&format!("Caller{i}"), "crowdneedle otherneedle", i))
            .collect();
        let graph = store_with_entities(&entities);
        let cursor = regression_first_cursor(&graph);
        for (key, value) in [("literal", "otherneedle"), ("kind", "class")] {
            let args =
                HashMap::from([("cursor".into(), json!(cursor)), (key.into(), json!(value))]);
            let error = handle_lexical_lookup(&args, &graph, None)
                .expect_err("a mismatch must refuse")
                .to_string();
            // The refusal names the argument that moved and what continues.
            let named = if key == "literal" {
                "otherneedle"
            } else {
                "class"
            };
            assert!(error.contains(named), "{key}: {error}");
            assert!(error.contains("to continue"), "{key}: {error}");
        }
    }

    /// The page after the first one's cursor, once the matches changed as
    /// `replacement` says, or the refusal.
    fn page_after_change(replacement: &str) -> std::result::Result<Value, String> {
        use kin_model::graph::EntityStore;
        let entities: Vec<_> = (0..3)
            .map(|i| entity_with_body(&format!("Caller{i}"), "crowdneedle", i))
            .collect();
        let graph = store_with_entities(&entities);
        let cursor = regression_first_cursor(&graph);
        if replacement == "edit" {
            let mut changed = entities[0].clone();
            changed.metadata.extra.insert(
                EMBEDDING_BODY_PREVIEW_KEY.into(),
                json!("crowdneedle changed"),
            );
            graph.upsert_entity(&changed).unwrap();
        } else {
            graph.remove_entity(&entities[0].id).unwrap();
            if replacement == "replace" {
                graph
                    .upsert_entity(&entity_with_body("Replacement", "crowdneedle", 0))
                    .unwrap();
            }
        }
        graph.flush_text_index().unwrap();
        let args = HashMap::from([("cursor".into(), json!(cursor))]);
        handle_lexical_lookup(&args, &graph, None)
            .map(|result| payload_of(&result))
            .map_err(|error| error.to_string())
    }

    fn assert_changed_matches_refuse(replacement: &str, counts: &str) {
        let error = page_after_change(replacement)
            .expect_err("a changed set of matches refuses the cursor");
        assert!(error.contains(counts), "{replacement}: {error}");
        assert!(error.contains("omit cursor to start again"), "{error}");
    }

    #[test]
    fn lexical_regression_cursor_rejects_deletion() {
        assert_changed_matches_refuse("delete", "3 then, 2 now");
    }

    #[test]
    fn lexical_regression_cursor_rejects_same_count_replacement() {
        assert_changed_matches_refuse("replace", "3 then, 3 now");
    }

    /// An edit that moves no row, as a background enrichment of a matching
    /// entity does, changes no page, so the cursor keeps working.
    #[test]
    fn lexical_regression_cursor_survives_a_same_id_content_edit() {
        let page = page_after_change("edit").expect("an edit that moves no row keeps the cursor");
        assert_eq!(page["hits"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn lexical_regression_cursor_rejects_insertion() {
        use kin_model::graph::EntityStore;
        let entities: Vec<_> = (0..3)
            .map(|i| entity_with_body(&format!("Caller{i}"), "crowdneedle", i))
            .collect();
        let graph = store_with_entities(&entities);
        let cursor = regression_first_cursor(&graph);
        graph
            .upsert_entity(&entity_with_body("Inserted", "crowdneedle", 0))
            .unwrap();
        let args = HashMap::from([("cursor".into(), json!(cursor))]);
        let error = handle_lexical_lookup(&args, &graph, None)
            .expect_err("an inserted match refuses the cursor")
            .to_string();
        assert!(error.contains("3 then, 4 now"), "{error}");
    }

    fn literal_query_config() -> crate::server::McpServerConfig {
        crate::server::McpServerConfig {
            allowed_tools: Some(crate::tools::name_set(
                crate::tools::agent_query_tool_names(),
            )),
            agent_belt: true,
            plain_replies: true,
            session_authority_mode: crate::SessionAuthorityMode::OfflineFallback,
            ..Default::default()
        }
    }

    async fn public_literal_call(
        graph: &kin_db::InMemoryGraph,
        config: &crate::server::McpServerConfig,
        sessions: &crate::session::SessionRegistry,
        mut arguments: Value,
    ) -> Value {
        let name = if arguments.get("cursor").is_some() {
            "locate"
        } else {
            if let Some(query) = arguments.as_object_mut().unwrap().remove("query") {
                arguments["literal"] = query;
            }
            "lexical_lookup"
        };
        arguments.as_object_mut().unwrap().remove("mode");
        let request = json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": name, "arguments": arguments},
        })
        .to_string();
        let response = crate::server::process_message(&request, graph, config, sessions)
            .await
            .expect("a response");
        let response = crate::server::present_for_client(response, &request, config);
        assert!(response.error.is_none(), "{response:?}");
        response.result.expect("a tool result")
    }

    fn public_literal_payload(result: &Value) -> Value {
        assert_ne!(result["isError"], true, "{result}");
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap()
    }

    /// Follow the delivered cursor-only instruction through the served schema,
    /// public request validation, dispatch, budget and plain presentation.
    /// No page may repeat or omit a match, including after an unrelated edit.
    #[tokio::test]
    async fn a_literal_search_pages_from_the_first_match_to_the_last() {
        use kin_model::graph::EntityStore;
        let entities: Vec<_> = (0..45)
            .map(|i| {
                entity_with_body(
                    &format!("{}Caller{i}", "long_name_".repeat(12)),
                    "ctx.pop(error)",
                    i,
                )
            })
            .collect();
        let graph = store_with_entities(&entities);
        let config = literal_query_config();
        let sessions = crate::session::SessionRegistry::empty_for_test();
        let listing = crate::server::process_message(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
            &graph,
            &config,
            &sessions,
        )
        .await
        .unwrap()
        .result
        .unwrap();
        let schema = &listing["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "locate")
            .unwrap()["inputSchema"];
        // Served bare: every page below says how to continue it, so the
        // listing does not pay for the same sentence on every turn.
        assert_eq!(
            schema["properties"]["cursor"],
            json!({"type": "string"}),
            "{schema}"
        );
        let mut arguments = json!({"query": ".pop(", "max_output_bytes": 4000});
        let mut seen = HashSet::new();
        let mut pages = 0;
        loop {
            pages += 1;
            assert!(pages < 60, "paging never finished");
            assert!(
                arguments.get("cursor").is_none()
                    || crate::schema_check::errors(schema, &arguments).is_empty(),
                "{arguments}"
            );
            let result = public_literal_call(&graph, &config, &sessions, arguments).await;
            let reply = public_literal_payload(&result);
            assert_eq!(
                reply["_kin"]["verdict"],
                json!(crate::plain_reply::COMPLETE)
            );
            assert_eq!(reply["total"], json!(45));
            for hit in reply["hits"].as_array().unwrap() {
                assert!(
                    seen.insert(hit["entity_id"].as_str().unwrap().to_string()),
                    "match repeated on page {pages}"
                );
            }
            let delivered = reply["_kin"]["delivered"].as_str().unwrap_or_default();
            let Some(cursor) = reply["next_cursor"].as_str() else {
                assert!(delivered.contains("the last of them"), "{delivered}");
                break;
            };
            assert!(
                delivered.ends_with("call locate again with only cursor set to next_cursor."),
                "{delivered}"
            );
            if pages == 2 {
                let mut edited = entities[0].clone();
                edited
                    .metadata
                    .extra
                    .insert("enriched".into(), json!({"by": "a language server"}));
                graph.upsert_entity(&edited).unwrap();
            }
            arguments = json!({"cursor": cursor});
        }
        assert_eq!(seen.len(), 45, "every match is reachable");
        assert!(pages > 2, "the fixture must page");
    }

    /// Earlier clients may repeat matching selectors or change the page size.
    /// Keep those accepted, reject mismatches, and make every refusal recover
    /// with the same cursor-only call the successful page teaches.
    #[tokio::test]
    async fn literal_cursor_public_compatibility_and_recovery() {
        let entities: Vec<_> = (0..3)
            .map(|i| entity_with_body(&format!("Caller{i}"), "crowdneedle otherneedle", i))
            .collect();
        let graph = store_with_entities(&entities);
        let config = literal_query_config();
        let sessions = crate::session::SessionRegistry::empty_for_test();
        let first = public_literal_call(
            &graph,
            &config,
            &sessions,
            json!({"literal": "crowdneedle", "kind": "function", "limit": 1}),
        )
        .await;
        let cursor = public_literal_payload(&first)["next_cursor"]
            .as_str()
            .unwrap()
            .to_string();
        let canonical =
            public_literal_call(&graph, &config, &sessions, json!({"cursor": cursor})).await;
        let expected = public_literal_payload(&canonical);
        assert_eq!(expected["hits"].as_array().unwrap().len(), 2);
        for arguments in [
            json!({"cursor": cursor, "literal": "crowdneedle", "kind": "function"}),
            json!({"cursor": format!("\"{cursor}\"")}),
            json!({"cursor": cursor, "limit": 30}),
        ] {
            let result = public_literal_call(&graph, &config, &sessions, arguments).await;
            assert_eq!(public_literal_payload(&result), expected);
        }
        for arguments in [
            json!({"cursor": cursor, "literal": "otherneedle"}),
            json!({"cursor": cursor, "kind": "class"}),
            json!({"cursor": format!("{cursor}0")}),
        ] {
            let refused = public_literal_call(&graph, &config, &sessions, arguments).await;
            assert_eq!(refused["isError"], true, "{refused}");
            assert!(
                refused["content"][0]["text"]
                    .as_str()
                    .unwrap()
                    .contains("cursor alone"),
                "{refused}"
            );
            let recovered =
                public_literal_call(&graph, &config, &sessions, json!({"cursor": cursor})).await;
            assert_eq!(public_literal_payload(&recovered), expected);
        }
    }

    #[test]
    fn lexical_lookup_reads_dirty_fields_and_survives_checkpoint_without_an_index() {
        let _lock = super::super::tests::ENV_MUTEX
            .get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        use kin_model::graph::EntityStore;
        let mut entity = entity_with_body("Caller", "oldliteral", 0);
        let graph = store_with_entities(&[entity.clone()]);
        entity.metadata.extra.insert(
            EMBEDDING_BODY_PREVIEW_KEY.into(),
            json!("newliteral => result"),
        );
        graph.upsert_entity(&entity).unwrap();
        assert!(
            graph.text_search("newliteral", 10).unwrap().is_empty(),
            "fixture keeps the old committed index"
        );
        let args = HashMap::from([("literal".into(), json!("newliteral"))]);
        let current = payload_of(&handle_lexical_lookup(&args, &graph, None).unwrap());
        assert_eq!(current["total_matching"], json!(1));
        assert_eq!(current["hits"][0]["score"], Value::Null);
        assert_eq!(
            current[DISCLOSURE_KEY]["answered_by"],
            json!("stored_graph_fields")
        );
        let bytes = graph.to_snapshot().to_bytes().unwrap();
        let reopened = kin_db::InMemoryGraph::from_snapshot_without_text_index(
            kin_db::GraphSnapshot::from_bytes(&bytes).unwrap(),
        )
        .unwrap();
        assert!(reopened.text_search("newliteral", 10).unwrap().is_empty());
        let restored = payload_of(&handle_lexical_lookup(&args, &reopened, None).unwrap());
        assert_eq!(restored, current);
        let old_args = HashMap::from([("literal".into(), json!("oldliteral"))]);
        assert_eq!(
            payload_of(&handle_lexical_lookup(&old_args, &graph, None).unwrap())["total_matching"],
            json!(0)
        );
    }

    #[test]
    fn lexical_lookup_ignores_derived_index_quarantine() {
        use kin_model::graph::EntityStore;
        let entities = [
            entity_with_body("Caller", "crowdneedle", 0),
            entity_with_body("Callee", "other", 1),
        ];
        let graph = store_with_entities(&entities);
        graph
            .upsert_relations_batch(&[kin_model::Relation {
                id: kin_model::RelationId::new(),
                kind: kin_model::RelationKind::Calls,
                src: kin_model::GraphNodeId::Entity(entities[0].id),
                dst: kin_model::GraphNodeId::Entity(entities[1].id),
                confidence: 1.0,
                origin: kin_model::RelationOrigin::Parsed,
                created_in: None,
                import_source: None,
                evidence: vec![],
            }])
            .unwrap();
        assert!(graph
            .text_search("crowdneedle", 10)
            .unwrap_err()
            .to_string()
            .contains("quarantined"));
        let args = HashMap::from([("literal".into(), json!("crowdneedle"))]);
        let result = payload_of(&handle_lexical_lookup(&args, &graph, None).unwrap());
        assert_eq!(result["total_matching"], json!(1));
    }

    #[test]
    fn lexical_lookup_cursor_is_stable_across_canonical_metadata_and_reopen() {
        use kin_model::graph::EntityStore;
        let mut entities: Vec<_> = (0..3)
            .map(|i| entity_with_body(&format!("Caller{i}"), "crowdneedle", i))
            .collect();
        entities[0]
            .metadata
            .extra
            .insert("nested".into(), json!({"z": 1, "a": 2}));
        let graph = store_with_entities(&entities);
        let cursor = regression_first_cursor(&graph);
        let mut reordered = entities[0].clone();
        let mut keys: Vec<_> = entities[0].metadata.extra.keys().cloned().collect();
        keys.sort_by(|a, b| b.cmp(a));
        reordered.metadata.extra = keys
            .into_iter()
            .map(|key| {
                let value = entities[0].metadata.extra[&key].clone();
                (key, value)
            })
            .collect();
        graph.upsert_entity(&reordered).unwrap();
        let reopened =
            kin_db::InMemoryGraph::from_snapshot_without_text_index(graph.to_snapshot()).unwrap();
        let args = HashMap::from([("cursor".into(), json!(cursor))]);
        let result = payload_of(&handle_lexical_lookup(&args, &reopened, None).unwrap());
        assert_eq!(result["hits"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn lexical_lookup_validates_inputs_and_test_role() {
        let mut test_entity = entity_with_body("TestCase", "crowdneedle", 0);
        test_entity.role = EntityRole::Test;
        let graph = store_with_entities(&[
            test_entity.clone(),
            entity_with_body("Ordinary", "crowdneedle", 1),
        ]);
        for (key, value) in [
            ("literal", json!(false)),
            ("literal", json!(" ")),
            ("literal", json!("x".repeat(MAX_LITERAL_BYTES + 1))),
            ("kind", json!("nonsense")),
            ("kind", json!(true)),
            ("include_tests", json!("false")),
            ("limit", json!(1.5)),
            ("limit", json!(-1)),
            ("cursor", json!(42)),
        ] {
            let mut args = HashMap::from([("literal".into(), json!("crowdneedle"))]);
            args.insert(key.into(), value);
            assert!(
                handle_lexical_lookup(&args, &graph, None).is_err(),
                "{key} must refuse"
            );
        }
        let args = HashMap::from([
            ("literal".into(), json!("crowdneedle")),
            ("kind".into(), json!("test")),
            ("limit".into(), json!(u64::MAX)),
        ]);
        let result = payload_of(&handle_lexical_lookup(&args, &graph, None).unwrap());
        assert_eq!(result["total_matching"], json!(1));
        assert_eq!(result["hits"][0]["entity_id"], json!(test_entity.id));
        let legacy = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(r#"{"l":"crowdneedle","k":null,"o":1,"t":2}"#);
        assert!(LexicalCursor::decode(&legacy).is_none());
    }

    #[test]
    fn literal_test_exclusion_uses_stored_role_and_preserves_every_other_role() {
        let roles = [
            EntityRole::Source,
            EntityRole::Test,
            EntityRole::External,
            EntityRole::Docs,
            EntityRole::Generated,
            EntityRole::Vendored,
        ];
        let entities: Vec<_> = roles
            .into_iter()
            .enumerate()
            .map(|(index, role)| {
                // A test may be a method with an ordinary name, and source may
                // have a test-shaped name. Only its stored role decides scope.
                let mut entity = entity_with_body(
                    if role == EntityRole::Test {
                        "ordinary"
                    } else {
                        "test_helper"
                    },
                    "scopeneedle",
                    index as u32,
                );
                entity.kind = EntityKind::Method;
                entity.role = role;
                entity
            })
            .collect();
        let graph = store_with_entities(&entities);
        for requested in [None, Some(false), Some(true)] {
            let mut args = HashMap::from([("literal".into(), json!("scopeneedle"))]);
            if let Some(include_tests) = requested {
                args.insert("include_tests".into(), json!(include_tests));
            }
            let payload = payload_of(&handle_lexical_lookup(&args, &graph, None).unwrap());
            let expected: HashSet<_> = entities
                .iter()
                .filter(|entity| requested == Some(true) || entity.role != EntityRole::Test)
                .map(|entity| entity.id.to_string())
                .collect();
            let actual: HashSet<_> = payload["hits"]
                .as_array()
                .unwrap()
                .iter()
                .map(|hit| hit["entity_id"].as_str().unwrap().to_string())
                .collect();
            assert_eq!(actual, expected);
            assert_eq!(payload["total_matching"], expected.len());
            assert_eq!(payload["include_tests"], requested.unwrap_or(false));
            args.insert("literal".into(), json!("no_matching_lookup_field"));
            let absent = payload_of(&handle_lexical_lookup(&args, &graph, None).unwrap());
            assert_eq!(absent["total_matching"], 0);
            assert_eq!(
                absent[crate::edge_coverage::EDGE_COVERAGE_KEY]["scope_entities"],
                expected.len()
            );
        }
    }

    #[test]
    fn literal_test_kind_is_explicit_selection_and_refuses_exclusion() {
        // The literal alias searches held source, not the stored preview.
        // Persist real adapter spans and source bytes, then reopen authority.
        let source = "class Ordinary:\n    def ordinary(self):\n        return 'scopeneedle'\n";
        let mut entities = kin_index::IndexPipeline::new()
            .index_file_content_with_tests(
                &kin_model::FilePathId::new("scope.py"),
                source.as_bytes(),
                kin_blobs::digest(source.as_bytes()),
            )
            .unwrap()
            .indexed_file
            .entities;
        let entity = entities
            .iter_mut()
            .find(|entity| entity.name == "Ordinary.ordinary")
            .unwrap();
        assert_eq!(entity.kind, EntityKind::Method);
        entity.role = EntityRole::Test;
        let entity_id = entity.id;
        let fixture = super::super::held_text::tests::DurableTextFixture::with_entities(
            &[("scope.py", source.as_bytes().to_vec())],
            entities,
        );
        let authority = fixture.local();
        let mut name = "locate".to_string();
        let mut args = HashMap::from([
            ("query".into(), json!("scopeneedle")),
            ("mode".into(), json!("literal")),
            ("kind".into(), json!("test")),
        ]);
        crate::verbs::resolve(&mut name, &mut args).unwrap();
        assert_eq!(name, TOOL_NAME);
        assert!(
            !args.contains_key("include_tests"),
            "routing must preserve omission"
        );
        assert_eq!(args[HELD_TEXT_KEY], true);
        for include_tests in [None, Some(true)] {
            if let Some(include_tests) = include_tests {
                args.insert("include_tests".into(), json!(include_tests));
            }
            let payload = payload_of(
                &handle_lexical_lookup(&args, &fixture.graph, Some(&authority)).unwrap(),
            );
            assert_eq!(payload["include_tests"], true);
            assert_eq!(payload["total_matching"], 1);
            assert_eq!(payload["hits"][0]["entity_id"], json!(entity_id));
            assert_eq!(payload["hits"][0]["role"], "test");
            assert_eq!(payload["hits"][0]["line_confidence"], "exact");
            assert!(payload["hits"][0]["excerpt"]
                .as_str()
                .unwrap()
                .contains("scopeneedle"));
        }
        args.insert("include_tests".into(), json!(false));
        let refusal = handle_lexical_lookup(&args, &fixture.graph, Some(&authority))
            .unwrap_err()
            .to_string();
        assert!(refusal.contains("cannot be combined"), "{refusal}");
    }

    #[test]
    fn literal_cursor_inherits_test_scope_and_refuses_changed_scope() {
        // Every role is source so changing the flag leaves the matching set
        // unchanged. A cursor must still refuse the different scope.
        let graph = store_with_entities(&[
            entity_with_body("First", "scopeneedle", 0),
            entity_with_body("Second", "scopeneedle", 1),
        ]);
        for include_tests in [false, true] {
            let args = HashMap::from([
                ("literal".into(), json!("scopeneedle")),
                ("include_tests".into(), json!(include_tests)),
                ("limit".into(), json!(1)),
            ]);
            let first = payload_of(&handle_lexical_lookup(&args, &graph, None).unwrap());
            let mut continuation = HashMap::from([("cursor".into(), first["next_cursor"].clone())]);
            let next = payload_of(&handle_lexical_lookup(&continuation, &graph, None).unwrap());
            assert_eq!(next["include_tests"], include_tests);
            assert_eq!(next["page_offset"], 1);
            assert_ne!(first["hits"][0]["entity_id"], next["hits"][0]["entity_id"]);
            continuation.insert("include_tests".into(), json!(!include_tests));
            let refusal = handle_lexical_lookup(&continuation, &graph, None)
                .unwrap_err()
                .to_string();
            assert!(refusal.contains("include_tests"), "{refusal}");
        }
    }

    #[test]
    fn old_literal_cursors_retain_their_all_role_scope() {
        let mut test_entity = entity_with_body("Ordinary", "scopeneedle", 0);
        test_entity.role = EntityRole::Test;
        let graph =
            store_with_entities(&[test_entity, entity_with_body("Other", "scopeneedle", 1)]);
        let args = HashMap::from([
            ("literal".into(), json!("scopeneedle")),
            ("include_tests".into(), json!(true)),
            ("limit".into(), json!(1)),
        ]);
        let first = payload_of(&handle_lexical_lookup(&args, &graph, None).unwrap());
        let body = format!(
            "lit.1.2.{}..{}",
            first["matching_snapshot"].as_str().unwrap(),
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode("scopeneedle")
        );
        let old_handle = format!("{body}.{}", handle_check(&body));
        let legacy = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            json!({"v":1,"l":"scopeneedle","k":null,"o":1,"t":2,"s":"b".repeat(64)}).to_string(),
        );
        for token in [old_handle, legacy] {
            let mut continuation = HashMap::from([("cursor".into(), json!(token))]);
            let next = payload_of(&handle_lexical_lookup(&continuation, &graph, None).unwrap());
            assert_eq!(next["include_tests"], true);
            assert_eq!(next["total_matching"], 2);
            assert_eq!(next["hits"].as_array().unwrap().len(), 1);
            assert_ne!(first["hits"][0]["entity_id"], next["hits"][0]["entity_id"]);
            continuation.insert("include_tests".into(), json!(false));
            assert!(handle_lexical_lookup(&continuation, &graph, None).is_err());
        }
    }

    #[test]
    fn lexical_lookup_budgeted_pages_keep_every_match_reachable() {
        for (requested_limit, include_tests) in [(20, false), (100, false), (20, true)] {
            let mut entities: Vec<_> = (0..30)
                .map(|i| {
                    entity_with_body(
                        &format!("{}Caller{i}", "long_name_".repeat(40)),
                        "crowdneedle",
                        i,
                    )
                })
                .collect();
            let mut test_entity = entity_with_body("Ordinary", "crowdneedle", 31);
            test_entity.role = EntityRole::Test;
            let test_id = test_entity.id.to_string();
            entities.push(test_entity);
            let graph = store_with_entities(&entities);
            let mut args = HashMap::from([
                ("literal".into(), json!("crowdneedle")),
                ("limit".into(), json!(requested_limit)),
                ("include_tests".into(), json!(include_tests)),
            ]);
            let budget = crate::budget::ResponseBudget {
                max_chars: 4_000,
                ..Default::default()
            };
            let mut found = HashSet::new();
            let mut saw_cut = false;
            for _ in 0..entities.len() {
                let mut payload = payload_of(&handle_lexical_lookup(&args, &graph, None).unwrap());
                assert_eq!(payload["include_tests"], include_tests);
                let before = payload["hits"].as_array().unwrap().len();
                crate::budget::enforce(&mut payload, TOOL_NAME, &budget).expect("budgeted");
                let hits = payload["hits"].as_array().unwrap();
                saw_cut |= hits.len() < before;
                for hit in hits {
                    assert!(
                        found.insert(hit["entity_id"].as_str().unwrap().to_string()),
                        "duplicate match"
                    );
                }
                if let Some(cursor) = payload.get("next_cursor").and_then(Value::as_str) {
                    args = HashMap::from([
                        ("cursor".into(), json!(cursor)),
                        ("limit".into(), json!(requested_limit)),
                    ]);
                } else {
                    break;
                }
            }
            assert!(saw_cut, "fixture must force a row cut");
            assert_eq!(
                found.len(),
                if include_tests { 31 } else { 30 },
                "budget must retain first and final page continuation"
            );
            assert_eq!(found.contains(&test_id), include_tests);
        }
    }

    #[test]
    fn handle_lexical_lookup_requires_literal_or_cursor() {
        let args: HashMap<String, Value> = HashMap::new();
        let graph = kin_db::InMemoryGraph::new();
        let error = handle_lexical_lookup(&args, &graph, None)
            .expect_err("missing literal and cursor must refuse");
        assert!(format!("{error}").contains("literal"));
    }
}
