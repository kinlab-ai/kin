// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Bound members a method reads as values through its own instance.
//!
//! `Consumer(process_event=self._process_event)` hands a bound method to
//! another object. Nothing calls it here, so it is no `Calls` edge, but it is
//! a use of that method, and without a relation the method's references
//! omitted the place its callback is registered. This records the read as a
//! `References` relation named `Class.member` for the enclosing class, tagged
//! [`RelationSyntacticRole::PythonSelfMemberValue`]; the linker decides
//! whether the class declares or inherits such a method.
//!
//! Only a reading the method's own instance parameter licenses is recorded: an
//! undecorated method, whose first parameter its body never rebinds, reading a
//! member directly in its own body. A name the class also assigns as instance
//! or class data is left out, because the read may reach that data rather
//! than the method. A member in call position is the call's own edge and is
//! not repeated here.

use std::collections::{HashMap, HashSet};

use tree_sitter::Node;

use crate::adapter::site_from_node;
use crate::extract::{ExtractedRelation, RelationSite, RelationSyntacticRole};

/// Append one relation per value read of a member through the instance
/// parameter of `method`, a method of `class`.
pub(super) fn push_self_member_values(
    method: &Node<'_>,
    source: &[u8],
    method_entity: &str,
    class: &str,
    skip: &HashSet<usize>,
    class_data: &mut HashMap<usize, HashSet<String>>,
    relations: &mut Vec<ExtractedRelation>,
) {
    if method
        .parent()
        .is_some_and(|parent| parent.kind() == "decorated_definition")
    {
        return;
    }
    let (Some(instance), Some(body)) = (
        instance_parameter(method, source),
        method.child_by_field_name("body"),
    ) else {
        return;
    };
    let mut rebound = HashSet::new();
    let mut outward = HashSet::new();
    super::collect_python_scope_bindings(&body, source, &mut rebound, &mut outward);
    if rebound.contains(&instance) || outward.contains(&instance) {
        return;
    }
    let empty = HashSet::new();
    let data = match super::python_enclosing_class(method) {
        Some(class) => &*class_data
            .entry(class.id())
            .or_insert_with(|| class_data_names(&class, source)),
        None => &empty,
    };
    let mut stack = vec![body];
    while let Some(node) = stack.pop() {
        if skip.contains(&node.id())
            || matches!(
                node.kind(),
                "function_definition" | "class_definition" | "decorated_definition" | "lambda"
            )
        {
            continue;
        }
        if let Some(member) = member_value(&node, source, &instance) {
            if !data.contains(member) {
                relations.push(ExtractedRelation {
                    kind: kin_model::RelationKind::References,
                    src_name: method_entity.to_string(),
                    dst_name: format!("{class}.{member}"),
                    import_source: None,
                    call_shape: None,
                    receiver: None,
                    site: Some(RelationSite {
                        syntactic_role: Some(RelationSyntacticRole::PythonSelfMemberValue),
                        ..site_from_node(&node)
                    }),
                });
            }
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
}

/// The name a method's first positional parameter binds, which is the
/// instance the method was called on.
fn instance_parameter(method: &Node<'_>, source: &[u8]) -> Option<String> {
    let parameters = method.child_by_field_name("parameters")?;
    let mut cursor = parameters.walk();
    let first = parameters
        .named_children(&mut cursor)
        .find(|parameter| parameter.kind() != "comment")?;
    let name = match first.kind() {
        "identifier" => first,
        "typed_parameter" => first
            .named_child(0)
            .filter(|name| name.kind() == "identifier")?,
        _ => return None,
    };
    name.utf8_text(source)
        .ok()
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

/// `member` when `node` is `instance.member` read as a value.
fn member_value<'a>(node: &Node<'_>, source: &'a [u8], instance: &str) -> Option<&'a str> {
    if node.kind() != "attribute" {
        return None;
    }
    let object = node.child_by_field_name("object")?;
    if object.kind() != "identifier" || object.utf8_text(source).ok()? != instance {
        return None;
    }
    let parent = node.parent()?;
    if parent.kind() == "call"
        && parent
            .child_by_field_name("function")
            .is_some_and(|function| function.id() == node.id())
    {
        return None;
    }
    if is_write_target(node) {
        return None;
    }
    node.child_by_field_name("attribute")?
        .utf8_text(source)
        .ok()
        .filter(|member| !member.is_empty())
}

/// Whether `node` is written to rather than read: an assignment, loop or
/// `del` target, possibly inside a destructuring pattern.
fn is_write_target(node: &Node<'_>) -> bool {
    let mut child = *node;
    while let Some(parent) = child.parent() {
        match parent.kind() {
            "pattern_list"
            | "tuple_pattern"
            | "list_pattern"
            | "tuple"
            | "list"
            | "expression_list"
            | "parenthesized_expression"
            | "list_splat_pattern" => {}
            "assignment" | "augmented_assignment" | "for_statement" | "for_in_clause" => {
                return parent
                    .child_by_field_name("left")
                    .is_some_and(|left| left.id() == child.id());
            }
            "delete_statement" => return true,
            _ => return false,
        }
        child = parent;
    }
    false
}

/// Names the class binds as data: assigned or annotated in its body, or
/// assigned through `self.name` (or any method's instance parameter) in one
/// of its methods.
fn class_data_names(class: &Node<'_>, source: &[u8]) -> HashSet<String> {
    let mut names = HashSet::new();
    let Some(body) = class.child_by_field_name("body") else {
        return names;
    };
    let mut cursor = body.walk();
    for statement in body.named_children(&mut cursor) {
        let assignment = match statement.kind() {
            "expression_statement" => statement
                .named_child(0)
                .filter(|child| matches!(child.kind(), "assignment" | "augmented_assignment")),
            _ => None,
        };
        if let Some(left) = assignment.and_then(|assignment| assignment.child_by_field_name("left"))
        {
            collect_identifiers(&left, source, &mut names);
        }
    }
    let mut stack = vec![body];
    while let Some(node) = stack.pop() {
        if node.kind() == "class_definition" && node.id() != class.id() {
            continue;
        }
        if matches!(node.kind(), "assignment" | "augmented_assignment") {
            if let Some(left) = node.child_by_field_name("left") {
                collect_attribute_leaves(&left, source, &mut names);
            }
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    names
}

fn collect_identifiers(node: &Node<'_>, source: &[u8], out: &mut HashSet<String>) {
    match node.kind() {
        "identifier" => {
            if let Ok(name) = node.utf8_text(source) {
                out.insert(name.to_string());
            }
        }
        "attribute" | "subscript" => {}
        _ => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                collect_identifiers(&child, source, out);
            }
        }
    }
}

/// The leaf of every `name.leaf` written to, through any destructuring. Any
/// object name counts, so a method whose instance parameter is not called
/// `self` still contributes.
fn collect_attribute_leaves(node: &Node<'_>, source: &[u8], out: &mut HashSet<String>) {
    match node.kind() {
        "attribute" => {
            let object_is_name = node
                .child_by_field_name("object")
                .is_some_and(|object| object.kind() == "identifier");
            if let (true, Some(leaf)) = (
                object_is_name,
                node.child_by_field_name("attribute")
                    .and_then(|leaf| leaf.utf8_text(source).ok()),
            ) {
                out.insert(leaf.to_string());
            }
        }
        "subscript" => {}
        _ => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                collect_attribute_leaves(&child, source, out);
            }
        }
    }
}
