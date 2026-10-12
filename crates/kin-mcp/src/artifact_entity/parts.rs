// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The parts of an unparsed entity held as text: the sections its headings
//! open.
//!
//! A changelog is a list of versions and a guide a list of topics, each under
//! a heading. Each section is a part, an entity of its own: addressed as
//! `<module>.<stem> :: <title>`, read and patched by itself, so a reader
//! takes the one version or topic it needs rather than the whole text. A part
//! holds the text from its heading to the next heading of its own depth or
//! above, its own sub-parts included.
//!
//! Each format is one [`PartReader`], asked by the kind
//! [`super::kind::describe`] names, in the order of [`READERS`]. A format
//! joins by adding its reader there; nothing else in Kin knows its headings.
//!
//! A part's id is derived from its entity's id and the titles that lead to it,
//! so it holds while the part's text changes and changes when its title does.
//! Parts are read from content the graph holds and kept by its digest for
//! every later question, within a bound. Nothing is stored.

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use kin_model::graph::GraphStore;
use kin_model::{ArtifactId, Hash256, TreeEntry};
use serde_json::{json, Map, Value};

use super::{kind, ArtifactEntity, Held, Level};
use crate::error::{McpError, Result};
use crate::handlers::common::HeldSourceAuthority;
use crate::handlers::repository_authority::AuthorityHeadReadSample;
use crate::source_base::{EntitySourceBase, SourceBaseSchema};

/// What every part id is derived under, so no other derivation shares one.
const PART_DOMAIN: &[u8] = b"kin.part.v1";

/// The last two bytes of every part id. A part id is a custom (version 8)
/// id that ends with them, which no id of code or of a whole unparsed entity
/// is, so a source_base names a part by its id alone. They end it so its
/// leading digits, which a short id is, stay as varied as any id's.
const PART_TAG: [u8; 2] = *b"kp";

/// The most parts kept across questions, summed over every content, before
/// the store is emptied and filled again.
const KEPT_PARTS_MAX: usize = 200_000;

/// The longest title a part keeps. A longer heading is a paragraph a format
/// happened to underline, not a section.
const TITLE_MAX_CHARS: usize = 200;

/// One section of a text, as byte offsets into it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    /// The heading's text, without its markup.
    pub title: String,
    /// How deep the heading is, 1 for the outermost its format writes.
    pub depth: usize,
    /// Where the heading begins: the start of the part.
    pub start: usize,
    /// Where the text under the heading begins.
    pub body_start: usize,
    /// Where the part ends: the next heading at its depth or above, or the
    /// end of the text.
    pub end: usize,
    /// The part that holds this one, as an index into the same list.
    pub parent: Option<usize>,
}

/// One heading a reader found, before the parts are closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Heading {
    pub title: String,
    pub depth: usize,
    pub start: usize,
    pub body_start: usize,
}

/// One format whose headings Kin reads.
pub trait PartReader: Sync {
    /// Whether this reader reads an entity of `kind`, as
    /// [`super::kind::describe`] names it.
    fn reads(&self, kind: &str) -> bool;

    /// The headings of `text`, in order.
    fn headings(&self, text: &str) -> Vec<Heading>;

    /// The spans of `text` its format marks as code, each without its
    /// markup: where prose names a function, a class or a module.
    fn code_spans<'t>(&self, text: &'t str) -> Vec<&'t str>;
}

/// The text between each pair of `open` and `close` in `text`, on one line.
fn between<'t>(text: &'t str, open: &str, close: &str) -> Vec<&'t str> {
    let mut spans = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find(open) {
        let after = &rest[at + open.len()..];
        match after.find(close) {
            Some(end) if !after[..end].contains('\n') && end > 0 => {
                spans.push(&after[..end]);
                rest = &after[end + close.len()..];
            }
            _ => rest = after,
        }
    }
    spans
}

/// Every format whose parts Kin reads, asked in order.
pub static READERS: &[&dyn PartReader] = &[&Markdown, &ReStructuredText];

/// The reader for an entity of `kind`, when Kin reads its parts.
pub fn reader_for(kind: &str) -> Option<&'static dyn PartReader> {
    READERS.iter().copied().find(|reader| reader.reads(kind))
}

/// The parts of `text`, read by `reader`: each heading closed at the next one
/// at its depth or above, and placed under the nearest shallower one.
pub fn parts(reader: &dyn PartReader, text: &str) -> Vec<Part> {
    let headings = reader.headings(text);
    let mut parts: Vec<Part> = Vec::with_capacity(headings.len());
    let mut open: Vec<usize> = Vec::new();
    for heading in headings {
        while let Some(&last) = open.last() {
            if parts[last].depth >= heading.depth {
                parts[last].end = heading.start;
                open.pop();
            } else {
                break;
            }
        }
        parts.push(Part {
            title: heading.title,
            depth: heading.depth,
            start: heading.start,
            body_start: heading.body_start,
            end: text.len(),
            parent: open.last().copied(),
        });
        open.push(parts.len() - 1);
    }
    parts
}

/// The parts of the content with digest `hash`, read by the reader for
/// `kind` from the text `load` gives, which is called only when they are not
/// kept already. `None` when Kin reads no parts of that kind or the text
/// cannot be loaded; an empty list when it has no heading.
pub fn parts_of(
    kind: &str,
    hash: Hash256,
    load: impl FnOnce() -> Option<Arc<Vec<u8>>>,
) -> Option<Arc<Vec<Part>>> {
    // Kept by the content and the reader that read it.
    let index = READERS.iter().position(|reader| reader.reads(kind))?;
    let reader = READERS[index];
    static KEPT: OnceLock<Mutex<(HashMap<(Hash256, usize), Arc<Vec<Part>>>, usize)>> =
        OnceLock::new();
    let kept = KEPT.get_or_init(Default::default);
    if let Some(found) = kept
        .lock()
        .ok()
        .and_then(|kept| kept.0.get(&(hash, index)).cloned())
    {
        return Some(found);
    }
    let content = load()?;
    let text = std::str::from_utf8(&content).ok()?;
    let read = Arc::new(parts(reader, text));
    if let Ok(mut kept) = kept.lock() {
        if kept.1 + read.len() > KEPT_PARTS_MAX {
            kept.0.clear();
            kept.1 = 0;
        }
        kept.1 += read.len();
        kept.0.insert((hash, index), Arc::clone(&read));
    }
    Some(read)
}

/// The titles that lead to the part at `index`, outermost first.
pub fn title_path(parts: &[Part], index: usize) -> Vec<&str> {
    let mut path = Vec::new();
    let mut at = Some(index);
    while let Some(here) = at {
        path.push(parts[here].title.as_str());
        at = parts[here].parent;
    }
    path.reverse();
    path
}

/// The id of the part at `index` of `parts`, a part of the entity `owner`:
/// derived from the owner and the titles that lead to the part, and, where
/// earlier parts lead by the same titles, how many do.
pub fn part_id(owner: ArtifactId, parts: &[Part], index: usize) -> uuid::Uuid {
    let path = title_path(parts, index);
    let occurrence = (0..index)
        .filter(|earlier| title_path(parts, *earlier) == path)
        .count();
    derive_id(owner, &path, occurrence)
}

/// The id of every part of `parts`, in order: [`part_id`] for each, counted
/// in one pass.
pub fn part_ids(owner: ArtifactId, parts: &[Part]) -> Vec<uuid::Uuid> {
    let mut seen: HashMap<Vec<&str>, usize> = HashMap::new();
    (0..parts.len())
        .map(|index| {
            let path = title_path(parts, index);
            let occurrence = seen.entry(path.clone()).or_default();
            let id = derive_id(owner, &path, *occurrence);
            *occurrence += 1;
            id
        })
        .collect()
}

fn derive_id(owner: ArtifactId, path: &[&str], occurrence: usize) -> uuid::Uuid {
    let mut name = PART_DOMAIN.to_vec();
    name.extend_from_slice(owner.0.as_bytes());
    for title in path {
        name.extend_from_slice(&(title.len() as u64).to_le_bytes());
        name.extend_from_slice(title.as_bytes());
    }
    name.extend_from_slice(&(occurrence as u64).to_le_bytes());
    let digest = kin_blobs::digest(&name);
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest.as_bytes()[..16]);
    bytes[14..].copy_from_slice(&PART_TAG);
    uuid::Builder::from_custom_bytes(bytes).into_uuid()
}

/// Whether `id` is a part's.
pub fn is_part_id(id: &uuid::Uuid) -> bool {
    id.get_version_num() == 8 && id.as_bytes()[14..] == PART_TAG
}

/// The lines of `text` with the byte offset each begins at, each without its
/// line end.
fn lines(text: &str) -> impl Iterator<Item = (usize, &str)> {
    let mut at = 0;
    text.split_inclusive('\n').map(move |line| {
        let start = at;
        at += line.len();
        (start, line.trim_end_matches(['\n', '\r']))
    })
}

/// A heading's text as a title, or `None` when it is not one: empty, or
/// longer than a title is.
fn title(text: &str) -> Option<String> {
    let title = text.split_whitespace().collect::<Vec<_>>().join(" ");
    (!title.is_empty() && title.chars().count() <= TITLE_MAX_CHARS).then_some(title)
}

/// Markdown: `#` headings and the two underlined forms, outside fenced code
/// and front matter.
struct Markdown;

impl Markdown {
    /// The depth and title of an ATX heading line: up to three spaces, one to
    /// six `#`, then a space or nothing.
    fn atx(line: &str) -> Option<(usize, String)> {
        let indent = line.len() - line.trim_start_matches(' ').len();
        if indent > 3 {
            return None;
        }
        let rest = &line[indent..];
        let depth = rest.len() - rest.trim_start_matches('#').len();
        if !(1..=6).contains(&depth) {
            return None;
        }
        let text = &rest[depth..];
        if !text.is_empty() && !text.starts_with([' ', '\t']) {
            return None;
        }
        // A closing run of `#` after a space is not part of the title.
        let text = text.trim();
        let text = match text.trim_end_matches('#') {
            kept if kept.is_empty() || kept.ends_with([' ', '\t']) => kept,
            _ => text,
        };
        Some((depth, title(text)?))
    }

    /// The depth an underline gives the line above it: 1 for `=`, 2 for `-`.
    fn underline(line: &str) -> Option<usize> {
        let indent = line.len() - line.trim_start_matches(' ').len();
        let marks = line.trim();
        if indent > 3 || marks.is_empty() {
            return None;
        }
        if marks.bytes().all(|byte| byte == b'=') {
            Some(1)
        } else if marks.bytes().all(|byte| byte == b'-') {
            Some(2)
        } else {
            None
        }
    }

    /// The fence a line opens or closes: its mark and length.
    fn fence(line: &str) -> Option<(char, usize)> {
        let trimmed = line.trim_start_matches(' ');
        if line.len() - trimmed.len() > 3 {
            return None;
        }
        let mark = trimmed.chars().next().filter(|c| *c == '`' || *c == '~')?;
        let run = trimmed.len() - trimmed.trim_start_matches(mark).len();
        (run >= 3).then_some((mark, run))
    }
}

impl PartReader for Markdown {
    fn reads(&self, kind: &str) -> bool {
        kind == "Markdown"
    }

    fn code_spans<'t>(&self, text: &'t str) -> Vec<&'t str> {
        // Inline code; a fence's text is code, not prose naming it.
        let mut spans = Vec::new();
        let mut fenced = false;
        for (_, line) in lines(text) {
            if Self::fence(line).is_some() {
                fenced = !fenced;
                continue;
            }
            if !fenced {
                spans.extend(
                    between(line, "`", "`")
                        .into_iter()
                        .map(|span| span.trim_matches('`')),
                );
            }
        }
        spans
    }

    fn headings(&self, text: &str) -> Vec<Heading> {
        let lines: Vec<(usize, &str)> = lines(text).collect();
        let mut headings = Vec::new();
        let mut fence: Option<(char, usize)> = None;
        let mut index = 0;
        // Front matter: a `---` first line up to the next `---` or `...`.
        if lines
            .first()
            .is_some_and(|(_, line)| line.trim_end() == "---")
        {
            if let Some(end) = lines
                .iter()
                .skip(1)
                .position(|(_, line)| matches!(line.trim_end(), "---" | "..."))
            {
                index = end + 2;
            }
        }
        while index < lines.len() {
            let (start, line) = lines[index];
            let next = lines
                .get(index + 1)
                .map(|(at, _)| *at)
                .unwrap_or(text.len());
            if let Some((mark, run)) = Self::fence(line) {
                match fence {
                    None => fence = Some((mark, run)),
                    Some((open, length)) if open == mark && run >= length => {
                        // A closing fence holds nothing after its marks.
                        if line.trim().trim_start_matches(mark).is_empty() {
                            fence = None;
                        }
                    }
                    Some(_) => {}
                }
                index += 1;
                continue;
            }
            if fence.is_some() {
                index += 1;
                continue;
            }
            if let Some((depth, title)) = Self::atx(line) {
                headings.push(Heading {
                    title,
                    depth,
                    start,
                    body_start: next,
                });
                index += 1;
                continue;
            }
            // An underlined heading: a line of text the next line underlines.
            // A paragraph's last line is one only when a blank line or a
            // heading came before it, so a list or table that happens to sit
            // above a rule is not taken for a title.
            let opens_block = index == 0 || lines[index - 1].1.trim().is_empty();
            if opens_block && !line.trim().is_empty() && line.len() - line.trim_start().len() <= 3 {
                if let Some((_, under)) = lines.get(index + 1) {
                    if let (Some(depth), Some(title)) = (Self::underline(under), title(line)) {
                        let after = lines
                            .get(index + 2)
                            .map(|(at, _)| *at)
                            .unwrap_or(text.len());
                        headings.push(Heading {
                            title,
                            depth,
                            start,
                            body_start: after,
                        });
                        index += 2;
                        continue;
                    }
                }
            }
            index += 1;
        }
        headings
    }
}

/// reStructuredText: a title underlined, or overlined and underlined, with a
/// run of one punctuation mark at least as long as the title. Depths follow
/// the order each style first appears in, as the format defines them.
struct ReStructuredText;

impl ReStructuredText {
    /// The mark a line is a run of, when it is an adornment: one punctuation
    /// character repeated, at least twice.
    fn adornment(line: &str) -> Option<char> {
        let line = line.trim_end();
        let mark = line.chars().next()?;
        (mark.is_ascii_punctuation() && line.len() >= 2 && line.chars().all(|c| c == mark))
            .then_some(mark)
    }
}

impl PartReader for ReStructuredText {
    fn reads(&self, kind: &str) -> bool {
        kind == "reStructuredText"
    }

    fn code_spans<'t>(&self, text: &'t str) -> Vec<&'t str> {
        // Inline literals, ``name``, and roles, :func:`name`.
        let mut spans = between(text, "``", "``");
        let mut rest = text;
        while let Some(at) = rest.find(":`") {
            let role = rest[..at].rsplit(':').next().unwrap_or_default();
            let after = &rest[at + 2..];
            let Some(end) = after.find('`') else {
                break;
            };
            if !role.is_empty() && role.chars().all(|c| c.is_ascii_alphanumeric() || c == '.') {
                // `title <target>` names its target.
                let span = &after[..end];
                let span = match (span.rfind('<'), span.strip_suffix('>')) {
                    (Some(open), Some(_)) => &span[open + 1..span.len() - 1],
                    _ => span,
                };
                spans.push(span);
            }
            rest = &after[end + 1..];
        }
        spans
    }

    fn headings(&self, text: &str) -> Vec<Heading> {
        let lines: Vec<(usize, &str)> = lines(text).collect();
        let mut styles: Vec<(char, bool)> = Vec::new();
        let mut headings = Vec::new();
        let mut index = 0;
        while index < lines.len() {
            let (start, line) = lines[index];
            let blank_before = index == 0 || lines[index - 1].1.trim().is_empty();
            // Overlined: adornment, title, the same adornment.
            if let (Some(over), Some((_, title_line)), Some((_, under))) = (
                Self::adornment(line),
                lines.get(index + 1),
                lines.get(index + 2),
            ) {
                let text_line = title_line.trim();
                if blank_before
                    && !text_line.is_empty()
                    && Self::adornment(title_line).is_none()
                    && Self::adornment(under) == Some(over)
                    && under.trim_end().len() == line.trim_end().len()
                    && line.trim_end().chars().count() >= text_line.chars().count()
                {
                    if let Some(title) = title(text_line) {
                        let depth = depth_of(&mut styles, (over, true));
                        let after = lines
                            .get(index + 3)
                            .map(|(at, _)| *at)
                            .unwrap_or(text.len());
                        headings.push(Heading {
                            title,
                            depth,
                            start,
                            body_start: after,
                        });
                        index += 3;
                        continue;
                    }
                }
            }
            // Underlined: a title at the margin, then an adornment at least
            // as long.
            if let Some((_, under)) = lines.get(index + 1) {
                if blank_before
                    && !line.trim().is_empty()
                    && !line.starts_with([' ', '\t'])
                    && Self::adornment(line).is_none()
                    && under.trim_end().chars().count() >= line.trim_end().chars().count()
                {
                    if let (Some(mark), Some(title)) = (Self::adornment(under), title(line)) {
                        let depth = depth_of(&mut styles, (mark, false));
                        let after = lines
                            .get(index + 2)
                            .map(|(at, _)| *at)
                            .unwrap_or(text.len());
                        headings.push(Heading {
                            title,
                            depth,
                            start,
                            body_start: after,
                        });
                        index += 2;
                        continue;
                    }
                }
            }
            index += 1;
        }
        headings
    }
}

/// The depth of a title style: the order it first appears in, from 1.
fn depth_of(styles: &mut Vec<(char, bool)>, style: (char, bool)) -> usize {
    match styles.iter().position(|seen| *seen == style) {
        Some(at) => at + 1,
        None => {
            styles.push(style);
            styles.len()
        }
    }
}

// ---------------------------------------------------------------------------
// Parts as entities
// ---------------------------------------------------------------------------

/// The key a source reply lists the sections of its body under, for the
/// presentation that serves a long text held with parts as its outline
/// ([`present`]): each section's id, title and depth, the row of the section
/// that holds it, and its span in the body. Never served as it is.
pub const PARTS_KEY: &str = "parts";

/// The key that names the entity a section is part of, by its address.
pub const PART_OF_KEY: &str = "part_of";

/// The kind every section is named by.
pub const PART_KIND: &str = "section";

/// The level clause of a section's reply.
pub const HELD: &str = "held as text (Kin knows it by its heading, and reads and changes it by \
itself)";

/// The most content one search for a section reads, summed over every text
/// it reads the headings of. Headings already kept cost nothing.
const SCAN_READ_MAX_BYTES: usize = 64 * 1024 * 1024;

/// The most sections an outline lists. The rest are found by title.
const LISTED_MAX: usize = 100;

/// The module a section of `owner` is placed in: its owner's module and its
/// owner's name before the extension, `shop.CHANGES`, or that name alone.
pub fn module_of(owner: &ArtifactEntity) -> String {
    let stem = match owner.name.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem,
        _ => owner.name.as_str(),
    };
    match &owner.module {
        Some(module) => format!("{module}.{stem}"),
        None => stem.to_string(),
    }
}

/// One section of one text, as the tree holds the text now.
#[derive(Debug, Clone)]
pub struct HeldPart {
    pub owner: ArtifactEntity,
    pub hash: Hash256,
    /// The kind its text is read as.
    pub kind: String,
    pub parts: Arc<Vec<Part>>,
    pub index: usize,
    pub id: uuid::Uuid,
}

impl HeldPart {
    pub fn part(&self) -> &Part {
        &self.parts[self.index]
    }

    pub fn module(&self) -> String {
        module_of(&self.owner)
    }

    /// Its address as a row writes it: `module :: title`.
    pub fn label(&self) -> String {
        format!("{} :: {}", self.module(), self.part().title)
    }

    /// Its address with every title that leads to it:
    /// `module :: Version 3.1.0 :: Bug fixes`.
    pub fn path_label(&self) -> String {
        format!(
            "{} :: {}",
            self.module(),
            title_path(&self.parts, self.index).join(" :: ")
        )
    }

    /// Whether another section lies inside this one.
    pub fn has_parts(&self) -> bool {
        self.parts
            .iter()
            .any(|part| part.parent == Some(self.index))
    }
}

/// The texts of `members` whose sections Kin reads, each with its content
/// digest and the kind its reader reads, in the order of where they are
/// projected. The kind is named from the entity's name, which is how every
/// format with a reader is known.
fn texts<'m>(
    members: &'m Held<'_>,
) -> impl Iterator<Item = (ArtifactEntity, Hash256, String)> + 'm {
    members.artifacts().filter_map(|entity| {
        let TreeEntry::Blob { hash, .. } = entity.artifact.entry else {
            return None;
        };
        let kind = kind::describe(&entity.place(), &entity.name, true, b"").kind;
        reader_for(&kind)?;
        Some((entity, hash, kind))
    })
}

/// What an address names among the sections of the texts Kin holds.
#[derive(Debug, Clone)]
pub enum NamedPart {
    One(HeldPart),
    Several(Vec<HeldPart>),
    Nothing,
}

/// The section `written` names: its id, or titles after an optional module,
/// each pair parted by `::`. `shop.CHANGES :: Unreleased` names the section
/// titled `Unreleased` in the text whose sections are placed in
/// `shop.CHANGES`; `shop.CHANGES :: Version 3.1.0 :: Bug fixes` the one
/// titled `Bug fixes` inside `Version 3.1.0`; `Unreleased` alone, every
/// section titled so. A title is matched exactly. A `#` and the leading
/// digits of an id narrow it to the section whose id begins with them.
pub fn named<G: GraphStore>(
    held: &HeldSourceAuthority<'_, G>,
    members: &Held<'_>,
    written: &str,
) -> NamedPart {
    let written = written.trim();
    if written.is_empty() {
        return NamedPart::Nothing;
    }
    let id = uuid::Uuid::parse_str(written).ok();
    if id.is_some_and(|id| !is_part_id(&id)) {
        return NamedPart::Nothing;
    }
    let (label, prefix) = match written.rsplit_once('#') {
        Some((label, digits)) if super::short_id(digits).is_some() => {
            (label.trim(), super::short_id(digits))
        }
        _ => (written, None),
    };
    let segments: Vec<&str> = label
        .split("::")
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .collect();
    if id.is_none() && segments.is_empty() && prefix.is_none() {
        return NamedPart::Nothing;
    }
    let read = Cell::new(0usize);
    let mut found = Vec::new();
    // With a module first, then with every segment a title.
    let readings: Vec<(Option<&str>, &[&str])> = match segments.split_first() {
        Some((module, titles)) if !titles.is_empty() => {
            vec![(Some(*module), titles), (None, segments.as_slice())]
        }
        _ => vec![(None, segments.as_slice())],
    };
    for (module, titles) in readings {
        for (owner, hash, kind) in texts(members) {
            if module.is_some_and(|module| module_of(&owner) != module) {
                continue;
            }
            if read.get() > SCAN_READ_MAX_BYTES {
                break;
            }
            let Some(parts) = parts_of(&kind, hash, || {
                let content = held.tree_content(hash).ok()?;
                read.set(read.get() + content.len());
                Some(content)
            }) else {
                continue;
            };
            if parts.is_empty() {
                continue;
            }
            let ids = part_ids(owner.id(), &parts);
            for (index, part_id) in ids.into_iter().enumerate() {
                let matches = match id {
                    Some(id) => part_id == id,
                    None => {
                        let path = title_path(&parts, index);
                        path.ends_with(titles)
                            && prefix.as_deref().is_none_or(|prefix| {
                                crate::entity_address::id_starts_with(&part_id.to_string(), prefix)
                            })
                    }
                };
                if matches {
                    found.push(HeldPart {
                        owner: owner.clone(),
                        hash,
                        kind: kind.clone(),
                        parts: Arc::clone(&parts),
                        index,
                        id: part_id,
                    });
                }
            }
            if id.is_some() && !found.is_empty() {
                break;
            }
        }
        if !found.is_empty() || id.is_some() {
            break;
        }
    }
    match found.len() {
        0 => NamedPart::Nothing,
        1 => NamedPart::One(found.remove(0)),
        _ => NamedPart::Several(found),
    }
}

/// The sections whose title holds `query`, the closest first: titled so
/// exactly, then beginning so, then holding it, each in the order of where
/// its text is projected and its place in the text. Case is not read. A
/// `module :: title` query is held to that module.
pub fn named_like<G: GraphStore>(
    held: &HeldSourceAuthority<'_, G>,
    members: &Held<'_>,
    query: &str,
) -> Vec<HeldPart> {
    let (module, wanted) = super::split_label(query.trim());
    let wanted = wanted.to_lowercase();
    if wanted.is_empty() {
        return Vec::new();
    }
    let read = Cell::new(0usize);
    let mut found: Vec<(u8, HeldPart)> = Vec::new();
    for (owner, hash, kind) in texts(members) {
        if module.is_some_and(|module| module_of(&owner) != module) {
            continue;
        }
        if read.get() > SCAN_READ_MAX_BYTES {
            break;
        }
        let Some(parts) = parts_of(&kind, hash, || {
            let content = held.tree_content(hash).ok()?;
            read.set(read.get() + content.len());
            Some(content)
        }) else {
            continue;
        };
        let mut ids: Option<Vec<uuid::Uuid>> = None;
        for (index, part) in parts.iter().enumerate() {
            let title = part.title.to_lowercase();
            let tier = if title == wanted {
                0
            } else if title.starts_with(&wanted) {
                1
            } else if title.contains(&wanted) {
                2
            } else {
                continue;
            };
            let id = ids.get_or_insert_with(|| part_ids(owner.id(), &parts))[index];
            found.push((
                tier,
                HeldPart {
                    owner: owner.clone(),
                    hash,
                    kind: kind.clone(),
                    parts: Arc::clone(&parts),
                    index,
                    id,
                },
            ));
        }
    }
    found.sort_by_key(|(tier, _)| *tier);
    found.into_iter().map(|(_, part)| part).collect()
}

/// The innermost section of `sections`, the sections of `owner`'s text, that
/// holds the byte at `at`.
pub fn section_at(
    owner: &ArtifactEntity,
    hash: Hash256,
    kind: &str,
    sections: &Arc<Vec<Part>>,
    at: usize,
) -> Option<HeldPart> {
    let index = sections
        .iter()
        .enumerate()
        .filter(|(_, part)| part.start <= at && at < part.end)
        .max_by_key(|(_, part)| part.depth)
        .map(|(index, _)| index)?;
    Some(HeldPart {
        owner: owner.clone(),
        hash,
        kind: kind.to_string(),
        parts: Arc::clone(sections),
        index,
        id: part_id(owner.id(), sections, index),
    })
}

/// The row a search writes for one section: its address says which text it
/// is part of.
pub fn search_row(part: &HeldPart) -> Value {
    let level = if part.has_parts() {
        Level::Parts
    } else {
        Level::Text
    };
    json!({
        "entity_id": part.id,
        "name": part.part().title,
        "module": part.module(),
        "kind": PART_KIND,
        super::HELD_AS_KEY: level.word(),
    })
}

/// Each of `several` by an address that reads back as it alone: its label,
/// its label with the titles that lead to it, or its id.
pub fn addresses(several: &[HeldPart]) -> Vec<String> {
    let count = |address: &str, of: fn(&HeldPart) -> String| {
        several.iter().filter(|part| of(part) == address).count()
    };
    several
        .iter()
        .map(|part| {
            let label = part.label();
            if count(&label, HeldPart::label) == 1 {
                return format!("`{label}`");
            }
            let path = part.path_label();
            if count(&path, HeldPart::path_label) == 1 {
                return format!("`{path}`");
            }
            format!("`{label}` ({})", part.id)
        })
        .collect()
}

/// The refusal for an address several sections answer to.
pub fn several_named(written: &str, several: &[HeldPart]) -> String {
    format!(
        "`{written}` names {} sections, and this call takes one. Pass one of: {}.",
        several.len(),
        addresses(several).join(", ")
    )
}

/// The rows [`PARTS_KEY`] lists: every section inside the one at `within`,
/// or every section of the text when it is `None`, with spans from the start
/// of the body that is served.
pub fn rows(owner: ArtifactId, parts: &[Part], within: Option<usize>) -> Value {
    let ids = part_ids(owner, parts);
    let inside = |index: usize| {
        let mut at = parts[index].parent;
        while let Some(here) = at {
            if Some(here) == within {
                return true;
            }
            at = parts[here].parent;
        }
        within.is_none()
    };
    let origin = within.map_or(0, |index| parts[index].start);
    let kept: Vec<usize> = (0..parts.len()).filter(|index| inside(*index)).collect();
    let row_of = |index: usize| kept.iter().position(|kept| *kept == index);
    Value::Array(
        kept.iter()
            .map(|index| {
                let part = &parts[*index];
                json!({
                    "entity_id": ids[*index],
                    "name": part.title,
                    "start": part.start - origin,
                    "end": part.end - origin,
                    "parent": part.parent.and_then(row_of),
                })
            })
            .collect(),
    )
}

/// The `source` reply for one section: its text, the base a patch to it
/// carries back, the entity it is part of, and its own sections for the
/// presentation that outlines a long one.
pub(super) fn source_reply<G: GraphStore>(
    held: &HeldSourceAuthority<'_, G>,
    sample: &AuthorityHeadReadSample,
    part: &HeldPart,
) -> Result<Value> {
    let content = held.tree_content(part.hash)?;
    let section = part.part();
    let body = content
        .get(section.start..section.end)
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .ok_or_else(|| McpError::Other(format!("{} is not text", part.label())))?;
    let level = if part.has_parts() {
        Level::Parts
    } else {
        Level::Text
    };
    let mut reply = Map::new();
    reply.insert("id".into(), json!(part.id));
    reply.insert("name".into(), json!(section.title));
    reply.insert("module".into(), json!(part.module()));
    reply.insert("kind".into(), json!(PART_KIND));
    reply.insert(super::HELD_AS_KEY.into(), json!(level.word()));
    reply.insert(PART_OF_KEY.into(), json!(part.owner.label()));
    reply.insert(super::PROJECTED_AT_KEY.into(), json!(part.owner.place()));
    reply.insert("source".into(), json!("graph"));
    if let Some(context) = sample.source_base_context.clone() {
        let base = EntitySourceBase {
            schema: SourceBaseSchema::V1,
            context,
            entity_id: kin_model::EntityId(part.id),
            artifact_id: part.owner.id(),
            source_blob_hash: part.hash.to_string(),
            start_byte: section.start,
            end_byte: section.end,
            body_hash: kin_blobs::digest(body.as_bytes()).to_string(),
        };
        base.validate().map_err(McpError::Context)?;
        reply.insert("source_base".into(), json!(base));
    }
    reply.insert("body".into(), json!(body));
    // The code it names, by name: the edges from prose to code.
    if let Some(reader) = reader_for(&part.kind) {
        let named = names(held.store(), reader, body);
        if !named.is_empty() {
            reply.insert(NAMES_KEY.into(), json!(named));
        }
    }
    if part.has_parts() {
        reply.insert(
            PARTS_KEY.into(),
            rows(part.owner.id(), &part.parts, Some(part.index)),
        );
    }
    reply.insert(
        super::REPLY_KEY.into(),
        json!({"level": level.word(), "bytes": body.len()}),
    );
    super::stamp(&mut reply, sample);
    Ok(Value::Object(reply))
}

/// The key a section's reply lists the code it names under.
pub const NAMES_KEY: &str = "names";

/// The most code entities one section's reply lists.
const NAMES_MAX: usize = 20;

/// A code span as the name it gives: without a leading `~` or `!`, a call's
/// parentheses, or anything after a space.
fn as_name(span: &str) -> Option<&str> {
    let span = span.trim().trim_start_matches(['~', '!']);
    let span = span.split(['(', ' ']).next().unwrap_or_default();
    let span = span.trim_end_matches(['.', ':', ',']);
    let mut chars = span.chars();
    let first = chars.next()?;
    ((first.is_alphabetic() || first == '_')
        && span
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '_' | '.' | ':')))
    .then_some(span)
}

/// The code entities `text` names in its code spans, each once, the one
/// entity each name settles on, with the name that settled it. A name that
/// settles on several entities or none adds no row. These are matches by
/// name, not proven references.
pub fn names<G: GraphStore>(store: &G, reader: &dyn PartReader, text: &str) -> Vec<Value> {
    use kin_ranking::entity_ranking::{resolve_name_strictly, StrictNameResolution};
    let mut seen = std::collections::BTreeSet::new();
    let mut rows = Vec::new();
    for span in reader.code_spans(text) {
        let Some(name) = as_name(span) else {
            continue;
        };
        if !seen.insert(name.to_string()) {
            continue;
        }
        let Ok(StrictNameResolution::One(entity)) = resolve_name_strictly(store, name) else {
            continue;
        };
        if rows
            .iter()
            .any(|row: &Value| row["entity_id"] == json!(entity.id.to_string()))
        {
            continue;
        }
        let mut row = json!({
            "entity_id": entity.id.to_string(),
            "name": entity.name,
            "kind": format!("{:?}", entity.kind),
            super::mentions::MATCHED_BY_KEY: name,
        });
        if let Some(module) = crate::semantic_owners::entity_module(store, &entity) {
            row["module"] = json!(module);
        }
        rows.push(row);
        if rows.len() >= NAMES_MAX {
            break;
        }
    }
    rows
}

/// The note an outline of a text held with parts carries.
pub const OUTLINE_NOTE: &str = "A text held with parts and longer than 4 KB is served as its \
outline: the text before its first section, and its sections by id. Read the section you need \
by its id; each is read and patched by itself.";

/// Why an outline carries no source_base.
pub const OUTLINE_BASE_UNAVAILABLE: &str =
    "section_outline: this reply serves the sections of the text \
by id and none of their text, so it is the base of no edit. Read a section by its id for its \
text and source_base.";

/// Serve a source reply that lists its sections ([`PARTS_KEY`]) the way a
/// text held with parts is read: whole when it is at most 4 KB, or when its
/// whole body is asked for and it is at most
/// [`crate::source_pages::FULL_BODY_MAX_BYTES`]; otherwise as its outline,
/// the text before its first section and its sections by id. Returns whether
/// the reply changed, or the refusal for a cursor, since such a text is read
/// by section and never in excerpts of the whole.
pub fn present(
    reply: &mut Map<String, Value>,
    full_body: bool,
    cursor: bool,
) -> std::result::Result<bool, String> {
    let Some(Value::Array(rows)) = reply.remove(PARTS_KEY) else {
        return Ok(false);
    };
    let Some(body) = reply.get("body").and_then(Value::as_str).map(str::to_owned) else {
        return Ok(true);
    };
    let name = reply
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("this text")
        .to_string();
    if cursor {
        return Err(format!(
            "source_cursor_invalid: {name} is held with parts and is read by section, never in \
             excerpts of the whole. Call source on {name} for its sections by id, and read the \
             one you need by its id."
        ));
    }
    let whole = body.len() <= crate::container_shape::CONTAINER_INLINE_MAX_BYTES
        || (full_body && body.len() <= crate::source_pages::FULL_BODY_MAX_BYTES);
    if whole {
        return Ok(true);
    }
    let listed = listed(&rows, body.len());
    let first = listed
        .first()
        .and_then(|row| row.get("start"))
        .and_then(Value::as_u64)
        .map_or(body.len(), |start| start as usize);
    let (header, complete) = header(&body[..first.min(body.len())]);
    let children = |row: &Value| {
        let at = rows.iter().position(|other| std::ptr::eq(other, row));
        rows.iter()
            .filter(|other| {
                other
                    .get("parent")
                    .and_then(Value::as_u64)
                    .map(|p| p as usize)
                    == at
                    && at.is_some()
            })
            .count()
    };
    let members: Vec<Value> = listed
        .iter()
        .take(LISTED_MAX)
        .map(|row| {
            let mut member = json!({
                "id": row["entity_id"],
                "name": row["name"],
                "kind": PART_KIND,
            });
            let inside = children(row);
            if inside > 0 {
                member["sections"] = json!(inside);
            }
            member
        })
        .collect();
    reply.remove("body");
    reply.remove("source_base");
    reply.insert(
        "projection".into(),
        json!(crate::container_shape::SHAPE_PROJECTION),
    );
    reply.insert("header".into(), json!(header));
    reply.insert("header_complete".into(), json!(complete));
    reply.insert("members".into(), json!(members));
    if listed.len() > LISTED_MAX {
        reply.insert("members_withheld".into(), json!(listed.len() - LISTED_MAX));
    }
    reply.insert("body_bytes".into(), json!(body.len()));
    reply.insert(
        "body_omitted".into(),
        json!(crate::container_shape::CONTAINER_SHAPE_REASON),
    );
    reply.insert(
        "source_base_unavailable".into(),
        json!(OUTLINE_BASE_UNAVAILABLE),
    );
    let note = if full_body {
        format!(
            "full_body serves a text of at most {} KB whole, and this one is {} KB. {OUTLINE_NOTE}",
            crate::source_pages::FULL_BODY_MAX_BYTES / 1024,
            body.len().div_ceil(1024)
        )
    } else if listed.len() > LISTED_MAX {
        format!(
            "{OUTLINE_NOTE} The first {LISTED_MAX} of its {} sections are listed; find another \
             with locate by its title.",
            listed.len()
        )
    } else {
        OUTLINE_NOTE.to_string()
    };
    reply.insert("note".into(), json!(note));
    Ok(true)
}

/// The sections an outline lists: those directly inside what is served, or,
/// where one section holds all the rest from the top, those inside it.
fn listed(rows: &[Value], body_len: usize) -> Vec<&Value> {
    let parent = |row: &Value| {
        row.get("parent")
            .and_then(Value::as_u64)
            .map(|p| p as usize)
    };
    let mut within: Option<usize> = None;
    loop {
        let inside: Vec<usize> = (0..rows.len())
            .filter(|index| parent(&rows[*index]) == within)
            .collect();
        match inside.as_slice() {
            [only]
                if rows[*only].get("end").and_then(Value::as_u64) == Some(body_len as u64)
                    && rows.iter().any(|row| parent(row) == Some(*only)) =>
            {
                within = Some(*only);
            }
            _ => return inside.into_iter().map(|index| &rows[index]).collect(),
        }
    }
}

/// The text before the first section listed, cut at a line end to the bound
/// a container's header has, and whether it is whole.
fn header(text: &str) -> (String, bool) {
    let max = crate::container_shape::CONTAINER_HEADER_MAX_BYTES;
    if text.len() <= max {
        return (text.trim_end().to_string(), true);
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let end = text[..end].rfind('\n').map_or(end, |at| at + 1);
    (text[..end].trim_end().to_string(), false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn titled(reader: &dyn PartReader, text: &str) -> Vec<(String, usize, Option<usize>)> {
        parts(reader, text)
            .into_iter()
            .map(|part| (part.title, part.depth, part.parent))
            .collect()
    }

    fn spans(reader: &dyn PartReader, text: &str) -> Vec<String> {
        parts(reader, text)
            .into_iter()
            .map(|part| text[part.start..part.end].to_string())
            .collect()
    }

    const CHANGES: &str = "Changes\n=======\n\nUnreleased\n----------\n\n- Add a thing.\n\n\
Version 3.1.0\n-------------\n\nReleased 2024-11-13\n\n- Drop Python 3.8.\n";

    /// A changelog's versions are parts of its title, each closed at the
    /// next version, and the spans of the outermost cover the text.
    #[test]
    fn a_changelog_is_read_as_its_versions() {
        assert_eq!(
            titled(&ReStructuredText, CHANGES),
            [
                ("Changes".to_string(), 1, None),
                ("Unreleased".to_string(), 2, Some(0)),
                ("Version 3.1.0".to_string(), 2, Some(0)),
            ]
        );
        let spans = spans(&ReStructuredText, CHANGES);
        assert_eq!(spans[0], CHANGES);
        assert_eq!(spans[1], "Unreleased\n----------\n\n- Add a thing.\n\n");
        assert!(spans[2].starts_with("Version 3.1.0\n-------------\n"));
        assert!(spans[2].ends_with("- Drop Python 3.8.\n"));
        let parts = parts(&ReStructuredText, CHANGES);
        assert_eq!(
            &CHANGES[parts[1].body_start..parts[1].end],
            "\n- Add a thing.\n\n"
        );
    }

    /// An overlined title is its own style, a short underline is no title,
    /// and a transition between blank lines is none either.
    #[test]
    fn restructuredtext_styles_rank_by_first_appearance() {
        let text = "=====\nGuide\n=====\n\nIntro text.\n\nInstall\n=======\n\nSteps.\n\n\
Short\n==\n\n----\n\nUsage\n=======\n";
        assert_eq!(
            titled(&ReStructuredText, text),
            [
                ("Guide".to_string(), 1, None),
                ("Install".to_string(), 2, Some(0)),
                ("Usage".to_string(), 2, Some(0)),
            ]
        );
    }

    /// Markdown's `#` headings and underlined forms are parts; a `#` inside
    /// fenced code, a front matter rule and a list over a rule are not.
    #[test]
    fn markdown_headings_skip_code_and_front_matter() {
        let text = "---\ntitle: Guide\n---\n# Guide\n\nIntro.\n\n## Install ##\n\n```sh\n\
# not a heading\npip install shop\n```\n\nUsage\n-----\n\n- one\n- two\n---\n\n### Notes\n\n#hashtag\n";
        assert_eq!(
            titled(&Markdown, text),
            [
                ("Guide".to_string(), 1, None),
                ("Install".to_string(), 2, Some(0)),
                ("Usage".to_string(), 2, Some(0)),
                ("Notes".to_string(), 3, Some(2)),
            ]
        );
        let parts = parts(&Markdown, text);
        assert!(text[parts[1].start..parts[1].end].contains("# not a heading"));
        assert_eq!(parts[0].end, text.len());
    }

    /// A part's id holds while its text changes, differs for the same title
    /// under another parent and for a repeated title, and follows its title.
    #[test]
    fn a_part_id_follows_its_titles_and_not_its_text() {
        let owner = ArtifactId(uuid::Uuid::from_u128(7));
        let text = "# A\n\n## Fixes\n\none\n\n# B\n\n## Fixes\n\ntwo\n\n## Fixes\n\nthree\n";
        let read = parts(&Markdown, text);
        let ids: Vec<_> = (0..read.len())
            .map(|at| part_id(owner, &read, at))
            .collect();
        let unique: std::collections::BTreeSet<_> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len(), "{ids:?}");

        let edited = text.replace("two", "two, longer now");
        let again = parts(&Markdown, &edited);
        let same: Vec<_> = (0..again.len())
            .map(|at| part_id(owner, &again, at))
            .collect();
        assert_eq!(same, ids);

        let retitled = parts(&Markdown, &text.replace("# B", "# C"));
        assert_ne!(part_id(owner, &retitled, 2), ids[2]);
        assert_eq!(part_id(owner, &retitled, 1), ids[1]);
        let other = ArtifactId(uuid::Uuid::from_u128(8));
        assert_ne!(part_id(other, &read, 1), ids[1]);
    }

    /// The reader follows the kind, and parts are kept by digest.
    #[test]
    fn parts_are_read_by_kind_and_kept_by_digest() {
        assert!(reader_for("Markdown").is_some());
        assert!(reader_for("reStructuredText").is_some());
        assert!(reader_for("licence").is_none());
        let text = |text: &str| {
            let text = Arc::new(text.as_bytes().to_vec());
            move || Some(text)
        };
        let hash = Hash256::from_bytes([3; 32]);
        let first = parts_of("Markdown", hash, text("# One\n")).unwrap();
        let kept = parts_of("Markdown", hash, || panic!("the digest is kept")).unwrap();
        assert!(Arc::ptr_eq(&first, &kept));
        let none = parts_of("Markdown", Hash256::from_bytes([4; 32]), text("no heading"));
        assert_eq!(none.unwrap().len(), 0);
        assert!(parts_of("licence", hash, text("# One\n")).is_none());
    }

    /// A reply for `text` as the handler answers one: its body, a base, and
    /// its sections.
    fn reply(text: &str) -> Map<String, Value> {
        let owner = ArtifactId(uuid::Uuid::from_u128(9));
        let read = parts(&ReStructuredText, text);
        json!({
            "id": owner.0, "name": "HISTORY.rst", "held_as": "parts", "body": text,
            "source_base": {"entity_id": owner.0}, PARTS_KEY: rows(owner, &read, None),
        })
        .as_object()
        .unwrap()
        .clone()
    }

    fn history(entries: usize) -> String {
        let mut text = String::from(
            "History\n=======\n\nVersion 2\n---------\n\n- new\n\nVersion 1\n---------\n\n",
        );
        for entry in 0..entries {
            text.push_str(&format!(
                "- Entry {entry:03}: one line of the first release.\n"
            ));
        }
        text
    }

    /// A long text with parts is served as its outline: the text before its
    /// first section, its sections by id, and no text or base. The title
    /// that holds every section is looked through.
    #[test]
    fn a_long_text_with_parts_is_served_as_its_outline() {
        let text = history(400);
        assert!(text.len() > crate::source_pages::FULL_BODY_MAX_BYTES);
        let mut served = reply(&text);
        assert_eq!(present(&mut served, false, false), Ok(true));
        assert!(served.get("body").is_none());
        assert!(served.get("source_base").is_none());
        assert!(served.get(PARTS_KEY).is_none());
        assert_eq!(served["header"], "History\n=======");
        assert_eq!(served["header_complete"], true);
        let names: Vec<&str> = served["members"]
            .as_array()
            .unwrap()
            .iter()
            .map(|member| member["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["Version 2", "Version 1"]);
        assert_eq!(served["body_bytes"], text.len());
        assert_eq!(served["note"], OUTLINE_NOTE);

        // Asked for whole, it is still an outline past the bound, and says why.
        let mut asked = reply(&text);
        assert_eq!(present(&mut asked, true, false), Ok(true));
        assert!(asked["note"]
            .as_str()
            .unwrap()
            .starts_with("full_body serves a text of at most 16 KB"));
        // Read in excerpts of the whole, it is refused.
        let refused = present(&mut reply(&text), false, true).unwrap_err();
        assert!(refused.contains("is read by section"), "{refused}");
    }

    /// A short text with parts, or one asked for whole within the bound, is
    /// served whole with its base.
    #[test]
    fn a_short_text_with_parts_is_served_whole() {
        for (text, full_body) in [(history(10), false), (history(120), true)] {
            let mut served = reply(&text);
            assert_eq!(present(&mut served, full_body, false), Ok(true));
            assert_eq!(served["body"], text);
            assert!(served["source_base"].is_object());
            assert!(served.get(PARTS_KEY).is_none());
        }
        let mut plain = json!({"body": "x"}).as_object().unwrap().clone();
        assert_eq!(present(&mut plain, false, false), Ok(false));
    }

    /// Code spans are read as the names they give, in both formats.
    #[test]
    fn code_spans_give_the_names_prose_names_code_by() {
        let rst =
            "Call :func:`~shop.db.init_db` and ``schema_version()``, see :ref:`the guide <guide>`.";
        let spans: Vec<_> = ReStructuredText
            .code_spans(rst)
            .into_iter()
            .filter_map(as_name)
            .collect();
        assert_eq!(spans, ["schema_version", "shop.db.init_db", "guide"]);
        let md = "Use `init_db(path)` here.\n```\n`not_this`\n```\n";
        let spans: Vec<_> = Markdown
            .code_spans(md)
            .into_iter()
            .filter_map(as_name)
            .collect();
        assert_eq!(spans, ["init_db"]);
    }

    /// A part id is told from any other by itself.
    #[test]
    fn a_part_id_is_told_from_other_ids() {
        let read = parts(&Markdown, "# A\n");
        let id = part_id(ArtifactId(uuid::Uuid::from_u128(7)), &read, 0);
        assert!(is_part_id(&id));
        assert!(!is_part_id(&uuid::Uuid::new_v4()));
        assert!(!is_part_id(&uuid::Uuid::from_u128(7)));
    }
}
