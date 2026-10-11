// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The facts about a language that code without a parser needs.
//!
//! A crate that reads the graph but never parses source still has to say a
//! language's name in a sentence, know whether its indentation is syntax, and
//! know how its module-level code marks the entities a module holds. Each of
//! those used to be a `match` on the language in the file that needed it.
//! They are one record per language here, read through
//! [`LanguageId::profile`], so a reader asks the record and never names a
//! language.
//!
//! Behaviour that needs a syntax tree lives on the parser's language adapter
//! instead. Nothing here is read at ingest and nothing here is stored: the
//! wire name is the one [`LanguageId`]'s `Display` has always written.

use crate::derivation::ModuleCodeSyntax;
use crate::LanguageId;

/// How a language closes the block a declaration opens, for a reader that
/// grows an excerpt to the end of one without parsing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockStyle {
    /// The block is the lines indented under its header.
    Indentation,
    /// The block runs to the brace that closes the one it opens.
    Braces,
    /// The block runs to the `end` keyword that closes it.
    EndKeyword,
}

/// The facts about one language that a reader without a parser needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LanguageProfile {
    /// The language as a sentence names it: `TypeScript`, `C++`.
    pub display_name: &'static str,
    /// The language as Kin writes it in a record or on the wire:
    /// `typescript`, `cpp`.
    pub wire_name: &'static str,
    /// Whether the language reads indentation as structure, so that code
    /// moved to another indentation must move as one block and a line laid
    /// out wrongly is a syntax error no token shows.
    pub indentation_is_syntax: bool,
    /// How the language's module-level code marks each entity a module
    /// holds, or `None` for a language whose modules are served as a member
    /// inventory only. A language joins by naming a comment its grammar
    /// accepts wherever a top-level declaration may stand.
    pub module_code: Option<ModuleCodeSyntax>,
    /// How a block the language opens is closed.
    pub block_style: BlockStyle,
}

const fn braces(display_name: &'static str, wire_name: &'static str) -> LanguageProfile {
    LanguageProfile {
        display_name,
        wire_name,
        indentation_is_syntax: false,
        module_code: None,
        block_style: BlockStyle::Braces,
    }
}

pub const TYPESCRIPT: LanguageProfile = braces("TypeScript", "typescript");
pub const JAVASCRIPT: LanguageProfile = braces("JavaScript", "javascript");
pub const PYTHON: LanguageProfile = LanguageProfile {
    display_name: "Python",
    wire_name: "python",
    indentation_is_syntax: true,
    module_code: Some(ModuleCodeSyntax {
        marker_open: "# ",
        marker_close: "",
    }),
    block_style: BlockStyle::Indentation,
};
pub const GO: LanguageProfile = braces("Go", "go");
pub const JAVA: LanguageProfile = braces("Java", "java");
pub const RUST: LanguageProfile = braces("Rust", "rust");
pub const C: LanguageProfile = braces("C", "c");
pub const CPP: LanguageProfile = braces("C++", "cpp");
pub const CSHARP: LanguageProfile = braces("C#", "csharp");
pub const RUBY: LanguageProfile = LanguageProfile {
    block_style: BlockStyle::EndKeyword,
    ..braces("Ruby", "ruby")
};
pub const PHP: LanguageProfile = braces("PHP", "php");
pub const SWIFT: LanguageProfile = braces("Swift", "swift");
pub const KOTLIN: LanguageProfile = braces("Kotlin", "kotlin");
pub const HCL: LanguageProfile = braces("HCL", "hcl");

impl LanguageId {
    /// This language's profile. Every language has one, so a new
    /// [`LanguageId`] does not compile until its facts are stated.
    pub const fn profile(self) -> &'static LanguageProfile {
        match self {
            LanguageId::TypeScript => &TYPESCRIPT,
            LanguageId::JavaScript => &JAVASCRIPT,
            LanguageId::Python => &PYTHON,
            LanguageId::Go => &GO,
            LanguageId::Java => &JAVA,
            LanguageId::Rust => &RUST,
            LanguageId::C => &C,
            LanguageId::Cpp => &CPP,
            LanguageId::CSharp => &CSHARP,
            LanguageId::Ruby => &RUBY,
            LanguageId::Php => &PHP,
            LanguageId::Swift => &SWIFT,
            LanguageId::Kotlin => &KOTLIN,
            LanguageId::Hcl => &HCL,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every language's five facts, as each was written where it was read
    /// before the profile held them: the sentence name in kin-mcp's syntax
    /// gate, the wire name in `Display`, indentation in kin-parser's syntax
    /// reading, the module-code marker in `module_code_syntax`, and the block
    /// style in kin-mcp's excerpt growth.
    #[test]
    fn every_language_keeps_the_facts_its_readers_had() {
        use BlockStyle::{Braces, EndKeyword, Indentation};
        let expected = [
            (
                LanguageId::TypeScript,
                "TypeScript",
                "typescript",
                false,
                Braces,
            ),
            (
                LanguageId::JavaScript,
                "JavaScript",
                "javascript",
                false,
                Braces,
            ),
            (LanguageId::Python, "Python", "python", true, Indentation),
            (LanguageId::Go, "Go", "go", false, Braces),
            (LanguageId::Java, "Java", "java", false, Braces),
            (LanguageId::Rust, "Rust", "rust", false, Braces),
            (LanguageId::C, "C", "c", false, Braces),
            (LanguageId::Cpp, "C++", "cpp", false, Braces),
            (LanguageId::CSharp, "C#", "csharp", false, Braces),
            (LanguageId::Ruby, "Ruby", "ruby", false, EndKeyword),
            (LanguageId::Php, "PHP", "php", false, Braces),
            (LanguageId::Swift, "Swift", "swift", false, Braces),
            (LanguageId::Kotlin, "Kotlin", "kotlin", false, Braces),
            (LanguageId::Hcl, "HCL", "hcl", false, Braces),
        ];
        for (language, display, wire, indentation, block) in expected {
            let profile = language.profile();
            // Python alone marks its module's entities, with a line comment.
            let module_code = (language == LanguageId::Python).then_some(ModuleCodeSyntax {
                marker_open: "# ",
                marker_close: "",
            });
            assert_eq!(
                (
                    profile.display_name,
                    profile.wire_name,
                    profile.indentation_is_syntax,
                    profile.module_code,
                    profile.block_style,
                ),
                (display, wire, indentation, module_code, block),
                "{language:?}"
            );
            assert_eq!(language.to_string(), wire);
            assert_eq!(crate::module_code_syntax(language), module_code);
        }
    }
}
