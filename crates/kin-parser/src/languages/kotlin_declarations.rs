// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The Kotlin declaration census.
//!
//! Counted, at any depth: class, interface, enum class, object and companion
//! object declarations; functions, methods and secondary constructors; type
//! aliases; properties at file and class scope, constructor properties and
//! enum entries; and, inside a callable, a lambda, anonymous function or
//! object expression bound to a local, which is named by that local. Not
//! counted: the package header and imports, which the import evidence covers;
//! parameters and plain local variables, which are locals; and an object
//! expression in any other position, which declares no name (its members are
//! counted).

use tree_sitter::{Node, Tree};

use crate::adapter::site_from_node;
use crate::declaration_census::{plain_name, DeclarationKind, DeclarationScope, SeenDeclaration};

#[derive(Clone, Copy)]
struct Place<'a> {
    scope: DeclarationScope,
    in_callable: bool,
    owner: Option<&'a str>,
}

pub(crate) fn kotlin_declaration_census(tree: &Tree, source: &[u8]) -> Vec<SeenDeclaration> {
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

fn find_kind<'t>(node: &Node<'t>, kind: &str) -> Option<Node<'t>> {
    let mut cursor = node.walk();
    let found = node
        .named_children(&mut cursor)
        .find(|child| child.kind() == kind);
    found
}

fn has_token(node: &Node, token: &str) -> bool {
    let mut cursor = node.walk();
    let found = node
        .children(&mut cursor)
        .any(|child| !child.is_named() && child.kind() == token);
    found
}

fn has_modifier(node: &Node, source: &[u8], word: &str) -> bool {
    find_kind(node, "modifiers").is_some_and(|modifiers| {
        modifiers
            .utf8_text(source)
            .is_ok_and(|text| text.split_whitespace().any(|w| w == word))
    })
}

/// The single name a property declaration binds.
fn property_name(node: &Node, source: &[u8]) -> Option<String> {
    let declaration = find_kind(node, "variable_declaration")?;
    let name = find_kind(&declaration, "identifier")?;
    plain_name(&name, source)
}

fn walk(node: &Node, source: &[u8], place: Place<'_>, out: &mut Vec<SeenDeclaration>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "class_declaration" | "object_declaration" | "companion_object" => {
                let name = child
                    .child_by_field_name("name")
                    .and_then(|name| plain_name(&name, source))
                    .or_else(|| (child.kind() == "companion_object").then(|| "Companion".into()));
                let kind = if has_token(&child, "interface") {
                    DeclarationKind::Interface
                } else if has_modifier(&child, source, "enum") {
                    DeclarationKind::Enum
                } else {
                    DeclarationKind::Class
                };
                if let Some(name) = &name {
                    push(out, place, name.clone(), kind, &child);
                }
                let inner = Place {
                    scope: DeclarationScope::Class,
                    owner: name.as_deref().or(place.owner),
                    ..place
                };
                if let Some(parameters) = find_kind(&child, "primary_constructor")
                    .and_then(|constructor| find_kind(&constructor, "class_parameters"))
                {
                    let mut parameters_cursor = parameters.walk();
                    for parameter in parameters.named_children(&mut parameters_cursor) {
                        if parameter.kind() == "class_parameter"
                            && (has_token(&parameter, "val") || has_token(&parameter, "var"))
                        {
                            if let Some(name) = find_kind(&parameter, "identifier")
                                .and_then(|n| plain_name(&n, source))
                            {
                                push(out, inner, name, DeclarationKind::Attribute, &parameter);
                            }
                        }
                    }
                }
                let mut body_cursor = child.walk();
                for body in child.children(&mut body_cursor) {
                    if matches!(body.kind(), "class_body" | "enum_class_body") {
                        walk(&body, source, inner, out);
                    }
                }
            }
            "enum_entry" => {
                if let Some(name) =
                    find_kind(&child, "identifier").and_then(|n| plain_name(&n, source))
                {
                    push(out, place, name, DeclarationKind::Attribute, &child);
                }
                walk(&child, source, place, out);
            }
            "function_declaration" | "secondary_constructor" => {
                let name = if child.kind() == "secondary_constructor" {
                    Some("constructor".to_string())
                } else {
                    child
                        .child_by_field_name("name")
                        .and_then(|name| plain_name(&name, source))
                };
                // A function directly in a class or object body is a method,
                // wherever that body sits.
                let kind = if matches!(node.kind(), "class_body" | "enum_class_body") {
                    DeclarationKind::Method
                } else {
                    DeclarationKind::Function
                };
                if let Some(name) = &name {
                    push(out, place, name.clone(), kind, &child);
                }
                walk(
                    &child,
                    source,
                    Place {
                        in_callable: true,
                        owner: name.as_deref().or(place.owner),
                        ..place
                    },
                    out,
                );
            }
            "type_alias" => {
                if let Some(name) = child
                    .child_by_field_name("type")
                    .and_then(|name| plain_name(&name, source))
                {
                    push(out, place, name, DeclarationKind::TypeAlias, &child);
                }
            }
            "property_declaration" => {
                if !place.in_callable {
                    if let Some(name) = property_name(&child, source) {
                        let kind = if place.scope == DeclarationScope::Class {
                            DeclarationKind::Attribute
                        } else {
                            DeclarationKind::Variable
                        };
                        push(out, place, name, kind, &child);
                    }
                } else if let Some(value) =
                    crate::value_origins::kotlin::property_initializer(child)
                {
                    let kind = match value.kind() {
                        "lambda_literal" | "anonymous_function" => Some(DeclarationKind::Function),
                        "object_literal" => Some(DeclarationKind::Class),
                        _ => None,
                    };
                    if let (Some(kind), Some(name)) = (kind, property_name(&child, source)) {
                        push(out, place, name, kind, &child);
                    }
                }
                // Accessors are callables; an initializer can hold an object
                // expression or a lambda.
                walk(&child, source, place, out);
            }
            "getter"
            | "setter"
            | "anonymous_initializer"
            | "lambda_literal"
            | "anonymous_function" => walk(
                &child,
                source,
                Place {
                    in_callable: true,
                    ..place
                },
                out,
            ),
            "object_literal" => {
                if let Some(body) = find_kind(&child, "class_body") {
                    walk(
                        &body,
                        source,
                        Place {
                            scope: DeclarationScope::Class,
                            ..place
                        },
                        out,
                    );
                }
            }
            "import" | "package_header" => {}
            _ => walk(&child, source, place, out),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::adapter::LanguageAdapter;
    use crate::declaration_census::{take_census, REASON_CLASS_MEMBER, REASON_NESTED_IN_CALLABLE};
    use crate::languages::kotlin::KotlinAdapter;
    use kin_model::FilePathId;

    fn census(source: &str) -> crate::declaration_census::DeclarationCensus {
        let adapter = KotlinAdapter;
        let tree = adapter.parse(source.as_bytes()).unwrap();
        assert!(!tree.root_node().has_error());
        let output = adapter
            .extract(&tree, source.as_bytes(), &FilePathId::new("app/Service.kt"))
            .unwrap();
        take_census(
            adapter
                .declaration_census(&tree, source.as_bytes())
                .unwrap(),
            &output.entities,
        )
    }

    #[test]
    fn admitted_declarations_pair_and_callbacks_and_companions_are_listed() {
        let source = r#"package app

const val LIMIT = 3

class Service(private val store: Store, plain: Int) : Runnable {
    private val cache = Cache()

    override fun run() {
        val later = { store.load() }
        executor.submit(object : Runnable {
            override fun run() {}
        })
        fun helper() {}
        class Local {
            fun go() {}
        }
    }

    companion object {
        fun create(): Service = Service(Store(), 1)
    }
}

enum class Mode { FAST, SLOW }
typealias Handler = () -> Unit
"#;
        let census = census(source);
        let unadmitted: Vec<_> = census
            .unadmitted
            .iter()
            .map(|d| (d.name.as_str(), d.kind.label(), d.reason.as_str()))
            .collect();
        assert_eq!(
            unadmitted,
            vec![
                ("later", "function", REASON_NESTED_IN_CALLABLE),
                ("run", "method", REASON_NESTED_IN_CALLABLE),
                ("helper", "function", REASON_NESTED_IN_CALLABLE),
                ("Companion", "class", REASON_CLASS_MEMBER),
            ],
            "{census:?}"
        );
        // LIMIT, Service, store, cache, run, Local, Local.go, create, Mode,
        // FAST, SLOW and Handler.
        assert_eq!(census.admitted, 12, "{census:?}");
        assert_eq!(census.seen, 16);
    }
}
