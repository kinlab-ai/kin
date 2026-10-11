// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The names and qualifiers a question writes.
//!
//! A question names what it asks about in a handful of ways: in backticks, as
//! a dotted or owner-qualified name, as an identifier-shaped word
//! (`snake_case`, `camelCase`, `PascalCase`, `name()`), as "the X function",
//! or failing all of those as the one word that is not a question word. It
//! can say where the name is declared ("the X function in flask.app", or a
//! `module :: name` row pasted from an earlier answer), and it can write a
//! path, which Kin does not take as an address and reports as not applied.
//!
//! What a name may look like is read from every language adapter's
//! [`kin_parser::name_syntax::NameSyntax`], and what a file name ends in from
//! the extensions the adapters claim, so nothing here names a language.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use kin_parser::languages::AdapterRegistry;
use kin_parser::name_syntax;
use regex::Regex;

/// Words written with a dot inside a sentence that are not names.
const ABBREVIATIONS: [&str; 5] = ["e.g", "i.e", "a.k.a", "etc", "vs"];

/// The words a question asks with, never the name it asks about.
const STOP_WORDS: &[&str] = &[
    "a",
    "an",
    "and",
    "any",
    "anywhere",
    "are",
    "as",
    "at",
    "be",
    "by",
    "called",
    "calling",
    "calls",
    "call",
    "can",
    "change",
    "changes",
    "changed",
    "chain",
    "code",
    "could",
    "class",
    "classes",
    "defined",
    "definition",
    "delete",
    "deleted",
    "direct",
    "directly",
    "do",
    "does",
    "each",
    "end",
    "every",
    "file",
    "files",
    "find",
    "for",
    "from",
    "function",
    "functions",
    "get",
    "give",
    "has",
    "have",
    "how",
    "i",
    "if",
    "implementation",
    "implement",
    "implements",
    "in",
    "indirectly",
    "into",
    "is",
    "it",
    "its",
    "just",
    "list",
    "me",
    "method",
    "methods",
    "module",
    "must",
    "my",
    "need",
    "of",
    "on",
    "or",
    "other",
    "override",
    "overrides",
    "parameter",
    "please",
    "remove",
    "repository",
    "safely",
    "show",
    "source",
    "tell",
    "tests",
    "test",
    "that",
    "the",
    "there",
    "they",
    "this",
    "through",
    "to",
    "up",
    "use",
    "used",
    "uses",
    "using",
    "want",
    "what",
    "when",
    "where",
    "whether",
    "which",
    "who",
    "whole",
    "why",
    "will",
    "with",
    "would",
    "you",
    "all",
    "also",
    "anything",
    "body",
    "callers",
    "caller",
    "dead",
    "depth",
    "examples",
    "exclude",
    "ignore",
    "including",
    "include",
    "located",
    "location",
    "look",
    "new",
    "no",
    "not",
    "one",
    "only",
    "out",
    "path",
    "required",
    "see",
    "so",
    "some",
    "them",
    "then",
    "transitively",
    "unused",
    "us",
    "we",
    "were",
    "kin",
    "repo",
];

/// Words that name tests or test doubles. A question writes them inside
/// underscores (`__tests__`) as a place, never as a name it asks about.
const TEST_WORDS: [&str; 4] = ["test", "tests", "mock", "mocks"];

/// What the extractor read from one question.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Extracted {
    /// Names the question could be asking about, in order of appearance,
    /// each once.
    pub candidates: Vec<String>,
    /// The module the question says the name is declared in, when it names
    /// one: a dotted qualifier or the module of a pasted `module :: name` row.
    pub qualifier: Option<String>,
    /// The first path-shaped token that names a file, then the first that
    /// names a directory. Kin does not take either as an address.
    pub file_path: Option<String>,
    pub directory_path: Option<String>,
}

impl Extracted {
    /// The first source path and first directory path, in that order.
    /// These are unapplied text, never addresses or repository scopes.
    pub fn paths(&self) -> Vec<String> {
        self.file_path
            .iter()
            .chain(self.directory_path.iter())
            .cloned()
            .collect()
    }
}

/// The identifier pattern, with every extra character an adapter's
/// identifiers take.
fn identifier_pattern() -> &'static str {
    static PATTERN: OnceLock<String> = OnceLock::new();
    PATTERN.get_or_init(|| {
        let extra: String = name_syntax::identifier_extra_characters()
            .into_iter()
            .map(|character| regex::escape(&character.to_string()))
            .collect();
        format!("[A-Za-z_{extra}][\\w{extra}]*")
    })
}

/// The separators an adapter's names are written with, as one alternation.
fn separator_pattern() -> &'static str {
    static PATTERN: OnceLock<String> = OnceLock::new();
    PATTERN.get_or_init(|| {
        let separators: Vec<String> = name_syntax::written_separators()
            .into_iter()
            .map(regex::escape)
            .collect();
        format!("(?:{})", separators.join("|"))
    })
}

fn compiled(pattern: String) -> Regex {
    Regex::new(&pattern).expect("an ask extraction pattern compiles")
}

/// Every extension a language adapter claims.
fn source_extensions() -> &'static HashSet<String> {
    static EXTENSIONS: OnceLock<HashSet<String>> = OnceLock::new();
    EXTENSIONS.get_or_init(|| {
        let registry = AdapterRegistry::new();
        registry
            .supported_languages_with_extensions()
            .into_iter()
            .flat_map(|(_, extensions)| extensions.iter().map(|extension| (*extension).to_string()))
            .collect()
    })
}

/// Whether `token` ends in an extension a language adapter claims.
fn names_a_file(token: &str) -> bool {
    token
        .rsplit_once('.')
        .is_some_and(|(_, extension)| source_extensions().contains(extension))
}

/// `token` without the punctuation a sentence wraps it in. A trailing call
/// `()` is kept, and so is a closing parenthesis the token opened itself.
/// `keep_open_parenthesis` keeps a leading `(`, which a written owner such as
/// `(*Server).Handle` begins with.
fn strip_token(token: &str, keep_open_parenthesis: bool) -> String {
    let leading: &[char] = if keep_open_parenthesis {
        &['[', '{', '<', '"', '\'', '`']
    } else {
        &['(', '[', '{', '<', '"', '\'', '`']
    };
    let mut token = token.trim_start_matches(leading).to_string();
    for possessive in ["'s", "\u{2019}s"] {
        if let Some(stripped) = token.strip_suffix(possessive) {
            token = stripped.to_string();
        }
    }
    loop {
        let mut cut = token
            .trim_end_matches(['.', ',', ';', ':', '?', '!', '"', '\'', '`', ']', '}', '>'])
            .to_string();
        if cut.ends_with(')') && cut.matches('(').count() < cut.matches(')').count() {
            cut.pop();
        }
        if cut == token {
            return token;
        }
        token = cut;
    }
}

/// `token` as a written name, call parentheses dropped, or `None` for a path,
/// a file name, an abbreviation or anything no adapter writes as a name.
fn name_of(token: &str) -> Option<String> {
    for keep_open_parenthesis in [false, true] {
        let stripped = strip_token(token, keep_open_parenthesis);
        let name = stripped.strip_suffix("()").unwrap_or(&stripped);
        if name.is_empty()
            || names_a_file(name)
            || ABBREVIATIONS.contains(&name.to_lowercase().as_str())
        {
            continue;
        }
        // A module segment may have syntax no identifier carries (such as
        // a package's hyphen). A valid qualified form still names a candidate.
        if !name_syntax::written_name_forms(name).is_empty() {
            return Some(name.to_string());
        }
    }
    None
}

/// Whether a written name has more than one segment, as `App.run`,
/// `Store::get` or `(*Server).Handle` do.
fn is_qualified(name: &str) -> bool {
    name_syntax::written_name_forms(name)
        .iter()
        .any(|form| form.module.is_some())
}

pub(super) fn is_stop_word(word: &str) -> bool {
    STOP_WORDS.contains(&word.to_lowercase().as_str())
}

/// A word whose cased letters are all capitals, as an acronym is.
fn is_all_capitals(word: &str) -> bool {
    let mut cased = false;
    for character in word.chars() {
        if character.is_lowercase() {
            return false;
        }
        cased |= character.is_uppercase();
    }
    cased
}

/// A word that is only underscores around a test word, as `__tests__` is.
fn is_test_place(word: &str) -> bool {
    word.starts_with('_') && TEST_WORDS.contains(&word.trim_matches('_').to_lowercase().as_str())
}

fn push_unique(found: &mut Vec<String>, name: String) {
    if !found.contains(&name) {
        found.push(name);
    }
}

/// A code-shaped member written without quoting. A plain prose word needs
/// an explicit quote or call to distinguish it from a possessive sentence.
fn unquoted_member_hint(raw: &str, name: &str) -> bool {
    raw.contains("()")
        || name.contains('_')
        || name.chars().skip(1).any(char::is_uppercase)
        || name_syntax::identifier_extra_characters()
            .iter()
            .any(|c| name.contains(*c))
}

fn possessive_member(owner_raw: &str, member_raw: &str) -> Option<(String, String, String)> {
    let owner = name_of(owner_raw)?;
    let member = name_of(member_raw)?;
    if !name_syntax::is_written_identifier(&member)
        || is_test_place(&owner)
        || is_test_place(&member)
    {
        return None;
    }
    let member_quoted = member_raw.starts_with('`');
    let member_hint = member_quoted || unquoted_member_hint(member_raw, &member);
    let owner_hint = owner_raw.starts_with('`')
        || member_quoted
        || is_qualified(&owner)
        || owner.chars().next().is_some_and(char::is_uppercase)
        || unquoted_member_hint(owner_raw, &owner);
    if !owner_hint || !member_hint {
        return None;
    }
    // Reuse a written owner separator when there is one. Otherwise try the
    // adapter-declared separators, validating the complete spelling too.
    let separators = name_syntax::written_separators();
    let preferred = separators
        .iter()
        .find(|separator| owner.contains(**separator));
    let joined = preferred
        .into_iter()
        .copied()
        .chain(separators.iter().rev().copied())
        .find_map(|separator| {
            let joined = format!("{owner}{separator}{member}");
            (!name_syntax::written_name_forms(&joined).is_empty()).then_some(joined)
        })?;
    Some((joined, owner, member))
}

/// The paths a question writes: the first that names a file, and the first
/// that names a directory.
fn paths(question: &str) -> (Option<String>, Option<String>) {
    static PATH: OnceLock<Regex> = OnceLock::new();
    static BARE_FILE: OnceLock<Regex> = OnceLock::new();
    let path = PATH.get_or_init(|| compiled(r"^[\w.@~+-]+(?:/[\w.@~+-]+)+/?$".into()));
    let bare_file = BARE_FILE.get_or_init(|| compiled(r"^[\w.@+-]+$".into()));
    let (mut file, mut directory) = (None, None);
    for raw in question.replace('`', " ").split_whitespace() {
        let token = strip_token(raw, false);
        if path.is_match(&token) {
            if names_a_file(&token) {
                file = file.or(Some(token));
            } else {
                directory = directory.or_else(|| Some(token.trim_end_matches('/').to_string()));
            }
        } else if names_a_file(&token) && bare_file.is_match(&token) {
            file = file.or(Some(token));
        }
    }
    (file, directory)
}

/// Names the question could be asking about, in order of appearance, each
/// once. Tiers, each used only when the one before found nothing:
/// backticked names and qualified names written outside backticks; then
/// identifier-shaped words; then "the X function" and "method X"; then any
/// word that is not a question word. "`m` method of the `C`" and "`C`'s `m`"
/// are joined into `C.m`, which replaces its parts while retaining other
/// identifier-shaped names. Names in `skip` (a module qualifier) are left out.
fn candidates(question: &str, skip: &HashSet<String>) -> Vec<String> {
    let ident = identifier_pattern();
    static TICKED: OnceLock<Regex> = OnceLock::new();
    static TICKED_SPAN: OnceLock<Regex> = OnceLock::new();
    static MEMBER_OF: OnceLock<Regex> = OnceLock::new();
    static OWNERS_MEMBER: OnceLock<Regex> = OnceLock::new();
    static POSSESSIVE_MEMBER: OnceLock<Regex> = OnceLock::new();
    static PLAIN_MEMBER_OF: OnceLock<Regex> = OnceLock::new();
    static SHAPED: OnceLock<Regex> = OnceLock::new();
    static THE_KIND: OnceLock<Regex> = OnceLock::new();
    static KIND_NAMED: OnceLock<Regex> = OnceLock::new();
    static CAPITAL_OR_UNDERSCORE: OnceLock<Regex> = OnceLock::new();
    let kind = "(?:method|function|property|attribute|member|classmethod|staticmethod)";
    let ticked_re = TICKED.get_or_init(|| compiled(r"`([^`\n]+)`".into()));
    let ticked_span = TICKED_SPAN.get_or_init(|| compiled(r"`[^`\n]*`".into()));

    let mut positions = HashMap::<String, usize>::new();
    let ticked: Vec<String> = ticked_re
        .captures_iter(question)
        .filter_map(|captures| {
            let name = name_of(&captures[1])?;
            positions
                .entry(name.clone())
                .or_insert(captures.get(1)?.start());
            Some(name)
        })
        .filter(|name| !skip.contains(name) && !is_test_place(name))
        .collect();
    // Preserve byte offsets, including for quoted Unicode, while preventing
    // a quoted name from being extracted twice by the plain-word tiers.
    let plain = ticked_span
        .replace_all(question, |captures: &regex::Captures<'_>| {
            " ".repeat(captures[0].len())
        })
        .into_owned();
    let mut searched = 0;
    let words: Vec<(String, String)> = plain
        .split_whitespace()
        .map(|raw| {
            let start = searched
                + plain[searched..]
                    .find(raw)
                    .expect("a split word is present");
            searched = start + raw.len();
            if let Some(name) = name_of(raw) {
                positions
                    .entry(name)
                    .and_modify(|held| *held = (*held).min(start))
                    .or_insert(start);
            }
            (raw.to_string(), strip_token(raw, false))
        })
        .filter(|(_, word)| !skip.contains(word))
        .collect();
    let qualified: Vec<String> = words
        .iter()
        .filter_map(|(raw, _)| name_of(raw))
        .filter(|name| is_qualified(name))
        .collect();
    let mut found: Vec<String> = ticked.into_iter().chain(qualified).collect();

    let mut joined: Vec<String> = Vec::new();
    let mut parts: HashSet<String> = HashSet::new();
    let member_of = MEMBER_OF.get_or_init(|| {
        compiled(format!(
            r"`({ident})(?:\(\))?`\s+{kind}\s+(?:of|on|in|from)\s+(?:the\s+)?(?:[\w-]+\s+){{0,3}}?`({ident}(?:\.{ident})*)`"
        ))
    });
    for captures in member_of.captures_iter(question) {
        let (member, owner) = (captures[1].to_string(), captures[2].to_string());
        if is_test_place(&owner) || is_test_place(&member) {
            continue;
        }
        let name = format!("{owner}.{member}");
        let start = captures.get(0).unwrap().start();
        positions
            .entry(name.clone())
            .and_modify(|held| *held = (*held).min(start))
            .or_insert(start);
        joined.push(name);
        parts.insert(member);
        parts.insert(owner);
    }
    let owners_member = OWNERS_MEMBER.get_or_init(|| {
        compiled(format!(
            r"`({ident}(?:\.{ident})*)`(?:'s|\s+class's)\s+`({ident})(?:\(\))?`"
        ))
    });
    for captures in owners_member.captures_iter(question) {
        let (owner, member) = (captures[1].to_string(), captures[2].to_string());
        if is_test_place(&owner) || is_test_place(&member) {
            continue;
        }
        let name = format!("{owner}.{member}");
        let start = captures.get(0).unwrap().start();
        positions
            .entry(name.clone())
            .and_modify(|held| *held = (*held).min(start))
            .or_insert(start);
        joined.push(name);
        parts.insert(member);
        parts.insert(owner);
    }
    let possessive = POSSESSIVE_MEMBER
        .get_or_init(|| compiled(r"(`[^`\n]+`|[^\s`]+?)(?:'s|’s)\s+(`[^`\n]+`|[^\s]+)".into()));
    for captures in possessive.captures_iter(question) {
        let Some((name, owner, member)) = possessive_member(&captures[1], &captures[2]) else {
            continue;
        };
        let start = captures.get(0).unwrap().start();
        positions
            .entry(name.clone())
            .and_modify(|held| *held = (*held).min(start))
            .or_insert(start);
        joined.push(name);
        parts.insert(owner);
        parts.insert(member);
    }
    if found.is_empty() {
        let plain_member_of = PLAIN_MEMBER_OF.get_or_init(|| {
            compiled(format!(
                r"\b({ident})\s+{kind}\s+(?:of|on|in|from)\s+(?:the\s+)?({ident})\b"
            ))
        });
        let capital_or_underscore =
            CAPITAL_OR_UNDERSCORE.get_or_init(|| compiled(r"[A-Z_]".into()));
        for captures in plain_member_of.captures_iter(&plain) {
            let (member, owner) = (captures[1].to_string(), captures[2].to_string());
            if capital_or_underscore.is_match(&owner)
                && !is_stop_word(&owner)
                && !is_stop_word(&member)
                && !is_test_place(&owner)
                && !is_test_place(&member)
            {
                let name = format!("{owner}.{member}");
                let start = captures.get(0).unwrap().start();
                positions
                    .entry(name.clone())
                    .and_modify(|held| *held = (*held).min(start))
                    .or_insert(start);
                joined.push(name);
                parts.insert(member);
                parts.insert(owner);
            }
        }
    }
    let has_joined = !joined.is_empty();
    if has_joined {
        let first = found
            .iter()
            .position(|name| parts.contains(name))
            .unwrap_or(0);
        let mut rebuilt: Vec<String> = found[..first].to_vec();
        rebuilt.extend(joined);
        rebuilt.extend(
            found[first..]
                .iter()
                .filter(|name| !parts.contains(*name))
                .cloned(),
        );
        found = rebuilt;
    }
    if found.is_empty() || has_joined {
        let shaped = SHAPED.get_or_init(|| {
            compiled(format!(
                r"^(?:{ident}_[\w$]+|_+[\w$]+|[a-z$][a-z0-9$]*[A-Z][\w$]*|(?:[A-Z][a-z0-9]+){{2,}}[\w$]*)$"
            ))
        });
        found.extend(
            words
                .iter()
                .filter(|(raw, word)| {
                    let named = name_of(raw).is_some();
                    named && (word.ends_with("()") || shaped.is_match(word)) && !is_test_place(word)
                })
                .map(|(_, word)| word.strip_suffix("()").unwrap_or(word).to_string())
                .filter(|name| !parts.contains(name)),
        );
    }
    if found.is_empty() {
        let the_kind = THE_KIND.get_or_init(|| {
            compiled(format!(
                r"\bthe\s+({ident})\s+(?:function|method|class|hook|component|constant|variable|property)\b"
            ))
        });
        let kind_named = KIND_NAMED.get_or_init(|| {
            compiled(format!(
                r"\b(?:function|method|class)\s+(?:named\s+|called\s+)?({ident})\b"
            ))
        });
        found = the_kind
            .captures_iter(&plain)
            .chain(kind_named.captures_iter(&plain))
            .map(|captures| captures[1].to_string())
            .filter(|word| !is_stop_word(word) && !skip.contains(word))
            .collect();
    }
    if found.is_empty() {
        found = words
            .iter()
            .filter(|(raw, word)| {
                !(name_of(raw).is_none()
                    || word.contains('.')
                    || is_stop_word(word)
                    || is_test_place(word)
                    || (is_all_capitals(word) && word.chars().count() > 1))
            })
            .map(|(_, word)| word.clone())
            .collect();
    }
    // Tier choice determines which names count, not which one was written
    // first. Exact token offsets also keep a name inside a path from moving
    // a later standalone candidate ahead of the text that actually named it.
    found.sort_by_key(|name| positions.get(name).copied().unwrap_or(usize::MAX));
    let mut unique = Vec::with_capacity(found.len());
    for name in found {
        push_unique(&mut unique, name);
    }
    unique
}

/// Candidates, the qualifier and the paths one question writes.
///
/// The qualifier is a pasted `module :: name` row's module, else a module the
/// question gives as a qualifier ("the X function in
/// react-reconciler.ReactFiberScope"). A module qualifier is not offered as a
/// candidate unless it is the only name in the question.
pub fn extract(question: &str) -> Extracted {
    static ROW: OnceLock<Regex> = OnceLock::new();
    static QUALIFIER: OnceLock<Regex> = OnceLock::new();
    let (file_path, directory_path) = paths(question);
    let row = ROW.get_or_init(|| compiled(r"([\w@.-]+)\s+::\s+`?([^\s`]+)".into()));
    for captures in row.captures_iter(question) {
        let module = captures.get(1).expect("a row carries its module");
        if question[..module.start()].ends_with(['/', '\\']) || names_a_file(module.as_str()) {
            continue;
        }
        if let Some(name) = name_of(&captures[2]) {
            return Extracted {
                candidates: vec![name],
                qualifier: Some(captures[1].to_string()),
                file_path,
                directory_path,
            };
        }
    }
    let separator = separator_pattern();
    let qualifier = QUALIFIER.get_or_init(|| {
        compiled(format!(
            r"\b(?:defined in|declared in|exported from|imported from|in|from|inside|within)\s+(?:the\s+)?(?:module\s+|package\s+)?`?([A-Za-z_@][\w-]*(?:{separator}[\w-]+)+)`?"
        ))
    });
    let module = qualifier
        .captures_iter(question)
        .filter_map(|captures| {
            let module = captures.get(1)?;
            // A valid-looking prefix of a path is still a path, never an
            // inferred module qualifier or a scope for a later operation.
            if question[module.end()..].starts_with(['/', '\\']) {
                return None;
            }
            Some(module.as_str().to_string())
        })
        .find(|module| !names_a_file(&strip_token(module, false)))
        .map(|module| strip_token(&module, false))
        .unwrap_or_default();
    if !module.is_empty() {
        let skip: HashSet<String> = [module.clone()].into_iter().collect();
        let candidates = candidates(question, &skip);
        if !candidates.is_empty() {
            return Extracted {
                candidates,
                qualifier: Some(module),
                file_path,
                directory_path,
            };
        }
    }
    Extracted {
        candidates: candidates(question, &HashSet::new()),
        qualifier: None,
        file_path,
        directory_path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_go_receiver_and_a_rust_path_are_candidates_as_written() {
        let extracted = extract("Who calls `(*Server).Handle`?");
        assert_eq!(extracted.candidates, vec!["(*Server).Handle"]);
        let extracted = extract("What calls crate::store::Store::get directly?");
        assert_eq!(extracted.candidates, vec!["crate::store::Store::get"]);
    }

    #[test]
    fn a_path_is_read_as_a_path_and_never_as_a_candidate() {
        let extracted = extract(
            "Who calls collectScopedNodes in packages/react-reconciler/src/ReactFiberScope.js?",
        );
        assert_eq!(extracted.candidates, vec!["collectScopedNodes"]);
        assert_eq!(
            extracted.paths(),
            vec!["packages/react-reconciler/src/ReactFiberScope.js"]
        );
    }

    #[test]
    fn a_test_place_is_not_a_candidate() {
        let extracted = extract("Who calls into the scheduler, excluding __tests__ and __mocks__?");
        assert!(extracted.candidates.iter().all(|name| !name.contains("__")));
    }

    #[test]
    fn adapter_valid_package_qualifiers_are_candidates_without_becoming_paths() {
        let extracted = extract("Who calls render-core.Store.save and `$cache.load`?");
        assert_eq!(
            extracted.candidates,
            ["render-core.Store.save", "$cache.load"]
        );
        assert!(extracted.paths().is_empty());
        assert_eq!(extracted.qualifier, None);
        assert!(name_syntax::written_name_forms(&extracted.candidates[0])
            .iter()
            .all(|form| form.module.is_some()));
    }

    #[test]
    fn mixed_quoted_and_plain_names_follow_textual_order_and_deduplicate() {
        assert_eq!(
            extract("App.run calls `db.load` before App.run and `db.load`?").candidates,
            ["App.run", "db.load"]
        );
    }

    #[test]
    fn quoted_unicode_preserves_offsets_for_later_plain_names() {
        assert_eq!(
            extract("`Δelta.load` calls App.run before `βeta.save`?").candidates,
            ["Δelta.load", "App.run", "βeta.save"]
        );
    }

    #[test]
    fn owner_member_phrases_keep_their_position_beside_written_names() {
        assert_eq!(
            extract("API.start calls the `run` method of `App`, then DB.save?").candidates,
            ["API.start", "App.run", "DB.save"]
        );
        assert_eq!(
            extract("The `run` method of `App` calls DB.save before App.run?").candidates,
            ["App.run", "DB.save"]
        );
    }

    #[test]
    fn names_embedded_in_paths_do_not_move_real_candidates() {
        let extracted = extract("In src/App.run, who calls API.start and `App.run`?");
        assert_eq!(extracted.candidates, ["API.start", "App.run"]);
        assert_eq!(extracted.directory_path.as_deref(), Some("src/App.run"));
        assert_eq!(extracted.qualifier, None);
    }

    #[test]
    fn a_module_like_path_prefix_is_not_a_qualifier() {
        for path in ["pkg.module/src/helper.rs", "@scope/pkg.module/helper.rs"] {
            let extracted = extract(&format!("Who calls `Store.get` in {path}?"));
            assert_eq!(extracted.candidates, ["Store.get"]);
            assert_eq!(extracted.qualifier, None);
            assert_eq!(extracted.file_path.as_deref(), Some(path));
        }
    }

    #[test]
    fn a_path_before_a_row_separator_is_not_a_module_address() {
        for path in ["pkg/src", "src/lib.rs", "lib.rs"] {
            let extracted = extract(&format!("Show {path} :: `Store.get`"));
            assert_eq!(extracted.candidates, ["Store.get"]);
            assert_eq!(extracted.qualifier, None);
            assert_eq!(extracted.paths(), [path]);
        }
    }

    #[test]
    fn named_modules_and_pasted_rows_remain_explicit_qualifiers() {
        let extracted = extract("Who calls `Store.get` in data.store?");
        assert_eq!(extracted.candidates, ["Store.get"]);
        assert_eq!(extracted.qualifier.as_deref(), Some("data.store"));
        assert!(extracted.paths().is_empty());
        let row = extract("Show data.store :: `Store.get`");
        assert_eq!(row.candidates, ["Store.get"]);
        assert_eq!(row.qualifier.as_deref(), Some("data.store"));
    }

    #[test]
    fn source_and_directory_metadata_never_enter_the_candidate_list() {
        let extracted = extract("Who calls `Store.get` in src/data/ and src/lib.rs?");
        assert_eq!(extracted.candidates, ["Store.get"]);
        assert_eq!(extracted.qualifier, None);
        assert_eq!(extracted.paths(), ["src/lib.rs", "src/data"]);
    }

    #[test]
    fn absent_names_and_punctuation_produce_no_candidates() {
        for question in ["", "   ", "Who calls?", "???", "__tests__ __mocks__"] {
            assert!(extract(question).candidates.is_empty(), "{question}");
        }
    }

    #[test]
    fn quoted_and_unquoted_possessive_members_keep_one_adapter_valid_name() {
        for question in [
            "Who calls `Store`'s `load_value`?",
            "Who calls Store's `load_value`?",
            "Who calls `Store`'s load_value?",
            "Who calls Store's load_value?",
            "Who calls Store’s load_value?",
        ] {
            assert_eq!(
                extract(question).candidates,
                ["Store.load_value"],
                "{question}"
            );
        }
        assert_eq!(
            extract("Who calls Store's get()?").candidates,
            ["Store.get"]
        );
    }

    #[test]
    fn a_qualified_possessive_owner_does_not_hide_its_written_member() {
        let got = extract("Does render-core.Server's pushSegmentFinale call `Queue.flush`?");
        assert_eq!(
            got.candidates,
            ["render-core.Server.pushSegmentFinale", "Queue.flush"]
        );
        assert!(got.paths().is_empty());
        assert_eq!(
            extract("Who calls crate::Store's load_value?").candidates,
            ["crate::Store::load_value"]
        );
        assert_eq!(
            extract("Who calls (*Server)'s HandleCall?").candidates,
            ["(*Server).HandleCall"]
        );
    }

    #[test]
    fn a_possessive_sentence_or_path_does_not_invent_an_owner_member() {
        for (owner, member) in [
            ("worker", "notes"),
            ("module.path", "own"),
            ("Store", "behavior"),
            ("src/Store.rs", "load_value"),
            ("Store", "__tests__"),
        ] {
            assert!(
                possessive_member(owner, member).is_none(),
                "{owner}'s {member}"
            );
        }
        let got = extract("Does the worker's notes mention `Store.load_value`?");
        assert_eq!(got.candidates, ["Store.load_value"]);
    }

    #[test]
    fn quoting_only_a_possessive_owner_does_not_promote_prose_members() {
        for question in [
            "Show `Store`'s callers",
            "Show `Store`'s own code",
            "What is `Store`'s behavior?",
        ] {
            assert_eq!(extract(question).candidates, ["Store"], "{question}");
        }
        for (question, expected) in [
            ("Show `Store`'s `get`", "Store.get"),
            ("Show `Store`'s get()", "Store.get"),
            ("Show `Store`'s load_value", "Store.load_value"),
            ("Show `Store`'s loadValue", "Store.loadValue"),
            ("Show `render-core.Store`'s `get`", "render-core.Store.get"),
            ("Show `crate::Store`'s get()", "crate::Store::get"),
        ] {
            assert_eq!(extract(question).candidates, [expected], "{question}");
        }
    }

    #[test]
    fn test_places_cannot_reenter_candidates_as_owners_or_members() {
        for place in ["__tests__", "__mocks__"] {
            for question in [
                format!("Who calls `{place}`'s load_value?"),
                format!("Who calls {place}'s `load_value`?"),
                format!("Who calls `{place}`'s `load_value`?"),
                format!("Who calls the `load_value` method of `{place}`?"),
                format!("Who calls the load_value method of {place}?"),
            ] {
                assert_eq!(extract(&question).candidates, ["load_value"], "{question}");
            }
            for question in [
                format!("Who calls `Store`'s `{place}`?"),
                format!("Who calls the `{place}` method of `Store`?"),
            ] {
                assert_eq!(extract(&question).candidates, ["Store"], "{question}");
            }
        }
    }

    #[test]
    fn joining_a_member_preserves_other_code_names_and_their_textual_order() {
        for question in [
            "Does Store's load_value call save_value?",
            "Does `Store`'s `load_value` call save_value?",
            "Does the `load_value` method of `Store` call save_value?",
        ] {
            assert_eq!(
                extract(question).candidates,
                ["Store.load_value", "save_value"],
                "{question}"
            );
        }
        assert_eq!(
            extract(
                "Does before_value call Store's load_value before save_value and before_value?"
            )
            .candidates,
            ["before_value", "Store.load_value", "save_value"]
        );
        let extracted =
            extract("Does render-core.Store's load_value call save_value in src/Store.rs?");
        assert_eq!(
            extracted.candidates,
            ["render-core.Store.load_value", "save_value"]
        );
        assert_eq!(extracted.paths(), ["src/Store.rs"]);
        assert_eq!(extracted.qualifier, None);
    }
}
