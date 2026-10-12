// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Receiver narrowing by a builtin `isinstance` guard.
//!
//! `if isinstance(c, AliasPath): c.convert_to_aliases()` writes down which
//! classes the receiver can be at that call, when nothing else in the
//! function does: `c` is a loop variable over a list annotated with a union.
//! The guard is evidence of possibility and never of dispatch. A subclass
//! passes the same guard and may override the member, so each guard class's
//! member is a possible target of the call, and the call is never certified to
//! run one body.
//!
//! The narrowing is deliberately small. It holds only in the true branch of an
//! `if` or `elif` whose condition is the guard, or a conjunction holding it,
//! inside the execution scope the guard is written in, and only while that
//! branch never binds the receiver's name again. The guard must be the
//! interpreter's own `isinstance`: no module or enclosing-function binding and
//! no star import may replace it. Each class must be a plain name the module
//! binds exactly once, by a top-level `class` statement (decorated or not) or
//! a top-level `from` import, that no enclosing function rebinds. A guard
//! whose classes cannot be grounded that way is a gap: the call keeps its
//! site and names no destination, rather than falling back to a same-named
//! member anywhere.

use std::collections::{HashMap, HashSet};

use tree_sitter::Node;

/// What a guard says about the receiver of one call.
pub(super) enum ReceiverGuard<'tree> {
    /// No admissible positive guard on the receiver encloses the call.
    Unguarded,
    /// A builtin `isinstance` guard admits exactly these classes, spelled as
    /// the module binds them, in the order written.
    Classes {
        guard: Node<'tree>,
        classes: Vec<String>,
    },
    /// A builtin `isinstance` guard on the receiver encloses the call, and its
    /// classes are not a finite set of grounded class names.
    Ungrounded,
}

/// The guard on `receiver` enclosing `call`, a call written through it.
pub(super) fn receiver_guard<'tree>(
    call: Node<'tree>,
    receiver: Node<'tree>,
    source: &[u8],
    module: &ModuleNames,
) -> ReceiverGuard<'tree> {
    if receiver.kind() != "identifier" {
        return ReceiverGuard::Unguarded;
    }
    let Ok(name) = receiver.utf8_text(source) else {
        return ReceiverGuard::Unguarded;
    };
    if name.is_empty() || name == "self" || name == "cls" {
        return ReceiverGuard::Unguarded;
    }
    let mut child = call;
    while let Some(parent) = child.parent() {
        match parent.kind() {
            // A nested scope runs later, or in a scope of its own, and the
            // name it reads may hold another value by then.
            "function_definition"
            | "class_definition"
            | "decorated_definition"
            | "lambda"
            | "list_comprehension"
            | "set_comprehension"
            | "dictionary_comprehension"
            | "generator_expression" => return ReceiverGuard::Unguarded,
            "if_statement" | "elif_clause" => {
                let in_branch = parent
                    .child_by_field_name("consequence")
                    .is_some_and(|branch| branch.id() == child.id());
                if let (true, Some(condition)) =
                    (in_branch, parent.child_by_field_name("condition"))
                {
                    match condition_guard(condition, name, source, module) {
                        Condition::Silent => {}
                        _ if rebinds(child, source, name) => return ReceiverGuard::Unguarded,
                        Condition::Classes(guard, classes) => {
                            return ReceiverGuard::Classes { guard, classes }
                        }
                        Condition::Ungrounded => return ReceiverGuard::Ungrounded,
                    }
                }
            }
            _ => {}
        }
        child = parent;
    }
    ReceiverGuard::Unguarded
}

enum Condition<'tree> {
    /// The condition says nothing about the receiver.
    Silent,
    Classes(Node<'tree>, Vec<String>),
    Ungrounded,
}

fn unparenthesized(mut node: Node<'_>) -> Node<'_> {
    while node.kind() == "parenthesized_expression" {
        let mut cursor = node.walk();
        let inner: Vec<_> = node
            .named_children(&mut cursor)
            .filter(|child| child.kind() != "comment")
            .collect();
        match inner.as_slice() {
            [only] => node = *only,
            _ => break,
        }
    }
    node
}

fn condition_guard<'tree>(
    condition: Node<'tree>,
    name: &str,
    source: &[u8],
    module: &ModuleNames,
) -> Condition<'tree> {
    let condition = unparenthesized(condition);
    match condition.kind() {
        "call" => isinstance_guard(condition, name, source, module),
        "boolean_operator"
            if condition
                .child_by_field_name("operator")
                .is_some_and(|operator| operator.kind() == "and") =>
        {
            let (Some(left), Some(right)) = (
                condition.child_by_field_name("left"),
                condition.child_by_field_name("right"),
            ) else {
                return Condition::Silent;
            };
            match (
                condition_guard(left, name, source, module),
                condition_guard(right, name, source, module),
            ) {
                (Condition::Silent, other) | (other, Condition::Silent) => other,
                // Two guards on one receiver intersect, and an intersection is
                // no finite set of classes to name.
                _ => Condition::Ungrounded,
            }
        }
        _ => Condition::Silent,
    }
}

fn isinstance_guard<'tree>(
    call: Node<'tree>,
    name: &str,
    source: &[u8],
    module: &ModuleNames,
) -> Condition<'tree> {
    let Some(function) = call.child_by_field_name("function") else {
        return Condition::Silent;
    };
    if function.kind() != "identifier" || function.utf8_text(source).ok() != Some("isinstance") {
        return Condition::Silent;
    }
    let Some(arguments) = call.child_by_field_name("arguments") else {
        return Condition::Silent;
    };
    let mut cursor = arguments.walk();
    let arguments: Vec<_> = arguments
        .named_children(&mut cursor)
        .filter(|argument| argument.kind() != "comment")
        .collect();
    let [subject, classes] = arguments.as_slice() else {
        return Condition::Silent;
    };
    if arguments.iter().any(|argument| {
        matches!(
            argument.kind(),
            "keyword_argument" | "list_splat" | "dictionary_splat"
        )
    }) {
        return Condition::Silent;
    }
    let subject = unparenthesized(*subject);
    if subject.kind() != "identifier" || subject.utf8_text(source).ok() != Some(name) {
        return Condition::Silent;
    }
    // A replaced `isinstance` is an ordinary call that guards nothing.
    if !super::module_import_read_is_unshadowed(function, source, "isinstance")
        || module.binding("isinstance") != ModuleBinding::Unbound
    {
        return Condition::Silent;
    }
    let classes = unparenthesized(*classes);
    let elements: Vec<Node<'tree>> = match classes.kind() {
        "identifier" => vec![classes],
        "tuple" => {
            let mut cursor = classes.walk();
            classes
                .named_children(&mut cursor)
                .filter(|element| element.kind() != "comment")
                .map(unparenthesized)
                .collect()
        }
        _ => return Condition::Ungrounded,
    };
    if elements.is_empty() {
        return Condition::Ungrounded;
    }
    let mut grounded: Vec<String> = Vec::new();
    for element in elements {
        if element.kind() != "identifier" {
            return Condition::Ungrounded;
        }
        let Ok(class) = element.utf8_text(source) else {
            return Condition::Ungrounded;
        };
        let bound = module.binding(class);
        if !matches!(bound, ModuleBinding::Class | ModuleBinding::Import)
            || !super::module_import_read_is_unshadowed(element, source, class)
        {
            return Condition::Ungrounded;
        }
        if !grounded.iter().any(|seen| seen == class) {
            grounded.push(class.to_string());
        }
    }
    Condition::Classes(call, grounded)
}

/// How the module scope binds one name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModuleBinding {
    Unbound,
    /// Exactly one top-level `class` statement, decorated or not.
    Class,
    /// Exactly one top-level `from module import name [as alias]`.
    Import,
    /// Anything else: a second binding, a conditional one, an assignment, a
    /// function, a star import or a `global` write from a function.
    Other,
}

/// How the module binds every name its own scope binds, read once per file.
#[derive(Default)]
pub(super) struct ModuleNames {
    bindings: HashMap<String, ModuleBinding>,
    /// A module-level `from module import *`, which may bind any name.
    star: bool,
}

impl ModuleNames {
    /// The module scope of the tree `root` heads: its statements, the blocks
    /// they hold that open no scope, and every name any function declares
    /// `global`.
    pub(super) fn of(root: Node<'_>, source: &[u8]) -> Self {
        let mut names = ModuleNames::default();
        let mut cursor = root.walk();
        for statement in root.named_children(&mut cursor) {
            names.scan(statement, source, true);
        }
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            if node.kind() == "global_statement" {
                let mut globals = HashSet::new();
                direct_targets(node, source, &mut globals);
                for name in globals {
                    names.bind(name, ModuleBinding::Other);
                }
            }
            let mut cursor = node.walk();
            stack.extend(node.named_children(&mut cursor));
        }
        names
    }

    fn binding(&self, name: &str) -> ModuleBinding {
        if self.star {
            return ModuleBinding::Other;
        }
        self.bindings
            .get(name)
            .copied()
            .unwrap_or(ModuleBinding::Unbound)
    }

    /// A second binding of one name makes it [`ModuleBinding::Other`].
    fn bind(&mut self, name: String, binding: ModuleBinding) {
        self.bindings
            .entry(name)
            .and_modify(|seen| *seen = ModuleBinding::Other)
            .or_insert(binding);
    }

    fn scan(&mut self, node: Node<'_>, source: &[u8], top_level: bool) {
        match node.kind() {
            "class_definition" => {
                if let Some(name) = definition_name(node, source) {
                    let binding = if top_level {
                        ModuleBinding::Class
                    } else {
                        ModuleBinding::Other
                    };
                    self.bind(name.to_string(), binding);
                }
                return;
            }
            "function_definition" => {
                if let Some(name) = definition_name(node, source) {
                    self.bind(name.to_string(), ModuleBinding::Other);
                }
                return;
            }
            // A decorated class still binds a class under its name: a class
            // decorator such as `dataclass(slots=True)` may rebuild the class,
            // and what a guard tests against is that result, whose members are
            // the ones the statement declares. A decorated function is no
            // class.
            "decorated_definition" => {
                if let Some(definition) = node.child_by_field_name("definition") {
                    if let Some(name) = definition_name(definition, source) {
                        let binding = if top_level && definition.kind() == "class_definition" {
                            ModuleBinding::Class
                        } else {
                            ModuleBinding::Other
                        };
                        self.bind(name.to_string(), binding);
                    }
                }
                return;
            }
            "lambda"
            | "list_comprehension"
            | "set_comprehension"
            | "dictionary_comprehension"
            | "generator_expression" => return,
            "import_from_statement" | "import_statement" | "future_import_statement" => {
                for binding in import_bindings(node, source) {
                    match binding {
                        ImportBinding::Star => self.star = true,
                        ImportBinding::Name(bound, plain_from) => {
                            let binding = if plain_from && top_level {
                                ModuleBinding::Import
                            } else {
                                ModuleBinding::Other
                            };
                            self.bind(bound, binding);
                        }
                    }
                }
                return;
            }
            _ => {}
        }
        let mut targets = HashSet::new();
        direct_targets(node, source, &mut targets);
        for name in targets {
            self.bind(name, ModuleBinding::Other);
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            self.scan(child, source, false);
        }
    }
}

fn definition_name<'a>(node: Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    node.child_by_field_name("name")?.utf8_text(source).ok()
}

enum ImportBinding {
    /// `from module import *`.
    Star,
    /// A bound name, and whether a `from module import` statement binds it.
    Name(String, bool),
}

fn import_bindings(node: Node<'_>, source: &[u8]) -> Vec<ImportBinding> {
    let from = node.kind() == "import_from_statement";
    let module = node
        .child_by_field_name("module_name")
        .map(|module| module.id());
    let mut out = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if Some(child.id()) == module {
            continue;
        }
        match child.kind() {
            "wildcard_import" => out.push(ImportBinding::Star),
            // `import a.b` binds `a`; `from m import x` binds `x`.
            "dotted_name" => {
                if let Some(bound) = child
                    .utf8_text(source)
                    .ok()
                    .and_then(|text| text.split('.').next())
                    .filter(|bound| !bound.is_empty())
                {
                    out.push(ImportBinding::Name(bound.to_string(), from));
                }
            }
            "aliased_import" => {
                if let Some(alias) = child
                    .child_by_field_name("alias")
                    .and_then(|alias| alias.utf8_text(source).ok())
                    .filter(|alias| !alias.is_empty())
                {
                    out.push(ImportBinding::Name(alias.to_string(), from));
                }
            }
            _ => {}
        }
    }
    out
}

/// The names `node` itself binds, other than by an import or a definition,
/// which their own arms read.
fn direct_targets(node: Node<'_>, source: &[u8], targets: &mut HashSet<String>) {
    match node.kind() {
        "assignment" | "augmented_assignment" | "for_statement" | "for_in_clause" => {
            if let Some(left) = node.child_by_field_name("left") {
                super::collect_python_binding_targets(&left, source, targets);
            }
        }
        "named_expression" => {
            if let Some(bound) = node.child_by_field_name("name") {
                super::collect_python_binding_targets(&bound, source, targets);
            }
        }
        "as_pattern" | "except_clause" => {
            if let Some(alias) = node.child_by_field_name("alias") {
                super::collect_python_binding_targets(&alias, source, targets);
            }
        }
        "delete_statement" | "global_statement" | "nonlocal_statement" => {
            let mut cursor = node.walk();
            for target in node.named_children(&mut cursor) {
                super::collect_python_binding_targets(&target, source, targets);
            }
        }
        "type_alias_statement" => {
            if let Some(bound) = node.named_child(0) {
                super::collect_python_binding_targets(&bound, source, targets);
            }
        }
        _ => {}
    }
}

fn binds_directly(node: Node<'_>, source: &[u8], name: &str) -> bool {
    let mut targets = HashSet::new();
    direct_targets(node, source, &mut targets);
    targets.contains(name)
}

/// Whether `branch` binds `name` anywhere, which ends the narrowing a guard
/// gave it. A binding after the call still counts: a loop inside the branch
/// can carry it back to the call.
fn rebinds(branch: Node<'_>, source: &[u8], name: &str) -> bool {
    let mut stack = vec![branch];
    while let Some(node) = stack.pop() {
        let bound = match node.kind() {
            "class_definition" | "function_definition" => {
                definition_name(node, source) == Some(name)
            }
            "import_from_statement" | "import_statement" => import_bindings(node, source)
                .iter()
                .any(|binding| match binding {
                    ImportBinding::Star => true,
                    ImportBinding::Name(bound, _) => bound == name,
                }),
            _ => binds_directly(node, source, name),
        };
        if bound {
            return true;
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    false
}
