// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Where a declaration stands among a type's members or a unit's
//! declarations, as its language writes it.
//!
//! A method is added to a type by naming a method already there, or the type
//! itself, and sending the new method alone; a type or a function is added
//! after a top-level one the same way; a method is removed by naming it.
//! Nothing is read back or resent around it. The planner that does it is
//! language-neutral (kin-mcp's `member_placement`): it reads the source's
//! syntax through this hook, which says where a declaration stands, what its
//! body is indented by, where a type's members end, and whether text a caller
//! sent is exactly the one declaration it names.
//!
//! An adapter that implements the hook opts its language into member
//! placement, through [`crate::LanguageAdapter::member_placement`]. Nothing
//! here is read at ingest and nothing here is stored.

use std::ops::Range;
use std::sync::OnceLock;

use kin_model::LanguageId;

use crate::languages::AdapterRegistry;

/// What a located declaration declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// A function or a method.
    Function,
    /// A class, struct, or other type that holds members.
    Class,
}

impl Shape {
    pub fn label(self) -> &'static str {
        match self {
            Self::Function => "function",
            Self::Class => "class",
        }
    }
}

/// A declaration proven in a unit's exact bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Site {
    /// Its entity span, as the graph holds it.
    pub span: Range<usize>,
    /// The bytes that go with it when it is removed: its span and whatever its
    /// language reads as its own around it, a doc comment or an `export`.
    pub extent: Range<usize>,
    pub shape: Shape,
    /// The whitespace before its first line.
    pub indent: String,
    /// Whether it is a statement of the unit itself.
    pub top_level: bool,
    /// Whether it is a member of a type: a statement of a type's body, or a
    /// method its language declares outside its type.
    pub member: bool,
    /// The whitespace before the first statement of its own body, when that
    /// statement starts a line of its own.
    pub body_indent: Option<String>,
    /// Whether taking it out would leave the scope it is written in without
    /// a statement its language requires there.
    pub only_statement: bool,
    /// For a type whose body takes members: where a new last member is
    /// written after, the end of the last member. `None` for a type whose
    /// members are written elsewhere, and for anything not a type.
    pub members_end: Option<usize>,
}

/// What a language tells the member-placement planner.
pub trait MemberPlacement: Send + Sync {
    /// Prove that `span` of `source` is one function, method or type, written
    /// as a statement of its unit or as a member of a type. A refusal is one
    /// sentence.
    fn locate(&self, source: &[u8], span: Range<usize>) -> Result<Site, String>;

    /// Check that `text`, written at the left margin, is exactly one
    /// declaration of `shape` named `name`: as a member of a type when
    /// `member`, else as a statement of a unit. Returns the byte of `text`
    /// where the declaration's entity span starts: past a doc comment or a
    /// decorator its language keeps outside the span.
    fn check_one_declaration(
        &self,
        text: &str,
        name: &str,
        shape: Shape,
        member: bool,
    ) -> Result<usize, String>;

    /// The lines of `text`, a declaration at the left margin read as `member`
    /// or not, whose bytes are content and are written exactly as sent: the
    /// lines that begin inside a string literal that is laid out with no code
    /// around it.
    fn verbatim_lines(&self, text: &str, member: bool) -> Result<Vec<bool>, String>;

    /// The text a declaration of `member` standing is read inside of when it
    /// is checked on its own, as the opening and closing text around it: a
    /// TypeScript method needs a class body around it to parse.
    fn shell(&self, member: bool) -> (&'static str, &'static str) {
        let _ = member;
        ("", "")
    }

    /// Whether `source` parses as complete valid source.
    fn parses(&self, source: &[u8]) -> bool;

    /// What is written between the declaration at `site` and one created
    /// directly after it: its line break and the blank lines that set two
    /// such declarations apart.
    fn gap_after(&self, site: &Site) -> &'static str {
        let _ = site;
        "\n\n"
    }

    /// The sentence that says where a member of the type at `site` is
    /// created instead, when `site` is a type whose members are not written
    /// in its body.
    fn members_elsewhere(&self, site: &Site) -> String {
        let _ = site;
        "placement member needs a type whose body holds its methods as its anchor".into()
    }
}

/// The member-placement hook `language`'s adapter implements, or `None`
/// when no registered adapter handles it or its adapter implements none.
pub fn member_placement_for(language: LanguageId) -> Option<&'static dyn MemberPlacement> {
    static REGISTRY: OnceLock<AdapterRegistry> = OnceLock::new();
    REGISTRY
        .get_or_init(AdapterRegistry::new)
        .get_by_language(language)?
        .member_placement()
}

/// The languages whose adapter implements member placement, as a sentence
/// names them, the last joined by `conjunction`.
pub fn member_placement_languages(conjunction: &str) -> String {
    crate::declaration_lifecycle::languages_named(
        |adapter| adapter.member_placement().is_some(),
        conjunction,
    )
}

/// The whitespace between the start of the line `at` sits on and `at`, or
/// `None` when anything else stands before it on that line.
pub fn indent_before(source: &[u8], at: usize) -> Option<String> {
    let line_start = source[..at]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |newline| newline + 1);
    let lead = &source[line_start..at];
    lead.iter()
        .all(|byte| *byte == b' ' || *byte == b'\t')
        .then(|| String::from_utf8_lossy(lead).into_owned())
}

/// The lines of `text`, read as `language` inside the `open` and `close`
/// text of a shell, that begin inside a string literal: their bytes are the
/// string's. The first line never does.
pub fn string_lines(
    language: LanguageId,
    (open, close): (&str, &str),
    text: &str,
) -> Result<Vec<bool>, String> {
    let wrapped = format!("{open}{text}{close}");
    let reading = crate::syntax_reading::read_syntax(language, wrapped.as_bytes())
        .ok_or("the code sent could not be read")?;
    let mut starts = vec![0usize];
    starts.extend(
        text.bytes()
            .enumerate()
            .filter(|(_, byte)| *byte == b'\n')
            .map(|(at, _)| at + 1),
    );
    Ok(starts
        .iter()
        .map(|start| *start > 0 && reading.starts_inside_string(open.len() + start))
        .collect())
}

/// Where, in `text` read as `language` inside the `open` and `close` text of
/// a shell, the entity its adapter extracts for the declaration named `name`
/// starts: the byte the entity's span starts at, which is where the graph
/// will start it once the text is written into a unit. `None` when the
/// adapter extracts no such entity from it.
pub fn entity_start(
    language: LanguageId,
    (open, close): (&str, &str),
    text: &str,
    name: &str,
) -> Option<usize> {
    static REGISTRY: OnceLock<AdapterRegistry> = OnceLock::new();
    let adapter = REGISTRY
        .get_or_init(AdapterRegistry::new)
        .get_by_language(language)?;
    let wrapped = format!("{open}{text}{close}");
    let tree = adapter.parse(wrapped.as_bytes()).ok()?;
    let parsed = adapter
        .extract(
            &tree,
            wrapped.as_bytes(),
            &kin_model::FilePathId::new("kin-shell"),
        )
        .ok()?;
    let separator = crate::name_syntax::name_syntax_for(language)
        .unwrap_or(crate::name_syntax::NameSyntax::UNSTATED)
        .member_separator;
    let suffix = format!("{separator}{name}");
    parsed
        .entities
        .iter()
        .filter(|entity| entity.name == name || entity.name.ends_with(&suffix))
        .map(|entity| entity.span.start_byte)
        .filter(|start| *start >= open.len() && *start < open.len() + text.len())
        .min()
        .map(|start| start - open.len())
}
