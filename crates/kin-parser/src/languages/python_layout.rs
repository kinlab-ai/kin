// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! What Python refuses about the layout of lines, and which of its strings
//! are docstrings: the facts kin-parser's syntax reading asks the Python
//! adapter for ([`crate::LanguageAdapter::layout_problems`],
//! [`crate::LanguageAdapter::is_docstring`]).

use tree_sitter::Node;

use crate::syntax_reading::{SyntaxProblem, SyntaxProblemKind};

/// Whether `node`, a string or a piece of one, is a Python docstring: the
/// one expression of the first statement of a module or of a definition's
/// body.
pub(crate) fn is_python_docstring(node: Node) -> bool {
    let mut string = node;
    while string.kind() != "string" {
        match string.parent() {
            Some(parent) => string = parent,
            None => return false,
        }
    }
    string.parent().is_some_and(|statement| {
        statement.kind() == "expression_statement"
            && statement.named_child_count() == 1
            && statement.parent().is_some_and(|block| {
                matches!(block.kind(), "block" | "module")
                    && block
                        .named_children(&mut block.walk())
                        .find(|child| child.kind() != "comment")
                        .map(|first| first.id())
                        == Some(statement.id())
            })
    })
}

/// The whitespace a line carries ahead of `byte`, or `None` when `byte` is not
/// the first thing on its line.
fn indent_before(source: &[u8], byte: usize) -> Option<&[u8]> {
    let byte = byte.min(source.len());
    let line_start = source[..byte]
        .iter()
        .rposition(|at| *at == b'\n')
        .map_or(0, |newline| newline + 1);
    let indent = &source[line_start..byte];
    indent
        .iter()
        .all(|at| matches!(at, b' ' | b'\t'))
        .then_some(indent)
}

/// The whitespace the line holding `byte` opens with.
fn line_indent(source: &[u8], byte: usize) -> &[u8] {
    let byte = byte.min(source.len());
    let line_start = source[..byte]
        .iter()
        .rposition(|at| *at == b'\n')
        .map_or(0, |newline| newline + 1);
    let width = source[line_start..]
        .iter()
        .position(|at| !matches!(at, b' ' | b'\t'))
        .unwrap_or(source.len() - line_start);
    &source[line_start..line_start + width]
}

/// The first byte at or after `from` that is not whitespace or a line break:
/// the statement a problem at the end of a header line is really about.
fn next_code(source: &[u8], from: usize) -> usize {
    source[from.min(source.len())..]
        .iter()
        .position(|at| !at.is_ascii_whitespace())
        .map_or(from, |offset| from + offset)
}

/// What Python refuses about indentation and its grammar here reads without
/// complaint.
///
/// The grammar takes its blocks from an external scanner that gives an
/// indent only where one is allowed and otherwise says nothing, so a body
/// written at its `def` line's indentation parses as a function with an
/// empty body followed by loose statements, and a statement indented deeper
/// than the ones beside it parses as their sibling. Python itself raises
/// `IndentationError` for both. The checks read the tree:
///
/// - a block holds at least one statement;
/// - a block that starts on its own line is indented under the line of the
///   statement that owns it;
/// - the statements of one block that start a line carry one indentation,
///   and those of a module none;
/// - an `else`, `elif`, `except` or `finally` lines up with the statement it
///   belongs to, and a decorated definition with its first decorator.
pub(crate) fn python_layout_problems(
    root: &Node,
    source: &[u8],
    problems: &mut Vec<SyntaxProblem>,
) {
    let mut at = |start: usize, what: &'static str| {
        let end = source[start.min(source.len())..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(source.len(), |offset| start + offset);
        problems.push(SyntaxProblem {
            start_byte: start,
            end_byte: end,
            kind: SyntaxProblemKind::Layout(what),
        });
    };
    let mut stack = vec![*root];
    while let Some(node) = stack.pop() {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            // An empty block has no child and is the first thing looked for.
            if child.child_count() > 0 || child.kind() == "block" {
                stack.push(child);
            }
        }
        if node.is_error() {
            continue;
        }
        let kind = node.kind();
        if kind == "decorated_definition" {
            if let (Some(own), Some(definition)) = (
                indent_before(source, node.start_byte()),
                node.child_by_field_name("definition"),
            ) {
                if indent_before(source, definition.start_byte()).is_some_and(|held| held != own) {
                    at(
                        definition.start_byte(),
                        "a definition is not indented like its decorator",
                    );
                }
            }
            continue;
        }
        // A clause of a statement, never one of a comprehension, which sits
        // inside brackets where layout is free.
        if matches!(
            kind,
            "else_clause"
                | "elif_clause"
                | "except_clause"
                | "except_group_clause"
                | "finally_clause"
        ) {
            let owner = node
                .parent()
                .filter(|owner| !owner.is_error() && owner.kind().ends_with("_statement"));
            if let (Some(own), Some(owner)) = (indent_before(source, node.start_byte()), owner) {
                if indent_before(source, owner.start_byte()).is_some_and(|held| held != own) {
                    at(
                        node.start_byte(),
                        "unindent does not match any outer indentation level",
                    );
                }
            }
            continue;
        }
        if kind != "module" && kind != "block" {
            continue;
        }
        let mut cursor = node.walk();
        let statements: Vec<Node> = node
            .named_children(&mut cursor)
            .filter(|child| child.kind() != "comment" && !child.is_error())
            .collect();
        if statements.is_empty() {
            if kind == "block" && !node.has_error() {
                at(
                    next_code(source, node.start_byte()),
                    "expected an indented block",
                );
            }
            continue;
        }
        // The indentation the block's statements must share.
        let mut shared: Option<&[u8]> = (kind == "module").then_some(&[][..]);
        if kind == "block" {
            let owner = node
                .parent()
                .and_then(|owner| indent_before(source, owner.start_byte()));
            let first = indent_before(source, statements[0].start_byte());
            if let (Some(owner), Some(first)) = (owner, first) {
                if first.len() <= owner.len() || !first.starts_with(owner) {
                    at(statements[0].start_byte(), "expected an indented block");
                    continue;
                }
            }
        }
        for (index, statement) in statements.iter().enumerate() {
            let Some(own) = indent_before(source, statement.start_byte()) else {
                continue;
            };
            match shared {
                None => shared = Some(own),
                Some(held) if held == own => {}
                Some(held) => {
                    // Deeper than its block after a statement that ended
                    // deeper still: it came back out of that statement's
                    // block and landed on no level.
                    let left_a_deeper_block = index > 0
                        && line_indent(source, statements[index - 1].end_byte().saturating_sub(1))
                            .len()
                            > own.len();
                    at(
                        statement.start_byte(),
                        if own.len() > held.len() && !left_a_deeper_block {
                            "unexpected indent"
                        } else {
                            "unindent does not match any outer indentation level"
                        },
                    )
                }
            }
        }
    }
}
