// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Java value origins: the declared types of parameters, locals, loop and
//! resource variables, catch parameters and fields, the value a `var` local is
//! initialized with, and the types methods declare they return.
//!
//! Java fixes a variable's static type where it declares it, and a call
//! through the variable dispatches from that type, so a declared type holds at
//! every read whatever is assigned later. A field or constructor parameter a
//! dependency-injection container fills (`@Inject`, `@Autowired`) is no
//! different: the container hands it an instance of its declared type, and a
//! call through it binds to that type's method. When the type is an interface
//! the call binds to the interface's method, never to a guessed
//! implementation. Generic types are read erased (`Repository<User>` is
//! `Repository`), and a variable typed by a type parameter takes the one bound
//! the parameter declares, or nothing when it declares none or several.
//!
//! A local is read only when the method declares its name exactly once and
//! every other use of the name sits where that declaration is visible: Java
//! scopes locals to their block, so the same name can mean another local in a
//! sibling block or a field outside the block. A `var` local's initializer is
//! read only when nothing assigns the local again. A field is read for a name
//! the method never declares, through the class that declares it; a class that
//! names a supertype may inherit a same-named field, so the walk outward stops
//! there. A call written inside an anonymous or local class body in the method
//! never carries a receiver these facts type, since that class's inherited
//! fields can shadow any name: the adapter stops at such a body.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use tree_sitter::{Node, Tree};

use super::{DeclaredReturn, FileFacts, ScopeFacts, ValueOrigin};
use crate::languages::java::{java_declares_supertype, java_erased_type_name};

pub(super) fn read_file(tree: &Tree, source: &[u8], _file_path: &str) -> FileFacts {
    let mut facts = FileFacts::default();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if matches!(
            node.kind(),
            "method_declaration" | "constructor_declaration"
        ) {
            if let Some(name) = node
                .child_by_field_name("name")
                .and_then(|name| name.utf8_text(source).ok())
            {
                let scope = method_scope(node, source);
                facts.by_node.push((
                    vec![(node.start_byte(), node.end_byte())],
                    name.to_string(),
                    scope,
                ));
            }
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    facts
}

/// The names a method's facts type, which are the receivers a call written in
/// it may carry for the linker to follow.
pub(crate) fn receiver_names(method: Node<'_>, source: &[u8]) -> HashSet<String> {
    method_scope(method, source).bindings.into_keys().collect()
}

fn method_scope(method: Node<'_>, source: &[u8]) -> ScopeFacts {
    let types = TypeScope::of(method, source);
    let mut scope = ScopeFacts {
        returns: (method.kind() == "method_declaration")
            .then(|| method.child_by_field_name("type"))
            .flatten()
            .and_then(|ty| types.returned(ty, source))
            .map(|type_name| DeclaredReturn::Type { type_name }),
        ..ScopeFacts::default()
    };
    let region = Region::of(method, source);
    let mut bindings: BTreeMap<String, ValueOrigin> = BTreeMap::new();
    for (name, declared) in &region.bindings {
        let [binding] = declared.as_slice() else {
            continue;
        };
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
            Origin::Initialized(value) if !region.reassigned.contains(name) => {
                initializer_origin(value, &types, source)
            }
            _ => None,
        };
        if let Some(origin) = origin {
            bindings.insert(name.clone(), origin);
        }
    }
    // A name the method never declares and does read is a field.
    for name in &region.used {
        if region.bindings.contains_key(name) || bindings.contains_key(name) {
            continue;
        }
        if let Some(type_name) = field_type(method, name, &types, source) {
            bindings.insert(name.clone(), ValueOrigin::Declared { type_name });
        }
    }
    // `this.name` reads the field whatever a local of the name shadows.
    for name in this_receivers(method, source) {
        if let Some(type_name) = field_type(method, &name, &types, source) {
            bindings.insert(format!("this.{name}"), ValueOrigin::Declared { type_name });
        }
    }
    scope.bindings = bindings;
    scope
}

/// The fields a method calls through written `this.name.method(...)`, outside
/// any class body nested in it, where `this` is another instance.
fn this_receivers(method: Node<'_>, source: &[u8]) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let mut stack = vec![method];
    while let Some(node) = stack.pop() {
        if node.id() != method.id()
            && matches!(node.kind(), "class_body" | "interface_body" | "enum_body")
        {
            continue;
        }
        if node.kind() == "method_invocation" {
            if let Some(name) = node
                .child_by_field_name("object")
                .and_then(|object| this_field(object, source))
            {
                names.insert(name.to_string());
            }
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    names
}

/// `name` of an expression written `this.name`.
pub(crate) fn this_field<'s>(node: Node<'_>, source: &'s [u8]) -> Option<&'s str> {
    if node.kind() != "field_access" {
        return None;
    }
    let object = node.child_by_field_name("object")?;
    let field = node.child_by_field_name("field")?;
    (object.kind() == "this" && field.kind() == "identifier")
        .then(|| field.utf8_text(source).ok())
        .flatten()
}

/// What a declaration says about the value its name holds.
enum Origin<'t> {
    /// A declared type.
    Declared(Node<'t>),
    /// A `var` local, typed by its initializer.
    Initialized(Node<'t>),
    /// A declaration whose type the reader does not take: a pattern, a
    /// varargs array, an untyped lambda parameter.
    Unread,
}

struct Binding<'t> {
    origin: Origin<'t>,
    /// The byte range the declaration is visible in.
    visible: (usize, usize),
}

/// One method's declarations and uses, outside any class body nested in it,
/// whose names belong to that class's scope.
struct Region<'t> {
    bindings: HashMap<String, Vec<Binding<'t>>>,
    /// Where each name is read, by start byte.
    references: HashMap<String, Vec<usize>>,
    /// Names read as a call's receiver or as another binding's value, which
    /// are the ones a field binding is worth recording for.
    used: HashSet<String>,
    /// Names a `var` local is assigned again under.
    reassigned: HashSet<String>,
}

impl<'t> Region<'t> {
    fn of(method: Node<'t>, source: &[u8]) -> Self {
        let mut region = Region {
            bindings: HashMap::new(),
            references: HashMap::new(),
            used: HashSet::new(),
            reassigned: HashSet::new(),
        };
        if let Some(parameters) = method.child_by_field_name("parameters") {
            region.parameters(parameters, (method.start_byte(), method.end_byte()), source);
        }
        if let Some(body) = method.child_by_field_name("body") {
            region.walk(body, source);
        }
        region
    }

    fn bind(&mut self, name: Node<'t>, origin: Origin<'t>, visible: (usize, usize), source: &[u8]) {
        let Ok(text) = name.utf8_text(source) else {
            return;
        };
        self.bindings
            .entry(text.to_string())
            .or_default()
            .push(Binding { origin, visible });
    }

    fn parameters(&mut self, parameters: Node<'t>, visible: (usize, usize), source: &[u8]) {
        let mut cursor = parameters.walk();
        for parameter in parameters.named_children(&mut cursor) {
            match parameter.kind() {
                "formal_parameter" => {
                    let (Some(name), Some(ty)) = (
                        parameter.child_by_field_name("name"),
                        parameter.child_by_field_name("type"),
                    ) else {
                        continue;
                    };
                    // `int[] a` spells its dimensions on the name.
                    let origin = if parameter.child_by_field_name("dimensions").is_some() {
                        Origin::Unread
                    } else {
                        Origin::Declared(ty)
                    };
                    self.bind(name, origin, visible, source);
                }
                "spread_parameter" => {
                    let mut inner = parameter.walk();
                    for child in parameter.named_children(&mut inner) {
                        if child.kind() == "variable_declarator" {
                            if let Some(name) = child.child_by_field_name("name") {
                                self.bind(name, Origin::Unread, visible, source);
                            }
                        }
                    }
                }
                "identifier" => self.bind(parameter, Origin::Unread, visible, source),
                _ => {}
            }
        }
    }

    fn walk(&mut self, node: Node<'t>, source: &[u8]) {
        match node.kind() {
            // A class body nested in the method is its own scope: its
            // inherited fields can shadow any name written in it.
            "class_declaration"
            | "record_declaration"
            | "interface_declaration"
            | "annotation_type_declaration"
            | "enum_declaration"
            | "class_body" => return,
            "local_variable_declaration" => {
                let visible_end = scope_end(node);
                let ty = node.child_by_field_name("type");
                let inferred = ty.is_some_and(|ty| {
                    ty.kind() == "type_identifier" && ty.utf8_text(source).ok() == Some("var")
                });
                let mut cursor = node.walk();
                for declarator in node.named_children(&mut cursor) {
                    if declarator.kind() != "variable_declarator" {
                        continue;
                    }
                    let Some(name) = declarator.child_by_field_name("name") else {
                        continue;
                    };
                    let origin = if declarator.child_by_field_name("dimensions").is_some() {
                        Origin::Unread
                    } else if inferred {
                        declarator
                            .child_by_field_name("value")
                            .map_or(Origin::Unread, Origin::Initialized)
                    } else {
                        ty.map_or(Origin::Unread, Origin::Declared)
                    };
                    self.bind(name, origin, (declarator.start_byte(), visible_end), source);
                }
            }
            "enhanced_for_statement" => {
                if let (Some(name), Some(ty)) = (
                    node.child_by_field_name("name"),
                    node.child_by_field_name("type"),
                ) {
                    let origin = if node.child_by_field_name("dimensions").is_some() {
                        Origin::Unread
                    } else {
                        Origin::Declared(ty)
                    };
                    self.bind(name, origin, (node.start_byte(), node.end_byte()), source);
                }
            }
            "resource" => {
                if let Some(name) = node.child_by_field_name("name") {
                    let ty = node.child_by_field_name("type");
                    let inferred = ty.is_some_and(|ty| ty.utf8_text(source).ok() == Some("var"));
                    let origin = if inferred {
                        node.child_by_field_name("value")
                            .map_or(Origin::Unread, Origin::Initialized)
                    } else {
                        ty.map_or(Origin::Unread, Origin::Declared)
                    };
                    let statement = node
                        .parent()
                        .and_then(|specification| specification.parent())
                        .unwrap_or(node);
                    self.bind(
                        name,
                        origin,
                        (statement.start_byte(), statement.end_byte()),
                        source,
                    );
                }
            }
            "catch_formal_parameter" => {
                if let Some(name) = node.child_by_field_name("name") {
                    let mut cursor = node.walk();
                    let catch_type = node
                        .named_children(&mut cursor)
                        .find(|child| child.kind() == "catch_type");
                    // `catch (A | B e)` names no single type.
                    let origin = catch_type
                        .filter(|catch_type| catch_type.named_child_count() == 1)
                        .and_then(|catch_type| catch_type.named_child(0))
                        .map_or(Origin::Unread, Origin::Declared);
                    let clause = node.parent().unwrap_or(node);
                    self.bind(
                        name,
                        origin,
                        (clause.start_byte(), clause.end_byte()),
                        source,
                    );
                }
            }
            "lambda_expression" => {
                let visible = (node.start_byte(), node.end_byte());
                if let Some(parameters) = node.child_by_field_name("parameters") {
                    match parameters.kind() {
                        "identifier" => self.bind(parameters, Origin::Unread, visible, source),
                        "formal_parameters" => self.parameters(parameters, visible, source),
                        "inferred_parameters" => {
                            let mut cursor = parameters.walk();
                            for name in parameters.named_children(&mut cursor) {
                                if name.kind() == "identifier" {
                                    self.bind(name, Origin::Unread, visible, source);
                                }
                            }
                        }
                        _ => {}
                    }
                }
                if let Some(body) = node.child_by_field_name("body") {
                    self.walk(body, source);
                }
                return;
            }
            // Pattern variables have flow-sensitive scope; they are counted
            // so they block every other binding of their name, and read as
            // nothing.
            "instanceof_expression" => {
                if let Some(name) = node.child_by_field_name("name") {
                    self.bind(name, Origin::Unread, (0, usize::MAX), source);
                }
            }
            "type_pattern" | "record_pattern_component" => {
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    if child.kind() == "identifier" {
                        self.bind(child, Origin::Unread, (0, usize::MAX), source);
                    }
                }
            }
            "assignment_expression" => {
                if let Some(left) = node.child_by_field_name("left") {
                    if left.kind() == "identifier" {
                        if let Ok(name) = left.utf8_text(source) {
                            self.reassigned.insert(name.to_string());
                        }
                    }
                }
            }
            "method_invocation" => {
                if let Some(object) = node.child_by_field_name("object") {
                    if object.kind() == "identifier" {
                        if let Ok(name) = object.utf8_text(source) {
                            self.used.insert(name.to_string());
                        }
                    }
                }
            }
            "variable_declarator" => {
                if let Some(value) = node.child_by_field_name("value") {
                    if value.kind() == "identifier" {
                        if let Ok(name) = value.utf8_text(source) {
                            self.used.insert(name.to_string());
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
}

/// Where a local declared by `statement` stops being visible: the end of the
/// block, loop header or switch that holds it.
fn scope_end(statement: Node<'_>) -> usize {
    let Some(parent) = statement.parent() else {
        return statement.end_byte();
    };
    if parent.kind() == "switch_block_statement_group" {
        return parent.parent().unwrap_or(parent).end_byte();
    }
    parent.end_byte()
}

/// Whether an identifier node reads a variable. Declared names, method names,
/// field names after a dot, method-reference names, labels and annotation
/// names never do; everything else might, and counting a doubtful one only
/// withholds a fact.
fn is_reference(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return true;
    };
    let field_is = |field: &str| {
        parent
            .child_by_field_name(field)
            .is_some_and(|held| held.id() == node.id())
    };
    match parent.kind() {
        "method_invocation" => !field_is("name"),
        "field_access" => !field_is("field"),
        "variable_declarator"
        | "formal_parameter"
        | "catch_formal_parameter"
        | "enhanced_for_statement"
        | "resource"
        | "instanceof_expression" => !field_is("name"),
        "method_reference" => parent
            .named_child(0)
            .is_some_and(|first| first.id() == node.id()),
        "lambda_expression" => !field_is("parameters"),
        "inferred_parameters"
        | "labeled_statement"
        | "break_statement"
        | "continue_statement"
        | "marker_annotation"
        | "annotation"
        | "type_pattern"
        | "record_pattern_component" => false,
        _ => true,
    }
}

/// The origin of the value a `var` local is initialized with: a constructor,
/// a cast, another name, or a method called on one.
fn initializer_origin(value: Node<'_>, types: &TypeScope, source: &[u8]) -> Option<ValueOrigin> {
    match value.kind() {
        "parenthesized_expression" => initializer_origin(value.named_child(0)?, types, source),
        "object_creation_expression" => {
            // An anonymous class is a subclass the source does not name.
            let mut cursor = value.walk();
            if value
                .children(&mut cursor)
                .any(|child| child.kind() == "class_body")
            {
                return None;
            }
            // `outer.new Inner()` creates a member class of `outer`'s type.
            if value.named_child(0).is_some_and(|first| {
                value
                    .child_by_field_name("type")
                    .is_some_and(|ty| ty.id() != first.id())
            }) {
                return None;
            }
            let callee = types.declared(value.child_by_field_name("type")?, source)?;
            Some(ValueOrigin::Call { callee })
        }
        "cast_expression" => {
            // `(A & B) x` names an intersection.
            if value.named_child_count() != 2 {
                return None;
            }
            let type_name = types.declared(value.child_by_field_name("type")?, source)?;
            Some(ValueOrigin::Declared { type_name })
        }
        "identifier" => Some(ValueOrigin::Local {
            name: value.utf8_text(source).ok()?.to_string(),
        }),
        "method_invocation" => {
            let object = value.child_by_field_name("object")?;
            let method = value.child_by_field_name("name")?.utf8_text(source).ok()?;
            let receiver = initializer_origin(object, types, source)?;
            Some(ValueOrigin::MethodCall {
                receiver: Box::new(receiver),
                method: method.to_string(),
            })
        }
        _ => None,
    }
}

/// The declared type of the field `name` a method in this class reads: the
/// class's own, or, for a class that names no supertype, the one an enclosing
/// class declares. A class declared inside a method body ends the walk, since
/// the name there may be a captured local instead.
fn field_type(method: Node<'_>, name: &str, types: &TypeScope, source: &[u8]) -> Option<String> {
    let mut current = method.parent();
    while let Some(body) = current {
        let owner = match body.kind() {
            "class_body" | "interface_body" | "annotation_type_body" => body.parent()?,
            "enum_body_declarations" => body.parent()?.parent()?,
            _ => {
                current = body.parent();
                continue;
            }
        };
        if let Some(ty) = declared_field(owner, body, name, source) {
            return types.declared(ty, source);
        }
        let declaration = match owner.kind() {
            // An anonymous class may inherit the name from the type it
            // instantiates.
            "object_creation_expression" => return None,
            // An enum constant's body is a subclass of its enum, and inherits
            // the enum's fields.
            "enum_constant" => {
                let declaration = owner.parent()?.parent()?;
                if let Some(ty) = declared_field_of(declaration, name, source) {
                    return types.declared(ty, source);
                }
                declaration
            }
            _ => owner,
        };
        if java_declares_supertype(&declaration, source) || !is_member_or_top_level(declaration) {
            return None;
        }
        current = declaration.parent();
    }
    None
}

/// Whether a type declaration sits at the top of its file or directly in
/// another type's body. A type declared in a method body sees the method's
/// locals, which the facts of its own methods do not follow.
fn is_member_or_top_level(declaration: Node<'_>) -> bool {
    declaration.parent().is_some_and(|parent| {
        matches!(
            parent.kind(),
            "program"
                | "class_body"
                | "interface_body"
                | "enum_body_declarations"
                | "annotation_type_body"
        )
    })
}

/// The type of the field `name` a type declaration declares directly.
fn declared_field_of<'t>(declaration: Node<'t>, name: &str, source: &[u8]) -> Option<Node<'t>> {
    let body = declaration.child_by_field_name("body")?;
    if let Some(ty) = declared_field(declaration, body, name, source) {
        return Some(ty);
    }
    let mut cursor = body.walk();
    let declarations = body
        .named_children(&mut cursor)
        .find(|child| child.kind() == "enum_body_declarations")?;
    declared_field(declaration, declarations, name, source)
}

/// The type of the field `name` the members of `body` declare, or a record
/// component of `owner`.
fn declared_field<'t>(
    owner: Node<'t>,
    body: Node<'t>,
    name: &str,
    source: &[u8],
) -> Option<Node<'t>> {
    if owner.kind() == "record_declaration" {
        if let Some(parameters) = owner.child_by_field_name("parameters") {
            let mut cursor = parameters.walk();
            for component in parameters.named_children(&mut cursor) {
                if component.kind() == "formal_parameter"
                    && component
                        .child_by_field_name("name")
                        .and_then(|n| n.utf8_text(source).ok())
                        == Some(name)
                {
                    return component.child_by_field_name("type");
                }
            }
        }
    }
    let mut cursor = body.walk();
    for member in body.named_children(&mut cursor) {
        if !matches!(member.kind(), "field_declaration" | "constant_declaration") {
            continue;
        }
        let mut inner = member.walk();
        for declarator in member.named_children(&mut inner) {
            if declarator.kind() == "variable_declarator"
                && declarator
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
                    == Some(name)
            {
                if declarator.child_by_field_name("dimensions").is_some() {
                    return None;
                }
                return member.child_by_field_name("type");
            }
        }
    }
    None
}

/// The type names a method's facts must not take at face value: the type
/// parameters in scope, each with the one bound it declares, and every type
/// declared inside a method body around it, which a same-named top-level type
/// elsewhere would otherwise answer for.
struct TypeScope {
    parameters: HashMap<String, Option<String>>,
    local_types: HashSet<String>,
}

impl TypeScope {
    fn of(method: Node<'_>, source: &[u8]) -> Self {
        let mut scope = TypeScope {
            parameters: HashMap::new(),
            local_types: HashSet::new(),
        };
        let mut current = Some(method);
        let mut outermost_callable = None;
        while let Some(node) = current {
            if matches!(
                node.kind(),
                "method_declaration"
                    | "constructor_declaration"
                    | "class_declaration"
                    | "interface_declaration"
                    | "record_declaration"
            ) {
                if let Some(parameters) = node.child_by_field_name("type_parameters") {
                    scope.read_type_parameters(parameters, source);
                }
            }
            if matches!(
                node.kind(),
                "method_declaration"
                    | "constructor_declaration"
                    | "compact_constructor_declaration"
                    | "lambda_expression"
                    | "static_initializer"
            ) {
                outermost_callable = Some(node);
            }
            current = node.parent();
        }
        if let Some(callable) = outermost_callable {
            let mut stack = vec![callable];
            while let Some(node) = stack.pop() {
                if node.id() != callable.id()
                    && matches!(
                        node.kind(),
                        "class_declaration"
                            | "record_declaration"
                            | "interface_declaration"
                            | "annotation_type_declaration"
                            | "enum_declaration"
                    )
                {
                    if let Some(name) = node
                        .child_by_field_name("name")
                        .and_then(|name| name.utf8_text(source).ok())
                    {
                        scope.local_types.insert(name.to_string());
                    }
                }
                let mut cursor = node.walk();
                stack.extend(node.named_children(&mut cursor));
            }
        }
        scope
    }

    fn read_type_parameters(&mut self, parameters: Node<'_>, source: &[u8]) {
        let mut cursor = parameters.walk();
        for parameter in parameters.named_children(&mut cursor) {
            if parameter.kind() != "type_parameter" {
                continue;
            }
            let mut inner = parameter.walk();
            let children: Vec<Node> = parameter.named_children(&mut inner).collect();
            let Some(name) = children
                .iter()
                .find(|child| child.kind() == "type_identifier")
                .and_then(|name| name.utf8_text(source).ok())
            else {
                continue;
            };
            // The innermost declaration of a name wins.
            if self.parameters.contains_key(name) {
                continue;
            }
            let bound = children
                .iter()
                .find(|child| child.kind() == "type_bound")
                .and_then(|bound| {
                    let mut cursor = bound.walk();
                    let types: Vec<Node> = bound.named_children(&mut cursor).collect();
                    match types.as_slice() {
                        [one] => java_erased_type_name(*one, source),
                        _ => None,
                    }
                });
            self.parameters.insert(name.to_string(), bound);
        }
    }

    /// The erased class a declared type names, for a variable of that type.
    fn declared(&self, ty: Node<'_>, source: &[u8]) -> Option<String> {
        let name = java_erased_type_name(ty, source)?;
        let root = name.split('.').next().unwrap_or(&name);
        if let Some(bound) = self.parameters.get(root) {
            // A type parameter's members are its bound's.
            let bound = bound.as_ref().filter(|_| !name.contains('.'))?;
            let bound_root = bound.split('.').next().unwrap_or(bound);
            if self.parameters.contains_key(bound_root) || self.local_types.contains(bound_root) {
                return None;
            }
            return Some(bound.clone());
        }
        if self.local_types.contains(root) {
            return None;
        }
        Some(name)
    }

    /// The erased class a method declares it returns. A type parameter is
    /// left alone: the caller's type argument, not the bound, is what the call
    /// returns.
    fn returned(&self, ty: Node<'_>, source: &[u8]) -> Option<String> {
        let name = java_erased_type_name(ty, source)?;
        let root = name.split('.').next().unwrap_or(&name);
        if self.parameters.contains_key(root) || self.local_types.contains(root) {
            return None;
        }
        Some(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::LanguageAdapter;

    fn facts(source: &str) -> FileFacts {
        let tree = crate::languages::JavaAdapter
            .parse(source.as_bytes())
            .unwrap();
        assert!(!tree.root_node().has_error(), "fixture must parse");
        read_file(&tree, source.as_bytes(), "src/main/java/app/Service.java")
    }

    fn scope<'a>(facts: &'a FileFacts, name: &str) -> &'a ScopeFacts {
        let matching: Vec<_> = facts
            .by_node
            .iter()
            .filter(|(_, leaf, _)| leaf == name)
            .collect();
        assert_eq!(matching.len(), 1, "one method named {name}");
        &matching[0].2
    }

    fn declared(name: &str) -> ValueOrigin {
        ValueOrigin::Declared {
            type_name: name.into(),
        }
    }

    #[test]
    fn a_call_through_this_field_is_typed_by_the_field_and_not_a_shadowing_local() {
        let facts = facts(
            "package app;\n\nclass Service {\n    private Cache cache;\n\n    \
             int serve(Store cache) {\n        Runnable r = new Runnable() {\n            \
             public void run() { this.cache.load(); }\n        };\n        \
             return this.cache.load() + cache.load();\n    }\n}\n",
        );
        let serve = scope(&facts, "serve");
        assert_eq!(serve.bindings.get("this.cache"), Some(&declared("Cache")));
        assert_eq!(
            serve.bindings.get("cache"),
            Some(&declared("Store")),
            "the parameter shadows the field for a bare read"
        );
    }

    const SERVICE: &str = r#"
package app;

import app.store.Store;
import java.util.List;
import javax.inject.Inject;

public class Service {
    @Inject private Store store;
    @Autowired Cache cache;
    private final List<Store> stores;
    private int count;

    @Inject
    public Service(Store injected, @Named("x") Cache c) {
        this.store = injected;
    }

    public Store build(Repository<User> repo, int n) {
        var fresh = new SqlStore();
        Store typed = new SqlStore();
        var opened = typed.open();
        var alias = fresh;
        var cast = (Store) lookup();
        for (Store each : stores) { each.load(); }
        try (Store res = open()) { res.load(); }
        store.load();
        cache.get();
        repo.save(null);
        return typed;
    }

    public <T extends Base> T generic(T item, List<? extends Store> items) {
        item.run();
        return item;
    }

    void count() { count++; }
}
"#;

    #[test]
    fn declared_injected_and_constructed_names_are_typed() {
        let facts = facts(SERVICE);
        let build = scope(&facts, "build");
        assert_eq!(build.bindings.get("typed"), Some(&declared("Store")));
        assert_eq!(
            build.bindings.get("fresh"),
            Some(&ValueOrigin::Call {
                callee: "SqlStore".into()
            }),
            "`var x = new C()` holds what C's constructor makes"
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
        assert_eq!(build.bindings.get("each"), Some(&declared("Store")));
        assert_eq!(build.bindings.get("res"), Some(&declared("Store")));
        assert_eq!(
            build.bindings.get("repo"),
            Some(&declared("Repository")),
            "a generic type binds through its erased type"
        );
        assert_eq!(
            build.bindings.get("store"),
            Some(&declared("Store")),
            "an @Inject field is typed by its declaration"
        );
        assert_eq!(build.bindings.get("cache"), Some(&declared("Cache")));
        assert!(
            !build.bindings.contains_key("n"),
            "a primitive names no class"
        );
        assert!(
            !build.bindings.contains_key("count"),
            "a field the method does not read is not recorded"
        );
        assert_eq!(
            build.returns,
            Some(DeclaredReturn::Type {
                type_name: "Store".into()
            })
        );

        let constructor = scope(&facts, "Service");
        assert_eq!(
            constructor.bindings.get("injected"),
            Some(&declared("Store"))
        );
        assert_eq!(constructor.bindings.get("c"), Some(&declared("Cache")));
        assert_eq!(constructor.returns, None);
    }

    #[test]
    fn a_type_parameter_binds_through_its_one_bound_and_returns_nothing() {
        let facts = facts(SERVICE);
        let generic = scope(&facts, "generic");
        assert_eq!(generic.bindings.get("item"), Some(&declared("Base")));
        assert_eq!(generic.bindings.get("items"), Some(&declared("List")));
        assert_eq!(
            generic.returns, None,
            "a returned type parameter is the caller's"
        );
    }

    #[test]
    fn a_name_declared_twice_or_shadowing_a_field_in_a_block_carries_no_fact() {
        let facts = facts(
            r#"
class Service {
    Store store;
    void run(boolean flag) {
        if (flag) { Cache store = cache(); store.get(); }
        store.load();
        { Store a = s(); a.load(); }
        { Cache a = c(); a.get(); }
        var moved = new SqlStore();
        moved = other();
        moved.load();
    }
}
"#,
        );
        let run = scope(&facts, "run");
        assert!(
            !run.bindings.contains_key("store"),
            "a block-scoped local read outside its block names the field: {run:?}"
        );
        assert!(!run.bindings.contains_key("a"), "two sibling locals");
        assert!(
            !run.bindings.contains_key("moved"),
            "a `var` assigned again no longer holds its initializer"
        );
    }

    #[test]
    fn a_nested_class_body_is_its_own_scope_and_a_local_class_types_nothing() {
        let facts = facts(
            r#"
class Service {
    Store store;
    void run() {
        Store captured = open();
        executor.submit(new Task() {
            public void call() { captured.load(); store.load(); }
        });
        class Local { void go() {} }
        Local local = new Local();
        local.go();
    }
}
"#,
        );
        let run = scope(&facts, "run");
        assert_eq!(
            run.bindings.keys().collect::<Vec<_>>(),
            vec!["captured"],
            "the uses of `store` sit in the anonymous class body, whose calls the adapter \
             never gives a receiver, and a local class is not the top-level class its name \
             would resolve to: {run:?}"
        );
    }

    #[test]
    fn a_field_is_read_through_the_class_that_declares_it_and_not_past_a_supertype() {
        let facts = facts(
            r#"
class Outer {
    Store store;
    Cache cache;
    class Inner {
        void a() { store.load(); }
    }
    class Child extends Base {
        void b() { cache.get(); }
    }
}
record Pair(Store left, Cache right) {
    void c() { left.load(); }
}
"#,
        );
        assert_eq!(
            scope(&facts, "a").bindings.get("store"),
            Some(&declared("Store")),
            "a member class naming no supertype sees its outer class's field"
        );
        assert!(
            scope(&facts, "b").bindings.is_empty(),
            "Base may declare a `cache` field that shadows the outer one"
        );
        assert_eq!(
            scope(&facts, "c").bindings.get("left"),
            Some(&declared("Store")),
            "a record component is a field"
        );
    }
}
