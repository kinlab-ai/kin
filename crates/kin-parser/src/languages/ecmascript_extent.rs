// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Where a TypeScript or JavaScript declaration starts.
//!
//! The grammar puts part of a declaration on a parent node: `export` on the
//! `export_statement` around a function or class, and `const` (or `let`,
//! `var`) on the `lexical_declaration` around the one declarator it binds. An
//! entity spanned from the inner node is served without them, an update that
//! sends them back doubles them, and removing the entity leaves a bare
//! `export` or `const` that no longer parses. So once both adapters have
//! extracted a file, every entity whose node stands alone in such a statement
//! is widened to the statement. The entity's id keys on its declaration's own
//! line and does not move.

use tree_sitter::Node;

use crate::adapter::span_from_node;
use crate::declaration_extent::declaration_line;
use crate::extract::ExtractedEntity;
use kin_model::{EntityKind, FilePathId};

/// The statement a declaration node stands alone in: the `const`, `let` or
/// `var` statement of a single declarator, and then the `export` statement
/// that exports exactly it. `node` itself when neither holds.
pub(crate) fn whole_statement<'t>(node: Node<'t>) -> Node<'t> {
    let mut whole = node;
    if whole.kind() == "variable_declarator" {
        if let Some(statement) = whole.parent().filter(|parent| {
            matches!(
                parent.kind(),
                "lexical_declaration" | "variable_declaration"
            )
        }) {
            let mut cursor = statement.walk();
            let declarators = statement
                .named_children(&mut cursor)
                .filter(|child| child.kind() == "variable_declarator")
                .count();
            if declarators == 1 {
                whole = statement;
            }
        }
    }
    if let Some(export) = whole
        .parent()
        .filter(|parent| parent.kind() == "export_statement")
    {
        let mut cursor = export.walk();
        let only = export
            .named_children(&mut cursor)
            .filter(|child| !matches!(child.kind(), "decorator" | "comment"))
            .all(|child| child.id() == whole.id());
        if only {
            whole = export;
        }
    }
    whole
}

/// The declaration a statement holds alone: the inverse of
/// [`whole_statement`]. An `export` statement of one declaration gives that
/// declaration, and a `const`, `let` or `var` statement of one declarator
/// gives the declarator. Any other node is itself.
pub(crate) fn declaration_in<'t>(node: Node<'t>) -> Node<'t> {
    let mut inner = node;
    if inner.kind() == "export_statement" {
        let mut cursor = inner.walk();
        let held: Vec<Node<'t>> = inner
            .named_children(&mut cursor)
            .filter(|child| !matches!(child.kind(), "decorator" | "comment"))
            .collect();
        if let [only] = held.as_slice() {
            inner = *only;
        }
    }
    if matches!(inner.kind(), "lexical_declaration" | "variable_declaration") {
        let mut cursor = inner.walk();
        let declarators: Vec<Node<'t>> = inner
            .named_children(&mut cursor)
            .filter(|child| child.kind() == "variable_declarator")
            .collect();
        if let [only] = declarators.as_slice() {
            inner = *only;
        }
    }
    if whole_statement(inner).id() == node.id() {
        inner
    } else {
        node
    }
}

/// The outermost node whose bytes are exactly `start..end`.
fn node_at<'t>(root: &Node<'t>, start: usize, end: usize) -> Option<Node<'t>> {
    let mut node = root.descendant_for_byte_range(start, end)?;
    if node.start_byte() != start || node.end_byte() != end {
        return None;
    }
    while let Some(parent) = node.parent() {
        if parent.start_byte() != start || parent.end_byte() != end {
            break;
        }
        node = parent;
    }
    Some(node)
}

/// Widen every entity of `root`'s file spanned from a declaration that
/// stands alone in a `const`/`let`/`var` statement or an `export` statement
/// to that whole statement.
pub(crate) fn widen_to_statements(
    root: &Node,
    file_id: &FilePathId,
    entities: &mut [ExtractedEntity],
) {
    for entity in entities.iter_mut() {
        if matches!(entity.kind, EntityKind::Module | EntityKind::Package) {
            continue;
        }
        let Some(node) = node_at(root, entity.span.start_byte, entity.span.end_byte) else {
            continue;
        };
        let whole = whole_statement(node);
        if whole.id() == node.id() {
            continue;
        }
        let span = span_from_node(&whole, file_id);
        if entity.declaration_line.is_none() {
            entity.declaration_line = declaration_line(&node, &span);
        }
        entity.span = span;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::LanguageAdapter;
    use crate::languages::{JavaScriptAdapter, TypeScriptAdapter};

    fn spans(adapter: &dyn LanguageAdapter, path: &str, source: &str) -> Vec<(String, String)> {
        let tree = adapter.parse(source.as_bytes()).unwrap();
        let output = adapter
            .extract(&tree, source.as_bytes(), &FilePathId::new(path))
            .unwrap();
        output
            .entities
            .iter()
            .filter(|entity| entity.kind != EntityKind::Module)
            .map(|entity| {
                (
                    entity.name.clone(),
                    source[entity.span.start_byte..entity.span.end_byte].to_string(),
                )
            })
            .collect()
    }

    fn text<'a>(spans: &'a [(String, String)], name: &str) -> &'a str {
        spans
            .iter()
            .find(|(held, _)| held == name)
            .map(|(_, text)| text.as_str())
            .unwrap_or_else(|| panic!("no entity {name} in {spans:?}"))
    }

    #[test]
    fn a_declaration_holds_its_export_and_its_binding_keyword() {
        let source = "export function total(values) {\n  return 1;\n}\n\nexport const average = (values) => total(values);\n\nconst local = () => 2;\n\nexport class Account {\n  deposit() {}\n}\n\nconst a = () => 1, b = () => 2;\n";
        for (adapter, path) in [
            (&TypeScriptAdapter as &dyn LanguageAdapter, "a.ts"),
            (&JavaScriptAdapter as &dyn LanguageAdapter, "a.js"),
        ] {
            let spans = spans(adapter, path, source);
            assert_eq!(
                text(&spans, "total"),
                "export function total(values) {\n  return 1;\n}"
            );
            assert_eq!(
                text(&spans, "average"),
                "export const average = (values) => total(values);"
            );
            assert_eq!(text(&spans, "local"), "const local = () => 2;");
            assert!(text(&spans, "Account").starts_with("export class Account {"));
            // A method is not a statement of its own.
            assert_eq!(text(&spans, "Account.deposit"), "deposit() {}");
            // A declarator that shares its statement keeps its own span.
            assert_eq!(text(&spans, "a"), "a = () => 1");
        }
    }
}
