// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! How Python source is edited at entity grain: the hooks the edit planner
//! reads instead of naming the language.

use std::ops::Range;

use tree_sitter::Node;

use crate::declaration_lifecycle::DeclarationLifecycle;
use crate::member_placement::{indent_before, MemberPlacement, Shape, Site};
use crate::LanguageAdapter;

/// Python's top-level function lifecycle.
pub(crate) struct PythonLifecycle;

impl DeclarationLifecycle for PythonLifecycle {
    fn top_level_function<'t>(&self, statement: Node<'t>) -> Option<Node<'t>> {
        match statement.kind() {
            "function_definition" => Some(statement),
            // The adapter's span holds the decorators, so the statement does.
            "decorated_definition" => statement
                .child_by_field_name("definition")
                .filter(|definition| definition.kind() == "function_definition"),
            _ => None,
        }
    }

    fn is_nested_declaration(&self, node: &Node) -> bool {
        matches!(node.kind(), "function_definition" | "class_definition")
    }

    fn new_unit_extension(&self, _owner: &str) -> String {
        "py".into()
    }

    /// Two blank lines at the top of a module, as PEP 8 sets them.
    fn sibling_separation(&self) -> usize {
        2
    }
}

/// Python's member placement: a method in an indented class body, a class or
/// a function at the top of a module.
pub(crate) struct PythonPlacement;

fn parse(source: &[u8]) -> Result<tree_sitter::Tree, String> {
    super::PythonAdapter
        .parse(source)
        .map_err(|error| error.to_string())
}

impl MemberPlacement for PythonPlacement {
    /// Prove that `span` of `source` is one function or class definition,
    /// with its decorators, written as a statement of the module or of a
    /// class body.
    fn locate(&self, source: &[u8], span: Range<usize>) -> Result<Site, String> {
        if span.start >= span.end || span.end > source.len() {
            return Err("the declaration's span is outside its source".into());
        }
        let tree = parse(source)?;
        let root = tree.root_node();
        if root.has_error() {
            return Err("the source unit does not parse as complete valid Python".into());
        }
        let mut node = root
            .descendant_for_byte_range(span.start, span.end)
            .ok_or("the declaration's span names no syntax")?;
        // A body that holds one statement shares its bytes with it.
        while !matches!(
            node.kind(),
            "function_definition" | "class_definition" | "decorated_definition"
        ) {
            let mut cursor = node.walk();
            let inner = node
                .named_children(&mut cursor)
                .find(|child| child.byte_range() == span);
            match inner {
                Some(inner) => node = inner,
                None => break,
            }
        }
        if node.byte_range() != span {
            return Err("the declaration's span is not one whole definition".into());
        }
        let definition = match node.kind() {
            "decorated_definition" => node
                .child_by_field_name("definition")
                .ok_or("a decorated definition without its definition")?,
            _ => node,
        };
        let shape = match definition.kind() {
            "function_definition" => Shape::Function,
            "class_definition" => Shape::Class,
            _ => return Err("the span is not a function or class definition".into()),
        };
        let parent = node
            .parent()
            .ok_or("the definition has no enclosing scope")?;
        let top_level = parent.kind() == "module";
        let member = parent.kind() == "block"
            && parent
                .parent()
                .is_some_and(|owner| owner.kind() == "class_definition");
        // The grammar reads a class header with nothing under it as a class,
        // so whether the definition is all its scope holds is asked of the
        // tree and not of a second parse.
        let only_statement = {
            let mut cursor = parent.walk();
            let statements = parent
                .named_children(&mut cursor)
                .filter(|child| child.kind() != "comment")
                .count();
            statements == 1
        };
        let indent = indent_before(source, span.start)
            .ok_or("the definition does not start its own line")?;
        let body_indent = definition.child_by_field_name("body").and_then(|body| {
            let mut cursor = body.walk();
            let first = body.named_children(&mut cursor).next()?;
            (first.start_position().row != definition.start_position().row)
                .then(|| indent_before(source, first.start_byte()))
                .flatten()
        });
        // A new last member goes after the class's last statement, which
        // is where the class ends.
        let members_end = (shape == Shape::Class).then_some(span.end);
        Ok(Site {
            extent: span.clone(),
            span,
            shape,
            indent,
            top_level,
            member,
            body_indent,
            only_statement,
            members_end,
        })
    }

    /// Exactly one function or class definition, decorated or not, named
    /// `name`. A Python member is written the way a module statement is.
    fn check_one_declaration(
        &self,
        text: &str,
        name: &str,
        shape: Shape,
        _member: bool,
    ) -> Result<usize, String> {
        let tree = parse(text.as_bytes())?;
        let root = tree.root_node();
        if root.has_error() {
            return Err("EntityCreate body does not parse as complete valid Python".into());
        }
        let mut cursor = root.walk();
        let statements: Vec<_> = root.named_children(&mut cursor).collect();
        let [statement] = statements.as_slice() else {
            return Err(format!(
                "EntityCreate body must be exactly one {} definition named {name}: no imports, \
                 comments above it or other declarations",
                shape.label()
            ));
        };
        let definition = match statement.kind() {
            "decorated_definition" => statement.child_by_field_name("definition"),
            _ => Some(*statement),
        };
        let found = definition.and_then(|definition| {
            let found = match definition.kind() {
                "function_definition" => Shape::Function,
                "class_definition" => Shape::Class,
                _ => return None,
            };
            let declared = definition
                .child_by_field_name("name")?
                .utf8_text(text.as_bytes())
                .ok()?;
            Some((found, declared.to_string()))
        });
        match found {
            // A decorated definition's span holds its decorators.
            Some((found, declared)) if found == shape && declared == name => Ok(0),
            Some((found, declared)) => Err(format!(
                "EntityCreate body declares the {} {declared}, and the operation names the {} \
                 {name}",
                found.label(),
                shape.label()
            )),
            None => Err(format!(
                "EntityCreate body must be exactly one {} definition named {name}",
                shape.label()
            )),
        }
    }

    /// The lines that begin inside a string that is not a docstring. A
    /// docstring, the first statement of a definition's body, is laid out
    /// with the code around it.
    fn verbatim_lines(&self, text: &str, _member: bool) -> Result<Vec<bool>, String> {
        let tree = parse(text.as_bytes())?;
        let root = tree.root_node();
        if root.has_error() {
            return Err("EntityCreate body does not parse as complete valid Python".into());
        }
        let mut starts = vec![0usize];
        starts.extend(
            text.bytes()
                .enumerate()
                .filter(|(_, byte)| *byte == b'\n')
                .map(|(at, _)| at + 1),
        );
        let mut content = vec![false; starts.len()];
        let mut pending = vec![root];
        while let Some(node) = pending.pop() {
            if matches!(node.kind(), "string" | "concatenated_string") {
                let docstring = node.parent().is_some_and(|statement| {
                    statement.kind() == "expression_statement"
                        && statement.named_child_count() == 1
                        && statement.parent().is_some_and(|block| {
                            block.kind() == "block"
                                && block.named_child(0).map(|first| first.id())
                                    == Some(statement.id())
                        })
                });
                if !docstring {
                    for (line, start) in starts.iter().enumerate() {
                        if node.start_byte() < *start && *start < node.end_byte() {
                            content[line] = true;
                        }
                    }
                }
                continue;
            }
            let mut cursor = node.walk();
            pending.extend(node.named_children(&mut cursor));
        }
        Ok(content)
    }

    fn parses(&self, source: &[u8]) -> bool {
        parse(source).is_ok_and(|tree| !tree.root_node().has_error())
    }

    /// Two blank lines apart at the top of a module, one inside a class.
    fn gap_after(&self, site: &Site) -> &'static str {
        if site.top_level {
            "\n\n\n"
        } else {
            "\n\n"
        }
    }
}

const PYTHON_KEYWORDS: &[&str] = &[
    "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class", "continue",
    "def", "del", "elif", "else", "except", "finally", "for", "from", "global", "if", "import",
    "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "return", "try", "while",
    "with", "yield",
];

fn python_identifier(name: &str) -> bool {
    crate::unit_append::is_ascii_identifier(name, PYTHON_KEYWORDS)
}

fn dotted(path: &str) -> bool {
    !path.is_empty() && path.split('.').all(python_identifier)
}

/// Python's unit editor: a module named by its package's dotted path and
/// its own name, its `import` lines first, its declarations two blank lines
/// apart.
pub(crate) const PYTHON_UNITS: crate::unit_append::AppendingEditor =
    crate::unit_append::AppendingEditor {
        syntax: crate::unit_append::AppendSyntax {
            language: kin_model::LanguageId::Python,
            separation: 2,
            import_kinds: &[
                "import_statement",
                "import_from_statement",
                "future_import_statement",
            ],
            render_import: |import| match &import.alias {
                Some(alias) => format!("import {} as {alias}", import.path),
                None => format!("import {}", import.path),
            },
            imported_path: |node, source| {
                if node.kind() != "import_statement" || node.named_child_count() != 1 {
                    return None;
                }
                let name = node.named_child(0)?;
                let path = match name.kind() {
                    "aliased_import" => name.child_by_field_name("name")?,
                    _ => name,
                };
                path.utf8_text(source).ok().map(str::to_string)
            },
            leads_imports: |node, _| {
                node.kind() == "expression_statement"
                    && node
                        .named_child(0)
                        .is_some_and(|child| child.kind() == "string")
            },
        },
        identity: crate::unit_append::UnitIdentity {
            validate: |package, name, _| {
                if package != "." && !dotted(package) {
                    return Err(format!(
                        "Python unit package {package:?} must be \".\" or a dotted package path, \
                         such as \"shop.models\"; address the unit by package, never by file path"
                    ));
                }
                if !python_identifier(name) {
                    return Err(format!(
                        "Python unit name {name:?} must be the module's name, one Python \
                         identifier, such as \"ledger\""
                    ));
                }
                Ok(())
            },
            path: |package, name, role| {
                let file = match role {
                    crate::unit_editor::UnitRole::Source => format!("{name}.py"),
                    crate::unit_editor::UnitRole::Test => format!("test_{name}.py"),
                };
                if package == "." {
                    file
                } else {
                    format!("{}/{file}", package.replace('.', "/"))
                }
            },
            describe: |package, name, role| {
                let module = if package == "." {
                    name.to_string()
                } else {
                    format!("{package}.{name}")
                };
                format!("Python module {module} ({} unit)", role.label())
            },
            validate_import: |import| {
                if !dotted(&import.path) {
                    return Err(format!(
                        "import {:?} must be a dotted module path, such as \"os.path\"",
                        import.path
                    ));
                }
                match import.alias.as_deref() {
                    Some(alias) if !python_identifier(alias) => Err(format!(
                        "import alias {alias:?} must be one Python identifier"
                    )),
                    _ => Ok(()),
                }
            },
            is_identifier: python_identifier,
            is_test_unit: |path| {
                let file = path.rsplit('/').next().unwrap_or(path);
                file.starts_with("test_") || file.ends_with("_test.py")
            },
        },
    };
