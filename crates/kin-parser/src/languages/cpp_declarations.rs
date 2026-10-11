// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The C and C++ declaration census. One walk serves both grammars.
//!
//! Counted: every function definition at any depth (a member function
//! defined out of line is counted by its own name, `save` for
//! `Store::save`), every function prototype at namespace scope and in a
//! class body, every named class, struct, union and enum with a body, a
//! named class declared without one where it stands alone (`class Store;`),
//! every enumerator and every field (as class members), every `typedef` and
//! `using` alias, every macro, every variable at
//! namespace scope, and every lambda bound to a local (a nested definition).
//! Not counted: `#include`, `using` declarations and directives, which the
//! import evidence covers, a function's other locals and its parameters,
//! operators and destructors, whose names are not plain identifiers, and
//! namespaces, which the adapter admits as module entities that name search
//! reaches through their members.

use tree_sitter::{Node, Tree};

use crate::adapter::site_from_node;
use crate::declaration_census::{plain_name, DeclarationKind, DeclarationScope, SeenDeclaration};

#[derive(Clone, Copy)]
struct Place<'a> {
    scope: DeclarationScope,
    in_callable: bool,
    owner: Option<&'a str>,
}

/// Take the census of a C or C++ tree. `cpp` says it came from the C++
/// adapter, which parses its own preprocessing of the source.
pub(crate) fn census(tree: &Tree, source: &[u8], cpp: bool) -> Vec<SeenDeclaration> {
    let preprocessed;
    let source = if cpp {
        preprocessed = crate::languages::cpp_lang::preprocess_cpp_source(source);
        preprocessed.as_slice()
    } else {
        source
    };
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

/// The plain name a declarator node spells: the leaf of a qualified name.
fn leaf_name(name: Node<'_>, source: &[u8]) -> Option<String> {
    let mut node = name;
    while node.kind() == "qualified_identifier" {
        node = node.child_by_field_name("name")?;
    }
    if node.kind() == "template_function" || node.kind() == "template_type" {
        node = node.child_by_field_name("name")?;
    }
    plain_name(&node, source)
}

fn walk(node: Node<'_>, source: &[u8], place: Place<'_>, out: &mut Vec<SeenDeclaration>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit(child, source, place, out);
    }
}

fn visit(child: Node<'_>, source: &[u8], place: Place<'_>, out: &mut Vec<SeenDeclaration>) {
    match child.kind() {
        "function_definition" => {
            let name = child
                .child_by_field_name("declarator")
                .and_then(super::cpp_escape::declarator_name_node);
            let qualified = name.is_some_and(|name| name.kind() == "qualified_identifier");
            let leaf = name.and_then(|name| leaf_name(name, source));
            // A block the grammar reads as a definition with no return type,
            // or with no function declarator, inside a body is a macro
            // opening a block, not a definition.
            let macro_block = place.in_callable
                && (child.child_by_field_name("type").is_none()
                    || !child
                        .child_by_field_name("declarator")
                        .is_some_and(is_function_declarator));
            if let Some(leaf) = &leaf {
                if !macro_block {
                    let kind = if (place.scope == DeclarationScope::Class || qualified)
                        && !place.in_callable
                    {
                        DeclarationKind::Method
                    } else {
                        DeclarationKind::Function
                    };
                    push(out, place, leaf.clone(), kind, child);
                }
            }
            if let Some(body) = child.child_by_field_name("body") {
                walk(
                    body,
                    source,
                    Place {
                        scope: place.scope,
                        in_callable: true,
                        owner: leaf.as_deref().or(place.owner),
                    },
                    out,
                );
            }
        }
        "declaration" | "field_declaration" => declaration(child, source, place, out),
        "class_specifier" | "struct_specifier" | "union_specifier" => {
            specifier(child, source, place, out, true);
        }
        "enum_specifier" => specifier(child, source, place, out, true),
        "type_definition" => {
            if let Some(ty) = child.child_by_field_name("type") {
                specifier(ty, source, place, out, false);
            }
            let mut cursor = child.walk();
            for declarator in child.children_by_field_name("declarator", &mut cursor) {
                if let Some(name) = typedef_name(declarator, source) {
                    push(out, place, name, DeclarationKind::TypeAlias, child);
                }
            }
        }
        "alias_declaration" => {
            if let Some(name) = child
                .child_by_field_name("name")
                .and_then(|name| plain_name(&name, source))
            {
                push(out, place, name, DeclarationKind::TypeAlias, child);
            }
        }
        // A namespace is admitted as a module entity, which the comparison
        // never pairs, so it is walked through rather than counted.
        "namespace_definition" => {
            if let Some(body) = child.child_by_field_name("body") {
                walk(body, source, place, out);
            }
        }
        "preproc_def" | "preproc_function_def" => {
            if let Some(name) = child
                .child_by_field_name("name")
                .and_then(|name| plain_name(&name, source))
            {
                push(out, place, name, DeclarationKind::Variable, child);
            }
        }
        "preproc_include" | "using_declaration" | "namespace_alias_definition" | "comment" => {}
        _ => walk(child, source, place, out),
    }
}

/// A class, struct, union or enum specifier. `standalone` says it stands as
/// its own statement, where a bodyless one is a forward declaration; inside
/// a declaration's type a bodyless one only names a type declared elsewhere.
fn specifier(
    node: Node<'_>,
    source: &[u8],
    place: Place<'_>,
    out: &mut Vec<SeenDeclaration>,
    standalone: bool,
) {
    if !matches!(
        node.kind(),
        "class_specifier" | "struct_specifier" | "union_specifier" | "enum_specifier"
    ) {
        return;
    }
    let body = node.child_by_field_name("body");
    let name = node
        .child_by_field_name("name")
        .and_then(|name| leaf_name(name, source));
    let kind = if node.kind() == "enum_specifier" {
        DeclarationKind::Enum
    } else {
        DeclarationKind::Class
    };
    if let Some(name) = &name {
        if body.is_some() || (standalone && node.kind() != "enum_specifier") {
            push(out, place, name.clone(), kind, node);
        }
    }
    let Some(body) = body else {
        return;
    };
    let inner = Place {
        scope: DeclarationScope::Class,
        in_callable: place.in_callable,
        owner: name.as_deref().or(place.owner),
    };
    if node.kind() == "enum_specifier" {
        let mut cursor = body.walk();
        for enumerator in body.named_children(&mut cursor) {
            if enumerator.kind() != "enumerator" {
                continue;
            }
            if let Some(name) = enumerator
                .child_by_field_name("name")
                .and_then(|name| plain_name(&name, source))
            {
                push(out, inner, name, DeclarationKind::Attribute, enumerator);
            }
        }
        return;
    }
    walk(body, source, inner, out);
}

/// A declaration: prototypes, variables, and the types it declares.
fn declaration(node: Node<'_>, source: &[u8], place: Place<'_>, out: &mut Vec<SeenDeclaration>) {
    if let Some(ty) = node.child_by_field_name("type") {
        specifier(ty, source, place, out, false);
    }
    let mut cursor = node.walk();
    let declarators: Vec<Node<'_>> = node
        .children_by_field_name("declarator", &mut cursor)
        .collect();
    for declarator in declarators {
        let function = is_function_declarator(declarator);
        let Some(name) = super::cpp_escape::declarator_name_node(declarator) else {
            continue;
        };
        let Some(leaf) = leaf_name(name, source) else {
            continue;
        };
        if place.in_callable {
            // A local is not a declaration a name search is asked to find,
            // but a lambda bound to one is a nested definition.
            let lambda = declarator.kind() == "init_declarator"
                && declarator
                    .child_by_field_name("value")
                    .is_some_and(|value| value.kind() == "lambda_expression");
            if lambda {
                push(out, place, leaf, DeclarationKind::Function, node);
            } else if function {
                // A block-scope prototype names a function declared at
                // namespace scope; it declares nothing new here.
            }
            continue;
        }
        let kind = match (function, place.scope) {
            (true, DeclarationScope::Class) => DeclarationKind::Method,
            (true, _) => DeclarationKind::Function,
            (false, DeclarationScope::Class) => DeclarationKind::Attribute,
            (false, _) => DeclarationKind::Variable,
        };
        push(out, place, leaf, kind, node);
    }
}

fn is_function_declarator(declarator: Node<'_>) -> bool {
    let mut current = declarator;
    for _ in 0..16 {
        match current.kind() {
            "function_declarator" => return true,
            "pointer_declarator" | "reference_declarator" | "attributed_declarator" => {
                match current
                    .child_by_field_name("declarator")
                    .or_else(|| current.named_child(0))
                {
                    Some(next) => current = next,
                    None => return false,
                }
            }
            _ => return false,
        }
    }
    false
}

/// The name a typedef declarator introduces, through pointers, arrays,
/// functions and parentheses.
fn typedef_name(declarator: Node<'_>, source: &[u8]) -> Option<String> {
    let mut current = declarator;
    for _ in 0..16 {
        match current.kind() {
            "type_identifier" | "identifier" | "primitive_type" => {
                return plain_name(&current, source)
            }
            _ => {
                current = current
                    .child_by_field_name("declarator")
                    .or_else(|| current.named_child(0))?;
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use crate::declaration_census::{
        take_census, DeclarationCensus, REASON_CLASS_MEMBER, REASON_MODULE_BINDING,
        REASON_NESTED_IN_CALLABLE,
    };
    use crate::{CppAdapter, LanguageAdapter};
    use kin_model::FilePathId;

    fn census(source: &str) -> DeclarationCensus {
        let adapter = CppAdapter;
        let tree = adapter.parse(source.as_bytes()).unwrap();
        let output = adapter
            .extract(&tree, source.as_bytes(), &FilePathId::new("src/store.cpp"))
            .unwrap();
        take_census(
            adapter
                .declaration_census(&tree, source.as_bytes())
                .expect("the C++ adapter takes a census"),
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
        let source = r#"#include "store.hpp"
#define STORE_VERSION 3

namespace app {

int open_count = 0;

class Store : public Base {
 public:
  Store();
  void save(const Record& record);
  Record load(int id) const { return cache_.get(id); }
  enum class Mode { kFast, kSafe };

 private:
  Cache cache_;
};

using StorePtr = std::shared_ptr<Store>;

void Store::save(const Record& record) {
  auto on_done = [this](int code) { report(code); };
  struct Probe { void hit() {} };
  on_done(0);
}

}  // namespace app
"#;
        let census = census(source);
        let listed = listed(&census);
        for admitted in ["STORE_VERSION", "Store", "load", "StorePtr", "cache_"] {
            assert!(
                !listed.iter().any(|(name, _)| *name == admitted),
                "{admitted} is admitted: {listed:?}"
            );
        }
        for expected in [
            ("open_count", REASON_MODULE_BINDING),
            ("Mode", REASON_CLASS_MEMBER),
            ("kFast", REASON_CLASS_MEMBER),
            ("kSafe", REASON_CLASS_MEMBER),
        ] {
            assert!(listed.contains(&expected), "{expected:?} in {listed:?}");
        }
        // The in-class prototype of a member with a return type is no entity;
        // its out-of-line definition is, and pairs.
        assert_eq!(
            listed.iter().filter(|(name, _)| *name == "save").count(),
            1,
            "{listed:?}"
        );
        // Nested definitions the adapter admits pair; nothing nested is
        // listed as nested_in_callable here.
        assert!(
            !listed
                .iter()
                .any(|(_, reason)| *reason == REASON_NESTED_IN_CALLABLE),
            "{listed:?}"
        );
        assert_eq!(census.seen, census.admitted + census.unadmitted.len());
    }

    #[test]
    fn a_nested_definition_the_adapter_declines_is_listed_as_nested() {
        let source = "void run() {\n  std::function<void()> cb = [] { tick(); };\n  cb();\n  enum Local { kA };\n}\n";
        let census = census(source);
        let listed = listed(&census);
        assert!(
            listed.contains(&("cb", REASON_NESTED_IN_CALLABLE)),
            "{listed:?}"
        );
        assert!(
            listed.contains(&("Local", REASON_NESTED_IN_CALLABLE)),
            "{listed:?}"
        );
    }
}
