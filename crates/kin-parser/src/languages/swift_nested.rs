// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Definitions nested in Swift function bodies.
//!
//! A function declared in another's body and a class, struct, enum or actor
//! declared there are real definitions: a test double conforms through a
//! local type, and a helper function is called by name. The adapter admits
//! both under the `enclosing.<locals>.name` rule of [`crate::nested`], with
//! the calls in their bodies moved onto them. A closure bound to a local is
//! recorded as a nested definition the adapter does not admit.

use kin_model::EntityKind;
use tree_sitter::Node;

use crate::value_escape::text_of;
use crate::value_origins::swift::{enclosing_block, local_declarations, type_declaration_name};

/// What one callable's body defines, found before its calls are extracted.
pub(crate) struct NestedPlan<'tree> {
    /// Functions declared in the body, outside any other nested function or
    /// local type, with the name a bare call reaches them by in scope.
    pub(crate) functions: Vec<NestedFunction<'tree>>,
    /// Types declared in the body, outside any nested function or closure.
    pub(crate) types: Vec<(String, Node<'tree>, (usize, usize))>,
    /// Definitions found and not admitted: name, kind, 0-based line.
    pub(crate) declined: Vec<(String, EntityKind, u32)>,
}

pub(crate) struct NestedFunction<'tree> {
    pub(crate) name: String,
    pub(crate) node: Node<'tree>,
    /// Where a bare call of the name reaches this function, when nothing
    /// else in the callable declares the name.
    pub(crate) scope: Option<(usize, usize)>,
}

/// Read what a function, initializer or accessor body defines.
pub(crate) fn plan<'tree>(callable: Node<'tree>, source: &[u8]) -> NestedPlan<'tree> {
    let mut plan = NestedPlan {
        functions: Vec::new(),
        types: Vec::new(),
        declined: Vec::new(),
    };
    let Some(body) = callable.child_by_field_name("body") else {
        return plan;
    };
    let locals = local_declarations(callable, source);
    let mut stack = vec![(body, false)];
    while let Some((node, in_closure)) = stack.pop() {
        match node.kind() {
            "function_declaration" => {
                if let Some(name) = node
                    .child_by_field_name("name")
                    .map(|name| text_of(name, source).to_string())
                    .filter(|name| !name.is_empty())
                {
                    let declared = locals.iter().filter(|(local, _, _)| *local == name).count();
                    plan.functions.push(NestedFunction {
                        scope: (declared == 1).then(|| enclosing_block(node)),
                        name,
                        node,
                    });
                }
                // Its own body is planned when it is admitted.
                continue;
            }
            "class_declaration" | "protocol_declaration" => {
                if let Some(name) = type_declaration_name(node, source) {
                    let extension = node
                        .child_by_field_name("declaration_kind")
                        .is_some_and(|kind| kind.kind() == "extension");
                    if in_closure || extension || node.kind() == "protocol_declaration" {
                        plan.declined.push((name, EntityKind::Class, row(node)));
                    } else {
                        plan.types.push((name, node, enclosing_block(node)));
                    }
                }
                continue;
            }
            "property_declaration" => {
                // `let onDone = { ... }`: a closure bound to a local.
                let value = node.child_by_field_name("value");
                if value.is_some_and(|value| value.kind() == "lambda_literal") {
                    if let Some(bound) = node
                        .child_by_field_name("name")
                        .and_then(|pattern| pattern.child_by_field_name("bound_identifier"))
                    {
                        plan.declined.push((
                            text_of(bound, source).to_string(),
                            EntityKind::Function,
                            row(node),
                        ));
                    }
                }
            }
            _ => {}
        }
        let inside = in_closure || node.kind() == "lambda_literal";
        let mut cursor = node.walk();
        let children: Vec<Node<'tree>> = node.children(&mut cursor).collect();
        stack.extend(children.into_iter().map(|child| (child, inside)));
    }
    plan.functions
        .sort_by_key(|function| function.node.start_byte());
    plan.types.sort_by_key(|(_, node, _)| node.start_byte());
    plan.declined.sort_by_key(|(_, _, line)| *line);
    plan
}

fn row(node: Node<'_>) -> u32 {
    node.start_position().row as u32
}

/// Every function nested in a callable's body that a bare read of its name
/// reaches, as (name, visible from, visible until, 0-based line), for the
/// escape record.
pub(crate) fn nested_function_scopes(
    callable: Node<'_>,
    source: &[u8],
) -> Vec<(String, usize, usize, u32)> {
    plan(callable, source)
        .functions
        .into_iter()
        .filter_map(|function| {
            let (from, until) = function.scope?;
            Some((function.name, from, until, row(function.node)))
        })
        .collect()
}
