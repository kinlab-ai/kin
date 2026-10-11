// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The Swift declaration census.
//!
//! Counted: every function at any depth, every initializer (`init`),
//! deinitializer (`deinit`) and subscript (`subscript`), every class,
//! struct, enum, actor and protocol, every protocol requirement, every
//! enum case, every property in a type body or extension (as a member) and
//! at the top level, every type alias and associated type, and every
//! closure bound to a local (a nested definition). Not counted: imports,
//! which the import evidence covers, an extension, which declares no new
//! name, and a function's other locals and parameters.

use tree_sitter::{Node, Tree};

use crate::adapter::site_from_node;
use crate::declaration_census::{plain_name, DeclarationKind, DeclarationScope, SeenDeclaration};
use crate::value_origins::swift::{type_body, type_declaration_name};

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

fn walk(node: Node<'_>, source: &[u8], place: Place<'_>, out: &mut Vec<SeenDeclaration>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit(child, source, place, out);
    }
}

fn visit(child: Node<'_>, source: &[u8], place: Place<'_>, out: &mut Vec<SeenDeclaration>) {
    let member = place.scope == DeclarationScope::Class && !place.in_callable;
    match child.kind() {
        "class_declaration" | "protocol_declaration" => {
            let name = type_declaration_name(child, source);
            let kind_node = child
                .child_by_field_name("declaration_kind")
                .map(|kind| kind.kind());
            if kind_node != Some("extension") {
                if let Some(name) = &name {
                    let kind = match kind_node {
                        Some("enum") => DeclarationKind::Enum,
                        Some("protocol") => DeclarationKind::Interface,
                        _ if child.kind() == "protocol_declaration" => DeclarationKind::Interface,
                        _ => DeclarationKind::Class,
                    };
                    push(out, place, name.clone(), kind, child);
                }
            }
            if let Some(body) = type_body(child) {
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
        "function_declaration"
        | "protocol_function_declaration"
        | "init_declaration"
        | "deinit_declaration"
        | "subscript_declaration" => {
            let name = match child.kind() {
                "init_declaration" => Some("init".to_string()),
                "deinit_declaration" => Some("deinit".to_string()),
                "subscript_declaration" => Some("subscript".to_string()),
                _ => child
                    .child_by_field_name("name")
                    .and_then(|name| plain_name(&name, source)),
            };
            if let Some(name) = &name {
                let kind = if member {
                    DeclarationKind::Method
                } else {
                    DeclarationKind::Function
                };
                push(out, place, name.clone(), kind, child);
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
        "property_declaration" | "protocol_property_declaration" => {
            let bound = child
                .child_by_field_name("name")
                .and_then(|pattern| pattern.child_by_field_name("bound_identifier"))
                .and_then(|name| plain_name(&name, source));
            if let Some(name) = bound {
                if place.in_callable {
                    // A local is no declaration a name search is asked to
                    // find; a closure bound to one is a nested definition.
                    let closure = child
                        .child_by_field_name("value")
                        .is_some_and(|value| value.kind() == "lambda_literal");
                    if closure {
                        push(out, place, name, DeclarationKind::Function, child);
                    }
                } else {
                    let kind = if place.scope == DeclarationScope::Class {
                        DeclarationKind::Attribute
                    } else {
                        DeclarationKind::Variable
                    };
                    push(out, place, name, kind, child);
                }
            }
            // Accessors and initial values can hold closures and nested
            // declarations.
            walk(
                child,
                source,
                Place {
                    in_callable: true,
                    ..place
                },
                out,
            );
        }
        "enum_entry" => {
            let mut cursor = child.walk();
            for name in child.children_by_field_name("name", &mut cursor) {
                if let Some(name) = plain_name(&name, source) {
                    push(out, place, name, DeclarationKind::Attribute, child);
                }
            }
        }
        "typealias_declaration" | "associatedtype_declaration" => {
            if let Some(name) = child
                .child_by_field_name("name")
                .and_then(|name| plain_name(&name, source))
            {
                push(out, place, name, DeclarationKind::TypeAlias, child);
            }
        }
        "import_declaration" | "comment" | "multiline_comment" => {}
        _ => walk(child, source, place, out),
    }
}

#[cfg(test)]
mod tests {
    use crate::declaration_census::{
        take_census, DeclarationCensus, REASON_CLASS_MEMBER, REASON_NESTED_IN_CALLABLE,
    };
    use crate::{LanguageAdapter, SwiftAdapter};
    use kin_model::FilePathId;

    fn census(source: &str) -> DeclarationCensus {
        let adapter = SwiftAdapter;
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
                &FilePathId::new("Sources/Store/Store.swift"),
            )
            .unwrap();
        take_census(
            adapter
                .declaration_census(&tree, source.as_bytes())
                .expect("the Swift adapter takes a census"),
            &output.entities,
        )
    }

    fn listed(census: &DeclarationCensus) -> Vec<(&str, &str)> {
        census
            .unadmitted
            .iter()
            .map(|declaration| (declaration.name.as_str(), declaration.reason.as_str()))
            .collect()
    }

    #[test]
    fn admitted_declarations_pair_and_the_rest_are_listed_with_a_reason() {
        let source = r#"import Foundation

let defaultShards = 4

protocol Persisting {
    associatedtype Record
    var name: String { get }
    func save()
}

final class Store: Persisting {
    typealias Record = Row
    let name: String
    private var cache = Cache()

    init(name: String) {
        self.name = name
    }

    func save() {
        func flushPending() {}
        let onDone = { report() }
        flushPending()
        onDone()
    }

    subscript(key: String) -> Row? { cache.row(key) }

    deinit {}
}

enum Mode {
    case fast, safe
}

extension Store {
    func archive() {}
}
"#;
        let census = census(source);
        let listed = listed(&census);
        for admitted in [
            "defaultShards",
            "Persisting",
            "name",
            "save",
            "Store",
            "cache",
            "init",
            "deinit",
            "flushPending",
            "Mode",
            "fast",
            "safe",
            "archive",
        ] {
            assert!(
                !listed.iter().any(|(name, _)| *name == admitted),
                "{admitted} is admitted: {listed:?}"
            );
        }
        // The class's type alias is admitted; the protocol's associated type
        // is not, and is the one `Record` listed.
        assert_eq!(
            listed.iter().filter(|(name, _)| *name == "Record").count(),
            1,
            "{listed:?}"
        );
        for expected in [
            ("Record", REASON_CLASS_MEMBER),
            ("subscript", REASON_CLASS_MEMBER),
            ("onDone", REASON_NESTED_IN_CALLABLE),
        ] {
            assert!(listed.contains(&expected), "{expected:?} in {listed:?}");
        }
        assert_eq!(census.seen, census.admitted + census.unadmitted.len());
    }
}
