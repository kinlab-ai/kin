// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The C# declaration census.
//!
//! Counted, at any depth: class, struct, record, interface, enum and
//! delegate declarations; methods, constructors,
//! destructors and operators by name, and local functions inside a callable;
//! properties, events, fields and enum members. Not counted: namespaces,
//! which name the unit's module surface; using directives, which the import
//! evidence covers; parameters and local
//! variables, which are locals; indexers and operators, which declare no
//! name a reader searches for.

use tree_sitter::{Node, Tree};

use crate::adapter::site_from_node;
use crate::declaration_census::{plain_name, DeclarationKind, DeclarationScope, SeenDeclaration};

#[derive(Clone, Copy)]
struct Place<'a> {
    in_callable: bool,
    owner: Option<&'a str>,
}

pub(crate) fn csharp_declaration_census(tree: &Tree, source: &[u8]) -> Vec<SeenDeclaration> {
    let mut out = Vec::new();
    walk(
        &tree.root_node(),
        source,
        DeclarationScope::Module,
        Place {
            in_callable: false,
            owner: None,
        },
        &mut out,
    );
    out
}

fn push(
    out: &mut Vec<SeenDeclaration>,
    scope: DeclarationScope,
    place: Place<'_>,
    name: String,
    kind: DeclarationKind,
    node: &Node,
) {
    out.push(SeenDeclaration {
        name,
        kind,
        scope,
        in_callable: place.in_callable,
        owner: place.owner.map(str::to_string),
        site: site_from_node(node),
    });
}

fn name_of(node: &Node, source: &[u8]) -> Option<String> {
    node.child_by_field_name("name")
        .and_then(|name| plain_name(&name, source))
}

/// The variables a field or event field declaration declares.
fn declarators<'t>(node: &Node<'t>) -> Vec<Node<'t>> {
    let mut found = Vec::new();
    let mut pending = vec![*node];
    while let Some(current) = pending.pop() {
        let mut cursor = current.walk();
        for child in current.named_children(&mut cursor) {
            match child.kind() {
                "variable_declarator" => found.push(child),
                "variable_declaration" => pending.push(child),
                _ => {}
            }
        }
    }
    found
}

fn declarator_name(node: &Node, source: &[u8]) -> Option<String> {
    name_of(node, source).or_else(|| {
        let mut cursor = node.walk();
        let identifier = node
            .named_children(&mut cursor)
            .find(|child| child.kind() == "identifier");
        identifier.and_then(|identifier| plain_name(&identifier, source))
    })
}

fn walk(
    node: &Node,
    source: &[u8],
    scope: DeclarationScope,
    place: Place<'_>,
    out: &mut Vec<SeenDeclaration>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            // A namespace is the unit's own module surface, which the census
            // does not pair; its members are module scope.
            "namespace_declaration" | "file_scoped_namespace_declaration" => {
                if let Some(body) = child.child_by_field_name("body") {
                    walk(&body, source, DeclarationScope::Module, place, out);
                }
            }
            "class_declaration"
            | "struct_declaration"
            | "record_declaration"
            | "interface_declaration"
            | "enum_declaration"
            | "delegate_declaration" => {
                let name = name_of(&child, source);
                let kind = match child.kind() {
                    "interface_declaration" => DeclarationKind::Interface,
                    "enum_declaration" => DeclarationKind::Enum,
                    "delegate_declaration" => DeclarationKind::Function,
                    _ => DeclarationKind::Class,
                };
                if let Some(name) = &name {
                    push(out, scope, place, name.clone(), kind, &child);
                }
                if let Some(body) = child.child_by_field_name("body") {
                    walk(
                        &body,
                        source,
                        DeclarationScope::Class,
                        Place {
                            owner: name.as_deref().or(place.owner),
                            ..place
                        },
                        out,
                    );
                }
            }
            "method_declaration" | "constructor_declaration" | "destructor_declaration" => {
                let name = name_of(&child, source);
                if let Some(name) = &name {
                    push(
                        out,
                        scope,
                        place,
                        name.clone(),
                        DeclarationKind::Method,
                        &child,
                    );
                }
                walk(
                    &child,
                    source,
                    scope,
                    Place {
                        in_callable: true,
                        owner: name.as_deref().or(place.owner),
                    },
                    out,
                );
            }
            "local_function_statement" => {
                let name = name_of(&child, source);
                if let Some(name) = &name {
                    push(
                        out,
                        scope,
                        place,
                        name.clone(),
                        DeclarationKind::Function,
                        &child,
                    );
                }
                walk(
                    &child,
                    source,
                    scope,
                    Place {
                        in_callable: true,
                        owner: name.as_deref().or(place.owner),
                    },
                    out,
                );
            }
            "property_declaration" | "event_declaration" | "enum_member_declaration" => {
                if let Some(name) = name_of(&child, source) {
                    push(
                        out,
                        DeclarationScope::Class,
                        place,
                        name,
                        DeclarationKind::Attribute,
                        &child,
                    );
                }
                // Accessor bodies are callables.
                walk(
                    &child,
                    source,
                    scope,
                    Place {
                        in_callable: true,
                        ..place
                    },
                    out,
                );
            }
            "field_declaration" | "event_field_declaration" => {
                for declarator in declarators(&child) {
                    if let Some(name) = declarator_name(&declarator, source) {
                        push(
                            out,
                            DeclarationScope::Class,
                            place,
                            name,
                            DeclarationKind::Attribute,
                            &declarator,
                        );
                    }
                }
            }
            "using_directive" => {}
            _ => walk(&child, source, scope, place, out),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::declaration_census::{take_census, REASON_NESTED_IN_CALLABLE};
    use crate::{CSharpAdapter, LanguageAdapter};
    use kin_model::FilePathId;

    #[test]
    fn admitted_declarations_pair_and_the_rest_are_listed_with_a_reason() {
        let source = b"using System;

namespace Shop
{
    public class Forms
    {
        private int count;
        public string Label { get; set; }

        public static int Total(int[] values)
        {
            int Double(int x) => x * 2;
            return Double(values.Length);
        }

        public class Inner
        {
            public string Ping() => \"pong\";
        }
    }

    public enum Mode { Fast, Safe }
}
";
        let adapter = CSharpAdapter;
        let tree = adapter.parse(source).unwrap();
        let output = adapter
            .extract(&tree, source, &FilePathId::new("src/Forms.cs"))
            .unwrap();
        let seen = adapter.declaration_census(&tree, source).expect("a census");
        let census = take_census(seen, &output.entities);
        let unadmitted: Vec<_> = census
            .unadmitted
            .iter()
            .map(|declaration| (declaration.name.as_str(), declaration.reason.as_str()))
            .collect();
        for admitted in ["Forms", "Total", "Inner", "Ping", "Mode", "count", "Label"] {
            assert!(
                !unadmitted.iter().any(|(name, _)| *name == admitted),
                "{admitted} is admitted: {unadmitted:?}"
            );
        }
        assert!(
            unadmitted.contains(&("Double", REASON_NESTED_IN_CALLABLE)),
            "{unadmitted:?}"
        );
        assert_eq!(census.seen, census.admitted + census.unadmitted.len());
    }
}
