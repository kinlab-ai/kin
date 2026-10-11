// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Swift's facts for the value-escape record (see [`crate::value_escape`]).
//!
//! Scoping follows Swift: a parameter or local is visible from its
//! declaration to the end of its block, an `if let` binding in its branch, a
//! closure parameter in its closure, and a read of one is never recorded.
//! Inside a type's member an unqualified name is a member of `self` before it
//! is anything outside the type, so it is read as the type's member when a
//! type body or extension in this file declares it, and otherwise as a member
//! of the type (one another file's extension or a superclass may declare) or
//! a module-level name, which a member read with an unknown receiver covers.
//! A top-level name is read as this file's own declaration when the file
//! declares it `private` or `fileprivate`, and as free otherwise, since every
//! file of the module sees an internal declaration.
//!
//! Recorded:
//!
//! - every read of a name, and every member read off a value, whose value
//!   flows on: an argument (a completion handler, a target-action pair), an
//!   assignment (into a member, which aliases it under the member name), a
//!   return, an array or dictionary element, an unapplied method reference
//!   (`let f = store.save`, `Store.save`), a `#selector`, a key path
//!   (`\Store.name`, `\.date`); never a callee, a declaration, a receiver or
//!   an inert use (a condition, a comparison, interpolation);
//! - reflective sites: a selector, a key-value coding key or a class looked
//!   up by a computed string (a literal names the member and is recorded as a
//!   use of it), and `Mirror(reflecting:)`;
//! - protocol sites: attributes that name a type (property wrappers, result
//!   builders, global actors), which run that type's members;
//! - protocol members: operators, `callAsFunction`, `deinit`, coding,
//!   hashing, iteration and literal-conversion members, and overrides, which a
//!   call through a superclass reaches without naming the subclass.

use std::collections::HashMap;

use kin_model::{
    FileEscapeRecord, ModuleScope, OccurrenceBinding, OccurrenceContext, ProtocolForm,
    ProtocolMember, ProtocolSite, ReceiverKind, ReflectiveForm, ReflectiveReach, ReflectiveSite,
    ValueOccurrence, EXCERPT_MAX, RECEIVER_TEXT_MAX,
};
use tree_sitter::{Node, Tree};

use crate::languages::swift_nested::nested_function_scopes;
use crate::value_escape::{
    collapsed_text, is_name, line_of, text_of, unparsed_lines, FileEscapeCensus, ProtocolStamp,
};
use crate::value_origins::swift::{local_declarations, type_body, type_declaration_name};

/// Attributes the compiler reads itself, which run no type's members.
const COMPILER_ATTRIBUTES: &[&str] = &[
    "MainActor",
    "Sendable",
    "UIApplicationMain",
    "NSApplicationMain",
    "NSManaged",
    "IBAction",
    "IBOutlet",
    "IBInspectable",
    "IBDesignable",
    "IBSegueAction",
    "GKInspectable",
    "NSCopying",
    "Test",
];

/// Initializer labels the compiler calls for a literal.
const LITERAL_LABELS: &[&str] = &[
    "stringLiteral",
    "integerLiteral",
    "floatLiteral",
    "booleanLiteral",
    "arrayLiteral",
    "dictionaryLiteral",
    "nilLiteral",
    "unicodeScalarLiteral",
    "extendedGraphemeClusterLiteral",
    "stringInterpolation",
];

/// Take the census of one Swift file.
pub(crate) fn census(tree: &Tree, source: &[u8]) -> FileEscapeCensus {
    let root = tree.root_node();
    let mut walker = Walker {
        source,
        record: FileEscapeRecord::new(ModuleScope::Closed),
        protocol_members: Vec::new(),
        file: FileNames::read(root, source),
        scopes: Vec::new(),
    };
    walker.record.unparsed = unparsed_lines(tree, source);
    walker.visit(root);
    FileEscapeCensus {
        record: walker.record,
        protocol_members: walker.protocol_members,
        non_enumerable: Vec::new(),
    }
}

/// What the file declares at its top level and in its type bodies.
#[derive(Default)]
struct FileNames {
    /// Top-level declarations by name: lines, and whether any is private to
    /// the file.
    module: HashMap<String, (Vec<u32>, bool)>,
    /// Members each type's bodies and extensions in this file declare.
    members: HashMap<String, HashMap<String, Vec<u32>>>,
}

impl FileNames {
    fn read(root: Node<'_>, source: &[u8]) -> Self {
        let mut names = Self::default();
        let mut cursor = root.walk();
        for child in root.children(&mut cursor) {
            match child.kind() {
                "function_declaration" | "property_declaration" => {
                    if let Some(name) = declared_name(child, source) {
                        let private = is_file_private(child, source);
                        let entry = names.module.entry(name).or_default();
                        entry.0.push(line_of(child));
                        entry.1 |= private;
                    }
                }
                "class_declaration" | "protocol_declaration" => names.read_type(child, source),
                _ => {}
            }
        }
        names
    }

    fn read_type(&mut self, node: Node<'_>, source: &[u8]) {
        let (Some(name), Some(body)) = (type_declaration_name(node, source), type_body(node))
        else {
            return;
        };
        let mut cursor = body.walk();
        for member in body.children(&mut cursor) {
            match member.kind() {
                "function_declaration"
                | "property_declaration"
                | "protocol_function_declaration"
                | "protocol_property_declaration" => {
                    if let Some(member_name) = declared_name(member, source) {
                        self.members
                            .entry(name.clone())
                            .or_default()
                            .entry(member_name)
                            .or_default()
                            .push(line_of(member));
                    }
                }
                "class_declaration" | "protocol_declaration" => self.read_type(member, source),
                _ => {}
            }
        }
    }
}

/// The name a function or property declaration declares.
fn declared_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    let name = node.child_by_field_name("name")?;
    let name = match name.kind() {
        "pattern" => name.child_by_field_name("bound_identifier")?,
        _ => name,
    };
    let text = text_of(name, source).trim();
    (!text.is_empty()).then(|| text.to_string())
}

fn is_file_private(node: Node<'_>, source: &[u8]) -> bool {
    let mut cursor = node.walk();
    let found = node.children(&mut cursor).any(|child| {
        child.kind() == "modifiers"
            && text_of(child, source)
                .split_whitespace()
                .any(|word| word == "private" || word == "fileprivate")
    });
    found
}

/// A callable or type the walk is inside.
enum Scope {
    Type(String),
    /// A callable's locals, and the functions nested in it that a read of
    /// their name reaches: (name, visible from, visible until, line).
    Callable(
        Vec<(String, usize, usize)>,
        Vec<(String, usize, usize, u32)>,
    ),
}

/// Where the value a read holds goes.
struct Flow(OccurrenceContext, Vec<String>);

struct Walker<'s> {
    source: &'s [u8],
    record: FileEscapeRecord,
    protocol_members: Vec<ProtocolStamp>,
    file: FileNames,
    scopes: Vec<Scope>,
}

impl<'s> Walker<'s> {
    fn text(&self, node: Node<'_>) -> &'s str {
        text_of(node, self.source)
    }

    fn visit(&mut self, node: Node<'_>) {
        match node.kind() {
            "class_declaration" | "protocol_declaration" => {
                let name = type_declaration_name(node, self.source);
                self.stamp_attributes(node);
                match name {
                    Some(name) => {
                        self.scopes.push(Scope::Type(name));
                        self.visit_children(node);
                        self.scopes.pop();
                    }
                    None => self.visit_children(node),
                }
                return;
            }
            "function_declaration"
            | "init_declaration"
            | "deinit_declaration"
            | "subscript_declaration"
            | "computed_property" => {
                self.stamp_protocol(node);
                self.stamp_attributes(node);
                let locals = local_declarations(node, self.source);
                let nested = nested_function_scopes(node, self.source);
                self.scopes.push(Scope::Callable(locals, nested));
                self.visit_children(node);
                self.scopes.pop();
                return;
            }
            "lambda_literal" if !self.in_callable() => {
                let locals = local_declarations(node, self.source);
                self.scopes.push(Scope::Callable(locals, Vec::new()));
                self.visit_children(node);
                self.scopes.pop();
                return;
            }
            "property_declaration" => self.stamp_attributes(node),
            "selector_expression" => {
                self.read_selector(node);
                return;
            }
            "simple_identifier" => {
                self.read_name(node);
                return;
            }
            "navigation_expression" => {
                if self.read_member(node) {
                    // The target is still read: `\Store.name` has none,
                    // `store.save` reads `store` as a receiver.
                }
            }
            "call_expression" => self.read_call(node),
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

    fn in_callable(&self) -> bool {
        self.scopes
            .iter()
            .any(|scope| matches!(scope, Scope::Callable(..)))
    }

    fn is_local(&self, name: &str, at: usize) -> bool {
        self.scopes.iter().any(|scope| match scope {
            Scope::Callable(locals, _) => locals
                .iter()
                .any(|(local, from, until)| local == name && *from <= at && at < *until),
            Scope::Type(_) => false,
        })
    }

    fn enclosing_type(&self) -> Option<&str> {
        self.scopes.iter().rev().find_map(|scope| match scope {
            Scope::Type(name) => Some(name.as_str()),
            Scope::Callable(..) => None,
        })
    }

    // ── Reads ─────────────────────────────────────────────────────────────

    fn read_name(&mut self, node: Node<'_>) {
        let Some(parent) = node.parent() else {
            return;
        };
        // Member names, labels and declared names are not reads.
        if matches!(
            parent.kind(),
            "navigation_suffix"
                | "value_argument_label"
                | "pattern"
                | "parameter"
                | "lambda_parameter"
                | "function_declaration"
                | "import_declaration"
                | "identifier"
                | "selector_expression"
                | "protocol_function_declaration"
                | "protocol_property_declaration"
                | "enum_entry"
                | "typealias_declaration"
                | "associatedtype_declaration"
                | "class_declaration"
                | "protocol_declaration"
                | "subscript_declaration"
                | "init_declaration"
                | "operator_declaration"
                | "precedence_group_declaration"
        ) {
            return;
        }
        let Some(Flow(context, aliases)) = self.context_of(node) else {
            return;
        };
        let name = self.text(node).to_string();
        let at = node.start_byte();
        let nested = self.scopes.iter().rev().find_map(|scope| match scope {
            Scope::Callable(_, functions) => functions
                .iter()
                .find(|(function, from, until, _)| *function == name && *from <= at && at < *until)
                .map(|(_, _, _, line)| *line),
            Scope::Type(_) => None,
        });
        if let Some(line) = nested {
            // A function nested in this one, which the adapter admits.
            self.push(
                node,
                &name,
                context,
                aliases,
                OccurrenceBinding::Local,
                None,
                None,
                Some(line),
            );
            return;
        }
        if name.is_empty() || name == "self" || self.is_local(&name, at) {
            return;
        }
        let (binding, receiver, kind, target) = self.plain_binding(&name);
        self.push(
            node, &name, context, aliases, binding, receiver, kind, target,
        );
    }

    fn plain_binding(
        &self,
        name: &str,
    ) -> (
        OccurrenceBinding,
        Option<String>,
        Option<ReceiverKind>,
        Option<u32>,
    ) {
        if let Some(owner) = self.enclosing_type() {
            if let Some(lines) = self
                .file
                .members
                .get(owner)
                .and_then(|members| members.get(name))
            {
                let target = (lines.len() == 1).then(|| lines[0]);
                return (OccurrenceBinding::Class, None, None, target);
            }
            return (
                OccurrenceBinding::Member,
                Some(owner.to_string()),
                Some(ReceiverKind::Other),
                None,
            );
        }
        match self.file.module.get(name) {
            Some((lines, true)) => {
                let target = (lines.len() == 1).then(|| lines[0]);
                (OccurrenceBinding::Module, None, None, target)
            }
            _ => (OccurrenceBinding::Free, None, None, None),
        }
    }

    /// A member read off a value that is not called. Returns whether it
    /// recorded one.
    fn read_member(&mut self, node: Node<'_>) -> bool {
        let Some(member) = node
            .child_by_field_name("suffix")
            .and_then(|suffix| suffix.child_by_field_name("suffix"))
        else {
            return false;
        };
        let name = self.text(member).to_string();
        if !is_name(&name) {
            return false;
        }
        let Some(Flow(context, aliases)) = self.context_of(node) else {
            return false;
        };
        let target = node.child_by_field_name("target");
        let (receiver, kind) = match target {
            Some(target) if target.kind() == "self_expression" => {
                (Some("self".to_string()), ReceiverKind::Instance)
            }
            Some(target) if target.kind() == "super_expression" => {
                (Some("super".to_string()), ReceiverKind::Instance)
            }
            Some(target) if target.kind() == "key_path_expression" => {
                let root = self.text(target).trim_start_matches('\\').trim();
                (
                    (!root.is_empty()).then(|| root.chars().take(RECEIVER_TEXT_MAX).collect()),
                    ReceiverKind::Other,
                )
            }
            Some(target) => (
                Some(collapsed_text(target, self.source, RECEIVER_TEXT_MAX)),
                ReceiverKind::Other,
            ),
            None => (None, ReceiverKind::Other),
        };
        self.push(
            member,
            &name,
            context,
            aliases,
            OccurrenceBinding::Member,
            receiver,
            Some(kind),
            None,
        );
        true
    }

    /// `#selector(refresh)`, `#selector(Store.save)`: a use of the member,
    /// handed to the Objective-C runtime.
    fn read_selector(&mut self, node: Node<'_>) {
        let mut stack = vec![node];
        let mut last = None;
        while let Some(current) = stack.pop() {
            if current.kind() == "simple_identifier" {
                last = Some(current);
            }
            let mut cursor = current.walk();
            stack.extend(current.children(&mut cursor));
        }
        let Some(name_node) = last else {
            return;
        };
        let spelled: String = self.text(node).split_whitespace().collect();
        let inner = spelled
            .trim_start_matches("#selector(")
            .trim_end_matches(')')
            .to_string();
        let inner = inner
            .trim_start_matches("getter:")
            .trim_start_matches("setter:")
            .to_string();
        let path = inner.split('(').next().unwrap_or(&inner).to_string();
        let (receiver, leaf) = match path.rsplit_once('.') {
            Some((receiver, leaf)) => (Some(receiver.to_string()), leaf.to_string()),
            None => (None, path.clone()),
        };
        let leaf = if leaf.is_empty() {
            self.text(name_node).to_string()
        } else {
            leaf
        };
        if !is_name(&leaf) {
            return;
        }
        let receiver = receiver.or_else(|| self.enclosing_type().map(str::to_string));
        self.push(
            name_node,
            &leaf,
            OccurrenceContext::Argument,
            Vec::new(),
            OccurrenceBinding::Member,
            receiver,
            Some(ReceiverKind::Other),
            None,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn push(
        &mut self,
        node: Node<'_>,
        name: &str,
        context: OccurrenceContext,
        aliases: Vec<String>,
        binding: OccurrenceBinding,
        receiver: Option<String>,
        receiver_kind: Option<ReceiverKind>,
        target_line: Option<u32>,
    ) {
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
                target_line,
                form: None,
            });
    }

    // ── Where a value goes ────────────────────────────────────────────────

    fn context_of(&self, node: Node<'_>) -> Option<Flow> {
        let mut child = node;
        loop {
            let parent = child.parent()?;
            match parent.kind() {
                "try_expression"
                | "await_expression"
                | "as_expression"
                | "parenthesized_expression"
                | "tuple_expression"
                | "force_unwrap_expression"
                | "postfix_expression" => {}
                "ternary_expression" => {
                    if parent
                        .named_child(0)
                        .is_some_and(|first| first.id() == child.id())
                    {
                        return None;
                    }
                }
                "value_argument" => {
                    return Some(Flow(OccurrenceContext::Argument, Vec::new()));
                }
                "array_literal" | "dictionary_literal" => {
                    return Some(Flow(OccurrenceContext::Collection, Vec::new()));
                }
                "assignment" => {
                    let target = parent.child_by_field_name("target");
                    if target.is_some_and(|target| target.id() == child.id()) {
                        return None;
                    }
                    return Some(self.assignment_flow(parent));
                }
                "property_declaration" => {
                    if parent
                        .child_by_field_name("value")
                        .is_none_or(|value| value.id() != child.id())
                    {
                        return None;
                    }
                    if self.in_callable() {
                        return Some(Flow(OccurrenceContext::Assignment, Vec::new()));
                    }
                    let names = declared_name(parent, self.source).into_iter().collect();
                    return Some(Flow(OccurrenceContext::Alias, names));
                }
                "control_transfer_statement" => {
                    let keyword = self.text(parent).split_whitespace().next().unwrap_or("");
                    return match keyword {
                        "return" => Some(Flow(OccurrenceContext::Return, Vec::new())),
                        "throw" => Some(Flow(OccurrenceContext::Other, Vec::new())),
                        _ => None,
                    };
                }
                "statements" => {
                    // A closure or function whose body is this one
                    // expression returns it.
                    let single = parent.named_child_count() == 1;
                    let returns = parent.parent().is_some_and(|holder| {
                        matches!(
                            holder.kind(),
                            "lambda_literal" | "computed_property" | "function_body"
                        )
                    });
                    return (single && returns)
                        .then(|| Flow(OccurrenceContext::Return, Vec::new()));
                }
                "call_expression"
                | "call_suffix"
                | "navigation_expression"
                | "navigation_suffix"
                | "if_statement"
                | "guard_statement"
                | "while_statement"
                | "repeat_while_statement"
                | "for_statement"
                | "switch_statement"
                | "switch_entry"
                | "switch_pattern"
                | "comparison_expression"
                | "equality_expression"
                | "conjunction_expression"
                | "disjunction_expression"
                | "additive_expression"
                | "multiplicative_expression"
                | "prefix_expression"
                | "range_expression"
                | "nil_coalescing_expression"
                | "check_expression"
                | "infix_expression"
                | "bitwise_operation"
                | "line_string_literal"
                | "multi_line_string_literal"
                | "interpolated_expression"
                | "directly_assignable_expression"
                | "key_path_expression"
                | "selector_expression"
                | "pattern"
                | "value_binding_pattern"
                | "type_annotation"
                | "user_type"
                | "class_body"
                | "source_file"
                | "function_body"
                | "attribute"
                | "modifiers"
                | "enum_entry"
                | "catch_block"
                | "do_statement"
                | "defer_statement"
                | "availability_condition"
                | "inheritance_specifier"
                | "import_declaration"
                | "ERROR" => return None,
                _ => return Some(Flow(OccurrenceContext::Other, Vec::new())),
            }
            child = parent;
        }
    }

    fn assignment_flow(&self, assignment: Node<'_>) -> Flow {
        let target = assignment.child_by_field_name("target").map(|target| {
            if target.kind() == "directly_assignable_expression" {
                target.named_child(0).unwrap_or(target)
            } else {
                target
            }
        });
        match target {
            Some(target) if target.kind() == "navigation_expression" => {
                let member = target
                    .child_by_field_name("suffix")
                    .and_then(|suffix| suffix.child_by_field_name("suffix"))
                    .map(|member| self.text(member).to_string());
                match member {
                    Some(member) => Flow(OccurrenceContext::Alias, vec![member]),
                    None => Flow(OccurrenceContext::Other, Vec::new()),
                }
            }
            Some(target) if target.kind() == "simple_identifier" => {
                let name = self.text(target);
                if self.is_local(name, target.start_byte()) {
                    Flow(OccurrenceContext::Assignment, Vec::new())
                } else {
                    Flow(OccurrenceContext::Alias, vec![name.to_string()])
                }
            }
            Some(target) if target.kind() == "call_expression" => {
                // `table[key] = handler`: a subscript store.
                Flow(OccurrenceContext::Collection, Vec::new())
            }
            _ => Flow(OccurrenceContext::Assignment, Vec::new()),
        }
    }

    // ── Reflective sites ──────────────────────────────────────────────────

    fn read_call(&mut self, call: Node<'_>) {
        let Some(callee) = call.named_child(0) else {
            return;
        };
        let (name, receiver) = match callee.kind() {
            "simple_identifier" => (self.text(callee).to_string(), None),
            "navigation_expression" => {
                let member = callee
                    .child_by_field_name("suffix")
                    .and_then(|suffix| suffix.child_by_field_name("suffix"))
                    .map(|member| self.text(member).to_string())
                    .unwrap_or_default();
                (member, callee.child_by_field_name("target"))
            }
            _ => return,
        };
        let arguments = arguments_of(call);
        let first = arguments.first().copied();
        let labelled = |label: &str| -> Option<Node<'_>> {
            arguments.iter().copied().find(|argument| {
                argument
                    .child_by_field_name("name")
                    .is_some_and(|name| self.text(name).trim() == label)
            })
        };
        match name.as_str() {
            "Selector" | "NSSelectorFromString" => {
                self.key_site(
                    call,
                    first,
                    None,
                    ReflectiveForm::ComputedMember,
                    ReflectiveReach::Members,
                    true,
                );
            }
            "NSClassFromString" => {
                self.key_site(
                    call,
                    first,
                    None,
                    ReflectiveForm::ComputedMember,
                    ReflectiveReach::Any,
                    false,
                );
            }
            "value" | "setValue" | "mutableArrayValue" => {
                let key = labelled("forKey").or_else(|| labelled("forKeyPath"));
                if key.is_some() {
                    self.key_site(
                        call,
                        key,
                        receiver,
                        ReflectiveForm::ComputedMember,
                        ReflectiveReach::Members,
                        true,
                    );
                }
            }
            "Mirror" if labelled("reflecting").is_some() => {
                self.push_reflective(call, ReflectiveForm::Enumerate, ReflectiveReach::Members);
            }
            _ => {}
        }
    }

    /// A lookup by a string: a literal names the member it reaches and is a
    /// use of it; a computed one is a reflective site.
    fn key_site(
        &mut self,
        call: Node<'_>,
        argument: Option<Node<'_>>,
        receiver: Option<Node<'_>>,
        form: ReflectiveForm,
        reach: ReflectiveReach,
        member: bool,
    ) {
        let value = argument.and_then(|argument| argument.child_by_field_name("value"));
        match value.and_then(|value| string_literal(value, self.source).map(|text| (value, text))) {
            Some((value, text)) => {
                // `refresh:`, `save(_:)`, `user.name`: the member leaf.
                let leaf = text
                    .split(['(', ':'])
                    .next()
                    .unwrap_or("")
                    .rsplit('.')
                    .next()
                    .unwrap_or("")
                    .to_string();
                if !is_name(&leaf) {
                    return;
                }
                let receiver = receiver
                    .map(|receiver| collapsed_text(receiver, self.source, RECEIVER_TEXT_MAX));
                self.push(
                    value,
                    &leaf,
                    OccurrenceContext::Argument,
                    Vec::new(),
                    if member {
                        OccurrenceBinding::Member
                    } else {
                        OccurrenceBinding::Free
                    },
                    receiver,
                    member.then_some(ReceiverKind::Other),
                    None,
                );
            }
            None => self.push_reflective(call, form, reach),
        }
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

    /// An attribute naming a type (a property wrapper, a result builder, a
    /// global actor) runs that type's members.
    fn stamp_attributes(&mut self, node: Node<'_>) {
        let mut cursor = node.walk();
        let modifiers: Vec<Node<'_>> = node
            .children(&mut cursor)
            .filter(|child| child.kind() == "modifiers")
            .collect();
        for modifier in modifiers {
            let mut inner = modifier.walk();
            let attributes: Vec<Node<'_>> = modifier
                .children(&mut inner)
                .filter(|child| child.kind() == "attribute")
                .collect();
            for attribute in attributes {
                let Some(ty) = attribute.named_child(0) else {
                    continue;
                };
                let name = self.text(ty).rsplit('.').next().unwrap_or("").to_string();
                let named_type = name
                    .chars()
                    .next()
                    .is_some_and(|first| first.is_ascii_uppercase());
                if named_type && is_name(&name) && !COMPILER_ATTRIBUTES.contains(&name.as_str()) {
                    self.record.protocol.push(ProtocolSite {
                        line: line_of(attribute),
                        form: ProtocolForm::Decorator,
                        name: Some(name),
                    });
                }
            }
        }
    }

    /// Stamp a member the language runs without a call that spells it.
    fn stamp_protocol(&mut self, node: Node<'_>) {
        let (name, at) = match node.kind() {
            "deinit_declaration" => ("deinit".to_string(), node.start_byte()),
            "init_declaration" => ("init".to_string(), node.start_byte()),
            "function_declaration" => match node.child_by_field_name("name") {
                Some(name) => (self.text(name).to_string(), name.start_byte()),
                None => return,
            },
            _ => return,
        };
        let labels = self.parameter_labels(node);
        let words = match name.as_str() {
            "deinit" => Some("deallocation"),
            "callAsFunction" => Some("a call of an instance"),
            "hash" if labels.first().map(String::as_str) == Some("into") => Some("hashing"),
            "encode" if labels.first().map(String::as_str) == Some("to") => Some("encoding"),
            "makeIterator" => Some("iteration"),
            "init" => match labels.first().map(String::as_str) {
                Some("from") if labels.len() == 1 => Some("decoding"),
                Some(label) if LITERAL_LABELS.contains(&label) => Some("literal conversion"),
                _ => None,
            },
            _ if !is_name(&name) => Some("operators"),
            _ if self.has_modifier(node, "override") => {
                Some("dispatch through a call on the superclass")
            }
            _ => None,
        };
        if let Some(words) = words {
            self.protocol_members.push(ProtocolStamp {
                name,
                at,
                member: ProtocolMember::Implicit(words.to_string()),
            });
        }
    }

    /// Each parameter's external label, or its name when it has none.
    fn parameter_labels(&self, node: Node<'_>) -> Vec<String> {
        let mut cursor = node.walk();
        let parameters: Vec<Node<'_>> = node
            .children(&mut cursor)
            .filter(|child| child.kind() == "parameter")
            .collect();
        parameters
            .into_iter()
            .filter_map(|parameter| {
                let label = parameter
                    .child_by_field_name("external_name")
                    .or_else(|| parameter.child_by_field_name("name"))?;
                Some(self.text(label).to_string())
            })
            .collect()
    }

    fn has_modifier(&self, node: Node<'_>, word: &str) -> bool {
        let mut cursor = node.walk();
        let found = node.children(&mut cursor).any(|child| {
            child.kind() == "modifiers"
                && self
                    .text(child)
                    .split_whitespace()
                    .any(|modifier| modifier == word)
        });
        found
    }
}

/// The arguments of a call.
fn arguments_of(call: Node<'_>) -> Vec<Node<'_>> {
    let mut out = Vec::new();
    let mut cursor = call.walk();
    for child in call.children(&mut cursor) {
        if child.kind() != "call_suffix" {
            continue;
        }
        let mut inner = child.walk();
        for part in child.children(&mut inner) {
            if part.kind() == "value_arguments" {
                let mut arguments = part.walk();
                out.extend(
                    part.children(&mut arguments)
                        .filter(|argument| argument.kind() == "value_argument"),
                );
            }
        }
    }
    out
}

/// A string literal with no interpolation or escape.
fn string_literal(node: Node<'_>, source: &[u8]) -> Option<String> {
    if node.kind() != "line_string_literal" {
        return None;
    }
    let mut cursor = node.walk();
    let parts: Vec<Node<'_>> = node.named_children(&mut cursor).collect();
    if parts.iter().any(|part| part.kind() != "line_str_text") {
        return None;
    }
    let text: String = parts.iter().map(|part| text_of(*part, source)).collect();
    (!text.contains('\\')).then_some(text)
}

#[cfg(test)]
mod tests;
