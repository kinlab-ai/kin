// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! What names an unparsed entity in its text: the first loose connections.
//!
//! A schema is opened by the code that names `"schema.sql"`, a logo is shown
//! by the template that names `static/logo.png`, and a licence is pointed at
//! by the readme. None of that is a resolved reference: Kin has not proved
//! what any of those strings is used for, and a name built while code runs,
//! or a pattern such as `static/*.png`, is not followed. So a mention is
//! reported as what it is, a match by name, and an answer made of them is
//! never complete.
//!
//! A token names an entity when it is the entity's own name, or a path-shaped
//! token whose last part is that name and whose earlier parts end the place
//! the entity is projected in: `static/logo.png` names the logo under
//! `static` and no other. A token in code is credited to the innermost entity
//! whose span holds it, or to its module; a token in an unparsed entity held
//! as text is credited to that entity. An entity's own text does not count.
//!
//! Every token is read from content the graph holds, and the tokens of one
//! content are kept by its digest for every later question, within a bound.
//! Nothing is stored.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use kin_model::graph::{EntityFilter, GraphStore};
use kin_model::{Entity, FilePathId, Hash256, ResolvedTree, TreeEntry};
use serde_json::{json, Map, Value};

use super::{ArtifactEntity, Held, Level, Named};
use crate::handlers::common::{EntitySourceScope, HeldSourceAuthority};

/// The key the rows that name an entity are listed under.
pub const NAMED_IN_KEY: &str = "named_in";

/// The key that says how every row of a reply names its entity.
pub const MATCHED_BY_KEY: &str = "matched_by";

/// What every row was matched by.
pub const BY_NAME: &str = "name";

/// The most content one question reads for names, whole.
const CONTENT_READ_MAX_BYTES: usize = 4 * 1024 * 1024;

/// The most rows one reply lists.
const ROWS_MAX: usize = 50;

/// The most characters of a line a row quotes.
const LINE_SHOWN: usize = 160;

/// The most bytes of tokens kept across questions before the store is
/// emptied and filled again.
const KEPT_TOKEN_BYTES_MAX: usize = 32 * 1024 * 1024;

/// One path-shaped token and where it begins in its content.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Token {
    text: Box<str>,
    at: u32,
}

/// The tokens of each content read so far, by digest, and their bytes.
type Kept = (HashMap<Hash256, Arc<Vec<Token>>>, usize);

fn kept() -> &'static Mutex<Kept> {
    static KEPT: OnceLock<Mutex<Kept>> = OnceLock::new();
    KEPT.get_or_init(|| Mutex::new((HashMap::new(), 0)))
}

fn name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | '@' | '+')
}

/// The tokens of `text` that could name an entity: runs of name characters
/// that hold a dot or a separator, or that hold a capital, at least four
/// characters long (`LICENSE`, `Dockerfile`).
fn tokens(text: &str) -> Vec<Token> {
    let mut found = Vec::new();
    let mut start = None;
    for (at, c) in text
        .char_indices()
        .chain(std::iter::once((text.len(), ' ')))
    {
        match (name_char(c), start) {
            (true, None) => start = Some(at),
            (false, Some(from)) => {
                start = None;
                let run = &text[from..at];
                let trimmed = run
                    .trim_start_matches("./")
                    .trim_start_matches('/')
                    .trim_end_matches(['.', '-', '/']);
                if trimmed.is_empty() {
                    continue;
                }
                let shaped = trimmed.contains(['.', '/'])
                    || (trimmed.len() >= 4 && trimmed.chars().any(|c| c.is_ascii_uppercase()));
                if shaped {
                    let offset = from + run.find(trimmed).unwrap_or(0);
                    found.push(Token {
                        text: trimmed.into(),
                        at: offset as u32,
                    });
                }
            }
            _ => {}
        }
    }
    found
}

/// The tokens of the content `hash` names, read once and kept: `None` when
/// the content cannot be read or is too large to read whole, and no tokens
/// for content held as bytes.
fn tokens_of<G: GraphStore>(
    held: &HeldSourceAuthority<'_, G>,
    hash: Hash256,
) -> Option<Arc<Vec<Token>>> {
    if let Some(found) = kept()
        .lock()
        .expect("mention tokens are never poisoned")
        .0
        .get(&hash)
    {
        return Some(Arc::clone(found));
    }
    let content = held.tree_content(hash).ok()?;
    if content.len() > CONTENT_READ_MAX_BYTES {
        return None;
    }
    let found = Arc::new(match (Level::of(&content), std::str::from_utf8(&content)) {
        (Level::Text, Ok(text)) => tokens(text),
        _ => Vec::new(),
    });
    let bytes: usize = found.iter().map(|token| token.text.len() + 16).sum();
    let mut store = kept().lock().expect("mention tokens are never poisoned");
    if store.1 + bytes > KEPT_TOKEN_BYTES_MAX {
        store.0.clear();
        store.1 = 0;
    }
    store.1 += bytes;
    store.0.insert(hash, Arc::clone(&found));
    Some(found)
}

/// Whether `token` names the entity called `name` projected in `dir`, the
/// parts of the place it is projected in above it.
fn names(token: &str, name: &str, dir: &[&str]) -> bool {
    if token == name {
        return true;
    }
    let Some(before) = token
        .strip_suffix(name)
        .and_then(|rest| rest.strip_suffix('/'))
    else {
        return false;
    };
    let parts: Vec<&str> = before.split('/').collect();
    !parts.iter().any(|part| part.is_empty()) && dir.ends_with(&parts)
}

/// The line of `text` that holds byte `at`, trimmed and cut to a quote.
fn line_at(text: &str, at: usize) -> String {
    let at = at.min(text.len());
    let start = text[..at].rfind('\n').map_or(0, |found| found + 1);
    let end = text[at..].find('\n').map_or(text.len(), |found| at + found);
    let line = text[start..end].trim();
    let mut shown: String = line.chars().take(LINE_SHOWN).collect();
    if shown.len() < line.len() {
        shown.push_str(" ...");
    }
    shown
}

/// The entity of code that holds byte `at` of the source projected at
/// `place`: the innermost with a span that holds it, or its module.
fn holder_in_code<G: GraphStore>(store: &G, place: &str, at: usize) -> Option<Entity> {
    let entities = store
        .query_entities(&EntityFilter {
            file_path: Some(FilePathId::new(place)),
            ..Default::default()
        })
        .ok()?;
    let innermost = entities
        .iter()
        .filter(|entity| !kin_model::is_file_module_surface(entity))
        .filter(|entity| {
            entity
                .span
                .as_ref()
                .is_some_and(|span| span.start_byte <= at && at < span.end_byte)
        })
        .min_by_key(|entity| {
            entity
                .span
                .as_ref()
                .map_or(usize::MAX, |span| span.end_byte - span.start_byte)
        });
    innermost
        .or_else(|| {
            entities
                .iter()
                .find(|entity| kin_model::serves_module_code(entity))
        })
        .or_else(|| {
            entities
                .iter()
                .find(|entity| kin_model::is_file_module_surface(entity))
        })
        .cloned()
}

/// The most rows a description lists of what names its entity; `refs`
/// lists the rest.
pub const DESCRIPTION_ROWS_MAX: usize = 5;

/// Everything that names `target` in the tree `tree` holds, as reply rows,
/// how many holders there are in all, and how many contents could not be
/// read for names.
pub(crate) fn named_in<G: GraphStore>(
    held: &HeldSourceAuthority<'_, G>,
    members: &Held<'_>,
    tree: &ResolvedTree,
    target: &ArtifactEntity,
) -> (Vec<Value>, usize, usize) {
    let place = target.place();
    let dir: Vec<&str> = place
        .rsplit_once('/')
        .map_or(Vec::new(), |(dir, _)| dir.split('/').collect());
    let mut rows: Vec<(String, Value)> = Vec::new();
    let mut counts: HashMap<String, usize> = HashMap::new();
    let mut unread = 0;
    for artifact in tree.artifacts_by_path() {
        if artifact.artifact_id == target.id() {
            continue;
        }
        let TreeEntry::Blob { hash, .. } = artifact.entry else {
            continue;
        };
        let Some(found) = tokens_of(held, hash) else {
            unread += 1;
            continue;
        };
        let hits: Vec<&Token> = found
            .iter()
            .filter(|token| names(&token.text, &target.name, &dir))
            .collect();
        if hits.is_empty() {
            continue;
        }
        let Ok(content) = held.tree_content(hash) else {
            unread += 1;
            continue;
        };
        let text = String::from_utf8_lossy(&content);
        let holder_place = String::from_utf8_lossy(artifact.path.as_bytes()).into_owned();
        for token in hits {
            let at = token.at as usize;
            let row = if super::is_code(&artifact.path) {
                let Some(entity) = holder_in_code(held.store(), &holder_place, at) else {
                    continue;
                };
                let mut row = json!({
                    "entity_id": entity.id,
                    "name": entity.name,
                    "kind": entity.kind,
                });
                if let Some(module) = crate::semantic_owners::entity_module(held.store(), &entity) {
                    row["module"] = json!(module);
                }
                row
            } else {
                let holder = members.placed(artifact);
                let kind = super::kind::describe(
                    &holder_place,
                    &holder.name,
                    true,
                    &content[..content.len().min(4096)],
                )
                .kind;
                // In a text held with parts, the innermost section that holds
                // the name, as code credits the innermost entity.
                let kept = Arc::clone(&content);
                let section =
                    super::parts::parts_of(&kind, hash, move || Some(kept)).and_then(|sections| {
                        super::parts::section_at(&holder, hash, &kind, &sections, at)
                    });
                match section {
                    Some(section) => super::parts::search_row(&section),
                    None => {
                        let mut row = json!({
                            "entity_id": holder.id().0,
                            "name": holder.name,
                            "kind": kind,
                            super::HELD_AS_KEY: Level::Text.word(),
                        });
                        if let Some(module) = &holder.module {
                            row["module"] = json!(module);
                        }
                        row
                    }
                }
            };
            let id = row["entity_id"].to_string();
            let seen = counts.entry(id.clone()).or_insert(0);
            *seen += 1;
            if *seen == 1 {
                let mut row = row;
                row["code"] = json!(line_at(&text, at));
                rows.push((id, row));
            }
        }
    }
    for (id, row) in &mut rows {
        let times = counts.get(id).copied().unwrap_or(1);
        if times > 1 {
            row["times"] = json!(times);
        }
    }
    let total = rows.len();
    let rows = rows
        .into_iter()
        .take(ROWS_MAX)
        .map(|(_, row)| row)
        .collect();
    (rows, total, unread)
}

/// The `refs` reply for the unparsed entity `args` names by `entity_id` or
/// `query`, or `None` when it names something else, which the reference
/// read of entities answers as before: an entity of code, a name a
/// declaration carries, or nothing Kin holds.
pub fn refs_reply<G: GraphStore>(
    held: &HeldSourceAuthority<'_, G>,
    args: &HashMap<String, Value>,
    scope: EntitySourceScope,
) -> Option<Value> {
    if scope != EntitySourceScope::WorkspaceHead {
        return None;
    }
    let (written, by_name) = match (args.get("entity_id"), args.get("query")) {
        (Some(Value::String(id)), _) => (id.trim().to_string(), false),
        (None, Some(Value::String(query))) => (query.trim().to_string(), true),
        _ => return None,
    };
    if written.is_empty() {
        return None;
    }
    // An id or name an entity of the graph answers to is that entity's.
    if let Ok(id) = crate::handlers::common::parse_entity_id(&written) {
        if held.store().get_entity(&id).ok().flatten().is_some() {
            return None;
        }
    }
    if by_name
        && !matches!(
            kin_ranking::entity_ranking::resolve_name_strictly(held.store(), &written),
            Ok(kin_ranking::entity_ranking::StrictNameResolution::Missing)
        )
    {
        return None;
    }
    let sample = held.workspace_sample().ok()?;
    let members = Held::load(
        held.store(),
        &sample.tree,
        super::root_name(held, &sample.tree),
    )
    .ok()?;
    let Named::One(target) = members.named(&written) else {
        return None;
    };
    let (kind, level) = match target.artifact.entry {
        TreeEntry::Blob { hash, .. } => {
            let content = held.tree_content(hash).ok()?;
            let level = Level::of(&content);
            let kind = super::kind::describe(
                &target.place(),
                &target.name,
                level == Level::Text,
                &content[..content.len().min(4096)],
            )
            .kind;
            (kind, level)
        }
        _ => ("link".to_string(), Level::Bytes),
    };
    let (rows, total, unread) = named_in(held, &members, &sample.tree, &target);
    let mut entity = Map::new();
    entity.insert("entity_id".into(), json!(target.id().0));
    entity.insert("name".into(), json!(target.name));
    if let Some(module) = &target.module {
        entity.insert("module".into(), json!(module));
    }
    entity.insert("kind".into(), json!(kind));
    entity.insert(super::HELD_AS_KEY.into(), json!(level.word()));
    let mut reply = json!({
        "entity": entity,
        NAMED_IN_KEY: rows,
        MATCHED_BY_KEY: BY_NAME,
        "named_in_total": total,
    });
    if total > ROWS_MAX {
        reply["named_in_withheld"] = json!(total - ROWS_MAX);
    }
    if unread > 0 {
        reply["named_in_unread"] = json!(unread);
    }
    Some(reply)
}

/// Whether a machine `refs` reply lists what names an unparsed entity.
pub fn is_named_in_reply(reply: &Map<String, Value>) -> bool {
    reply.get(MATCHED_BY_KEY).and_then(Value::as_str) == Some(BY_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(found: &[Token]) -> Vec<&str> {
        found.iter().map(|token| &*token.text).collect()
    }

    #[test]
    fn a_token_is_a_run_shaped_like_a_name_or_a_place() {
        let code = "with resources.files(\"shop\").joinpath(\"schema.sql\").open():\n    \
                    render_template(\"index.html\", logo=\"./static/logo.png\")\n";
        assert_eq!(
            texts(&tokens(code)),
            // A method called after another reads as a dotted name, and
            // names only an entity so named.
            [
                "resources.files",
                ".joinpath",
                "schema.sql",
                ".open",
                "index.html",
                "static/logo.png"
            ]
        );
        let prose = "See [LICENSE](LICENSE). Load `schema.sql` into the database.";
        assert_eq!(
            texts(&tokens(prose)),
            // A capitalized word of three letters is too short to keep.
            ["LICENSE", "LICENSE", "Load", "schema.sql"]
        );
        let found = tokens(code);
        let logo = found
            .iter()
            .find(|token| &*token.text == "static/logo.png")
            .unwrap();
        assert_eq!(
            &code[logo.at as usize..logo.at as usize + 15],
            "static/logo.png"
        );
    }

    #[test]
    fn a_token_names_an_entity_by_its_name_or_by_the_end_of_its_place() {
        let dir = ["src", "shop", "static"];
        assert!(names("logo.png", "logo.png", &dir));
        assert!(names("static/logo.png", "logo.png", &dir));
        assert!(names("shop/static/logo.png", "logo.png", &dir));
        assert!(!names("docs/logo.png", "logo.png", &dir));
        assert!(!names("logo.png.bak", "logo.png", &dir));
        assert!(!names("big-logo.png", "logo.png", &dir));
        assert!(!names("static//logo.png", "logo.png", &dir));
        assert!(names("LICENSE", "LICENSE", &[]));
        assert!(!names("docs/LICENSE", "LICENSE", &[]));
    }

    #[test]
    fn a_quoted_line_is_the_line_that_holds_the_name_trimmed() {
        let text = "one\n    load `schema.sql` here\nthree";
        let at = text.find("schema").unwrap();
        assert_eq!(line_at(text, at), "load `schema.sql` here");
        let long = format!("{}schema.sql", "x".repeat(400));
        assert!(line_at(&long, 400).ends_with(" ..."));
    }
}
