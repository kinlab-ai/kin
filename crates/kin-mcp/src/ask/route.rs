// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Deterministic question rules. A plan describes text, not graph evidence.
//!
//! Rule priority is deliberate: transitive callers, impact, uses, location,
//! source, direct callers, then other. Declaration questions only replace
//! other, or a source route caused solely by asking for more implementations.
//! No plan resolves an entity, applies a scope, or certifies an answer.

use std::sync::OnceLock;

use kin_parser::name_syntax;
use regex::Regex;
use serde::{Deserialize, Serialize};

use super::{extract, Extracted};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Intent {
    CallersDirect,
    CallersTransitive,
    ChangeImpact,
    IsItUsed,
    WhereIsIt,
    ShowSource,
    Declares,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeclarationQuestion {
    pub function: String,
    pub modules: Vec<String>,
    pub known: Vec<String>,
    pub asked_other: bool,
    /// Written package hints, not applied repository scopes.
    pub packages: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Plan {
    pub intent: Intent,
    pub target: Option<String>,
    pub targets: Vec<String>,
    pub candidates: Vec<String>,
    pub qualifier: Option<String>,
    /// Words explicitly naming a scope; still require graph resolution.
    pub scope_words: Option<String>,
    pub include_tests: bool,
    /// Exact text that selected the route, preserving spelling and case.
    pub cue: Option<String>,
    pub test_cue: Option<String>,
    /// Path text is retained to explain what was not applied, never as scope.
    pub paths_not_applied: Vec<String>,
    pub declares: Option<DeclarationQuestion>,
}

fn compiled(pattern: &str) -> Regex {
    Regex::new(pattern).expect("a deterministic ask rule compiles")
}

fn keyword_rules() -> &'static [(Intent, Regex)] {
    static RULES: OnceLock<Vec<(Intent, Regex)>> = OnceLock::new();
    RULES.get_or_init(|| {
        use Intent::*;
        [
            (CallersTransitive, r"end up|chain|indirect|transitiv|eventually|reach|callers of its callers|any depth"),
            (ChangeImpact, r"have to change|has to change|must change|signature|required (parameter|argument)|breaks?\b|blast radius|overrid|renam"),
            (IsItUsed, r"used anywhere|called anywhere|referenced anywhere|unused|\bdelete|dead code|still used|any uses?\b|\b(?:imported|referenced|stored|passed)\b[^.?!]{0,80}\banywhere\b|\bimports?,? (?:or |and )?referenc|\breferenc\w*,? (?:or |and )?imports?|all relation kinds|every (?:use|reference)\b|\bwho (?:references|imports|uses)\b|\b(?:calls?|callers?),? imports?"),
            (WhereIsIt, r"^\s*where\b|\bwhere (?:is|are|does|do)\b|\b(?:which|what) (?:file|module|package)\b|definition of"),
            (ShowSource, r"\bshow\b|\bprint\b|source code|\bbody\b|implementation of"),
            (CallersDirect, r"\bcalls?\b|\bcallers?\b|\bcalled\b|\bcalling\b|\binvok"),
        ].into_iter().map(|(intent, pattern)| (intent, compiled(&format!("(?i){pattern}")))).collect()
    })
}

fn word_character(character: char) -> bool {
    character.is_alphanumeric()
        || character == '_'
        || name_syntax::identifier_extra_characters().contains(&character)
}

fn bounded(text: &str, start: usize, end: usize) -> bool {
    text[..start]
        .chars()
        .next_back()
        .is_none_or(|c| !word_character(c))
        && text[end..]
            .chars()
            .next()
            .is_none_or(|c| !word_character(c))
}

// Python's case-insensitive [A-Za-z] includes these four Unicode letters.
// Preserve the historical lookaround contract when spelling it without
// regex lookbehind; this is not the language-adapter identifier alphabet.
fn historical_test_boundary_letter(character: char) -> bool {
    character.is_ascii_alphabetic() || matches!(character, 'K' | 'ſ' | 'İ' | 'ı')
}

fn test_exclusion(question: &str) -> Option<(usize, usize)> {
    static PREFIX: OnceLock<Regex> = OnceLock::new();
    static NON_TEST: OnceLock<Regex> = OnceLock::new();
    static NOT_TEST: OnceLock<Regex> = OnceLock::new();
    static TEST: OnceLock<Regex> = OnceLock::new();
    let prefix = PREFIX.get_or_init(|| compiled(r"(?i)\b(?:ignor\w*|exclud\w*|skip\w*|not count\w*|do not count|outside(?: of)?|without|leav\w* out|omit\w*)\b"));
    let non_test = NON_TEST.get_or_init(|| compiled(r"(?i)\bnon-?test"));
    let not_test = NOT_TEST.get_or_init(|| compiled(r"(?i)\bnot (?:in |from |the |any )?$"));
    let test = TEST.get_or_init(|| compiled(r"(?i)tests?"));
    let mut matches = non_test
        .find_iter(question)
        .map(|m| (m.start(), m.end()))
        .collect::<Vec<_>>();
    // Preserve the original case-insensitive letter lookarounds without
    // regex lookbehind, and measure the 40-character gap in characters.
    for marker in test.find_iter(question) {
        let boundary = question[..marker.start()]
            .chars()
            .next_back()
            .is_none_or(|c| !historical_test_boundary_letter(c))
            && question[marker.end()..]
                .chars()
                .next()
                .is_none_or(|c| !historical_test_boundary_letter(c));
        if !boundary {
            continue;
        }
        if let Some(found) = not_test.find(&question[..marker.start()]) {
            matches.push((found.start(), marker.end()));
        }
        for found in prefix
            .find_iter(question)
            .filter(|found| found.end() <= marker.start())
        {
            let gap = &question[found.end()..marker.start()];
            if gap.chars().count() <= 40 && !gap.contains(['.', '?', '!']) {
                matches.push((found.start(), marker.end()));
            }
        }
    }
    matches.into_iter().min()
}

fn split_name(name: &str) -> (&str, &str) {
    let last = name_syntax::written_separators()
        .into_iter()
        .filter_map(|separator| name.rfind(separator).map(|at| (at, separator.len())))
        .max_by_key(|(at, _)| *at);
    last.map_or((name, ""), |(at, width)| (&name[at + width..], &name[..at]))
}

fn declaration_function(question: &str, candidates: &[String]) -> Option<(String, String)> {
    static TICKS: OnceLock<Regex> = OnceLock::new();
    let ticks = TICKS.get_or_init(|| compiled(r"`([^`\n]+)`"));
    for captures in ticks.captures_iter(question) {
        let written = captures[1].trim();
        let (leaf, owner) = split_name(written);
        if candidates
            .iter()
            .any(|candidate| candidate == written || candidate == leaf)
        {
            return Some((leaf.into(), owner.into()));
        }
    }
    for candidate in candidates {
        let (leaf, owner) = split_name(candidate);
        if !owner.is_empty() && leaf.chars().next().is_some_and(char::is_lowercase) {
            return Some((leaf.into(), owner.into()));
        }
    }
    let lowercase = |candidate: &str| {
        candidate.chars().next().is_some_and(char::is_lowercase) && !candidate.contains('-')
    };
    for candidate in candidates {
        if lowercase(candidate)
            && candidate
                .chars()
                .skip(1)
                .any(|c| c.is_ascii_uppercase() || c == '_')
        {
            return Some((candidate.clone(), String::new()));
        }
    }
    candidates
        .iter()
        .find(|candidate| lowercase(candidate) && candidate.chars().count() > 3)
        .map(|candidate| (candidate.clone(), String::new()))
}

fn declarations(question: &str, candidates: &[String]) -> Option<DeclarationQuestion> {
    static WITHOUT: OnceLock<Regex> = OnceLock::new();
    static TAIL: OnceLock<Regex> = OnceLock::new();
    static OTHER: OnceLock<Regex> = OnceLock::new();
    static PACKAGE: OnceLock<Regex> = OnceLock::new();
    let (function, qualifier) = declaration_function(question, candidates)?;
    let identifier_extra: String = name_syntax::identifier_extra_characters()
        .into_iter()
        .map(|c| regex::escape(&c.to_string()))
        .collect();
    let without = WITHOUT.get_or_init(|| {
        compiled(&format!(
            r"(?i)\bwithout\s+`?([A-Za-z_{identifier_extra}][\w{identifier_extra}]*)"
        ))
    });
    let dropped = without
        .captures_iter(question)
        .map(|c| c[1].to_string())
        .collect::<Vec<_>>();
    let separators = name_syntax::written_separators();
    let mut modules = candidates
        .iter()
        .filter(|candidate| {
            *candidate != &function
                && !dropped.contains(candidate)
                && !separators
                    .iter()
                    .any(|separator| candidate.starts_with(&format!("{function}{separator}")))
        })
        .cloned()
        .collect::<Vec<_>>();
    if !qualifier.is_empty() && !modules.contains(&qualifier) {
        modules.insert(0, qualifier);
    }
    let mut unique = Vec::new();
    for module in modules {
        if !unique.contains(&module) {
            unique.push(module);
        }
    }
    let tail = TAIL.get_or_init(|| {
        compiled(
            r"(?i)\b(?:besides|other than|apart from|aside from|except(?: for)?|beyond)\b([^?.!]*)",
        )
    });
    let tail_text = tail
        .captures_iter(question)
        .map(|c| c[1].to_string())
        .collect::<Vec<_>>()
        .join(" ");
    let known = unique
        .iter()
        .filter(|module| {
            tail_text
                .match_indices(module.as_str())
                .any(|(at, found)| bounded(&tail_text, at, at + found.len()))
        })
        .cloned()
        .collect();
    let other = OTHER.get_or_init(|| compiled(r"(?i)\b(?:other|another|any more|anymore|additional|else|besides|apart from|aside from|other than|more)\b"));
    let package = PACKAGE.get_or_init(|| compiled(r"[a-z][a-z0-9]*(?:-[a-z0-9]+)+"));
    let packages = package
        .find_iter(question)
        .filter(|found| {
            let before = question[..found.start()].chars().next_back();
            let after = question[found.end()..].chars().next();
            before.is_none_or(|c| !word_character(c) && !matches!(c, '.' | '-' | '/' | '\\'))
                && after.is_none_or(|c| !word_character(c) && !matches!(c, '-' | '/' | '\\'))
                && !["re-export", "non-test", "built-in"].contains(&found.as_str())
        })
        .map(|found| found.as_str().to_string())
        .collect();
    Some(DeclarationQuestion {
        function,
        modules: unique,
        known,
        asked_other: other.is_match(question),
        packages,
    })
}

fn scope_words(question: &str) -> Option<String> {
    static SCOPE: OnceLock<Regex> = OnceLock::new();
    let scope = SCOPE.get_or_init(|| compiled(r"\b(?:inside|within)\s+(?:the\s+)?`?([A-Za-z][\w-]*)`?(?:\s+package)?|\b([A-Za-z][\w-]*)'s own code"));
    scope.captures_iter(question).find_map(|captures| {
        let name = captures.get(1).or_else(|| captures.get(2))?;
        // Do not turn a path or a longer qualified spelling into a bare scope.
        // A sentence-final period has no following identifier segment.
        let suffix = &question[name.end()..];
        // A later separator in this written token still makes it a path.
        // Stop at whitespace so another path cannot erase a valid scope hint.
        let token_tail = suffix.split(char::is_whitespace).next().unwrap_or("");
        let path_continuation = token_tail.contains(['/', '\\']);
        let dotted_continuation = suffix
            .strip_prefix('.')
            .and_then(|rest| rest.chars().next())
            .is_some_and(word_character);
        if question[..name.start()].ends_with(['/', '\\', '.', ':'])
            || path_continuation
            || suffix.starts_with(':')
            || dotted_continuation
        {
            return None;
        }
        Some(name.as_str().into())
    })
}

fn first_name_at(question: &str, name: &str) -> Option<(usize, usize)> {
    if name.is_empty() {
        return None;
    }
    question.match_indices(name).find_map(|(at, found)| {
        let end = at + found.len();
        let prefix = &question[..at];
        (bounded(question, at, end)
            && !prefix.ends_with(['/', '\\'])
            && !name_syntax::written_separators()
                .iter()
                .any(|separator| prefix.ends_with(*separator)))
        .then_some((at, end))
    })
}

/// Whether a written name is shaped as code rather than as a word: qualified
/// by a separator an adapter writes, joined by `_` or an extra identifier
/// character, written in camel or Pascal humps, or called with `()`. A plain
/// word such as "reach" is not, even when the extractor offers it as a name,
/// because a question writes its cues as plain words.
fn shaped_as_code(name: &str) -> bool {
    let name = name.strip_suffix("()").unwrap_or(name);
    let extra = name_syntax::identifier_extra_characters();
    let mut previous_lower = false;
    let humped = name.chars().any(|character| {
        let hump = previous_lower && character.is_uppercase();
        previous_lower = character.is_lowercase() || character.is_ascii_digit();
        hump
    });
    humped
        || name.contains('_')
        || name.chars().any(|character| extra.contains(&character))
        || name_syntax::written_name_forms(name)
            .iter()
            .any(|form| form.module.is_some())
}

/// The question with every backticked span and every code-shaped name the
/// extractor read blanked to spaces of the same length, so a cue word inside
/// a name (`unused` in `unused_helper`) never routes, and every offset still
/// points at the same text.
fn cue_text(question: &str, extracted: &Extracted) -> String {
    let mut blank = vec![false; question.len()];
    let mut inside: Option<usize> = None;
    for (at, character) in question.char_indices() {
        if character != '`' {
            continue;
        }
        match inside.take() {
            Some(start) => blank[start..at + 1].iter_mut().for_each(|b| *b = true),
            None => inside = Some(at),
        }
    }
    let names = extracted
        .candidates
        .iter()
        .chain(extracted.qualifier.iter())
        .filter(|name| shaped_as_code(name));
    for name in names {
        for (at, found) in question.match_indices(name.as_str()) {
            let end = at + found.len();
            if bounded(question, at, end) {
                blank[at..end].iter_mut().for_each(|b| *b = true);
            }
        }
    }
    question
        .char_indices()
        .map(|(at, character)| {
            if blank[at] {
                " ".repeat(character.len_utf8())
            } else {
                character.to_string()
            }
        })
        .collect()
}

fn targets(question: &str, target: &str, candidates: &[String], intent: Intent) -> Vec<String> {
    if target.is_empty() {
        return Vec::new();
    }
    if intent == Intent::Declares {
        return vec![target.into()];
    }
    static JOIN: OnceLock<Regex> = OnceLock::new();
    let join = JOIN.get_or_init(|| {
        compiled(r"^[\s`()]*(?:,|and/or|\band\b|\bor\b)[\s,`()]*(?:and/or|and|or)?[\s`(]*$")
    });
    let mut positions = candidates
        .iter()
        .filter_map(|name| first_name_at(question, name).map(|(start, end)| (start, end, name)))
        .collect::<Vec<_>>();
    positions.sort_by_key(|(start, _, _)| *start);
    let mut best: Vec<String> = Vec::new();
    let mut current: Vec<String> = Vec::new();
    let mut previous_end = None;
    for (start, end, name) in positions {
        if previous_end
            .is_none_or(|previous| previous > start || !join.is_match(&question[previous..start]))
        {
            if current.len() > best.len() {
                best = std::mem::take(&mut current);
            }
            current.clear();
        }
        current.push(name.clone());
        previous_end = Some(end);
    }
    if current.len() > best.len() {
        best = current;
    }
    if best.len() >= 2
        && (best.iter().any(|name| name == target)
            || best.iter().any(|name| name == split_name(target).0))
    {
        best
    } else {
        vec![target.into()]
    }
}

/// Describe a question with deterministic rules, without accessing a repository.
pub fn plan(question: &str) -> Plan {
    plan_with_extracted(question, extract(question))
}

fn plan_with_extracted(question: &str, extracted: Extracted) -> Plan {
    static VERB: OnceLock<Regex> = OnceLock::new();
    static CUE: OnceLock<Regex> = OnceLock::new();
    static MORE_IMPL: OnceLock<Regex> = OnceLock::new();
    static SOURCE_WORD: OnceLock<Regex> = OnceLock::new();
    // Cues are read from the question's words, never from inside a name.
    let words = cue_text(question, &extracted);
    let matched = keyword_rules()
        .iter()
        .find_map(|(intent, pattern)| pattern.find(&words).map(|found| (*intent, found)));
    let mut intent = matched
        .as_ref()
        .map_or(Intent::Other, |(intent, _)| *intent);
    let mut cue = matched.map(|(_, found)| found.as_str().to_string());
    let mut target = extracted.candidates.first().cloned();
    let more = MORE_IMPL.get_or_init(|| {
        compiled(
            r"(?i)\b(?:another|other|any other|any more|additional|more)\s+implementations?\s+of\b",
        )
    });
    let source = SOURCE_WORD
        .get_or_init(|| compiled(r"(?i)\bshow\b|\bprint\b|source code|\bbody\b|\bcode\b"));
    let more_implementations =
        intent == Intent::ShowSource && more.is_match(&words) && !source.is_match(&words);
    let verb = VERB.get_or_init(|| {
        compiled(r"(?i)\b(?:declar\w*|defin\w*|implement\w*|re-?export\w*|export\w*|exposes?)\b")
    });
    let question_cue = CUE.get_or_init(|| compiled(r"(?i)\b(?:does|do|did|is|are|there|any|other|more|additional|else|besides|which|whether|also)\b"));
    let declares =
        if (intent == Intent::Other || more_implementations) && question_cue.is_match(&words) {
            verb.find(&words).and_then(|found| {
                declarations(question, &extracted.candidates).inspect(|declaration| {
                    intent = Intent::Declares;
                    target = Some(declaration.function.clone());
                    cue = Some(found.as_str().into());
                })
            })
        } else {
            None
        };
    let test_cue = test_exclusion(question).map(|(start, end)| question[start..end].to_string());
    let targets = targets(
        question,
        target.as_deref().unwrap_or(""),
        &extracted.candidates,
        intent,
    );
    Plan {
        intent,
        target,
        targets,
        paths_not_applied: extracted.paths(),
        candidates: extracted.candidates,
        qualifier: extracted.qualifier,
        scope_words: scope_words(question),
        include_tests: test_cue.is_none(),
        cue,
        test_cue,
        declares,
    }
}

#[cfg(test)]
#[path = "tests/route.rs"]
mod tests;
