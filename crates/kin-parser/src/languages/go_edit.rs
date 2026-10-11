// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! How Go source is edited at entity grain: the hooks the edit planner reads
//! instead of naming the language.

use std::ops::Range;

use tree_sitter::{Node, Tree};

use crate::declaration_lifecycle::{leading_comment_start, DeclarationLifecycle};
use crate::member_placement::{
    entity_start, indent_before, string_lines, MemberPlacement, Shape, Site,
};
use crate::LanguageAdapter;

/// Go's top-level function lifecycle.
pub(crate) struct GoLifecycle;

impl DeclarationLifecycle for GoLifecycle {
    fn top_level_function<'t>(&self, statement: Node<'t>) -> Option<Node<'t>> {
        (statement.kind() == "function_declaration").then_some(statement)
    }

    fn is_nested_declaration(&self, node: &Node) -> bool {
        matches!(
            node.kind(),
            "function_declaration" | "method_declaration" | "type_declaration"
        )
    }

    /// A Go doc comment is the comment lines directly above the declaration,
    /// with no blank line between: it goes with the function.
    fn declaration_start(&self, statement: Node, source: &[u8]) -> usize {
        leading_comment_start(statement, source, |_| true)
    }

    /// Every Go source unit opens with its package clause, and a unit beside
    /// another in its directory repeats it.
    fn unit_prelude(&self, tree: &Tree, source: &[u8]) -> Result<Option<String>, String> {
        let mut cursor = tree.root_node().walk();
        let packages = tree
            .root_node()
            .named_children(&mut cursor)
            .filter(|node| node.kind() == "package_clause")
            .map(|node| std::str::from_utf8(&source[node.byte_range()]).map(str::to_owned))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?;
        if packages.len() != 1 {
            return Err("Go lifecycle requires one exact package owner".into());
        }
        Ok(packages.into_iter().next())
    }

    /// A Go unit's filename and leading directives may restrict which builds
    /// it belongs to. This lifecycle does not synthesize build ownership, so
    /// it refuses such an owner rather than promote a test or platform
    /// function into an ordinary unit.
    fn check_new_unit_owner(&self, owner: &str, source: &[u8]) -> Result<(), String> {
        let filename = std::path::Path::new(owner)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .ok_or("anchor owner is not UTF8")?;
        if filename.contains('_') || filename.starts_with('.') {
            return Err(
                "Go new source units currently require a plain owner filename without \
                        underscores or a leading dot; test/platform build-owner placement is not \
                        yet supported; use sibling_after"
                    .into(),
            );
        }
        let tree = super::GoAdapter
            .parse(source)
            .map_err(|error| error.to_string())?;
        let mut cursor = tree.root_node().walk();
        let package = tree
            .root_node()
            .named_children(&mut cursor)
            .find(|node| node.kind() == "package_clause")
            .ok_or("missing Go package owner")?;
        let header = std::str::from_utf8(&source[..package.start_byte()])
            .map_err(|error| error.to_string())?;
        if header.lines().any(|line| {
            let line = line.trim_start();
            line.starts_with("//go:build") || line.starts_with("// +build")
        }) {
            return Err(
                "Go new source units with build directives require build-owner \
                        placement, which is not yet supported; use sibling_after"
                    .into(),
            );
        }
        Ok(())
    }

    fn new_unit_extension(&self, _owner: &str) -> String {
        "go".into()
    }

    /// One Go package is every unit of its directory, so a name taken in any
    /// of them is taken in all.
    fn package_scoped_names(&self) -> bool {
        true
    }
}

/// Go's member placement: a method is a top-level declaration with a
/// receiver, written beside the methods of its type; a type or a function is
/// written at the top of a unit.
pub(crate) struct GoPlacement;

fn go_parse(source: &[u8]) -> Result<tree_sitter::Tree, String> {
    super::GoAdapter
        .parse(source)
        .map_err(|error| error.to_string())
}

impl MemberPlacement for GoPlacement {
    fn locate(&self, source: &[u8], span: Range<usize>) -> Result<Site, String> {
        if span.start >= span.end || span.end > source.len() {
            return Err("the declaration's span is outside its source".into());
        }
        let tree = go_parse(source)?;
        let root = tree.root_node();
        if root.has_error() {
            return Err("the source unit does not parse as complete valid Go".into());
        }
        // The span may start at the comments above the declaration, which
        // are its siblings, so the declaration is the node that ends the span
        // and starts inside it.
        let mut node = root
            .descendant_for_byte_range(span.end - 1, span.end)
            .ok_or("the declaration's span names no syntax")?;
        while !(node.end_byte() == span.end
            && node.start_byte() >= span.start
            && matches!(
                node.kind(),
                "function_declaration" | "method_declaration" | "type_spec" | "type_declaration"
            ))
        {
            node = node
                .parent()
                .ok_or("the span is not a function, method or type")?;
        }
        // A type's entity spans its spec; the declaration around it is what
        // stands at the top of the unit.
        let statement = if node.kind() == "type_spec" {
            node.parent().ok_or("a type spec outside a declaration")?
        } else {
            node
        };
        if statement.parent().map(|parent| parent.kind()) != Some("source_file") {
            return Err("the declaration is not written at the top of its unit".into());
        }
        let shape = match node.kind() {
            "function_declaration" | "method_declaration" => Shape::Function,
            _ => Shape::Class,
        };
        let start = leading_comment_start(statement, source, |_| true);
        let extent = start.min(span.start)..statement.end_byte().max(span.end);
        let indent = indent_before(source, extent.start)
            .ok_or("the declaration does not start its own line")?;
        let body_indent = node.child_by_field_name("body").and_then(|body| {
            let mut cursor = body.walk();
            let first = body.named_children(&mut cursor).next()?;
            (first.start_position().row != node.start_position().row)
                .then(|| indent_before(source, first.start_byte()))
                .flatten()
        });
        Ok(Site {
            span,
            extent,
            shape,
            indent,
            top_level: true,
            // A method is a member of its receiver's type, written at the top.
            member: node.kind() == "method_declaration",
            body_indent,
            only_statement: false,
            members_end: None,
        })
    }

    fn check_one_declaration(
        &self,
        text: &str,
        name: &str,
        shape: Shape,
        member: bool,
    ) -> Result<usize, String> {
        let (open, close) = self.shell(member);
        let wrapped = format!("{open}{text}{close}");
        let tree = go_parse(wrapped.as_bytes())?;
        let root = tree.root_node();
        if root.has_error() {
            return Err("EntityCreate body does not parse as complete valid Go".into());
        }
        let refusal = || {
            format!(
                "EntityCreate body must be exactly one {} named {name}, led by its doc comment at \
                 most: no package clause, imports or other declarations",
                shape.label()
            )
        };
        let mut cursor = root.walk();
        let items: Vec<Node> = root
            .named_children(&mut cursor)
            .filter(|child| child.kind() != "comment")
            .collect();
        let [item] = items.as_slice() else {
            return Err(refusal());
        };
        let (found, named) = match (item.kind(), member) {
            ("method_declaration", true) | ("function_declaration", false) => {
                (Shape::Function, Some(*item))
            }
            ("type_declaration", false) => {
                let mut cursor = item.walk();
                let specs: Vec<Node> = item
                    .named_children(&mut cursor)
                    .filter(|spec| spec.kind() == "type_spec")
                    .collect();
                (Shape::Class, (specs.len() == 1).then(|| specs[0]))
            }
            _ => return Err(refusal()),
        };
        let written = named
            .and_then(|named| named.child_by_field_name("name"))
            .and_then(|node| node.utf8_text(wrapped.as_bytes()).ok())
            .ok_or_else(refusal)?;
        if found == shape && written == name {
            // The entity starts past the doc comment above it.
            entity_start(kin_model::LanguageId::Go, (open, close), text, name).ok_or_else(|| {
                format!(
                    "EntityCreate body declares the {} {name} in a form Kin does not record as one",
                    shape.label()
                )
            })
        } else {
            Err(format!(
                "EntityCreate body declares the {} {written}, and the operation names the {} \
                 {name}",
                found.label(),
                shape.label()
            ))
        }
    }

    /// The lines that begin inside a raw string literal.
    fn verbatim_lines(&self, text: &str, member: bool) -> Result<Vec<bool>, String> {
        string_lines(kin_model::LanguageId::Go, self.shell(member), text)
    }

    /// A Go declaration stands on its own lines: the grammar ends a
    /// top-level declaration at a line break, so one is read after it.
    fn shell(&self, _member: bool) -> (&'static str, &'static str) {
        ("", "\n")
    }

    fn parses(&self, source: &[u8]) -> bool {
        go_parse(source).is_ok_and(|tree| !tree.root_node().has_error())
    }

    fn members_elsewhere(&self, _site: &Site) -> String {
        "a Go type declares its fields in its body, and its methods beside it: create a method \
         beside one of its methods with that method's source_base and placement sibling_after, or \
         address the unit with repository_base and unit"
            .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each checked as it would stand in a unit, a type at the end of the
    /// text included, which the grammar reads only before a line break.
    #[test]
    fn a_method_a_function_and_a_type_are_each_one_declaration() {
        let placement = GoPlacement;
        for (text, name, shape, member) in [
            (
                "// Added returns three.\nfunc (s Store) Added() int { return 3 }",
                "Added",
                Shape::Function,
                true,
            ),
            (
                "func helper() int { return 1 }",
                "helper",
                Shape::Function,
                false,
            ),
            (
                "type Other struct {\n\tvalue int\n}",
                "Other",
                Shape::Class,
                false,
            ),
        ] {
            placement
                .check_one_declaration(text, name, shape, member)
                .unwrap_or_else(|error| panic!("{text}: {error}"));
        }
    }
}
