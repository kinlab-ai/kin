// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Definitions nested in C++ function bodies.
//!
//! A class declared in a function body and a lambda bound to a local are real
//! definitions: a test double overrides through a local class, and a callback
//! bound to a local is called by that name. The adapter admits both under the
//! `enclosing.<locals>.name` rule of [`crate::nested`], with the calls in
//! their bodies moved onto them.

use kin_model::EntityKind;
use tree_sitter::Node;

use super::cpp_escape::{locals_of, Local};
use crate::value_escape::text_of;

/// What one callable's body defines, found before its calls are extracted so
/// the calls inside an admitted definition can move onto it.
pub(crate) struct NestedPlan<'tree> {
    /// Lambdas bound once to a local, with the lambda expression.
    pub(crate) lambdas: Vec<(LocalLambda, Node<'tree>)>,
    /// Named classes, structs and unions with a body, declared in the body
    /// outside any lambda.
    pub(crate) classes: Vec<Node<'tree>>,
    /// Definitions found and not admitted: the name as written, what it
    /// declares, and its 0-based line.
    pub(crate) declined: Vec<(String, EntityKind, u32)>,
}

/// Read what a function definition's body defines.
pub(crate) fn plan<'tree>(function: Node<'tree>, source: &[u8]) -> NestedPlan<'tree> {
    let mut plan = NestedPlan {
        lambdas: Vec::new(),
        classes: Vec::new(),
        declined: Vec::new(),
    };
    let Some(body) = function.child_by_field_name("body") else {
        return plan;
    };
    let admitted = local_lambdas(function, source);
    let mut stack = vec![(body, false)];
    while let Some((node, in_lambda)) = stack.pop() {
        match node.kind() {
            "function_definition" => {
                // A definition with a return type directly in a block is a
                // GNU C nested function. One without is how the grammar reads
                // a macro that opens a block, and declares nothing.
                let real = node.child_by_field_name("type").is_some()
                    && node
                        .child_by_field_name("declarator")
                        .is_some_and(|declarator| {
                            super::cpp_escape::declarator_name_node(declarator).is_some()
                                && is_function_declarator(declarator)
                        });
                if real {
                    if let Some(name) = definition_name(node, source) {
                        plan.declined.push((name, EntityKind::Function, row(node)));
                    }
                }
                continue;
            }
            "class_specifier" | "struct_specifier" | "union_specifier" => {
                if let (Some(name), Some(_)) = (
                    node.child_by_field_name("name"),
                    node.child_by_field_name("body"),
                ) {
                    if in_lambda {
                        plan.declined.push((
                            text_of(name, source).to_string(),
                            EntityKind::Class,
                            row(node),
                        ));
                    } else {
                        plan.classes.push(node);
                    }
                }
                // Its members are its own; a class nested in it is read
                // when it is admitted.
                continue;
            }
            "enum_specifier" => {
                if let (Some(name), Some(_)) = (
                    node.child_by_field_name("name"),
                    node.child_by_field_name("body"),
                ) {
                    plan.declined.push((
                        text_of(name, source).to_string(),
                        EntityKind::EnumDef,
                        row(node),
                    ));
                }
                continue;
            }
            "declaration" if !in_lambda => {
                if let Some(lambda) = admitted
                    .iter()
                    .find(|lambda| lambda.declaration == (node.start_byte(), node.end_byte()))
                {
                    if let Some(expression) = lambda_value(node) {
                        plan.lambdas.push((lambda.clone(), expression));
                    }
                } else if let Some(name) = lambda_declarator_name(node, source) {
                    plan.declined.push((name, EntityKind::Function, row(node)));
                }
            }
            _ => {}
        }
        let inside = in_lambda || node.kind() == "lambda_expression";
        let mut cursor = node.walk();
        let children: Vec<Node<'tree>> = node.children(&mut cursor).collect();
        stack.extend(children.into_iter().map(|child| (child, inside)));
    }
    plan.lambdas.sort_by_key(|(lambda, _)| lambda.declaration.0);
    plan.classes.sort_by_key(|class| class.start_byte());
    plan.declined.sort_by_key(|(_, _, line)| *line);
    plan
}

fn is_function_declarator(declarator: Node<'_>) -> bool {
    let mut current = declarator;
    for _ in 0..16 {
        match current.kind() {
            "function_declarator" => return true,
            "pointer_declarator" | "reference_declarator" => match current
                .child_by_field_name("declarator")
                .or_else(|| current.named_child(0))
            {
                Some(next) => current = next,
                None => return false,
            },
            _ => return false,
        }
    }
    false
}

fn row(node: Node<'_>) -> u32 {
    node.start_position().row as u32
}

fn definition_name(function: Node<'_>, source: &[u8]) -> Option<String> {
    let declarator = function.child_by_field_name("declarator")?;
    let name = super::cpp_escape::declarator_name_node(declarator)?;
    let spelled = text_of(name, source).trim();
    (!spelled.is_empty()).then(|| spelled.to_string())
}

/// The lambda a one-declarator declaration binds.
fn lambda_value(declaration: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = declaration.walk();
    let declarator = declaration
        .children_by_field_name("declarator", &mut cursor)
        .next()?;
    let mut value = declarator.child_by_field_name("value")?;
    while value.kind() == "parenthesized_expression" {
        value = value.named_child(0)?;
    }
    (value.kind() == "lambda_expression").then_some(value)
}

/// The name a declaration binds a lambda to, whatever else binds it.
fn lambda_declarator_name(declaration: Node<'_>, source: &[u8]) -> Option<String> {
    lambda_value(declaration)?;
    let mut cursor = declaration.walk();
    let declarator = declaration
        .children_by_field_name("declarator", &mut cursor)
        .next()?;
    let name = declarator
        .child_by_field_name("declarator")
        .filter(|name| name.kind() == "identifier")?;
    Some(text_of(name, source).to_string())
}

/// A lambda bound once to a local: `auto on_done = [&](int code) { ... };`.
#[derive(Debug, Clone)]
pub(crate) struct LocalLambda {
    pub(crate) name: String,
    /// Where a read of the name sees this binding.
    pub(crate) visible_from: usize,
    pub(crate) end: usize,
    /// The declaration statement, which the admitted entity spans.
    pub(crate) declaration: (usize, usize),
    /// The 0-based line the declaration starts on.
    pub(crate) line: u32,
}

/// Every lambda the callable binds to an `auto` local it declares exactly
/// once, so a read of that name anywhere it is visible reads this lambda. A
/// local declared twice, a name a parameter or another local also declares,
/// and a local of any other type are left out.
pub(crate) fn local_lambdas(function: Node<'_>, source: &[u8]) -> Vec<LocalLambda> {
    let locals: Vec<Local> = locals_of(function, source);
    let mut out = Vec::new();
    let Some(body) = function.child_by_field_name("body") else {
        return out;
    };
    let mut stack = vec![body];
    while let Some(node) = stack.pop() {
        match node.kind() {
            // A local class's members are its own.
            "field_declaration_list" | "function_definition" => continue,
            "declaration" => {
                if let Some(lambda) = lambda_binding(node, source, &locals) {
                    out.push(lambda);
                }
            }
            _ => {}
        }
        let mut cursor = node.walk();
        stack.extend(node.children(&mut cursor));
    }
    out.sort_by_key(|lambda| lambda.declaration.0);
    out
}

fn lambda_binding(declaration: Node<'_>, source: &[u8], locals: &[Local]) -> Option<LocalLambda> {
    let mut cursor = declaration.walk();
    let declarators: Vec<Node<'_>> = declaration
        .children_by_field_name("declarator", &mut cursor)
        .collect();
    let [declarator] = declarators.as_slice() else {
        return None;
    };
    // `auto` gives the local the lambda's own closure type, which nothing
    // but a copy of this lambda can be assigned to. A `std::function` local
    // could be reassigned any callable, so a call through it names none.
    if declaration
        .child_by_field_name("type")
        .is_none_or(|ty| ty.kind() != "placeholder_type_specifier")
    {
        return None;
    }
    if declarator.kind() != "init_declarator" {
        return None;
    }
    let name = declarator
        .child_by_field_name("declarator")
        .filter(|name| name.kind() == "identifier")?;
    let mut value = declarator.child_by_field_name("value")?;
    while value.kind() == "parenthesized_expression" {
        value = value.named_child(0)?;
    }
    if value.kind() != "lambda_expression" {
        return None;
    }
    let spelled = text_of(name, source);
    let mut bindings = locals.iter().filter(|local| local.name == spelled);
    let binding = bindings.next()?;
    if bindings.next().is_some() || binding.visible_from != name.end_byte() {
        return None;
    }
    Some(LocalLambda {
        name: spelled.to_string(),
        visible_from: binding.visible_from,
        end: binding.end,
        declaration: (declaration.start_byte(), declaration.end_byte()),
        line: declaration.start_position().row as u32,
    })
}
