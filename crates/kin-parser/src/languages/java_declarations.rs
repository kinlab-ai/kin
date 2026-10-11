// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The Java declaration census.
//!
//! Counted, at any depth: class, record, interface, enum and annotation type
//! declarations; methods, constructors and annotation elements; fields,
//! interface constants and record components; enum constants; and, inside a
//! callable, an anonymous class or a lambda bound to a local variable, which
//! are named by that variable. Not counted: packages and imports, which the
//! import evidence covers; parameters and plain local variables, which are
//! locals; and an anonymous class in any other position, which declares no
//! name (its methods are counted).

use tree_sitter::{Node, Tree};

use crate::adapter::site_from_node;
use crate::declaration_census::{plain_name, DeclarationKind, DeclarationScope, SeenDeclaration};

#[derive(Clone, Copy)]
struct Place<'a> {
    in_callable: bool,
    owner: Option<&'a str>,
}

pub(crate) fn java_declaration_census(tree: &Tree, source: &[u8]) -> Vec<SeenDeclaration> {
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
            "class_declaration"
            | "record_declaration"
            | "interface_declaration"
            | "annotation_type_declaration"
            | "enum_declaration" => {
                let name = name_of(&child, source);
                let kind = match child.kind() {
                    "interface_declaration" | "annotation_type_declaration" => {
                        DeclarationKind::Interface
                    }
                    "enum_declaration" => DeclarationKind::Enum,
                    _ => DeclarationKind::Class,
                };
                if let Some(name) = &name {
                    push(out, scope, place, name.clone(), kind, &child);
                }
                let inner = Place {
                    owner: name.as_deref().or(place.owner),
                    ..place
                };
                if child.kind() == "record_declaration" {
                    if let Some(parameters) = child.child_by_field_name("parameters") {
                        let mut components = parameters.walk();
                        for component in parameters.named_children(&mut components) {
                            if let Some(name) = name_of(&component, source) {
                                push(
                                    out,
                                    DeclarationScope::Class,
                                    inner,
                                    name,
                                    DeclarationKind::Attribute,
                                    &component,
                                );
                            }
                        }
                    }
                }
                if let Some(body) = child.child_by_field_name("body") {
                    walk(&body, source, DeclarationScope::Class, inner, out);
                }
            }
            "enum_body_declarations" => walk(&child, source, DeclarationScope::Class, place, out),
            "enum_constant" => {
                let name = name_of(&child, source);
                if let Some(name) = &name {
                    push(
                        out,
                        DeclarationScope::Class,
                        place,
                        name.clone(),
                        DeclarationKind::Attribute,
                        &child,
                    );
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
            "method_declaration"
            | "constructor_declaration"
            | "compact_constructor_declaration"
            | "annotation_type_element_declaration" => {
                let name = name_of(&child, source).or_else(|| {
                    // A compact canonical constructor carries its record's
                    // name without a name field.
                    (child.kind() == "compact_constructor_declaration")
                        .then(|| place.owner.map(str::to_string))
                        .flatten()
                });
                if let Some(name) = &name {
                    push(
                        out,
                        DeclarationScope::Class,
                        place,
                        name.clone(),
                        DeclarationKind::Method,
                        &child,
                    );
                }
                walk(
                    &child,
                    source,
                    DeclarationScope::Class,
                    Place {
                        in_callable: true,
                        owner: name.as_deref().or(place.owner),
                    },
                    out,
                );
            }
            "field_declaration" | "constant_declaration" => {
                let mut declarators = child.walk();
                for declarator in child.named_children(&mut declarators) {
                    if declarator.kind() == "variable_declarator" {
                        if let Some(name) = name_of(&declarator, source) {
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
                // An initializer can hold an anonymous class or a lambda.
                walk(&child, source, scope, place, out);
            }
            "local_variable_declaration" if place.in_callable => {
                let mut declarators = child.walk();
                for declarator in child.named_children(&mut declarators) {
                    if declarator.kind() != "variable_declarator" {
                        continue;
                    }
                    let kind = match declarator.child_by_field_name("value") {
                        Some(value) if value.kind() == "lambda_expression" => {
                            Some(DeclarationKind::Function)
                        }
                        Some(value)
                            if value.kind() == "object_creation_expression"
                                && has_class_body(&value) =>
                        {
                            Some(DeclarationKind::Class)
                        }
                        _ => None,
                    };
                    if let (Some(kind), Some(name)) = (kind, name_of(&declarator, source)) {
                        push(out, scope, place, name, kind, &declarator);
                    }
                }
                walk(&child, source, scope, place, out);
            }
            "lambda_expression" | "static_initializer" => walk(
                &child,
                source,
                scope,
                Place {
                    in_callable: true,
                    ..place
                },
                out,
            ),
            // An instance initializer runs as part of every constructor.
            "block" if matches!(node.kind(), "class_body" | "enum_body_declarations") => walk(
                &child,
                source,
                scope,
                Place {
                    in_callable: true,
                    ..place
                },
                out,
            ),
            "import_declaration" | "package_declaration" => {}
            _ => walk(&child, source, scope, place, out),
        }
    }
}

fn has_class_body(creation: &Node) -> bool {
    let mut cursor = creation.walk();
    let found = creation
        .children(&mut cursor)
        .any(|child| child.kind() == "class_body");
    found
}

#[cfg(test)]
mod tests {
    use crate::adapter::LanguageAdapter;
    use crate::declaration_census::{take_census, REASON_CLASS_MEMBER, REASON_NESTED_IN_CALLABLE};
    use crate::languages::java::JavaAdapter;
    use kin_model::FilePathId;

    fn census(source: &str) -> crate::declaration_census::DeclarationCensus {
        let adapter = JavaAdapter;
        let tree = adapter.parse(source.as_bytes()).unwrap();
        let output = adapter
            .extract(
                &tree,
                source.as_bytes(),
                &FilePathId::new("app/Service.java"),
            )
            .unwrap();
        take_census(
            adapter
                .declaration_census(&tree, source.as_bytes())
                .unwrap(),
            &output.entities,
        )
    }

    #[test]
    fn admitted_declarations_pair_and_fields_and_callbacks_are_listed() {
        let source = r#"package app;

public class Service implements Store {
    static final int LIMIT = 3;
    private Cache cache;

    public Service(Cache cache) { this.cache = cache; }

    public void load() {
        Runnable later = () -> cache.get();
        executor.submit(new Runnable() { public void run() { load(); } });
        class Local { void go() {} }
    }

    interface Loader { Store load(String key); }
    enum Mode { FAST, SLOW; void apply() {} }
    record Pair(Store left, int right) {}
}
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
                ("left", "attribute", REASON_CLASS_MEMBER),
                ("right", "attribute", REASON_CLASS_MEMBER),
            ],
            "{census:?}"
        );
        // Service, LIMIT, the field cache, the constructor, load, Local and
        // Local.go, Loader and Loader.load, Mode with FAST, SLOW and apply, and
        // Pair.
        assert_eq!(census.admitted, 14, "{census:?}");
        assert_eq!(census.seen, 18);
    }
}
