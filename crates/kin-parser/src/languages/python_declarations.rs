// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The Python declaration census.
//!
//! Counted: every `function_definition` and `class_definition` at any depth,
//! and every assignment to a plain name at module scope (including inside a
//! top-level `if`, `try` or `with` block) or directly in a class body, with or
//! without an annotation. Not counted: imports, which the import evidence
//! covers, assignments inside a callable, which are locals, and tuple or
//! attribute targets, which declare no new name.

use tree_sitter::{Node, Tree};

use kin_model::RelationKind;

use crate::adapter::site_from_node;
use crate::declaration_census::{plain_name, DeclarationKind, DeclarationScope, SeenDeclaration};
use crate::extract::{ExtractedEntity, ExtractedRelation, RelationSite};
use crate::file_evidence::{
    call_extraction_census_record, call_extraction_gap_record, GAP_CALLEE_NOT_NAMEABLE,
    GAP_OUTSIDE_CALLABLE,
};

#[derive(Clone, Copy)]
struct Place<'a> {
    scope: DeclarationScope,
    in_callable: bool,
    owner: Option<&'a str>,
}

pub(crate) fn python_declaration_census(tree: &Tree, source: &[u8]) -> Vec<SeenDeclaration> {
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

fn walk(node: &Node, source: &[u8], place: Place<'_>, out: &mut Vec<SeenDeclaration>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "function_definition" => {
                let name = child
                    .child_by_field_name("name")
                    .and_then(|name| plain_name(&name, source));
                let kind = if place.scope == DeclarationScope::Class && !place.in_callable {
                    DeclarationKind::Method
                } else {
                    DeclarationKind::Function
                };
                if let Some(name) = &name {
                    push(out, place, name.clone(), kind, &child);
                }
                let owner = name.as_deref().or(place.owner);
                // Parameters and the body both run in the callable's scope.
                walk(
                    &child,
                    source,
                    Place {
                        scope: place.scope,
                        in_callable: true,
                        owner,
                    },
                    out,
                );
            }
            "class_definition" => {
                let name = child
                    .child_by_field_name("name")
                    .and_then(|name| plain_name(&name, source));
                if let Some(name) = &name {
                    push(out, place, name.clone(), DeclarationKind::Class, &child);
                }
                if let Some(body) = child.child_by_field_name("body") {
                    walk(
                        &body,
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
            "import_statement" | "import_from_statement" | "future_import_statement" => {}
            "lambda" => walk(
                &child,
                source,
                Place {
                    in_callable: true,
                    ..place
                },
                out,
            ),
            "expression_statement" if !place.in_callable => {
                let mut statement = child.walk();
                for expression in child.children(&mut statement) {
                    if expression.kind() == "assignment" {
                        assignment_targets(&expression, source, place, out);
                    }
                }
            }
            "if_statement" | "try_statement" | "with_statement" | "for_statement"
            | "while_statement" | "match_statement"
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
            _ => walk(&child, source, place, out),
        }
    }
}

/// `a = b = value` binds `a` and `b`; each `assignment` node's `left` is one
/// target, and a chained one nests its next target in `right`.
fn assignment_targets(
    node: &Node,
    source: &[u8],
    place: Place<'_>,
    out: &mut Vec<SeenDeclaration>,
) {
    let Some(left) = node.child_by_field_name("left") else {
        return;
    };
    if left.kind() == "identifier" {
        if let Some(name) = plain_name(&left, source) {
            let kind = if place.scope == DeclarationScope::Class {
                DeclarationKind::Attribute
            } else {
                DeclarationKind::Variable
            };
            push(out, place, name, kind, node);
        }
    }
    if let Some(right) = node.child_by_field_name("right") {
        if right.kind() == "assignment" {
            assignment_targets(&right, source, place, out);
        }
    }
}

/// Add the call node inside each carried decorator to `carried`.
///
/// A decorator's `Calls` relation cites the `@decorator` node, while the call
/// census and a call walk both meet the `call` node inside it, so a decorator
/// that is carried carries that call too.
pub(crate) fn carry_decorator_call_sites(
    node: &Node,
    carried: &mut std::collections::HashSet<(usize, usize)>,
) {
    if node.kind() == "decorator" {
        if let Some(call) = node.named_child(0).filter(|child| child.kind() == "call") {
            if carried.contains(&(node.start_byte(), node.end_byte())) {
                carried.insert((call.start_byte(), call.end_byte()));
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        carry_decorator_call_sites(&child, carried);
    }
}

/// Record every census call site that no named `Calls` relation carries.
///
/// A site a call walk visited but could not name is a
/// [`GAP_CALLEE_NOT_NAMEABLE`] gap. Module and class body code is walked too
/// and charged to the entity holding it ([`crate::call_owner`]), so a site no
/// walk visited is one no admitted entity holds, which only a file without a
/// module entity has, and is a [`GAP_OUTSIDE_CALLABLE`] gap. A site whose callee
/// was named but whose receiver the parser could not type is not a gap: its
/// relation exists, and the language server settles its target.
pub(crate) fn push_call_extraction_gaps(
    tree: &Tree,
    census_total: u64,
    census_sites: &[RelationSite],
    walked: &std::collections::HashSet<(usize, usize)>,
    entities: &[ExtractedEntity],
    relations: &mut Vec<ExtractedRelation>,
) {
    let mut carried: std::collections::HashSet<(usize, usize)> = relations
        .iter()
        .filter(|relation| relation.kind == RelationKind::Calls)
        .filter_map(|relation| relation.site.as_ref())
        .map(|site| (site.start_byte, site.end_byte))
        .collect();
    carry_decorator_call_sites(&tree.root_node(), &mut carried);
    relations.push(call_extraction_census_record(census_total));
    for site in census_sites {
        let key = (site.start_byte, site.end_byte);
        if carried.contains(&key) {
            continue;
        }
        let reason = if walked.contains(&key) {
            GAP_CALLEE_NOT_NAMEABLE
        } else {
            GAP_OUTSIDE_CALLABLE
        };
        let owner = entities
            .iter()
            .filter(|entity| {
                entity.span.start_byte <= site.start_byte && site.end_byte <= entity.span.end_byte
            })
            .min_by_key(|entity| entity.span.end_byte - entity.span.start_byte)
            .map(|entity| entity.name.as_str());
        relations.push(call_extraction_gap_record(reason, owner, site.clone()));
    }
}

#[cfg(test)]
mod tests {
    use crate::adapter::LanguageAdapter;
    use crate::declaration_census::{take_census, REASON_NESTED_IN_CALLABLE};
    use crate::languages::python::PythonAdapter;
    use kin_model::FilePathId;

    fn census(source: &str) -> crate::declaration_census::DeclarationCensus {
        let adapter = PythonAdapter;
        let tree = adapter.parse(source.as_bytes()).unwrap();
        let output = adapter
            .extract(&tree, source.as_bytes(), &FilePathId::new("pkg/mod.py"))
            .unwrap();
        take_census(
            adapter
                .declaration_census(&tree, source.as_bytes())
                .unwrap(),
            &output.entities,
        )
    }

    /// Definitions nested in a function body are entities of their own, so
    /// they pair like any other. Both declarations in a conditional binding
    /// collision remain listed rather than pairing an arbitrary first branch.
    #[test]
    fn admitted_declarations_pair_including_nested_ones_and_the_rest_are_listed() {
        let source = "\
MAX_RETRIES = 3

class Store:
    kind = \"memory\"

    def pop(self):
        def restore():
            return 1
        if self:
            def restore():
                return 2
        return restore()

@decorate
def view():
    class Local:
        pass
    return Local
";
        let census = census(source);
        let unadmitted: Vec<_> = census
            .unadmitted
            .iter()
            .map(|d| {
                (
                    d.name.as_str(),
                    d.kind.label(),
                    d.reason.as_str(),
                    d.site.start_line,
                )
            })
            .collect();
        assert_eq!(
            unadmitted,
            vec![
                ("restore", "function", REASON_NESTED_IN_CALLABLE, 6),
                ("restore", "function", REASON_NESTED_IN_CALLABLE, 9),
            ],
            "{census:?}"
        );
        // MAX_RETRIES, Store, Store.kind, Store.pop, view and its nested Local pair.
        assert_eq!(census.admitted, 6, "{census:?}");
        assert_eq!(census.seen, 8);
    }
}
