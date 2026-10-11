// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! A Rust source read as named items, and the text a key takes from it.
//!
//! A key reads a deciding Rust file as its tokens, without comments or the
//! whitespace between tokens: a comment, a doc comment or a formatting pass
//! changes nothing Kin derives, so none of them moves a key. String literals
//! are tokens and stay whole.
//!
//! Some deciding files also hold code no derivation runs, such as a hook only
//! a reader calls. A classification may name such items to leave out of the
//! key, each with a reason. Everything a classification does not name is
//! kept, so a new function defaults to deciding. An item is named by its
//! path: a top-level item by its kind and name (`fn parse_module`), an
//! `impl` by its header (`impl LanguageAdapter for PythonAdapter`), and a
//! member of an `impl`, `trait` or `mod` by the block's path, a slash and its
//! own (`impl LanguageAdapter for PythonAdapter / fn name_syntax`).
//!
//! Items under `#[cfg(test)]`, at any depth this reads, are always left out.

use std::collections::BTreeSet;

use crate::rust_items::{attribute, skip_blank, skip_token};

/// One item of a Rust source.
#[derive(Debug, Clone)]
pub struct RustItem {
    /// Its path, as a classification names it.
    pub path: String,
    /// The name it declares, when it declares one.
    pub name: Option<String>,
    /// Its members, when it is an `impl`, `trait` or `mod` with a body.
    pub members: Vec<RustItem>,
    start: usize,
    end: usize,
    test_only: bool,
    /// Whether an attribute is written on it.
    attributed: bool,
    /// The braces of the body that holds its members.
    body: Option<(usize, usize)>,
}

impl RustItem {
    /// Whether it compiles only under `#[cfg(test)]`.
    pub fn is_test_only(&self) -> bool {
        self.test_only
    }
}

/// The text a key reads from a Rust source, with what it left out.
#[derive(Debug, Clone, Default)]
pub struct Kept {
    /// The kept tokens, one space after each.
    pub text: String,
    /// Every identifier among the kept tokens.
    pub identifiers: BTreeSet<String>,
    /// Each excluded item's path, with every name it and its members declare.
    pub excluded: Vec<(String, Vec<String>)>,
    /// The name of every trait the source declares, kept or not.
    pub traits: BTreeSet<String>,
}

/// The items of `source`, top-level first, with their members.
pub fn items(source: &str) -> Result<Vec<RustItem>, String> {
    items_in(source, 0, source.len(), "")
}

/// The tokens of `source` without its test items or the items `exclude`
/// names. Each path in `exclude` must name exactly one item.
pub fn kept(source: &str, exclude: &[String]) -> Result<Kept, String> {
    let items = items(source)?;
    let mut paths = Vec::new();
    collect_paths(&items, &mut paths);
    for path in exclude {
        match paths.iter().filter(|candidate| *candidate == path).count() {
            1 => {}
            0 => {
                return Err(format!(
                    "holds no item `{path}`; the items it holds are: {}",
                    paths.join(", ")
                ))
            }
            found => return Err(format!("holds {found} items named `{path}`")),
        }
    }
    let exclude: BTreeSet<&str> = exclude.iter().map(String::as_str).collect();
    let mut kept = Kept::default();
    emit(source, &items, &exclude, &mut kept);
    collect_traits(&items, &mut kept.traits);
    Ok(kept)
}

fn collect_traits(items: &[RustItem], traits: &mut BTreeSet<String>) {
    for item in items {
        if item.test_only {
            continue;
        }
        let own = item.path.rsplit(" / ").next().unwrap_or(&item.path);
        if let Some(name) = own.strip_prefix("trait ") {
            traits.insert(name.to_string());
        }
        collect_traits(&item.members, traits);
    }
}

/// The trait an `impl` path segment implements, by its last path segment
/// without generic arguments: `impl kin::Adapter<T> for X` gives `Adapter`.
pub fn implemented_trait(segment: &str) -> Option<&str> {
    let header = segment.strip_prefix("impl ")?;
    // `for` as a word: after a space or a closing angle bracket, and before
    // anything that does not continue a word (`for X`, `for&G`, `for(A, B)`).
    let bytes = header.as_bytes();
    let at = header.match_indices("for").map(|(at, _)| at).find(|&at| {
        let before = at.checked_sub(1).map(|before| bytes[before]);
        let after = bytes.get(at + 3).copied();
        matches!(before, Some(b' ' | b'>'))
            && !after.is_some_and(|after| after == b'_' || after.is_ascii_alphanumeric())
    })?;
    let implemented = header[..at].trim_end();
    let without_arguments = implemented.split('<').next().unwrap_or(implemented);
    Some(
        without_arguments
            .rsplit("::")
            .next()
            .unwrap_or(without_arguments)
            .trim_start_matches('!'),
    )
}

fn collect_paths(items: &[RustItem], paths: &mut Vec<String>) {
    for item in items {
        if item.test_only {
            continue;
        }
        paths.push(item.path.clone());
        collect_paths(&item.members, paths);
    }
}

fn declared_names(item: &RustItem, names: &mut Vec<String>) {
    if let Some(name) = &item.name {
        names.push(name.clone());
    }
    for member in &item.members {
        if !member.test_only {
            declared_names(member, names);
        }
    }
}

fn emit(source: &str, items: &[RustItem], exclude: &BTreeSet<&str>, kept: &mut Kept) {
    for item in items {
        if item.test_only {
            continue;
        }
        if declares_a_file(item) {
            // `mod parse;` only compiles the file named parse, which is
            // classified and keyed as itself. With an attribute (`#[path]`,
            // `#[cfg]`) it can change which code compiles, so it is kept.
            continue;
        }
        if exclude.contains(item.path.as_str()) {
            let mut names = Vec::new();
            declared_names(item, &mut names);
            kept.excluded.push((item.path.clone(), names));
            continue;
        }
        match item.body {
            Some((open, close)) if !item.members.is_empty() || holds_members(&item.path) => {
                push_tokens(source, item.start, open + 1, kept, true);
                emit(source, &item.members, exclude, kept);
                push_tokens(source, close, item.end, kept, true);
            }
            _ => push_tokens(source, item.start, item.end, kept, runs(item)),
        }
    }
}

fn holds_members(path: &str) -> bool {
    let own = path.rsplit(" / ").next().unwrap_or(path);
    own.starts_with("impl") || own.starts_with("trait ") || own.starts_with("mod ")
}

/// A plain `mod name;`: a module whose code is a file of its own.
fn declares_a_file(item: &RustItem) -> bool {
    let own = item.path.rsplit(" / ").next().unwrap_or(&item.path);
    own.starts_with("mod ") && item.body.is_none() && !item.attributed
}

/// Whether an item can run anything. A `use` and a `mod` declaration
/// without a body only bring names into scope: whatever they bring in runs
/// only where some other code names it, so their identifiers name nothing.
fn runs(item: &RustItem) -> bool {
    let own = item.path.rsplit(" / ").next().unwrap_or(&item.path);
    !(own.starts_with("use ") || (own.starts_with("mod ") && item.body.is_none()))
}

fn push_tokens(source: &str, from: usize, to: usize, kept: &mut Kept, names: bool) {
    let mut at = from;
    loop {
        at = skip_blank(source, at);
        if at >= to {
            break;
        }
        let next = skip_token(source, at);
        let token = &source[at..next];
        kept.text.push_str(token);
        kept.text.push(' ');
        if names && is_identifier(token) {
            kept.identifiers.insert(token.to_string());
        }
        at = next;
    }
}

fn is_identifier(token: &str) -> bool {
    token
        .chars()
        .next()
        .is_some_and(|first| first == '_' || first.is_ascii_alphabetic())
        && token.chars().all(|c| c == '_' || c.is_ascii_alphanumeric())
}

fn line_of(source: &str, at: usize) -> usize {
    source[..at].lines().count() + 1
}

fn items_in(source: &str, from: usize, to: usize, prefix: &str) -> Result<Vec<RustItem>, String> {
    let mut items = Vec::new();
    let mut at = from;
    loop {
        let start = skip_blank(source, at);
        if start >= to {
            break;
        }
        let bounds = item_bounds(source, start, to).ok_or_else(|| {
            format!(
                "has an item on line {} whose end cannot be found",
                line_of(source, start)
            )
        })?;
        let (key, name) = describe(source, bounds.declaration, bounds.body, bounds.end);
        let path = format!("{prefix}{key}");
        let mut item = RustItem {
            path: path.clone(),
            name,
            members: Vec::new(),
            start,
            end: bounds.end,
            test_only: bounds.test_only,
            attributed: bounds.declaration != start,
            body: bounds.body,
        };
        if let Some((open, close)) = bounds.body {
            if holds_members(&path) && !bounds.test_only {
                item.members = items_in(source, open + 1, close, &format!("{path} / "))?;
            }
        }
        items.push(item);
        at = bounds.end;
    }
    Ok(items)
}

struct Bounds {
    /// Just past its last token.
    end: usize,
    /// Where its declaration starts, after its attributes.
    declaration: usize,
    test_only: bool,
    /// The first brace pair at its own depth.
    body: Option<(usize, usize)>,
}

/// The item whose first token is at `at`, ending before `limit`.
fn item_bounds(source: &str, at: usize, limit: usize) -> Option<Bounds> {
    let bytes = source.as_bytes();
    let mut cursor = at;
    let mut test_only = false;
    loop {
        let token = skip_blank(source, cursor);
        if token >= limit {
            return None;
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
            return Some(Bounds {
                end,
                declaration: token,
                test_only: false,
                body: None,
            });
        }
        test_only |= spelled == "#[cfg(test)]";
        cursor = end;
    }
    let declaration = cursor;
    let (mut braces, mut parens, mut brackets) = (0usize, 0usize, 0usize);
    let mut open = None;
    loop {
        cursor = skip_blank(source, cursor);
        if cursor >= limit {
            return None;
        }
        let byte = bytes[cursor];
        let next = skip_token(source, cursor);
        match byte {
            b'(' => parens += 1,
            b')' => parens = parens.checked_sub(1)?,
            b'[' => brackets += 1,
            b']' => brackets = brackets.checked_sub(1)?,
            b'{' => {
                if braces == 0 && parens == 0 && brackets == 0 && open.is_none() {
                    open = Some(cursor);
                }
                braces += 1;
            }
            b'}' => {
                braces = braces.checked_sub(1)?;
                if braces == 0 && parens == 0 && brackets == 0 {
                    let behind = skip_blank(source, next);
                    let end = if behind < limit && bytes.get(behind) == Some(&b';') {
                        behind + 1
                    } else {
                        next
                    };
                    return Some(Bounds {
                        end,
                        declaration,
                        test_only,
                        body: open.map(|open| (open, cursor)),
                    });
                }
            }
            b';' if braces == 0 && parens == 0 && brackets == 0 => {
                return Some(Bounds {
                    end: next,
                    declaration,
                    test_only,
                    body: None,
                });
            }
            _ => {}
        }
        cursor = next;
    }
}

/// The tokens from `from` up to `to`, comments and whitespace left out.
fn tokens(source: &str, from: usize, to: usize) -> Vec<&str> {
    let mut tokens = Vec::new();
    let mut at = from;
    loop {
        at = skip_blank(source, at);
        if at >= to {
            break;
        }
        let next = skip_token(source, at);
        tokens.push(&source[at..next]);
        at = next;
    }
    tokens
}

/// Tokens written the way a person reads them: a space only between two
/// words.
fn compact(tokens: &[&str]) -> String {
    let mut text = String::new();
    for token in tokens {
        // A word after a closing angle bracket is a new word too:
        // `Deserialize<'de> for X`, never `Deserialize<'de>for X`.
        let word_before = text
            .chars()
            .last()
            .is_some_and(|c| c == '_' || c == '>' || c.is_ascii_alphanumeric());
        let word_now = token
            .chars()
            .next()
            .is_some_and(|c| c == '_' || c.is_ascii_alphanumeric());
        if word_before && word_now {
            text.push(' ');
        }
        text.push_str(token);
    }
    text
}

/// The key of the item declared at `declaration`, and the name it declares.
fn describe(
    source: &str,
    declaration: usize,
    body: Option<(usize, usize)>,
    end: usize,
) -> (String, Option<String>) {
    let stop = body.map_or(end, |(open, _)| open);
    let tokens = tokens(source, declaration, stop);
    let mut at = 0;
    let get = |at: usize| tokens.get(at).copied().unwrap_or("");
    if get(at) == "pub" {
        at += 1;
        if get(at) == "(" {
            while at < tokens.len() && get(at) != ")" {
                at += 1;
            }
            at += 1;
        }
    }
    loop {
        match get(at) {
            "default" | "async" | "unsafe" => at += 1,
            "const" if matches!(get(at + 1), "fn" | "unsafe" | "async" | "extern") => at += 1,
            "extern" if get(at + 1).starts_with('"') => at += 2,
            _ => break,
        }
    }
    let kind = get(at);
    match kind {
        "fn" | "struct" | "enum" | "union" | "trait" | "type" | "mod" | "const" => {
            let name = get(at + 1).to_string();
            (format!("{kind} {name}"), Some(name))
        }
        "static" => {
            let name_at = if get(at + 1) == "mut" { at + 2 } else { at + 1 };
            let name = get(name_at).to_string();
            (format!("static {name}"), Some(name))
        }
        "impl" => {
            let mut start = at + 1;
            if get(start) == "<" {
                let mut depth = 0usize;
                while start < tokens.len() {
                    match get(start) {
                        "<" => depth += 1,
                        ">" if get(start.wrapping_sub(1)) != "-" => {
                            depth = depth.saturating_sub(1);
                            if depth == 0 {
                                start += 1;
                                break;
                            }
                        }
                        _ => {}
                    }
                    start += 1;
                }
            }
            let header_end = tokens[start.min(tokens.len())..]
                .iter()
                .position(|token| *token == "where")
                .map_or(tokens.len(), |offset| start + offset);
            (
                format!(
                    "impl {}",
                    compact(&tokens[start.min(header_end)..header_end])
                ),
                None,
            )
        }
        "use" => {
            // A brace in a `use` is a group of names, not a body.
            let whole = self::tokens(source, declaration, end);
            let from = whole
                .iter()
                .position(|token| *token == "use")
                .map_or(0, |at| at + 1);
            let rest: Vec<&str> = whole[from..]
                .iter()
                .copied()
                .filter(|token| *token != ";")
                .collect();
            (format!("use {}", compact(&rest)), None)
        }
        "macro_rules" if get(at + 1) == "!" => {
            let name = get(at + 2).to_string();
            (format!("macro_rules! {name}"), Some(name))
        }
        "extern" if get(at + 1) == "crate" => (format!("extern crate {}", get(at + 2)), None),
        word if is_identifier(word) && get(at + 1) == "!" => (format!("{word}!"), None),
        _ if at >= tokens.len() => ("item".to_string(), None),
        _ => (compact(&tokens[at..]), None),
    }
}
