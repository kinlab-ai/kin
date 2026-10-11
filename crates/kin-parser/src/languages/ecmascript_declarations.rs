// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The ECMAScript declaration census, shared by the JavaScript and TypeScript
//! adapters and by Flow files either grammar parses.
//!
//! Counted, at any depth: function and generator declarations; class
//! declarations and named class expressions bound to a name; methods of
//! classes and of object literals; class fields; object-literal properties
//! whose value is a function; TypeScript interfaces, type aliases and enums,
//! including those inside a namespace; and variable declarators that bind a
//! plain name, at module scope whatever their value, and inside a callable
//! only when the value is a function or a class. Not counted: `require`
//! bindings and imports, which the import evidence covers; plain local
//! variables, which are locals; destructuring patterns, which declare no
//! single name; and namespaces themselves, which the adapter admits as module
//! entities.

use tree_sitter::{Node, Tree};

use crate::adapter::site_from_node;
use crate::declaration_census::{plain_name, DeclarationKind, DeclarationScope, SeenDeclaration};

#[derive(Clone, Copy)]
struct Place<'a> {
    scope: DeclarationScope,
    in_callable: bool,
    owner: Option<&'a str>,
}

pub(crate) fn ecmascript_declaration_census(tree: &Tree, source: &[u8]) -> Vec<SeenDeclaration> {
    let mut out = Vec::new();
    walk(
        &tree.root_node(),
        source,
        Place {
            scope: DeclarationScope::Module,
            in_callable: false,
            owner: None,
        },
        &mut out,
    );
    out
}

fn push(
    out: &mut Vec<SeenDeclaration>,
    place: Place<'_>,
    name: String,
    kind: DeclarationKind,
    node: &Node,
) {
    out.push(SeenDeclaration {
        name,
        kind,
        scope: place.scope,
        in_callable: place.in_callable,
        owner: place.owner.map(str::to_string),
        site: site_from_node(node),
    });
}

fn is_function_like(node: &Node) -> bool {
    matches!(
        node.kind(),
        "function_expression" | "function" | "arrow_function" | "generator_function"
    )
}

fn is_require_call(node: &Node, source: &[u8]) -> bool {
    let mut current = *node;
    // `require('x').member` and `await import('x')` bind an import too.
    loop {
        match current.kind() {
            "member_expression" | "await_expression" | "parenthesized_expression" => {
                match current.named_child(0) {
                    Some(inner) => current = inner,
                    None => return false,
                }
            }
            "call_expression" => {
                return current
                    .child_by_field_name("function")
                    .and_then(|function| function.utf8_text(source).ok())
                    .is_some_and(|callee| callee == "require" || callee == "import");
            }
            _ => return false,
        }
    }
}

fn named(node: &Node, source: &[u8]) -> Option<String> {
    node.child_by_field_name("name")
        .and_then(|name| plain_name(&name, source))
}

/// Walk a callable's parameters and body in the callable's own scope.
fn walk_callable(
    node: &Node,
    source: &[u8],
    place: Place<'_>,
    owner: Option<&str>,
    out: &mut Vec<SeenDeclaration>,
) {
    walk(
        node,
        source,
        Place {
            scope: place.scope,
            in_callable: true,
            owner: owner.or(place.owner),
        },
        out,
    );
}

fn walk(node: &Node, source: &[u8], place: Place<'_>, out: &mut Vec<SeenDeclaration>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "function_declaration" | "generator_function_declaration" => {
                let name = named(&child, source);
                if let Some(name) = &name {
                    push(out, place, name.clone(), DeclarationKind::Function, &child);
                }
                walk_callable(&child, source, place, name.as_deref(), out);
            }
            "class_declaration" | "abstract_class_declaration" => {
                let name = named(&child, source);
                if let Some(name) = &name {
                    push(out, place, name.clone(), DeclarationKind::Class, &child);
                }
                walk_class(&child, source, place, name.as_deref(), out);
            }
            "class" => {
                // A class expression. A bound one is counted by its binding;
                // an anonymous one still declares its members.
                let name = named(&child, source);
                walk_class(&child, source, place, name.as_deref(), out);
            }
            "interface_declaration" => {
                if let Some(name) = named(&child, source) {
                    push(out, place, name, DeclarationKind::Interface, &child);
                }
            }
            "type_alias_declaration" => {
                if let Some(name) = named(&child, source) {
                    push(out, place, name, DeclarationKind::TypeAlias, &child);
                }
            }
            "enum_declaration" => {
                if let Some(name) = named(&child, source) {
                    push(out, place, name, DeclarationKind::Enum, &child);
                }
            }
            // A namespace is admitted as a module entity, which the comparison
            // never pairs, so it is walked through rather than counted, as the
            // C++, C#, PHP and Ruby censuses do.
            "module" | "internal_module" => {
                let name = named(&child, source);
                walk(
                    &child,
                    source,
                    Place {
                        scope: if place.in_callable {
                            place.scope
                        } else {
                            DeclarationScope::ModuleBlock
                        },
                        owner: name.as_deref().or(place.owner),
                        ..place
                    },
                    out,
                );
            }
            "method_definition" => {
                let name = named(&child, source);
                if let Some(name) = &name {
                    push(out, place, name.clone(), DeclarationKind::Method, &child);
                }
                walk_callable(&child, source, place, name.as_deref(), out);
            }
            "public_field_definition" | "field_definition" => {
                let name = child
                    .child_by_field_name("name")
                    .or_else(|| child.child_by_field_name("property"))
                    .and_then(|name| plain_name(&name, source));
                let value = child.child_by_field_name("value");
                let kind = if value.as_ref().is_some_and(is_function_like) {
                    DeclarationKind::Method
                } else {
                    DeclarationKind::Attribute
                };
                if let Some(name) = &name {
                    push(out, place, name.clone(), kind, &child);
                }
                walk_callable(&child, source, place, name.as_deref(), out);
            }
            "pair" if place.scope == DeclarationScope::Object => {
                let name = child
                    .child_by_field_name("key")
                    .and_then(|key| plain_name(&key, source));
                let value = child.child_by_field_name("value");
                if let (Some(name), Some(value)) = (&name, &value) {
                    if is_function_like(value) {
                        push(out, place, name.clone(), DeclarationKind::Method, &child);
                    }
                }
                walk(
                    &child,
                    source,
                    Place {
                        owner: name.as_deref().or(place.owner),
                        ..place
                    },
                    out,
                );
            }
            "object" => walk(
                &child,
                source,
                Place {
                    scope: DeclarationScope::Object,
                    ..place
                },
                out,
            ),
            "variable_declarator" => declarator(&child, source, place, out),
            "import_statement" => {}
            "arrow_function" | "function_expression" | "function" | "generator_function" => {
                walk_callable(&child, source, place, None, out);
            }
            "statement_block" | "if_statement" | "try_statement" | "for_statement"
            | "for_in_statement" | "while_statement" | "do_statement" | "switch_statement"
            | "labeled_statement"
                if place.scope == DeclarationScope::Module && !place.in_callable =>
            {
                walk(
                    &child,
                    source,
                    Place {
                        scope: DeclarationScope::ModuleBlock,
                        ..place
                    },
                    out,
                );
            }
            "class_static_block" => walk_callable(&child, source, place, None, out),
            _ => walk(&child, source, place, out),
        }
    }
}

fn walk_class(
    node: &Node,
    source: &[u8],
    place: Place<'_>,
    name: Option<&str>,
    out: &mut Vec<SeenDeclaration>,
) {
    let mut cursor = node.walk();
    for part in node.children(&mut cursor) {
        let scope = if part.kind() == "class_body" {
            DeclarationScope::Class
        } else {
            place.scope
        };
        walk(
            &part,
            source,
            Place {
                scope,
                in_callable: place.in_callable,
                owner: name.or(place.owner),
            },
            out,
        );
    }
}

fn declarator(node: &Node, source: &[u8], place: Place<'_>, out: &mut Vec<SeenDeclaration>) {
    let name_node = node.child_by_field_name("name");
    let name = name_node
        .filter(|name| name.kind() == "identifier")
        .and_then(|name| plain_name(&name, source));
    let value = node.child_by_field_name("value");
    let kind = match value.as_ref() {
        Some(value) if is_function_like(value) => Some(DeclarationKind::Function),
        Some(value) if value.kind() == "class" => Some(DeclarationKind::Class),
        Some(value) if is_require_call(value, source) => None,
        _ if place.in_callable => None,
        _ => Some(DeclarationKind::Variable),
    };
    if let (Some(name), Some(kind)) = (&name, kind) {
        let place = Place {
            // A binding sits in the statement's scope, never in an object.
            scope: match place.scope {
                DeclarationScope::Object | DeclarationScope::Class => DeclarationScope::ModuleBlock,
                scope => scope,
            },
            ..place
        };
        push(out, place, name.clone(), kind, node);
    }
    if let Some(value) = value {
        let owner = name.as_deref().or(place.owner);
        if value.kind() == "class" {
            walk_class(&value, source, place, owner, out);
        } else if is_function_like(&value) {
            walk_callable(&value, source, place, owner, out);
        } else {
            walk(node, source, Place { owner, ..place }, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::adapter::LanguageAdapter;
    use crate::declaration_census::{
        take_census, REASON_MODULE_BINDING, REASON_NESTED_IN_CALLABLE,
    };
    use kin_model::FilePathId;

    fn census(
        adapter: &dyn LanguageAdapter,
        path: &str,
        source: &str,
    ) -> crate::declaration_census::DeclarationCensus {
        let tree = adapter.parse(source.as_bytes()).unwrap();
        let output = adapter
            .extract(&tree, source.as_bytes(), &FilePathId::new(path))
            .unwrap();
        take_census(
            adapter
                .declaration_census(&tree, source.as_bytes())
                .unwrap(),
            &output.entities,
        )
    }

    /// A local object literal's methods are entities of their own, so they
    /// pair; a plain function nested in a callable is not admitted and is
    /// listed.
    #[test]
    fn javascript_nested_object_methods_pair_and_nested_functions_are_listed() {
        let source = "\
const THEME = { color: 'red' };
export function createNoop() {
  const hostConfig = {
    commitTextUpdate(textInstance, oldText, newText) {
      textInstance.text = newText;
    },
  };
  function helper() {}
  return hostConfig;
}
export class Store {
  containsElement(id) { return true; }
}
";
        let census = census(
            &crate::languages::javascript::JavaScriptAdapter,
            "src/noop.js",
            source,
        );
        let unadmitted: Vec<_> = census
            .unadmitted
            .iter()
            .map(|d| (d.name.as_str(), d.kind.label(), d.reason.as_str()))
            .collect();
        assert_eq!(
            unadmitted,
            vec![
                ("THEME", "variable", REASON_MODULE_BINDING),
                ("helper", "function", REASON_NESTED_IN_CALLABLE),
            ],
            "{census:?}"
        );
        for admitted in ["createNoop", "commitTextUpdate", "Store", "containsElement"] {
            assert!(
                !unadmitted.iter().any(|(name, _, _)| *name == admitted),
                "{admitted} should pair with its entity: {unadmitted:?}"
            );
        }
        assert_eq!(census.admitted, 4, "{census:?}");
        assert_eq!(census.seen, 6, "{census:?}");
    }

    #[test]
    fn typescript_types_and_top_level_functions_pair() {
        let source = "\
export interface Props { id: number }
export type Id = number;
export function render(props: Props): Id {
  const local = (x: number) => x;
  return local(props.id);
}
";
        let census = census(
            &crate::languages::typescript::TypeScriptAdapter,
            "src/render.ts",
            source,
        );
        let unadmitted: Vec<_> = census.unadmitted.iter().map(|d| d.name.as_str()).collect();
        assert!(unadmitted.contains(&"local"), "{census:?}");
        assert!(!unadmitted.contains(&"render"), "{census:?}");
    }
}
