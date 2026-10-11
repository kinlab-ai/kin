// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The PHP declaration census.
//!
//! Counted: every function at any depth, every method, every class,
//! interface, trait and enum, every enum case, every constant (in a class
//! body as a member, at the top level as a variable), every property and
//! promoted constructor property (as members), and every closure bound to a
//! local (a nested definition). Not counted: `use` imports, which the import
//! evidence covers, a namespace, which the adapter admits as the file's
//! module surface, an anonymous class, which has no name, and a function's
//! other locals and parameters.

use tree_sitter::{Node, Tree};

use crate::adapter::site_from_node;
use crate::declaration_census::{plain_name, DeclarationKind, DeclarationScope, SeenDeclaration};

#[derive(Clone, Copy)]
struct Place<'a> {
    scope: DeclarationScope,
    in_callable: bool,
    owner: Option<&'a str>,
}

pub(crate) fn census(tree: &Tree, source: &[u8]) -> Vec<SeenDeclaration> {
    let mut out = Vec::new();
    walk(
        tree.root_node(),
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
    node: Node<'_>,
) {
    out.push(SeenDeclaration {
        name,
        kind,
        scope: place.scope,
        in_callable: place.in_callable,
        owner: place.owner.map(str::to_string),
        site: site_from_node(&node),
    });
}

fn name_of(node: Node<'_>, source: &[u8]) -> Option<String> {
    let name = node.child_by_field_name("name")?;
    plain_name(&name, source).map(|name| name.trim_start_matches('$').to_string())
}

fn walk(node: Node<'_>, source: &[u8], place: Place<'_>, out: &mut Vec<SeenDeclaration>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit(child, source, place, out);
    }
}

fn visit(child: Node<'_>, source: &[u8], place: Place<'_>, out: &mut Vec<SeenDeclaration>) {
    match child.kind() {
        "function_definition" | "method_declaration" => {
            let name = name_of(child, source);
            if let Some(name) = &name {
                let kind = if child.kind() == "method_declaration" {
                    DeclarationKind::Method
                } else {
                    DeclarationKind::Function
                };
                push(out, place, name.clone(), kind, child);
            }
            if let Some(params) = child.child_by_field_name("parameters") {
                // Promoted constructor properties are members of the class.
                let mut cursor = params.walk();
                for param in params.named_children(&mut cursor) {
                    if param.kind() == "property_promotion_parameter" {
                        if let Some(prop) = name_of(param, source) {
                            push(out, place, prop, DeclarationKind::Attribute, param);
                        }
                    }
                }
            }
            if let Some(body) = child.child_by_field_name("body") {
                walk(
                    body,
                    source,
                    Place {
                        scope: place.scope,
                        in_callable: true,
                        owner: name.as_deref().or(place.owner),
                    },
                    out,
                );
            }
        }
        "class_declaration"
        | "interface_declaration"
        | "trait_declaration"
        | "enum_declaration" => {
            let name = name_of(child, source);
            if let Some(name) = &name {
                let kind = match child.kind() {
                    "interface_declaration" | "trait_declaration" => DeclarationKind::Interface,
                    "enum_declaration" => DeclarationKind::Enum,
                    _ => DeclarationKind::Class,
                };
                push(out, place, name.clone(), kind, child);
            }
            if let Some(body) = child.child_by_field_name("body") {
                walk(
                    body,
                    source,
                    Place {
                        scope: DeclarationScope::Class,
                        in_callable: place.in_callable,
                        owner: name.as_deref().or(place.owner),
                    },
                    out,
                );
            }
        }
        "anonymous_class" => {
            if let Some(body) = child.child_by_field_name("body") {
                walk(
                    body,
                    source,
                    Place {
                        scope: DeclarationScope::Class,
                        ..place
                    },
                    out,
                );
            }
        }
        "enum_case" => {
            if let Some(name) = name_of(child, source) {
                push(out, place, name, DeclarationKind::Attribute, child);
            }
        }
        "const_declaration" => {
            let mut cursor = child.walk();
            for element in child.named_children(&mut cursor) {
                if element.kind() != "const_element" {
                    continue;
                }
                let mut inner = element.walk();
                let name = element
                    .named_children(&mut inner)
                    .find(|part| part.kind() == "name")
                    .and_then(|name| plain_name(&name, source));
                if let Some(name) = name {
                    let kind = if place.scope == DeclarationScope::Class {
                        DeclarationKind::Attribute
                    } else {
                        DeclarationKind::Variable
                    };
                    push(out, place, name, kind, child);
                }
            }
        }
        "property_declaration" => {
            let mut cursor = child.walk();
            for element in child.named_children(&mut cursor) {
                if element.kind() == "property_element" {
                    if let Some(name) = name_of(element, source) {
                        push(out, place, name, DeclarationKind::Attribute, element);
                    }
                }
            }
        }
        "assignment_expression" if place.in_callable => {
            // A closure bound to a local is a nested definition.
            let right = child.child_by_field_name("right");
            if right.is_some_and(|right| {
                matches!(right.kind(), "anonymous_function" | "arrow_function")
            }) {
                if let Some(name) = right
                    .and_then(|right| crate::value_origins::php::bound_closure_name(right, source))
                {
                    push(
                        out,
                        place,
                        name,
                        DeclarationKind::Function,
                        right.unwrap_or(child),
                    );
                }
            }
            walk(child, source, place, out);
        }
        "namespace_use_declaration" | "comment" => {}
        _ => walk(child, source, place, out),
    }
}

#[cfg(test)]
mod tests {
    use crate::declaration_census::{take_census, DeclarationCensus, REASON_NESTED_IN_CALLABLE};
    use crate::{LanguageAdapter, PhpAdapter};
    use kin_model::FilePathId;

    fn census(source: &str) -> DeclarationCensus {
        let adapter = PhpAdapter;
        let tree = adapter.parse(source.as_bytes()).unwrap();
        assert!(
            !tree.root_node().has_error(),
            "{}",
            tree.root_node().to_sexp()
        );
        let output = adapter
            .extract(
                &tree,
                source.as_bytes(),
                &FilePathId::new("app/Models/Store.php"),
            )
            .unwrap();
        take_census(
            adapter
                .declaration_census(&tree, source.as_bytes())
                .expect("the PHP adapter takes a census"),
            &output.entities,
        )
    }

    #[test]
    fn admitted_declarations_pair_and_the_rest_are_listed_with_a_reason() {
        let source = r#"<?php
namespace App\Models;

const DEFAULT_SHARDS = 4;

interface Persisting
{
    public function save(): void;
}

trait Audits
{
    public function audit(): void {}
}

enum Mode: string
{
    case Fast = 'fast';
    case Safe = 'safe';

    public function label(): string { return $this->value; }
}

final class Store implements Persisting
{
    use Audits;

    public const LIMIT = 3;
    private array $rows = [];

    public function __construct(private readonly Cache $cache) {}

    public function save(): void
    {
        $onDone = function () { report(); };
        $again = function () {};
        $again = function () {};
        $onDone();
    }
}

function make_store(): Store
{
    return new Store(new Cache());
}
"#;
        let census = census(source);
        let listed: Vec<(&str, &str)> = census
            .unadmitted
            .iter()
            .map(|declaration| (declaration.name.as_str(), declaration.reason.as_str()))
            .collect();
        for admitted in [
            "DEFAULT_SHARDS",
            "Persisting",
            "save",
            "Audits",
            "audit",
            "Mode",
            "Fast",
            "Safe",
            "label",
            "rows",
            "cache",
            "Store",
            "LIMIT",
            "__construct",
            "onDone",
            "make_store",
        ] {
            assert!(
                !listed.iter().any(|(name, _)| *name == admitted),
                "{admitted} is admitted: {listed:?}"
            );
        }
        let expected = ("again", REASON_NESTED_IN_CALLABLE);
        assert!(listed.contains(&expected), "{expected:?} in {listed:?}");
        assert_eq!(census.seen, census.admitted + census.unadmitted.len());
    }
}
