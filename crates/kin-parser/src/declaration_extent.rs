// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Where a declaration starts: the text above its own node that belongs to
//! it.
//!
//! An entity's span is what a reader is served and what an edit replaces or
//! removes, so it has to hold the whole declaration. A grammar often leaves
//! part of that outside the declaration's node: documentation written above
//! it, attributes that are sibling nodes, a keyword or wrapper the grammar
//! puts on a parent. A span taken from the node alone serves a function
//! without its documentation, and removing it leaves the documentation behind
//! with nothing to document.
//!
//! Which siblings belong to a declaration is each language's rule, so each
//! adapter classifies them; the walk and the span are shared here. The
//! entity's id does not move when its span widens, because the id keys on
//! the declaration's own line (see `ExtractedEntity::declaration_line`).

use kin_model::{FilePathId, SourceSpan};
use tree_sitter::Node;

use crate::adapter::span_from_node;

/// What a sibling above a declaration is to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Leading {
    /// It belongs to the declaration: documentation, an attribute.
    Owned,
    /// It may sit between the declaration and what it owns without ending
    /// the run, but it is not the declaration's on its own: an ordinary
    /// comment between a doc comment and its item.
    Passable,
    /// It ends the run.
    Stop,
}

/// The siblings above `node` that belong to it, in source order.
///
/// The walk reads `node`'s previous siblings while `classify` says they are
/// `Owned` or `Passable`. With `adjacent`, a sibling separated from the run
/// below it by a blank line ends the walk, which is how a language that
/// attaches documentation only when nothing stands between says so. Passable
/// siblings above the topmost owned one are not the declaration's, so the
/// run never starts with one.
pub fn leading_run<'t>(
    node: &Node<'t>,
    adjacent: bool,
    classify: impl Fn(&Node<'t>) -> Leading,
) -> Vec<Node<'t>> {
    let mut run: Vec<(Node<'t>, bool)> = Vec::new();
    let mut top_row = node.start_position().row;
    let mut prev = node.prev_sibling();
    while let Some(sibling) = prev {
        if adjacent && sibling.end_position().row + 1 < top_row {
            break;
        }
        let owned = match classify(&sibling) {
            Leading::Owned => true,
            Leading::Passable => false,
            Leading::Stop => break,
        };
        run.push((sibling, owned));
        top_row = sibling.start_position().row;
        prev = sibling.prev_sibling();
    }
    // The run was read bottom-up; whatever passable siblings sit above the
    // topmost owned one are not the declaration's.
    while matches!(run.last(), Some((_, false))) {
        run.pop();
    }
    run.into_iter().rev().map(|(sibling, _)| sibling).collect()
}

/// The comments directly above `node`, the rule most languages attach
/// documentation by: each comment starts its own line, and no blank line
/// stands between them or between the last of them and the declaration. A
/// comment trailing the code on the line above is that code's, not this
/// declaration's.
pub fn leading_comments<'t>(node: &Node<'t>, source: &[u8]) -> Vec<Node<'t>> {
    leading_run(node, true, |sibling| {
        if sibling.kind().contains("comment") && starts_its_line(source, sibling.start_byte()) {
            Leading::Owned
        } else {
            Leading::Stop
        }
    })
}

/// Whether only whitespace stands between the start of its line and `at`.
pub fn starts_its_line(source: &[u8], at: usize) -> bool {
    source[..at.min(source.len())]
        .iter()
        .rev()
        .take_while(|byte| **byte != b'\n')
        .all(|byte| byte.is_ascii_whitespace())
}

/// The span of the declaration at `node` with the comments directly above
/// it, and the line its id keys on when that span starts above it.
pub fn span_with_comments(
    node: &Node,
    source: &[u8],
    file_id: &FilePathId,
) -> (SourceSpan, Option<u32>) {
    let span = span_with_leading(node, &leading_comments(node, source), file_id);
    let line = declaration_line(node, &span);
    (span, line)
}

/// The span of the declaration at `node` together with the `leading` nodes
/// that belong to it: from the first of them, or the node itself, to the end
/// of the node.
pub fn span_with_leading(node: &Node, leading: &[Node], file_id: &FilePathId) -> SourceSpan {
    let mut span = span_from_node(node, file_id);
    if let Some(first) = leading.first() {
        let position = first.start_position();
        span.start_byte = first.start_byte();
        span.start_line = position.row as u32;
        span.start_col = position.column as u32;
    }
    span
}

/// The line an entity's id keys on when its span starts above the line its
/// declaration was keyed on before: `Some(row)` exactly when they differ.
pub fn declaration_line(keyed: &Node, span: &SourceSpan) -> Option<u32> {
    let row = keyed.start_position().row as u32;
    (span.start_line != row).then_some(row)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::LanguageAdapter;
    use crate::languages::GoAdapter;

    fn classify_comment(sibling: &Node) -> Leading {
        if sibling.kind() == "comment" {
            Leading::Owned
        } else {
            Leading::Stop
        }
    }

    fn first_function<'t>(tree: &'t tree_sitter::Tree) -> Node<'t> {
        let root = tree.root_node();
        let mut cursor = root.walk();
        let found = root
            .named_children(&mut cursor)
            .find(|node| node.kind() == "function_declaration")
            .expect("a function");
        found
    }

    #[test]
    fn the_run_reads_the_owned_siblings_in_source_order() {
        let source = b"package p\n\n// One.\n// Two.\nfunc f() {}\n";
        let tree = GoAdapter.parse(source).unwrap();
        let function = first_function(&tree);
        let run = leading_run(&function, true, classify_comment);
        let texts: Vec<&str> = run
            .iter()
            .map(|node| node.utf8_text(source).unwrap())
            .collect();
        assert_eq!(texts, ["// One.", "// Two."]);
        let span = span_with_leading(&function, &run, &FilePathId::new("p.go"));
        assert!(source[span.start_byte..].starts_with(b"// One."));
        assert_eq!(span.end_byte, function.end_byte());
        assert_eq!(declaration_line(&function, &span), Some(4));
    }

    #[test]
    fn a_blank_line_ends_an_adjacent_run() {
        let source = b"package p\n\n// Detached.\n\n// Attached.\nfunc f() {}\n";
        let tree = GoAdapter.parse(source).unwrap();
        let function = first_function(&tree);
        let run = leading_run(&function, true, classify_comment);
        assert_eq!(run.len(), 1);
        assert_eq!(run[0].utf8_text(source).unwrap(), "// Attached.");
        let loose = leading_run(&function, false, classify_comment);
        assert_eq!(
            loose.len(),
            2,
            "without adjacency the blank line is crossed"
        );
    }

    #[test]
    fn passable_siblings_never_start_the_run() {
        let source = b"package p\n\n// Plain.\nfunc f() {}\n";
        let tree = GoAdapter.parse(source).unwrap();
        let function = first_function(&tree);
        let run = leading_run(&function, true, |sibling| {
            if sibling.kind() == "comment" {
                Leading::Passable
            } else {
                Leading::Stop
            }
        });
        assert!(run.is_empty());
        let span = span_with_leading(&function, &run, &FilePathId::new("p.go"));
        assert_eq!(span.start_byte, function.start_byte());
        assert_eq!(declaration_line(&function, &span), None);
    }
}
