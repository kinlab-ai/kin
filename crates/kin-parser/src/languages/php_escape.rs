// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! PHP's facts for the value-escape record (see [`crate::value_escape`]).
//!
//! PHP holds a function or method as a value only through a callable: a
//! string naming a function (`'helper'`, `'Store::make'`), an array naming a
//! method (`[$store, 'save']`, `[Store::class, 'make']`), or first-class
//! callable syntax (`helper(...)`, `$store->save(...)`). A bare name is a
//! constant, never a function, and a variable is a local, so neither is
//! recorded. PHP functions and classes are global once declared, so every
//! function name is read as free, and every method name as a member of the
//! value or class the callable names.
//!
//! Recorded:
//!
//! - every callable string in a callable position of a builtin that takes one
//!   (`array_map`, `usort`, `call_user_func`, `register_shutdown_function`,
//!   `Closure::fromCallable`, ...), every array callable anywhere, every
//!   first-class callable, and every `use function` or `use` alias, with where
//!   the value goes (an argument, an assignment, which aliases it under a
//!   property's name, a return, an array);
//! - reflective sites: a variable function or method call (`$f()`,
//!   `$obj->$name()`, `$class::make()`, `new $class`), a callable builtin
//!   handed a computed callable, `eval`, `include` or `require` of a computed
//!   path, a reflection object built from a computed name (a literal names the
//!   member and is recorded as a use of it), and `get_class_methods`;
//! - protocol sites: attributes, which frameworks instantiate by reflection;
//! - protocol members: magic methods, and the members an interface the class
//!   declares makes the language call (`ArrayAccess`, `Iterator`,
//!   `IteratorAggregate`, `Countable`, `JsonSerializable`, `Stringable`).

use std::collections::HashSet;

use kin_model::{
    FileEscapeRecord, ModuleScope, OccurrenceBinding, OccurrenceContext, ProtocolForm,
    ProtocolMember, ProtocolSite, ReceiverKind, ReflectiveForm, ReflectiveReach, ReflectiveSite,
    ValueOccurrence, EXCERPT_MAX, RECEIVER_TEXT_MAX,
};
use tree_sitter::{Node, Tree};

use crate::value_escape::{
    collapsed_text, is_name, line_of, text_of, unparsed_lines, FileEscapeCensus, ProtocolStamp,
};

/// Builtins that take a callable, by lowercase name, with the argument
/// positions that hold it.
const CALLABLE_BUILTINS: &[(&str, &[usize])] = &[
    ("array_filter", &[1]),
    ("array_map", &[0]),
    ("array_reduce", &[1]),
    ("array_walk", &[1]),
    ("array_walk_recursive", &[1]),
    ("call_user_func", &[0]),
    ("call_user_func_array", &[0]),
    ("forward_static_call", &[0]),
    ("forward_static_call_array", &[0]),
    ("fromcallable", &[0]),
    ("iterator_apply", &[1]),
    ("ob_start", &[0]),
    ("preg_replace_callback", &[1]),
    ("register_shutdown_function", &[0]),
    ("register_tick_function", &[0]),
    ("set_error_handler", &[0]),
    ("set_exception_handler", &[0]),
    ("spl_autoload_register", &[0]),
    ("uasort", &[1]),
    ("uksort", &[1]),
    ("usort", &[1]),
    ("array_udiff", &[2]),
    ("array_uintersect", &[2]),
    ("header_register_callback", &[0]),
];

/// Reflection classes whose constructor takes a class and a member name.
const REFLECTION_MEMBERS: &[&str] = &["reflectionmethod", "reflectionproperty"];

/// Take the census of one PHP file.
pub(crate) fn census(tree: &Tree, source: &[u8]) -> FileEscapeCensus {
    let root = tree.root_node();
    let mut walker = Walker {
        source,
        record: FileEscapeRecord::new(ModuleScope::Closed),
        protocol_members: Vec::new(),
        classes: Vec::new(),
        closures: Vec::new(),
        file: crate::value_origins::php::FileContext::read(root, source),
    };
    walker.record.unparsed = unparsed_lines(tree, source);
    walker.visit(root);
    FileEscapeCensus {
        record: walker.record,
        protocol_members: walker.protocol_members,
        non_enumerable: Vec::new(),
    }
}

/// A class body the walk is inside: its name and the interfaces it declares.
struct Class {
    name: Option<String>,
    interfaces: HashSet<String>,
}

struct Walker<'s> {
    source: &'s [u8],
    record: FileEscapeRecord,
    protocol_members: Vec<ProtocolStamp>,
    classes: Vec<Class>,
    /// For each function or method the walk is inside, the variables bound
    /// once to a closure, which the adapter admits as nested definitions: a
    /// call through one is a call of that closure, not a lookup.
    closures: Vec<HashSet<String>>,
    file: crate::value_origins::php::FileContext,
}

impl<'s> Walker<'s> {
    fn text(&self, node: Node<'_>) -> &'s str {
        text_of(node, self.source)
    }

    fn visit(&mut self, node: Node<'_>) {
        match node.kind() {
            "class_declaration"
            | "trait_declaration"
            | "enum_declaration"
            | "interface_declaration"
            | "anonymous_class" => {
                self.stamp_attributes(node);
                let name = node
                    .child_by_field_name("name")
                    .map(|name| self.text(name).to_string());
                let interfaces = self.interfaces_of(node);
                self.classes.push(Class { name, interfaces });
                self.visit_children(node);
                self.classes.pop();
                return;
            }
            "method_declaration" | "function_definition" => {
                self.stamp_attributes(node);
                if node.kind() == "method_declaration" {
                    self.stamp_protocol(node);
                }
                let bound = self.bound_closures(node);
                self.closures.push(bound);
                self.visit_children(node);
                self.closures.pop();
                return;
            }
            "property_declaration" => self.stamp_attributes(node),
            "namespace_use_declaration" => {
                self.read_use(node);
                return;
            }
            "function_call_expression" => self.read_function_call(node),
            "member_call_expression" | "nullsafe_member_call_expression" => {
                self.read_member_call(node)
            }
            "scoped_call_expression" => self.read_scoped_call(node),
            "object_creation_expression" => self.read_creation(node),
            "array_creation_expression" => self.read_array_callable(node),
            "include_expression"
            | "include_once_expression"
            | "require_expression"
            | "require_once_expression" => {
                let literal = node
                    .named_child(0)
                    .is_some_and(|path| string_literal(path, self.source).is_some());
                if !literal {
                    self.push_reflective(
                        node,
                        ReflectiveForm::DynamicImport,
                        ReflectiveReach::Nothing,
                    );
                }
            }
            _ => {}
        }
        self.visit_children(node);
    }

    fn visit_children(&mut self, node: Node<'_>) {
        let mut cursor = node.walk();
        let children: Vec<Node<'_>> = node.children(&mut cursor).collect();
        for child in children {
            self.visit(child);
        }
    }

    /// The variables a function or method binds exactly once to a closure in
    /// its own body, the ones the adapter admits as nested definitions.
    fn bound_closures(&self, callable: Node<'_>) -> HashSet<String> {
        let mut out = HashSet::new();
        let Some(body) = callable.child_by_field_name("body") else {
            return out;
        };
        let census = crate::value_origins::php::Census::of(callable, &self.file, self.source);
        let mut stack = vec![body];
        while let Some(node) = stack.pop() {
            match node.kind() {
                "anonymous_function" | "arrow_function" => {
                    if let Some(name) =
                        crate::value_origins::php::bound_closure_name(node, self.source)
                    {
                        let variable = format!("${name}");
                        if census.single(&variable) {
                            out.insert(variable);
                        }
                    }
                    continue;
                }
                "function_definition"
                | "method_declaration"
                | "class_declaration"
                | "interface_declaration"
                | "trait_declaration"
                | "enum_declaration"
                | "anonymous_class" => continue,
                _ => {}
            }
            let mut cursor = node.walk();
            stack.extend(node.named_children(&mut cursor));
        }
        out
    }

    fn enclosing_class(&self) -> Option<&str> {
        self.classes.last().and_then(|class| class.name.as_deref())
    }

    // ── Callables ─────────────────────────────────────────────────────────

    fn read_function_call(&mut self, call: Node<'_>) {
        let Some(function) = call.child_by_field_name("function") else {
            return;
        };
        let arguments = arguments_of(call);
        match function.kind() {
            "name" | "qualified_name" => {
                let spelled = self.text(function).trim_start_matches('\\').to_string();
                let leaf = spelled
                    .rsplit('\\')
                    .next()
                    .unwrap_or(&spelled)
                    .to_lowercase();
                if is_first_class_callable(call) {
                    let name = spelled.rsplit('\\').next().unwrap_or(&spelled).to_string();
                    if let Some(context) = self.context_of(call) {
                        self.push(
                            function,
                            &name,
                            context,
                            OccurrenceBinding::Free,
                            None,
                            None,
                        );
                    }
                    return;
                }
                if leaf == "eval" {
                    self.push_reflective(call, ReflectiveForm::Evaluate, ReflectiveReach::Any);
                    return;
                }
                if leaf == "get_class_methods" {
                    self.push_reflective(call, ReflectiveForm::Enumerate, ReflectiveReach::Members);
                    return;
                }
                self.callable_arguments(call, &leaf, &arguments);
            }
            // `$f(...)`: a variable function, whatever the variable holds,
            // unless it holds the one closure it is bound to.
            "variable_name"
            | "parenthesized_expression"
            | "subscript_expression"
            | "member_access_expression" => {
                let bound = function.kind() == "variable_name"
                    && self
                        .closures
                        .last()
                        .is_some_and(|closures| closures.contains(self.text(function)));
                if !bound {
                    self.push_reflective(
                        call,
                        ReflectiveForm::ComputedMember,
                        ReflectiveReach::Any,
                    );
                }
            }
            _ => {}
        }
    }

    /// Callables handed to a builtin that calls them.
    fn callable_arguments(&mut self, call: Node<'_>, builtin: &str, arguments: &[Node<'_>]) {
        let Some((_, positions)) = CALLABLE_BUILTINS.iter().find(|(name, _)| *name == builtin)
        else {
            return;
        };
        for &position in positions.iter() {
            let Some(argument) = arguments.get(position) else {
                continue;
            };
            let Some(value) = argument_value(*argument) else {
                continue;
            };
            match value.kind() {
                "string" | "encapsed_string" => match string_literal(value, self.source) {
                    Some(literal) => {
                        self.push_callable_string(value, &literal, OccurrenceContext::Argument)
                    }
                    None => self.push_reflective(
                        call,
                        ReflectiveForm::ComputedMember,
                        ReflectiveReach::Any,
                    ),
                },
                // An array callable is read on its own; a closure is a call
                // the adapter reads; anything else is computed.
                "array_creation_expression" | "anonymous_function" | "arrow_function" => {}
                "function_call_expression"
                | "member_call_expression"
                | "scoped_call_expression"
                    if is_first_class_callable(value) => {}
                _ => {
                    self.push_reflective(call, ReflectiveForm::ComputedMember, ReflectiveReach::Any)
                }
            }
        }
    }

    /// `'helper'` or `'Store::make'` in a callable position.
    fn push_callable_string(&mut self, node: Node<'_>, literal: &str, context: OccurrenceContext) {
        let literal = literal.trim_start_matches('\\');
        match literal.split_once("::") {
            Some((class, method)) => {
                let class = class.rsplit('\\').next().unwrap_or(class);
                if is_name(method) {
                    self.push(
                        node,
                        method,
                        Flow(context, Vec::new()),
                        OccurrenceBinding::Member,
                        Some(class.to_string()),
                        Some(ReceiverKind::Other),
                    );
                }
            }
            None => {
                let name = literal.rsplit('\\').next().unwrap_or(literal);
                if is_name(name) {
                    self.push(
                        node,
                        name,
                        Flow(context, Vec::new()),
                        OccurrenceBinding::Free,
                        None,
                        None,
                    );
                }
            }
        }
    }

    fn read_member_call(&mut self, call: Node<'_>) {
        let Some(name) = call.child_by_field_name("name") else {
            return;
        };
        if name.kind() != "name" {
            // `$obj->$method()`: a member picked by a computed name.
            self.push_reflective(
                call,
                ReflectiveForm::ComputedMember,
                ReflectiveReach::Members,
            );
            return;
        }
        if !is_first_class_callable(call) {
            return;
        }
        let Some(context) = self.context_of(call) else {
            return;
        };
        let object = call.child_by_field_name("object");
        let (receiver, kind) = self.receiver_of(object);
        let method = self.text(name).to_string();
        self.push(
            name,
            &method,
            context,
            OccurrenceBinding::Member,
            receiver,
            Some(kind),
        );
    }

    fn read_scoped_call(&mut self, call: Node<'_>) {
        let (Some(name), Some(scope)) = (
            call.child_by_field_name("name"),
            call.child_by_field_name("scope"),
        ) else {
            return;
        };
        let computed_scope = !matches!(
            scope.kind(),
            "name" | "qualified_name" | "relative_name" | "relative_scope"
        );
        if name.kind() != "name" || computed_scope {
            // `$class::make()` or `Store::$method()`.
            self.push_reflective(
                call,
                ReflectiveForm::ComputedMember,
                ReflectiveReach::Members,
            );
            return;
        }
        if !is_first_class_callable(call) {
            return;
        }
        let Some(context) = self.context_of(call) else {
            return;
        };
        let (receiver, kind) = self.class_receiver(scope);
        let method = self.text(name).to_string();
        self.push(
            name,
            &method,
            context,
            OccurrenceBinding::Member,
            receiver,
            Some(kind),
        );
    }

    fn read_creation(&mut self, creation: Node<'_>) {
        let Some(class) = creation.named_child(0) else {
            return;
        };
        match class.kind() {
            "name" | "qualified_name" => {
                let spelled = self.text(class).trim_start_matches('\\').to_lowercase();
                let leaf = spelled.rsplit('\\').next().unwrap_or(&spelled).to_string();
                if REFLECTION_MEMBERS.contains(&leaf.as_str()) || leaf == "reflectionfunction" {
                    self.reflection(creation, &leaf);
                }
            }
            // `new $class(...)`: any class, by a computed name.
            "variable_name"
            | "member_access_expression"
            | "parenthesized_expression"
            | "subscript_expression"
            | "scoped_property_access_expression" => {
                self.push_reflective(
                    creation,
                    ReflectiveForm::ComputedMember,
                    ReflectiveReach::Any,
                );
            }
            _ => {}
        }
    }

    /// `new ReflectionMethod(Store::class, 'save')`: a literal names the
    /// member; a computed name is a reflective site.
    fn reflection(&mut self, creation: Node<'_>, class: &str) {
        let arguments = arguments_of(creation);
        let (member_at, class_at) = if class == "reflectionfunction" {
            (0, None)
        } else {
            (1, Some(0))
        };
        let member = arguments
            .get(member_at)
            .and_then(|argument| argument_value(*argument))
            .and_then(|value| string_literal(value, self.source).map(|literal| (value, literal)));
        match member {
            Some((value, literal)) if class_at.is_none() => {
                let name = literal.trim_start_matches('\\');
                let name = name.rsplit('\\').next().unwrap_or(name).to_string();
                if is_name(&name) {
                    self.push(
                        value,
                        &name,
                        Flow(OccurrenceContext::Argument, Vec::new()),
                        OccurrenceBinding::Free,
                        None,
                        None,
                    );
                }
            }
            Some((value, literal)) => {
                let receiver = class_at
                    .and_then(|at| arguments.get(at))
                    .and_then(|argument| argument_value(*argument))
                    .map(|target| self.class_text(target));
                if is_name(&literal) {
                    self.push(
                        value,
                        &literal,
                        Flow(OccurrenceContext::Argument, Vec::new()),
                        OccurrenceBinding::Member,
                        receiver,
                        Some(ReceiverKind::Other),
                    );
                }
            }
            None => self.push_reflective(
                creation,
                ReflectiveForm::ComputedMember,
                ReflectiveReach::Any,
            ),
        }
    }

    /// `[$store, 'save']`, `[$this, 'persist']`, `[Store::class, 'make']`,
    /// `['Store', 'make']`: an array that names a method is a callable
    /// wherever it goes.
    fn read_array_callable(&mut self, array: Node<'_>) {
        let mut cursor = array.walk();
        let elements: Vec<Node<'_>> = array
            .named_children(&mut cursor)
            .filter(|child| child.kind() == "array_element_initializer")
            .collect();
        let [target, method] = elements.as_slice() else {
            return;
        };
        // Keyed elements make a map, not a callable.
        if target.named_child_count() != 1 || method.named_child_count() != 1 {
            return;
        }
        let (Some(target), Some(method)) = (target.named_child(0), method.named_child(0)) else {
            return;
        };
        let Some(name) = string_literal(method, self.source) else {
            return;
        };
        if !is_name(&name) {
            return;
        }
        let (receiver, kind) = match target.kind() {
            "variable_name" | "member_access_expression" => self.receiver_of(Some(target)),
            "class_constant_access_expression" | "string" | "encapsed_string" => {
                (Some(self.class_text(target)), ReceiverKind::Other)
            }
            _ => return,
        };
        let context = self
            .context_of(array)
            .unwrap_or(Flow(OccurrenceContext::Other, Vec::new()));
        self.push(
            method,
            &name,
            context,
            OccurrenceBinding::Member,
            receiver,
            Some(kind),
        );
    }

    /// The receiver text and kind of a callable's object.
    fn receiver_of(&self, object: Option<Node<'_>>) -> (Option<String>, ReceiverKind) {
        match object {
            Some(object) if self.text(object) == "$this" => {
                (Some("$this".into()), ReceiverKind::Instance)
            }
            Some(object) => (
                Some(collapsed_text(object, self.source, RECEIVER_TEXT_MAX)),
                ReceiverKind::Other,
            ),
            None => (None, ReceiverKind::Other),
        }
    }

    fn class_receiver(&self, scope: Node<'_>) -> (Option<String>, ReceiverKind) {
        if scope.kind() == "relative_scope" {
            let spelled = self.text(scope).to_lowercase();
            let receiver = match spelled.as_str() {
                "self" | "static" => self.enclosing_class().map(str::to_string).or(Some(spelled)),
                _ => Some(spelled),
            };
            return (receiver, ReceiverKind::Instance);
        }
        (Some(self.class_text(scope)), ReceiverKind::Other)
    }

    /// A class a callable names, by leaf: `Store::class`, `'App\Store'`,
    /// `\App\Store`.
    fn class_text(&self, node: Node<'_>) -> String {
        let spelled = match node.kind() {
            "class_constant_access_expression" => node
                .named_child(0)
                .map(|class| self.text(class).to_string())
                .unwrap_or_default(),
            "string" | "encapsed_string" => string_literal(node, self.source).unwrap_or_default(),
            _ => self.text(node).to_string(),
        };
        let spelled = spelled.trim_start_matches('\\');
        spelled
            .rsplit('\\')
            .next()
            .unwrap_or(spelled)
            .chars()
            .take(RECEIVER_TEXT_MAX)
            .collect()
    }

    // ── Imports ───────────────────────────────────────────────────────────

    /// `use function A\helper as h;` and `use A\Store as S;` import a name
    /// under another.
    fn read_use(&mut self, declaration: Node<'_>) {
        let range = (
            declaration.start_byte() as u32,
            declaration.end_byte() as u32,
        );
        let mut prefix = String::new();
        let mut clauses = Vec::new();
        let mut cursor = declaration.walk();
        for child in declaration.named_children(&mut cursor) {
            match child.kind() {
                "namespace_name" | "qualified_name" | "name" => {
                    prefix = self.text(child).trim_start_matches('\\').to_string();
                }
                "namespace_use_group" => {
                    let mut inner = child.walk();
                    clauses.extend(
                        child
                            .named_children(&mut inner)
                            .filter(|clause| clause.kind() == "namespace_use_clause"),
                    );
                }
                "namespace_use_clause" => clauses.push(child),
                _ => {}
            }
        }
        for clause in clauses {
            let Some((path, alias)) =
                crate::value_origins::php::use_clause_parts(clause, self.source)
            else {
                continue;
            };
            let Some(alias) = alias else {
                continue;
            };
            let clause_type = clause
                .child_by_field_name("type")
                .map(|kind| self.text(kind).to_string());
            if clause_type.as_deref() == Some("const") {
                continue;
            }
            let full = if prefix.is_empty() {
                path
            } else {
                format!("{prefix}\\{path}")
            };
            let (module, imported) = match full.rsplit_once('\\') {
                Some((module, leaf)) => (module.to_string(), leaf.to_string()),
                None => (String::new(), full.clone()),
            };
            if alias == imported {
                continue;
            }
            self.record
                .values
                .entry(imported)
                .or_default()
                .push(ValueOccurrence {
                    line: line_of(clause),
                    context: OccurrenceContext::Import,
                    binding: OccurrenceBinding::Import,
                    receiver: None,
                    receiver_kind: None,
                    aliases: vec![alias],
                    module: (!module.is_empty()).then_some(module),
                    import: Some(range),
                    imported: None,
                    target_line: None,
                    form: None,
                });
        }
    }

    // ── Where a value goes ────────────────────────────────────────────────

    fn context_of(&self, node: Node<'_>) -> Option<Flow> {
        let mut child = node;
        loop {
            let parent = child.parent()?;
            match parent.kind() {
                "parenthesized_expression" | "argument" => {
                    if parent.kind() == "argument" {
                        return Some(Flow(OccurrenceContext::Argument, Vec::new()));
                    }
                }
                "conditional_expression" => {
                    if parent
                        .named_child(0)
                        .is_some_and(|first| first.id() == child.id())
                    {
                        return None;
                    }
                }
                "array_element_initializer" => {
                    return Some(Flow(OccurrenceContext::Collection, Vec::new()));
                }
                "assignment_expression" => {
                    if parent
                        .child_by_field_name("right")
                        .is_none_or(|right| right.id() != child.id())
                    {
                        return None;
                    }
                    let left = parent.child_by_field_name("left")?;
                    return Some(match left.kind() {
                        "variable_name" => Flow(OccurrenceContext::Assignment, Vec::new()),
                        "member_access_expression"
                        | "nullsafe_member_access_expression"
                        | "scoped_property_access_expression" => {
                            let name = left
                                .child_by_field_name("name")
                                .map(|name| self.text(name).trim_start_matches('$').to_string())
                                .unwrap_or_default();
                            Flow(OccurrenceContext::Alias, vec![name])
                        }
                        "subscript_expression" => Flow(OccurrenceContext::Collection, Vec::new()),
                        _ => Flow(OccurrenceContext::Assignment, Vec::new()),
                    });
                }
                "return_statement" | "yield_expression" | "arrow_function" => {
                    return Some(Flow(OccurrenceContext::Return, Vec::new()));
                }
                "property_element" | "property_declaration" | "const_element" => {
                    return Some(Flow(OccurrenceContext::Alias, Vec::new()));
                }
                "expression_statement"
                | "binary_expression"
                | "unary_op_expression"
                | "if_statement"
                | "while_statement"
                | "for_statement"
                | "switch_statement"
                | "echo_statement"
                | "print_intrinsic" => return None,
                _ => return Some(Flow(OccurrenceContext::Other, Vec::new())),
            }
            child = parent;
        }
    }

    fn push(
        &mut self,
        node: Node<'_>,
        name: &str,
        flow: Flow,
        binding: OccurrenceBinding,
        receiver: Option<String>,
        receiver_kind: Option<ReceiverKind>,
    ) {
        let Flow(context, aliases) = flow;
        self.record
            .values
            .entry(name.to_string())
            .or_default()
            .push(ValueOccurrence {
                line: line_of(node),
                context,
                binding,
                receiver,
                receiver_kind,
                aliases,
                module: None,
                import: None,
                imported: None,
                target_line: None,
                form: None,
            });
    }

    fn push_reflective(&mut self, node: Node<'_>, form: ReflectiveForm, reach: ReflectiveReach) {
        self.record.reflective.push(ReflectiveSite {
            line: line_of(node),
            form,
            reach,
            key_prefix: None,
            key_suffix: None,
            called: false,
            excerpt: collapsed_text(node, self.source, EXCERPT_MAX),
            key_parts: Vec::new(),
            receiver: None,
            enumerable_only: false,
        });
    }

    // ── Protocol sites and members ────────────────────────────────────────

    /// `#[Route('/x')]`: an attribute names a class a framework instantiates
    /// by reflection.
    fn stamp_attributes(&mut self, node: Node<'_>) {
        let Some(list) = node.child_by_field_name("attributes") else {
            return;
        };
        let mut stack = vec![list];
        while let Some(current) = stack.pop() {
            if current.kind() == "attribute" {
                if let Some(name) = current.named_child(0) {
                    let spelled = self.text(name).trim_start_matches('\\');
                    let leaf = spelled.rsplit('\\').next().unwrap_or(spelled).to_string();
                    if is_name(&leaf) {
                        self.record.protocol.push(ProtocolSite {
                            line: line_of(current),
                            form: ProtocolForm::Decorator,
                            name: Some(leaf),
                        });
                    }
                }
                continue;
            }
            let mut cursor = current.walk();
            stack.extend(current.named_children(&mut cursor));
        }
    }

    fn interfaces_of(&self, class: Node<'_>) -> HashSet<String> {
        let mut out = HashSet::new();
        let mut cursor = class.walk();
        for child in class.children(&mut cursor) {
            if child.kind() != "class_interface_clause" {
                continue;
            }
            let mut inner = child.walk();
            for interface in child.named_children(&mut inner) {
                let spelled = self.text(interface).trim_start_matches('\\');
                out.insert(
                    spelled
                        .rsplit('\\')
                        .next()
                        .unwrap_or(spelled)
                        .to_lowercase(),
                );
            }
        }
        out
    }

    /// Stamp a method the language calls without a call that spells it.
    fn stamp_protocol(&mut self, method: Node<'_>) {
        let Some(name_node) = method.child_by_field_name("name") else {
            return;
        };
        let name = self.text(name_node).to_string();
        let lower = name.to_lowercase();
        let declares = |interface: &str| {
            self.classes
                .last()
                .is_some_and(|class| class.interfaces.contains(interface))
        };
        let words = match lower.as_str() {
            "__destruct" => Some("destruction"),
            "__call" | "__callstatic" => Some("a call of an undeclared method"),
            "__get" | "__set" | "__isset" | "__unset" => Some("property access"),
            "__tostring" => Some("conversion to a string"),
            "__invoke" => Some("a call of an instance"),
            "__clone" => Some("clone"),
            "__serialize" | "__unserialize" | "__sleep" | "__wakeup" | "__set_state" => {
                Some("serialization")
            }
            "__debuginfo" => Some("var_dump"),
            "offsetget" | "offsetset" | "offsetexists" | "offsetunset"
                if declares("arrayaccess") =>
            {
                Some("subscription")
            }
            "current" | "key" | "next" | "rewind" | "valid" if declares("iterator") => {
                Some("iteration")
            }
            "getiterator" if declares("iteratoraggregate") => Some("iteration"),
            "count" if declares("countable") => Some("count()"),
            "jsonserialize" if declares("jsonserializable") => Some("json_encode"),
            _ => None,
        };
        if let Some(words) = words {
            self.protocol_members.push(ProtocolStamp {
                name,
                at: name_node.start_byte(),
                member: ProtocolMember::Implicit(words.to_string()),
            });
        }
    }
}

/// Where a value goes, with the names it is aliased under.
struct Flow(OccurrenceContext, Vec<String>);

/// The arguments of a call or creation, in order.
fn arguments_of(call: Node<'_>) -> Vec<Node<'_>> {
    let arguments = call.child_by_field_name("arguments").or_else(|| {
        let mut cursor = call.walk();
        let found = call
            .named_children(&mut cursor)
            .find(|child| child.kind() == "arguments");
        found
    });
    let Some(arguments) = arguments else {
        return Vec::new();
    };
    let mut cursor = arguments.walk();
    let found = arguments
        .named_children(&mut cursor)
        .filter(|argument| argument.kind() == "argument")
        .collect();
    found
}

/// An argument's value, past a named argument's label.
fn argument_value(argument: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = argument.walk();
    let found = argument
        .named_children(&mut cursor)
        .filter(|child| child.kind() != "name" || argument.child_by_field_name("name").is_none())
        .last();
    found
}

/// Whether a call is first-class callable syntax, `f(...)`.
fn is_first_class_callable(call: Node<'_>) -> bool {
    crate::value_origins::php::is_first_class_callable(call)
}

/// A string literal's text, when it holds no interpolation or escape.
fn string_literal(node: Node<'_>, source: &[u8]) -> Option<String> {
    if !matches!(node.kind(), "string" | "encapsed_string") {
        return None;
    }
    let mut cursor = node.walk();
    let parts: Vec<Node<'_>> = node.named_children(&mut cursor).collect();
    if parts.iter().any(|part| part.kind() != "string_content") {
        return None;
    }
    let text: String = parts.iter().map(|part| text_of(*part, source)).collect();
    Some(text.replace("\\\\", "\\"))
}

#[cfg(test)]
mod tests;
