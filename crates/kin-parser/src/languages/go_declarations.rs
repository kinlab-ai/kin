// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The Go declaration census.
//!
//! Counted: every function and method, every type a `type` declaration names
//! at package level or inside a function (with the named fields of a struct
//! and the methods an interface declares), every name a package-level `var`
//! or `const` spec declares, and every name a function literal is bound to
//! inside a function (`helper := func() {...}`, `var helper = func() {...}`,
//! or an assignment of one to a local declared without it). Not counted:
//! imports, which the import evidence covers, embedded fields, which declare
//! no name of their own, the blank identifier, which declares nothing, and
//! any other local, which a name search is not asked to find.

use tree_sitter::{Node, Tree};

use super::go_escape::{list_identifiers, specs_of, FunctionScopes};
use crate::adapter::site_from_node;
use crate::declaration_census::{plain_name, DeclarationKind, DeclarationScope, SeenDeclaration};

pub(crate) fn go_declaration_census(tree: &Tree, source: &[u8]) -> Vec<SeenDeclaration> {
    let mut out = Vec::new();
    let root = tree.root_node();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        match child.kind() {
            "function_declaration" | "method_declaration" => {
                let name = child
                    .child_by_field_name("name")
                    .and_then(|name| plain_name(&name, source));
                let (kind, scope, owner) = if child.kind() == "method_declaration" {
                    (
                        DeclarationKind::Method,
                        DeclarationScope::Class,
                        super::go::method_receiver_type(&child, source),
                    )
                } else {
                    (DeclarationKind::Function, DeclarationScope::Module, None)
                };
                if let Some(name) = &name {
                    push(&mut out, name.clone(), kind, scope, false, owner, &child);
                }
                if let Some(body) = child.child_by_field_name("body") {
                    let scopes = FunctionScopes::of(child, source);
                    walk_body(body, source, name.as_deref(), &scopes, &mut out);
                }
            }
            "type_declaration" => type_declaration(child, source, false, None, &mut out),
            "var_declaration" | "const_declaration" => {
                for spec in specs_of(child) {
                    let mut names = spec.walk();
                    let declared: Vec<Node<'_>> =
                        spec.children_by_field_name("name", &mut names).collect();
                    for name in declared {
                        if let Some(text) = named(&name, source) {
                            push(
                                &mut out,
                                text,
                                DeclarationKind::Variable,
                                DeclarationScope::Module,
                                false,
                                None,
                                &spec,
                            );
                        }
                    }
                    if let Some(value) = spec.child_by_field_name("value") {
                        let owner = spec_owner(&spec, source);
                        let scopes = FunctionScopes::of(value, source);
                        walk_body(value, source, owner.as_deref(), &scopes, &mut out);
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// The name a declaration node spells, unless it is the blank identifier.
fn named(node: &Node<'_>, source: &[u8]) -> Option<String> {
    plain_name(node, source).filter(|name| name != "_")
}

/// The first name a package-level spec declares, which owns what its value
/// declares.
fn spec_owner(spec: &Node<'_>, source: &[u8]) -> Option<String> {
    let mut cursor = spec.walk();
    let first = spec
        .children_by_field_name("name", &mut cursor)
        .find_map(|name| named(&name, source));
    first
}

fn push(
    out: &mut Vec<SeenDeclaration>,
    name: String,
    kind: DeclarationKind,
    scope: DeclarationScope,
    in_callable: bool,
    owner: Option<String>,
    node: &Node<'_>,
) {
    out.push(SeenDeclaration {
        name,
        kind,
        scope,
        in_callable,
        owner,
        site: site_from_node(node),
    });
}

/// Every type spec of a `type` declaration, with the named fields of a
/// struct and the method specs of an interface.
fn type_declaration(
    declaration: Node<'_>,
    source: &[u8],
    in_callable: bool,
    owner: Option<&str>,
    out: &mut Vec<SeenDeclaration>,
) {
    let mut cursor = declaration.walk();
    for spec in declaration.named_children(&mut cursor) {
        if !matches!(spec.kind(), "type_spec" | "type_alias") {
            continue;
        }
        let Some(name) = spec
            .child_by_field_name("name")
            .and_then(|name| named(&name, source))
        else {
            continue;
        };
        let body = spec.child_by_field_name("type");
        let kind = match (spec.kind(), body.map(|body| body.kind())) {
            ("type_spec", Some("struct_type")) => DeclarationKind::Class,
            ("type_spec", Some("interface_type")) => DeclarationKind::Interface,
            _ => DeclarationKind::TypeAlias,
        };
        push(
            out,
            name.clone(),
            kind,
            DeclarationScope::Module,
            in_callable,
            owner.map(str::to_string),
            &spec,
        );
        let Some(body) = body.filter(|_| spec.kind() == "type_spec") else {
            continue;
        };
        let mut stack = vec![body];
        while let Some(node) = stack.pop() {
            match node.kind() {
                "field_declaration" => {
                    let mut names = node.walk();
                    let fields: Vec<Node<'_>> =
                        node.children_by_field_name("name", &mut names).collect();
                    for field in fields {
                        if let Some(field_name) = named(&field, source) {
                            push(
                                out,
                                field_name,
                                DeclarationKind::Attribute,
                                DeclarationScope::Class,
                                in_callable,
                                Some(name.clone()),
                                &node,
                            );
                        }
                    }
                    // A field's own type (an anonymous struct) declares
                    // fields of no named type.
                }
                "method_elem" => {
                    if let Some(method) = node
                        .child_by_field_name("name")
                        .and_then(|method| named(&method, source))
                    {
                        push(
                            out,
                            method,
                            DeclarationKind::Method,
                            DeclarationScope::Class,
                            in_callable,
                            Some(name.clone()),
                            &node,
                        );
                    }
                }
                "struct_type" | "interface_type" | "field_declaration_list" => {
                    let mut children = node.walk();
                    let mut nested: Vec<Node<'_>> = node.named_children(&mut children).collect();
                    nested.reverse();
                    stack.extend(nested);
                }
                _ => {}
            }
        }
    }
}

/// The declarations a function body (or a package-level initializer holding
/// function literals) makes: local types, and function literals bound to a
/// name.
fn walk_body(
    node: Node<'_>,
    source: &[u8],
    owner: Option<&str>,
    scopes: &FunctionScopes<'_>,
    out: &mut Vec<SeenDeclaration>,
) {
    let mut stack = vec![node];
    let mut found: Vec<Node<'_>> = Vec::new();
    while let Some(current) = stack.pop() {
        match current.kind() {
            "type_declaration" => {
                type_declaration(current, source, true, owner, out);
                continue;
            }
            "short_var_declaration" | "var_spec" | "assignment_statement" => {
                found.push(current);
            }
            _ => {}
        }
        let mut cursor = current.walk();
        let mut children: Vec<Node<'_>> = current.named_children(&mut cursor).collect();
        children.reverse();
        stack.extend(children);
    }
    for binding in found {
        for (name, literal) in bound_literals(binding, source, scopes) {
            push(
                out,
                name,
                DeclarationKind::Function,
                DeclarationScope::Module,
                true,
                owner.map(str::to_string),
                &literal,
            );
        }
    }
}

/// The names a statement binds function literals to, with each literal:
/// `helper := func() {...}`, `var helper = func() {...}`, and `helper =
/// func() {...}` when `helper` is a local of the enclosing function.
pub(crate) fn bound_literals<'tree>(
    statement: Node<'tree>,
    source: &[u8],
    scopes: &FunctionScopes<'_>,
) -> Vec<(String, Node<'tree>)> {
    let (names, values) = match statement.kind() {
        "short_var_declaration" | "assignment_statement" => (
            list_identifiers_or_targets(statement.child_by_field_name("left")),
            statement.child_by_field_name("right"),
        ),
        "var_spec" => {
            let mut cursor = statement.walk();
            let names: Vec<Option<Node<'tree>>> = statement
                .children_by_field_name("name", &mut cursor)
                .map(Some)
                .collect();
            (names, statement.child_by_field_name("value"))
        }
        _ => return Vec::new(),
    };
    let Some(values) = values else {
        return Vec::new();
    };
    let mut cursor = values.walk();
    let values: Vec<Node<'tree>> = values.named_children(&mut cursor).collect();
    if values.len() != names.len() {
        return Vec::new();
    }
    let plain_assignment = statement.kind() != "assignment_statement"
        || statement
            .child_by_field_name("operator")
            .and_then(|operator| operator.utf8_text(source).ok())
            == Some("=");
    if !plain_assignment {
        return Vec::new();
    }
    let mut bound = Vec::new();
    for (name, value) in names.into_iter().zip(values) {
        let (Some(name), "func_literal") = (name, value.kind()) else {
            continue;
        };
        let Some(text) = named(&name, source) else {
            continue;
        };
        // An assignment binds a literal to a local only when the name it
        // writes is declared in the function; otherwise it writes a package
        // variable, which is declared, and counted, where it is.
        if statement.kind() == "assignment_statement" && !scopes.local_at(&text, name.start_byte())
        {
            continue;
        }
        bound.push((text, value));
    }
    bound
}

/// The targets of a left-hand expression list, `None` for any target that is
/// not a plain name.
fn list_identifiers_or_targets(list: Option<Node<'_>>) -> Vec<Option<Node<'_>>> {
    let Some(list) = list else {
        return Vec::new();
    };
    let identifiers = list_identifiers(Some(list));
    let mut cursor = list.walk();
    let targets: Vec<Option<Node<'_>>> = list
        .named_children(&mut cursor)
        .map(|target| {
            identifiers
                .iter()
                .find(|id| id.id() == target.id())
                .copied()
        })
        .collect();
    targets
}

#[cfg(test)]
mod tests {
    use crate::adapter::LanguageAdapter;
    use crate::declaration_census::{
        take_census, REASON_MODULE_BINDING, REASON_NESTED_IN_CALLABLE,
    };
    use crate::languages::GoAdapter;
    use kin_model::FilePathId;

    fn census(source: &str) -> crate::declaration_census::DeclarationCensus {
        let adapter = GoAdapter;
        let tree = adapter.parse(source.as_bytes()).unwrap();
        let output = adapter
            .extract(&tree, source.as_bytes(), &FilePathId::new("pkg/store.go"))
            .unwrap();
        take_census(
            adapter
                .declaration_census(&tree, source.as_bytes())
                .unwrap(),
            &output.entities,
        )
    }

    #[test]
    fn package_declarations_pair_and_nested_ones_are_listed() {
        let source = r#"package store

const (
	MaxRetries = 3
	minWait, maxWait = 1, 2
)

var _ Loader = (*Store)(nil)

type Loader interface {
	Load(key string) (string, error)
}

type Store struct {
	cache map[string]string
	Name  string
	*Base
}

func (s *Store) Load(key string) (string, error) {
	type result struct {
		value string
	}
	retry := func() error { return nil }
	var walk func(n int)
	walk = func(n int) { walk(n - 1) }
	_ = retry
	return key, nil
}

func New() *Store { return &Store{} }
"#;
        let census = census(source);
        let unadmitted: Vec<(&str, &str, &str)> = census
            .unadmitted
            .iter()
            .map(|d| (d.name.as_str(), d.kind.label(), d.reason.as_str()))
            .collect();
        assert_eq!(
            unadmitted,
            vec![
                ("maxWait", "variable", REASON_MODULE_BINDING),
                ("retry", "function", REASON_NESTED_IN_CALLABLE),
                ("walk", "function", REASON_NESTED_IN_CALLABLE),
            ],
            "{census:?}"
        );
        // MaxRetries, minWait, Loader, Loader.Load, Store, Store.cache,
        // Store.Name, Store.Load, result, result.value and New pair with
        // admitted entities.
        assert_eq!(census.admitted, 11, "{census:?}");
        assert_eq!(census.seen, 14);
    }
}
