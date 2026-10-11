// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! How a language spells a declaration's name: as Kin stores it, and as a
//! person writes it in a question.
//!
//! A person asking about code writes a name the way their language's tools
//! print it: `(*Server).Handle` for a Go method on a pointer receiver,
//! `crate::store::Store::get` or `Store.get` for a Rust method,
//! `Store.prototype.containsElement` for a JavaScript method,
//! `flask.app.Flask.run` for a Python one. Kin stores each of those under one
//! spelling, `Server.Handle`, `Store::get`, `Store.containsElement` and
//! `Flask.run` in a module, and its name resolver already takes the stored
//! spelling with either owner separator and with a module in front.
//!
//! What only the language knows is how a written form folds into that stored
//! spelling: which separator joins an owner and its member, which leading
//! segments name no module, which wrappers around an owner are syntax and not
//! name, and which segments sit between an owner and its member without being
//! part of either. Adapters state those facts as one generic record through
//! [`crate::LanguageAdapter::name_syntax`], and [`written_name_forms`] applies
//! every stated record to a written name, so the reader matches on no
//! language.
//!
//! Nothing here is read at ingest and nothing here is stored: the record only
//! shapes how a written name is looked up in the graph.

use std::sync::OnceLock;

use kin_model::LanguageId;

use crate::languages::AdapterRegistry;

/// The name-spelling facts a reader of written names needs about one
/// language.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NameSyntax {
    /// What joins an owner and its member in the name Kin stores for this
    /// language: `Flask.run`, `Server.Handle`, `Store::get`.
    pub member_separator: &'static str,
    /// What a person writes between the segments of a name: between a module
    /// and a name, and between an owner and its member. Includes
    /// [`Self::member_separator`].
    pub written_separators: &'static [&'static str],
    /// Characters an identifier may carry beyond letters, digits and `_`.
    pub identifier_extra: &'static [char],
    /// Characters a module segment may carry beyond an identifier's, as an
    /// npm package name carries `-` and `@`.
    pub module_segment_extra: &'static [char],
    /// Leading written segments that name no module in Kin's spelling, as a
    /// Rust path's `crate`, `self` and `super` do.
    pub path_roots: &'static [&'static str],
    /// Written forms of an owner that fold to the owner itself: the opening
    /// and closing text around it, as a Go method expression writes
    /// `(*Server)` for `Server`.
    pub owner_wrappers: &'static [(&'static str, &'static str)],
    /// Segments a person writes between an owner and its member that the
    /// stored name leaves out, as JavaScript's `prototype` in
    /// `Store.prototype.containsElement`.
    pub member_infixes: &'static [&'static str],
}

impl NameSyntax {
    /// What a reader assumes for a language whose adapter states nothing:
    /// `.` between every segment and nothing to fold.
    pub const UNSTATED: Self = Self {
        member_separator: ".",
        written_separators: &["."],
        identifier_extra: &[],
        module_segment_extra: &[],
        path_roots: &[],
        owner_wrappers: &[],
        member_infixes: &[],
    };

    /// Whether `segment` is one identifier in this language.
    pub fn is_identifier(&self, segment: &str) -> bool {
        let mut characters = segment.chars();
        characters.next().is_some_and(|first| {
            first.is_alphabetic() || first == '_' || self.identifier_extra.contains(&first)
        }) && characters.all(|rest| self.identifier_character(rest))
    }

    fn identifier_character(&self, character: char) -> bool {
        character.is_alphanumeric()
            || character == '_'
            || self.identifier_extra.contains(&character)
    }

    /// Whether `segment` can be one segment of a module's name in this
    /// language.
    fn is_module_segment(&self, segment: &str) -> bool {
        !segment.is_empty()
            && segment.chars().all(|character| {
                self.identifier_character(character)
                    || self.module_segment_extra.contains(&character)
            })
            && !segment.starts_with('-')
            && !segment.ends_with('-')
    }

    /// The spellings `written` could be held under, by this record alone, the
    /// most name segments, and so the fewest module segments, first. Empty
    /// when `written` is not a name this language writes.
    pub fn forms(&self, written: &str) -> Vec<NameForm> {
        let Some(segments) = self.segments(written) else {
            return Vec::new();
        };
        let mut forms = Vec::new();
        for named in (1..=segments.len()).rev() {
            let (module, name) = segments.split_at(segments.len() - named);
            if !name.iter().all(|segment| self.is_identifier(segment)) {
                continue;
            }
            forms.push(NameForm {
                module: (!module.is_empty()).then(|| module.join(".")),
                name: name.join(self.member_separator),
            });
        }
        forms
    }

    /// `written` as its segments, with owner wrappers folded, leading path
    /// roots and member infixes dropped, or `None` when a segment is not one
    /// this language writes.
    fn segments(&self, written: &str) -> Option<Vec<String>> {
        let written = written.trim().trim_matches('`');
        let written = written.strip_suffix("()").unwrap_or(written);
        if written.is_empty() || written.chars().any(char::is_whitespace) {
            return None;
        }
        let folded = self.fold_owner_wrappers(written);
        let mut segments = split_on_separators(&folded, self.written_separators);
        if segments.iter().any(String::is_empty) {
            return None;
        }
        let roots = segments
            .iter()
            .take_while(|segment| self.path_roots.contains(&segment.as_str()))
            .count();
        segments.drain(..roots);
        let last = segments.len().saturating_sub(1);
        let mut kept = Vec::with_capacity(segments.len());
        for (index, segment) in segments.into_iter().enumerate() {
            let inner = index > 0 && index < last;
            if inner && self.member_infixes.contains(&segment.as_str()) {
                continue;
            }
            kept.push(segment);
        }
        let (name, module) = kept.split_last()?;
        (self.is_identifier(name) && module.iter().all(|segment| self.is_module_segment(segment)))
            .then_some(kept)
    }

    /// `written` with every wrapped owner, such as `(*Server)`, replaced by
    /// the owner it wraps. A wrapper whose inside is not a written name is
    /// left as it is, and the name then fails to read.
    fn fold_owner_wrappers(&self, written: &str) -> String {
        let mut folded = written.to_string();
        for (open, close) in self.owner_wrappers {
            let mut searched = 0;
            while let Some(start) = folded[searched..].find(open).map(|at| at + searched) {
                let inner_start = start + open.len();
                let Some(inner_len) = folded[inner_start..].find(close) else {
                    break;
                };
                let inner = folded[inner_start..inner_start + inner_len].to_string();
                let names_an_owner = !inner.is_empty()
                    && split_on_separators(&inner, self.written_separators)
                        .iter()
                        .all(|segment| self.is_module_segment(segment));
                if names_an_owner {
                    folded.replace_range(start..inner_start + inner_len + close.len(), &inner);
                    searched = start + inner.len();
                } else {
                    searched = inner_start;
                }
            }
        }
        folded
    }
}

/// `text` split at every separator in `separators`, the longest separator
/// taken first where two start at the same place.
fn split_on_separators(text: &str, separators: &[&str]) -> Vec<String> {
    let mut ordered: Vec<&str> = separators
        .iter()
        .copied()
        .filter(|s| !s.is_empty())
        .collect();
    ordered.sort_by_key(|separator| std::cmp::Reverse(separator.len()));
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut rest = text;
    'scan: while !rest.is_empty() {
        for separator in &ordered {
            if let Some(after) = rest.strip_prefix(separator) {
                segments.push(std::mem::take(&mut current));
                rest = after;
                continue 'scan;
            }
        }
        let mut characters = rest.chars();
        if let Some(character) = characters.next() {
            current.push(character);
        }
        rest = characters.as_str();
    }
    segments.push(current);
    segments
}

/// One spelling a written name could be held under: the name as Kin stores
/// it, and the module the written form put in front of it, dotted as rows
/// spell a `module`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NameForm {
    /// The module the written form named before the declaration, when it
    /// named one. A hint for choosing among same-named declarations, never an
    /// address on its own.
    pub module: Option<String>,
    /// The declaration's name in Kin's stored spelling for the language whose
    /// record produced it.
    pub name: String,
}

/// Python's name syntax.
pub const PYTHON: NameSyntax = NameSyntax::UNSTATED;

/// The name syntax JavaScript and TypeScript share.
pub const ECMASCRIPT: NameSyntax = NameSyntax {
    member_separator: ".",
    written_separators: &["."],
    identifier_extra: &['$'],
    module_segment_extra: &['-', '@'],
    path_roots: &[],
    owner_wrappers: &[],
    member_infixes: &["prototype"],
};

/// Go's name syntax.
pub const GO: NameSyntax = NameSyntax {
    member_separator: ".",
    written_separators: &["."],
    identifier_extra: &[],
    module_segment_extra: &[],
    path_roots: &[],
    owner_wrappers: &[("(*", ")"), ("(", ")")],
    member_infixes: &[],
};

/// Rust's name syntax.
pub const RUST: NameSyntax = NameSyntax {
    member_separator: "::",
    written_separators: &["::", "."],
    identifier_extra: &[],
    module_segment_extra: &[],
    path_roots: &["crate", "self", "super"],
    owner_wrappers: &[],
    member_infixes: &[],
};

/// Every registered adapter's stated name syntax, each distinct record once,
/// in registration order.
fn stated() -> &'static [NameSyntax] {
    static STATED: OnceLock<Vec<NameSyntax>> = OnceLock::new();
    STATED.get_or_init(|| {
        let registry = AdapterRegistry::new();
        let mut records: Vec<NameSyntax> = Vec::new();
        for language in registry.supported_languages() {
            let Some(record) = registry
                .get_by_language(language)
                .and_then(|adapter| adapter.name_syntax())
            else {
                continue;
            };
            if !records.contains(&record) {
                records.push(record);
            }
        }
        records
    })
}

/// The name syntax `language`'s adapter states, or `None` when no registered
/// adapter handles it or its adapter states none.
pub fn name_syntax_for(language: LanguageId) -> Option<NameSyntax> {
    static REGISTRY: OnceLock<AdapterRegistry> = OnceLock::new();
    REGISTRY
        .get_or_init(AdapterRegistry::new)
        .get_by_language(language)?
        .name_syntax()
}

/// Every spelling `written` could be held under in any language whose
/// adapter states a name syntax: the forms with the most name segments
/// first, each once. The first is the written name in a stored spelling
/// whenever one exists, so a reader that tries them in order asks the graph
/// for what the person wrote before it asks for any part of it.
///
/// Empty when no stated syntax reads `written` as a name.
pub fn written_name_forms(written: &str) -> Vec<NameForm> {
    // Each record lists its forms with the fewest module segments first.
    // Ordering every record's forms by that count, stably, keeps that order
    // across records, and registration order within one count.
    let mut ordered: Vec<(usize, NameForm)> = stated()
        .iter()
        .flat_map(|record| record.forms(written))
        .map(|form| {
            let depth = form
                .module
                .as_deref()
                .map_or(0, |module| module.split('.').count());
            (depth, form)
        })
        .collect();
    ordered.sort_by_key(|(depth, _)| *depth);
    let mut forms: Vec<NameForm> = Vec::with_capacity(ordered.len());
    for (_, form) in ordered {
        if !forms.contains(&form) {
            forms.push(form);
        }
    }
    forms
}

/// Whether any stated syntax reads `word` as one identifier.
pub fn is_written_identifier(word: &str) -> bool {
    stated().iter().any(|record| record.is_identifier(word))
}

/// Every separator a stated syntax writes between the segments of a name,
/// each once, the longest first, for a reader that has to find written names
/// in running text before it knows their language.
pub fn written_separators() -> Vec<&'static str> {
    let mut separators: Vec<&'static str> = Vec::new();
    for record in stated() {
        for separator in record.written_separators {
            if !separators.contains(separator) {
                separators.push(separator);
            }
        }
    }
    separators.sort_by_key(|separator| std::cmp::Reverse(separator.len()));
    separators
}

/// Every character a stated syntax lets an identifier carry beyond letters,
/// digits and `_`, each once.
pub fn identifier_extra_characters() -> Vec<char> {
    let mut extra: Vec<char> = Vec::new();
    for record in stated() {
        for character in record.identifier_extra {
            if !extra.contains(character) {
                extra.push(*character);
            }
        }
    }
    extra
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form(module: Option<&str>, name: &str) -> NameForm {
        NameForm {
            module: module.map(str::to_string),
            name: name.to_string(),
        }
    }

    #[test]
    fn the_five_adapters_state_their_name_syntax() {
        assert_eq!(name_syntax_for(LanguageId::Python), Some(PYTHON));
        assert_eq!(name_syntax_for(LanguageId::JavaScript), Some(ECMASCRIPT));
        assert_eq!(name_syntax_for(LanguageId::TypeScript), Some(ECMASCRIPT));
        assert_eq!(name_syntax_for(LanguageId::Go), Some(GO));
        assert_eq!(name_syntax_for(LanguageId::Rust), Some(RUST));
    }

    #[test]
    fn an_adapter_that_states_nothing_reads_as_none() {
        assert_eq!(name_syntax_for(LanguageId::Hcl), None);
    }

    #[test]
    fn a_go_pointer_receiver_folds_to_the_stored_owner() {
        assert_eq!(GO.forms("(*Server).Handle")[0], form(None, "Server.Handle"));
        assert_eq!(GO.forms("(Server).Handle")[0], form(None, "Server.Handle"));
        assert!(GO
            .forms("http.(*Server).Handle")
            .contains(&form(Some("http"), "Server.Handle")));
        let forms = written_name_forms("(*Server).Handle");
        assert_eq!(forms[0], form(None, "Server.Handle"));
    }

    #[test]
    fn a_rust_path_drops_its_root_and_joins_members_with_its_separator() {
        let forms = RUST.forms("crate::store::Store::get");
        assert_eq!(forms[0], form(None, "store::Store::get"));
        assert!(forms.contains(&form(Some("store"), "Store::get")));
        assert!(forms.contains(&form(Some("store.Store"), "get")));
        assert_eq!(
            RUST.forms("super::helpers::parse")[1],
            form(Some("helpers"), "parse")
        );
    }

    #[test]
    fn a_rust_method_written_with_a_dot_reaches_its_stored_spelling() {
        let forms = written_name_forms("Store.get");
        assert_eq!(forms[0], form(None, "Store.get"));
        assert!(forms.contains(&form(None, "Store::get")));
        assert!(forms.contains(&form(Some("Store"), "get")));
    }

    #[test]
    fn a_python_dotted_name_keeps_the_written_form_first() {
        let forms = written_name_forms("flask.app.Flask.run");
        assert_eq!(forms[0], form(None, "flask.app.Flask.run"));
        assert!(forms.contains(&form(Some("flask.app"), "Flask.run")));
        assert!(forms.contains(&form(Some("flask.app.Flask"), "run")));
    }

    #[test]
    fn a_javascript_prototype_member_drops_the_infix() {
        assert_eq!(
            ECMASCRIPT.forms("Store.prototype.containsElement")[0],
            form(None, "Store.containsElement")
        );
        assert!(written_name_forms("Store.prototype.containsElement")
            .contains(&form(None, "Store.containsElement")));
    }

    #[test]
    fn an_npm_package_module_keeps_its_hyphens_as_a_module_hint() {
        let forms = written_name_forms("react-reconciler.ReactFiberScope.collectScopedNodes");
        assert!(forms.contains(&form(
            Some("react-reconciler.ReactFiberScope"),
            "collectScopedNodes"
        )));
        assert!(forms.iter().all(|form| !form.name.contains('-')));
        assert!(written_name_forms("$scope.$apply").contains(&form(None, "$scope.$apply")));
    }

    #[test]
    fn a_call_written_with_parentheses_or_backticks_reads_as_its_name() {
        assert_eq!(
            written_name_forms("`advanceTimers()`")[0],
            form(None, "advanceTimers")
        );
    }

    #[test]
    fn what_is_not_a_written_name_has_no_form() {
        for written in [
            "",
            "hello world",
            "packages/react/src/index.js",
            "a..b",
            "-x.y",
            "3d",
            "(*).Handle",
        ] {
            assert!(written_name_forms(written).is_empty(), "{written:?}");
        }
    }

    #[test]
    fn identifiers_take_each_language_s_extra_characters() {
        assert!(is_written_identifier("parse_config"));
        assert!(is_written_identifier("$apply"));
        assert!(!is_written_identifier("react-reconciler"));
        assert!(!is_written_identifier("a.b"));
        assert_eq!(identifier_extra_characters(), vec!['$']);
    }

    #[test]
    fn written_separators_are_the_union_of_every_record_longest_first() {
        assert_eq!(written_separators(), vec!["::", "."]);
    }
}
