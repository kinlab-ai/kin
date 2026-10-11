// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! How a language's top-level functions are created beside one another and
//! removed.
//!
//! Kin creates a function beside a current one, or as a new unit beside the
//! anchor's, and removes a top-level function, without reading or resending
//! anything around it. The planner that does it reads the source's tree
//! through this hook and never names a language: which statement declares a
//! function, what inside a function is a declaration of its own, what a new
//! unit must repeat from its neighbour, and how far apart two declarations
//! stand when nothing shows it.
//!
//! An adapter that implements the hook opts its language into the anchored
//! lifecycle, through [`crate::LanguageAdapter::declaration_lifecycle`].
//! Nothing here is read at ingest and nothing here is stored: the hook only
//! shapes how an edit is planned against source the graph already holds.

use std::ops::Range;
use std::sync::OnceLock;

use kin_model::LanguageId;
use tree_sitter::{Node, Tree};

use crate::languages::AdapterRegistry;

/// What a language tells the lifecycle planner about its top-level
/// functions.
pub trait DeclarationLifecycle: Send + Sync {
    /// The function `statement` declares at the top of a unit, when it
    /// declares one: `statement` is a direct child of the tree's root, and
    /// the result is the node whose `name` field names the function and
    /// whose bytes end where the function's entity span ends. A statement
    /// that wraps the function, a decorator or an `export`, yields the
    /// function inside it.
    fn top_level_function<'t>(&self, statement: Node<'t>) -> Option<Node<'t>>;

    /// Whether `node`, found inside a top-level function, declares something
    /// of its own: a nested function, a type, an item. The lifecycle creates
    /// and removes leaf functions only.
    fn is_nested_declaration(&self, node: &Node) -> bool;

    /// The first byte of the function `statement` declares, counting the doc
    /// comment the language reads as the function's own, which goes with the
    /// function when it is removed and may lead its code when it is created.
    /// By default the statement's own first byte.
    fn declaration_start(&self, statement: Node, source: &[u8]) -> usize {
        let _ = source;
        statement.start_byte()
    }

    /// What a new unit of this language must open with, repeated from the
    /// unit at `tree`: Go's package clause. `None` when a unit needs nothing
    /// ahead of its declarations. A refusal is one sentence.
    fn unit_prelude(&self, tree: &Tree, source: &[u8]) -> Result<Option<String>, String> {
        let _ = (tree, source);
        Ok(None)
    }

    /// Refuse a new unit beside `owner`, the unit the anchor is declared in,
    /// whose bytes are `source`, when this language cannot place one there.
    /// A refusal is one sentence that says what to do instead.
    fn check_new_unit_owner(&self, owner: &str, source: &[u8]) -> Result<(), String> {
        let _ = (owner, source);
        Ok(())
    }

    /// The extension a new unit beside `owner` is written under.
    fn new_unit_extension(&self, owner: &str) -> String;

    /// How many blank lines stand between two top-level declarations when
    /// none of the anchor's neighbours shows it.
    fn sibling_separation(&self) -> usize {
        1
    }

    /// Whether a top-level name belongs to every unit of one directory, as a
    /// Go package's names do, rather than to its own unit alone.
    fn package_scoped_names(&self) -> bool {
        false
    }
}

/// One top-level function proven in a unit's syntax.
#[derive(Debug, Clone)]
pub struct ProvenFunction<'t> {
    /// The function's entity span, as the adapter records it.
    pub span: Range<usize>,
    /// The bytes that go with the function when it is removed and that its
    /// code may hold when it is created: its whole statement, from the doc
    /// comment the language reads as its own to its last byte.
    pub extent: Range<usize>,
    /// The function's own node, as [`DeclarationLifecycle::top_level_function`]
    /// found it.
    pub declaration: Node<'t>,
}

/// The lifecycle hook `language`'s adapter implements, or `None` when no
/// registered adapter handles it or its adapter implements none.
pub fn declaration_lifecycle_for(
    language: LanguageId,
) -> Option<&'static dyn DeclarationLifecycle> {
    registry()
        .get_by_language(language)?
        .declaration_lifecycle()
}

/// The languages whose adapter implements the lifecycle hook, as a sentence
/// names them, the last joined by `conjunction`: `TypeScript, JavaScript,
/// Python, Go and Rust`.
pub fn lifecycle_languages(conjunction: &str) -> String {
    languages_named(
        |adapter| adapter.declaration_lifecycle().is_some(),
        conjunction,
    )
}

/// The languages whose adapter `has` a capability, in registration order,
/// as a sentence names them, the last joined by `conjunction`.
pub fn languages_named(
    has: impl Fn(&dyn crate::LanguageAdapter) -> bool,
    conjunction: &str,
) -> String {
    let registry = registry();
    let names: Vec<&str> = registry
        .supported_languages()
        .into_iter()
        .filter(|language| {
            registry
                .get_by_language(*language)
                .is_some_and(|adapter| has(adapter))
        })
        .map(|language| language.profile().display_name)
        .collect();
    match names.split_last() {
        None => String::new(),
        Some((last, [])) => (*last).to_string(),
        Some((last, rest)) => format!("{} {conjunction} {last}", rest.join(", ")),
    }
}

fn registry() -> &'static AdapterRegistry {
    static REGISTRY: OnceLock<AdapterRegistry> = OnceLock::new();
    REGISTRY.get_or_init(AdapterRegistry::new)
}

/// Prove that the function whose entity spans `span` in `tree` is declared
/// by one top-level statement, and return what goes with it.
///
/// `name` is the function's own identifier. The statement must hold the
/// function's whole declaration prefix: a decorator or an attribute the
/// adapter widens the span over sits inside the span, and a prefix outside
/// it, an `export`, sits inside the statement.
pub fn prove_top_level_function<'t>(
    lifecycle: &dyn DeclarationLifecycle,
    tree: &'t Tree,
    source: &[u8],
    span: &Range<usize>,
    name: &str,
) -> Option<ProvenFunction<'t>> {
    let root = tree.root_node();
    let mut cursor = root.walk();
    let (statement, declaration) = root.named_children(&mut cursor).find_map(|statement| {
        let declaration = lifecycle.top_level_function(statement)?;
        // The span ends with the declaration, or with the statement it stands
        // alone in (`const f = () => 1;` ends with its semicolon).
        (declaration.end_byte() <= span.end
            && span.end <= statement.end_byte()
            && declaration.start_byte() >= span.start
            && declaration
                .child_by_field_name("name")
                .is_some_and(|name_node| name_node.utf8_text(source).ok() == Some(name)))
        .then_some((statement, declaration))
    })?;
    let start = lifecycle
        .declaration_start(statement, source)
        .min(statement.start_byte())
        .min(span.start);
    let end = statement.end_byte().max(span.end);
    Some(ProvenFunction {
        span: span.clone(),
        extent: start..end,
        declaration,
    })
}

/// The first declaration nested in `declaration`, a top-level function, read
/// from the syntax: some adapters do not extract declarations nested in
/// function bodies, so the leaf property is proven from the tree too.
pub fn first_nested_declaration<'t>(
    lifecycle: &dyn DeclarationLifecycle,
    declaration: Node<'t>,
) -> Option<Node<'t>> {
    let mut pending = Vec::new();
    let mut cursor = declaration.walk();
    pending.extend(declaration.named_children(&mut cursor));
    while let Some(node) = pending.pop() {
        if lifecycle.is_nested_declaration(&node) {
            return Some(node);
        }
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
    None
}

/// The leading comment lines directly above `statement`, with no blank line
/// between them or between the last of them and the statement: where a
/// language that reads such comments as the declaration's own documentation
/// starts it. `opens` decides which comments count.
pub fn leading_comment_start(
    statement: Node,
    source: &[u8],
    opens: impl Fn(&str) -> bool,
) -> usize {
    let mut start = statement.start_byte();
    let mut previous = statement.prev_sibling();
    while let Some(comment) = previous {
        if !comment.kind().contains("comment") {
            break;
        }
        let text = comment.utf8_text(source).unwrap_or_default();
        let between = &source[comment.end_byte()..start];
        let lines_between = between.iter().filter(|byte| **byte == b'\n').count();
        if !opens(text)
            || lines_between != 1
            || !between.iter().all(u8::is_ascii_whitespace)
            || !source[line_start(source, comment.start_byte())..comment.start_byte()]
                .iter()
                .all(|byte| matches!(byte, b' ' | b'\t'))
        {
            break;
        }
        start = comment.start_byte();
        previous = comment.prev_sibling();
    }
    start
}

fn line_start(source: &[u8], at: usize) -> usize {
    source[..at.min(source.len())]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |newline| newline + 1)
}

/// The extension of `owner`'s last path segment, when it has one.
pub fn owner_extension(owner: &str) -> Option<&str> {
    let file = owner.rsplit('/').next().unwrap_or(owner);
    let (stem, extension) = file.rsplit_once('.')?;
    (!stem.is_empty() && !extension.is_empty()).then_some(extension)
}
