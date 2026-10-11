// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! How TypeScript and JavaScript source is edited at entity grain: the hooks
//! the edit planner reads instead of naming the language. The two share one
//! grammar family and one set of rules, and differ in their unit extensions.

use std::ops::Range;

use tree_sitter::Node;

use crate::declaration_lifecycle::{leading_comment_start, owner_extension, DeclarationLifecycle};
use crate::member_placement::{
    entity_start, indent_before, string_lines, MemberPlacement, Shape, Site,
};
use crate::LanguageAdapter;

/// The top-level function lifecycle TypeScript and JavaScript share.
pub(crate) struct EcmascriptLifecycle {
    /// The extensions a unit of the language is written under, the one a new
    /// unit takes when its owner's says nothing first.
    extensions: &'static [&'static str],
}

pub(crate) const TYPESCRIPT_LIFECYCLE: EcmascriptLifecycle = EcmascriptLifecycle {
    extensions: &["ts", "tsx"],
};

pub(crate) const JAVASCRIPT_LIFECYCLE: EcmascriptLifecycle = EcmascriptLifecycle {
    extensions: &["js", "jsx", "mjs", "cjs"],
};

fn is_function_value(node: &Node) -> bool {
    matches!(
        node.kind(),
        "arrow_function" | "function_expression" | "function" | "generator_function"
    )
}

/// The one declarator of a `const`, `let` or `var` statement that binds a
/// function, which the adapter records as a function entity spanning the
/// declarator.
fn function_declarator<'t>(statement: Node<'t>) -> Option<Node<'t>> {
    if !matches!(
        statement.kind(),
        "lexical_declaration" | "variable_declaration"
    ) {
        return None;
    }
    let mut cursor = statement.walk();
    let declarators: Vec<Node<'t>> = statement
        .named_children(&mut cursor)
        .filter(|child| child.kind() == "variable_declarator")
        .collect();
    let [declarator] = declarators.as_slice() else {
        return None;
    };
    declarator
        .child_by_field_name("value")
        .filter(is_function_value)
        .map(|_| *declarator)
}

fn function_declaration<'t>(statement: Node<'t>) -> Option<Node<'t>> {
    match statement.kind() {
        "function_declaration" | "generator_function_declaration" => Some(statement),
        _ => function_declarator(statement),
    }
}

impl DeclarationLifecycle for EcmascriptLifecycle {
    /// A function declaration, or a `const` bound to one function, either
    /// standing alone or exported. The adapter's span starts after `export`,
    /// so the statement holds what the span does not.
    fn top_level_function<'t>(&self, statement: Node<'t>) -> Option<Node<'t>> {
        match statement.kind() {
            "export_statement" => statement
                .child_by_field_name("declaration")
                .and_then(function_declaration),
            _ => function_declaration(statement),
        }
    }

    fn is_nested_declaration(&self, node: &Node) -> bool {
        match node.kind() {
            "function_declaration"
            | "generator_function_declaration"
            | "class_declaration"
            | "abstract_class_declaration"
            | "interface_declaration"
            | "type_alias_declaration"
            | "enum_declaration" => true,
            // A local bound to a function or a class declares one.
            "variable_declarator" => node
                .child_by_field_name("value")
                .is_some_and(|value| is_function_value(&value) || value.kind() == "class"),
            _ => false,
        }
    }

    /// A JSDoc comment (`/** ... */`) directly above the declaration is its
    /// documentation and goes with it.
    fn declaration_start(&self, statement: Node, source: &[u8]) -> usize {
        leading_comment_start(statement, source, |text| text.starts_with("/**"))
    }

    /// The owner's own extension, so a new unit beside a `.tsx` or `.mjs`
    /// unit is written the way its neighbour is.
    fn new_unit_extension(&self, owner: &str) -> String {
        owner_extension(owner)
            .filter(|extension| self.extensions.contains(extension))
            .unwrap_or(self.extensions[0])
            .to_string()
    }
}

/// The member placement TypeScript and JavaScript share: a method in a
/// class body, a class or a function at the top of a unit.
pub(crate) struct EcmascriptPlacement {
    parse: fn(&[u8]) -> crate::error::Result<tree_sitter::Tree>,
    language: kin_model::LanguageId,
}

pub(crate) const TYPESCRIPT_PLACEMENT: EcmascriptPlacement = EcmascriptPlacement {
    parse: |source| super::TypeScriptAdapter.parse(source),
    language: kin_model::LanguageId::TypeScript,
};

pub(crate) const JAVASCRIPT_PLACEMENT: EcmascriptPlacement = EcmascriptPlacement {
    parse: |source| super::JavaScriptAdapter.parse(source),
    language: kin_model::LanguageId::JavaScript,
};

const CLASS_SHELL: (&str, &str) = ("class KinShell {\n", "\n}\n");

fn is_class(node: &Node) -> bool {
    matches!(
        node.kind(),
        "class_declaration" | "abstract_class_declaration"
    )
}

/// The first byte of what goes with a class member: the decorators written
/// before it, and the JSDoc block above them.
fn member_start(member: Node, source: &[u8]) -> usize {
    let mut first = member;
    while let Some(previous) = first.prev_named_sibling() {
        if previous.kind() != "decorator" {
            break;
        }
        first = previous;
    }
    leading_comment_start(first, source, |text| text.starts_with("/**"))
}

fn line_end(source: &[u8], at: usize) -> usize {
    source[at..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(source.len(), |offset| at + offset)
}

/// The indentation of the first named node in `block` that starts a line of
/// its own after `owner`'s first line.
fn first_inner_indent(block: Node, owner: Node, source: &[u8]) -> Option<String> {
    let mut cursor = block.walk();
    let first = block
        .named_children(&mut cursor)
        .find(|child| !child.kind().contains("comment"))?;
    (first.start_position().row != owner.start_position().row)
        .then(|| indent_before(source, first.start_byte()))
        .flatten()
}

impl MemberPlacement for EcmascriptPlacement {
    fn locate(&self, source: &[u8], span: Range<usize>) -> Result<Site, String> {
        let display = self.language.profile().display_name;
        if span.start >= span.end || span.end > source.len() {
            return Err("the declaration's span is outside its source".into());
        }
        let tree = (self.parse)(source).map_err(|error| error.to_string())?;
        let root = tree.root_node();
        if root.has_error() {
            return Err(format!(
                "the source unit does not parse as complete valid {display}"
            ));
        }
        let mut node = root
            .descendant_for_byte_range(span.start, span.end)
            .ok_or("the declaration's span names no syntax")?;
        while node.byte_range() == span
            && !matches!(
                node.kind(),
                "method_definition"
                    | "function_declaration"
                    | "class_declaration"
                    | "abstract_class_declaration"
                    | "variable_declarator"
            )
        {
            match node.parent().filter(|parent| parent.byte_range() == span) {
                Some(parent) => node = parent,
                None => break,
            }
        }
        // The adapter spans a declaration that stands alone in an `export`
        // or `const` statement as that whole statement.
        let node = super::ecmascript_extent::declaration_in(node);
        if node.byte_range() != span
            && super::ecmascript_extent::whole_statement(node).byte_range() != span
        {
            return Err("the declaration's span is not one whole definition".into());
        }
        let (shape, statement, body) = match node.kind() {
            "method_definition" => (Shape::Function, node, node.child_by_field_name("body")),
            "function_declaration" => (
                Shape::Function,
                node.parent()
                    .filter(|parent| parent.kind() == "export_statement")
                    .unwrap_or(node),
                node.child_by_field_name("body"),
            ),
            "variable_declarator" => {
                let declaration = node.parent().ok_or("a declarator outside a declaration")?;
                let statement = declaration
                    .parent()
                    .filter(|parent| parent.kind() == "export_statement")
                    .unwrap_or(declaration);
                let body = node
                    .child_by_field_name("value")
                    .and_then(|value| value.child_by_field_name("body"));
                (Shape::Function, statement, body)
            }
            _ if is_class(&node) => (
                Shape::Class,
                node.parent()
                    .filter(|parent| parent.kind() == "export_statement")
                    .unwrap_or(node),
                node.child_by_field_name("body"),
            ),
            _ => return Err("the span is not a function, method or class".into()),
        };
        let parent = statement
            .parent()
            .ok_or("the definition has no enclosing scope")?;
        let top_level = parent.kind() == "program";
        let member = node.kind() == "method_definition" && parent.kind() == "class_body";
        let start = if member {
            member_start(node, source)
        } else {
            leading_comment_start(statement, source, |text| text.starts_with("/**"))
        };
        let extent = start.min(span.start)..statement.end_byte().max(span.end);
        let indent = indent_before(source, extent.start)
            .ok_or("the definition does not start its own line")?;
        let body_indent = body.and_then(|body| first_inner_indent(body, node, source));
        // A new last member goes after the last member, past whatever
        // comment shares its line.
        let members_end = (shape == Shape::Class).then(|| {
            let body = body.expect("a class has a body");
            let mut cursor = body.walk();
            let last = body
                .named_children(&mut cursor)
                .filter(|child| !child.kind().contains("comment"))
                .last();
            match last {
                Some(last) => {
                    let end = line_end(source, last.end_byte());
                    let rest = String::from_utf8_lossy(&source[last.end_byte()..end]);
                    let rest = rest.trim();
                    if rest.is_empty() || rest.starts_with("//") {
                        end
                    } else {
                        last.end_byte()
                    }
                }
                None => body.start_byte() + 1,
            }
        });
        Ok(Site {
            span,
            extent,
            shape,
            indent,
            top_level,
            member,
            body_indent,
            only_statement: false,
            members_end,
        })
    }

    fn check_one_declaration(
        &self,
        text: &str,
        name: &str,
        shape: Shape,
        member: bool,
    ) -> Result<usize, String> {
        let display = self.language.profile().display_name;
        let (open, close) = self.shell(member);
        let wrapped = format!("{open}{text}{close}");
        let tree = (self.parse)(wrapped.as_bytes()).map_err(|error| error.to_string())?;
        let root = tree.root_node();
        if root.has_error() {
            return Err(format!(
                "EntityCreate body does not parse as complete valid {display}"
            ));
        }
        let scope = if member {
            let mut cursor = root.walk();
            let class = root.named_children(&mut cursor).find(is_class);
            class
                .and_then(|class| class.child_by_field_name("body"))
                .ok_or("EntityCreate body does not read as a class member")?
        } else {
            root
        };
        // A JSDoc block may lead the declaration; nothing else may stand
        // beside it.
        let mut cursor = scope.walk();
        let children: Vec<Node> = scope.named_children(&mut cursor).collect();
        let mut rest = children.iter().skip_while(|child| {
            child.kind() == "comment"
                && child
                    .utf8_text(wrapped.as_bytes())
                    .is_ok_and(|text| text.starts_with("/**"))
        });
        let declared = rest.next();
        let refusal = || {
            format!(
                "EntityCreate body must be exactly one {} named {name}, led by its doc comment \
                 at most: no imports, other comments or other declarations",
                shape.label()
            )
        };
        let (Some(declared), None) = (declared, rest.next()) else {
            return Err(refusal());
        };
        let declaration = match declared.kind() {
            "export_statement" => declared.child_by_field_name("declaration"),
            "decorator" => return Err(refusal()),
            _ => Some(*declared),
        };
        let found = declaration.and_then(|declaration| {
            let (found, named) = match declaration.kind() {
                "method_definition" if member => (Shape::Function, declaration),
                "function_declaration" if !member => (Shape::Function, declaration),
                "lexical_declaration" | "variable_declaration" if !member => {
                    (Shape::Function, function_declarator(declaration)?)
                }
                _ if is_class(&declaration) && !member => (Shape::Class, declaration),
                _ => return None,
            };
            let written = named
                .child_by_field_name("name")?
                .utf8_text(wrapped.as_bytes())
                .ok()?;
            Some((found, written.to_string()))
        });
        match found {
            // The entity spans the declaration itself, past an export and the
            // doc comment above it, as the adapter reads it.
            Some((found, written)) if found == shape && written == name => {
                entity_start(self.language, (open, close), text, name).ok_or_else(|| {
                    format!(
                        "EntityCreate body declares the {} {name} in a form Kin does not record                          as one",
                        shape.label()
                    )
                })
            }
            Some((found, written)) => Err(format!(
                "EntityCreate body declares the {} {written}, and the operation names the {} \
                 {name}",
                found.label(),
                shape.label()
            )),
            None => Err(refusal()),
        }
    }

    /// The lines that begin inside a template literal: its text.
    fn verbatim_lines(&self, text: &str, member: bool) -> Result<Vec<bool>, String> {
        string_lines(self.language, self.shell(member), text)
    }

    fn shell(&self, member: bool) -> (&'static str, &'static str) {
        if member {
            CLASS_SHELL
        } else {
            ("", "")
        }
    }

    fn parses(&self, source: &[u8]) -> bool {
        (self.parse)(source).is_ok_and(|tree| !tree.root_node().has_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::declaration_lifecycle::{first_nested_declaration, prove_top_level_function};
    use crate::LanguageAdapter;
    use kin_model::{EntityKind, FilePathId};

    /// The function entity named `name` the adapter extracts from `source`,
    /// proven as a top-level function: its span and its extent as text.
    fn proven(
        adapter: &dyn LanguageAdapter,
        lifecycle: &EcmascriptLifecycle,
        source: &str,
        name: &str,
    ) -> Option<(String, String, bool)> {
        let tree = adapter.parse(source.as_bytes()).unwrap();
        let parsed = adapter
            .extract(&tree, source.as_bytes(), &FilePathId::new("src/unit.ts"))
            .unwrap();
        let entity = parsed
            .entities
            .iter()
            .find(|entity| entity.kind == EntityKind::Function && entity.name == name)?;
        let span = entity.span.start_byte..entity.span.end_byte;
        let proven = prove_top_level_function(lifecycle, &tree, source.as_bytes(), &span, name)?;
        Some((
            source[proven.span.clone()].to_string(),
            source[proven.extent.clone()].to_string(),
            first_nested_declaration(lifecycle, proven.declaration).is_some(),
        ))
    }

    #[test]
    fn exported_declared_and_bound_functions_are_top_level_with_their_prefix() {
        for (adapter, lifecycle) in [
            (
                &super::super::TypeScriptAdapter as &dyn LanguageAdapter,
                &TYPESCRIPT_LIFECYCLE,
            ),
            (&super::super::JavaScriptAdapter, &JAVASCRIPT_LIFECYCLE),
        ] {
            for (source, name, extent) in [
                (
                    "function plain() {\n  return 1;\n}\n",
                    "plain",
                    "function plain() {\n  return 1;\n}",
                ),
                (
                    "export function shown() {\n  return 1;\n}\n",
                    "shown",
                    "export function shown() {\n  return 1;\n}",
                ),
                (
                    "export default function main() {\n  return 1;\n}\n",
                    "main",
                    "export default function main() {\n  return 1;\n}",
                ),
                (
                    "async function later() {\n  return 1;\n}\n",
                    "later",
                    "async function later() {\n  return 1;\n}",
                ),
                (
                    "export const bound = () => 1;\n",
                    "bound",
                    "export const bound = () => 1;",
                ),
                ("const local = function () {\n  return 1;\n};\n", "local", "const local = function () {\n  return 1;\n};"),
                (
                    "// note\n\n/**\n * Says one.\n */\nexport function documented() {\n  return 1;\n}\n",
                    "documented",
                    "/**\n * Says one.\n */\nexport function documented() {\n  return 1;\n}",
                ),
            ] {
                let (_, held, nested) = proven(adapter, lifecycle, source, name)
                    .unwrap_or_else(|| panic!("{source}"));
                assert_eq!(held, extent, "{source}");
                assert!(!nested, "{source}");
            }
        }
    }

    #[test]
    fn a_function_owning_a_declaration_and_a_member_are_not_leaf_top_level_functions() {
        let ts = &super::super::TypeScriptAdapter;
        let (_, _, nested) = proven(
            ts,
            &TYPESCRIPT_LIFECYCLE,
            "function outer() {\n  function inner() {}\n  return inner();\n}\n",
            "outer",
        )
        .unwrap();
        assert!(nested);
        let (_, _, nested) = proven(
            ts,
            &TYPESCRIPT_LIFECYCLE,
            "function outer() {\n  const inner = () => 1;\n  return [1].map((x) => x + inner());\n}\n",
            "outer",
        )
        .unwrap();
        assert!(nested);
        // A callback is no declaration of the function's own.
        let (_, _, nested) = proven(
            ts,
            &TYPESCRIPT_LIFECYCLE,
            "function outer() {\n  return [1].map((x) => x + 1);\n}\n",
            "outer",
        )
        .unwrap();
        assert!(!nested);
        // Two declarators bind no single function.
        assert!(proven(
            ts,
            &TYPESCRIPT_LIFECYCLE,
            "const a = () => 1, b = () => 2;\n",
            "a"
        )
        .is_none());
    }

    #[test]
    fn a_new_unit_takes_its_owners_extension() {
        assert_eq!(
            TYPESCRIPT_LIFECYCLE.new_unit_extension("src/Button.tsx"),
            "tsx"
        );
        assert_eq!(TYPESCRIPT_LIFECYCLE.new_unit_extension("src/run.ts"), "ts");
        assert_eq!(
            JAVASCRIPT_LIFECYCLE.new_unit_extension("lib/run.mjs"),
            "mjs"
        );
        assert_eq!(JAVASCRIPT_LIFECYCLE.new_unit_extension("lib/run"), "js");
    }
}

const ECMASCRIPT_RESERVED: &[&str] = &[
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "debugger",
    "default",
    "delete",
    "do",
    "else",
    "export",
    "extends",
    "finally",
    "for",
    "function",
    "if",
    "import",
    "in",
    "instanceof",
    "new",
    "return",
    "super",
    "switch",
    "this",
    "throw",
    "try",
    "typeof",
    "var",
    "void",
    "while",
    "with",
    "yield",
    "let",
    "static",
    "enum",
    "await",
];

fn ecmascript_identifier(name: &str) -> bool {
    crate::unit_append::is_ascii_identifier(name, ECMASCRIPT_RESERVED)
}

/// Whether `segment` is one plain directory or module name.
fn plain_segment(segment: &str) -> bool {
    !segment.is_empty()
        && !segment.starts_with('.')
        && segment != "node_modules"
        && segment
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-@".contains(&byte))
}

fn ecmascript_validate(display: &str, package: &str, name: &str) -> Result<(), String> {
    if package != "." && !package.split('/').all(plain_segment) {
        return Err(format!(
            "{display} unit package {package:?} must be \".\" or a directory relative to the \
             project root, such as \"src/runs\"; address the unit by package and name, never by \
             file path"
        ));
    }
    if !plain_segment(name) {
        return Err(format!(
            "{display} unit name {name:?} must be the module's name with no extension, such as \
             \"cache\"; Kin chooses the unit's extension"
        ));
    }
    Ok(())
}

fn ecmascript_path(
    package: &str,
    name: &str,
    role: crate::unit_editor::UnitRole,
    extension: &str,
) -> String {
    let file = match role {
        crate::unit_editor::UnitRole::Source => format!("{name}.{extension}"),
        crate::unit_editor::UnitRole::Test => format!("{name}.test.{extension}"),
    };
    if package == "." {
        file
    } else {
        format!("{package}/{file}")
    }
}

fn ecmascript_import(import: &crate::unit_editor::UnitImport) -> Result<(), String> {
    crate::unit_append::quoted_import_path(&import.path)?;
    match import.alias.as_deref() {
        Some(alias) if !ecmascript_identifier(alias) => Err(format!(
            "import alias {alias:?} must be one identifier; it names the module's namespace"
        )),
        _ => Ok(()),
    }
}

fn ecmascript_test_unit(path: &str) -> bool {
    let file = path.rsplit('/').next().unwrap_or(path);
    file.contains(".test.") || file.contains(".spec.")
}

const fn ecmascript_syntax(language: kin_model::LanguageId) -> crate::unit_append::AppendSyntax {
    crate::unit_append::AppendSyntax {
        language,
        separation: 1,
        import_kinds: &["import_statement"],
        render_import: |import| match &import.alias {
            Some(alias) => format!("import * as {alias} from \"{}\";", import.path),
            None => format!("import \"{}\";", import.path),
        },
        imported_path: |node, source| {
            let literal = node.child_by_field_name("source")?.utf8_text(source).ok()?;
            Some(
                literal
                    .trim_matches(|quote| quote == '"' || quote == '\'')
                    .to_string(),
            )
        },
        leads_imports: |node, _| {
            node.kind() == "expression_statement"
                && node
                    .named_child(0)
                    .is_some_and(|child| child.kind() == "string")
        },
    }
}

/// TypeScript's unit editor: a module named by its directory and its own
/// name, its imports first, its declarations a blank line apart.
pub(crate) const TYPESCRIPT_UNITS: crate::unit_append::AppendingEditor =
    crate::unit_append::AppendingEditor {
        syntax: ecmascript_syntax(kin_model::LanguageId::TypeScript),
        identity: crate::unit_append::UnitIdentity {
            validate: |package, name, _| ecmascript_validate("TypeScript", package, name),
            path: |package, name, role| ecmascript_path(package, name, role, "ts"),
            describe: |package, name, role| {
                format!("TypeScript module {package}/{name} ({} unit)", role.label())
            },
            validate_import: ecmascript_import,
            is_identifier: ecmascript_identifier,
            is_test_unit: ecmascript_test_unit,
        },
    };

/// JavaScript's unit editor, the same as TypeScript's under `.js`.
pub(crate) const JAVASCRIPT_UNITS: crate::unit_append::AppendingEditor =
    crate::unit_append::AppendingEditor {
        syntax: ecmascript_syntax(kin_model::LanguageId::JavaScript),
        identity: crate::unit_append::UnitIdentity {
            validate: |package, name, _| ecmascript_validate("JavaScript", package, name),
            path: |package, name, role| ecmascript_path(package, name, role, "js"),
            describe: |package, name, role| {
                format!("JavaScript module {package}/{name} ({} unit)", role.label())
            },
            validate_import: ecmascript_import,
            is_identifier: ecmascript_identifier,
            is_test_unit: ecmascript_test_unit,
        },
    };
