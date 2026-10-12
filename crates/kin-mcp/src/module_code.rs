// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! A module's own code: its imports and the top-level statements outside
//! every entity it holds.
//!
//! A whole file's module surface has no body of its own to serve, because its
//! span is the file, and Kin does not return or edit whole files. What only
//! the module can say is the code no entity owns: the imports, the
//! assignments, the registrations and the `__all__` at its top level. That
//! code is served here, in file order, with every function, class and large
//! value the module holds replaced by one marker line:
//!
//! ```text
//! import os
//! import typing as t
//!
//! _missing = object()
//!
//! # kin:entity function get_debug_flag #5f3a9c1e
//! ```
//!
//! A marker begins with the language's comment opener and the word
//! `kin:entity`, then names the entity's kind, its name and the short id
//! `source` reads it by. Nothing in it depends on the entity's body, so the
//! served text changes only when the module's own code does, or when the
//! module gains, loses or renames an entity. Every line without that word is
//! the module's code exactly as the file holds it. A value entity of at most
//! [`INLINE_VALUE_MAX_BYTES`] (`LIMIT = 3`) stays in the code as the statement
//! it is; a larger one is a marker, read and patched by its own id.
//!
//! Language neutral by construction: the code is cut around the spans the
//! graph already records for the entities of the file, so no syntax is read
//! here. A language joins by naming its marker comment in
//! [`kin_model::module_code_syntax`].
//!
//! The same view is the base of a change. A patch names literal anchors in the
//! served text, each exactly once, and is applied to that text; the result must
//! still hold every marker, unchanged and in order, and each marker is then
//! put back as the entity's exact bytes. So a patch can add, change or remove
//! module-level code anywhere around the entities, and can never change, drop,
//! reorder or overwrite one: a whole-file overwrite has no spelling here.
//!
//! The base of that change guards the served text and nothing wider: its
//! length and its digest. So it holds through an edit to the body of any
//! entity the module holds, the way an entity's base holds through edits
//! beside it, and goes stale when the module's own code changes.

use std::ops::Range;

use kin_model::{Entity, EntityId, EntityKind, GraphStore};
use serde_json::{json, Map, Value};

use crate::error::{McpError, Result};
use crate::handlers::common::{
    EntitySourceScope, ExactEntitySource, HeldSourceAuthority, SpanCoherence,
};
use crate::source_base::{EntitySourceBase, EntityTextEdit, SourceBaseSchema};

/// The word every marker carries right after its comment opener. Code that is
/// the module's own never gains it through a patch.
pub const MARKER_WORD: &str = "kin:entity";

/// The largest value entity served as the statement it is. A larger one is a
/// marker, like a function: it has an id to be read and patched by, and a data
/// table of many kilobytes is not what a reader of module-level code came for.
pub const INLINE_VALUE_MAX_BYTES: usize = 1_024;

/// The key a module-code reply carries beside its body.
pub const REPLY_KEY: &str = "module_code";

/// The key a class-code reply carries beside its body.
pub const CLASS_REPLY_KEY: &str = "class_code";

/// The code a class reply leads its reason with when its own code could not
/// be served and its outline was served instead, with no base.
pub const CLASS_CODE_UNAVAILABLE: &str = "class_code_unavailable";

/// Whether `kind` is a type that holds members in its own span.
fn is_class_kind(kind: EntityKind) -> bool {
    matches!(
        kind,
        EntityKind::Class | EntityKind::Interface | EntityKind::TraitDef | EntityKind::EnumDef
    )
}

/// Whether `entity` is a class Kin serves as its own code: its header, its
/// docstring and the statements of its body, with each member it holds
/// replaced by one marker line.
///
/// A class that fits a reply whole is served whole, by default or when its
/// whole body is asked for, and is patched and replaced as one entity. A
/// class larger than that could only be served as an outline, the start of
/// its code and a list of its members, so a statement of its body after its
/// first member could not be read at all, and a patch to the class had to be
/// written blind against a base that guarded every member's bytes. Such a
/// class is served the way a module is: what only the class can say, in
/// source order, and its members by id. A class with no member is its own
/// code already and stays an ordinary entity.
///
/// The same test decides, on the write side, which base a class carries, so
/// it reads nothing but the graph.
pub fn serves_class_code<S: kin_model::EntityStore>(store: &S, entity: &Entity) -> bool {
    if !is_class_kind(entity.kind) || kin_model::module_code_syntax(entity.language).is_none() {
        return false;
    }
    let (Some(span), Some(file)) = (entity.span.as_ref(), entity.file_origin.as_ref()) else {
        return false;
    };
    if span.file != *file
        || span.end_byte.saturating_sub(span.start_byte)
            <= crate::container_shape::CONTAINER_FULL_BODY_MAX_BYTES
    {
        return false;
    }
    store
        .query_entities(&kin_model::EntityFilter {
            file_path: Some(file.clone()),
            ..Default::default()
        })
        .is_ok_and(|entities| {
            entities.iter().any(|held| {
                held.id != entity.id
                    && held.span.as_ref().is_some_and(|held| {
                        held.file == *file
                            && held.start_byte < held.end_byte
                            && span.start_byte <= held.start_byte
                            && held.end_byte <= span.end_byte
                    })
            })
        })
}

/// The code a module reply leads its reason with when its own code could not
/// be served and its inventory was served instead.
pub const MODULE_CODE_UNAVAILABLE: &str = "module_code_unavailable";

/// Why a module-code read issued no `source_base`.
pub const SOURCE_BASE_UNAVAILABLE_UNVERIFIED: &str = "source_base_unavailable: a span in this \
module is unverified against the module's bytes, because the graph recorded no source digest \
for it, so no source_base was issued and Kin cannot guard a change to this module's code. Stop \
and report this gap rather than sending an unguarded change. An older repository may need \
`kin upgrade`; reread afterward and proceed only if Kin issues a source_base.";

/// One entity a module holds, as its marker stands for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldEntity {
    pub id: EntityId,
    pub name: String,
    pub kind: EntityKind,
    /// The exact marker text served for it.
    pub marker: String,
}

/// One run of the served text and the file bytes it stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Piece {
    /// Where the run sits in [`ModuleCode::text`].
    text: Range<usize>,
    /// The file bytes it stands for: the same bytes for the module's own
    /// code, and the whole entity for a marker.
    file: Range<usize>,
    held: Option<HeldEntity>,
}

/// A module's own code with its entities elided, and the map back to the
/// file's bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleCode {
    module: String,
    /// What holds the code, as a sentence names it: `module` or `class`.
    noun: &'static str,
    text: String,
    pieces: Vec<Piece>,
    /// The module's span in its file.
    span: Range<usize>,
}

/// An entity kind as a marker and a refusal spell it: `function`, `class`.
pub fn kind_word(kind: EntityKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|value| value.as_str().map(str::to_ascii_lowercase))
        .unwrap_or_else(|| format!("{kind:?}").to_ascii_lowercase())
}

/// The marker that stands for `entity`: its kind, its name and the leading
/// digits of its id. None of them moves when the entity's body is edited.
fn marker_line(syntax: kin_model::ModuleCodeSyntax, entity: &Entity, id_digits: usize) -> String {
    let id: String = entity
        .id
        .to_string()
        .chars()
        .filter(|c| *c != '-')
        .take(id_digits)
        .collect();
    format!(
        "{}{MARKER_WORD} {} {} #{id}{}",
        syntax.marker_open,
        kind_word(entity.kind),
        entity.name,
        syntax.marker_close,
    )
}

impl ModuleCode {
    /// Cut `module`'s own code out of `source`, the exact bytes of its file,
    /// around `entities`, every entity the graph holds for that file.
    ///
    /// Only the outermost entities are marked: a method is inside its class's
    /// marker. Spans that cross, leave the module or split a character are a
    /// graph gap and refuse the whole view, since a cut along them would serve
    /// part of an entity as the module's code.
    pub fn build(
        module: &Entity,
        source: &[u8],
        entities: &[Entity],
    ) -> std::result::Result<Self, String> {
        // A class is cut the same way, around the entities inside its span:
        // its members, each with whatever is nested in it.
        let class = is_class_kind(module.kind);
        let noun = if class { "class" } else { "module" };
        let syntax = kin_model::module_code_syntax(module.language)
            .filter(|_| class || kin_model::serves_module_code(module))
            .ok_or_else(|| format!("{} is not a module whose own code Kin serves", module.name))?;
        let span = module
            .span
            .as_ref()
            .ok_or_else(|| format!("{noun} {} records no source span", module.name))?;
        let file = module
            .file_origin
            .as_ref()
            .filter(|file| **file == span.file)
            .ok_or_else(|| format!("{noun} {} records no source file", module.name))?;
        // An empty span is a module that holds no code yet: its code is the
        // empty text, which a patch gives its first code.
        if span.start_byte > span.end_byte || span.end_byte > source.len() {
            return Err(format!(
                "{noun} {} span {}..{} is outside its {} byte file",
                module.name,
                span.start_byte,
                span.end_byte,
                source.len()
            ));
        }
        let code = std::str::from_utf8(&source[span.start_byte..span.end_byte])
            .map_err(|error| format!("{noun} {} is not valid UTF-8: {error}", module.name))?;

        let mut held: Vec<&Entity> = entities
            .iter()
            .filter(|entity| {
                entity.id != module.id
                    && entity.file_origin.as_ref() == Some(file)
                    && !kin_model::is_file_module_surface(entity)
                    && entity.span.as_ref().is_some_and(|held| {
                        held.file == *file
                            && held.start_byte < held.end_byte
                            // A class holds what lies inside it. What stands
                            // beside it, and what it stands in, are not its.
                            && (!class
                                || (span.start_byte <= held.start_byte
                                    && held.end_byte <= span.end_byte))
                    })
            })
            .collect();
        // Outermost first: by start, and the wider span first at one start.
        held.sort_by(|left, right| {
            let (left_span, right_span) =
                (left.span.as_ref().unwrap(), right.span.as_ref().unwrap());
            left_span
                .start_byte
                .cmp(&right_span.start_byte)
                .then(right_span.end_byte.cmp(&left_span.end_byte))
                .then(left.id.cmp(&right.id))
        });
        let mut outermost: Vec<(&Entity, bool)> = Vec::new();
        let mut covered = span.start_byte;
        for entity in held {
            let held_span = entity.span.as_ref().unwrap();
            if held_span.start_byte < span.start_byte || held_span.end_byte > span.end_byte {
                return Err(format!(
                    "entity {} lies outside {noun} {}'s recorded span",
                    entity.name, module.name
                ));
            }
            if held_span.start_byte >= covered {
                outermost.push((entity, false));
                covered = held_span.end_byte;
            } else if held_span.end_byte <= covered {
                // Nested in the outermost entity before it.
                if let Some(last) = outermost.last_mut() {
                    last.1 = true;
                }
            } else {
                return Err(format!(
                    "entity {} overlaps another entity of {noun} {} without nesting in it",
                    entity.name, module.name
                ));
            }
        }

        // A small value with nothing nested in it is the statement it is.
        let elided: Vec<&Entity> = outermost
            .into_iter()
            .filter(|(entity, nests)| {
                let held_span = entity.span.as_ref().unwrap();
                *nests
                    || !kin_model::is_module_value_kind(entity.kind)
                    || held_span.end_byte - held_span.start_byte > INLINE_VALUE_MAX_BYTES
            })
            .map(|(entity, _)| entity)
            .collect();

        let render = |id_digits: usize| -> std::result::Result<(String, Vec<Piece>), String> {
            let mut text = String::with_capacity(code.len().min(64 * 1024));
            let mut pieces = Vec::with_capacity(elided.len() * 2 + 1);
            let mut cursor = span.start_byte;
            let own = |text: &mut String, pieces: &mut Vec<Piece>, file: Range<usize>| {
                if file.is_empty() {
                    return Ok(());
                }
                let slice = code
                    .get(file.start - span.start_byte..file.end - span.start_byte)
                    .ok_or_else(|| {
                        format!(
                            "an entity span of {noun} {} splits a character",
                            module.name
                        )
                    })?;
                let start = text.len();
                text.push_str(slice);
                pieces.push(Piece {
                    text: start..text.len(),
                    file,
                    held: None,
                });
                Ok::<(), String>(())
            };
            for entity in &elided {
                let held_span = entity.span.as_ref().unwrap();
                own(&mut text, &mut pieces, cursor..held_span.start_byte)?;
                let marker = marker_line(syntax, entity, id_digits);
                let start = text.len();
                text.push_str(&marker);
                pieces.push(Piece {
                    text: start..text.len(),
                    file: held_span.start_byte..held_span.end_byte,
                    held: Some(HeldEntity {
                        id: entity.id,
                        name: entity.name.clone(),
                        kind: entity.kind,
                        marker,
                    }),
                });
                cursor = held_span.end_byte;
            }
            own(&mut text, &mut pieces, cursor..span.end_byte)?;
            Ok((text, pieces))
        };

        // A marker must be found again as itself, so each stands once in the
        // text. The short id makes it so; the whole id is the fallback.
        let (mut text, mut pieces) = render(8)?;
        if !markers_stand_once(&text, &pieces) {
            (text, pieces) = render(32)?;
            if !markers_stand_once(&text, &pieces) {
                return Err(format!(
                    "{noun} {}'s own code restates a marker Kin would serve, so its entities \
                     cannot be told from its code",
                    module.name
                ));
            }
        }
        Ok(Self {
            module: module.name.clone(),
            noun,
            text,
            pieces,
            span: span.start_byte..span.end_byte,
        })
    }

    /// The served text: the module's own code and one marker per entity.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// What holds this code, as a sentence names it: `module` or `class`.
    pub fn noun(&self) -> &'static str {
        self.noun
    }

    /// The code as a sentence names it: `the module-level code of app`.
    pub fn label(&self) -> String {
        format!("the {}-level code of {}", self.noun, self.module)
    }

    /// The declaration a patch may not write into this code, and the way to
    /// create one.
    fn creation(&self) -> &'static str {
        if self.noun == "class" {
            "create a method with EntityCreate and placement member"
        } else {
            "create a function or class with EntityCreate"
        }
    }

    /// The entities the text marks, in file order.
    pub fn held(&self) -> impl Iterator<Item = &HeldEntity> {
        self.pieces.iter().filter_map(|piece| piece.held.as_ref())
    }

    /// Whether `base` was read from this text: it records the text's length
    /// as its span's and the text's digest as its body's. Where the module's
    /// file has moved since, inside an entity or around the module's span,
    /// is not part of what a module's base guards.
    pub fn is_guarded_by(&self, base: &EntitySourceBase) -> bool {
        base.end_byte.checked_sub(base.start_byte) == Some(self.text.len())
            && kin_blobs::digest(self.text.as_bytes()).to_string() == base.body_hash
    }

    /// The entity whose bytes hold `needle`, when the text does not: an
    /// anchor copied from an entity's own body.
    fn entity_holding(&self, source: &[u8], needle: &str) -> Option<&HeldEntity> {
        let code = source.get(self.span.clone())?;
        let needle = needle.as_bytes();
        let mut from = 0;
        while let Some(at) = code[from..]
            .windows(needle.len())
            .position(|window| window == needle)
        {
            let start = self.span.start + from + at;
            let end = start + needle.len();
            if let Some(held) = self
                .pieces
                .iter()
                .filter(|piece| piece.file.start < end && start < piece.file.end)
                .find_map(|piece| piece.held.as_ref())
            {
                return Some(held);
            }
            from += at + 1;
        }
        None
    }

    /// Apply a patch's edits to the served text and return the module's file
    /// with exactly that change, or the one sentence that says why not and
    /// what to send instead.
    ///
    /// Every anchor addresses the text as it was served, never an earlier
    /// replacement, the way an entity patch reads its anchors. The patched
    /// text must hold every marker once, unchanged and in order; each is put
    /// back as its entity's exact bytes, so no edit reaches into an entity.
    pub fn apply(
        &self,
        source: &[u8],
        edits: &[EntityTextEdit],
    ) -> std::result::Result<Vec<u8>, String> {
        let module = &self.module;
        let noun = self.noun;
        let code = self.label();
        if source.len() < self.span.end {
            return Err(format!(
                "{noun} {module} is not the source its code was read from, so read {module} \
                 again with source and patch against that read"
            ));
        }
        if edits.is_empty() {
            return Err(format!("a {noun} patch needs at least one exact text edit"));
        }
        let text = self.text.as_str();
        // One edit with an empty anchor is the first code of a module that
        // holds none. Nothing else has a place an empty anchor could name.
        if let [edit] = edits {
            if edit.old_text.is_empty() {
                if !text.is_empty() {
                    return Err(format!(
                        "patch edit #0: old_text is empty, which only gives a module with no \
                         code its first code, and {module} holds code, so anchor old_text on \
                         text copied from the body source returned for {module}"
                    ));
                }
                if edit.new_text.is_empty() {
                    return Err(format!(
                        "the patch leaves {code} as it is, so nothing was written"
                    ));
                }
                if edit.new_text.contains(MARKER_WORD) {
                    return Err(format!(
                        "new_text writes a {MARKER_WORD} line, and those lines stand for \
                         entities Kin already holds and are never written as code, so leave it \
                         out and {}",
                        self.creation()
                    ));
                }
                let mut rebuilt = Vec::with_capacity(source.len() + edit.new_text.len());
                rebuilt.extend_from_slice(&source[..self.span.start]);
                rebuilt.extend_from_slice(edit.new_text.as_bytes());
                rebuilt.extend_from_slice(&source[self.span.end..]);
                return Ok(rebuilt);
            }
        }
        let mut replacements = Vec::with_capacity(edits.len());
        for (index, edit) in edits.iter().enumerate() {
            if edit.old_text.is_empty() {
                return Err(format!("patch edit #{index}: old_text must be nonempty"));
            }
            let Some(start) = text.find(&edit.old_text) else {
                return Err(match self.entity_holding(source, &edit.old_text) {
                    Some(held) => format!(
                        "patch edit #{index}: old_text is inside {} {}, which {code} shows only \
                         as a {MARKER_WORD} line, so read {} with source (entity_id {}) and \
                         patch it with the source_base that read returns",
                        kind_word(held.kind),
                        held.name,
                        held.name,
                        held.id
                    ),
                    None => format!(
                        "patch edit #{index}: old_text is not in {code}, so copy it exactly \
                         from the body source returned for {module}, or read {module} again \
                         with source"
                    ),
                });
            };
            // Counted from one character after each match, so overlapping
            // occurrences ("aa" in "aaa") are ambiguous too.
            let step = edit.old_text.chars().next().unwrap().len_utf8();
            let mut count = 1;
            let mut from = start + step;
            while let Some(at) = text[from..].find(&edit.old_text) {
                count += 1;
                from += at + step;
            }
            if count > 1 {
                return Err(format!(
                    "patch edit #{index}: old_text occurs {count} times in {code}, so extend \
                     it with the text beside the one you mean until it occurs once"
                ));
            }
            replacements.push((start, start + edit.old_text.len(), index, &edit.new_text));
        }
        replacements.sort_by_key(|(start, _, _, _)| *start);
        for pair in replacements.windows(2) {
            if pair[0].1 > pair[1].0 {
                return Err(format!(
                    "patch edits #{} and #{} overlap in {code}, so send them as one edit",
                    pair[0].2, pair[1].2
                ));
            }
        }
        let mut patched = text.to_owned();
        for (start, end, _, replacement) in replacements.into_iter().rev() {
            patched.replace_range(start..end, replacement);
        }

        // Put every entity back as its exact bytes, where its marker stands.
        // Markers are found in order from the last one, so the pass reads the
        // patched text once.
        let mut rebuilt = Vec::with_capacity(source.len() + patched.len());
        rebuilt.extend_from_slice(&source[..self.span.start]);
        let mut cursor = 0;
        for piece in &self.pieces {
            let Some(held) = piece.held.as_ref() else {
                continue;
            };
            let Some(at) = patched[cursor..].find(held.marker.as_str()) else {
                let what = format!("{} {}", kind_word(held.kind), held.name);
                return Err(if patched[..cursor].contains(held.marker.as_str()) {
                    format!(
                        "the patch moves {what} past another entity, and a {noun} patch changes \
                         the code around entities without reordering them, so keep every \
                         {MARKER_WORD} line in the order source returned"
                    )
                } else {
                    format!(
                        "the patch changes or removes the {MARKER_WORD} line that stands for \
                         {what}, which is not code of {module}, so keep that line exactly as \
                         source returned it and change {} by its own id (entity_id {}): source \
                         it and patch it, or remove it with EntityRemove",
                        held.name, held.id
                    )
                });
            };
            let at = cursor + at;
            rebuilt.extend_from_slice(&patched.as_bytes()[cursor..at]);
            rebuilt.extend_from_slice(&source[piece.file.clone()]);
            cursor = at + held.marker.len();
        }
        rebuilt.extend_from_slice(&patched.as_bytes()[cursor..]);
        rebuilt.extend_from_slice(&source[self.span.end..]);

        // Every marker word the patch added is a marker written as code: a
        // copy of a served one, or an invented one.
        if patched.matches(MARKER_WORD).count() > text.matches(MARKER_WORD).count() {
            return Err(
                match self
                    .held()
                    .find(|held| patched.matches(held.marker.as_str()).count() > 1)
                {
                    Some(held) => format!(
                        "the patch repeats the {MARKER_WORD} line that stands for {} {}, and \
                         that line stands for one entity in one place, so keep it once, where \
                         source returned it",
                        kind_word(held.kind),
                        held.name
                    ),
                    None => format!(
                        "new_text writes a {MARKER_WORD} line, and those lines stand for \
                         entities Kin already holds and are never written as code, so leave it \
                         out and {}",
                        self.creation()
                    ),
                },
            );
        }
        if rebuilt == source {
            return Err(format!(
                "the patch leaves {code} as it is, so nothing was written"
            ));
        }
        Ok(rebuilt)
    }
}

/// Whether every marker stands exactly once in `text`, so each is found again
/// as itself after a patch.
fn markers_stand_once(text: &str, pieces: &[Piece]) -> bool {
    let markers: Vec<&str> = pieces
        .iter()
        .filter_map(|piece| piece.held.as_ref())
        .map(|held| held.marker.as_str())
        .collect();
    // The common case reads nothing twice: no code of the module says the
    // marker word, each marker says it once, and no two markers are alike, so
    // none can stand inside another or inside the code.
    let own_says_word = pieces
        .iter()
        .filter(|piece| piece.held.is_none())
        .any(|piece| text[piece.text.clone()].contains(MARKER_WORD));
    if !own_says_word
        && markers
            .iter()
            .all(|marker| marker.matches(MARKER_WORD).count() == 1)
    {
        let distinct: std::collections::BTreeSet<&str> = markers.iter().copied().collect();
        return distinct.len() == markers.len();
    }
    markers
        .iter()
        .all(|marker| text.matches(marker).count() == 1)
}

/// The base a patch to a module's own code carries back: the workspace
/// instant, the module's artifact, and the length and digest of the code as
/// served. Its span opens where the module's does and is as long as the
/// served text, not the file: the text is what a patch is written against,
/// so the text is what the base guards ([`ModuleCode::is_guarded_by`]).
fn module_source_base(
    context: crate::source_base::SourceBaseContext,
    module: &Entity,
    source: &ExactEntitySource,
    span: &kin_model::SourceSpan,
    code: &ModuleCode,
) -> std::result::Result<Option<EntitySourceBase>, String> {
    let kin_model::TreeEntry::Blob { hash, .. } = source.entry else {
        return Ok(None);
    };
    let base = EntitySourceBase {
        schema: SourceBaseSchema::V1,
        context,
        entity_id: module.id,
        artifact_id: source.artifact_id,
        source_blob_hash: hash.to_string(),
        start_byte: span.start_byte,
        end_byte: span.start_byte + code.text().len(),
        body_hash: kin_blobs::digest(code.text().as_bytes()).to_string(),
    };
    base.validate()?;
    Ok(Some(base))
}

/// The `source` reply for a module whose own code Kin serves, or `None` for
/// any other entity and for a history read, which are answered as before. A
/// module that holds no code yet is served as the empty text with a base, so
/// a patch can give it its first code.
///
/// The reply has the shape of an entity's source reply: `body` is the
/// module-level code, and `source_base` guards a patch to it. Nothing here
/// reads a working file: the bytes are the artifact's, through the same held
/// authority and the same digest check an entity read makes, applied to the
/// module and to every entity whose span the code is cut around.
pub fn module_code_reply<G: GraphStore>(
    held: &HeldSourceAuthority<'_, G>,
    module: &Entity,
    scope: EntitySourceScope,
) -> Result<Option<Value>> {
    if !kin_model::serves_module_code(module) || scope != EntitySourceScope::WorkspaceHead {
        return Ok(None);
    }
    let Some((source, bytes, span)) =
        crate::handlers::common::resolve_module_code_authority(held, module, scope)?
    else {
        return Ok(None);
    };
    let kin_model::TreeEntry::Blob { hash, .. } = source.entry else {
        return Ok(None);
    };
    let Some(file) = module.file_origin.clone() else {
        return Ok(None);
    };
    let entities = held
        .store()
        .query_entities(&kin_model::EntityFilter {
            file_path: Some(file.clone()),
            ..Default::default()
        })
        .map_err(McpError::graph)?;
    // The code is cut along every held span, so each must describe these
    // bytes, exactly as the span of an entity read must.
    let mut verified = source.span_coherence != SpanCoherence::Unverified;
    for entity in &entities {
        if entity.id == module.id || entity.span.is_none() {
            continue;
        }
        if crate::handlers::common::span_source_coherence(entity, &hash, &file.0)?
            == SpanCoherence::Unverified
        {
            verified = false;
        }
    }
    let mut current = module.clone();
    current.span = Some(span.clone());
    let code = ModuleCode::build(&current, &bytes, &entities)
        .map_err(|reason| McpError::Context(format!("graph authority gap: {reason}")))?;

    let source_base = if verified {
        match held.workspace_sample()?.source_base_context.clone() {
            Some(context) => module_source_base(context, module, &source, &span, &code)
                .map_err(McpError::Context)?,
            // Hosted source has no local workspace to change.
            None => None,
        }
    } else {
        None
    };
    let mut reply = json!({
        "source_base": source_base,
        "id": module.id,
        "name": module.name,
        "kind": module.kind,
        "language": module.language,
        "file_path": module.file_origin.as_ref().map(|path| path.to_string()),
        "read_path": crate::handlers::common::entity_read_path(module),
        "start_line": crate::handlers::common::entity_presentation_start_line(module),
        "end_line": crate::handlers::common::entity_presentation_end_line(module),
        "start_byte": span.start_byte,
        "end_byte": span.end_byte,
        "signature": module.signature,
        "body": code.text(),
        "source": "graph",
        REPLY_KEY: {
            "entities": code.held().count(),
            "marker": MARKER_WORD,
        },
    });
    if code.text().is_empty() {
        reply[REPLY_KEY]["empty"] = json!(true);
    }
    if let Some(map) = reply.as_object_mut() {
        map.extend(crate::handlers::common::source_provenance_fields(&source));
        // The module's artifact entry is its file's, which no reader of
        // module-level code acts on; the base carries what a write needs.
        map.remove("artifact_entry");
        if !verified {
            // The label of the weakest span the code was cut along.
            map.insert(
                "span_coherence".into(),
                json!(SpanCoherence::Unverified.label()),
            );
            map.insert(
                "source_base_unavailable".into(),
                json!(SOURCE_BASE_UNAVAILABLE_UNVERIFIED),
            );
        }
    }
    Ok(Some(reply))
}

/// The `source` reply for a class Kin serves as its own code
/// ([`serves_class_code`]), or `None` for any other entity and for a history
/// read, which are answered as before.
///
/// The reply has the shape of an entity's source reply: `body` is the class's
/// own code, from its header to its last line, with one marker line where
/// each member stands, and `source_base` guards a patch to it. The base
/// guards that text and nothing wider, so it holds through an edit to any
/// member's body and goes stale when the class's own code changes or the
/// class gains, loses or renames a member.
pub fn class_code_reply<G: GraphStore>(
    held: &HeldSourceAuthority<'_, G>,
    class: &Entity,
    scope: EntitySourceScope,
) -> Result<Option<Value>> {
    if scope != EntitySourceScope::WorkspaceHead || !serves_class_code(held.store(), class) {
        return Ok(None);
    }
    let Some((source, bytes, span)) =
        crate::handlers::common::resolve_class_code_authority(held, class, scope)?
    else {
        return Ok(None);
    };
    let kin_model::TreeEntry::Blob { hash, .. } = source.entry else {
        return Ok(None);
    };
    let Some(file) = class.file_origin.clone() else {
        return Ok(None);
    };
    let entities = held
        .store()
        .query_entities(&kin_model::EntityFilter {
            file_path: Some(file.clone()),
            ..Default::default()
        })
        .map_err(McpError::graph)?;
    // The code is cut along the span of every entity inside the class, so
    // each must describe these bytes, exactly as the span of an entity read
    // must.
    let mut verified = source.span_coherence != SpanCoherence::Unverified;
    for entity in &entities {
        let inside = entity.id != class.id
            && entity.span.as_ref().is_some_and(|held| {
                span.start_byte <= held.start_byte && held.end_byte <= span.end_byte
            });
        if inside
            && crate::handlers::common::span_source_coherence(entity, &hash, &file.0)?
                == SpanCoherence::Unverified
        {
            verified = false;
        }
    }
    let mut current = class.clone();
    current.span = Some(span.clone());
    let code = ModuleCode::build(&current, &bytes, &entities)
        .map_err(|reason| McpError::Context(format!("graph authority gap: {reason}")))?;
    let members = code.held().count();
    if members == 0 {
        return Ok(None);
    }
    let source_base = if verified {
        match held.workspace_sample()?.source_base_context.clone() {
            Some(context) => module_source_base(context, class, &source, &span, &code)
                .map_err(McpError::Context)?,
            // Hosted source has no local workspace to change.
            None => None,
        }
    } else {
        None
    };
    let mut reply = json!({
        "source_base": source_base,
        "id": class.id,
        "name": class.name,
        "kind": class.kind,
        "language": class.language,
        "file_path": class.file_origin.as_ref().map(|path| path.to_string()),
        "read_path": crate::handlers::common::entity_read_path(class),
        "start_line": crate::handlers::common::entity_presentation_start_line(class),
        "end_line": crate::handlers::common::entity_presentation_end_line(class),
        "start_byte": span.start_byte,
        "end_byte": span.end_byte,
        "signature": class.signature,
        "body": code.text(),
        "source": "graph",
        CLASS_REPLY_KEY: {
            "members": members,
            "marker": MARKER_WORD,
        },
    });
    if let Some(map) = reply.as_object_mut() {
        map.extend(crate::handlers::common::source_provenance_fields(&source));
        // The artifact entry is the source unit's, which no reader of a
        // class's own code acts on; the base carries what a write needs.
        map.remove("artifact_entry");
        if !verified {
            // The label of the weakest span the code was cut along.
            map.insert(
                "span_coherence".into(),
                json!(SpanCoherence::Unverified.label()),
            );
            map.insert(
                "source_base_unavailable".into(),
                json!(SOURCE_BASE_UNAVAILABLE_UNVERIFIED),
            );
        }
    }
    Ok(Some(reply))
}

/// The `source` reply for any module container: its own code where Kin
/// serves that, and its recorded member inventory otherwise; and for a class
/// served as its own code, that code. `None` for any other entity, which the
/// ordinary read answers.
///
/// A module whose code cannot be served now, because its bytes or a span the
/// code is cut along cannot be proven, is answered as its inventory with the
/// reason, so a reader still gets its entities by id and a write still learns
/// why it has no base.
pub fn source_reply<G: GraphStore>(
    held: &HeldSourceAuthority<'_, G>,
    entity: &Entity,
    scope: EntitySourceScope,
) -> Option<Value> {
    if is_class_kind(entity.kind) {
        // A class whose own code cannot be served now is answered by the
        // ordinary read as its outline, which then carries no base
        // ([`crate::container_shape::shape_source_reply`]).
        return class_code_reply(held, entity, scope).ok().flatten();
    }
    match module_code_reply(held, entity, scope) {
        Ok(Some(reply)) => Some(reply),
        Ok(None) => crate::container_shape::module_source_reply(held.store(), entity),
        Err(error) => {
            let mut reply = crate::container_shape::module_source_reply(held.store(), entity)?;
            let reason = format!(
                "{MODULE_CODE_UNAVAILABLE}: the module-level code of {} cannot be served now ({error}), \
                 so this reply lists its entities only and carries no source_base",
                entity.name
            );
            reply["source_base_unavailable"] = json!(reason);
            Some(reply)
        }
    }
}

/// How a write reply names a change to a module: the module-level code of the
/// module, by its dotted name. A module is changed in no other way, so a
/// changed row whose entity is a module is a patch to that code. `None` for a
/// row about any other entity.
pub(crate) fn change_label(row: &Value) -> Option<String> {
    let kind = row.get("kind").and_then(Value::as_str)?;
    if !matches!(kind, "module" | "package") {
        return None;
    }
    let name = row
        .get("module")
        .or_else(|| row.get("name"))
        .and_then(Value::as_str)?;
    Some(format!("the module-level code of {name}"))
}

/// Whether a machine `source` reply is a module's own code, or a class's.
pub fn is_module_code_reply(reply: &Map<String, Value>) -> bool {
    reply.get(REPLY_KEY).is_some_and(Value::is_object) || is_class_code_reply(reply)
}

/// Whether a machine `source` reply is a class's own code.
pub fn is_class_code_reply(reply: &Map<String, Value>) -> bool {
    reply.get(CLASS_REPLY_KEY).is_some_and(Value::is_object)
}

/// What a module-code reply is, in the words a plain reply leads with.
pub fn plain_lead(name: &str, reply: &Map<String, Value>) -> String {
    if is_class_code_reply(reply) {
        let members = reply
            .get(CLASS_REPLY_KEY)
            .and_then(|fields| fields.get("members"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let held = match members {
            1 => format!(
                "the {MARKER_WORD} line stands for the one member it holds, read and changed \
                 with source and the #id on that line"
            ),
            count => format!(
                "each of the {count} {MARKER_WORD} lines stands for one member it holds, read \
                 and changed with source and the #id on that line"
            ),
        };
        return format!(
            "this is the class-level code of {name}, its header, docstring and the statements \
             of its body outside every member ({held})"
        );
    }
    // A module is named the way its rows name it, by its dotted name.
    let name = reply
        .get("module")
        .and_then(Value::as_str)
        .filter(|module| !module.is_empty())
        .unwrap_or(name);
    let entities = reply
        .get(REPLY_KEY)
        .and_then(|fields| fields.get("entities"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let empty = reply
        .get(REPLY_KEY)
        .and_then(|fields| fields.get("empty"))
        .and_then(Value::as_bool)
        == Some(true);
    if empty {
        return format!(
            "{name} is a module that holds no code yet (give it its first code with a patch on \
             this source_base whose one edit has an empty old_text and the code as new_text)"
        );
    }
    let held = match entities {
        0 => "it holds no function or class".to_string(),
        1 => format!(
            "the {MARKER_WORD} line stands for the one entity it holds, read with source and \
             the #id on that line"
        ),
        count => format!(
            "each of the {count} {MARKER_WORD} lines stands for one entity it holds, read with \
             source and the #id on that line"
        ),
    };
    format!(
        "this is the module-level code of {name}, its imports and top-level statements ({held})"
    )
}

#[cfg(test)]
mod tests;
