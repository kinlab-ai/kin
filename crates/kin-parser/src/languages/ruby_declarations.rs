// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The Ruby declaration census.
//!
//! Counted, at any depth: classes; methods and singleton methods (`def
//! self.name`), a method defined inside another method counting as local to
//! it; and constants assigned in a module or class body. Not counted:
//! modules, which Kin admits as module entities the census does not pair;
//! `require` calls, which the import evidence covers; and local variables.

use tree_sitter::{Node, Tree};

use crate::adapter::site_from_node;
use crate::declaration_census::{plain_name, DeclarationKind, DeclarationScope, SeenDeclaration};

#[derive(Clone, Copy)]
struct Place<'a> {
    in_callable: bool,
    owner: Option<&'a str>,
}

pub(crate) fn ruby_declaration_census(tree: &Tree, source: &[u8]) -> Vec<SeenDeclaration> {
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

/// The last segment of a declaration's name: `Ledger` of `Shop::Ledger`.
fn leaf_name(node: &Node, source: &[u8]) -> Option<String> {
    let name = node.child_by_field_name("name")?;
    let text = name.utf8_text(source).ok()?;
    let leaf = text.rsplit("::").next().unwrap_or(text).trim();
    (!leaf.is_empty()).then(|| leaf.to_string())
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
            "module" => {
                let name = leaf_name(&child, source);
                if let Some(body) = child.child_by_field_name("body") {
                    walk(
                        &body,
                        source,
                        DeclarationScope::Module,
                        Place {
                            owner: name.as_deref().or(place.owner),
                            ..place
                        },
                        out,
                    );
                }
            }
            "class" => {
                let name = leaf_name(&child, source);
                if let Some(name) = &name {
                    push(
                        out,
                        scope,
                        place,
                        name.clone(),
                        DeclarationKind::Class,
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
            "method" | "singleton_method" => {
                let name = child
                    .child_by_field_name("name")
                    .and_then(|name| plain_name(&name, source));
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
            "assignment" if !place.in_callable => {
                let constant = child
                    .child_by_field_name("left")
                    .filter(|left| left.kind() == "constant")
                    .and_then(|left| plain_name(&left, source));
                if let Some(name) = constant {
                    let kind = if scope == DeclarationScope::Class {
                        DeclarationKind::Attribute
                    } else {
                        DeclarationKind::Variable
                    };
                    push(out, scope, place, name, kind, &child);
                }
                walk(&child, source, scope, place, out);
            }
            _ => walk(&child, source, scope, place, out),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::declaration_census::take_census;
    use crate::{LanguageAdapter, RubyAdapter};
    use kin_model::FilePathId;

    #[test]
    fn admitted_declarations_pair_and_nested_definitions_are_listed() {
        let source = b"module Shop
  LIMIT = 3

  class Forms
    SQUARE = ->(value) { value * value }

    def self.build
      new
    end

    def each_value
      def helper
        1
      end
      yield helper
    end

    class Inner
      def ping
        \"pong\"
      end
    end
  end
end
";
        let adapter = RubyAdapter;
        let tree = adapter.parse(source).unwrap();
        let output = adapter
            .extract(&tree, source, &FilePathId::new("lib/shop/forms.rb"))
            .unwrap();
        let seen = adapter.declaration_census(&tree, source).expect("a census");
        let census = take_census(seen, &output.entities);
        let unadmitted: Vec<_> = census
            .unadmitted
            .iter()
            .map(|declaration| declaration.name.as_str())
            .collect();
        for admitted in [
            "Forms",
            "build",
            "each_value",
            "Inner",
            "ping",
            "SQUARE",
            "LIMIT",
        ] {
            assert!(
                !unadmitted.contains(&admitted),
                "{admitted} is admitted: {unadmitted:?}"
            );
        }
        assert_eq!(census.seen, census.admitted + census.unadmitted.len());
    }
}
