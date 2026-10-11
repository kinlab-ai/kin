// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! How Rust source is edited at entity grain: the hooks the edit planner
//! reads instead of naming the language.

use std::ops::Range;

use tree_sitter::Node;

use crate::declaration_lifecycle::DeclarationLifecycle;
use crate::member_placement::{
    entity_start, indent_before, string_lines, MemberPlacement, Shape, Site,
};
use crate::LanguageAdapter;

/// Rust's top-level function lifecycle. An item's doc comments and outer
/// attributes are inside its entity span already.
pub(crate) struct RustLifecycle;

impl DeclarationLifecycle for RustLifecycle {
    fn top_level_function<'t>(&self, statement: Node<'t>) -> Option<Node<'t>> {
        (statement.kind() == "function_item").then_some(statement)
    }

    fn is_nested_declaration(&self, node: &Node) -> bool {
        matches!(
            node.kind(),
            "function_item"
                | "struct_item"
                | "enum_item"
                | "trait_item"
                | "impl_item"
                | "mod_item"
                | "type_item"
                | "const_item"
                | "static_item"
                | "macro_definition"
                | "use_declaration"
                | "extern_crate_declaration"
        )
    }

    /// A new Rust source unit is part of the crate only through a `mod`
    /// declaration in its parent module, which this lifecycle does not write.
    fn check_new_unit_owner(&self, _owner: &str, _source: &[u8]) -> Result<(), String> {
        Err(
            "Rust new source units require a module-owner declaration; use sibling_after until \
             module-owner creation is supported"
                .into(),
        )
    }

    fn new_unit_extension(&self, _owner: &str) -> String {
        "rs".into()
    }
}

/// Rust's member placement: a method in an `impl` block, a type or a
/// function at the top of a unit. An item's doc comments and outer
/// attributes are inside its entity span.
pub(crate) struct RustPlacement;

const IMPL_SHELL: (&str, &str) = ("impl KinShell {\n", "\n}\n");

fn rust_type(kind: &str) -> bool {
    matches!(kind, "struct_item" | "enum_item" | "union_item")
}

fn rust_parse(source: &[u8]) -> Result<tree_sitter::Tree, String> {
    super::RustAdapter
        .parse(source)
        .map_err(|error| error.to_string())
}

fn first_line_indent(block: Node, owner: Node, source: &[u8]) -> Option<String> {
    let mut cursor = block.walk();
    let first = block
        .named_children(&mut cursor)
        .find(|child| !child.kind().contains("comment") && child.kind() != "attribute_item")?;
    (first.start_position().row != owner.start_position().row)
        .then(|| indent_before(source, first.start_byte()))
        .flatten()
}

impl MemberPlacement for RustPlacement {
    fn locate(&self, source: &[u8], span: Range<usize>) -> Result<Site, String> {
        if span.start >= span.end || span.end > source.len() {
            return Err("the declaration's span is outside its source".into());
        }
        let tree = rust_parse(source)?;
        let root = tree.root_node();
        if root.has_error() {
            return Err("the source unit does not parse as complete valid Rust".into());
        }
        // The span starts at the item's first doc comment or attribute, which
        // are siblings of the item, so the item is the node that ends the
        // span and starts inside it.
        let mut node = root
            .descendant_for_byte_range(span.end - 1, span.end)
            .ok_or("the declaration's span names no syntax")?;
        while !(node.end_byte() == span.end
            && node.start_byte() >= span.start
            && (node.kind() == "function_item" || rust_type(node.kind())))
        {
            node = node
                .parent()
                .ok_or("the declaration's span is not one whole item")?;
        }
        let shape = if node.kind() == "function_item" {
            Shape::Function
        } else {
            Shape::Class
        };
        let parent = node.parent().ok_or("the item has no enclosing scope")?;
        let top_level = parent.kind() == "source_file";
        let member = parent.kind() == "declaration_list"
            && parent
                .parent()
                .is_some_and(|owner| owner.kind() == "impl_item");
        let indent =
            indent_before(source, span.start).ok_or("the item does not start its own line")?;
        let body_indent = node
            .child_by_field_name("body")
            .and_then(|body| first_line_indent(body, node, source));
        Ok(Site {
            extent: span.clone(),
            span,
            shape,
            indent,
            top_level,
            member,
            body_indent,
            only_statement: false,
            // A Rust type declares its fields in its body; its methods are
            // written in impl blocks.
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
        let tree = rust_parse(wrapped.as_bytes())?;
        let root = tree.root_node();
        if root.has_error() {
            return Err("EntityCreate body does not parse as complete valid Rust".into());
        }
        let scope = if member {
            let mut cursor = root.walk();
            let block = root
                .named_children(&mut cursor)
                .find(|child| child.kind() == "impl_item");
            block
                .and_then(|block| block.child_by_field_name("body"))
                .ok_or("EntityCreate body does not read as a method")?
        } else {
            root
        };
        let refusal = || {
            format!(
                "EntityCreate body must be exactly one {} named {name}, led by its doc comments \
                 and attributes at most: no use lines or other items",
                shape.label()
            )
        };
        // Doc comments and attributes lead the item and belong to it.
        let mut cursor = scope.walk();
        let items: Vec<Node> = scope
            .named_children(&mut cursor)
            .filter(|child| {
                !matches!(
                    child.kind(),
                    "attribute_item" | "line_comment" | "block_comment"
                )
            })
            .collect();
        let [item] = items.as_slice() else {
            return Err(refusal());
        };
        let found = match item.kind() {
            "function_item" => Shape::Function,
            kind if rust_type(kind) && !member => Shape::Class,
            _ => return Err(refusal()),
        };
        let written = item
            .child_by_field_name("name")
            .and_then(|node| node.utf8_text(wrapped.as_bytes()).ok())
            .unwrap_or_default();
        if found == shape && written == name {
            // The entity spans its doc comments and attributes too.
            entity_start(kin_model::LanguageId::Rust, (open, close), text, name).ok_or_else(|| {
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

    /// The lines that begin inside a string literal, raw strings included.
    fn verbatim_lines(&self, text: &str, member: bool) -> Result<Vec<bool>, String> {
        string_lines(kin_model::LanguageId::Rust, self.shell(member), text)
    }

    fn shell(&self, member: bool) -> (&'static str, &'static str) {
        if member {
            IMPL_SHELL
        } else {
            ("", "")
        }
    }

    fn parses(&self, source: &[u8]) -> bool {
        rust_parse(source).is_ok_and(|tree| !tree.root_node().has_error())
    }

    fn members_elsewhere(&self, _site: &Site) -> String {
        "a Rust type declares its fields in its body, and its methods in impl blocks: create a \
         method beside a method of its impl block, with that method's source_base and placement \
         sibling_after"
            .into()
    }
}

const RUST_KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
    "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub",
    "ref", "return", "self", "Self", "static", "struct", "super", "trait", "true", "type",
    "unsafe", "use", "where", "while",
];

fn rust_identifier(name: &str) -> bool {
    crate::unit_append::is_ascii_identifier(name, RUST_KEYWORDS)
}

/// Rust's unit editor: a crate root (`lib` or `main`) or an integration
/// test, each a whole crate on its own, its `use` lines first, its items a
/// blank line apart. A module below the crate root belongs to the crate
/// only through a `mod` line in its parent, which a unit edit does not
/// write, so it is not addressed as a unit.
pub(crate) const RUST_UNITS: crate::unit_append::AppendingEditor =
    crate::unit_append::AppendingEditor {
        syntax: crate::unit_append::AppendSyntax {
            language: kin_model::LanguageId::Rust,
            separation: 1,
            import_kinds: &["use_declaration", "extern_crate_declaration"],
            render_import: |import| match &import.alias {
                Some(alias) => format!("use {} as {alias};", import.path),
                None => format!("use {};", import.path),
            },
            imported_path: |node, source| {
                let argument = node.child_by_field_name("argument")?;
                let path = match argument.kind() {
                    "use_as_clause" => argument.child_by_field_name("path")?,
                    _ => argument,
                };
                path.utf8_text(source).ok().map(str::to_string)
            },
            leads_imports: |node, _| node.kind() == "inner_attribute_item",
        },
        identity: crate::unit_append::UnitIdentity {
            validate: |package, name, role| {
                if package != "." {
                    return Err(format!(
                        "Rust unit package {package:?} must be \".\", the crate's own root; a \
                         module below it is declared with mod in its parent"
                    ));
                }
                match role {
                    crate::unit_editor::UnitRole::Source if !matches!(name, "lib" | "main") => {
                        Err(format!(
                            "Rust unit name {name:?} must be lib or main, the crate root; a module \
                             below it belongs to the crate through mod in its parent, so create it \
                             there"
                        ))
                    }
                    crate::unit_editor::UnitRole::Test if !rust_identifier(name) => Err(format!(
                        "Rust test unit name {name:?} must be one Rust identifier"
                    )),
                    _ => Ok(()),
                }
            },
            path: |_, name, role| match role {
                crate::unit_editor::UnitRole::Source => format!("src/{name}.rs"),
                crate::unit_editor::UnitRole::Test => format!("tests/{name}.rs"),
            },
            describe: |_, name, role| match role {
                crate::unit_editor::UnitRole::Source => format!("Rust crate root {name}"),
                crate::unit_editor::UnitRole::Test => format!("Rust integration test {name}"),
            },
            validate_import: |import| {
                let segments_ok = import.path.split("::").all(|segment| {
                    matches!(segment, "crate" | "self" | "super") || rust_identifier(segment)
                });
                if import.path.is_empty() || !segments_ok {
                    return Err(format!(
                        "use path {:?} must be :: separated identifiers, such as \
                         \"std::collections::BTreeMap\"",
                        import.path
                    ));
                }
                match import.alias.as_deref() {
                    Some(alias) if !rust_identifier(alias) && alias != "_" => {
                        Err(format!("use alias {alias:?} must be one Rust identifier"))
                    }
                    _ => Ok(()),
                }
            },
            is_identifier: rust_identifier,
            is_test_unit: |path| path.starts_with("tests/") || path.contains("/tests/"),
        },
    };
