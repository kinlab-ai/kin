// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Kotlin value origins: the declared types of parameters, locals, loop and
//! catch variables and properties (constructor properties included), the
//! value a local without a declared type is initialized with, and the types
//! functions declare they return.
//!
//! Kotlin fixes a variable's static type where it declares it, from the
//! written type or else from the initializer, and a call through the variable
//! dispatches from that type whatever is assigned later. A property a
//! dependency-injection container fills (`@Inject lateinit var`, or a
//! constructor property of an `@Inject constructor`) is no different, and a
//! call through a property typed by an interface binds to the interface's
//! method. Generic types are read erased, and a variable typed by a type
//! parameter takes the one bound the parameter declares, or nothing.
//!
//! A local is read only when the function declares its name exactly once and
//! every use sits where that declaration is visible. A name the function tests
//! with `is`, casts with `as` or matches in a `when` is left alone, since a
//! smart cast can give it a narrower type at the call. A property is read for
//! a name the function never declares, through the class that declares it, its
//! companion object, and for an `inner` class its outer class; a class that
//! names a supertype may inherit a same-named property, so the walk stops
//! there, and an extension function reads no property at all, since its
//! receiver's members come first. A call written in a lambda may reach a
//! member of an implicit receiver, so only local bindings type the receivers
//! of calls inside lambdas; the adapter applies that rule, and calls inside a
//! nested class or object body carry no receiver at all.

use std::collections::{BTreeMap, HashMap, HashSet};

use tree_sitter::{Node, Tree};

use super::{DeclaredReturn, FileFacts, ScopeFacts, ValueOrigin};

pub(super) fn read_file(tree: &Tree, source: &[u8], _file_path: &str) -> FileFacts {
    let mut facts = FileFacts::default();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        let leaf = match node.kind() {
            "function_declaration" => function_name(node, source),
            "secondary_constructor" => Some("constructor".to_string()),
            _ => None,
        };
        if let Some(leaf) = leaf {
            let scope = function_scope(node, source).facts;
            facts
                .by_node
                .push((vec![(node.start_byte(), node.end_byte())], leaf, scope));
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    facts
}

/// The names a function's facts type, which are the receivers a call written
/// in it may carry for the linker to follow.
pub(crate) struct Receivers {
    /// Every typed name: a call in the function's own body may carry any.
    pub(crate) anywhere: HashSet<String>,
    /// The typed names the function declares itself, which no implicit
    /// receiver's member can shadow: a call inside a lambda may carry these.
    pub(crate) in_lambdas: HashSet<String>,
}

pub(crate) fn receivers(function: Node<'_>, source: &[u8]) -> Receivers {
    let scope = function_scope(function, source);
    Receivers {
        anywhere: scope.facts.bindings.keys().cloned().collect(),
        in_lambdas: scope
            .facts
            .bindings
            .keys()
            .filter(|name| scope.locals.contains(*name))
            .cloned()
            .collect(),
    }
}

fn function_name(function: Node<'_>, source: &[u8]) -> Option<String> {
    let name = function
        .child_by_field_name("name")?
        .utf8_text(source)
        .ok()?;
    (!name.is_empty()).then(|| name.to_string())
}

struct FunctionScope {
    facts: ScopeFacts,
    /// Bound names the function declares itself, rather than properties.
    locals: HashSet<String>,
}

fn function_scope(function: Node<'_>, source: &[u8]) -> FunctionScope {
    let types = TypeScope::of(function, source);
    let mut facts = ScopeFacts {
        returns: return_type(function)
            .and_then(|ty| types.returned(ty, source))
            .map(|type_name| DeclaredReturn::Type { type_name }),
        ..ScopeFacts::default()
    };
    let region = Region::of(function, source);
    let mut bindings: BTreeMap<String, ValueOrigin> = BTreeMap::new();
    let mut locals = HashSet::new();
    for (name, declared) in &region.bindings {
        let [binding] = declared.as_slice() else {
            continue;
        };
        if region.narrowed.contains(name) {
            continue;
        }
        let visible = region
            .references
            .get(name)
            .into_iter()
            .flatten()
            .all(|at| binding.visible.0 <= *at && *at < binding.visible.1);
        if !visible {
            continue;
        }
        let origin = match binding.origin {
            Origin::Declared(ty) => types
                .declared(ty, source)
                .map(|type_name| ValueOrigin::Declared { type_name }),
            Origin::Initialized(value) => initializer_origin(value, &region, &types, source),
            Origin::Unread => None,
        };
        if let Some(origin) = origin {
            locals.insert(name.clone());
            bindings.insert(name.clone(), origin);
        }
    }
    if !is_extension(function) {
        for name in &region.used {
            if region.bindings.contains_key(name)
                || region.narrowed.contains(name)
                || bindings.contains_key(name)
            {
                continue;
            }
            if let Some(origin) = property_origin(function, name, &region, &types, source) {
                bindings.insert(name.clone(), origin);
            }
        }
    }
    facts.bindings = bindings;
    FunctionScope { facts, locals }
}

/// Whether a function declares a receiver type, which makes the receiver's
/// members visible by their bare names ahead of any property.
fn is_extension(function: Node<'_>) -> bool {
    let Some(name) = function.child_by_field_name("name") else {
        return false;
    };
    let mut cursor = function.walk();
    let found = function
        .children(&mut cursor)
        .take_while(|child| child.id() != name.id())
        .any(|child| is_type_node(child));
    found
}

fn is_type_node(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "user_type" | "nullable_type" | "function_type" | "parenthesized_type" | "type_reference"
    )
}

/// The return type a function declares: the type after its parameter list.
fn return_type(function: Node<'_>) -> Option<Node<'_>> {
    if function.kind() != "function_declaration" {
        return None;
    }
    let mut cursor = function.walk();
    let mut after_parameters = false;
    for child in function.children(&mut cursor) {
        if child.kind() == "function_value_parameters" {
            after_parameters = true;
            continue;
        }
        if after_parameters && is_type_node(child) {
            return Some(child);
        }
        if after_parameters && matches!(child.kind(), "function_body" | "type_constraints") {
            return None;
        }
    }
    None
}

/// The erased class a Kotlin type node names: `Store` for `Store`, `Store?`
/// and `Store<T>`, `Map.Entry` for `Map.Entry<K, V>`. `None` for a function
/// type or anything else that names no class.
pub(crate) fn kotlin_erased_type_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    match node.kind() {
        "nullable_type" | "type_reference" | "parenthesized_type" => {
            let mut cursor = node.walk();
            let inner = node.named_children(&mut cursor).find(|child| {
                matches!(
                    child.kind(),
                    "user_type" | "nullable_type" | "type_reference" | "parenthesized_type"
                )
            })?;
            kotlin_erased_type_name(inner, source)
        }
        "user_type" => {
            let mut cursor = node.walk();
            let segments: Vec<&str> = node
                .named_children(&mut cursor)
                .filter(|child| child.kind() == "identifier")
                .filter_map(|child| child.utf8_text(source).ok())
                .collect();
            (!segments.is_empty() && segments.iter().all(|segment| !segment.is_empty()))
                .then(|| segments.join("."))
        }
        _ => None,
    }
}

/// What a declaration says about the value its name holds.
enum Origin<'t> {
    Declared(Node<'t>),
    Initialized(Node<'t>),
    Unread,
}

struct Binding<'t> {
    origin: Origin<'t>,
    visible: (usize, usize),
}

/// One function's declarations and uses, outside any class or object body
/// nested in it.
struct Region<'t> {
    bindings: HashMap<String, Vec<Binding<'t>>>,
    references: HashMap<String, Vec<usize>>,
    /// Names read as a call's receiver, or as a local's initializer.
    used: HashSet<String>,
    /// Names a smart cast may narrow.
    narrowed: HashSet<String>,
    /// Names the function declares as local functions, classes or objects,
    /// which a call's callee may spell.
    declared_callables: HashSet<String>,
}

impl<'t> Region<'t> {
    fn of(function: Node<'t>, source: &[u8]) -> Self {
        let mut region = Region {
            bindings: HashMap::new(),
            references: HashMap::new(),
            used: HashSet::new(),
            narrowed: HashSet::new(),
            declared_callables: HashSet::new(),
        };
        let whole = (function.start_byte(), function.end_byte());
        let mut cursor = function.walk();
        for child in function.children(&mut cursor) {
            match child.kind() {
                "function_value_parameters" => region.parameters(child, whole, source),
                "function_body" | "block" => region.walk(child, source),
                // A secondary constructor's delegation call reads its
                // parameters.
                "constructor_delegation_call" => region.walk(child, source),
                _ => {}
            }
        }
        region
    }

    fn bind(&mut self, name: Node<'t>, origin: Origin<'t>, visible: (usize, usize), source: &[u8]) {
        let Ok(text) = name.utf8_text(source) else {
            return;
        };
        if text.is_empty() {
            return;
        }
        self.bindings
            .entry(text.to_string())
            .or_default()
            .push(Binding { origin, visible });
    }

    fn parameters(&mut self, parameters: Node<'t>, visible: (usize, usize), source: &[u8]) {
        let mut cursor = parameters.walk();
        let mut vararg = false;
        for child in parameters.named_children(&mut cursor) {
            match child.kind() {
                "parameter_modifiers" => {
                    vararg = child
                        .utf8_text(source)
                        .is_ok_and(|text| text.split_whitespace().any(|word| word == "vararg"));
                }
                "parameter" => {
                    let mut inner = child.walk();
                    let parts: Vec<Node> = child.named_children(&mut inner).collect();
                    let name = parts.iter().find(|part| part.kind() == "identifier");
                    let ty = parts.iter().find(|part| is_type_node(**part));
                    if let Some(name) = name {
                        // A vararg parameter holds an array of its type.
                        let origin = match (ty, vararg) {
                            (Some(ty), false) => Origin::Declared(*ty),
                            _ => Origin::Unread,
                        };
                        self.bind(*name, origin, visible, source);
                    }
                    vararg = false;
                }
                _ => {}
            }
        }
    }

    /// A `variable_declaration` or the names of a destructuring one.
    fn declare(
        &mut self,
        declaration: Node<'t>,
        value: Option<Node<'t>>,
        visible: (usize, usize),
        source: &[u8],
    ) {
        match declaration.kind() {
            "variable_declaration" => {
                let mut cursor = declaration.walk();
                let parts: Vec<Node> = declaration.named_children(&mut cursor).collect();
                let Some(name) = parts.iter().find(|part| part.kind() == "identifier") else {
                    return;
                };
                let origin = match (parts.iter().find(|part| is_type_node(**part)), value) {
                    (Some(ty), _) => Origin::Declared(*ty),
                    (None, Some(value)) => Origin::Initialized(value),
                    (None, None) => Origin::Unread,
                };
                self.bind(*name, origin, visible, source);
            }
            "multi_variable_declaration" => {
                let mut cursor = declaration.walk();
                for part in declaration.named_children(&mut cursor) {
                    if part.kind() == "variable_declaration" {
                        let mut inner = part.walk();
                        let name = part
                            .named_children(&mut inner)
                            .find(|child| child.kind() == "identifier");
                        if let Some(name) = name {
                            self.bind(name, Origin::Unread, visible, source);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn walk(&mut self, node: Node<'t>, source: &[u8]) {
        match node.kind() {
            // A class or object body nested in the function is its own scope.
            "class_declaration" | "object_declaration" | "object_literal" => {
                if let Some(name) = node
                    .child_by_field_name("name")
                    .and_then(|name| name.utf8_text(source).ok())
                {
                    self.declared_callables.insert(name.to_string());
                }
                return;
            }
            "function_declaration" => {
                if let Some(name) = function_name(node, source) {
                    self.declared_callables.insert(name);
                }
                // A local function's parameters and locals are its own; its
                // body is still scanned for uses of this function's names.
                let whole = (node.start_byte(), node.end_byte());
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    match child.kind() {
                        "function_value_parameters" => self.parameters(child, whole, source),
                        "function_body" => self.walk(child, source),
                        _ => {}
                    }
                }
                return;
            }
            "property_declaration" => {
                let visible = (node.start_byte(), scope_end(node));
                let value = property_initializer(node);
                let mut cursor = node.walk();
                let declaration = node.named_children(&mut cursor).find(|child| {
                    matches!(
                        child.kind(),
                        "variable_declaration" | "multi_variable_declaration"
                    )
                });
                if let Some(declaration) = declaration {
                    // A delegated local (`by lazy {}`) holds what its delegate
                    // returns.
                    let delegated = find_kind(node, "property_delegate").is_some();
                    let value = if delegated { None } else { value };
                    if delegated && declaration.kind() == "variable_declaration" {
                        if let Some(name) = find_kind(declaration, "identifier") {
                            self.bind(name, Origin::Unread, visible, source);
                        }
                    } else {
                        self.declare(declaration, value, visible, source);
                    }
                }
            }
            "for_statement" => {
                let visible = (node.start_byte(), node.end_byte());
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    if matches!(
                        child.kind(),
                        "variable_declaration" | "multi_variable_declaration"
                    ) {
                        self.declare(child, None, visible, source);
                    }
                }
            }
            "catch_block" => {
                let visible = (node.start_byte(), node.end_byte());
                let mut cursor = node.walk();
                let parts: Vec<Node> = node.named_children(&mut cursor).collect();
                if let Some(name) = parts.iter().find(|part| part.kind() == "identifier") {
                    let origin = parts
                        .iter()
                        .find(|part| is_type_node(**part))
                        .map_or(Origin::Unread, |ty| Origin::Declared(*ty));
                    self.bind(*name, origin, visible, source);
                }
            }
            "lambda_literal" | "anonymous_function" => {
                let visible = (node.start_byte(), node.end_byte());
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    match child.kind() {
                        "lambda_parameters" => {
                            let mut inner = child.walk();
                            for parameter in child.named_children(&mut inner) {
                                if matches!(
                                    parameter.kind(),
                                    "variable_declaration" | "multi_variable_declaration"
                                ) {
                                    // A lambda parameter's type may be
                                    // inferred from a context the reader does
                                    // not follow.
                                    self.declare_unread(parameter, visible, source);
                                }
                            }
                        }
                        "function_value_parameters" => {
                            self.parameters(child, visible, source);
                        }
                        _ => {}
                    }
                }
            }
            "when_expression" => {
                let visible = (node.start_byte(), node.end_byte());
                let mut cursor = node.walk();
                let children: Vec<Node> = node.named_children(&mut cursor).collect();
                let tests_type = children.iter().any(|entry| {
                    entry.kind() == "when_entry" && find_kind(*entry, "type_test").is_some()
                });
                if let Some(subject) = children.iter().find(|child| child.kind() == "when_subject")
                {
                    let mut inner = subject.walk();
                    let parts: Vec<Node> = subject.named_children(&mut inner).collect();
                    if let Some(declaration) = parts
                        .iter()
                        .find(|part| part.kind() == "variable_declaration")
                    {
                        let value = parts
                            .iter()
                            .rev()
                            .find(|part| part.kind() != "variable_declaration")
                            .copied();
                        self.declare(*declaration, value, visible, source);
                        if tests_type {
                            if let Some(name) = find_kind(*declaration, "identifier")
                                .and_then(|name| name.utf8_text(source).ok())
                            {
                                self.narrowed.insert(name.to_string());
                            }
                        }
                    } else if tests_type {
                        for part in &parts {
                            if part.kind() == "identifier" {
                                if let Ok(name) = part.utf8_text(source) {
                                    self.narrowed.insert(name.to_string());
                                }
                            }
                        }
                    }
                }
            }
            "is_expression" | "as_expression" => {
                if let Some(left) = node.child_by_field_name("left") {
                    if left.kind() == "identifier" {
                        if let Ok(name) = left.utf8_text(source) {
                            self.narrowed.insert(name.to_string());
                        }
                    }
                }
            }
            "call_expression" => {
                if let Some(callee) = find_kind(node, "navigation_expression") {
                    if let Some(object) = callee.named_child(0) {
                        if object.kind() == "identifier" {
                            if let Ok(name) = object.utf8_text(source) {
                                self.used.insert(name.to_string());
                            }
                        }
                    }
                }
            }
            "identifier" => {
                if is_reference(node) {
                    if let Ok(name) = node.utf8_text(source) {
                        self.references
                            .entry(name.to_string())
                            .or_default()
                            .push(node.start_byte());
                    }
                }
                return;
            }
            _ => {}
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            self.walk(child, source);
        }
    }

    fn declare_unread(&mut self, declaration: Node<'t>, visible: (usize, usize), source: &[u8]) {
        let mut stack = vec![declaration];
        while let Some(node) = stack.pop() {
            if node.kind() == "variable_declaration" {
                let mut inner = node.walk();
                if let Some(name) = node
                    .named_children(&mut inner)
                    .find(|child| child.kind() == "identifier")
                {
                    self.bind(name, Origin::Unread, visible, source);
                }
                continue;
            }
            let mut cursor = node.walk();
            stack.extend(node.named_children(&mut cursor));
        }
    }
}

fn find_kind<'t>(node: Node<'t>, kind: &str) -> Option<Node<'t>> {
    let mut cursor = node.walk();
    let found = node
        .named_children(&mut cursor)
        .find(|child| child.kind() == kind);
    found
}

/// The value a property declaration initializes its name with: the
/// expression after its `=`.
pub(crate) fn property_initializer(declaration: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = declaration.walk();
    let mut seen_equals = false;
    for child in declaration.children(&mut cursor) {
        if seen_equals && child.is_named() {
            return Some(child);
        }
        if child.kind() == "=" {
            seen_equals = true;
        }
    }
    None
}

/// Where a local declared by `statement` stops being visible.
fn scope_end(statement: Node<'_>) -> usize {
    statement
        .parent()
        .map_or(statement.end_byte(), |parent| parent.end_byte())
}

/// Whether an identifier node reads a variable. Type names, member names
/// after a dot, declared names, labels and named arguments never do;
/// everything else might, and counting a doubtful one only withholds a fact.
fn is_reference(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return true;
    };
    match parent.kind() {
        "navigation_expression" => parent
            .named_child(0)
            .is_some_and(|first| first.id() == node.id()),
        "user_type"
        | "variable_declaration"
        | "parameter"
        | "class_parameter"
        | "type_parameter"
        | "type_constraint"
        | "label"
        | "catch_block"
        | "qualified_identifier"
        | "import"
        | "package_header" => false,
        "function_declaration" | "class_declaration" | "object_declaration" => false,
        "return_expression" => parent
            .child_by_field_name("label")
            .is_none_or(|label| label.id() != node.id()),
        _ => true,
    }
}

/// The origin of the value a local without a declared type is initialized
/// with: a constructor or function called by name, a cast, another name, or
/// a method called on one.
fn initializer_origin(
    value: Node<'_>,
    region: &Region<'_>,
    types: &TypeScope,
    source: &[u8],
) -> Option<ValueOrigin> {
    match value.kind() {
        "parenthesized_expression" => {
            initializer_origin(value.named_child(0)?, region, types, source)
        }
        "call_expression" => {
            let callee = value.named_child(0)?;
            match callee.kind() {
                "identifier" => {
                    let name = callee.utf8_text(source).ok()?;
                    // A callee the function binds or declares itself is not
                    // the file-level name the linker would resolve.
                    if region.bindings.contains_key(name)
                        || region.declared_callables.contains(name)
                        || types.is_local_or_parameter(name)
                    {
                        return None;
                    }
                    Some(ValueOrigin::Call {
                        callee: name.to_string(),
                    })
                }
                "navigation_expression" => {
                    let mut cursor = callee.walk();
                    let parts: Vec<Node> = callee.children(&mut cursor).collect();
                    let [object, operator, method] = parts.as_slice() else {
                        return None;
                    };
                    if !matches!(operator.kind(), "." | "?.") || method.kind() != "identifier" {
                        return None;
                    }
                    let receiver = initializer_origin(*object, region, types, source)?;
                    Some(ValueOrigin::MethodCall {
                        receiver: Box::new(receiver),
                        method: method.utf8_text(source).ok()?.to_string(),
                    })
                }
                _ => None,
            }
        }
        "as_expression" => {
            let ty = value.child_by_field_name("right")?;
            let type_name = types.declared(ty, source)?;
            Some(ValueOrigin::Declared { type_name })
        }
        "identifier" => {
            let name = value.utf8_text(source).ok()?;
            Some(ValueOrigin::Local {
                name: name.to_string(),
            })
        }
        _ => None,
    }
}

/// The type of the property `name` a function in a class reads: the class's
/// own property or constructor property, else its companion object's, else,
/// for an `inner` class naming no supertype, its outer class's. A class that
/// names a supertype may inherit a same-named property, which ends the walk,
/// and so does a class or object declared inside a function body, where the
/// name may be a captured local. A function at the top of the file reads a
/// top-level property of the file that no import shadows.
fn property_origin(
    function: Node<'_>,
    name: &str,
    region: &Region<'_>,
    types: &TypeScope,
    source: &[u8],
) -> Option<ValueOrigin> {
    let mut current = function.parent();
    while let Some(node) = current {
        match node.kind() {
            "source_file" => {
                if file_imports(node, name, source) {
                    return None;
                }
                let found = declared_property(node, name, source)?;
                return property_value(found, region, types, source);
            }
            "class_body" | "enum_class_body" => {
                let owner = node.parent()?;
                if let Some(found) = declared_property(node, name, source)
                    .or_else(|| constructor_property(owner, name, source))
                {
                    return property_value(found, region, types, source);
                }
                if declares_supertype(owner) {
                    return None;
                }
                match owner.kind() {
                    "class_declaration" => {
                        if let Some(companion) = find_kind(node, "companion_object")
                            .and_then(|companion| find_kind(companion, "class_body"))
                        {
                            if let Some(found) = declared_property(companion, name, source) {
                                return property_value(found, region, types, source);
                            }
                        }
                        let inner = find_kind(owner, "modifiers").is_some_and(|modifiers| {
                            modifiers
                                .utf8_text(source)
                                .is_ok_and(|text| text.split_whitespace().any(|w| w == "inner"))
                        });
                        let member = owner
                            .parent()
                            .is_some_and(|parent| matches!(parent.kind(), "class_body"));
                        if !(inner && member) {
                            return None;
                        }
                        current = owner.parent();
                        continue;
                    }
                    _ => return None,
                }
            }
            _ => current = node.parent(),
        }
    }
    None
}

/// Whether a file's imports bind `name`, which then shadows its own
/// top-level declarations of it.
fn file_imports(file: Node<'_>, name: &str, source: &[u8]) -> bool {
    let mut cursor = file.walk();
    let found = file.named_children(&mut cursor).any(|child| {
        if child.kind() != "import" {
            return false;
        }
        let text = child.utf8_text(source).unwrap_or("");
        let bound = text
            .split_whitespace()
            .last()
            .map(|last| last.rsplit('.').next().unwrap_or(last));
        bound == Some(name) || text.trim_end().ends_with('*')
    });
    found
}

fn declares_supertype(owner: Node<'_>) -> bool {
    let mut cursor = owner.walk();
    let found = owner.children(&mut cursor).any(|child| {
        matches!(
            child.kind(),
            "delegation_specifiers" | "delegation_specifier"
        )
    });
    found
}

/// A property a class body, object body or file declares directly under
/// `name`.
fn declared_property<'t>(body: Node<'t>, name: &str, source: &[u8]) -> Option<Node<'t>> {
    let mut cursor = body.walk();
    let found = body.named_children(&mut cursor).find(|member| {
        member.kind() == "property_declaration"
            && find_kind(*member, "variable_declaration")
                .and_then(|declaration| find_kind(declaration, "identifier"))
                .and_then(|identifier| identifier.utf8_text(source).ok())
                == Some(name)
    });
    found
}

/// A `val` or `var` constructor parameter a class declares under `name`.
fn constructor_property<'t>(owner: Node<'t>, name: &str, source: &[u8]) -> Option<Node<'t>> {
    let parameters = find_kind(owner, "primary_constructor")
        .and_then(|constructor| find_kind(constructor, "class_parameters"))?;
    let mut cursor = parameters.walk();
    let found = parameters.named_children(&mut cursor).find(|parameter| {
        if parameter.kind() != "class_parameter" {
            return false;
        }
        let mut inner = parameter.walk();
        let is_property = parameter
            .children(&mut inner)
            .any(|child| matches!(child.kind(), "val" | "var"));
        is_property
            && find_kind(*parameter, "identifier")
                .and_then(|identifier| identifier.utf8_text(source).ok())
                == Some(name)
    });
    found
}

/// What a property or constructor property holds: its declared type, else
/// the value it is initialized with.
fn property_value(
    declaration: Node<'_>,
    region: &Region<'_>,
    types: &TypeScope,
    source: &[u8],
) -> Option<ValueOrigin> {
    let typed = match declaration.kind() {
        "class_parameter" => {
            let mut cursor = declaration.walk();
            let found = declaration
                .named_children(&mut cursor)
                .find(|child| is_type_node(*child));
            found
        }
        _ => find_kind(declaration, "variable_declaration").and_then(|variable| {
            let mut cursor = variable.walk();
            let found = variable
                .named_children(&mut cursor)
                .find(|child| is_type_node(*child));
            found
        }),
    };
    if let Some(ty) = typed {
        return types
            .declared(ty, source)
            .map(|type_name| ValueOrigin::Declared { type_name });
    }
    if declaration.kind() != "property_declaration"
        || find_kind(declaration, "property_delegate").is_some()
    {
        return None;
    }
    // An initializer is evaluated in the class, not the function: only a
    // constructor or function called by a name the function does not rebind
    // reads the same there.
    let value = property_initializer(declaration)?;
    let callee = (value.kind() == "call_expression")
        .then(|| value.named_child(0))
        .flatten()
        .filter(|callee| callee.kind() == "identifier")?;
    let name = callee.utf8_text(source).ok()?;
    if region.bindings.contains_key(name) || region.declared_callables.contains(name) {
        return None;
    }
    Some(ValueOrigin::Call {
        callee: name.to_string(),
    })
}

/// The type names a function's facts must not take at face value: the type
/// parameters in scope, each with the one bound it declares, the types
/// declared inside a function body around it, and the file's type aliases,
/// which stand for the type they alias.
struct TypeScope {
    parameters: HashMap<String, Option<String>>,
    local_types: HashSet<String>,
    aliases: HashMap<String, Option<String>>,
}

impl TypeScope {
    fn of(function: Node<'_>, source: &[u8]) -> Self {
        let mut scope = TypeScope {
            parameters: HashMap::new(),
            local_types: HashSet::new(),
            aliases: HashMap::new(),
        };
        let mut current = Some(function);
        let mut outermost_callable = None;
        let mut file = None;
        while let Some(node) = current {
            if matches!(
                node.kind(),
                "function_declaration" | "class_declaration" | "secondary_constructor"
            ) {
                scope.read_type_parameters(node, source);
            }
            if matches!(
                node.kind(),
                "function_declaration"
                    | "secondary_constructor"
                    | "anonymous_initializer"
                    | "lambda_literal"
                    | "anonymous_function"
                    | "getter"
                    | "setter"
            ) {
                outermost_callable = Some(node);
            }
            if node.kind() == "source_file" {
                file = Some(node);
            }
            current = node.parent();
        }
        if let Some(callable) = outermost_callable {
            let mut stack = vec![callable];
            while let Some(node) = stack.pop() {
                if node.id() != callable.id()
                    && matches!(
                        node.kind(),
                        "class_declaration" | "object_declaration" | "type_alias"
                    )
                {
                    let name = node
                        .child_by_field_name("name")
                        .or_else(|| node.child_by_field_name("type"))
                        .and_then(|name| name.utf8_text(source).ok());
                    if let Some(name) = name {
                        scope.local_types.insert(name.to_string());
                    }
                }
                let mut cursor = node.walk();
                stack.extend(node.named_children(&mut cursor));
            }
        }
        if let Some(file) = file {
            let mut cursor = file.walk();
            for alias in file.named_children(&mut cursor) {
                if alias.kind() != "type_alias" {
                    continue;
                }
                let Some(name) = alias
                    .child_by_field_name("type")
                    .and_then(|name| name.utf8_text(source).ok())
                else {
                    continue;
                };
                let mut inner = alias.walk();
                let target = alias
                    .named_children(&mut inner)
                    .filter(|child| is_type_node(*child))
                    .last()
                    .and_then(|ty| kotlin_erased_type_name(ty, source));
                // A name aliased twice stands for nothing certain.
                if scope.aliases.contains_key(name) {
                    scope.aliases.insert(name.to_string(), None);
                } else {
                    scope.aliases.insert(name.to_string(), target);
                }
            }
        }
        scope
    }

    fn read_type_parameters(&mut self, declaration: Node<'_>, source: &[u8]) {
        let Some(parameters) = find_kind(declaration, "type_parameters") else {
            return;
        };
        let constraints: Vec<(String, Option<String>)> = find_kind(declaration, "type_constraints")
            .map(|constraints| {
                let mut cursor = constraints.walk();
                let found: Vec<(String, Option<String>)> = constraints
                    .named_children(&mut cursor)
                    .filter(|constraint| constraint.kind() == "type_constraint")
                    .filter_map(|constraint| {
                        let mut inner = constraint.walk();
                        let parts: Vec<Node> = constraint.named_children(&mut inner).collect();
                        let name = parts
                            .iter()
                            .find(|part| part.kind() == "identifier")?
                            .utf8_text(source)
                            .ok()?
                            .to_string();
                        let bound = parts
                            .iter()
                            .find(|part| is_type_node(**part))
                            .and_then(|ty| kotlin_erased_type_name(*ty, source));
                        Some((name, bound))
                    })
                    .collect();
                found
            })
            .unwrap_or_default();
        let mut cursor = parameters.walk();
        for parameter in parameters.named_children(&mut cursor) {
            if parameter.kind() != "type_parameter" {
                continue;
            }
            let mut inner = parameter.walk();
            let parts: Vec<Node> = parameter.named_children(&mut inner).collect();
            let Some(name) = parts
                .iter()
                .find(|part| part.kind() == "identifier")
                .and_then(|name| name.utf8_text(source).ok())
            else {
                continue;
            };
            if self.parameters.contains_key(name) {
                continue;
            }
            let mut bounds: Vec<Option<String>> = parts
                .iter()
                .filter(|part| is_type_node(**part))
                .map(|ty| kotlin_erased_type_name(*ty, source))
                .collect();
            bounds.extend(
                constraints
                    .iter()
                    .filter(|(constrained, _)| constrained == name)
                    .map(|(_, bound)| bound.clone()),
            );
            let bound = match bounds.as_slice() {
                [Some(one)] => Some(one.clone()),
                _ => None,
            };
            self.parameters.insert(name.to_string(), bound);
        }
    }

    fn is_local_or_parameter(&self, name: &str) -> bool {
        self.parameters.contains_key(name) || self.local_types.contains(name)
    }

    /// The erased class a declared type names, for a variable of that type.
    fn declared(&self, ty: Node<'_>, source: &[u8]) -> Option<String> {
        let name = kotlin_erased_type_name(ty, source)?;
        self.resolve(name, 0)
    }

    fn resolve(&self, name: String, depth: usize) -> Option<String> {
        let root = name.split('.').next().unwrap_or(&name).to_string();
        if let Some(bound) = self.parameters.get(&root) {
            let bound = bound.as_ref().filter(|_| !name.contains('.'))?;
            return self.resolve(bound.clone(), depth + 1).filter(|_| depth < 4);
        }
        if self.local_types.contains(&root) {
            return None;
        }
        if let Some(target) = self.aliases.get(&root) {
            if depth >= 4 || name.contains('.') {
                return None;
            }
            return self.resolve(target.clone()?, depth + 1);
        }
        Some(name)
    }

    /// The erased class a function declares it returns. A type parameter is
    /// the caller's, and names nothing here.
    fn returned(&self, ty: Node<'_>, source: &[u8]) -> Option<String> {
        let name = kotlin_erased_type_name(ty, source)?;
        let root = name.split('.').next().unwrap_or(&name);
        if self.parameters.contains_key(root) {
            return None;
        }
        self.resolve(name, 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::LanguageAdapter;

    fn facts(source: &str) -> FileFacts {
        let tree = crate::languages::KotlinAdapter
            .parse(source.as_bytes())
            .unwrap();
        assert!(!tree.root_node().has_error(), "fixture must parse");
        read_file(&tree, source.as_bytes(), "src/main/kotlin/app/Service.kt")
    }

    fn scope<'a>(facts: &'a FileFacts, name: &str) -> &'a ScopeFacts {
        let matching: Vec<_> = facts
            .by_node
            .iter()
            .filter(|(_, leaf, _)| leaf == name)
            .collect();
        assert_eq!(matching.len(), 1, "one function named {name}");
        &matching[0].2
    }

    fn declared(name: &str) -> ValueOrigin {
        ValueOrigin::Declared {
            type_name: name.into(),
        }
    }

    const SERVICE: &str = r#"package app

import app.store.Store
import javax.inject.Inject

typealias Repo = app.data.Repository

class Service @Inject constructor(
    private val store: Store,
    val cache: Cache,
    plain: Int,
) {
    @Inject lateinit var audit: AuditLog
    private val mapper = OrderMapper()

    fun build(item: Store?, repo: Repo, count: Int = 1): Order {
        val fresh = SqlStore()
        val typed: Store = SqlStore()
        var moved = SqlStore()
        moved = SqlStore()
        val opened = typed.open()
        val alias = fresh
        val cast = lookup() as Store
        for (each in stores) { each.load() }
        store.load()
        cache.get()
        audit.record()
        mapper.map()
        repo.save()
        items.forEach { entry -> entry.load() }
        return fresh.load()
    }

    fun <T : Base> generic(t: T, list: List<Store>): T {
        t.run()
        list.size()
        return t
    }

    fun narrowed(s: Store) {
        if (s is SqlStore) { s.flush() }
    }
}
"#;

    #[test]
    fn declared_injected_and_constructed_names_are_typed() {
        let facts = facts(SERVICE);
        let build = scope(&facts, "build");
        assert_eq!(build.bindings.get("item"), Some(&declared("Store")));
        assert_eq!(
            build.bindings.get("repo"),
            Some(&declared("app.data.Repository")),
            "a type alias of the file stands for the type it aliases"
        );
        assert_eq!(build.bindings.get("count"), Some(&declared("Int")));
        assert_eq!(
            build.bindings.get("fresh"),
            Some(&ValueOrigin::Call {
                callee: "SqlStore".into()
            }),
            "`val x = C()` holds what C makes"
        );
        assert_eq!(build.bindings.get("typed"), Some(&declared("Store")));
        assert_eq!(
            build.bindings.get("moved"),
            Some(&ValueOrigin::Call {
                callee: "SqlStore".into()
            }),
            "a `var`'s type is fixed by its initializer"
        );
        assert_eq!(
            build.bindings.get("opened"),
            Some(&ValueOrigin::MethodCall {
                receiver: Box::new(ValueOrigin::Local {
                    name: "typed".into()
                }),
                method: "open".into(),
            })
        );
        assert_eq!(
            build.bindings.get("alias"),
            Some(&ValueOrigin::Local {
                name: "fresh".into()
            })
        );
        assert_eq!(build.bindings.get("cast"), Some(&declared("Store")));
        assert!(
            !build.bindings.contains_key("each"),
            "an untyped loop variable"
        );
        assert!(
            !build.bindings.contains_key("entry"),
            "an untyped lambda parameter"
        );
        assert_eq!(
            build.bindings.get("store"),
            Some(&declared("Store")),
            "a constructor property of an injected constructor"
        );
        assert_eq!(build.bindings.get("cache"), Some(&declared("Cache")));
        assert_eq!(build.bindings.get("audit"), Some(&declared("AuditLog")));
        assert_eq!(
            build.bindings.get("mapper"),
            Some(&ValueOrigin::Call {
                callee: "OrderMapper".into()
            })
        );
        assert!(!build.bindings.contains_key("plain"), "not a property");
        assert_eq!(
            build.returns,
            Some(DeclaredReturn::Type {
                type_name: "Order".into()
            })
        );
    }

    #[test]
    fn type_parameters_bind_through_one_bound_and_smart_casts_withdraw_a_fact() {
        let facts = facts(SERVICE);
        let generic = scope(&facts, "generic");
        assert_eq!(generic.bindings.get("t"), Some(&declared("Base")));
        assert_eq!(generic.bindings.get("list"), Some(&declared("List")));
        assert_eq!(generic.returns, None);
        assert!(
            scope(&facts, "narrowed").bindings.is_empty(),
            "a name tested with `is` may be smart cast"
        );
    }

    #[test]
    fn a_supertype_an_extension_or_a_nested_scope_stops_the_property_walk() {
        let facts = facts(
            r#"
class Outer(val store: Store) {
    inner class Inner {
        fun a() { store.load() }
    }
    class Nested {
        fun b() { store.load() }
    }
    fun Cache.c() { store.load() }
}

class Child(val store: Store) : Base() {
    class Deep : Base() {
        fun d() { store.load() }
    }
    fun e() { store.load() }
}
"#,
        );
        assert_eq!(
            scope(&facts, "a").bindings.get("store"),
            Some(&declared("Store"))
        );
        assert!(
            scope(&facts, "b").bindings.is_empty(),
            "a nested class sees no outer instance"
        );
        assert!(
            scope(&facts, "c").bindings.is_empty(),
            "an extension receiver's members come first"
        );
        assert!(
            scope(&facts, "d").bindings.is_empty(),
            "Base may declare `store`"
        );
        assert_eq!(
            scope(&facts, "e").bindings.get("store"),
            Some(&declared("Store")),
            "a class's own property is found before its supertypes are consulted"
        );
    }

    #[test]
    fn a_name_declared_twice_or_read_outside_its_block_carries_no_fact() {
        let facts = facts(
            r#"
class Service(val store: Store) {
    fun run(flag: Boolean) {
        if (flag) { val store = cache(); store.get() }
        store.load()
        run { val a = Store(); a.load() }
        run { val a = Cache(); a.get() }
    }
}
"#,
        );
        let run = scope(&facts, "run");
        assert_eq!(
            run.bindings.keys().collect::<Vec<_>>(),
            vec!["flag"],
            "`store` is read outside the block that shadows it, and `a` is declared twice: \
             {run:?}"
        );
    }
}
