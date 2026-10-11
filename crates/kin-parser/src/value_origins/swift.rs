// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Swift value origins: declared parameter, local and property types,
//! initializer calls, method calls on typed values, and declared returns.
//!
//! Swift fixes a name's static type where it is declared: an annotation
//! (`store: Store`, `let s: Store`), or the type of its initial value (`let s
//! = Store()`, `let r = registry.lookup()`). Assigning to a `var` later
//! cannot change that type, so, as in C++, only declarations matter. A name
//! is recorded for a callable when every declaration of it anywhere in the
//! callable's subtree (parameters, `let` and `var`, `if let`, `guard let`,
//! `for` and `case` patterns, closure parameters, nested functions) records
//! the same origin, and every read of it sits inside one of those
//! declarations' scopes. A name the callable reads and never declares is the
//! enclosing type's instance property of that name when a type body or
//! extension in this file declares one, because Swift finds a member of
//! `self` before anything outside the type.
//!
//! An optional of one type is that type: a call through `store?` reaches the
//! same method. A type the standard library or Foundation declares is no
//! evidence for a repository class and is left out, and so is a generic, a
//! tuple, a function type and an existential.
//!
//! The same reading types receivers inside the adapter
//! ([`SwiftScopeReader`]): a call through a receiver with a fact the linker
//! can follow carries its receiver and the bare method for the linker's
//! value-origin tier, which asks the class the chain reaches for the method.

use std::collections::{BTreeMap, HashMap};

use tree_sitter::{Node, Tree};

use super::{DeclaredReturn, FileFacts, ScopeFacts, ValueOrigin};

/// Type names the Swift standard library and Foundation declare. A value of
/// one is no instance of a repository class.
const SYSTEM_TYPES: &[&str] = &[
    "Any",
    "AnyObject",
    "AnyHashable",
    "Array",
    "Bool",
    "Character",
    "Data",
    "Date",
    "Decimal",
    "Dictionary",
    "Double",
    "Error",
    "Float",
    "Int",
    "Int8",
    "Int16",
    "Int32",
    "Int64",
    "Never",
    "NSObject",
    "Optional",
    "Result",
    "Set",
    "String",
    "Substring",
    "UInt",
    "UInt8",
    "UInt16",
    "UInt32",
    "UInt64",
    "URL",
    "URLRequest",
    "UUID",
    "Void",
    "JSONDecoder",
    "JSONEncoder",
    "NotificationCenter",
    "DispatchQueue",
    "FileManager",
    "UserDefaults",
    "Bundle",
    "Task",
    "Self",
];

pub(super) fn read_file(tree: &Tree, source: &[u8], _file_path: &str) -> FileFacts {
    let root = tree.root_node();
    let file = SwiftFileTypes::read(root, source);
    let mut facts = FileFacts::default();
    walk(root, source, None, &file, &mut facts);
    facts
}

/// Walk the declarations the adapter admits, in its shape, and record each
/// callable's facts under the node its entity spans.
fn walk(
    node: Node<'_>,
    source: &[u8],
    owner: Option<&str>,
    file: &SwiftFileTypes,
    facts: &mut FileFacts,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "class_declaration" | "protocol_declaration" => {
                let Some(name) = type_declaration_name(child, source) else {
                    continue;
                };
                if let Some(body) = type_body(child) {
                    walk(body, source, Some(&name), file, facts);
                }
            }
            "function_declaration" | "protocol_function_declaration" | "init_declaration" => {
                let Some(leaf) = callable_leaf(child, source) else {
                    continue;
                };
                let reader = SwiftScopeReader::for_callable(child, source, owner, file);
                let scope = ScopeFacts {
                    bindings: reader.bindings.clone(),
                    returns: if child.kind() == "init_declaration" {
                        None
                    } else {
                        declared_return(child, source)
                    },
                    ..ScopeFacts::default()
                };
                facts
                    .by_node
                    .push((vec![(child.start_byte(), child.end_byte())], leaf, scope));
            }
            _ => {}
        }
    }
}

/// The leaf name the adapter gives a callable's entity.
fn callable_leaf(node: Node<'_>, source: &[u8]) -> Option<String> {
    if node.kind() == "init_declaration" {
        return Some("init".to_string());
    }
    let name = node.child_by_field_name("name")?;
    let text = text(name, source).trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// A class, struct, enum, actor, extension or protocol's name as the adapter
/// names its members' owner.
pub(crate) fn type_declaration_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    let name = node.child_by_field_name("name")?;
    let text = match name.kind() {
        "type_identifier" | "simple_identifier" => text(name, source).to_string(),
        _ => {
            let mut cursor = name.walk();
            let found = name
                .children(&mut cursor)
                .find(|child| matches!(child.kind(), "type_identifier" | "simple_identifier"))
                .map(|child| text(child, source).to_string());
            found?
        }
    };
    (!text.is_empty()).then_some(text)
}

pub(crate) fn type_body(node: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = node.walk();
    let found = node.children(&mut cursor).find(|child| {
        matches!(
            child.kind(),
            "class_body" | "enum_class_body" | "protocol_body"
        )
    });
    found
}

// ── Types ─────────────────────────────────────────────────────────────────

/// The one class-like type a type node names, as Kin names types: by leaf.
/// An optional or implicitly unwrapped optional of one type is that type.
pub(crate) fn named_type(node: Node<'_>, source: &[u8]) -> Option<String> {
    match node.kind() {
        "type_annotation" => named_type(node.child_by_field_name("name")?, source),
        "optional_type" => named_type(node.child_by_field_name("wrapped")?, source),
        "user_type" => {
            // One identifier and no type arguments: `Store`. A nested
            // `Outer.Inner` names the inner type, which Kin names by leaf.
            let mut cursor = node.walk();
            let parts: Vec<Node<'_>> = node.named_children(&mut cursor).collect();
            if parts.iter().any(|part| part.kind() == "type_arguments") {
                return None;
            }
            let leaf = parts
                .last()
                .filter(|part| part.kind() == "type_identifier")?;
            let name = text(*leaf, source);
            (!name.is_empty() && !SYSTEM_TYPES.contains(&name)).then(|| name.to_string())
        }
        "type_identifier" => {
            let name = text(node, source);
            (!name.is_empty() && !SYSTEM_TYPES.contains(&name)).then(|| name.to_string())
        }
        _ => {
            // `Store!` parses as the type followed by a bang in some grammar
            // versions; anything else (tuples, functions, arrays,
            // dictionaries, existentials) names no one class.
            None
        }
    }
}

/// The return type a function declares: the type after `->`.
fn return_type_node(function: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = function.walk();
    let mut after_arrow = false;
    for child in function.children(&mut cursor) {
        if child.kind() == "->" {
            after_arrow = true;
            continue;
        }
        if after_arrow && child.is_named() {
            return Some(child);
        }
    }
    None
}

/// A callable's declared return: one named type, or `Self` for the
/// receiver's own type.
pub(crate) fn declared_return(function: Node<'_>, source: &[u8]) -> Option<DeclaredReturn> {
    let ty = return_type_node(function)?;
    let spelled: String = text(ty, source).split_whitespace().collect();
    if spelled == "Self" || spelled == "Self?" {
        return Some(DeclaredReturn::Receiver);
    }
    named_type(ty, source).map(|type_name| DeclaredReturn::Type { type_name })
}

// ── What the whole file declares ──────────────────────────────────────────

/// A declared return every declaration of one name in the file agrees on.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Returned {
    Class(String),
    Mixed,
}

fn merge(slot: &mut Option<Returned>, next: Option<String>) {
    let next = next.map_or(Returned::Mixed, Returned::Class);
    match slot {
        None => *slot = Some(next),
        Some(existing) if *existing == next => {}
        Some(existing) => *existing = Returned::Mixed,
    }
}

fn agreed(slot: Option<&Option<Returned>>) -> Option<&str> {
    match slot?.as_ref()? {
        Returned::Class(class) => Some(class.as_str()),
        Returned::Mixed => None,
    }
}

/// What one Swift file declares, which is what the adapter can settle a
/// receiver with before the linker sees the rest of the repository.
#[derive(Debug, Default)]
pub(crate) struct SwiftFileTypes {
    /// Every type the file declares a body for (class, struct, enum, actor,
    /// protocol), extensions aside.
    types: HashMap<String, usize>,
    /// Instance properties by type, across the type's body and its
    /// extensions in this file, with the origin their declaration gives.
    properties: HashMap<String, HashMap<String, Option<ValueOrigin>>>,
    /// Free functions by name.
    functions: HashMap<String, Option<Returned>>,
    /// Methods by (type, name).
    methods: HashMap<(String, String), Option<Returned>>,
}

impl SwiftFileTypes {
    pub(crate) fn read(root: Node<'_>, source: &[u8]) -> Self {
        let mut types = Self::default();
        types.read_scope(root, source, None);
        types
    }

    fn read_scope(&mut self, node: Node<'_>, source: &[u8], owner: Option<&str>) {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            match child.kind() {
                "class_declaration" | "protocol_declaration" => {
                    let Some(name) = type_declaration_name(child, source) else {
                        continue;
                    };
                    let extension = child
                        .child_by_field_name("declaration_kind")
                        .is_some_and(|kind| kind.kind() == "extension");
                    if !extension {
                        *self.types.entry(name.clone()).or_default() += 1;
                    }
                    if let Some(body) = type_body(child) {
                        self.read_scope(body, source, Some(&name));
                    }
                }
                "function_declaration" | "protocol_function_declaration" => {
                    let Some(name) = callable_leaf(child, source) else {
                        continue;
                    };
                    let returned = match declared_return(child, source) {
                        Some(DeclaredReturn::Type { type_name }) => Some(type_name),
                        _ => None,
                    };
                    match owner {
                        Some(owner) => merge(
                            self.methods.entry((owner.to_string(), name)).or_default(),
                            returned,
                        ),
                        None => merge(self.functions.entry(name).or_default(), returned),
                    }
                }
                "property_declaration" | "protocol_property_declaration" => {
                    let Some(owner) = owner else {
                        continue;
                    };
                    if is_static(child, source) {
                        continue;
                    }
                    let Some(name) = property_name(child, source) else {
                        continue;
                    };
                    let origin = property_origin(child, source);
                    let table = self.properties.entry(owner.to_string()).or_default();
                    match table.get_mut(&name) {
                        Some(slot) => *slot = None,
                        None => {
                            table.insert(name, origin);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// Whether a callee names a type: one this file declares, or, when the
    /// file declares no function of that name, one spelled as Swift spells
    /// types (a leading capital), which is how an initializer call reads.
    pub(crate) fn is_type_name(&self, name: &str) -> bool {
        if self.functions.contains_key(name) {
            return false;
        }
        self.types.contains_key(name)
            || name
                .chars()
                .next()
                .is_some_and(|first| first.is_ascii_uppercase())
                && !SYSTEM_TYPES.contains(&name)
    }

    pub(crate) fn declares_function(&self, name: &str) -> bool {
        self.functions.contains_key(name)
    }

    fn function_return(&self, name: &str) -> Option<&str> {
        agreed(self.functions.get(name))
    }

    fn method_return(&self, owner: &str, method: &str) -> Option<&str> {
        agreed(self.methods.get(&(owner.to_string(), method.to_string())))
    }

    fn property(&self, owner: &str, name: &str) -> Option<&ValueOrigin> {
        self.properties.get(owner)?.get(name)?.as_ref()
    }
}

fn is_static(node: Node<'_>, source: &[u8]) -> bool {
    let mut cursor = node.walk();
    let found = node.children(&mut cursor).any(|child| {
        child.kind() == "modifiers" && {
            let spelled = text(child, source);
            spelled
                .split_whitespace()
                .any(|word| word == "static" || word == "class")
        }
    });
    found
}

/// The one name a property declaration binds.
fn property_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    let pattern = node.child_by_field_name("name")?;
    let bound = pattern.child_by_field_name("bound_identifier")?;
    let name = text(bound, source);
    (!name.is_empty()).then(|| name.to_string())
}

/// What a property declaration says its value is: its annotation, else its
/// initial value when that is an initializer call.
fn property_origin(node: Node<'_>, source: &[u8]) -> Option<ValueOrigin> {
    let mut cursor = node.walk();
    let annotation = node
        .children(&mut cursor)
        .find(|child| child.kind() == "type_annotation");
    if let Some(annotation) = annotation {
        return named_type(annotation, source).map(|type_name| ValueOrigin::Declared { type_name });
    }
    let value = node.child_by_field_name("value")?;
    let callee = initializer_callee(value, source)?;
    Some(ValueOrigin::Call { callee })
}

/// `Store(...)`: the type an initializer call names.
fn initializer_callee(value: Node<'_>, source: &[u8]) -> Option<String> {
    let value = unwrap_expression(value);
    if value.kind() != "call_expression" {
        return None;
    }
    let callee = value.named_child(0)?;
    if callee.kind() != "simple_identifier" {
        return None;
    }
    let name = text(callee, source);
    (!name.is_empty()
        && name.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        && !SYSTEM_TYPES.contains(&name))
    .then(|| name.to_string())
}

/// An expression through `try`, `await` and parentheses.
fn unwrap_expression(mut node: Node<'_>) -> Node<'_> {
    loop {
        match node.kind() {
            "try_expression" | "await_expression" => match node.child_by_field_name("expr") {
                Some(inner) => node = inner,
                None => match node.named_children(&mut node.walk()).last() {
                    Some(inner) => node = inner,
                    None => return node,
                },
            },
            "tuple_expression" if node.named_child_count() == 1 => match node.named_child(0) {
                Some(inner) => node = inner.child_by_field_name("value").unwrap_or(inner),
                None => return node,
            },
            _ => return node,
        }
    }
}

// ── One callable's bindings ───────────────────────────────────────────────

#[derive(Debug, Clone)]
struct Declared {
    origin: Option<ValueOrigin>,
    visible_from: usize,
    end: usize,
}

/// A callable's bindings, read once and shared by the adapter (to type
/// receivers) and by the value-origin facts.
#[derive(Debug, Default)]
pub(crate) struct SwiftScopeReader {
    pub(crate) bindings: BTreeMap<String, ValueOrigin>,
    /// Names bound from the enclosing type's property, which `self.name`
    /// reads too.
    members: Vec<String>,
}

impl SwiftScopeReader {
    pub(crate) fn for_callable(
        callable: Node<'_>,
        source: &[u8],
        owner: Option<&str>,
        file: &SwiftFileTypes,
    ) -> Self {
        let mut reader = Self::default();
        if callable.has_error() {
            return reader;
        }
        let mut census = Census::default();
        census.collect(callable, source, file);
        let (bindings, members) = census.settle(owner, file);
        reader.bindings = bindings;
        reader.members = members;
        reader
    }

    /// Whether `name` read through `self.name` is the binding recorded for
    /// it: it is when the binding came from the enclosing type's property.
    pub(crate) fn is_member(&self, name: &str) -> bool {
        self.members.iter().any(|member| member == name)
    }

    /// Whether the linker's value-origin tier can follow a receiver's
    /// origin: a declared type, an initializer of a type, a function this
    /// file declares (the linker finds a free function only in the calling
    /// file), or a method called on one of those.
    pub(crate) fn linker_can_follow(&self, receiver: &str, file: &SwiftFileTypes) -> bool {
        self.bindings
            .get(receiver)
            .is_some_and(|origin| self.followable(origin, file, 0))
    }

    fn followable(&self, origin: &ValueOrigin, file: &SwiftFileTypes, depth: usize) -> bool {
        if depth > 8 {
            return false;
        }
        match origin {
            ValueOrigin::Declared { .. } => true,
            ValueOrigin::Local { name } => self
                .bindings
                .get(name)
                .is_some_and(|bound| self.followable(bound, file, depth + 1)),
            ValueOrigin::Call { callee } => {
                file.is_type_name(callee) || file.declares_function(callee)
            }
            ValueOrigin::MethodCall { receiver, .. } => self.followable(receiver, file, depth + 1),
            ValueOrigin::Entered { .. }
            | ValueOrigin::Provided { .. }
            | ValueOrigin::External { .. }
            | ValueOrigin::Supplied { .. }
            | ValueOrigin::ImportedProvider { .. }
            | ValueOrigin::ImportedCall { .. }
            | ValueOrigin::Global { .. }
            | ValueOrigin::Module { .. }
            | ValueOrigin::ModuleMember { .. }
            | ValueOrigin::Enclosing { .. } => false,
        }
    }

    /// The class a receiver holds when this file alone settles it.
    #[allow(dead_code)]
    pub(crate) fn class_of(&self, receiver: &str, file: &SwiftFileTypes) -> Option<String> {
        self.resolve(self.bindings.get(receiver)?, file, 0)
    }

    fn resolve(&self, origin: &ValueOrigin, file: &SwiftFileTypes, depth: usize) -> Option<String> {
        if depth > 8 {
            return None;
        }
        match origin {
            ValueOrigin::Declared { type_name } => Some(type_name.clone()),
            ValueOrigin::Local { name } => self.resolve(self.bindings.get(name)?, file, depth + 1),
            ValueOrigin::Call { callee } => {
                if file.declares_function(callee) {
                    file.function_return(callee).map(str::to_string)
                } else {
                    file.is_type_name(callee).then(|| callee.clone())
                }
            }
            ValueOrigin::MethodCall { receiver, method } => {
                let class = self.resolve(receiver, file, depth + 1)?;
                file.method_return(&class, method).map(str::to_string)
            }
            ValueOrigin::Entered { .. }
            | ValueOrigin::Provided { .. }
            | ValueOrigin::External { .. }
            | ValueOrigin::Supplied { .. }
            | ValueOrigin::ImportedProvider { .. }
            | ValueOrigin::ImportedCall { .. }
            | ValueOrigin::Global { .. }
            | ValueOrigin::Module { .. }
            | ValueOrigin::ModuleMember { .. }
            | ValueOrigin::Enclosing { .. } => None,
        }
    }
}

/// Every local a callable's subtree declares, as (name, visible from,
/// visible until): parameters, `let` and `var`, conditional and loop
/// bindings, closure parameters and nested functions.
pub(crate) fn local_declarations(callable: Node<'_>, source: &[u8]) -> Vec<(String, usize, usize)> {
    let mut census = Census::default();
    census.collect(callable, source, &SwiftFileTypes::default());
    let mut out = Vec::new();
    for (name, declarations) in census.declared {
        for declared in declarations {
            out.push((name.clone(), declared.visible_from, declared.end));
        }
    }
    out
}

#[derive(Debug, Default)]
struct Census {
    declared: HashMap<String, Vec<Declared>>,
    reads: HashMap<String, Vec<usize>>,
}

impl Census {
    fn declare(
        &mut self,
        name: &str,
        origin: Option<ValueOrigin>,
        visible_from: usize,
        end: usize,
    ) {
        if name.is_empty() || name == "_" {
            return;
        }
        self.declared
            .entry(name.to_string())
            .or_default()
            .push(Declared {
                origin,
                visible_from,
                end,
            });
    }

    fn collect(&mut self, callable: Node<'_>, source: &[u8], file: &SwiftFileTypes) {
        let mut declaring: Vec<(usize, usize)> = Vec::new();
        let mut stack = vec![callable];
        while let Some(node) = stack.pop() {
            match node.kind() {
                "parameter" => {
                    // `name` holds the parameter's name first and its type
                    // after; the external label is its own field.
                    let mut cursor = node.walk();
                    let names: Vec<Node<'_>> =
                        node.children_by_field_name("name", &mut cursor).collect();
                    if let Some(name) = names
                        .first()
                        .filter(|name| name.kind() == "simple_identifier")
                    {
                        declaring.push((name.start_byte(), name.end_byte()));
                        let variadic = {
                            let mut inner = node.walk();
                            let found =
                                node.children(&mut inner).any(|child| child.kind() == "...");
                            found
                        };
                        let origin = names
                            .get(1)
                            .filter(|_| !variadic)
                            .and_then(|ty| named_type(*ty, source))
                            .map(|type_name| ValueOrigin::Declared { type_name });
                        let scope = parameter_scope(node);
                        self.declare(text(*name, source), origin, scope.0, scope.1);
                    }
                    let mut labels = node.walk();
                    for label in node.children_by_field_name("external_name", &mut labels) {
                        declaring.push((label.start_byte(), label.end_byte()));
                    }
                }
                "lambda_parameter" => {
                    let mut cursor = node.walk();
                    let names: Vec<Node<'_>> =
                        node.children_by_field_name("name", &mut cursor).collect();
                    if let Some(name) = names
                        .first()
                        .filter(|name| name.kind() == "simple_identifier")
                    {
                        declaring.push((name.start_byte(), name.end_byte()));
                        let origin = names
                            .get(1)
                            .and_then(|ty| named_type(*ty, source))
                            .map(|type_name| ValueOrigin::Declared { type_name });
                        let lambda = ancestor(node, "lambda_literal").unwrap_or(node);
                        self.declare(
                            text(*name, source),
                            origin,
                            lambda.start_byte(),
                            lambda.end_byte(),
                        );
                    }
                }
                "property_declaration" if node.id() != callable.id() => {
                    let scope = enclosing_block(node);
                    let mut cursor = node.walk();
                    for pattern in node.children_by_field_name("name", &mut cursor) {
                        let single = pattern.child_by_field_name("bound_identifier");
                        for bound in bound_identifiers(pattern) {
                            declaring.push((bound.start_byte(), bound.end_byte()));
                            let origin = if single.is_some_and(|single| single.id() == bound.id()) {
                                local_origin(node, source, file)
                            } else {
                                None
                            };
                            self.declare(text(bound, source), origin, bound.end_byte(), scope.1);
                        }
                    }
                }
                "if_statement" | "guard_statement" | "while_statement" => {
                    // `if let x = value`: the statement holds the binding in
                    // its `bound_identifier` field and the value after `=`.
                    let mut cursor = node.walk();
                    let bounds: Vec<Node<'_>> = node
                        .children_by_field_name("bound_identifier", &mut cursor)
                        .collect();
                    let conditions: Vec<Node<'_>> = {
                        let mut inner = node.walk();
                        node.children_by_field_name("condition", &mut inner)
                            .collect()
                    };
                    let (from, end) = if node.kind() == "guard_statement" {
                        (node.end_byte(), enclosing_block(node).1)
                    } else {
                        let body = first_child_of_kind(node, "statements");
                        (
                            body.map_or(node.end_byte(), |body| body.start_byte()),
                            body.map_or(node.end_byte(), |body| body.end_byte()),
                        )
                    };
                    for (index, bound) in bounds.iter().enumerate() {
                        declaring.push((bound.start_byte(), bound.end_byte()));
                        // With one binding the value is the expression after
                        // the `=` among the conditions.
                        let origin = if bounds.len() == 1 && index == 0 {
                            conditions
                                .iter()
                                .rev()
                                .find(|condition| {
                                    condition.is_named()
                                        && condition.kind() != "value_binding_pattern"
                                })
                                .and_then(|value| binding_value(*value, *bound, source, file))
                        } else {
                            None
                        };
                        match origin {
                            Some(OriginOrSame::Same) => {
                                // Shadows with the value it unwraps: no new
                                // declaration of a different value.
                                self.reads
                                    .entry(text(*bound, source).to_string())
                                    .or_default();
                            }
                            Some(OriginOrSame::Origin(origin)) => {
                                self.declare(text(*bound, source), Some(origin), from, end)
                            }
                            None => self.declare(text(*bound, source), None, from, end),
                        }
                    }
                }
                "for_statement" => {
                    if let Some(item) = node.child_by_field_name("item") {
                        let body = first_child_of_kind(node, "statements");
                        let (from, end) = (
                            body.map_or(node.start_byte(), |body| body.start_byte()),
                            node.end_byte(),
                        );
                        for bound in bound_identifiers(item) {
                            declaring.push((bound.start_byte(), bound.end_byte()));
                            self.declare(text(bound, source), None, from, end);
                        }
                    }
                }
                "switch_entry" | "catch_block" => {
                    let mut cursor = node.walk();
                    for child in node.children(&mut cursor) {
                        if matches!(child.kind(), "switch_pattern" | "catch_keyword")
                            || child.kind() == "pattern"
                        {
                            for bound in bound_identifiers(child) {
                                declaring.push((bound.start_byte(), bound.end_byte()));
                                self.declare(
                                    text(bound, source),
                                    None,
                                    node.start_byte(),
                                    node.end_byte(),
                                );
                            }
                        }
                    }
                    if node.kind() == "catch_block" {
                        // An untyped `catch` binds `error`.
                        self.declare("error", None, node.start_byte(), node.end_byte());
                    }
                }
                "function_declaration" if node.id() != callable.id() => {
                    if let Some(name) = node.child_by_field_name("name") {
                        declaring.push((name.start_byte(), name.end_byte()));
                        let block = enclosing_block(node);
                        self.declare(text(name, source), None, block.0, block.1);
                    }
                }
                _ => {}
            }
            let mut cursor = node.walk();
            stack.extend(node.children(&mut cursor));
        }
        // Reads: every simple identifier that is not a declared name, a
        // member name after a dot, or an argument label.
        let mut stack = vec![callable];
        while let Some(node) = stack.pop() {
            if node.kind() == "simple_identifier"
                && !declaring.contains(&(node.start_byte(), node.end_byte()))
                && !node.parent().is_some_and(|parent| {
                    matches!(parent.kind(), "navigation_suffix" | "value_argument_label")
                        || (parent.kind() == "function_declaration"
                            && is_field(parent, "name", node))
                })
            {
                self.reads
                    .entry(text(node, source).to_string())
                    .or_default()
                    .push(node.start_byte());
            }
            let mut cursor = node.walk();
            stack.extend(node.children(&mut cursor));
        }
    }

    fn settle(
        self,
        owner: Option<&str>,
        file: &SwiftFileTypes,
    ) -> (BTreeMap<String, ValueOrigin>, Vec<String>) {
        let mut bindings = BTreeMap::new();
        let mut members = Vec::new();
        for (name, declarations) in &self.declared {
            let covered = self.reads.get(name).is_none_or(|reads| {
                reads.iter().all(|&at| {
                    declarations
                        .iter()
                        .any(|declared| declared.visible_from <= at && at < declared.end)
                })
            });
            if !covered {
                continue;
            }
            let Some(first) = declarations.first().and_then(|d| d.origin.clone()) else {
                continue;
            };
            if declarations
                .iter()
                .all(|d| d.origin.as_ref() == Some(&first))
            {
                bindings.insert(name.clone(), first);
            }
        }
        if let Some(owner) = owner {
            for name in self.reads.keys() {
                if self.declared.contains_key(name) {
                    continue;
                }
                if let Some(origin) = file.property(owner, name) {
                    bindings.insert(name.clone(), origin.clone());
                    members.push(name.clone());
                }
            }
        }
        (bindings, members)
    }
}

/// What an `if let` binding holds: the value it unwraps when that is the
/// same name, or an origin.
enum OriginOrSame {
    Same,
    Origin(ValueOrigin),
}

fn binding_value(
    value: Node<'_>,
    bound: Node<'_>,
    source: &[u8],
    file: &SwiftFileTypes,
) -> Option<OriginOrSame> {
    let value = unwrap_expression(value);
    if value.kind() == "simple_identifier" && text(value, source) == text(bound, source) {
        // `if let store = store`: the same value, unwrapped.
        return Some(OriginOrSame::Same);
    }
    expression_origin(value, source, file).map(OriginOrSame::Origin)
}

/// A local `let` or `var`'s origin: its annotation, else its initial value.
fn local_origin(node: Node<'_>, source: &[u8], file: &SwiftFileTypes) -> Option<ValueOrigin> {
    let mut cursor = node.walk();
    let annotation = node
        .children(&mut cursor)
        .find(|child| child.kind() == "type_annotation");
    if let Some(annotation) = annotation {
        return named_type(annotation, source).map(|type_name| ValueOrigin::Declared { type_name });
    }
    expression_origin(node.child_by_field_name("value")?, source, file)
}

/// The origin of the value an expression evaluates to, when it is a shape
/// the linker can follow: a name, an initializer or function call, or a
/// method call on another origin.
fn expression_origin(value: Node<'_>, source: &[u8], file: &SwiftFileTypes) -> Option<ValueOrigin> {
    let value = unwrap_expression(value);
    match value.kind() {
        "simple_identifier" => Some(ValueOrigin::Local {
            name: text(value, source).to_string(),
        }),
        "call_expression" => {
            let callee = value.named_child(0)?;
            // A trailing closure or arguments follow the callee.
            match callee.kind() {
                "simple_identifier" => {
                    let name = text(callee, source);
                    if SYSTEM_TYPES.contains(&name) {
                        return None;
                    }
                    Some(ValueOrigin::Call {
                        callee: name.to_string(),
                    })
                }
                "navigation_expression" => {
                    let method = callee
                        .child_by_field_name("suffix")?
                        .child_by_field_name("suffix")?;
                    if method.kind() != "simple_identifier" {
                        return None;
                    }
                    let target = callee.child_by_field_name("target")?;
                    let receiver = match target.kind() {
                        "simple_identifier" => {
                            let name = text(target, source);
                            // `Store.make()` calls a static member of a type,
                            // which no binding describes.
                            if file.is_type_name(name) && !file.declares_function(name) {
                                return None;
                            }
                            ValueOrigin::Local {
                                name: name.to_string(),
                            }
                        }
                        "self_expression" => return None,
                        _ => expression_origin(target, source, file)?,
                    };
                    Some(ValueOrigin::MethodCall {
                        receiver: Box::new(receiver),
                        method: text(method, source).to_string(),
                    })
                }
                _ => None,
            }
        }
        _ => None,
    }
}

// ── Syntax helpers ────────────────────────────────────────────────────────

/// Every identifier a pattern binds.
pub(crate) fn bound_identifiers(pattern: Node<'_>) -> Vec<Node<'_>> {
    let mut out = Vec::new();
    let mut stack = vec![pattern];
    while let Some(node) = stack.pop() {
        if let Some(bound) = node.child_by_field_name("bound_identifier") {
            out.push(bound);
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "pattern"
                || child.kind() == "tuple_pattern"
                || child.kind() == "value_binding_pattern"
            {
                stack.push(child);
            }
        }
    }
    out.sort_by_key(|node| node.start_byte());
    out.dedup_by_key(|node| node.start_byte());
    out
}

/// The bytes a parameter is visible in: its function, initializer or
/// subscript.
fn parameter_scope(parameter: Node<'_>) -> (usize, usize) {
    let holder = parameter.parent().unwrap_or(parameter);
    (holder.start_byte(), holder.end_byte())
}

/// The block a local declaration sits in: the nearest `statements`, or the
/// callable body.
pub(crate) fn enclosing_block(node: Node<'_>) -> (usize, usize) {
    let mut current = node.parent();
    while let Some(parent) = current {
        if matches!(
            parent.kind(),
            "statements" | "function_body" | "class_body" | "source_file"
        ) {
            return (parent.start_byte(), parent.end_byte());
        }
        current = parent.parent();
    }
    (node.start_byte(), node.end_byte())
}

fn first_child_of_kind<'tree>(node: Node<'tree>, kind: &str) -> Option<Node<'tree>> {
    let mut cursor = node.walk();
    let found = node
        .children(&mut cursor)
        .find(|child| child.kind() == kind);
    found
}

fn ancestor<'tree>(node: Node<'tree>, kind: &str) -> Option<Node<'tree>> {
    let mut current = node.parent();
    while let Some(parent) = current {
        if parent.kind() == kind {
            return Some(parent);
        }
        current = parent.parent();
    }
    None
}

fn is_field(parent: Node<'_>, field: &str, child: Node<'_>) -> bool {
    let mut cursor = parent.walk();
    let found = parent
        .children_by_field_name(field, &mut cursor)
        .any(|held| held.id() == child.id());
    found
}

fn text<'a>(node: Node<'_>, source: &'a [u8]) -> &'a str {
    node.utf8_text(source).unwrap_or("")
}

#[cfg(test)]
mod tests;
