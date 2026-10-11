// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The exact source a caller read, carried unchanged into a guarded body edit.
//! This is an optimistic concurrency expectation, not an authorization token.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceBaseSchema {
    #[serde(rename = "kin.entity.source_base.v1")]
    V1,
}

/// One local workspace instant. V1 conservatively refuses any advance of the
/// workspace head or tree, including unrelated edits. A generation that
/// advanced over the same head and tree, such as language-server enrichment
/// publishing what it proved, changed no source bytes and is not a conflict.
/// A generation alone cannot identify a repository or distinguish branches
/// with identical source bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceBaseContext {
    pub repository_id: String,
    pub workspace_id: String,
    pub workspace_generation: u64,
    pub workspace_head_hash: String,
    pub workspace_tree_hash: String,
}

impl SourceBaseContext {
    pub fn from_workspace(workspace: &kin_model::WorkspaceState) -> Result<Self, String> {
        let head = serde_json::to_vec(&workspace.head).map_err(|error| error.to_string())?;
        Ok(Self {
            repository_id: workspace.repository_id.to_string(),
            workspace_id: workspace.workspace_id.to_string(),
            workspace_generation: workspace.generation,
            workspace_head_hash: kin_blobs::digest(&head).to_string(),
            workspace_tree_hash: workspace.tree_hash.to_string(),
        })
    }

    /// Well-formed identity: a repository id, a workspace UUID and two
    /// lowercase BLAKE3 hex digests. Freshness is the daemon's check.
    pub fn validate(&self) -> Result<(), String> {
        kin_model::RepositoryId::new(self.repository_id.clone())
            .map_err(|error| error.to_string())?;
        uuid::Uuid::parse_str(&self.workspace_id).map_err(|error| error.to_string())?;
        for hash in [&self.workspace_head_hash, &self.workspace_tree_hash] {
            if !is_lower_hex_digest(hash) {
                return Err("source base hashes must be 64 lowercase hex characters".into());
            }
        }
        Ok(())
    }
}

fn is_lower_hex_digest(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Closed, versioned identity of the complete entity body served by Kin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntitySourceBase {
    pub schema: SourceBaseSchema,
    pub context: SourceBaseContext,
    pub entity_id: kin_model::EntityId,
    pub artifact_id: kin_model::ArtifactId,
    pub source_blob_hash: String,
    pub start_byte: usize,
    pub end_byte: usize,
    pub body_hash: String,
}

/// An exact replacement within the original entity body. Anchors are literal
/// UTF-8 text, not regexes, offsets, or instructions to search working files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntityTextEdit {
    pub old_text: String,
    pub new_text: String,
}

/// Compact guarded source editing. Every anchor addresses the same original
/// body; replacements never become input to later anchors in the batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntitySourcePatch {
    pub source_base: EntitySourceBase,
    pub edits: Vec<EntityTextEdit>,
}

impl EntitySourcePatch {
    pub fn validate(&self) -> Result<(), String> {
        self.source_base.validate()?;
        if self.edits.is_empty() {
            return Err("an entity patch requires at least one exact text edit".into());
        }
        for (index, edit) in self.edits.iter().enumerate() {
            // An empty anchor is the one edit that gives a module with no
            // code its first code ([`crate::module_code`]). It names no place
            // in a body, so nothing else may carry one.
            if edit.old_text.is_empty() && !self.is_first_code() {
                return Err(format!(
                    "patch edit #{index}: old_text must be nonempty, since an empty old_text \
                     is only the first code of a module that holds none, so anchor it on text \
                     copied from the source read"
                ));
            }
            if edit.old_text == edit.new_text {
                return Err(format!("patch edit #{index}: replacement is a no-op"));
            }
        }
        Ok(())
    }

    /// Whether this patch is the first code of a module that holds none: one
    /// edit with an empty anchor, against a base that guards an empty text.
    /// Only such a module's read returns a base of no length, since no entity
    /// body is empty and a module's base is as long as its served code. So an
    /// empty anchor for anything else is refused here, before it is staged,
    /// with no graph read.
    pub fn is_first_code(&self) -> bool {
        self.source_base.start_byte == self.source_base.end_byte
            && matches!(self.edits.as_slice(), [edit] if edit.old_text.is_empty())
    }

    /// [`Self::apply_to_exact_body_at`] for a body whose place carries no
    /// indentation, named only as "the entity".
    pub fn apply_to_exact_body(&self, body: &str) -> Result<String, String> {
        self.apply_to_exact_body_at(body, "", "the entity")
    }

    /// Apply only to the exact body bound by the source base. The daemon also
    /// checks the repository/workspace/artifact binding under authority locks.
    ///
    /// `place` is the indentation the entity `name` stands at in its source.
    /// A body is served from the entity's first character, so its first line
    /// has no indentation and every later line has the source's. An anchor
    /// that is not in the body as written is read once more with its lines
    /// moved by `place`, the two ways a caller that saw the same code at
    /// another indentation writes it ([`Moved`]); when exactly one place fits,
    /// the replacement is moved the same way and lands there. An anchor that
    /// fits nowhere is refused with the code nearest it, as the body holds
    /// it, so the next one can be copied from that.
    pub fn apply_to_exact_body_at(
        &self,
        body: &str,
        place: &str,
        name: &str,
    ) -> Result<String, String> {
        self.validate()?;
        if self.is_first_code() {
            return Err("patch edit #0: old_text must be nonempty".into());
        }
        if body.len() != self.source_base.end_byte - self.source_base.start_byte
            || kin_blobs::digest(body.as_bytes()).to_string() != self.source_base.body_hash
        {
            return Err("entity patch body differs from its source_base".into());
        }
        let mut replacements = Vec::with_capacity(self.edits.len());
        for (index, edit) in self.edits.iter().enumerate() {
            let (start, end, new_text) = match occurrences(body, &edit.old_text) {
                (Some(start), 1) => (start, start + edit.old_text.len(), edit.new_text.clone()),
                (Some(_), count) => {
                    return Err(format!(
                        "patch edit #{index}: old_text is ambiguous in {name}: it occurs {count} \
                         times there, so extend it with the text beside the one you mean until \
                         it occurs once"
                    ))
                }
                (None, _) => moved_anchor(body, place, edit).map_err(|fit| match fit {
                    Unfit::Several => format!(
                        "patch edit #{index}: old_text is ambiguous in {name}: it is not there \
                         as written, and with its lines moved to the indentation {name} stands \
                         at it fits more than one place, so copy the lines you mean exactly as \
                         source returned them"
                    ),
                    Unfit::ReplacementApart => format!(
                        "patch edit #{index}: old_text fits {name} only with its lines moved to \
                         the indentation {name} stands at, and new_text cannot move with it \
                         because one of its lines is indented less than that, so write old_text \
                         and new_text indented exactly as source returned the code"
                    ),
                    Unfit::Nowhere => absent(index, name, body, &edit.old_text),
                })?,
            };
            replacements.push((start, end, index, new_text));
        }
        replacements.sort_by_key(|(start, _, _, _)| *start);
        for pair in replacements.windows(2) {
            if pair[0].1 > pair[1].0 {
                return Err(format!(
                    "patch edits #{} and #{} overlap in the original entity body",
                    pair[0].2, pair[1].2
                ));
            }
        }
        let mut patched = body.to_owned();
        for (start, end, _, replacement) in replacements.into_iter().rev() {
            patched.replace_range(start..end, &replacement);
        }
        if patched == body {
            return Err("entity patch is a no-op".into());
        }
        Ok(patched)
    }
}

/// Where `anchor` first occurs in `body`, and how many times it occurs,
/// counted from one character after each match and not after its end:
/// overlapping occurrences ("aa" in "aaa") are ambiguous too.
fn occurrences(body: &str, anchor: &str) -> (Option<usize>, usize) {
    let Some(first) = body.find(anchor) else {
        return (None, 0);
    };
    let step = anchor.chars().next().map_or(1, char::len_utf8);
    let mut count = 1;
    let mut from = first + step;
    while let Some(at) = body[from..].find(anchor) {
        count += 1;
        from += at + step;
    }
    (Some(first), count)
}

/// One way an anchor's lines are moved to where the entity's code stands.
///
/// A caller that read a method at the margin (a display that takes the
/// method's own indentation off every line) writes the lines after the first
/// too shallow by that indentation. One that read it in its source, or put
/// the indentation back twice, writes lines too deep by it. In both the text
/// is the entity's own, line for line, and only its distance from the margin
/// differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Moved {
    /// The lines after the first are too shallow: the indentation is added
    /// to each. The first line begins wherever in its line the anchor does.
    LaterIn,
    /// Every line is too deep: the indentation is taken off each.
    AllOut,
    /// The lines after the first are too deep.
    LaterOut,
    /// Only the first line is too deep: the text was copied with the
    /// indentation that stands ahead of the entity in its source.
    FirstOut,
}

impl Moved {
    const EVERY: [Moved; 4] = [
        Moved::LaterIn,
        Moved::AllOut,
        Moved::LaterOut,
        Moved::FirstOut,
    ];

    /// `text` moved this way by `place`. `None` when a line that must give
    /// up the indentation does not carry it, or, for an `anchor`, when the
    /// move leaves it as it was written, which has been tried.
    fn of(self, text: &str, place: &str, anchor: bool) -> Option<String> {
        let mut out = String::with_capacity(text.len() + place.len() * 4);
        for (at, line) in text.split('\n').enumerate() {
            if at > 0 {
                out.push('\n');
            }
            let blank = line.trim().is_empty();
            let first = at == 0;
            match self {
                Moved::LaterIn if !first && !blank => {
                    out.push_str(place);
                    out.push_str(line);
                }
                Moved::AllOut | Moved::LaterOut | Moved::FirstOut
                    if !blank
                        && match self {
                            Moved::AllOut => true,
                            Moved::LaterOut => !first,
                            _ => first,
                        } =>
                {
                    match line.strip_prefix(place) {
                        Some(rest) => out.push_str(rest),
                        // A replacement's first line continues the line the
                        // anchor began in, and may begin anywhere.
                        None if first && !anchor => out.push_str(line),
                        None => return None,
                    }
                }
                _ => out.push_str(line),
            }
        }
        (!anchor || out != text).then_some(out)
    }
}

/// Why an anchor moved to the entity's indentation still names no one place
/// to put its replacement.
enum Unfit {
    /// It fits nowhere.
    Nowhere,
    /// It fits more than one place, or two ways to different effect.
    Several,
    /// It fits one place, and the replacement cannot be moved with it: a
    /// line of the replacement that must give up the indentation lacks it.
    ReplacementApart,
}

/// The one place `edit`'s anchor fits in `body` once its lines are moved by
/// `place`, and the replacement moved the same way: `(start, end, new_text)`.
///
/// Every way of moving is tried, and the ones that fit must agree on what
/// the body becomes. An anchor one way fits twice, or two ways fit to
/// different effect, names no one place.
fn moved_anchor(
    body: &str,
    place: &str,
    edit: &EntityTextEdit,
) -> Result<(usize, usize, String), Unfit> {
    if place.is_empty() {
        return Err(Unfit::Nowhere);
    }
    let mut fit: Option<(usize, usize, String)> = None;
    let mut apart = false;
    for how in Moved::EVERY {
        let Some(anchor) = how.of(&edit.old_text, place, true) else {
            continue;
        };
        let (Some(start), count) = occurrences(body, &anchor) else {
            continue;
        };
        if count > 1 {
            return Err(Unfit::Several);
        }
        let Some(new_text) = how.of(&edit.new_text, place, false) else {
            apart = true;
            continue;
        };
        let end = start + anchor.len();
        let becomes = |(start, end, new): &(usize, usize, String)| {
            format!("{}{new}{}", &body[..*start], &body[*end..])
        };
        let found = (start, end, new_text);
        match &fit {
            Some(held) if becomes(held) != becomes(&found) => return Err(Unfit::Several),
            Some(_) => {}
            None => fit = Some(found),
        }
    }
    fit.ok_or(if apart {
        Unfit::ReplacementApart
    } else {
        Unfit::Nowhere
    })
}

/// The most lines of an entity's code a refusal quotes.
const NEAREST_LINES_MAX: usize = 8;
/// The most bytes of an entity's code a refusal quotes.
const NEAREST_BYTES_MAX: usize = 800;

/// The lines of `body` nearest `anchor`, exactly as the body holds them: from
/// the line most like the anchor's first line of code, as many lines as the
/// anchor has. `None` when no line of the body is like it.
fn nearest_code<'a>(body: &'a str, anchor: &str) -> Option<&'a str> {
    let wanted = anchor
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    let lines: Vec<&str> = body.split('\n').collect();
    // How alike a line of the body and the wanted line are: the same text,
    // one holding the other, or a long shared start.
    let likeness = |line: &str| -> usize {
        let line = line.trim();
        if line.is_empty() {
            return 0;
        }
        if line == wanted {
            return usize::MAX;
        }
        if line.contains(wanted) || wanted.contains(line) && line.len() >= 8 {
            return usize::MAX / 2 + line.len().min(wanted.len());
        }
        let shared = line
            .bytes()
            .zip(wanted.bytes())
            .take_while(|(held, sent)| held == sent)
            .count();
        if shared >= 8 || shared * 2 >= wanted.len().max(2) {
            shared
        } else {
            0
        }
    };
    let (best, score) = lines
        .iter()
        .enumerate()
        .map(|(at, line)| (at, likeness(line)))
        // The first of equally alike lines.
        .rev()
        .max_by_key(|(_, score)| *score)?;
    if score == 0 {
        return None;
    }
    let count = anchor.lines().count().clamp(1, NEAREST_LINES_MAX);
    let start: usize = lines[..best].iter().map(|line| line.len() + 1).sum();
    let mut end = start;
    for line in lines.iter().skip(best).take(count) {
        if end > start && end + line.len() - start > NEAREST_BYTES_MAX {
            break;
        }
        end += line.len() + 1;
    }
    let end = end.saturating_sub(1).min(body.len());
    body.get(start..end).filter(|code| !code.trim().is_empty())
}

/// The refusal for an anchor that fits nowhere in `name`: that it is absent,
/// and the code nearest it as the body holds it, when the agent surface
/// would carry that code.
fn absent(index: usize, name: &str, body: &str, anchor: &str) -> String {
    let lead = format!("patch edit #{index}: old_text is absent from {name} as its code stands");
    let reread = format!("read {name} again with source and copy old_text from what it returns");
    if let Some(code) = nearest_code(body, anchor) {
        let said = format!(
            "{lead}. The code nearest it there reads:\n{code}\nCopy old_text from those lines \
             exactly, with their indentation, or {reread}"
        );
        if crate::semantic_surface::is_semantic(&said) {
            return said;
        }
    }
    format!("{lead}, so {reread}")
}

pub fn entity_source_patch_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "source_base": crate::source_ticket::write_base_schema(),
            "edits": {
                "type": "array", "minItems": 1,
                "description": "Exact unique nonoverlapping anchors in the original entity body. All edits use that same original body, not earlier replacements.",
                "items": {
                    "type": "object",
                    "properties": {
                        "old_text": { "type": "string", "description": "Empty only as the one edit that gives a module with no code its first code." },
                        "new_text": { "type": "string" }
                    },
                    "required": ["old_text", "new_text"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["source_base", "edits"],
        "additionalProperties": false
    })
}

impl EntitySourceBase {
    pub fn from_exact_body(
        context: SourceBaseContext,
        entity: &kin_model::Entity,
        artifact_id: kin_model::ArtifactId,
        source_blob_hash: kin_model::Hash256,
        body: &str,
    ) -> Result<Self, String> {
        kin_model::require_targeted_source(entity)?;
        let span = entity
            .span
            .as_ref()
            .ok_or("source base requires an exact span")?;
        let base = Self {
            schema: SourceBaseSchema::V1,
            context,
            entity_id: entity.id,
            artifact_id,
            source_blob_hash: source_blob_hash.to_string(),
            start_byte: span.start_byte,
            end_byte: span.end_byte,
            body_hash: kin_blobs::digest(body.as_bytes()).to_string(),
        };
        base.validate()?;
        if body.len() != span.end_byte - span.start_byte {
            return Err("source body length differs from its exact span".into());
        }
        Ok(base)
    }

    pub fn validate(&self) -> Result<(), String> {
        self.context.validate()?;
        for hash in [&self.source_blob_hash, &self.body_hash] {
            if !is_lower_hex_digest(hash) {
                return Err("source base hashes must be 64 lowercase hex characters".into());
            }
        }
        // An empty span is a module that holds no code yet. No entity body
        // is empty, and a base is checked against its entity's own span.
        if self.start_byte > self.end_byte {
            return Err("source base must name an exact byte span".into());
        }
        Ok(())
    }
}

pub(crate) fn source_base_for_read<G: kin_model::GraphStore>(
    held: &crate::handlers::common::HeldSourceAuthority<'_, G>,
    entity: &kin_model::Entity,
    source: &crate::handlers::common::ExactEntitySource,
) -> crate::error::Result<Option<EntitySourceBase>> {
    use crate::handlers::common::SpanCoherence;
    if source.span_coherence == SpanCoherence::Unverified {
        return Ok(None);
    }
    let Some(context) = held.workspace_sample()?.source_base_context.clone() else {
        // Hosted/historical source has no local workspace to mutate.
        return Ok(None);
    };
    let kin_model::TreeEntry::Blob { hash, .. } = source.entry else {
        return Ok(None);
    };
    let base =
        EntitySourceBase::from_exact_body(context, entity, source.artifact_id, hash, &source.body)
            .map_err(crate::error::McpError::Context)?;
    Ok(Some(base))
}

/// Why a current source read issued no `source_base`: the entity's span carries no
/// recorded source digest, so the bytes it cuts cannot be proven to be the entity's.
///
/// A whole-entity replacement requires a source base, so this names the state as a
/// stable code a caller can tell apart from `source_base_required`, which means only
/// that a base was omitted. Entities derived before source digests were recorded carry
/// none until the repository is re-derived. An older repository may need
/// `kin upgrade`; the current read must actually issue a base before a guarded edit.
pub const SOURCE_BASE_UNAVAILABLE_UNVERIFIED: &str = "source_base_unavailable: this entity's \
span is unverified against its file's bytes, because the graph recorded no source digest for \
it, so no source_base was issued and Kin cannot guard a change to it. Stop and report this \
gap rather than sending an unguarded change. An older repository may need `kin upgrade`; \
reread afterward and proceed only if Kin issues a source_base. If the repository is already \
current or the base remains unavailable, report the unresolved source verification gap.";

/// Machine-readable refusal, emitted only after the unchanged staged work is durable.
pub fn source_base_conflict(transaction_id: &str, reason: &str) -> String {
    serde_json::json!({
        "schema": "kin.entity.source_base_conflict.v1",
        "code": "source_base_conflict",
        "transaction_id": transaction_id,
        "applied": false,
        "staged_operations_retained": true,
        "reason": reason,
        "remedy": "One fresh get_entity_source read and a resend is the fix: read the current entity, compare it with your retained draft, and submit the resolved work with the source_base that read returned, unchanged, in a new transaction (and a new request_id for a keyed mutation). The stale transaction remains available until you explicitly abort it."
    }).to_string()
}

pub fn is_source_base_conflict(text: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(text).is_ok_and(|value| {
        value["schema"] == "kin.entity.source_base_conflict.v1"
            && value["code"] == "source_base_conflict"
            && value["applied"] == false
            && value["staged_operations_retained"] == true
    })
}

/// The schema used by MCP discovery and mirrored by boundary-contracts.
/// Keep a crate-local copy so published kin-mcp sources are self-contained.
pub fn source_base_schema() -> serde_json::Value {
    serde_json::from_str(include_str!("source_base.schema.json"))
        .expect("the checked-in entity source base schema is valid JSON")
}

#[cfg(test)]
mod patch_tests {
    use super::*;

    fn patch(body: &str, edits: &[(&str, &str)]) -> EntitySourcePatch {
        EntitySourcePatch {
            source_base: EntitySourceBase {
                schema: SourceBaseSchema::V1,
                context: SourceBaseContext {
                    repository_id: "patch-test".into(),
                    workspace_id: uuid::Uuid::new_v4().to_string(),
                    workspace_generation: 1,
                    workspace_head_hash: "a".repeat(64),
                    workspace_tree_hash: "b".repeat(64),
                },
                entity_id: kin_model::EntityId::new(),
                artifact_id: kin_model::ArtifactId::new(),
                source_blob_hash: "c".repeat(64),
                start_byte: 0,
                end_byte: body.len(),
                body_hash: kin_blobs::digest(body.as_bytes()).to_string(),
            },
            edits: edits
                .iter()
                .map(|(old, new)| EntityTextEdit {
                    old_text: (*old).into(),
                    new_text: (*new).into(),
                })
                .collect(),
        }
    }

    const METHOD: &str =
        "def area(self):\n        if self.radius < 0:\n            return 0\n        return area(self.radius)";

    fn apply(edits: &[(&str, &str)]) -> Result<String, String> {
        patch(METHOD, edits).apply_to_exact_body_at(METHOD, "    ", "Circle.area")
    }

    /// An anchor written at another indentation than the body's is read at
    /// the entity's, and its replacement lands there with it.
    #[test]
    fn an_anchor_at_another_indentation_fits_where_its_lines_stand() {
        let wanted =
            "def area(self):\n        if self.radius <= 0:\n            return 1\n        return area(self.radius)";
        for (how, old, new) in [
            (
                "as the body holds it",
                "        if self.radius < 0:\n            return 0",
                "        if self.radius <= 0:\n            return 1",
            ),
            (
                "at the margin, from the line's start",
                "    if self.radius < 0:\n        return 0",
                "    if self.radius <= 0:\n        return 1",
            ),
            (
                "at the margin, from the first token",
                "if self.radius < 0:\n        return 0",
                "if self.radius <= 0:\n        return 1",
            ),
            (
                "one indentation too deep",
                "            if self.radius < 0:\n                return 0",
                "            if self.radius <= 0:\n                return 1",
            ),
            (
                "too deep after the first token",
                "if self.radius < 0:\n                return 0",
                "if self.radius <= 0:\n                return 1",
            ),
            (
                "at the margin, from the entity's first line",
                "def area(self):\n    if self.radius < 0:\n        return 0",
                "def area(self):\n    if self.radius <= 0:\n        return 1",
            ),
            (
                "copied with the indentation ahead of the entity",
                "    def area(self):\n        if self.radius < 0:\n            return 0",
                "    def area(self):\n        if self.radius <= 0:\n            return 1",
            ),
        ] {
            assert_eq!(apply(&[(old, new)]).as_deref(), Ok(wanted), "{how}");
        }
        // A replacement of more lines than the anchor keeps each line's own
        // depth under the moved block.
        assert_eq!(
            apply(&[(
                "    if self.radius < 0:\n        return 0",
                "    if self.radius < 0:\n        value = 0\n        if value:\n            pass\n\n        return value"
            )])
            .as_deref(),
            Ok("def area(self):\n        if self.radius < 0:\n            value = 0\n            if value:\n                pass\n\n            return value\n        return area(self.radius)")
        );
    }

    #[test]
    fn an_anchor_that_fits_twice_once_moved_is_refused_as_ambiguous() {
        let body = "def twice(self):\n        if self.radius:\n            pass\n        if self.radius:\n            pass\n        return 2";
        assert_eq!(
            patch(body, &[("    if self.radius:\n        pass", "    pass")])
                .apply_to_exact_body_at(body, "    ", "Circle.twice")
                .unwrap_err(),
            "patch edit #0: old_text is ambiguous in Circle.twice: it is not there as written, \
             and with its lines moved to the indentation Circle.twice stands at it fits more \
             than one place, so copy the lines you mean exactly as source returned them"
        );
        // Written as the body holds it, it is ambiguous the way it always was.
        assert_eq!(
            patch(body, &[("if self.radius:\n            pass", "pass")])
                .apply_to_exact_body_at(body, "    ", "Circle.twice")
                .unwrap_err(),
            "patch edit #0: old_text is ambiguous in Circle.twice: it occurs 2 times there, so \
             extend it with the text beside the one you mean until it occurs once"
        );
    }

    /// An anchor that fits nowhere is answered with the code nearest it,
    /// exactly as the body holds it.
    #[test]
    fn an_absent_anchor_is_answered_with_the_nearest_code_as_the_body_holds_it() {
        assert_eq!(
            apply(&[("    if self.radius < 1:\n        return 0", "    pass")]).unwrap_err(),
            "patch edit #0: old_text is absent from Circle.area as its code stands. The code \
             nearest it there reads:\n        if self.radius < 0:\n            return 0\nCopy \
             old_text from those lines exactly, with their indentation, or read Circle.area \
             again with source and copy old_text from what it returns"
        );
        // Nothing in the body is like it.
        assert_eq!(
            apply(&[("raise NotImplementedError", "pass")]).unwrap_err(),
            "patch edit #0: old_text is absent from Circle.area as its code stands, so read \
             Circle.area again with source and copy old_text from what it returns"
        );
        // The anchor fits once moved, and the replacement cannot move with it.
        assert_eq!(
            apply(&[(
                "            if self.radius < 0:\n                return 0",
                "            if self.radius < 0:\n  return 0"
            )])
            .unwrap_err(),
            "patch edit #0: old_text fits Circle.area only with its lines moved to the \
             indentation Circle.area stands at, and new_text cannot move with it because one \
             of its lines is indented less than that, so write old_text and new_text indented \
             exactly as source returned the code"
        );
        // An entity at the margin has no indentation to move by.
        let body = "def area(radius):\n    return radius";
        assert!(
            patch(body, &[("        return radius", "        return 2")])
                .apply_to_exact_body_at(body, "", "area")
                .unwrap_err()
                .starts_with("patch edit #0: old_text is absent from area as its code stands.")
        );
    }

    #[test]
    fn entity_patch_applies_original_body_anchors_and_utf8_exactly() {
        let body = "café a=1 b=2 suffix";
        let patch = patch(
            body,
            &[("1", "2"), ("2", "3"), ("café", "😀"), (" suffix", "")],
        );
        assert_eq!(patch.apply_to_exact_body(body).unwrap(), "😀 a=2 b=3");
    }

    #[test]
    fn entity_patch_rejects_missing_ambiguous_and_overlapping_anchors() {
        for (body, edits, reason) in [
            ("abc", vec![("z", "x")], "absent"),
            ("aaa", vec![("aa", "x")], "ambiguous"),
            ("ééé", vec![("éé", "x")], "ambiguous"),
            ("abc", vec![("ab", "x"), ("bc", "y")], "overlap"),
            ("abc", vec![("ab", "x"), ("ab", "y")], "overlap"),
            ("abc", vec![("a", "x"), ("x", "y")], "absent"),
        ] {
            assert!(patch(body, &edits)
                .apply_to_exact_body(body)
                .unwrap_err()
                .contains(reason));
        }
    }

    #[test]
    fn entity_patch_rejects_stale_body_empty_and_noop_edits() {
        assert!(patch("abc", &[("a", "z")])
            .apply_to_exact_body("abd")
            .unwrap_err()
            .contains("source_base"));
        assert!(patch("abc", &[]).validate().is_err());
        // An empty anchor names no place in a body that holds text.
        assert!(patch("abc", &[("", "x")])
            .validate()
            .unwrap_err()
            .contains("old_text must be nonempty"));
        // It is admitted only as the one edit of a patch whose base guards an
        // empty text, which is a module with no code, and never applies to an
        // entity body.
        let first_code = patch("", &[("", "x")]);
        assert!(first_code.is_first_code());
        assert!(first_code.validate().is_ok());
        assert!(first_code
            .apply_to_exact_body("")
            .unwrap_err()
            .contains("old_text must be nonempty"));
        assert!(patch("", &[("", "x"), ("a", "z")])
            .validate()
            .unwrap_err()
            .contains("old_text must be nonempty"));
        assert!(patch("", &[("", "")]).validate().is_err());
        assert!(patch("abc", &[("a", "a")])
            .validate()
            .unwrap_err()
            .contains("no-op"));
        // Individually different, adjacent replacements can still cancel out.
        assert!(patch("abc", &[("a", "ab"), ("bc", "c")])
            .apply_to_exact_body("abc")
            .unwrap_err()
            .contains("no-op"));
    }

    #[test]
    fn entity_patch_decoder_and_staging_fail_closed() {
        let patch = patch("abc", &[("a", "z")]);
        let operation = serde_json::json!({
            "verb":"patch", "target":patch.source_base.entity_id,
            "payload":{"EntitySourcePatch":patch}, "description":"localized edit"
        });
        let decode = |op| crate::session::parse_staged_operations(&serde_json::json!([op]));
        let valid = decode(operation.clone()).unwrap();
        crate::session::validate_staged_operations(&valid).unwrap();
        assert!(
            crate::session::carries_source_body(&valid[0]),
            "offline commits must refuse patches as source writes"
        );
        for pointer in [
            "/payload/EntitySourcePatch/unknown",
            "/payload/EntitySourcePatch/edits/0/unknown",
            "/payload/EntitySourcePatch/source_base/unknown",
        ] {
            let mut bad = operation.clone();
            let (parent, key) = pointer.rsplit_once('/').unwrap();
            bad.pointer_mut(parent)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .insert(key.into(), serde_json::json!(true));
            assert!(
                decode(bad).is_err(),
                "unknown field {pointer} must not disappear"
            );
        }
        for (field, value) in [
            ("verb", serde_json::json!("update")),
            ("target", serde_json::json!("abc")),
            ("body", serde_json::json!("")),
            ("destination", serde_json::json!("x.rs")),
        ] {
            let mut bad = operation.clone();
            bad[field] = value;
            assert!(decode(bad)
                .and_then(|ops| crate::session::validate_staged_operations(&ops))
                .is_err());
        }
        for field in ["body", "destination"] {
            let mut bad = operation.clone();
            bad[field] = serde_json::Value::Null;
            assert!(decode(bad).is_err(), "explicit null {field} must refuse");
        }
        let mut bad = operation.clone();
        bad["payload"]["EntitySourcePatch"]["source_base"]["schema"] =
            serde_json::json!("kin.entity.source_base.v2");
        assert!(decode(bad).is_err());
        let mut bad = operation;
        bad["payload"]["EntitySourcePatch"]
            .as_object_mut()
            .unwrap()
            .remove("source_base");
        assert!(decode(bad).is_err());
    }
}
