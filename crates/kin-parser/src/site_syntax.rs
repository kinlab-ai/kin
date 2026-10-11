// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! How a language writes the text around one call or use site.
//!
//! A reader that quotes a site as written, from its caller's stored body and
//! without parsing that body again, needs a few facts only the language knows:
//! where a comment or a string starts, which names a method body uses for the
//! instance it runs on, and which global names the language or its runtime
//! binds before any repository code runs. Adapters state them here as one
//! generic record through [`crate::LanguageAdapter::site_syntax`], and a shared
//! reader applies the record to whichever language's adapter fills it.
//!
//! Nothing here is read at ingest and nothing here is stored: the record only
//! shapes how an answer quotes text the graph already holds.

use std::sync::OnceLock;

use kin_model::LanguageId;

use crate::languages::AdapterRegistry;

/// The syntax facts a site reader needs about one language.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SiteSyntax {
    /// Tokens that start a comment running to the end of its line.
    pub line_comments: &'static [&'static str],
    /// The opening and closing tokens of a block comment, when the language
    /// has one.
    pub block_comment: Option<(&'static str, &'static str)>,
    /// Characters that open a string literal, each closed by itself. A
    /// backslash escapes the character after it.
    pub string_quotes: &'static [char],
    /// Names a method body uses for the instance or class it runs on, or for
    /// its parent's members.
    pub instance_receivers: &'static [&'static str],
    /// Global names the language or its runtime binds before any repository
    /// code runs, whose members a repository does not declare.
    pub builtin_receivers: &'static [&'static str],
    /// The keyword written before a constructor call, when the language
    /// writes one (`new Stack()`), so a quoted call keeps it.
    pub constructor_keyword: Option<&'static str>,
    /// Whether a call of a bare name never reaches a member through an
    /// implicit instance: `foo()` in a Python or JavaScript method calls a
    /// name its scope binds, while in a Java or C++ method it may call
    /// `this.foo()`.
    pub bare_calls_reach_no_member: bool,
}

impl SiteSyntax {
    /// What a reader assumes for a language whose adapter states nothing: no
    /// comment syntax, double-quoted strings only, no instance or builtin
    /// names, so no receiver is read as one, and a bare call that may reach a
    /// member through an implicit instance.
    pub const UNSTATED: Self = Self {
        line_comments: &[],
        block_comment: None,
        string_quotes: &['"'],
        instance_receivers: &[],
        builtin_receivers: &[],
        constructor_keyword: None,
        bare_calls_reach_no_member: false,
    };
}

/// Python's site syntax.
pub const PYTHON: SiteSyntax = SiteSyntax {
    line_comments: &["#"],
    block_comment: None,
    string_quotes: &['"', '\''],
    instance_receivers: &["self", "cls", "super"],
    builtin_receivers: &[
        "bool",
        "bytearray",
        "bytes",
        "complex",
        "dict",
        "float",
        "frozenset",
        "int",
        "list",
        "memoryview",
        "object",
        "range",
        "set",
        "str",
        "tuple",
        "type",
    ],
    constructor_keyword: None,
    bare_calls_reach_no_member: true,
};

/// The site syntax JavaScript and TypeScript share.
pub const ECMASCRIPT: SiteSyntax = SiteSyntax {
    line_comments: &["//"],
    block_comment: Some(("/*", "*/")),
    string_quotes: &['"', '\'', '`'],
    instance_receivers: &["this", "super"],
    builtin_receivers: &[
        "Array",
        "BigInt",
        "Boolean",
        "Buffer",
        "Date",
        "Intl",
        "JSON",
        "Map",
        "Math",
        "Number",
        "Object",
        "Promise",
        "Proxy",
        "Reflect",
        "RegExp",
        "Set",
        "String",
        "Symbol",
        "WeakMap",
        "WeakSet",
        "console",
        "document",
        "globalThis",
        "process",
        "window",
    ],
    constructor_keyword: Some("new"),
    bare_calls_reach_no_member: true,
};

/// The site syntax `language`'s adapter states, or `None` when no registered
/// adapter handles it or its adapter states none.
pub fn site_syntax_for(language: LanguageId) -> Option<SiteSyntax> {
    static REGISTRY: OnceLock<AdapterRegistry> = OnceLock::new();
    REGISTRY
        .get_or_init(AdapterRegistry::new)
        .get_by_language(language)?
        .site_syntax()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_and_ecmascript_adapters_state_their_site_syntax() {
        assert_eq!(site_syntax_for(LanguageId::Python), Some(PYTHON));
        assert_eq!(site_syntax_for(LanguageId::JavaScript), Some(ECMASCRIPT));
        assert_eq!(site_syntax_for(LanguageId::TypeScript), Some(ECMASCRIPT));
        // A Python or JavaScript method calls a sibling only through its
        // instance; a language that states nothing may do it bare.
        const {
            assert!(PYTHON.bare_calls_reach_no_member && ECMASCRIPT.bare_calls_reach_no_member);
            assert!(!SiteSyntax::UNSTATED.bare_calls_reach_no_member);
        }
    }

    #[test]
    fn an_adapter_that_states_nothing_reads_as_none() {
        assert_eq!(site_syntax_for(LanguageId::Hcl), None);
    }
}
