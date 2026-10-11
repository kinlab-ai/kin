// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! How a language writes the head of a declaration's body.
//!
//! A reader that wants a declaration's code without its documentation, from
//! the body text the graph holds and without parsing it again, needs a few
//! facts only the language knows: how a documentation string opening a body is
//! quoted, what ends a declaration's header, and what starts a decorator,
//! which is code. Adapters state them here through
//! [`crate::LanguageAdapter::body_syntax`], and a shared reader applies the
//! record to whichever language's adapter fills it.
//!
//! Nothing here is read at ingest and nothing here is stored.

use std::sync::OnceLock;

use kin_model::LanguageId;

use crate::languages::AdapterRegistry;

/// The facts a body reader needs about one language.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BodySyntax {
    /// Tokens that open a documentation string written as the first statement
    /// of a body, each closed by itself. Empty when the language documents a
    /// declaration elsewhere, such as in a comment above it.
    pub doc_string_quotes: &'static [&'static str],
    /// Letters that may stand before such a string, alone or in a pair, in
    /// either case.
    pub string_prefixes: &'static [char],
    /// Characters that end a declaration's header where its body begins.
    pub header_end: &'static [char],
    /// The character that starts a decorator written above a declaration.
    pub decorator: Option<char>,
}

/// Python's body syntax: a docstring is the body's first statement.
pub const PYTHON: BodySyntax = BodySyntax {
    doc_string_quotes: &["\"\"\"", "'''"],
    string_prefixes: &['r', 'b', 'u', 'f'],
    header_end: &[':'],
    decorator: Some('@'),
};

/// JavaScript's and TypeScript's body syntax: documentation is a comment above
/// the declaration, never a string in its body.
pub const ECMASCRIPT: BodySyntax = BodySyntax {
    doc_string_quotes: &[],
    string_prefixes: &[],
    header_end: &['{'],
    decorator: Some('@'),
};

/// The body syntax `language`'s adapter states, or `None` when no registered
/// adapter handles it or its adapter states none.
pub fn body_syntax_for(language: LanguageId) -> Option<BodySyntax> {
    static REGISTRY: OnceLock<AdapterRegistry> = OnceLock::new();
    REGISTRY
        .get_or_init(AdapterRegistry::new)
        .get_by_language(language)?
        .body_syntax()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_and_ecmascript_adapters_state_their_body_syntax() {
        assert_eq!(body_syntax_for(LanguageId::Python), Some(PYTHON));
        assert_eq!(body_syntax_for(LanguageId::JavaScript), Some(ECMASCRIPT));
        assert_eq!(body_syntax_for(LanguageId::TypeScript), Some(ECMASCRIPT));
    }
}
