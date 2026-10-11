// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! C's and C++'s facts for the value-escape record (see
//! [`crate::value_escape`]). One walk serves both grammars: the C++ grammar
//! extends the C one, so every node kind a C tree holds means the same in a
//! C++ tree, and the C++-only kinds never appear in a C tree. C and C++ files
//! share one calling domain, so both are recorded or neither is.
//!
//! Scoping follows the languages: a parameter or local is visible from its
//! declarator to the end of its block, and a read of one is never recorded.
//! Every other name read as a value is: a function or global read by name, a
//! qualified name (`ns::helper`, `&Store::save`), and a member read off a
//! value (`ops->save`, `this->handler`). Where scoping could only be
//! approximated (a name a macro declares), the walk treats the name as not
//! local, which records more, never less.
//!
//! A name the file defines with internal linkage (`static`) is read as the
//! file's own declaration. Every other plain name may be declared in a header
//! and defined, or overloaded, in another file, so it is read as free. Inside
//! a member function an unqualified name may be a member of the class or of a
//! base, or a namespace-level name, so it is read as a member of the class
//! unless the class body in this file declares it.
//!
//! Recorded:
//!
//! - every read of a name whose value flows on: an argument (a callback, a
//!   `std::function`, `std::bind` or `std::thread` target), an initializer, an
//!   assignment, a return, a braced list, a member initializer. A designated
//!   initializer (`{ .save = store_save }`), an assignment into a member
//!   (`ops->save = store_save`) and a namespace- or class-scope initializer
//!   alias the value under the member or variable name, so a call spelling
//!   that name is followed. Never a callee, a declaration, a receiver, or an
//!   inert use (a condition, a comparison, `sizeof`, `decltype`);
//! - reflective sites: `dlsym` and `GetProcAddress` by a computed name
//!   (a literal names the symbol and is recorded as a use of it), `dlopen` by
//!   a computed path, and Qt's `QMetaObject::invokeMethod` by a computed
//!   member name;
//! - every macro body as a region the grammar does not parse, with the names
//!   it spells: a macro can call or hold any function it names;
//! - protocol members: operators and conversions, constructors (a
//!   declaration, a copy or a conversion runs one without a call that spells
//!   it), destructors, and virtual members, which a call through a base
//!   class reaches without naming the derived class.

use std::collections::{HashMap, HashSet};

use kin_model::{
    FileEscapeRecord, ModuleScope, OccurrenceBinding, OccurrenceContext, ProtocolMember,
    ReceiverKind, ReflectiveForm, ReflectiveReach, ReflectiveSite, UnparsedRegion, ValueOccurrence,
    EXCERPT_MAX, RECEIVER_TEXT_MAX,
};
use tree_sitter::{Node, Tree};

use crate::value_escape::{
    collapsed_text, is_field, is_name, line_of, text_of, unparsed_lines, FileEscapeCensus,
    ProtocolStamp,
};

/// Functions that find a symbol by its name at run time.
const SYMBOL_LOOKUPS: &[&str] = &["dlsym", "dlvsym", "GetProcAddress"];

/// Functions that load a library by a path computed at run time.
const LIBRARY_LOADS: &[&str] = &["dlopen", "LoadLibrary", "LoadLibraryA", "LoadLibraryW"];

/// Take the census of one C or C++ file. `cpp` says the tree came from the
/// C++ adapter, which parses the source after its own preprocessing.
pub(crate) fn census(tree: &Tree, source: &[u8], cpp: bool) -> FileEscapeCensus {
    let preprocessed;
    let source = if cpp {
        preprocessed = crate::languages::cpp_lang::preprocess_cpp_source(source);
        preprocessed.as_slice()
    } else {
        source
    };
    let root = tree.root_node();
    let mut walker = Walker {
        source,
        record: FileEscapeRecord::new(ModuleScope::Closed),
        protocol_members: Vec::new(),
        file: FileNames::read(root, source),
        callables: Vec::new(),
    };
    walker.record.unparsed = unparsed_lines(tree, source);
    walker.visit(root);
    walker
        .record
        .unparsed
        .sort_by_key(|region| (region.first, region.last));
    FileEscapeCensus {
        record: walker.record,
        protocol_members: walker.protocol_members,
        non_enumerable: Vec::new(),
    }
}

// ── What the file declares ────────────────────────────────────────────────

/// The names a file declares at namespace scope and in its class bodies.
#[derive(Default)]
struct FileNames {
    /// Every namespace-scope declaration of a name (definition or prototype),
    /// by line, with whether it has internal linkage.
    module: HashMap<String, Vec<(u32, bool)>>,
    /// Every member a class body in this file declares, by class, with the
    /// lines. A class with two bodies in the file keeps no entry.
    classes: HashMap<String, HashMap<String, Vec<u32>>>,
    class_bodies: HashMap<String, usize>,
}

impl FileNames {
    fn read(root: Node<'_>, source: &[u8]) -> Self {
        let mut names = Self::default();
        names.read_scope(root, source, false);
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            if matches!(
                node.kind(),
                "class_specifier" | "struct_specifier" | "union_specifier"
            ) {
                names.read_class(node, source);
            }
            let mut cursor = node.walk();
            stack.extend(node.children(&mut cursor));
        }
        names
    }

    /// Declarations directly at namespace scope, through namespaces, linkage
    /// blocks, templates and preprocessor conditionals.
    fn read_scope(&mut self, node: Node<'_>, source: &[u8], anonymous: bool) {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            match child.kind() {
                "function_definition" | "declaration" => {
                    let internal = anonymous || has_storage(child, source, "static");
                    for (name, _) in declared_names(child, source) {
                        if name.contains("::") {
                            continue;
                        }
                        self.module
                            .entry(name)
                            .or_default()
                            .push((line_of(child), internal));
                    }
                }
                "namespace_definition" => {
                    let unnamed = child.child_by_field_name("name").is_none();
                    if let Some(body) = child.child_by_field_name("body") {
                        self.read_scope(body, source, anonymous || unnamed);
                    }
                }
                "template_declaration"
                | "linkage_specification"
                | "declaration_list"
                | "preproc_ifdef"
                | "preproc_if"
                | "preproc_else"
                | "preproc_elif" => {
                    self.read_scope(child, source, anonymous);
                }
                _ => {}
            }
        }
    }

    fn read_class(&mut self, class: Node<'_>, source: &[u8]) {
        let (Some(name), Some(body)) = (
            class.child_by_field_name("name"),
            class.child_by_field_name("body"),
        ) else {
            return;
        };
        let name = text_of(name, source).to_string();
        let bodies = self.class_bodies.entry(name.clone()).or_default();
        *bodies += 1;
        if *bodies > 1 {
            self.classes.remove(&name);
            return;
        }
        let mut members: HashMap<String, Vec<u32>> = HashMap::new();
        let mut cursor = body.walk();
        for member in body.children(&mut cursor) {
            let member = if member.kind() == "template_declaration" {
                let mut inner = member.walk();
                let found = member.named_children(&mut inner).find(|child| {
                    matches!(
                        child.kind(),
                        "function_definition" | "declaration" | "field_declaration"
                    )
                });
                match found {
                    Some(found) => found,
                    None => continue,
                }
            } else {
                member
            };
            if !matches!(
                member.kind(),
                "function_definition" | "declaration" | "field_declaration"
            ) {
                continue;
            }
            for (member_name, _) in declared_names(member, source) {
                members
                    .entry(member_name)
                    .or_default()
                    .push(line_of(member));
            }
        }
        self.classes.insert(name, members);
    }

    /// The one namespace-scope declaration line of a name, when it has one.
    fn module_line(&self, name: &str) -> Option<u32> {
        match self.module.get(name)?.as_slice() {
            [(line, _)] => Some(*line),
            _ => None,
        }
    }

    fn internal(&self, name: &str) -> bool {
        self.module
            .get(name)
            .is_some_and(|declared| declared.iter().any(|(_, internal)| *internal))
    }
}

/// Whether a declaration carries a storage class keyword.
fn has_storage(node: Node<'_>, source: &[u8], keyword: &str) -> bool {
    let mut cursor = node.walk();
    let found = node.children(&mut cursor).any(|child| {
        child.kind() == "storage_class_specifier" && text_of(child, source) == keyword
    });
    found
}

/// Every name a declaration or definition declares, with the node that
/// spells it. A function's name is its declarator's name as written
/// (`Store::save` stays qualified); a variable's is its identifier.
fn declared_names<'tree>(node: Node<'tree>, source: &[u8]) -> Vec<(String, Node<'tree>)> {
    let mut out = Vec::new();
    let mut cursor = node.walk();
    for declarator in node.children_by_field_name("declarator", &mut cursor) {
        if let Some(name) = declarator_name_node(declarator) {
            let spelled = text_of(name, source).trim();
            if !spelled.is_empty() {
                out.push((spelled.to_string(), name));
            }
        }
    }
    out
}

/// The node naming what a declarator declares: through pointers,
/// references, arrays, initializers, parentheses and function declarators.
pub(crate) fn declarator_name_node(node: Node<'_>) -> Option<Node<'_>> {
    let mut current = node;
    for _ in 0..16 {
        match current.kind() {
            "identifier"
            | "field_identifier"
            | "qualified_identifier"
            | "destructor_name"
            | "operator_name"
            | "type_identifier" => return Some(current),
            "pointer_declarator"
            | "reference_declarator"
            | "array_declarator"
            | "init_declarator"
            | "function_declarator"
            | "parenthesized_declarator"
            | "attributed_declarator" => {
                current = current
                    .child_by_field_name("declarator")
                    .or_else(|| current.named_child(0))?;
            }
            _ => return None,
        }
    }
    None
}

// ── Locals ────────────────────────────────────────────────────────────────

/// One local declaration: a parameter, a local variable, a loop, condition
/// or catch variable, a lambda parameter or capture, a structured binding.
#[derive(Debug, Clone)]
pub(crate) struct Local {
    pub(crate) name: String,
    pub(crate) visible_from: usize,
    pub(crate) end: usize,
}

/// Every local a callable's subtree declares, with where it is visible.
pub(crate) fn locals_of(callable: Node<'_>, source: &[u8]) -> Vec<Local> {
    let mut out = Vec::new();
    let mut stack = vec![callable];
    while let Some(node) = stack.pop() {
        match node.kind() {
            "parameter_declaration"
            | "optional_parameter_declaration"
            | "variadic_parameter_declaration" => {
                if let Some(name) = node
                    .child_by_field_name("declarator")
                    .and_then(declarator_name_node)
                    .filter(|name| name.kind() == "identifier")
                {
                    let scope = parameter_scope(node);
                    out.push(Local {
                        name: text_of(name, source).to_string(),
                        visible_from: scope.0,
                        end: scope.1,
                    });
                }
            }
            "declaration" => {
                // A local class's member declarations are members, not
                // locals; they never reach here because a class body is not
                // a block.
                let scope = block_scope(node);
                let mut cursor = node.walk();
                for declarator in node.children_by_field_name("declarator", &mut cursor) {
                    for name in bound_identifiers(declarator) {
                        out.push(Local {
                            name: text_of(name, source).to_string(),
                            visible_from: name.end_byte(),
                            end: scope.1,
                        });
                    }
                }
            }
            "for_range_loop" => {
                if let Some(declarator) = node.child_by_field_name("declarator") {
                    for name in bound_identifiers(declarator) {
                        out.push(Local {
                            name: text_of(name, source).to_string(),
                            visible_from: name.end_byte(),
                            end: node.end_byte(),
                        });
                    }
                }
            }
            "lambda_capture_initializer" => {
                if let Some(left) = node
                    .child_by_field_name("left")
                    .filter(|left| left.kind() == "identifier")
                {
                    let lambda = ancestor(node, "lambda_expression").unwrap_or(node);
                    out.push(Local {
                        name: text_of(left, source).to_string(),
                        visible_from: left.end_byte(),
                        end: lambda.end_byte(),
                    });
                }
            }
            // A label is visible in its whole function, and names nothing a
            // file declares: a macro handed one (`ERR(0, L_error)`) reads the
            // label.
            "labeled_statement" => {
                if let Some(label) = node.child_by_field_name("label") {
                    out.push(Local {
                        name: text_of(label, source).to_string(),
                        visible_from: callable.start_byte(),
                        end: callable.end_byte(),
                    });
                }
            }
            // A local class's members and its member functions' locals are
            // their own scope, read when the walk enters them.
            "field_declaration_list" => continue,
            _ => {}
        }
        let mut cursor = node.walk();
        stack.extend(node.children(&mut cursor));
    }
    out
}

/// Every identifier a declarator binds: one, or each of a structured
/// binding's.
pub(crate) fn bound_identifiers(declarator: Node<'_>) -> Vec<Node<'_>> {
    let mut target = declarator;
    if target.kind() == "init_declarator" {
        match target.child_by_field_name("declarator") {
            Some(inner) => target = inner,
            None => return Vec::new(),
        }
    }
    if target.kind() == "reference_declarator" {
        if let Some(inner) = target.named_child(0) {
            target = inner;
        }
    }
    if target.kind() == "structured_binding_declarator" {
        let mut cursor = target.walk();
        return target
            .named_children(&mut cursor)
            .filter(|child| child.kind() == "identifier")
            .collect();
    }
    declarator_name_node(target)
        .filter(|name| name.kind() == "identifier")
        .into_iter()
        .collect()
}

/// The bytes a parameter is visible in: its function, lambda or catch
/// clause, or its own prototype.
pub(crate) fn parameter_scope(parameter: Node<'_>) -> (usize, usize) {
    let holder = parameter
        .parent()
        .and_then(|list| list.parent())
        .unwrap_or(parameter);
    let scope = match holder.kind() {
        "catch_clause" => holder,
        "abstract_function_declarator" => holder
            .parent()
            .filter(|parent| parent.kind() == "lambda_expression")
            .unwrap_or(holder),
        "function_declarator" => {
            let mut current = holder;
            let mut found = holder;
            while let Some(parent) = current.parent() {
                match parent.kind() {
                    "pointer_declarator" | "reference_declarator" | "parenthesized_declarator" => {
                        current = parent;
                    }
                    "function_definition" => {
                        found = parent;
                        break;
                    }
                    _ => break,
                }
            }
            found
        }
        _ => holder,
    };
    (scope.start_byte(), scope.end_byte())
}

/// The bytes a local declaration's names are visible in: its block, or the
/// whole statement a condition or init-statement declares for.
pub(crate) fn block_scope(declaration: Node<'_>) -> (usize, usize) {
    let Some(parent) = declaration.parent() else {
        return (declaration.start_byte(), declaration.end_byte());
    };
    let holder = match parent.kind() {
        "init_statement" | "condition_clause" => {
            let clause = if parent.kind() == "init_statement" {
                parent.parent().unwrap_or(parent)
            } else {
                parent
            };
            clause.parent().unwrap_or(clause)
        }
        _ => parent,
    };
    (holder.start_byte(), holder.end_byte())
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

// ── The walk ──────────────────────────────────────────────────────────────

/// A callable the walk is inside.
struct Callable {
    locals: Vec<Local>,
    /// The class a member function belongs to, and whether its body sits in
    /// that class's body in this file.
    owner: Option<(String, bool)>,
    /// Locals bound once to a lambda, which the adapter admits as nested
    /// entities: a read of one is a read of that declaration.
    lambdas: Vec<(Local, u32)>,
}

/// Where the value a name read holds goes.
enum Flow {
    Value(OccurrenceContext, Vec<String>),
}

impl Flow {
    fn value(context: OccurrenceContext) -> Self {
        Self::Value(context, Vec::new())
    }
}

struct Walker<'s> {
    source: &'s [u8],
    record: FileEscapeRecord,
    protocol_members: Vec<ProtocolStamp>,
    file: FileNames,
    callables: Vec<Callable>,
}

impl<'s> Walker<'s> {
    fn text(&self, node: Node<'_>) -> &'s str {
        text_of(node, self.source)
    }

    fn visit(&mut self, node: Node<'_>) {
        match node.kind() {
            "function_definition" => {
                self.stamp_protocol(node);
                let owner = self.member_owner(node);
                let locals = locals_of(node, self.source);
                let lambdas = crate::languages::cpp_nested::local_lambdas(node, self.source)
                    .into_iter()
                    .map(|lambda| {
                        (
                            Local {
                                name: lambda.name,
                                visible_from: lambda.visible_from,
                                end: lambda.end,
                            },
                            lambda.line,
                        )
                    })
                    .collect();
                self.callables.push(Callable {
                    locals,
                    owner,
                    lambdas,
                });
                self.visit_children(node);
                self.callables.pop();
                return;
            }
            "lambda_expression" if self.callables.is_empty() => {
                let locals = locals_of(node, self.source);
                self.callables.push(Callable {
                    locals,
                    owner: None,
                    lambdas: Vec::new(),
                });
                self.visit_children(node);
                self.callables.pop();
                return;
            }
            "declaration" | "field_declaration" => self.stamp_protocol(node),
            "preproc_def" | "preproc_function_def" => {
                self.record_macro_body(node);
                return;
            }
            "identifier" => {
                self.read_identifier(node);
                return;
            }
            "qualified_identifier" => {
                self.read_qualified(node);
                return;
            }
            "template_function" => {
                if let Some(name) = node.child_by_field_name("name") {
                    if let Some(flow) = self.context_of(node) {
                        self.record_plain(name, self.text(name), flow);
                    }
                }
                return;
            }
            "field_expression" => {
                self.read_member(node);
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

    /// The class a function definition is a member of: the class body it
    /// sits in, or the owner its qualified name spells.
    fn member_owner(&self, function: Node<'_>) -> Option<(String, bool)> {
        let mut parent = function.parent()?;
        if parent.kind() == "template_declaration" {
            parent = parent.parent()?;
        }
        if parent.kind() == "field_declaration_list" {
            let class = parent.parent()?;
            let name = class.child_by_field_name("name")?;
            return Some((self.text(name).to_string(), true));
        }
        let declarator = function.child_by_field_name("declarator")?;
        let name = declarator_name_node(declarator)?;
        if name.kind() != "qualified_identifier" {
            return None;
        }
        let spelled = self.text(name);
        let (owner, _) = spelled.rsplit_once("::")?;
        let owner = owner.rsplit("::").next().unwrap_or(owner);
        let owner = owner.split('<').next().unwrap_or(owner).trim();
        (!owner.is_empty()).then(|| (owner.to_string(), false))
    }

    // ── Reads ─────────────────────────────────────────────────────────────

    fn read_identifier(&mut self, node: Node<'_>) {
        // The name half of a qualified name or a template call is read with
        // the node around it.
        if node.parent().is_some_and(|parent| {
            matches!(parent.kind(), "qualified_identifier" | "template_function")
        }) {
            return;
        }
        let Some(flow) = self.context_of(node) else {
            return;
        };
        let name = self.text(node);
        self.record_plain(node, name, flow);
    }

    /// Record a read of a plain name, bound by the scopes around it.
    fn record_plain(&mut self, node: Node<'_>, name: &str, flow: Flow) {
        if name.is_empty() {
            return;
        }
        let at = node.start_byte();
        if let Some(callable) = self.callables.last() {
            if let Some((_, line)) = callable.lambdas.iter().find(|(lambda, _)| {
                lambda.name == name && lambda.visible_from <= at && at < lambda.end
            }) {
                let line = *line;
                self.push(
                    node,
                    name,
                    flow,
                    OccurrenceBinding::Local,
                    None,
                    None,
                    Some(line),
                );
                return;
            }
            let local = self.callables.iter().any(|callable| {
                callable
                    .locals
                    .iter()
                    .any(|local| local.name == name && local.visible_from <= at && at < local.end)
            });
            if local {
                return;
            }
        }
        let owner = self
            .callables
            .iter()
            .rev()
            .find_map(|callable| callable.owner.clone());
        if let Some((owner, inline)) = owner {
            let declared = inline
                .then(|| {
                    self.file
                        .classes
                        .get(&owner)
                        .and_then(|members| members.get(name))
                })
                .flatten()
                .cloned();
            match declared {
                Some(lines) => {
                    let target = (lines.len() == 1).then(|| lines[0]);
                    self.push(
                        node,
                        name,
                        flow,
                        OccurrenceBinding::Class,
                        None,
                        None,
                        target,
                    );
                }
                None => {
                    // A member of the class or a base, or a namespace-level
                    // name: a member read with an unknown receiver covers
                    // both.
                    self.push(
                        node,
                        name,
                        flow,
                        OccurrenceBinding::Member,
                        Some(owner),
                        Some(ReceiverKind::Other),
                        None,
                    );
                }
            }
            return;
        }
        if self.file.internal(name) {
            let target = self.file.module_line(name);
            self.push(
                node,
                name,
                flow,
                OccurrenceBinding::Module,
                None,
                None,
                target,
            );
        } else {
            self.push(node, name, flow, OccurrenceBinding::Free, None, None, None);
        }
    }

    fn read_qualified(&mut self, node: Node<'_>) {
        let Some(flow) = self.context_of(node) else {
            return;
        };
        let Some(name) = node.child_by_field_name("name") else {
            return;
        };
        let leaf_node = if name.kind() == "template_function" {
            match name.child_by_field_name("name") {
                Some(inner) => inner,
                None => return,
            }
        } else {
            name
        };
        // A longer path nests: the leaf is the innermost name.
        let mut leaf_node = leaf_node;
        while leaf_node.kind() == "qualified_identifier" {
            match leaf_node.child_by_field_name("name") {
                Some(inner) => leaf_node = inner,
                None => return,
            }
        }
        let leaf = self.text(leaf_node).to_string();
        if !is_name(&leaf) {
            return;
        }
        let spelled = self.text(node);
        let scope = spelled
            .rsplit_once("::")
            .map(|(scope, _)| scope.trim())
            .unwrap_or("");
        if scope.is_empty() {
            // `::helper`: the global namespace's name.
            self.record_plain(leaf_node, &leaf, flow);
            return;
        }
        if scope == "std" || scope.starts_with("std::") {
            return;
        }
        let receiver: String = scope.chars().take(RECEIVER_TEXT_MAX).collect();
        self.push(
            leaf_node,
            &leaf,
            flow,
            OccurrenceBinding::Member,
            Some(receiver),
            Some(ReceiverKind::Other),
            None,
        );
    }

    fn read_member(&mut self, node: Node<'_>) {
        let Some(field) = node.child_by_field_name("field") else {
            return;
        };
        if field.kind() != "field_identifier" {
            return;
        }
        let Some(flow) = self.context_of(node) else {
            return;
        };
        let Some(argument) = node.child_by_field_name("argument") else {
            return;
        };
        let instance = argument.kind() == "this"
            || (argument.kind() == "parenthesized_expression"
                && self.text(argument).split_whitespace().collect::<String>() == "(*this)");
        let name = self.text(field).to_string();
        self.push(
            field,
            &name,
            flow,
            OccurrenceBinding::Member,
            Some(collapsed_text(argument, self.source, RECEIVER_TEXT_MAX)),
            Some(if instance {
                ReceiverKind::Instance
            } else {
                ReceiverKind::Other
            }),
            None,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn push(
        &mut self,
        node: Node<'_>,
        name: &str,
        flow: Flow,
        binding: OccurrenceBinding,
        receiver: Option<String>,
        receiver_kind: Option<ReceiverKind>,
        target_line: Option<u32>,
    ) {
        let Flow::Value(context, aliases) = flow;
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

    /// Where the value `node` reads goes, walking up through the syntax that
    /// passes a value through unchanged, or `None` when it goes nowhere a
    /// later call could reach it through.
    fn context_of(&self, node: Node<'_>) -> Option<Flow> {
        let mut child = node;
        let mut wrapped = false;
        loop {
            let parent = child.parent()?;
            match parent.kind() {
                "parenthesized_expression" => {}
                "pointer_expression" => {
                    // `&f` hands on f; `*f` reads through it.
                }
                "cast_expression" => {
                    if !is_field(parent, "value", child) {
                        return None;
                    }
                }
                "conditional_expression" => {
                    if is_field(parent, "condition", child) {
                        return None;
                    }
                }
                "comma_expression" => {
                    if !is_field(parent, "right", child) {
                        return None;
                    }
                }
                "argument_list" => {
                    let holder = parent.parent()?;
                    return Some(match holder.kind() {
                        "field_initializer" => self.member_initializer(holder),
                        _ => Flow::value(OccurrenceContext::Argument),
                    });
                }
                "initializer_list" => {
                    return Some(match parent.parent().map(|holder| holder.kind()) {
                        Some("field_initializer") => self.member_initializer(parent.parent()?),
                        _ => Flow::value(OccurrenceContext::Collection),
                    });
                }
                "initializer_pair" => {
                    if !is_field(parent, "value", child) {
                        return None;
                    }
                    let designator = parent.child_by_field_name("designator");
                    let field = designator
                        .filter(|designator| designator.kind() == "field_designator")
                        .and_then(|designator| designator.named_child(0))
                        .map(|field| self.text(field).to_string());
                    return Some(match field {
                        Some(field) => Flow::Value(OccurrenceContext::Alias, vec![field]),
                        None => Flow::value(OccurrenceContext::Collection),
                    });
                }
                "init_declarator" => {
                    if !is_field(parent, "value", child) {
                        return None;
                    }
                    return Some(self.declared_flow(parent));
                }
                "field_declaration" | "optional_parameter_declaration" => {
                    if !is_field(parent, "default_value", child) {
                        return None;
                    }
                    if parent.kind() == "field_declaration" {
                        let names: Vec<String> = declared_names(parent, self.source)
                            .into_iter()
                            .map(|(name, _)| name)
                            .collect();
                        return Some(Flow::Value(OccurrenceContext::Alias, names));
                    }
                    return Some(Flow::value(OccurrenceContext::Assignment));
                }
                "assignment_expression" => {
                    if !is_field(parent, "right", child) {
                        return None;
                    }
                    let operator = parent
                        .child_by_field_name("operator")
                        .map(|operator| self.text(operator))
                        .unwrap_or("=");
                    if operator != "=" {
                        return None;
                    }
                    return Some(self.assignment_flow(parent));
                }
                "return_statement" | "co_return_statement" | "co_yield_statement" => {
                    return Some(Flow::value(OccurrenceContext::Return));
                }
                "lambda_capture_initializer" => {
                    return is_field(parent, "right", child)
                        .then(|| Flow::value(OccurrenceContext::Assignment));
                }
                "call_expression" => {
                    if is_field(parent, "function", child) {
                        // `(*f)(x)` and `(f)(x)` call through the name
                        // without the adapter reading it as a callee.
                        return wrapped.then(|| Flow::value(OccurrenceContext::Other));
                    }
                    return None;
                }
                "template_argument_list"
                | "throw_statement"
                | "co_await_expression"
                | "new_expression" => return Some(Flow::value(OccurrenceContext::Other)),
                "binary_expression"
                | "unary_expression"
                | "update_expression"
                | "sizeof_expression"
                | "alignof_expression"
                | "offsetof_expression"
                | "decltype"
                | "subscript_expression"
                | "subscript_argument_list"
                | "field_expression"
                | "condition_clause"
                | "if_statement"
                | "while_statement"
                | "do_statement"
                | "for_statement"
                | "switch_statement"
                | "case_statement"
                | "expression_statement"
                | "compound_statement"
                | "static_assert_declaration"
                | "delete_expression"
                | "enumerator"
                | "bitfield_clause"
                | "gnu_asm_expression"
                | "translation_unit"
                | "declaration_list"
                | "field_declaration_list"
                | "ERROR"
                | "declaration"
                | "parameter_declaration"
                | "function_declarator"
                | "pointer_declarator"
                | "reference_declarator"
                | "array_declarator"
                | "parenthesized_declarator"
                | "structured_binding_declarator"
                | "lambda_capture_specifier"
                | "using_declaration"
                | "alias_declaration"
                | "namespace_alias_definition"
                | "attribute"
                | "attribute_declaration"
                | "labeled_statement"
                | "goto_statement"
                | "requires_clause"
                | "requires_expression"
                | "concept_definition"
                | "type_descriptor"
                | "template_parameter_list"
                | "preproc_if"
                | "preproc_elif"
                | "preproc_ifdef"
                | "preproc_defined"
                | "preproc_call"
                | "field_initializer"
                | "type_definition"
                | "for_range_loop"
                | "noexcept"
                | "throw_specifier"
                | "enumerator_list"
                | "string_literal"
                | "concatenated_string"
                | "trailing_return_type"
                | "friend_declaration"
                | "template_declaration"
                | "linkage_specification"
                | "namespace_definition"
                | "access_specifier"
                | "base_class_clause"
                | "function_definition"
                | "compound_literal_expression"
                | "destructor_name"
                | "attributed_declarator"
                | "operator_cast" => return None,
                _ => return Some(Flow::value(OccurrenceContext::Other)),
            }
            child = parent;
            wrapped = true;
        }
    }

    /// A member initializer's value is held by that member.
    fn member_initializer(&self, initializer: Node<'_>) -> Flow {
        let mut cursor = initializer.walk();
        let field = initializer
            .named_children(&mut cursor)
            .find(|child| child.kind() == "field_identifier")
            .map(|field| self.text(field).to_string());
        match field {
            Some(field) => Flow::Value(OccurrenceContext::Alias, vec![field]),
            None => Flow::value(OccurrenceContext::Argument),
        }
    }

    /// Where an initializer's value goes: a local's value may travel on
    /// through that local; a namespace- or class-scope variable's is called
    /// through the variable's name.
    fn declared_flow(&self, declarator: Node<'_>) -> Flow {
        if !self.callables.is_empty() && is_local_declaration(declarator) {
            return Flow::value(OccurrenceContext::Assignment);
        }
        let names: Vec<String> = bound_identifiers(declarator)
            .into_iter()
            .map(|name| self.text(name).to_string())
            .collect();
        if names.is_empty() {
            return Flow::value(OccurrenceContext::Other);
        }
        Flow::Value(OccurrenceContext::Alias, names)
    }

    fn assignment_flow(&self, assignment: Node<'_>) -> Flow {
        let Some(left) = assignment.child_by_field_name("left") else {
            return Flow::value(OccurrenceContext::Other);
        };
        match left.kind() {
            "identifier" => {
                let name = self.text(left);
                let at = left.start_byte();
                let local = self.callables.iter().any(|callable| {
                    callable.locals.iter().any(|local| {
                        local.name == name && local.visible_from <= at && at < local.end
                    })
                });
                if local {
                    Flow::value(OccurrenceContext::Assignment)
                } else {
                    Flow::Value(OccurrenceContext::Alias, vec![name.to_string()])
                }
            }
            "field_expression" => match left.child_by_field_name("field") {
                Some(field) => {
                    Flow::Value(OccurrenceContext::Alias, vec![self.text(field).to_string()])
                }
                None => Flow::value(OccurrenceContext::Other),
            },
            "qualified_identifier" => {
                let spelled = self.text(left);
                let leaf = spelled.rsplit("::").next().unwrap_or(spelled).trim();
                Flow::Value(OccurrenceContext::Alias, vec![leaf.to_string()])
            }
            "subscript_expression" => Flow::value(OccurrenceContext::Collection),
            _ => Flow::value(OccurrenceContext::Assignment),
        }
    }

    // ── Reflective sites ──────────────────────────────────────────────────

    fn read_call(&mut self, call: Node<'_>) {
        let Some(function) = call.child_by_field_name("function") else {
            return;
        };
        let spelled = self.text(function);
        let leaf = spelled.rsplit("::").next().unwrap_or(spelled).trim();
        let arguments = call.child_by_field_name("arguments");
        let argument = |index: usize| -> Option<Node<'_>> {
            let arguments = arguments?;
            let mut cursor = arguments.walk();
            let found = arguments
                .named_children(&mut cursor)
                .filter(|arg| arg.kind() != "comment")
                .nth(index);
            found
        };
        if SYMBOL_LOOKUPS.contains(&leaf) {
            match argument(1) {
                Some(key) => match string_literal(key, self.source) {
                    Some(name) if is_name(&name) => {
                        self.push(
                            key,
                            &name,
                            Flow::value(OccurrenceContext::Argument),
                            OccurrenceBinding::Free,
                            None,
                            None,
                            None,
                        );
                    }
                    Some(_) => {}
                    None => self.push_reflective(
                        call,
                        ReflectiveForm::ComputedMember,
                        ReflectiveReach::Any,
                    ),
                },
                None => {
                    self.push_reflective(call, ReflectiveForm::ComputedMember, ReflectiveReach::Any)
                }
            }
        } else if LIBRARY_LOADS.contains(&leaf) {
            if argument(0).is_none_or(|path| string_literal(path, self.source).is_none()) {
                self.push_reflective(
                    call,
                    ReflectiveForm::DynamicImport,
                    ReflectiveReach::Nothing,
                );
            }
        } else if leaf == "invokeMethod" && spelled.contains("QMetaObject") {
            let target = argument(0);
            match argument(1)
                .and_then(|key| string_literal(key, self.source).map(|name| (key, name)))
            {
                Some((key, name)) if is_name(&name) => {
                    let receiver =
                        target.map(|target| collapsed_text(target, self.source, RECEIVER_TEXT_MAX));
                    self.push(
                        key,
                        &name,
                        Flow::value(OccurrenceContext::Argument),
                        OccurrenceBinding::Member,
                        receiver,
                        Some(ReceiverKind::Other),
                        None,
                    );
                }
                Some(_) => {}
                None => self.push_reflective(
                    call,
                    ReflectiveForm::ComputedMember,
                    ReflectiveReach::Members,
                ),
            }
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

    // ── Macros ────────────────────────────────────────────────────────────

    /// A macro body is text the grammar never parses: list every name it
    /// spells, its own parameters aside.
    fn record_macro_body(&mut self, node: Node<'_>) {
        let Some(value) = node.child_by_field_name("value") else {
            return;
        };
        let mut params: HashSet<&str> = HashSet::new();
        if let Some(list) = node.child_by_field_name("parameters") {
            let mut cursor = list.walk();
            for param in list.named_children(&mut cursor) {
                params.insert(self.text(param));
            }
        }
        let body = self.text(value);
        let mut names: Vec<String> = identifier_tokens(body)
            .into_iter()
            .filter(|token| !params.contains(token))
            .map(str::to_string)
            .collect();
        names.sort();
        names.dedup();
        if names.is_empty() {
            return;
        }
        self.record.unparsed.push(UnparsedRegion {
            first: line_of(value),
            last: value.end_position().row as u32,
            names,
        });
    }

    // ── Protocol members ──────────────────────────────────────────────────

    /// Stamp a member the language runs without a call that spells it.
    fn stamp_protocol(&mut self, node: Node<'_>) {
        let mut cursor = node.walk();
        let declarators: Vec<Node<'_>> = node
            .children_by_field_name("declarator", &mut cursor)
            .collect();
        for declarator in declarators {
            if declarator.kind() == "operator_cast" {
                // `operator bool() const`: a conversion, with no name token
                // an entity could carry beyond the keyword.
                self.stamp(declarator, "conversion");
                continue;
            }
            if !declares_function(declarator) {
                continue;
            }
            let Some(name) = declarator_name_node(declarator) else {
                continue;
            };
            let spelled = self.text(name);
            let leaf_node = if name.kind() == "qualified_identifier" {
                let mut inner = name;
                while let Some(next) = inner.child_by_field_name("name") {
                    inner = next;
                    if inner.kind() != "qualified_identifier" {
                        break;
                    }
                }
                inner
            } else {
                name
            };
            let owner = self.declaring_class(node, spelled);
            let leaf = self.text(leaf_node);
            let words = match leaf_node.kind() {
                "operator_name" => Some("operators"),
                "destructor_name" => Some("destruction at the end of a lifetime"),
                _ if owner.as_deref() == Some(leaf) => {
                    Some("construction by a declaration, a copy or a conversion")
                }
                _ if self.is_virtual(node, declarator) => {
                    Some("virtual dispatch through a call on a base class")
                }
                _ => None,
            };
            if let Some(words) = words {
                self.stamp(leaf_node, words);
            }
        }
    }

    fn stamp(&mut self, name: Node<'_>, words: &str) {
        let spelled = self.text(name);
        let leaf = spelled.rsplit(['.', ':']).next().unwrap_or(spelled).trim();
        if leaf.is_empty() {
            return;
        }
        self.protocol_members.push(ProtocolStamp {
            name: leaf.to_string(),
            at: name.start_byte(),
            member: ProtocolMember::Implicit(words.to_string()),
        });
    }

    /// The class a member declaration belongs to: the class body it sits in,
    /// or the owner its qualified name spells.
    fn declaring_class(&self, node: Node<'_>, spelled: &str) -> Option<String> {
        let mut parent = node.parent()?;
        if parent.kind() == "template_declaration" {
            parent = parent.parent()?;
        }
        if parent.kind() == "field_declaration_list" {
            let class = parent.parent()?;
            return class
                .child_by_field_name("name")
                .map(|name| self.text(name).to_string());
        }
        let (owner, _) = spelled.rsplit_once("::")?;
        let owner = owner.rsplit("::").next().unwrap_or(owner);
        Some(owner.split('<').next().unwrap_or(owner).trim().to_string())
    }

    /// Whether a member is declared `virtual`, `override` or `final`.
    fn is_virtual(&self, node: Node<'_>, declarator: Node<'_>) -> bool {
        let mut cursor = node.walk();
        let keyword = node
            .children(&mut cursor)
            .any(|child| child.kind() == "virtual" || self.text(child) == "virtual");
        if keyword {
            return true;
        }
        let mut stack = vec![declarator];
        while let Some(current) = stack.pop() {
            if current.kind() == "virtual_specifier" {
                return true;
            }
            if current.kind() == "parameter_list" {
                continue;
            }
            let mut inner = current.walk();
            stack.extend(current.children(&mut inner));
        }
        false
    }
}

/// Whether a declarator sits in a block, a condition or a loop header rather
/// than at namespace or class scope.
fn is_local_declaration(declarator: Node<'_>) -> bool {
    let mut current = declarator.parent();
    while let Some(parent) = current {
        match parent.kind() {
            "compound_statement" | "condition_clause" | "init_statement" | "for_statement"
            | "for_range_loop" | "lambda_expression" => return true,
            "field_declaration_list" | "translation_unit" | "declaration_list" => return false,
            _ => {}
        }
        current = parent.parent();
    }
    false
}

/// Whether a declarator declares a function rather than a variable.
fn declares_function(declarator: Node<'_>) -> bool {
    let mut current = declarator;
    for _ in 0..16 {
        match current.kind() {
            "function_declarator" => return true,
            "pointer_declarator" | "reference_declarator" | "attributed_declarator" => {
                current = match current
                    .child_by_field_name("declarator")
                    .or_else(|| current.named_child(0))
                {
                    Some(next) => next,
                    None => return false,
                };
            }
            _ => return false,
        }
    }
    false
}

/// The text of a string literal with no escape, prefix or concatenation.
fn string_literal(node: Node<'_>, source: &[u8]) -> Option<String> {
    if node.kind() != "string_literal" {
        return None;
    }
    let mut cursor = node.walk();
    if node
        .named_children(&mut cursor)
        .any(|child| child.kind() != "string_content")
    {
        return None;
    }
    let raw = text_of(node, source);
    let inner = raw.strip_prefix('"')?.strip_suffix('"')?;
    (!inner.contains('\\')).then(|| inner.to_string())
}

/// Every identifier-shaped token of a macro body, outside string and
/// character literals.
fn identifier_tokens(body: &str) -> Vec<&str> {
    let bytes = body.as_bytes();
    let mut out = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let byte = bytes[at];
        if byte == b'"' || byte == b'\'' {
            let quote = byte;
            at += 1;
            while at < bytes.len() && bytes[at] != quote {
                if bytes[at] == b'\\' {
                    at += 1;
                }
                at += 1;
            }
            at += 1;
            continue;
        }
        if byte == b'_' || byte.is_ascii_alphabetic() {
            let start = at;
            while at < bytes.len() && (bytes[at] == b'_' || bytes[at].is_ascii_alphanumeric()) {
                at += 1;
            }
            out.push(&body[start..at]);
            continue;
        }
        if byte.is_ascii_digit() {
            while at < bytes.len() && (bytes[at] == b'_' || bytes[at].is_ascii_alphanumeric()) {
                at += 1;
            }
            continue;
        }
        at += 1;
    }
    out
}

#[cfg(test)]
mod tests;
