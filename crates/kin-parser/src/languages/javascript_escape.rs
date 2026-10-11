// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! JavaScript's and TypeScript's facts for the value-escape record (see
//! [`crate::value_escape`]). One walk serves both grammars, and a Flow file,
//! which the JavaScript adapter parses with a TypeScript-family grammar.
//!
//! Scoping follows ECMAScript: `var` and parameters bind in the enclosing
//! function, `let`, `const`, `class` and block-level function declarations
//! bind in their block, and imports bind at module scope. A read of a plain
//! local or parameter is never recorded; a read of a nested function or
//! class declaration is. Where the scope rules could only be approximated,
//! the walk treats the name as not local, which records more, never less.
//!
//! Recorded:
//!
//! - every read of a name, member or string-keyed member whose value flows on
//!   (an argument, a JSX attribute, an assignment, a return, a container, an
//!   export), never a call's callee or a declaration;
//! - every other read of a name the file can bind, or of a member, as a
//!   mention with the form of its position: a plain import, a type position
//!   (annotations, generic arguments, `implements`, interface and type alias
//!   bodies, signatures, `typeof` in a type), a condition, a comparison, a
//!   receiver, a subscript, a decorator, a class base, a JSX element name, a
//!   store into a member, or any other inert read (`typeof`, a template
//!   substitution, an operand, an iterable, a bare expression statement);
//! - renaming imports (`import {a as b}`, default imports, renaming
//!   `require` destructuring) as import aliases, export lists, and
//!   module-scope or member rebindings to another name;
//! - reflective sites: computed member reads (`o[k]`, `Reflect.get`, a
//!   computed destructuring key), `Object.values`/`entries`, `eval`,
//!   `Function`, `with`, and `import()`/`require()` by a computed specifier,
//!   each with what its receiver is (`this`, a class or function the file
//!   declares, or an `import * as` namespace), every literal run of its key,
//!   and whether it yields only own enumerable properties;
//! - no lookup whose key is always a number (a literal, arithmetic, a counter
//!   only ever written numbers, a `number` parameter, an element of a number
//!   array, the index of a callback over an array) or a symbol, and a key
//!   written once as a string constant reads as that string;
//! - protocol sites: JSX elements, decorators, and `f.call(...)` /
//!   `f.apply(...)`;
//! - every named class with the base it extends;
//! - protocol members: accessors, `then`, conversions and `handleEvent`, and
//!   non-enumerable members: every method and accessor of a class body.

use std::collections::{HashMap, HashSet};

use kin_model::{
    ClassBases, FileEscapeRecord, LookupReceiver, MentionForm, ModuleScope, OccurrenceBinding,
    OccurrenceContext, ProtocolForm, ProtocolMember, ProtocolSite, ReceiverKind, ReflectiveForm,
    ReflectiveReach, ReflectiveSite, ValueOccurrence, EXCERPT_MAX, RECEIVER_TEXT_MAX,
};
use tree_sitter::{Node, Tree};

use crate::value_escape::{
    collapsed_text, is_field, is_name, line_of, plain_string_literal, text_of, unparsed_lines,
    FileEscapeCensus, KeyLiterals, MemberStamp, ProtocolStamp,
};

use super::javascript_key_facts::{
    callback_index, collect_writes, declared_type, greatest, integer_text, is_symbol_expression,
    numeric_call, numeric_expression, writes_any, DeclaredType, NameFacts, Need, Written,
};

/// Subtrees that only describe types: nothing in them is read at run time.
pub(super) const TYPE_ONLY: &[&str] = &[
    "type_annotation",
    "type_arguments",
    "type_parameters",
    "type_alias_declaration",
    "interface_declaration",
    "ambient_declaration",
    "abstract_method_signature",
    "method_signature",
    "property_signature",
    "index_signature",
    "function_signature",
    "implements_clause",
    "extends_type_clause",
    "type_predicate_annotation",
    "asserts_annotation",
    "opting_type_annotation",
    "omitting_type_annotation",
    "enum_declaration",
    "import_alias",
    "comment",
    "regex",
    "string",
    "number",
];

/// Whether `kind` is syntax that only describes a type, whose names the walk
/// records as [`MentionForm::Annotation`] mentions: everything in
/// [`TYPE_ONLY`] but the comments and literals, and every type node.
fn is_type_syntax(kind: &str) -> bool {
    (TYPE_ONLY.contains(&kind) && !matches!(kind, "comment" | "regex" | "string" | "number"))
        || kind.ends_with("_type")
        || matches!(
            kind,
            "type_identifier"
                | "nested_type_identifier"
                | "type_query"
                | "type_predicate"
                | "asserts"
                | "constraint"
                | "mapped_type_clause"
                | "type_parameter"
        )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScopeKind {
    Module,
    Function,
    Block,
}

#[derive(Debug, Clone, Default)]
struct Binds {
    /// Lines of function and class declarations binding the name.
    definitions: Vec<u32>,
    /// Whether anything else binds it.
    other: bool,
    /// Imports binding it: module, imported name, statement range.
    imports: Vec<(String, String, (u32, u32))>,
    /// Whether it holds a module object (`import * as`, `require(...)`).
    namespace: bool,
}

#[derive(Debug)]
struct Scope {
    kind: ScopeKind,
    names: HashMap<String, Binds>,
    /// Names this scope binds that hold only numbers (see
    /// [`Walker::settle_numeric`]).
    numeric: HashSet<String>,
    /// Whether some binding of this scope starts as a number, the only way
    /// one of its names can hold only numbers; a scope with none is not
    /// settled.
    numeric_candidate: bool,
    /// Names this scope declares as types only (an interface or a type
    /// alias), as an enum, or by an ambient declaration, with the line of
    /// each declaration and whether it also names a value (an enum, an
    /// ambient class, function, variable or namespace). A value read never
    /// resolves through these; a mention falls back on them (see
    /// [`Walker::resolve_mention`]).
    types: HashMap<String, Vec<(u32, bool)>>,
}

impl Scope {
    fn new(kind: ScopeKind) -> Self {
        Self {
            kind,
            names: HashMap::new(),
            numeric: HashSet::new(),
            numeric_candidate: false,
            types: HashMap::new(),
        }
    }
}

/// Take the census of one JavaScript or TypeScript file.
pub(crate) fn census(tree: &Tree, source: &[u8]) -> FileEscapeCensus {
    let walker = walk(tree, source);
    FileEscapeCensus {
        record: walker.record,
        protocol_members: walker.protocol_members,
        non_enumerable: walker.non_enumerable,
    }
}

/// The calls of one JavaScript or TypeScript file whose callee is a bare name
/// a function or a block around the call binds, each by the byte range of its
/// callee token.
///
/// `function measureInstance() {}` inside `attach` binds the name for every
/// call written in `attach`, whatever another module declares under it: such
/// a call runs what the local name holds. The scopes are the walk's own, so a
/// function declared in a block binds only inside that block, a parameter
/// binds in its function, and a `var` in the function that holds it. A name
/// only the module binds, by a declaration or an import, is not local.
pub(crate) fn local_callees(tree: &Tree, source: &[u8]) -> Vec<(usize, usize)> {
    walk(tree, source).local_callees
}

/// Walk one file, recording what [`census`] and [`local_callees`] read.
fn walk<'s>(tree: &Tree, source: &'s [u8]) -> Walker<'s> {
    let root = tree.root_node();
    let mut walker = Walker {
        source,
        record: FileEscapeRecord::new(ModuleScope::Closed),
        protocol_members: Vec::new(),
        non_enumerable: Vec::new(),
        scopes: Vec::new(),
        names: NameFacts::default(),
        inert: None,
        local_callees: Vec::new(),
    };
    walker.record.lists_mentions = true;
    walker.record.unparsed = unparsed_lines(tree, source);
    walker.names = NameFacts::collect(root, source);
    let mut module = Scope::new(ScopeKind::Module);
    walker.collect_hoisted(root, &mut module);
    walker.collect_lexical(root, &mut module);
    walker.settle_numeric(root, &mut module);
    walker.scopes.push(module);
    walker.visit_children(root);
    walker.scopes.pop();
    walker.record.exports.sort();
    walker.record.exports.dedup();
    walker
}

struct Walker<'s> {
    source: &'s [u8],
    record: FileEscapeRecord,
    protocol_members: Vec<ProtocolStamp>,
    /// Methods and accessors declared in a class body, which live on the
    /// prototype and are not enumerable.
    non_enumerable: Vec<MemberStamp>,
    scopes: Vec<Scope>,
    /// What every write of a name in the file says it holds.
    names: NameFacts,
    /// Set while the walk is under an expression whose value goes nowhere
    /// (an enum member initializer): every value use recorded there is a
    /// mention of this form instead.
    inert: Option<MentionForm>,
    /// The callee tokens of calls through a name a function or a block
    /// binds (see [`local_callees`]). No part of the record.
    local_callees: Vec<(usize, usize)>,
}

/// Where a read goes.
enum Flow {
    Value(OccurrenceContext, Vec<String>),
    /// Invoked: `f.call(...)`, `(0, f)(...)`.
    Invoke,
}

impl Flow {
    fn value(context: OccurrenceContext) -> Self {
        Self::Value(context, Vec::new())
    }
}

/// What the position of an expression makes of it.
enum Position {
    /// A read whose value flows on.
    Flow(Flow),
    /// A read whose value flows nowhere a later call could reach it through:
    /// a mention of this form.
    Inert(MentionForm),
    /// Not a read: a call's callee, a declaration, an assignment target, a
    /// property key, or a name an import or export visit records itself.
    Not,
}

fn is_function_like(kind: &str) -> bool {
    matches!(
        kind,
        "function_declaration"
            | "generator_function_declaration"
            | "function_expression"
            | "function"
            | "generator_function"
            | "arrow_function"
            | "method_definition"
            | "class_static_block"
    )
}

fn is_class_like(kind: &str) -> bool {
    matches!(
        kind,
        "class_declaration" | "class" | "abstract_class_declaration"
    )
}

/// Fill what a read of `name` says about the binding it reads: the one
/// declaration it reads, or the one import that binds it.
fn fill_binding(occurrence: &mut ValueOccurrence, name: &str, binds: Option<&Binds>) {
    let Some(binds) = binds else {
        return;
    };
    if binds.definitions.len() == 1 && !binds.other && binds.imports.is_empty() {
        occurrence.target_line = Some(binds.definitions[0]);
    }
    if let [(module, imported, range)] = binds.imports.as_slice() {
        occurrence.module = Some(module.clone());
        occurrence.import = Some(*range);
        if imported != name {
            occurrence.imported = Some(imported.clone());
        }
    }
}

impl<'s> Walker<'s> {
    fn text(&self, node: Node<'_>) -> &'s str {
        text_of(node, self.source)
    }

    // ── Scopes ────────────────────────────────────────────────────────────

    fn bind(&self, scope: &mut Scope, name: &str, definition: Option<u32>) {
        if name.is_empty() {
            return;
        }
        let binds = scope.names.entry(name.to_string()).or_default();
        match definition {
            Some(line) => binds.definitions.push(line),
            None => binds.other = true,
        }
    }

    /// Bind every identifier a pattern writes.
    fn bind_pattern(&self, pattern: Node<'_>, scope: &mut Scope) {
        match pattern.kind() {
            "identifier" | "shorthand_property_identifier_pattern" => {
                self.bind(scope, self.text(pattern), None)
            }
            "pair_pattern" => {
                if let Some(value) = pattern.child_by_field_name("value") {
                    self.bind_pattern(value, scope);
                }
            }
            "assignment_pattern" | "object_assignment_pattern" => {
                if let Some(left) = pattern.child_by_field_name("left") {
                    self.bind_pattern(left, scope);
                }
            }
            "required_parameter" | "optional_parameter" => {
                if let Some(inner) = pattern
                    .child_by_field_name("pattern")
                    .or_else(|| pattern.child_by_field_name("name"))
                {
                    self.bind_pattern(inner, scope);
                }
            }
            "member_expression" | "subscript_expression" | "this" | "undefined" => {}
            kind if TYPE_ONLY.contains(&kind) => {}
            _ => {
                let mut cursor = pattern.walk();
                for child in pattern.named_children(&mut cursor) {
                    self.bind_pattern(child, scope);
                }
            }
        }
    }

    /// `var` declarations anywhere under `node`, without entering a nested
    /// function: they bind in the function (or module) that holds them.
    fn collect_hoisted(&self, node: Node<'_>, scope: &mut Scope) {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if is_function_like(child.kind()) || is_class_like(child.kind()) {
                continue;
            }
            if child.kind() == "variable_declaration" {
                let mut inner = child.walk();
                for declarator in child.named_children(&mut inner) {
                    if let Some(name) = declarator.child_by_field_name("name") {
                        self.bind_declarator(declarator, name, scope);
                    }
                }
            }
            if TYPE_ONLY.contains(&child.kind()) {
                continue;
            }
            self.collect_hoisted(child, scope);
        }
    }

    /// Declarations made directly in a block or a module: `let`, `const`,
    /// classes, functions and imports.
    fn collect_lexical(&self, block: Node<'_>, scope: &mut Scope) {
        let mut cursor = block.walk();
        for child in block.named_children(&mut cursor) {
            self.collect_declaration(child, scope);
        }
    }

    fn collect_declaration(&self, node: Node<'_>, scope: &mut Scope) {
        match node.kind() {
            "lexical_declaration" => {
                let mut inner = node.walk();
                for declarator in node.named_children(&mut inner) {
                    if let Some(name) = declarator.child_by_field_name("name") {
                        self.bind_declarator(declarator, name, scope);
                    }
                }
            }
            kind if is_class_like(kind)
                || matches!(
                    kind,
                    "function_declaration" | "generator_function_declaration"
                ) =>
            {
                if let Some(name) = node.child_by_field_name("name") {
                    self.bind(scope, self.text(name), Some(line_of(node)));
                }
            }
            "export_statement" => {
                if let Some(declaration) = node.child_by_field_name("declaration") {
                    self.collect_declaration(declaration, scope);
                }
                // `export default function f() {}` binds `f` too.
                if let Some(value) = node.child_by_field_name("value") {
                    self.collect_declaration(value, scope);
                }
            }
            "import_statement" => self.bind_import(node, scope),
            "interface_declaration" | "type_alias_declaration" | "enum_declaration" => {
                if let Some(name) = node.child_by_field_name("name") {
                    let value = node.kind() == "enum_declaration";
                    scope
                        .types
                        .entry(self.text(name).to_string())
                        .or_default()
                        .push((line_of(node), value));
                }
            }
            // `declare class C`, `declare enum E`: names a value read can
            // reach only through a mention's fallback, so value reads keep
            // resolving as they did.
            "ambient_declaration" => {
                let mut inner = node.walk();
                let declarations: Vec<Node<'_>> = node.named_children(&mut inner).collect();
                for declaration in declarations {
                    self.collect_ambient(declaration, scope);
                }
            }
            "internal_module" | "module" => {
                if let Some(name) = node.child_by_field_name("name") {
                    self.bind(scope, self.text(name), None);
                }
            }
            "expression_statement" => {
                // `namespace X {}` parses as an expression statement in some
                // grammars.
                if let Some(inner) = node.named_child(0) {
                    if matches!(inner.kind(), "internal_module" | "module") {
                        self.collect_declaration(inner, scope);
                    }
                }
            }
            _ => {}
        }
    }

    /// Note the names an ambient declaration declares among `scope`'s types.
    fn collect_ambient(&self, declaration: Node<'_>, scope: &mut Scope) {
        let mut declared = |name: Node<'_>, value: bool| {
            if name.kind() == "identifier" || name.kind() == "type_identifier" {
                scope
                    .types
                    .entry(self.text(name).to_string())
                    .or_default()
                    .push((line_of(declaration), value));
            }
        };
        match declaration.kind() {
            "interface_declaration" | "type_alias_declaration" => {
                if let Some(name) = declaration.child_by_field_name("name") {
                    declared(name, false);
                }
            }
            "lexical_declaration" | "variable_declaration" => {
                let mut cursor = declaration.walk();
                for declarator in declaration.named_children(&mut cursor) {
                    if let Some(name) = declarator.child_by_field_name("name") {
                        declared(name, true);
                    }
                }
            }
            _ => {
                if let Some(name) = declaration.child_by_field_name("name") {
                    declared(name, true);
                }
            }
        }
    }

    fn bind_declarator(&self, declarator: Node<'_>, name: Node<'_>, scope: &mut Scope) {
        let value = declarator.child_by_field_name("value");
        if value.is_some_and(|value| numeric_expression(value, self.source, &mut Vec::new())) {
            scope.numeric_candidate = true;
        }
        let required = value.and_then(|value| self.required_module(value));
        match (name.kind(), required) {
            ("identifier", Some(_)) => {
                let binds = scope.names.entry(self.text(name).to_string()).or_default();
                binds.other = true;
                binds.namespace = true;
            }
            ("object_pattern", Some(module)) => {
                let range = (declarator.start_byte() as u32, declarator.end_byte() as u32);
                let mut cursor = name.walk();
                for property in name.named_children(&mut cursor) {
                    match property.kind() {
                        "shorthand_property_identifier_pattern" => {
                            let local = self.text(property).to_string();
                            scope.names.entry(local.clone()).or_default().imports.push((
                                module.clone(),
                                local,
                                range,
                            ));
                        }
                        "pair_pattern" => {
                            let key = property
                                .child_by_field_name("key")
                                .map(|key| self.text(key).to_string())
                                .unwrap_or_default();
                            match property.child_by_field_name("value") {
                                Some(value) if value.kind() == "identifier" => {
                                    scope
                                        .names
                                        .entry(self.text(value).to_string())
                                        .or_default()
                                        .imports
                                        .push((module.clone(), key, range));
                                }
                                Some(value) => self.bind_pattern(value, scope),
                                None => {}
                            }
                        }
                        _ => self.bind_pattern(property, scope),
                    }
                }
            }
            _ => self.bind_pattern(name, scope),
        }
    }

    /// The module a `require("m")` call names, when `node` is one.
    fn required_module(&self, node: Node<'_>) -> Option<String> {
        if node.kind() != "call_expression" {
            return None;
        }
        let function = node.child_by_field_name("function")?;
        if function.kind() != "identifier" || self.text(function) != "require" {
            return None;
        }
        let arguments = node.child_by_field_name("arguments")?;
        let first = arguments.named_child(0)?;
        plain_string_literal(first, self.source)
    }

    fn bind_import(&self, statement: Node<'_>, scope: &mut Scope) {
        let range = (statement.start_byte() as u32, statement.end_byte() as u32);
        let module = statement
            .child_by_field_name("source")
            .and_then(|source| plain_string_literal(source, self.source))
            .unwrap_or_default();
        let mut cursor = statement.walk();
        for clause in statement.named_children(&mut cursor) {
            if clause.kind() != "import_clause" {
                continue;
            }
            let mut inner = clause.walk();
            for part in clause.named_children(&mut inner) {
                match part.kind() {
                    "identifier" => {
                        scope
                            .names
                            .entry(self.text(part).to_string())
                            .or_default()
                            .imports
                            .push((module.clone(), "default".into(), range));
                    }
                    "namespace_import" => {
                        if let Some(name) = part.named_child(0) {
                            let binds = scope.names.entry(self.text(name).to_string()).or_default();
                            binds.namespace = true;
                            binds.imports.push((module.clone(), "*".into(), range));
                        }
                    }
                    "named_imports" => {
                        let mut specifiers = part.walk();
                        for specifier in part.named_children(&mut specifiers) {
                            if specifier.kind() != "import_specifier" {
                                continue;
                            }
                            let Some(name) = specifier.child_by_field_name("name") else {
                                continue;
                            };
                            let imported = self.text(name).to_string();
                            let local = specifier
                                .child_by_field_name("alias")
                                .map(|alias| self.text(alias).to_string())
                                .unwrap_or_else(|| imported.clone());
                            scope.names.entry(local).or_default().imports.push((
                                module.clone(),
                                imported,
                                range,
                            ));
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    /// How a read of `name` binds, or `None` for a plain local.
    fn resolve(&self, name: &str) -> Option<(OccurrenceBinding, Option<&Binds>)> {
        for scope in self.scopes.iter().rev() {
            let Some(binds) = scope.names.get(name) else {
                continue;
            };
            return match scope.kind {
                ScopeKind::Module => Some((
                    if binds.definitions.is_empty() && !binds.other && !binds.imports.is_empty() {
                        OccurrenceBinding::Import
                    } else {
                        OccurrenceBinding::Module
                    },
                    Some(binds),
                )),
                ScopeKind::Function | ScopeKind::Block => (!binds.definitions.is_empty()
                    && !binds.other)
                    .then_some((OccurrenceBinding::Local, Some(binds))),
            };
        }
        Some((OccurrenceBinding::Free, None))
    }

    /// How a mention of `name` binds. A read in a type position
    /// (`type_position`) reads TypeScript's type namespace: an interface, a
    /// type alias, an enum, a class or an import binds there, and a plain
    /// local, parameter or variable does not, so it never hides a type. A
    /// read anywhere else binds as [`Self::resolve`] says, except that a name
    /// no value binding of the file holds falls back on an enum or ambient
    /// declaration of an enclosing scope. `None` for a plain local. The
    /// answer carries the declaration line a type or ambient name settles.
    fn resolve_mention(
        &self,
        name: &str,
        type_position: bool,
    ) -> Option<(OccurrenceBinding, Option<&Binds>, Option<u32>)> {
        let declared_type = |scope: &Scope, values_only: bool| {
            let lines = scope.types.get(name)?;
            let lines: Vec<u32> = lines
                .iter()
                .filter(|(_, value)| *value || !values_only)
                .map(|(line, _)| *line)
                .collect();
            if lines.is_empty() {
                return None;
            }
            let binding = if scope.kind == ScopeKind::Module {
                OccurrenceBinding::Module
            } else {
                OccurrenceBinding::Local
            };
            let target = (lines.len() == 1 && !scope.names.contains_key(name)).then(|| lines[0]);
            Some((binding, None, target))
        };
        if !type_position {
            return match self.resolve(name) {
                Some((OccurrenceBinding::Free, _)) => Some(
                    self.scopes
                        .iter()
                        .rev()
                        .find_map(|scope| declared_type(scope, true))
                        .unwrap_or((OccurrenceBinding::Free, None, None)),
                ),
                Some((binding, binds)) => Some((binding, binds, None)),
                None => None,
            };
        }
        for scope in self.scopes.iter().rev() {
            if let Some(found) = declared_type(scope, false) {
                return Some(found);
            }
            let Some(binds) = scope.names.get(name) else {
                continue;
            };
            // A variable or parameter binds no type.
            if binds.definitions.is_empty() && binds.imports.is_empty() && !binds.namespace {
                continue;
            }
            let binding = match scope.kind {
                ScopeKind::Module
                    if binds.definitions.is_empty()
                        && !binds.other
                        && !binds.imports.is_empty() =>
                {
                    OccurrenceBinding::Import
                }
                ScopeKind::Module => OccurrenceBinding::Module,
                ScopeKind::Function | ScopeKind::Block => OccurrenceBinding::Local,
            };
            return Some((binding, Some(binds), None));
        }
        Some((OccurrenceBinding::Free, None, None))
    }

    fn is_numeric_name(&self, name: &str) -> bool {
        for scope in self.scopes.iter().rev() {
            if scope.numeric.contains(name) {
                return true;
            }
            if scope.names.contains_key(name) {
                return false;
            }
        }
        false
    }

    /// Whether the walk stands directly at module scope, where a `let`,
    /// `const` or destructuring binds a module-level name.
    fn at_module_scope(&self) -> bool {
        self.scopes.len() == 1
    }

    /// Whether `declarator` binds at module scope: a `var` hoists out of
    /// blocks to its function, a `let` or `const` stays in its block.
    fn declarator_at_module(&self, declarator: Node<'_>) -> bool {
        match declarator.parent().map(|declaration| declaration.kind()) {
            Some("variable_declaration") => self
                .scopes
                .iter()
                .all(|scope| scope.kind != ScopeKind::Function),
            _ => self.at_module_scope(),
        }
    }

    // ── The walk ──────────────────────────────────────────────────────────

    fn visit_children(&mut self, node: Node<'_>) {
        let mut cursor = node.walk();
        let children: Vec<Node<'_>> = node.named_children(&mut cursor).collect();
        for child in children {
            self.visit(child);
        }
    }

    fn visit(&mut self, node: Node<'_>) {
        let kind = node.kind();
        if is_type_syntax(kind) {
            self.visit_type(node);
            return;
        }
        if TYPE_ONLY.contains(&kind) {
            return;
        }
        match kind {
            "identifier" => self.read_name(node, node),
            "shorthand_property_identifier" => self.read_name(node, node),
            "member_expression" => self.visit_member(node),
            "subscript_expression" => self.visit_subscript(node),
            "call_expression" => self.visit_call(node),
            "new_expression" => {
                if let Some(constructor) = node.child_by_field_name("constructor") {
                    if constructor.kind() == "identifier" {
                        self.note_local_callee(constructor);
                    }
                    if !matches!(constructor.kind(), "identifier" | "member_expression") {
                        self.visit(constructor);
                    } else if constructor.kind() == "member_expression" {
                        if let Some(object) = constructor.child_by_field_name("object") {
                            self.visit_receiver(object, MentionForm::Receiver);
                        }
                    }
                    if constructor.kind() == "identifier" && self.text(constructor) == "Function" {
                        self.push_reflective(
                            node,
                            ReflectiveForm::Evaluate,
                            ReflectiveReach::Any,
                            None,
                            None,
                            false,
                        );
                    }
                }
                if let Some(type_arguments) = node.child_by_field_name("type_arguments") {
                    self.visit_type(type_arguments);
                }
                if let Some(arguments) = node.child_by_field_name("arguments") {
                    self.visit(arguments);
                }
            }
            kind if is_function_like(kind) => self.visit_function(node),
            kind if is_class_like(kind) => self.visit_class(node),
            "statement_block" | "switch_body" | "class_body" => {
                let mut scope = Scope::new(ScopeKind::Block);
                self.collect_lexical(node, &mut scope);
                self.settle_numeric(node, &mut scope);
                self.scopes.push(scope);
                self.visit_children(node);
                self.scopes.pop();
            }
            "for_statement" => self.visit_for(node),
            "for_in_statement" => self.visit_for_in(node),
            "catch_clause" => {
                let mut scope = Scope::new(ScopeKind::Block);
                if let Some(parameter) = node.child_by_field_name("parameter") {
                    self.bind_pattern(parameter, &mut scope);
                }
                self.scopes.push(scope);
                if let Some(annotation) = node.child_by_field_name("type") {
                    self.visit_type(annotation);
                }
                if let Some(body) = node.child_by_field_name("body") {
                    self.visit(body);
                }
                self.scopes.pop();
            }
            "variable_declarator" => self.visit_declarator(node),
            "assignment_expression" => self.visit_assignment(node),
            "augmented_assignment_expression" => {
                if let Some(right) = node.child_by_field_name("right") {
                    self.visit(right);
                }
                if let Some(left) = node.child_by_field_name("left") {
                    // `x += 1` reads `x` before it writes it.
                    if left.kind() == "identifier" {
                        self.push_mention(left, MentionForm::Other);
                    }
                    self.visit_target(left);
                }
            }
            "import_statement" => self.visit_import(node),
            "export_statement" => self.visit_export(node),
            "pair" => {
                if let Some(key) = node.child_by_field_name("key") {
                    if key.kind() == "computed_property_name" {
                        self.visit_children(key);
                    }
                }
                if let Some(value) = node.child_by_field_name("value") {
                    self.visit(value);
                }
            }
            "jsx_opening_element" | "jsx_self_closing_element" => {
                if let Some(name) = node.child_by_field_name("name") {
                    self.mark_jsx(name);
                }
                if let Some(type_arguments) = node.child_by_field_name("type_arguments") {
                    self.visit_type(type_arguments);
                }
                let mut cursor = node.walk();
                let attributes: Vec<Node<'_>> = node
                    .children_by_field_name("attribute", &mut cursor)
                    .collect();
                for attribute in attributes {
                    self.visit(attribute);
                }
            }
            "jsx_closing_element"
            | "property_identifier"
            | "statement_identifier"
            | "private_property_identifier"
            | "this"
            | "super"
            | "undefined" => {}
            "jsx_attribute" => {
                let mut cursor = node.walk();
                let values: Vec<Node<'_>> = node
                    .named_children(&mut cursor)
                    .filter(|child| {
                        !matches!(child.kind(), "property_identifier" | "jsx_namespace_name")
                    })
                    .collect();
                for value in values {
                    self.visit(value);
                }
            }
            "decorator" => {
                if let Some(expression) = node.named_child(0) {
                    match expression.kind() {
                        "identifier" | "member_expression" => {
                            let name = self.leaf_name(expression);
                            self.push_protocol(expression, ProtocolForm::Decorator, name);
                            self.visit_receiver(expression, MentionForm::Decorator);
                        }
                        _ => self.visit(expression),
                    }
                }
            }
            "with_statement" => {
                self.push_reflective(
                    node,
                    ReflectiveForm::Evaluate,
                    ReflectiveReach::Any,
                    None,
                    None,
                    false,
                );
                self.visit_children(node);
            }
            "field_definition" | "public_field_definition" => {
                self.visit_decorators(node);
                // A computed field name is evaluated once, with the class.
                if let Some(name) = node
                    .child_by_field_name("property")
                    .or_else(|| node.child_by_field_name("name"))
                    .filter(|name| name.kind() == "computed_property_name")
                {
                    self.visit_children(name);
                }
                if let Some(annotation) = node.child_by_field_name("type") {
                    self.visit_type(annotation);
                }
                if let Some(value) = node.child_by_field_name("value") {
                    self.visit(value);
                }
            }
            "template_string" => {
                let mut cursor = node.walk();
                let substitutions: Vec<Node<'_>> = node
                    .named_children(&mut cursor)
                    .filter(|child| child.kind() == "template_substitution")
                    .collect();
                for substitution in substitutions {
                    self.visit_children(substitution);
                }
            }
            "as_expression" | "satisfies_expression" | "type_assertion" => {
                // The expression is the first named child; the rest is a type.
                let mut cursor = node.walk();
                let children: Vec<Node<'_>> = node.named_children(&mut cursor).collect();
                let expression = children.iter().copied().find(|child| {
                    !TYPE_ONLY.contains(&child.kind()) && !child.kind().ends_with("type")
                });
                if let Some(expression) = expression {
                    self.visit(expression);
                }
                for child in children {
                    if expression.is_none_or(|expression| expression.id() != child.id()) {
                        self.visit_type(child);
                    }
                }
            }
            "internal_module" | "module" => {
                if let Some(body) = node.child_by_field_name("body") {
                    self.visit(body);
                }
            }
            _ => self.visit_children(node),
        }
    }

    fn visit_function(&mut self, node: Node<'_>) {
        if node.kind() == "method_definition" {
            self.mark_protocol_member(node);
            self.mark_class_member(node);
            // JavaScript holds a method's decorators on the method;
            // TypeScript holds them in the class body, which visits them.
            self.visit_decorators(node);
        }
        let mut scope = Scope::new(ScopeKind::Function);
        if matches!(
            node.kind(),
            "function_expression" | "function" | "generator_function"
        ) {
            if let Some(name) = node.child_by_field_name("name") {
                self.bind(&mut scope, self.text(name), Some(line_of(node)));
            }
        }
        if let Some(parameter) = node.child_by_field_name("parameter") {
            self.bind_pattern(parameter, &mut scope);
        }
        let parameters = node.child_by_field_name("parameters");
        if let Some(parameters) = parameters {
            self.bind_pattern(parameters, &mut scope);
        }
        let body = node.child_by_field_name("body");
        if let Some(body) = body {
            self.collect_hoisted(body, &mut scope);
            if body.kind() == "statement_block" {
                self.collect_lexical(body, &mut scope);
            }
        }
        // Computed method names and parameter defaults are evaluated in the
        // enclosing scope for names, and in this one for defaults; reading
        // them in this scope can only record more.
        if node.kind() == "method_definition" {
            if let Some(name) = node.child_by_field_name("name") {
                if name.kind() == "computed_property_name" {
                    self.visit_children(name);
                }
            }
        }
        if let Some(parameters) =
            parameters.filter(|parameters| parameters.kind() == "formal_parameters")
        {
            let mut cursor = parameters.walk();
            let typed = parameters.named_children(&mut cursor).any(|parameter| {
                matches!(
                    declared_type(parameter, self.source),
                    Some(DeclaredType::Number)
                )
            });
            if typed || callback_index(parameters, self.source).is_some() {
                scope.numeric_candidate = true;
            }
        }
        self.settle_numeric(node, &mut scope);
        self.scopes.push(scope);
        if let Some(parameters) = parameters {
            self.visit_parameters(parameters);
        }
        for field in ["type_parameters", "return_type"] {
            if let Some(annotation) = node.child_by_field_name(field) {
                self.visit_type(annotation);
            }
        }
        if let Some(body) = body {
            if body.kind() == "statement_block" {
                self.visit_children(body);
            } else {
                self.visit(body);
            }
        }
        self.scopes.pop();
    }

    /// Parameter defaults and destructuring keys of a parameter list.
    fn visit_parameters(&mut self, parameters: Node<'_>) {
        let mut cursor = parameters.walk();
        let children: Vec<Node<'_>> = parameters.named_children(&mut cursor).collect();
        for parameter in children {
            self.visit_pattern(parameter, None);
        }
    }

    /// Visit a binding pattern: defaults are reads, and each key a
    /// destructuring pattern reads off `source` is a member read.
    fn visit_pattern(&mut self, pattern: Node<'_>, source: Option<Node<'_>>) {
        match pattern.kind() {
            "identifier" | "shorthand_property_identifier_pattern" | "this" | "undefined" => {
                if pattern.kind() == "shorthand_property_identifier_pattern" {
                    let name = self.text(pattern).to_string();
                    self.push_pattern_key(pattern, name, source, Vec::new());
                }
            }
            "assignment_pattern" | "object_assignment_pattern" => {
                if let Some(left) = pattern.child_by_field_name("left") {
                    self.visit_pattern(left, source);
                }
                if let Some(right) = pattern.child_by_field_name("right") {
                    self.visit(right);
                }
            }
            "required_parameter" | "optional_parameter" => {
                self.visit_decorators(pattern);
                if let Some(inner) = pattern
                    .child_by_field_name("pattern")
                    .or_else(|| pattern.child_by_field_name("name"))
                {
                    self.visit_pattern(inner, source);
                }
                if let Some(annotation) = pattern.child_by_field_name("type") {
                    self.visit_type(annotation);
                }
                if let Some(value) = pattern.child_by_field_name("value") {
                    self.visit(value);
                }
            }
            "pair_pattern" => {
                let value = pattern.child_by_field_name("value");
                if let Some(key) = pattern.child_by_field_name("key") {
                    if key.kind() == "computed_property_name" {
                        let expression = key.named_child(0);
                        let skip = expression.is_some_and(|expression| {
                            self.is_numeric(expression) || self.is_symbol(expression)
                        });
                        if !skip {
                            let literals = expression
                                .map(|expression| self.key_literals(expression))
                                .unwrap_or_default();
                            let (reach, receiver) = match source {
                                Some(source) => self.lookup_reach(source),
                                None => (ReflectiveReach::Any, None),
                            };
                            self.push_lookup(
                                key,
                                ReflectiveForm::ComputedMember,
                                reach,
                                receiver,
                                literals,
                                false,
                                false,
                            );
                        }
                        self.visit_children(key);
                    } else {
                        let name = plain_string_literal(key, self.source)
                            .unwrap_or_else(|| self.text(key).to_string());
                        let alias = value
                            .filter(|value| value.kind() == "identifier")
                            .map(|value| vec![self.text(value).to_string()])
                            .unwrap_or_default();
                        self.push_pattern_key(key, name, source, alias);
                    }
                }
                if let Some(value) = value {
                    self.visit_pattern(value, None);
                }
            }
            "member_expression" | "subscript_expression" => self.visit_target(pattern),
            kind if is_type_syntax(kind) => self.visit_type(pattern),
            kind if TYPE_ONLY.contains(&kind) => {}
            _ => {
                let mut cursor = pattern.walk();
                let children: Vec<Node<'_>> = pattern.named_children(&mut cursor).collect();
                for child in children {
                    self.visit_pattern(
                        child,
                        if pattern.kind() == "object_pattern" {
                            source
                        } else {
                            None
                        },
                    );
                }
            }
        }
    }

    /// A destructuring key reads that member off the source value into a
    /// binding: an alias at module scope, a local otherwise.
    fn push_pattern_key(
        &mut self,
        key: Node<'_>,
        name: String,
        source: Option<Node<'_>>,
        aliases: Vec<String>,
    ) {
        if !is_name(&name) {
            return;
        }
        // Renaming `require` destructuring is an import alias, and a plain
        // one a plain import.
        if let Some(module) = source.and_then(|source| self.required_module(source)) {
            let local = aliases.first().cloned().unwrap_or_else(|| name.clone());
            let statement = key
                .parent()
                .and_then(|pattern| pattern.parent())
                .and_then(|pattern| {
                    if pattern.kind() == "variable_declarator" {
                        Some(pattern)
                    } else {
                        pattern.parent()
                    }
                });
            let import = statement.map(|node| (node.start_byte() as u32, node.end_byte() as u32));
            let renamed = local != name;
            self.push_value(
                name,
                ValueOccurrence {
                    line: line_of(key),
                    context: if renamed {
                        OccurrenceContext::Import
                    } else {
                        OccurrenceContext::Mention
                    },
                    binding: OccurrenceBinding::Import,
                    receiver: None,
                    receiver_kind: None,
                    aliases: if renamed { vec![local] } else { Vec::new() },
                    module: Some(module),
                    import,
                    imported: None,
                    target_line: None,
                    form: (!renamed).then_some(MentionForm::Import),
                },
            );
            return;
        }
        let context = if self.at_module_scope() {
            OccurrenceContext::Alias
        } else {
            OccurrenceContext::Assignment
        };
        let aliases = if context == OccurrenceContext::Alias {
            if aliases.is_empty() {
                vec![name.clone()]
            } else {
                aliases
            }
        } else {
            Vec::new()
        };
        let receiver = source.map(|source| collapsed_text(source, self.source, RECEIVER_TEXT_MAX));
        let receiver_kind = source.map(|source| self.receiver_kind(source));
        self.push_value(
            name,
            ValueOccurrence {
                line: line_of(key),
                context,
                binding: OccurrenceBinding::Member,
                receiver,
                receiver_kind: Some(receiver_kind.unwrap_or(ReceiverKind::Other)),
                aliases,
                module: None,
                import: None,
                imported: None,
                target_line: None,
                form: None,
            },
        );
    }

    fn visit_class(&mut self, node: Node<'_>) {
        self.record_class(node);
        let mut scope = Scope::new(ScopeKind::Block);
        if node.kind() == "class" {
            if let Some(name) = node.child_by_field_name("name") {
                self.bind(&mut scope, self.text(name), Some(line_of(node)));
            }
        }
        let mut cursor = node.walk();
        let children: Vec<Node<'_>> = node.named_children(&mut cursor).collect();
        for child in &children {
            match child.kind() {
                "class_heritage" => {
                    // `extends Base` inherits; a computed base is a read.
                    let mut inner = child.walk();
                    let bases: Vec<Node<'_>> = child.named_children(&mut inner).collect();
                    for base in bases {
                        match base.kind() {
                            "identifier" | "member_expression" => {
                                self.visit_receiver(base, MentionForm::Base)
                            }
                            // `implements I`.
                            kind if is_type_syntax(kind) => self.visit_type(base),
                            _ => self.visit(base),
                        }
                    }
                }
                "decorator" => self.visit(*child),
                "type_parameters" => self.visit_type(*child),
                _ => {}
            }
        }
        self.scopes.push(scope);
        if let Some(body) = node.child_by_field_name("body") {
            self.visit(body);
        }
        self.scopes.pop();
    }

    /// Record the class `node` declares and the base it extends.
    fn record_class(&mut self, node: Node<'_>) {
        let Some(name) = node.child_by_field_name("name") else {
            return;
        };
        let mut open = false;
        let mut bases = Vec::new();
        let mut cursor = node.walk();
        let children: Vec<Node<'_>> = node.named_children(&mut cursor).collect();
        for child in children {
            match child.kind() {
                // A decorator may hand back any value in place of the class.
                "decorator" => open = true,
                "class_heritage" => {
                    let mut inner = child.walk();
                    let parts: Vec<Node<'_>> = child.named_children(&mut inner).collect();
                    for part in parts {
                        // `implements` names types, which hold no members.
                        if part.kind() == "implements_clause" {
                            continue;
                        }
                        let base = if part.kind() == "extends_clause" {
                            part.child_by_field_name("value")
                                .or_else(|| part.named_child(0))
                        } else {
                            Some(part)
                        };
                        match base.and_then(|base| self.base_name(base)) {
                            Some(name) => bases.push(name),
                            None => open = true,
                        }
                    }
                }
                _ => {}
            }
        }
        self.record.classes.push(ClassBases {
            name: self.text(name).to_string(),
            line: line_of(node),
            bases,
            open,
            typing_only: false,
            structural: false,
            dynamic: false,
        });
    }

    /// The name the class a base expression reads is declared under, or
    /// `None` when no declared name settles it.
    fn base_name(&self, base: Node<'_>) -> Option<String> {
        match base.kind() {
            "identifier" => {
                let name = self.text(base);
                match self.resolve(name) {
                    Some((OccurrenceBinding::Free, _)) => Some(name.to_string()),
                    Some((_, Some(binds)))
                        if !binds.other && binds.imports.is_empty() && !binds.namespace =>
                    {
                        (!binds.definitions.is_empty()).then(|| name.to_string())
                    }
                    Some((_, Some(binds)))
                        if !binds.other && binds.definitions.is_empty() && !binds.namespace =>
                    {
                        // `import {Base as Local}`: the name the module
                        // exports. A default import names no declaration.
                        match binds.imports.as_slice() {
                            [(_, imported, _)] if imported != "default" && imported != "*" => {
                                Some(imported.clone())
                            }
                            _ => None,
                        }
                    }
                    _ => None,
                }
            }
            // `React.Component`: the member the object holds.
            "member_expression" => base
                .child_by_field_name("property")
                .map(|property| self.text(property).to_string()),
            _ => None,
        }
    }

    fn visit_for(&mut self, node: Node<'_>) {
        let mut scope = Scope::new(ScopeKind::Block);
        if let Some(initializer) = node.child_by_field_name("initializer") {
            if initializer.kind() == "lexical_declaration" {
                self.collect_declaration(initializer, &mut scope);
            }
        }
        // A `let` counter is settled over the loop; a `var` one belongs to
        // the function and is settled there.
        self.settle_numeric(node, &mut scope);
        self.scopes.push(scope);
        self.visit_children(node);
        self.scopes.pop();
    }

    fn visit_for_in(&mut self, node: Node<'_>) {
        let mut scope = Scope::new(ScopeKind::Block);
        if let Some(left) = node.child_by_field_name("left") {
            let declares = node.child_by_field_name("kind").is_some();
            if declares {
                self.bind_pattern(left, &mut scope);
            }
        }
        if let Some(right) = node.child_by_field_name("right") {
            self.visit(right);
        }
        self.scopes.push(scope);
        if let Some(left) = node.child_by_field_name("left") {
            self.visit_pattern(left, None);
        }
        if let Some(body) = node.child_by_field_name("body") {
            self.visit(body);
        }
        self.scopes.pop();
    }

    fn visit_declarator(&mut self, node: Node<'_>) {
        if let Some(annotation) = node.child_by_field_name("type") {
            self.visit_type(annotation);
        }
        let value = node.child_by_field_name("value");
        if let Some(value) = value {
            self.visit(value);
        }
        if let Some(name) = node.child_by_field_name("name") {
            if name.kind() == "identifier" {
                if self.declarator_at_module(node)
                    && value.is_some_and(|value| {
                        matches!(value.kind(), "identifier" | "member_expression")
                    })
                {
                    self.push_binding(name, self.text(name).to_string(), None);
                }
            } else {
                self.visit_pattern(name, value);
            }
        }
    }

    fn visit_assignment(&mut self, node: Node<'_>) {
        let right = node.child_by_field_name("right");
        if let Some(right) = right {
            self.visit(right);
        }
        let Some(left) = node.child_by_field_name("left") else {
            return;
        };
        let aliases_a_name =
            right.is_some_and(|right| matches!(right.kind(), "identifier" | "member_expression"));
        match left.kind() {
            "identifier" => {
                if aliases_a_name && self.at_module_scope() {
                    self.push_binding(left, self.text(left).to_string(), None);
                }
            }
            "member_expression" => {
                if let Some(property) = left.child_by_field_name("property") {
                    let name = self.text(property).to_string();
                    let object = left.child_by_field_name("object");
                    if object.is_some_and(|object| self.is_exports_object(object)) {
                        self.record.exports.push(name.clone());
                    }
                    if aliases_a_name {
                        self.push_binding(property, name, object);
                    }
                }
                self.visit_target(left);
            }
            "object_pattern" | "array_pattern" => self.visit_pattern(left, right),
            _ => self.visit_target(left),
        }
    }

    /// Visit an assignment target: the object of a member target and the
    /// pieces of a subscript target are reads, the target itself is not. A
    /// member target is a store into the member it names.
    fn visit_target(&mut self, node: Node<'_>) {
        match node.kind() {
            "identifier" => {}
            "member_expression" => {
                if let Some(object) = node.child_by_field_name("object") {
                    if let Some(property) = node.child_by_field_name("property") {
                        let name = self.text(property).to_string();
                        self.push_member_mention(property, name, object, MentionForm::Store);
                    }
                    self.visit_receiver(object, MentionForm::Receiver);
                }
            }
            "subscript_expression" => {
                let index = node.child_by_field_name("index");
                if let Some(object) = node.child_by_field_name("object") {
                    // `o["name"] = v` stores into the member it spells.
                    if let Some((index, name)) = index.and_then(|index| {
                        self.literal_key(index)
                            .filter(|name| is_name(name))
                            .map(|name| (index, name))
                    }) {
                        self.push_member_mention(index, name, object, MentionForm::Store);
                    }
                    self.visit_receiver(object, MentionForm::Subscript);
                }
                if let Some(index) = index {
                    self.visit(index);
                }
            }
            "parenthesized_expression" => {
                if let Some(inner) = node.named_child(0) {
                    self.visit_target(inner);
                }
            }
            _ => self.visit_pattern(node, None),
        }
    }

    /// Visit an expression read in place, whose value flows nowhere: a
    /// receiver, a decorator, a class base, a JSX element name. A name is a
    /// mention of `form`, and a member read is a member mention of `form`
    /// whose own object is a receiver.
    fn visit_receiver(&mut self, node: Node<'_>, form: MentionForm) {
        match node.kind() {
            "identifier" => self.push_mention(node, form),
            "this" | "super" => {}
            "member_expression" => {
                if let Some(object) = node.child_by_field_name("object") {
                    if let Some(property) = node.child_by_field_name("property") {
                        let name = self.text(property).to_string();
                        self.push_member_mention(property, name, object, form);
                    }
                    self.visit_receiver(object, MentionForm::Receiver);
                }
            }
            "parenthesized_expression" | "non_null_expression" => {
                if let Some(inner) = node.named_child(0) {
                    self.visit_receiver(inner, form);
                }
            }
            _ => self.visit(node),
        }
    }

    fn visit_import(&mut self, statement: Node<'_>) {
        let range = (statement.start_byte() as u32, statement.end_byte() as u32);
        let module = statement
            .child_by_field_name("source")
            .and_then(|source| plain_string_literal(source, self.source));
        let mut cursor = statement.walk();
        let clauses: Vec<Node<'_>> = statement
            .named_children(&mut cursor)
            .filter(|child| child.kind() == "import_clause")
            .collect();
        for clause in clauses {
            let mut inner = clause.walk();
            let parts: Vec<Node<'_>> = clause.named_children(&mut inner).collect();
            for part in parts {
                match part.kind() {
                    "identifier" => {
                        let local = self.text(part).to_string();
                        self.push_import_alias(
                            part,
                            "default".into(),
                            local,
                            module.clone(),
                            range,
                        );
                    }
                    "named_imports" => {
                        let mut specifiers = part.walk();
                        let specifiers: Vec<Node<'_>> =
                            part.named_children(&mut specifiers).collect();
                        for specifier in specifiers {
                            let Some(name) = specifier.child_by_field_name("name") else {
                                continue;
                            };
                            let imported = plain_string_literal(name, self.source)
                                .unwrap_or_else(|| self.text(name).to_string());
                            let local = specifier
                                .child_by_field_name("alias")
                                .map(|alias| self.text(alias).to_string());
                            match local {
                                Some(local) if imported != local => self.push_import_alias(
                                    name,
                                    imported,
                                    local,
                                    module.clone(),
                                    range,
                                ),
                                _ => self.push_plain_import(name, imported, module.clone(), range),
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    /// An import that binds a name under its own name: a mention of the
    /// name it imports.
    fn push_plain_import(
        &mut self,
        node: Node<'_>,
        imported: String,
        module: Option<String>,
        range: (u32, u32),
    ) {
        if !is_name(&imported) {
            return;
        }
        self.push_value(
            imported,
            ValueOccurrence {
                line: line_of(node),
                context: OccurrenceContext::Mention,
                binding: OccurrenceBinding::Import,
                receiver: None,
                receiver_kind: None,
                aliases: Vec::new(),
                module,
                import: Some(range),
                imported: None,
                target_line: None,
                form: Some(MentionForm::Import),
            },
        );
    }

    fn push_import_alias(
        &mut self,
        node: Node<'_>,
        imported: String,
        local: String,
        module: Option<String>,
        range: (u32, u32),
    ) {
        self.push_value(
            imported,
            ValueOccurrence {
                line: line_of(node),
                context: OccurrenceContext::Import,
                binding: OccurrenceBinding::Import,
                receiver: None,
                receiver_kind: None,
                aliases: vec![local],
                module,
                import: Some(range),
                imported: None,
                target_line: None,
                form: None,
            },
        );
    }

    fn visit_export(&mut self, statement: Node<'_>) {
        // `@dec export class A {}` holds the class's decorators.
        self.visit_decorators(statement);
        let range = (statement.start_byte() as u32, statement.end_byte() as u32);
        let source = statement
            .child_by_field_name("source")
            .and_then(|source| plain_string_literal(source, self.source));
        let default = {
            let mut cursor = statement.walk();
            let found = statement
                .children(&mut cursor)
                .any(|child| child.kind() == "default");
            found
        };
        if default {
            self.record.exports.push("default".into());
        }
        let exported = statement
            .child_by_field_name("declaration")
            .or_else(|| statement.child_by_field_name("value"));
        if let Some(exported) = exported {
            // `export default function f() {}` exports `f` as `default`.
            let named = (is_function_like(exported.kind()) || is_class_like(exported.kind()))
                .then(|| exported.child_by_field_name("name"))
                .flatten();
            if let (true, Some(name_node)) = (default, named) {
                let name = self.text(name_node).to_string();
                self.push_value(
                    name,
                    ValueOccurrence {
                        line: line_of(name_node),
                        context: OccurrenceContext::Export,
                        binding: OccurrenceBinding::Module,
                        receiver: None,
                        receiver_kind: None,
                        aliases: vec!["default".into()],
                        module: None,
                        import: None,
                        imported: None,
                        target_line: Some(line_of(exported)),
                        form: None,
                    },
                );
            }
            if statement
                .child_by_field_name("declaration")
                .is_some_and(|declaration| declaration.id() == exported.id())
            {
                self.note_declared_exports(exported);
            }
            self.visit(exported);
        }
        let mut cursor = statement.walk();
        let children: Vec<Node<'_>> = statement.named_children(&mut cursor).collect();
        for child in children {
            match child.kind() {
                "export_clause" => {
                    let mut inner = child.walk();
                    let specifiers: Vec<Node<'_>> = child.named_children(&mut inner).collect();
                    for specifier in specifiers {
                        let Some(name) = specifier.child_by_field_name("name") else {
                            continue;
                        };
                        let local = plain_string_literal(name, self.source)
                            .unwrap_or_else(|| self.text(name).to_string());
                        let exported = specifier
                            .child_by_field_name("alias")
                            .map(|alias| {
                                plain_string_literal(alias, self.source)
                                    .unwrap_or_else(|| self.text(alias).to_string())
                            })
                            .unwrap_or_else(|| local.clone());
                        self.record.exports.push(exported.clone());
                        let mut occurrence = ValueOccurrence {
                            line: line_of(name),
                            context: OccurrenceContext::Export,
                            binding: OccurrenceBinding::Module,
                            receiver: None,
                            receiver_kind: None,
                            aliases: vec![exported],
                            module: None,
                            import: None,
                            imported: None,
                            target_line: None,
                            form: None,
                        };
                        if let Some(module) = &source {
                            occurrence.binding = OccurrenceBinding::Import;
                            occurrence.module = Some(module.clone());
                            occurrence.import = Some(range);
                        } else if let Some((binding, binds)) = self.resolve(&local) {
                            occurrence.binding = binding;
                            if let Some(binds) = binds {
                                if let [(module, imported, import)] = binds.imports.as_slice() {
                                    occurrence.module = Some(module.clone());
                                    occurrence.import = Some(*import);
                                    if imported != &local {
                                        occurrence.imported = Some(imported.clone());
                                    }
                                }
                                if binds.definitions.len() == 1 && !binds.other {
                                    occurrence.target_line = Some(binds.definitions[0]);
                                }
                            }
                        }
                        self.push_value(local, occurrence);
                    }
                }
                // TypeScript's `export = value` exports the module value.
                kind if kind != "namespace_export"
                    && statement
                        .child_by_field_name("declaration")
                        .is_none_or(|declaration| declaration.id() != child.id())
                    && statement
                        .child_by_field_name("value")
                        .is_none_or(|value| value.id() != child.id())
                    && statement
                        .child_by_field_name("source")
                        .is_none_or(|value| value.id() != child.id())
                    && child.kind() != "decorator"
                    && !TYPE_ONLY.contains(&kind) =>
                {
                    self.visit(child);
                }
                // `export import A = ns.B`.
                kind if is_type_syntax(kind)
                    && exported.is_none_or(|exported| exported.id() != child.id()) =>
                {
                    self.visit_type(child)
                }
                _ => {}
            }
        }
    }

    /// Names an exported declaration exports as themselves.
    fn note_declared_exports(&mut self, declaration: Node<'_>) {
        match declaration.kind() {
            "lexical_declaration" | "variable_declaration" => {
                let mut cursor = declaration.walk();
                let names: Vec<String> = declaration
                    .named_children(&mut cursor)
                    .filter_map(|declarator| declarator.child_by_field_name("name"))
                    .filter(|name| name.kind() == "identifier")
                    .map(|name| self.text(name).to_string())
                    .collect();
                self.record.exports.extend(names);
            }
            _ => {
                if let Some(name) = declaration.child_by_field_name("name") {
                    self.record.exports.push(self.text(name).to_string());
                }
            }
        }
    }

    // ── Types ─────────────────────────────────────────────────────────────

    /// Walk syntax that only describes types. Nothing in it is read at run
    /// time, so no value flows anywhere: every name it reads is a mention of
    /// [`MentionForm::Annotation`], and the names it declares (an interface,
    /// a type alias, a type parameter, a property or parameter of a
    /// signature) are not reads.
    fn visit_type(&mut self, node: Node<'_>) {
        match node.kind() {
            "comment"
            | "regex"
            | "string"
            | "number"
            | "template_string"
            | "predefined_type"
            | "this_type"
            | "existential_type"
            | "literal_type"
            | "this"
            | "super"
            | "import"
            | "property_identifier"
            | "private_property_identifier"
            | "statement_identifier"
            | "accessibility_modifier"
            | "override_modifier" => {}
            "type_identifier" => self.push_type_mention(node, MentionForm::Annotation),
            "identifier" => self.push_mention(node, MentionForm::Annotation),
            "nested_type_identifier" | "nested_identifier" | "member_expression" => {
                self.mention_path(node, MentionForm::Annotation)
            }
            // `x is T`, `asserts x is T`: `x` is a parameter.
            "type_predicate" => {
                if let Some(predicate) = node.child_by_field_name("type") {
                    self.visit_type(predicate);
                }
            }
            "asserts" => {
                let mut cursor = node.walk();
                let predicates: Vec<Node<'_>> = node
                    .named_children(&mut cursor)
                    .filter(|child| child.kind() == "type_predicate")
                    .collect();
                for predicate in predicates {
                    self.visit_type(predicate);
                }
            }
            // `infer U` declares `U`; a constraint after it is read.
            "infer_type" => {
                let mut cursor = node.walk();
                let parts: Vec<Node<'_>> = node.named_children(&mut cursor).skip(1).collect();
                for part in parts {
                    self.visit_type(part);
                }
            }
            // `import A = ns.B`: `A` is declared, `ns.B` read.
            "import_alias" => {
                let mut cursor = node.walk();
                let parts: Vec<Node<'_>> = node.named_children(&mut cursor).skip(1).collect();
                for part in parts {
                    self.mention_path(part, MentionForm::Other);
                }
            }
            // An import inside an ambient module.
            "import_statement" => self.visit_ambient_import(node),
            "export_specifier" => {
                if let Some(name) = node
                    .child_by_field_name("name")
                    .filter(|name| name.kind() == "identifier")
                {
                    self.push_mention(name, MentionForm::Annotation);
                }
            }
            // An ambient class's base.
            "extends_clause" => {
                if let Some(base) = node.child_by_field_name("value") {
                    match base.kind() {
                        "identifier" => self.push_mention(base, MentionForm::Base),
                        _ => self.mention_path(base, MentionForm::Base),
                    }
                }
                if let Some(type_arguments) = node.child_by_field_name("type_arguments") {
                    self.visit_type(type_arguments);
                }
            }
            // An enum member's initializer is evaluated at run time, and its
            // value goes into the enum.
            "enum_assignment" => {
                if let Some(value) = node.child_by_field_name("value") {
                    self.visit_inert(value, MentionForm::Other);
                }
            }
            _ => {
                // A `name` or `pattern` declares, except a generic type's
                // name, which it reads.
                let declaring = node.kind() != "generic_type";
                let mut children = Vec::new();
                let mut cursor = node.walk();
                if cursor.goto_first_child() {
                    loop {
                        let child = cursor.node();
                        let declares =
                            declaring && matches!(cursor.field_name(), Some("name" | "pattern"));
                        // `[Symbol.iterator](): T` reads its key.
                        if child.is_named()
                            && (!declares || child.kind() == "computed_property_name")
                        {
                            children.push(child);
                        }
                        if !cursor.goto_next_sibling() {
                            break;
                        }
                    }
                }
                for child in children {
                    self.visit_type(child);
                }
            }
        }
    }

    /// A dotted name read in a type position, or a base or alias target: a
    /// member mention of its last segment, whose receiver is the rest, and
    /// a mention of each segment before it, all of `form`.
    fn mention_path(&mut self, node: Node<'_>, form: MentionForm) {
        let (object, property) = match node.kind() {
            "identifier" => {
                self.push_mention(node, form);
                return;
            }
            "type_identifier" => {
                self.push_type_mention(node, form);
                return;
            }
            "nested_type_identifier" => (
                node.child_by_field_name("module"),
                node.child_by_field_name("name"),
            ),
            "member_expression" | "nested_identifier" => (
                node.child_by_field_name("object"),
                node.child_by_field_name("property"),
            ),
            "parenthesized_expression" | "non_null_expression" => {
                if let Some(inner) = node.named_child(0) {
                    self.mention_path(inner, form);
                }
                return;
            }
            _ => {
                self.visit_type(node);
                return;
            }
        };
        let (Some(object), Some(property)) = (object, property) else {
            return;
        };
        let name = self.text(property).to_string();
        self.push_member_mention(property, name, object, form);
        self.mention_path(object, form);
    }

    /// Whether the type name `name` at `node` is a type parameter some
    /// enclosing declaration binds: a `<T>` list, a mapped type's key, or a
    /// conditional type's `infer`.
    fn bound_type_parameter(&self, node: Node<'_>, name: &str) -> bool {
        let names = |list: Node<'_>, kind: &str| {
            let mut cursor = list.walk();
            let found = list.named_children(&mut cursor).any(|child| {
                child.kind() == kind
                    && child
                        .child_by_field_name("name")
                        .is_some_and(|declared| self.text(declared) == name)
            });
            found
        };
        let mut current = node.parent();
        while let Some(ancestor) = current {
            let generic = matches!(
                ancestor.kind(),
                "abstract_class_declaration"
                    | "abstract_method_signature"
                    | "arrow_function"
                    | "call_signature"
                    | "class"
                    | "class_declaration"
                    | "construct_signature"
                    | "constructor_type"
                    | "function_declaration"
                    | "function_expression"
                    | "function_signature"
                    | "function_type"
                    | "generator_function"
                    | "generator_function_declaration"
                    | "interface_declaration"
                    | "method_definition"
                    | "method_signature"
                    | "type_alias_declaration"
            );
            if generic
                && ancestor
                    .child_by_field_name("type_parameters")
                    .is_some_and(|parameters| names(parameters, "type_parameter"))
            {
                return true;
            }
            match ancestor.kind() {
                // `{ [K in keyof T]: ... }`.
                "index_signature" if names(ancestor, "mapped_type_clause") => return true,
                // `T extends Foo<infer U> ? U : never`.
                "conditional_type" => {
                    let mut stack: Vec<Node<'_>> =
                        ancestor.child_by_field_name("right").into_iter().collect();
                    while let Some(part) = stack.pop() {
                        if part.kind() == "infer_type"
                            && part
                                .named_child(0)
                                .is_some_and(|declared| self.text(declared) == name)
                        {
                            return true;
                        }
                        let mut cursor = part.walk();
                        stack.extend(part.named_children(&mut cursor));
                    }
                }
                _ => {}
            }
            current = ancestor.parent();
        }
        false
    }

    /// An import inside an ambient module (`declare module "m" { import
    /// {f} from "x"; }`): a mention of each name it imports. It binds
    /// nothing this file reads elsewhere.
    fn visit_ambient_import(&mut self, statement: Node<'_>) {
        let range = (statement.start_byte() as u32, statement.end_byte() as u32);
        let module = statement
            .child_by_field_name("source")
            .and_then(|source| plain_string_literal(source, self.source));
        let mut specifiers = Vec::new();
        let mut stack = vec![statement];
        while let Some(current) = stack.pop() {
            let mut cursor = current.walk();
            for child in current.named_children(&mut cursor) {
                match child.kind() {
                    "import_specifier" => specifiers.push(child),
                    "import_clause" | "named_imports" => stack.push(child),
                    _ => {}
                }
            }
        }
        for specifier in specifiers {
            if let Some(name) = specifier.child_by_field_name("name") {
                let imported = plain_string_literal(name, self.source)
                    .unwrap_or_else(|| self.text(name).to_string());
                self.push_plain_import(name, imported, module.clone(), range);
            }
        }
    }

    /// Visit an expression whose value goes nowhere a later call could reach
    /// it through, though it runs: every value use under it is a mention of
    /// `form`.
    fn visit_inert(&mut self, node: Node<'_>, form: MentionForm) {
        let outer = self.inert.replace(form);
        self.visit(node);
        self.inert = outer;
    }

    /// Visit the decorators `node` holds under its `decorator` field.
    fn visit_decorators(&mut self, node: Node<'_>) {
        let mut cursor = node.walk();
        let decorators: Vec<Node<'_>> = node
            .children_by_field_name("decorator", &mut cursor)
            .collect();
        for decorator in decorators {
            self.visit(decorator);
        }
    }

    // ── Reads ─────────────────────────────────────────────────────────────

    /// A read of the identifier `node` (or the shorthand property that
    /// spells it); `anchor` is the node whose position decides the flow.
    fn read_name(&mut self, node: Node<'_>, anchor: Node<'_>) {
        let name = self.text(node);
        if name.is_empty() || !is_name(name) {
            return;
        }
        let flow = match self.position_of(anchor) {
            Position::Flow(flow) => flow,
            Position::Inert(form) => {
                self.push_mention(node, form);
                return;
            }
            Position::Not => return,
        };
        let Some((binding, binds)) = self.resolve(name) else {
            return;
        };
        let (context, aliases) = match flow {
            Flow::Value(context, aliases) => (context, aliases),
            Flow::Invoke => {
                self.push_protocol(node, ProtocolForm::Invoke, Some(name.to_string()));
                return;
            }
        };
        let mut occurrence = ValueOccurrence {
            line: line_of(node),
            context,
            binding,
            receiver: None,
            receiver_kind: None,
            aliases,
            module: None,
            import: None,
            imported: None,
            target_line: None,
            form: None,
        };
        fill_binding(&mut occurrence, name, binds);
        self.push_value(name.to_string(), occurrence);
    }

    /// Record a mention of the name `node` spells, read in the position
    /// `form` names, with the binding it reads as [`Self::read_name`] finds
    /// it. A plain local or parameter is not recorded.
    fn push_mention(&mut self, node: Node<'_>, form: MentionForm) {
        self.push_resolved_mention(node, form, false);
    }

    /// Record a mention of the type name `node` spells, resolved in the type
    /// namespace (see [`Self::resolve_mention`]).
    fn push_type_mention(&mut self, node: Node<'_>, form: MentionForm) {
        if !self.bound_type_parameter(node, self.text(node)) {
            self.push_resolved_mention(node, form, true);
        }
    }

    fn push_resolved_mention(&mut self, node: Node<'_>, form: MentionForm, type_position: bool) {
        let name = self.text(node);
        if name.is_empty() || !is_name(name) {
            return;
        }
        let Some((binding, binds, declared)) = self.resolve_mention(name, type_position) else {
            return;
        };
        let mut occurrence = ValueOccurrence {
            line: line_of(node),
            context: OccurrenceContext::Mention,
            binding,
            receiver: None,
            receiver_kind: None,
            aliases: Vec::new(),
            module: None,
            import: None,
            imported: None,
            target_line: declared,
            form: Some(form),
        };
        fill_binding(&mut occurrence, name, binds);
        self.push_value(name.to_string(), occurrence);
    }

    /// Record a mention of the member `name` read off `object`, at the token
    /// `at`, in the position `form` names.
    fn push_member_mention(
        &mut self,
        at: Node<'_>,
        name: String,
        object: Node<'_>,
        form: MentionForm,
    ) {
        if name.is_empty() {
            return;
        }
        let receiver_kind = self.receiver_kind(object);
        self.push_value(
            name,
            ValueOccurrence {
                line: line_of(at),
                context: OccurrenceContext::Mention,
                binding: OccurrenceBinding::Member,
                receiver: Some(collapsed_text(object, self.source, RECEIVER_TEXT_MAX)),
                receiver_kind: Some(receiver_kind),
                aliases: Vec::new(),
                module: None,
                import: None,
                imported: None,
                target_line: None,
                form: Some(form),
            },
        );
    }

    fn visit_member(&mut self, node: Node<'_>) {
        let (Some(object), Some(property)) = (
            node.child_by_field_name("object"),
            node.child_by_field_name("property"),
        ) else {
            self.visit_children(node);
            return;
        };
        let name = self.text(property).to_string();
        let flow = match self.position_of(node) {
            Position::Flow(flow) => Some(flow),
            Position::Inert(form) => {
                self.push_member_mention(property, name.clone(), object, form);
                None
            }
            Position::Not => None,
        };
        if let Some(flow) = flow {
            match flow {
                Flow::Value(context, aliases) => {
                    let receiver_kind = self.receiver_kind(object);
                    self.push_value(
                        name,
                        ValueOccurrence {
                            line: line_of(property),
                            context,
                            binding: OccurrenceBinding::Member,
                            receiver: Some(collapsed_text(object, self.source, RECEIVER_TEXT_MAX)),
                            receiver_kind: Some(receiver_kind),
                            aliases,
                            module: None,
                            import: None,
                            imported: None,
                            target_line: None,
                            form: None,
                        },
                    );
                }
                Flow::Invoke => self.push_protocol(property, ProtocolForm::Invoke, Some(name)),
            }
        }
        self.visit_receiver(object, MentionForm::Receiver);
    }

    fn visit_subscript(&mut self, node: Node<'_>) {
        let object = node.child_by_field_name("object");
        let index = node.child_by_field_name("index");
        if let (Some(object), Some(index)) = (object, index) {
            let called = node.parent().is_some_and(|parent| {
                (parent.kind() == "call_expression" && is_field(parent, "function", node))
                    || (parent.kind() == "new_expression" && is_field(parent, "constructor", node))
            });
            if let Some(name) = self.literal_key(index) {
                // `o["name"]` reads the member it spells.
                if !called && is_name(&name) {
                    let position = self.position_of(node);
                    if let Position::Inert(form) = position {
                        self.push_member_mention(index, name.clone(), object, form);
                    }
                    if let Position::Flow(Flow::Value(context, aliases)) = position {
                        let receiver_kind = self.receiver_kind(object);
                        self.push_value(
                            name,
                            ValueOccurrence {
                                line: line_of(index),
                                context,
                                binding: OccurrenceBinding::Member,
                                receiver: Some(collapsed_text(
                                    object,
                                    self.source,
                                    RECEIVER_TEXT_MAX,
                                )),
                                receiver_kind: Some(receiver_kind),
                                aliases,
                                module: None,
                                import: None,
                                imported: None,
                                target_line: None,
                                form: None,
                            },
                        );
                    }
                }
            } else if !self.is_numeric(index)
                && !self.is_symbol(index)
                && (called || self.context_of(node).is_some())
            {
                let (reach, receiver) = self.lookup_reach(object);
                let key = self.key_literals(index);
                let own = self.keys_are_own(index, object);
                self.push_lookup(
                    node,
                    ReflectiveForm::ComputedMember,
                    reach,
                    receiver,
                    key,
                    own,
                    called,
                );
            }
        }
        if let Some(object) = object {
            self.visit_receiver(object, MentionForm::Subscript);
        }
        if let Some(index) = index {
            self.visit(index);
        }
    }

    fn visit_call(&mut self, node: Node<'_>) {
        // `await f<T>(x)` calls `f`: see `js_callee_node`.
        let function = node
            .child_by_field_name("function")
            .map(super::javascript::js_callee_node);
        let arguments = node.child_by_field_name("arguments");
        if let Some(function) = function {
            self.mark_reflective_call(node, function, arguments);
            match function.kind() {
                "identifier" => self.note_local_callee(function),
                "import" => {}
                "member_expression" => {
                    let property = function
                        .child_by_field_name("property")
                        .map(|property| self.text(property))
                        .unwrap_or("");
                    if let Some(object) = function.child_by_field_name("object") {
                        if matches!(property, "call" | "apply" | "bind") {
                            // The object is invoked, or bound into a value.
                            self.visit(object);
                        } else {
                            self.visit_receiver(object, MentionForm::Receiver);
                        }
                    }
                }
                _ => self.visit(function),
            }
        }
        if let Some(type_arguments) = node.child_by_field_name("type_arguments") {
            self.visit_type(type_arguments);
        }
        if let Some(arguments) = arguments {
            self.visit(arguments);
        }
    }

    /// Record `callee`, the bare name a call is made through, when the
    /// nearest scope that binds it is a function or a block, not the module.
    fn note_local_callee(&mut self, callee: Node<'_>) {
        let name = self.text(callee);
        let local = self
            .scopes
            .iter()
            .rev()
            .find(|scope| scope.names.contains_key(name))
            .is_some_and(|scope| scope.kind != ScopeKind::Module);
        if local {
            self.local_callees
                .push((callee.start_byte(), callee.end_byte()));
        }
    }

    fn leaf_name(&self, node: Node<'_>) -> Option<String> {
        match node.kind() {
            "identifier" => Some(self.text(node).to_string()),
            "member_expression" => node
                .child_by_field_name("property")
                .map(|property| self.text(property).to_string()),
            _ => None,
        }
    }

    fn is_exports_object(&self, node: Node<'_>) -> bool {
        let text = self.text(node);
        (text == "exports" || text == "module.exports")
            && matches!(self.resolve("exports"), Some((OccurrenceBinding::Free, _)))
    }

    fn receiver_kind(&self, object: Node<'_>) -> ReceiverKind {
        match object.kind() {
            "this" | "super" => ReceiverKind::Instance,
            "identifier" => {
                let name = self.text(object);
                if name == "exports" {
                    return ReceiverKind::Namespace;
                }
                match self.resolve(name) {
                    Some((_, Some(binds))) if binds.namespace => ReceiverKind::Namespace,
                    _ => ReceiverKind::Other,
                }
            }
            "member_expression" if self.text(object) == "module.exports" => ReceiverKind::Namespace,
            _ => ReceiverKind::Other,
        }
    }

    /// Whether an expression always evaluates to a number.
    fn is_numeric(&self, node: Node<'_>) -> bool {
        match node.kind() {
            "number" => true,
            "parenthesized_expression" => node
                .named_child(0)
                .is_some_and(|inner| self.is_numeric(inner)),
            "unary_expression" => node
                .child_by_field_name("operator")
                .is_some_and(|operator| matches!(self.text(operator), "-" | "+" | "~")),
            "update_expression" => true,
            "binary_expression" => {
                let operator = node
                    .child_by_field_name("operator")
                    .map(|operator| self.text(operator))
                    .unwrap_or("");
                match operator {
                    "-" | "*" | "/" | "%" | "**" | "|" | "&" | "^" | "<<" | ">>" | ">>>" => true,
                    "+" => {
                        node.child_by_field_name("left")
                            .is_some_and(|left| self.is_numeric(left))
                            && node
                                .child_by_field_name("right")
                                .is_some_and(|right| self.is_numeric(right))
                    }
                    _ => false,
                }
            }
            "member_expression" => node
                .child_by_field_name("property")
                .is_some_and(|property| self.text(property) == "length"),
            "identifier" => self.is_numeric_name(self.text(node)),
            "ternary_expression" => {
                node.child_by_field_name("consequence")
                    .is_some_and(|value| self.is_numeric(value))
                    && node
                        .child_by_field_name("alternative")
                        .is_some_and(|value| self.is_numeric(value))
            }
            "call_expression" => node
                .child_by_field_name("function")
                .is_some_and(|function| numeric_call(function, self.source)),
            "as_expression" | "satisfies_expression" => {
                numeric_expression(node, self.source, &mut Vec::new())
            }
            "subscript_expression" => node
                .child_by_field_name("object")
                .filter(|object| object.kind() == "identifier")
                .is_some_and(|object| {
                    let name = self.text(object);
                    self.names.number_array.contains(name) && self.bound_in_file(name)
                }),
            _ => false,
        }
    }

    /// Settle which names `scope` binds hold only numbers, from every write
    /// of them under `node`, the syntax the scope covers: a declaration with
    /// a numeric initializer, an arithmetic update, an assignment of a
    /// number, a parameter declared `number`, or the index parameter of a
    /// callback over an array. A write that could store anything else, or a
    /// name the scope binds with no write here, settles nothing.
    fn settle_numeric(&self, node: Node<'_>, scope: &mut Scope) {
        if scope.names.is_empty() || !scope.numeric_candidate {
            return;
        }
        let mut written: HashMap<String, Vec<Written>> = HashMap::new();
        let mut stack = vec![node];
        while let Some(current) = stack.pop() {
            collect_writes(current, self.source, &mut written);
            let mut cursor = current.walk();
            stack.extend(current.named_children(&mut cursor));
        }
        // A name this scope binds is bound in the file, though the scope is
        // not entered yet.
        let bound = |name: &str| scope.names.contains_key(name) || self.bound_in_file(name);
        // Every name written here is settled together, by its spelling, so a
        // counter stepped by a number bound in an inner block is settled too;
        // only the names this scope binds are kept.
        let numeric = greatest(&written, |write, numeric| match write {
            Written::Numeric => Some(true),
            Written::NumericIf(needs) => Some(needs.iter().all(|need| match need {
                Need::Number(name) if written.contains_key(name) => numeric.contains(name),
                // A name bound outside this scope is settled there.
                Need::Number(name) => self.is_numeric_name(name),
                Need::NumberArray(name) => self.names.number_array.contains(name) && bound(name),
            })),
            Written::IndexIf(needs) => Some(
                needs
                    .iter()
                    .all(|need| self.names.array.contains(need) && bound(need)),
            ),
            _ => None,
        });
        scope.numeric.extend(
            numeric
                .into_iter()
                .filter(|name| scope.names.contains_key(name)),
        );
    }

    /// Whether a read of `name` here reads a binding this file declares, so
    /// what the file writes to the name is all it can hold: not a global, and
    /// not an import another module writes.
    fn bound_in_file(&self, name: &str) -> bool {
        !matches!(
            self.resolve(name),
            Some((OccurrenceBinding::Free | OccurrenceBinding::Import, _))
        )
    }

    /// Whether a key is a symbol, which never spells a string name.
    fn is_symbol(&self, node: Node<'_>) -> bool {
        is_symbol_expression(node, self.source, &self.names.symbol)
            && (node.kind() != "identifier" || self.bound_in_file(self.text(node)))
    }

    /// The one string a key holds: a string literal, or a name written once,
    /// by a `const` or `let` declaration of one.
    fn literal_key(&self, node: Node<'_>) -> Option<String> {
        if let Some(text) = plain_string_literal(node, self.source) {
            return Some(text);
        }
        match node.kind() {
            "identifier" => {
                let name = self.text(node);
                if self.bound_in_file(name) {
                    self.names.constant.get(name).cloned()
                } else {
                    None
                }
            }
            "parenthesized_expression" => node
                .named_child(0)
                .and_then(|inner| self.literal_key(inner)),
            _ => None,
        }
    }

    /// What a lookup's receiver is and so what it can read: members of the
    /// enclosing instance, a class or function this file declares, a whole
    /// module object, or anything.
    fn lookup_reach(&self, object: Node<'_>) -> (ReflectiveReach, Option<LookupReceiver>) {
        if matches!(object.kind(), "this" | "super") {
            return (ReflectiveReach::Members, None);
        }
        if object.kind() != "identifier" {
            return (ReflectiveReach::Any, None);
        }
        let name = self.text(object);
        // A name reassigned anywhere in the file may hold anything.
        let written_once = self.names.writes.get(name).copied().unwrap_or(0) <= 1;
        let receiver = match self.resolve(name) {
            Some((_, Some(binds)))
                if written_once
                    && binds.definitions.len() == 1
                    && !binds.other
                    && binds.imports.is_empty() =>
            {
                Some(LookupReceiver::Declared {
                    name: name.to_string(),
                    line: binds.definitions[0],
                })
            }
            // `import * as ns`: the module's namespace object. A `require`
            // result is whatever the module exports, which may be an
            // instance, so it is not one.
            Some((_, Some(binds)))
                if written_once
                    && binds.namespace
                    && !binds.other
                    && binds.definitions.is_empty()
                    && !binds.imports.is_empty()
                    && binds.imports.iter().all(|(_, imported, _)| imported == "*") =>
            {
                Some(LookupReceiver::Module)
            }
            _ => None,
        };
        (ReflectiveReach::Any, receiver)
    }

    /// Whether the key `index` of a lookup on `object` can only be one of
    /// `object`'s own or inherited enumerable property names: the loop
    /// variable of a `for (k in object)`, of a `for (k of Object.keys(object))`
    /// or the first parameter of a callback over `Object.keys(object)`, with
    /// neither the key nor the receiver written again inside the loop.
    fn keys_are_own(&self, index: Node<'_>, object: Node<'_>) -> bool {
        if index.kind() != "identifier" || object.kind() != "identifier" {
            return false;
        }
        let key = self.text(index);
        let receiver = self.text(object);
        let mut child = index;
        while let Some(parent) = child.parent() {
            match parent.kind() {
                "for_in_statement" => {
                    let binds_key = parent
                        .child_by_field_name("left")
                        .is_some_and(|left| left.kind() == "identifier" && self.text(left) == key);
                    if binds_key {
                        let over = parent.child_by_field_name("right");
                        let operator = parent
                            .child_by_field_name("operator")
                            .map(|operator| self.text(operator));
                        let own = match operator {
                            Some("in") => over.is_some_and(|over| {
                                over.kind() == "identifier" && self.text(over) == receiver
                            }),
                            Some("of") => over.is_some_and(|over| self.keys_of(over, receiver)),
                            _ => false,
                        };
                        // A write later in the function does not change the
                        // keys this loop already took. A write in the body
                        // does: the lookup may then name a different object.
                        return own
                            && parent.child_by_field_name("body").is_some_and(|body| {
                                !writes_any(body, self.source, &[key, receiver])
                            });
                    }
                }
                kind if is_function_like(kind) => {
                    let parameters = parent
                        .child_by_field_name("parameters")
                        .or_else(|| parent.child_by_field_name("parameter"));
                    let first = parameters.and_then(|parameters| {
                        if parameters.kind() == "identifier" {
                            Some(parameters)
                        } else {
                            parameters.named_child(0)
                        }
                    });
                    let binds_key = first.is_some_and(|first| {
                        let name = first.child_by_field_name("pattern").unwrap_or(first);
                        name.kind() == "identifier" && self.text(name) == key
                    });
                    if !binds_key {
                        // A closure may run after the loop moved on or the
                        // receiver was written again.
                        return false;
                    }
                    // A callback handed to `Object.keys(object).forEach(...)`
                    // and its kin.
                    let call = parent
                        .parent()
                        .filter(|arguments| arguments.kind() == "arguments")
                        .and_then(|arguments| arguments.parent())
                        .filter(|call| call.kind() == "call_expression");
                    let over_keys = call
                        .and_then(|call| call.child_by_field_name("function"))
                        .filter(|function| function.kind() == "member_expression")
                        .is_some_and(|function| {
                            function
                                .child_by_field_name("property")
                                .is_some_and(|property| {
                                    matches!(
                                        self.text(property),
                                        "forEach"
                                            | "map"
                                            | "filter"
                                            | "some"
                                            | "every"
                                            | "find"
                                            | "findIndex"
                                            | "flatMap"
                                    )
                                })
                                && function
                                    .child_by_field_name("object")
                                    .is_some_and(|keys| self.keys_of(keys, receiver))
                        });
                    return over_keys
                        && parent
                            .child_by_field_name("body")
                            .is_some_and(|body| !writes_any(body, self.source, &[key, receiver]));
                }
                "statement_block" => {
                    // A block that declares the key binds another name.
                    let mut cursor = parent.walk();
                    let declares = parent.named_children(&mut cursor).any(|statement| {
                        matches!(
                            statement.kind(),
                            "lexical_declaration" | "variable_declaration"
                        ) && {
                            let mut inner = statement.walk();
                            let found = statement.named_children(&mut inner).any(|declarator| {
                                declarator
                                    .child_by_field_name("name")
                                    .is_some_and(|name| self.text(name) == key)
                            });
                            found
                        }
                    });
                    if declares {
                        return false;
                    }
                }
                _ => {}
            }
            child = parent;
        }
        false
    }

    /// Whether `node` is `Object.keys(receiver)`.
    fn keys_of(&self, node: Node<'_>, receiver: &str) -> bool {
        node.kind() == "call_expression"
            && node
                .child_by_field_name("function")
                .is_some_and(|function| self.text(function) == "Object.keys")
            && node
                .child_by_field_name("arguments")
                .and_then(|arguments| {
                    (arguments.named_child_count() == 1).then(|| arguments.named_child(0))?
                })
                .is_some_and(|argument| {
                    argument.kind() == "identifier" && self.text(argument) == receiver
                })
    }

    /// The literals a computed key provably holds: the pieces of a template
    /// string and of a `+` chain of strings.
    fn key_literals(&self, key: Node<'_>) -> KeyLiterals {
        let mut pieces = Vec::new();
        if !self.key_pieces(key, &mut pieces) {
            return KeyLiterals::default();
        }
        KeyLiterals::from_pieces(pieces)
    }

    /// Append the pieces `key` is made of, `Some(text)` for a literal run and
    /// `None` for a computed one, or answer false when `key` is no string
    /// built from literals and computed parts.
    fn key_pieces(&self, key: Node<'_>, pieces: &mut Vec<Option<String>>) -> bool {
        if let Some(text) = self.literal_key(key) {
            pieces.push(Some(text));
            return true;
        }
        match key.kind() {
            "template_string" => {
                let mut cursor = key.walk();
                let parts: Vec<Node<'_>> = key.named_children(&mut cursor).collect();
                for part in parts {
                    match part.kind() {
                        "string_fragment" => pieces.push(Some(self.text(part).to_string())),
                        _ => pieces.push(None),
                    }
                }
                true
            }
            "binary_expression" => {
                let operator = key
                    .child_by_field_name("operator")
                    .map(|operator| self.text(operator));
                let (Some(left), Some(right)) = (
                    key.child_by_field_name("left"),
                    key.child_by_field_name("right"),
                ) else {
                    return false;
                };
                if operator != Some("+") {
                    return false;
                }
                let mut left_pieces = Vec::new();
                let mut right_pieces = Vec::new();
                let mut left_string = self.key_pieces(left, &mut left_pieces);
                let mut right_string = self.key_pieces(right, &mut right_pieces);
                // `x + 1` is a number, or, when `x` is a string, `x` then the
                // digits: either way no name that does not end in them.
                if !left_string && !right_string {
                    if let Some(digits) = integer_text(right, self.source) {
                        right_pieces.push(Some(digits));
                        right_string = true;
                    } else if let Some(digits) = integer_text(left, self.source) {
                        left_pieces.push(Some(digits));
                        left_string = true;
                    } else {
                        return false;
                    }
                }
                pieces.extend(if left_string { left_pieces } else { vec![None] });
                pieces.extend(if right_string {
                    right_pieces
                } else {
                    vec![None]
                });
                true
            }
            "parenthesized_expression" => key
                .named_child(0)
                .is_some_and(|inner| self.key_pieces(inner, pieces)),
            _ => false,
        }
    }

    /// Where the value `node` reads goes, walking up through the syntax that
    /// passes a value through unchanged, or `None` when it goes nowhere a
    /// later call could reach it through.
    fn context_of(&self, node: Node<'_>) -> Option<Flow> {
        match self.position_of(node) {
            Position::Flow(flow) => Some(flow),
            Position::Inert(_) | Position::Not => None,
        }
    }

    /// What the position of `node` makes of it, walking up through the syntax
    /// that passes a value through unchanged: a read whose value flows on
    /// (what [`Self::context_of`] answers), a read whose value goes nowhere,
    /// with the form of its position, or no read at all.
    fn position_of(&self, node: Node<'_>) -> Position {
        let store_or = |form: MentionForm| {
            if matches!(node.kind(), "member_expression" | "subscript_expression") {
                MentionForm::Store
            } else {
                form
            }
        };
        let mut child = node;
        let mut wrapped = false;
        loop {
            let Some(parent) = child.parent() else {
                return Position::Inert(MentionForm::Other);
            };
            match parent.kind() {
                "parenthesized_expression"
                | "await_expression"
                | "as_expression"
                | "satisfies_expression"
                | "non_null_expression"
                | "type_assertion"
                | "instantiation_expression" => {}
                "ternary_expression" => {
                    if is_field(parent, "condition", child) {
                        return Position::Inert(MentionForm::Condition);
                    }
                }
                "binary_expression" => {
                    let operator = parent
                        .child_by_field_name("operator")
                        .map(|operator| self.text(operator))
                        .unwrap_or("");
                    match operator {
                        "&&" | "||" | "??" => {}
                        "==" | "===" | "!=" | "!==" | "<" | ">" | "<=" | ">=" | "instanceof"
                        | "in" => return Position::Inert(MentionForm::Comparison),
                        _ => return Position::Inert(MentionForm::Other),
                    }
                }
                "sequence_expression" => {
                    let last =
                        parent.named_child(parent.named_child_count().saturating_sub(1) as u32);
                    if last.is_none_or(|last| last.id() != child.id()) {
                        return Position::Inert(MentionForm::Other);
                    }
                }
                "arguments" => {
                    return Position::Flow(match parent.parent().map(|call| call.kind()) {
                        Some("call_expression" | "new_expression") => {
                            Flow::value(OccurrenceContext::Argument)
                        }
                        Some(_) => Flow::value(OccurrenceContext::Other),
                        None => return Position::Inert(MentionForm::Other),
                    });
                }
                "array" => return Position::Flow(Flow::value(OccurrenceContext::Collection)),
                "spread_element" => return Position::Inert(MentionForm::Other),
                "pair" => {
                    if !is_field(parent, "value", child) {
                        return Position::Not;
                    }
                    let key = parent
                        .child_by_field_name("key")
                        .and_then(|key| {
                            plain_string_literal(key, self.source)
                                .or_else(|| Some(self.text(key).to_string()))
                        })
                        .unwrap_or_default();
                    return match parent.parent() {
                        Some(object) => Position::Flow(self.object_flow(object, key)),
                        None => Position::Inert(MentionForm::Other),
                    };
                }
                "object" => {
                    // A shorthand property: `{ name }`.
                    let key = self.text(child).to_string();
                    return Position::Flow(self.object_flow(parent, key));
                }
                "return_statement" | "yield_expression" => {
                    return Position::Flow(Flow::value(OccurrenceContext::Return))
                }
                "arrow_function" => {
                    return if is_field(parent, "body", child) {
                        Position::Flow(Flow::value(OccurrenceContext::Return))
                    } else {
                        Position::Not
                    };
                }
                "throw_statement" => return Position::Flow(Flow::value(OccurrenceContext::Other)),
                "assignment_expression" => {
                    if !is_field(parent, "right", child) {
                        return Position::Not;
                    }
                    return Position::Flow(self.assignment_flow(parent));
                }
                "augmented_assignment_expression" => {
                    return if is_field(parent, "right", child) {
                        Position::Flow(Flow::value(OccurrenceContext::Other))
                    } else {
                        // `x += 1` reads its target too.
                        Position::Inert(store_or(MentionForm::Other))
                    };
                }
                "variable_declarator" => {
                    if !is_field(parent, "value", child) {
                        return Position::Not;
                    }
                    let Some(name) = parent.child_by_field_name("name") else {
                        return Position::Inert(MentionForm::Other);
                    };
                    if name.kind() != "identifier" {
                        // A destructuring source: its keys are recorded as
                        // member reads, and it is their receiver.
                        return Position::Inert(MentionForm::Receiver);
                    }
                    return Position::Flow(if self.declarator_at_module(parent) {
                        Flow::Value(OccurrenceContext::Alias, vec![self.text(name).to_string()])
                    } else {
                        Flow::value(OccurrenceContext::Assignment)
                    });
                }
                "assignment_pattern"
                | "object_assignment_pattern"
                | "required_parameter"
                | "optional_parameter" => {
                    return if is_field(parent, "right", child) || is_field(parent, "value", child) {
                        Position::Flow(Flow::value(OccurrenceContext::Assignment))
                    } else {
                        Position::Not
                    };
                }
                "field_definition" | "public_field_definition" => {
                    if !is_field(parent, "value", child) {
                        return Position::Not;
                    }
                    let name = parent
                        .child_by_field_name("property")
                        .or_else(|| parent.child_by_field_name("name"))
                        .map(|name| self.text(name).to_string())
                        .unwrap_or_default();
                    return Position::Flow(Flow::Value(OccurrenceContext::Alias, vec![name]));
                }
                "export_statement" => {
                    if is_field(parent, "value", child) {
                        return Position::Flow(Flow::Value(
                            OccurrenceContext::Export,
                            vec!["default".into()],
                        ));
                    }
                    // `export = value`.
                    return Position::Flow(Flow::value(OccurrenceContext::Export));
                }
                "jsx_expression" => {
                    return match parent.parent().map(|element| element.kind()) {
                        // A spread attribute: `<Row {...props} />`.
                        Some("jsx_opening_element" | "jsx_self_closing_element") | None => {
                            Position::Inert(MentionForm::Other)
                        }
                        Some(_) => Position::Flow(Flow::value(OccurrenceContext::Argument)),
                    };
                }
                "call_expression" => {
                    // A callee read through the grammar's `await f<T>(x)`
                    // reading, or through `f<T>`, is called directly, and
                    // the call walk records it; only a wrapped value is
                    // invoked through syntax that does not name it.
                    if is_field(parent, "function", child)
                        && wrapped
                        && super::javascript::js_callee_node(child).id() != node.id()
                    {
                        return Position::Flow(Flow::Invoke);
                    }
                    return Position::Not;
                }
                "new_expression" => {
                    if is_field(parent, "constructor", child) && wrapped {
                        return Position::Flow(Flow::Invoke);
                    }
                    return Position::Not;
                }
                "member_expression" => {
                    if !is_field(parent, "object", child) {
                        return Position::Not;
                    }
                    let property = parent
                        .child_by_field_name("property")
                        .map(|property| self.text(property))
                        .unwrap_or("");
                    let call = parent.parent().filter(|call| {
                        call.kind() == "call_expression" && is_field(*call, "function", parent)
                    });
                    match (property, call) {
                        ("call" | "apply", Some(_)) => return Position::Flow(Flow::Invoke),
                        ("bind", Some(call)) => {
                            child = call;
                            wrapped = false;
                            continue;
                        }
                        _ => return Position::Inert(MentionForm::Receiver),
                    }
                }
                "subscript_expression" => return Position::Inert(MentionForm::Subscript),
                "unary_expression" => {
                    let operator = parent
                        .child_by_field_name("operator")
                        .map(|operator| self.text(operator))
                        .unwrap_or("");
                    return Position::Inert(match operator {
                        "!" => MentionForm::Condition,
                        "delete" => store_or(MentionForm::Other),
                        _ => MentionForm::Other,
                    });
                }
                "update_expression" => {
                    return Position::Inert(store_or(MentionForm::Other));
                }
                "if_statement" | "while_statement" | "do_statement" => {
                    return Position::Inert(MentionForm::Condition)
                }
                "for_statement" => {
                    return Position::Inert(if is_field(parent, "condition", child) {
                        MentionForm::Condition
                    } else {
                        MentionForm::Other
                    });
                }
                "for_in_statement" => {
                    // The iterable is read; the loop variable is written.
                    return if is_field(parent, "right", child) {
                        Position::Inert(MentionForm::Other)
                    } else {
                        Position::Not
                    };
                }
                "switch_statement" | "switch_case" => {
                    return Position::Inert(MentionForm::Comparison)
                }
                "class_heritage" => return Position::Inert(MentionForm::Base),
                "decorator" => return Position::Inert(MentionForm::Decorator),
                "template_substitution"
                | "expression_statement"
                | "computed_property_name"
                | "statement_block"
                | "program"
                | "jsx_opening_element"
                | "jsx_self_closing_element"
                | "jsx_attribute"
                | "labeled_statement"
                | "with_statement"
                | "else_clause" => return Position::Inert(MentionForm::Other),
                // A catch parameter declares; an import or export list is
                // recorded by its own visit; a type by the type walk.
                "catch_clause" | "export_clause" | "export_specifier" | "import_clause"
                | "type_annotation" => return Position::Not,
                _ => return Position::Flow(Flow::value(OccurrenceContext::Other)),
            }
            child = parent;
            wrapped = true;
        }
    }

    /// Where a value held by an object literal under `key` goes: exported
    /// under `key` when the literal is the module's exports, kept in a
    /// container otherwise.
    fn object_flow(&self, object: Node<'_>, key: String) -> Flow {
        let exported = object.parent().is_some_and(|parent| {
            parent.kind() == "assignment_expression"
                && is_field(parent, "right", object)
                && parent
                    .child_by_field_name("left")
                    .is_some_and(|left| self.is_exports_object(left))
        });
        if exported {
            Flow::Value(OccurrenceContext::Export, vec![key])
        } else {
            Flow::value(OccurrenceContext::Collection)
        }
    }

    fn assignment_flow(&self, assignment: Node<'_>) -> Flow {
        let mut targets = Vec::new();
        let mut current = Some(assignment);
        while let Some(node) = current {
            if node.kind() != "assignment_expression" {
                break;
            }
            if let Some(left) = node.child_by_field_name("left") {
                targets.push(left);
            }
            current = node.parent().filter(|parent| {
                parent.kind() == "assignment_expression" && is_field(*parent, "right", node)
            });
        }
        let mut aliases = Vec::new();
        let mut escapes = None;
        let mut exported = false;
        for target in targets {
            match target.kind() {
                "identifier" => {
                    let name = self.text(target);
                    match self.resolve(name) {
                        Some((OccurrenceBinding::Module | OccurrenceBinding::Free, _)) => {
                            aliases.push(name.to_string())
                        }
                        _ => escapes = Some(OccurrenceContext::Assignment),
                    }
                }
                "member_expression" => {
                    if self.is_exports_object(target) {
                        // `module.exports = value`: the module's value itself.
                        escapes = Some(OccurrenceContext::Export);
                        continue;
                    }
                    if let Some(property) = target.child_by_field_name("property") {
                        aliases.push(self.text(property).to_string());
                    }
                    if target
                        .child_by_field_name("object")
                        .is_some_and(|object| self.is_exports_object(object))
                    {
                        exported = true;
                    }
                }
                "subscript_expression" => escapes = Some(OccurrenceContext::Collection),
                _ => escapes = Some(OccurrenceContext::Assignment),
            }
        }
        match escapes {
            Some(context) => Flow::Value(context, aliases),
            None if exported => Flow::Value(OccurrenceContext::Export, aliases),
            None => Flow::Value(OccurrenceContext::Alias, aliases),
        }
    }

    // ── Reflective and protocol sites ─────────────────────────────────────

    fn mark_reflective_call(
        &mut self,
        call: Node<'_>,
        function: Node<'_>,
        arguments: Option<Node<'_>>,
    ) {
        let first = arguments.and_then(|arguments| arguments.named_child(0));
        let second = arguments.and_then(|arguments| arguments.named_child(1));
        let literal =
            |node: Option<Node<'_>>| node.and_then(|node| plain_string_literal(node, self.source));
        let called = call.parent().is_some_and(|parent| {
            parent.kind() == "call_expression" && is_field(parent, "function", call)
        });
        match function.kind() {
            "import" => {
                if literal(first).is_none() {
                    self.push_reflective(
                        call,
                        ReflectiveForm::DynamicImport,
                        ReflectiveReach::Nothing,
                        None,
                        None,
                        false,
                    );
                }
            }
            "identifier" => {
                let name = self.text(function);
                if !matches!(self.resolve(name), Some((OccurrenceBinding::Free, _))) {
                    return;
                }
                match name {
                    "eval" | "Function" => self.push_reflective(
                        call,
                        ReflectiveForm::Evaluate,
                        ReflectiveReach::Any,
                        None,
                        None,
                        false,
                    ),
                    "require" if literal(first).is_none() => self.push_reflective(
                        call,
                        ReflectiveForm::DynamicImport,
                        ReflectiveReach::Nothing,
                        None,
                        None,
                        false,
                    ),
                    _ => {}
                }
            }
            "member_expression" => {
                let text = self.text(function);
                match text {
                    "Reflect.get"
                    | "Reflect.getOwnPropertyDescriptor"
                    | "Object.getOwnPropertyDescriptor" => {
                        let Some(object) = first else { return };
                        if let Some(name) = second.and_then(|key| self.literal_key(key)) {
                            if !called && is_name(&name) {
                                if let Some(Flow::Value(context, aliases)) = self.context_of(call) {
                                    let receiver_kind = self.receiver_kind(object);
                                    self.push_value(
                                        name,
                                        ValueOccurrence {
                                            line: line_of(call),
                                            context,
                                            binding: OccurrenceBinding::Member,
                                            receiver: Some(collapsed_text(
                                                object,
                                                self.source,
                                                RECEIVER_TEXT_MAX,
                                            )),
                                            receiver_kind: Some(receiver_kind),
                                            aliases,
                                            module: None,
                                            import: None,
                                            imported: None,
                                            target_line: None,
                                            form: None,
                                        },
                                    );
                                }
                            }
                        } else if let Some(key) =
                            second.filter(|key| !self.is_numeric(*key) && !self.is_symbol(*key))
                        {
                            let (reach, receiver) = self.lookup_reach(object);
                            let literals = self.key_literals(key);
                            let own = self.keys_are_own(key, object);
                            self.push_lookup(
                                call,
                                ReflectiveForm::ComputedMember,
                                reach,
                                receiver,
                                literals,
                                own,
                                called,
                            );
                        }
                    }
                    // Own enumerable string-keyed properties only.
                    "Object.values" | "Object.entries" => {
                        let (reach, receiver) = match first {
                            Some(object) => self.lookup_reach(object),
                            None => (ReflectiveReach::Any, None),
                        };
                        self.push_lookup(
                            call,
                            ReflectiveForm::Enumerate,
                            reach,
                            receiver,
                            KeyLiterals::default(),
                            true,
                            false,
                        );
                    }
                    // Every own property, enumerable or not.
                    "Object.getOwnPropertyDescriptors" => {
                        let (reach, receiver) = match first {
                            Some(object) => self.lookup_reach(object),
                            None => (ReflectiveReach::Any, None),
                        };
                        self.push_lookup(
                            call,
                            ReflectiveForm::Enumerate,
                            reach,
                            receiver,
                            KeyLiterals::default(),
                            false,
                            false,
                        );
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn mark_jsx(&mut self, name: Node<'_>) {
        let spelled = match name.kind() {
            "identifier" => {
                let text = self.text(name);
                // A lowercase tag is a host element named by a string.
                text.chars()
                    .next()
                    .is_some_and(char::is_uppercase)
                    .then(|| text.to_string())
            }
            "member_expression" | "nested_identifier" => {
                let text = self.text(name);
                text.rsplit('.').next().map(str::to_string)
            }
            _ => None,
        };
        if let Some(spelled) = spelled {
            self.push_protocol(name, ProtocolForm::Jsx, Some(spelled));
            // The element reads the component its name spells.
            match name.kind() {
                "identifier" | "member_expression" => self.visit_receiver(name, MentionForm::Other),
                _ => self.mention_path(name, MentionForm::Other),
            }
        }
    }

    /// A method or accessor declared in a class body lives on the prototype
    /// (or the constructor, for a static one) and is not enumerable.
    fn mark_class_member(&mut self, method: Node<'_>) {
        if method
            .parent()
            .is_none_or(|parent| parent.kind() != "class_body")
        {
            return;
        }
        let Some(name) = method.child_by_field_name("name") else {
            return;
        };
        if !matches!(name.kind(), "property_identifier" | "string" | "number") {
            return;
        }
        let text =
            plain_string_literal(name, self.source).unwrap_or_else(|| self.text(name).to_string());
        self.non_enumerable.push(MemberStamp {
            name: text,
            at: name.start_byte(),
        });
    }

    fn mark_protocol_member(&mut self, method: Node<'_>) {
        let Some(name_node) = method.child_by_field_name("name") else {
            return;
        };
        if name_node.kind() == "computed_property_name" {
            return;
        }
        let name = self.text(name_node).to_string();
        let accessor = {
            let mut cursor = method.walk();
            let found = method
                .children(&mut cursor)
                .any(|child| matches!(child.kind(), "get" | "set"));
            found
        };
        let member = if accessor {
            Some(ProtocolMember::Implicit("property access".into()))
        } else {
            match name.as_str() {
                "then" => Some(ProtocolMember::Implicit(
                    "await and promise resolution".into(),
                )),
                "toString" | "valueOf" | "toJSON" => {
                    Some(ProtocolMember::Implicit("conversion".into()))
                }
                "handleEvent" => Some(ProtocolMember::Implicit("event dispatch".into())),
                _ => None,
            }
        };
        if let Some(member) = member {
            self.protocol_members.push(ProtocolStamp {
                name,
                at: name_node.start_byte(),
                member,
            });
        }
    }

    // ── Output ────────────────────────────────────────────────────────────

    /// Record `name` as the target of a module-scope or member assignment
    /// from another name.
    fn push_binding(&mut self, node: Node<'_>, name: String, receiver: Option<Node<'_>>) {
        let binding = if receiver.is_some() {
            OccurrenceBinding::Member
        } else {
            OccurrenceBinding::Module
        };
        let receiver_kind = receiver.map(|receiver| self.receiver_kind(receiver));
        self.push_value(
            name,
            ValueOccurrence {
                line: line_of(node),
                context: OccurrenceContext::Binding,
                binding,
                receiver: receiver
                    .map(|receiver| collapsed_text(receiver, self.source, RECEIVER_TEXT_MAX)),
                receiver_kind,
                aliases: Vec::new(),
                module: None,
                import: None,
                imported: None,
                target_line: None,
                form: None,
            },
        );
    }

    fn push_value(&mut self, name: String, mut occurrence: ValueOccurrence) {
        if let Some(form) = self.inert {
            if occurrence.context != OccurrenceContext::Mention {
                occurrence.context = OccurrenceContext::Mention;
                occurrence.form = Some(form);
                occurrence.aliases.clear();
            }
        }
        self.record.values.entry(name).or_default().push(occurrence);
    }

    fn push_reflective(
        &mut self,
        node: Node<'_>,
        form: ReflectiveForm,
        reach: ReflectiveReach,
        key_prefix: Option<String>,
        key_suffix: Option<String>,
        called: bool,
    ) {
        self.record.reflective.push(ReflectiveSite {
            line: line_of(node),
            form,
            reach,
            key_prefix,
            key_suffix,
            called,
            excerpt: collapsed_text(node, self.source, EXCERPT_MAX),
            key_parts: Vec::new(),
            receiver: None,
            enumerable_only: false,
        });
    }

    /// Record a lookup whose receiver and key literals are known.
    #[allow(clippy::too_many_arguments)]
    fn push_lookup(
        &mut self,
        node: Node<'_>,
        form: ReflectiveForm,
        reach: ReflectiveReach,
        receiver: Option<LookupReceiver>,
        key: KeyLiterals,
        enumerable_only: bool,
        called: bool,
    ) {
        self.record.reflective.push(ReflectiveSite {
            line: line_of(node),
            form,
            reach,
            key_prefix: key.prefix,
            key_suffix: key.suffix,
            called,
            excerpt: collapsed_text(node, self.source, EXCERPT_MAX),
            key_parts: key.parts,
            receiver,
            enumerable_only,
        });
    }

    fn push_protocol(&mut self, node: Node<'_>, form: ProtocolForm, name: Option<String>) {
        self.record.protocol.push(ProtocolSite {
            line: line_of(node),
            form,
            name,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::LanguageAdapter;

    fn census_js(source: &str) -> FileEscapeCensus {
        let adapter = crate::languages::JavaScriptAdapter;
        let tree = adapter.parse(source.as_bytes()).unwrap();
        census(&tree, source.as_bytes())
    }

    fn census_ts(source: &str) -> FileEscapeCensus {
        let adapter = crate::languages::TypeScriptAdapter;
        let tree = adapter.parse(source.as_bytes()).unwrap();
        census(&tree, source.as_bytes())
    }

    /// Each locally bound callee of `source`, as its 1-based line and name.
    fn local_calls(source: &str) -> Vec<(usize, &str)> {
        let adapter = crate::languages::JavaScriptAdapter;
        let tree = adapter.parse(source.as_bytes()).unwrap();
        local_callees(&tree, source.as_bytes())
            .into_iter()
            .map(|(start, end)| (source[..start].lines().count().max(1), &source[start..end]))
            .collect()
    }

    /// React's devtools renderer at the benchmark tree: `attach` declares its
    /// own `measureInstance` and calls it from its body and from the closures
    /// inside it. The reconciler's host config exports the same name, and
    /// none of these calls reaches it.
    #[test]
    fn a_call_through_a_function_nested_in_its_caller_is_a_local_call() {
        let source = "import {getDisplayName} from './utils';\n\
            export function attach(hook, rendererID, renderer, global) {\n\
            \x20 function measureInstance(instance) {\n\
            \x20   return getDisplayName(instance);\n\
            \x20 }\n\
            \x20 function updateSuspenseNode(fiberInstance) {\n\
            \x20   const nextRects = measureInstance(fiberInstance);\n\
            \x20   return nextRects;\n\
            \x20 }\n\
            \x20 const newSuspenseNode = createSuspenseNode(hook);\n\
            \x20 newSuspenseNode.rects = measureInstance(hook);\n\
            \x20 return updateSuspenseNode(renderer);\n\
            }\n\
            function createSuspenseNode(instance) {\n\
            \x20 return {rects: null, instance};\n\
            }\n";
        assert_eq!(
            local_calls(source),
            vec![
                // `getDisplayName` is an import and `createSuspenseNode` a
                // module-level function: neither is local.
                (7, "measureInstance"),
                (11, "measureInstance"),
                (12, "updateSuspenseNode"),
            ]
        );
    }

    #[test]
    fn only_a_scope_around_the_call_makes_its_callee_local() {
        // A function declared in a block binds inside that block alone.
        let source = "function measure() {}\n\
            export function run(flag, callback) {\n\
            \x20 if (flag) {\n\
            \x20   function measure() {}\n\
            \x20   measure();\n\
            \x20 }\n\
            \x20 measure();\n\
            \x20 callback(flag);\n\
            \x20 const Local = class {};\n\
            \x20 new Local();\n\
            \x20 new Map();\n\
            \x20 flag.measure();\n\
            }\n";
        assert_eq!(
            local_calls(source),
            vec![(5, "measure"), (8, "callback"), (10, "Local")]
        );
        // A `var` binds in the function that holds it, wherever it is written.
        let source = "function measure() {}\n\
            export function run(flag) {\n\
            \x20 measure();\n\
            \x20 if (flag) {\n\
            \x20   var measure = flag;\n\
            \x20 }\n\
            }\n\
            export function other() {\n\
            \x20 measure();\n\
            }\n";
        assert_eq!(local_calls(source), vec![(3, "measure")]);
    }

    /// The occurrences of `name` other than mentions: the uses this walk
    /// recorded before it listed mentions.
    fn flows<'c>(census: &'c FileEscapeCensus, name: &str) -> Vec<&'c ValueOccurrence> {
        census
            .record
            .values
            .get(name)
            .map(|values| {
                values
                    .iter()
                    .filter(|value| value.context != OccurrenceContext::Mention)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn uses(census: &FileEscapeCensus, name: &str) -> Vec<(u32, OccurrenceContext, Vec<String>)> {
        flows(census, name)
            .into_iter()
            .map(|value| (value.line, value.context, value.aliases.clone()))
            .collect()
    }

    #[test]
    fn a_renaming_import_is_an_alias_and_a_plain_one_is_nothing() {
        let census = census_js(
            "import {setEnabled as ReactBrowserEventEmitterSetEnabled, other} from './events';\n\
             import Default from './default';\n",
        );
        assert_eq!(
            uses(&census, "setEnabled"),
            vec![(
                0,
                OccurrenceContext::Import,
                vec!["ReactBrowserEventEmitterSetEnabled".into()]
            )]
        );
        assert!(uses(&census, "other").is_empty());
        assert_eq!(
            uses(&census, "default"),
            vec![(1, OccurrenceContext::Import, vec!["Default".into()])]
        );
    }

    #[test]
    fn a_function_passed_as_an_argument_escapes_and_a_call_does_not() {
        let census = census_js(
            "import {commitTextUpdate} from './ReactFiberConfig';\n\
             export function commitHostTextUpdate(finishedWork, newText, oldText) {\n\
             \x20 if (__DEV__) {\n\
             \x20   runWithFiberInDEV(finishedWork, commitTextUpdate, finishedWork.stateNode, oldText, newText);\n\
             \x20 } else {\n\
             \x20   commitTextUpdate(finishedWork.stateNode, oldText, newText);\n\
             \x20 }\n\
             }\n",
        );
        let values = flows(&census, "commitTextUpdate");
        assert_eq!(values.len(), 1, "{values:?}");
        assert_eq!(values[0].line, 3);
        assert_eq!(values[0].context, OccurrenceContext::Argument);
        assert_eq!(values[0].binding, OccurrenceBinding::Import);
        assert_eq!(values[0].module.as_deref(), Some("./ReactFiberConfig"));
    }

    #[test]
    fn locals_shadow_module_names_only_in_their_block() {
        let census = census_js(
            "function setEnabled() {}\n\
             function run(a) {\n\
             \x20 if (a) { const setEnabled = 1; use(setEnabled); }\n\
             \x20 register(setEnabled);\n\
             }\n",
        );
        assert_eq!(
            uses(&census, "setEnabled"),
            vec![(3, OccurrenceContext::Argument, vec![])]
        );
    }

    #[test]
    fn member_reads_record_their_receiver_and_inert_uses_do_not() {
        let census = census_js(
            "class Row extends Component {\n\
             \x20 handleClick = this.onClick;\n\
             \x20 render() {\n\
             \x20   if (store.containsElement) {}\n\
             \x20   store.containsElement(id);\n\
             \x20   const bound = this.handleClick.bind(this);\n\
             \x20   return <Button onPress={this.props.onPress} />;\n\
             \x20 }\n\
             }\n",
        );
        assert!(
            uses(&census, "containsElement").is_empty(),
            "{:?}",
            census.record.values
        );
        assert_eq!(
            uses(&census, "onClick"),
            vec![(1, OccurrenceContext::Alias, vec!["handleClick".into()])]
        );
        assert_eq!(
            uses(&census, "handleClick"),
            vec![(5, OccurrenceContext::Assignment, vec![])]
        );
        assert_eq!(
            uses(&census, "onPress"),
            vec![(6, OccurrenceContext::Argument, vec![])]
        );
        let jsx: Vec<(u32, ProtocolForm, Option<&str>)> = census
            .record
            .protocol
            .iter()
            .map(|site| (site.line, site.form, site.name.as_deref()))
            .collect();
        assert_eq!(jsx, vec![(6, ProtocolForm::Jsx, Some("Button"))]);
    }

    #[test]
    fn exports_and_module_scope_rebindings_are_recorded() {
        let census = census_js(
            "function helper() {}\n\
             function other() {}\n\
             export { helper, other as renamed };\n\
             export const commitTextUpdate = $$$config.commitTextUpdate;\n\
             module.exports.legacy = helper;\n\
             export default other;\n",
        );
        assert_eq!(
            uses(&census, "helper"),
            vec![
                (2, OccurrenceContext::Export, vec!["helper".into()]),
                (4, OccurrenceContext::Export, vec!["legacy".into()]),
            ]
        );
        assert_eq!(
            uses(&census, "other"),
            vec![
                (2, OccurrenceContext::Export, vec!["renamed".into()]),
                (5, OccurrenceContext::Export, vec!["default".into()]),
            ]
        );
        let config = uses(&census, "commitTextUpdate");
        assert!(
            config.contains(&(3, OccurrenceContext::Alias, vec!["commitTextUpdate".into()])),
            "{config:?}"
        );
        assert!(
            config.contains(&(3, OccurrenceContext::Binding, vec![])),
            "{config:?}"
        );
        for name in ["helper", "renamed", "commitTextUpdate", "legacy", "default"] {
            assert!(
                census.record.exports.contains(&name.to_string()),
                "{name}: {:?}",
                census.record.exports
            );
        }
    }

    #[test]
    fn computed_reads_are_reflective_and_numeric_indexes_are_not() {
        let census = census_js(
            "function run(obj, key, list) {\n\
             \x20 for (let i = 0; i < list.length; i++) { use(list[i]); }\n\
             \x20 use(list[list.length - 1]);\n\
             \x20 const handler = obj[key];\n\
             \x20 obj[key] = 1;\n\
             \x20 if (obj[key]) {}\n\
             \x20 this[`on${key}`]();\n\
             \x20 const known = obj['literal'];\n\
             \x20 Object.values(obj).forEach(f => f());\n\
             \x20 eval(key);\n\
             \x20 require(key);\n\
             }\n",
        );
        let sites: Vec<(u32, ReflectiveForm, ReflectiveReach, bool)> = census
            .record
            .reflective
            .iter()
            .map(|site| (site.line, site.form, site.reach, site.called))
            .collect();
        assert_eq!(
            sites,
            vec![
                (
                    3,
                    ReflectiveForm::ComputedMember,
                    ReflectiveReach::Any,
                    false
                ),
                (
                    6,
                    ReflectiveForm::ComputedMember,
                    ReflectiveReach::Members,
                    true
                ),
                (8, ReflectiveForm::Enumerate, ReflectiveReach::Any, false),
                (9, ReflectiveForm::Evaluate, ReflectiveReach::Any, false),
                (
                    10,
                    ReflectiveForm::DynamicImport,
                    ReflectiveReach::Nothing,
                    false
                ),
            ]
        );
        let on = census
            .record
            .reflective
            .iter()
            .find(|site| site.line == 6)
            .unwrap();
        assert_eq!(on.key_prefix.as_deref(), Some("on"));
        assert_eq!(
            uses(&census, "literal"),
            vec![(7, OccurrenceContext::Assignment, vec![])]
        );
    }

    fn lookups(census: &FileEscapeCensus) -> Vec<(u32, &str)> {
        census
            .record
            .reflective
            .iter()
            .map(|site| (site.line, site.excerpt.as_str()))
            .collect()
    }

    fn lookup(census: &FileEscapeCensus, line: u32) -> &ReflectiveSite {
        census
            .record
            .reflective
            .iter()
            .find(|site| site.line == line)
            .unwrap_or_else(|| panic!("a site on line {line}: {:?}", census.record.reflective))
    }

    /// Keys that are always numbers read no named member: counters written
    /// only with numbers, `number` parameters, `as number` casts, and the
    /// index of a callback over an array.
    #[test]
    fn numeric_keys_are_not_lookups_by_name() {
        let census = census_ts(
            "function run(obj: any, list: Array<string>, left: number, key: string, xs) {\n\
             \x20 let i = 2;\n\
             \x20 const size = list.indexOf(key);\n\
             \x20 while (i < size) { use(obj[i]); i += 2; i++; }\n\
             \x20 use(obj[left]);\n\
             \x20 use(obj[(key as any) as number]);\n\
             \x20 list.map((item, at) => use(obj[at]));\n\
             \x20 Object.keys(obj).forEach((name, at) => use(obj[at]));\n\
             \x20 xs.forEach((item, at) => use(obj[at]));\n\
             \x20 let j = 0;\n\
             \x20 j = key;\n\
             \x20 use(obj[j]);\n\
             }\n",
        );
        // `xs` may be a Map, whose `forEach` hands a key; `j` is written a
        // string.
        assert_eq!(
            lookups(&census),
            vec![(8, "obj[at]"), (11, "obj[j]")],
            "{:?}",
            census.record.reflective
        );
    }

    /// An element of an array its type declares numbers is a number, and so
    /// is a counter stepped by one.
    #[test]
    fn elements_of_a_number_array_are_numbers() {
        let census = census_ts(
            "function run(operations: Array<number>, table: Array<string>, obj: any) {\n\
             \x20 let i = 2;\n\
             \x20 while (i < operations.length) {\n\
             \x20   const length = operations[i];\n\
             \x20   i += length;\n\
             \x20   use(obj[i], obj[operations[i]], obj[table[i]]);\n\
             \x20 }\n\
             }\n",
        );
        assert_eq!(lookups(&census), vec![(5, "obj[table[i]]")]);
    }

    /// `x + 1` is a number, or `x` then `1`: no name that does not end in
    /// the digit.
    #[test]
    fn a_key_plus_an_integer_ends_in_its_digits() {
        let census = census_js("function run(obj, i) { use(obj[i + 1]); use(obj[2 + i]); }\n");
        let sites = &census.record.reflective;
        assert_eq!(sites.len(), 2, "{sites:?}");
        assert_eq!(sites[0].key_suffix.as_deref(), Some("1"));
        assert!(!sites[0].key_admits("containsElement"));
        assert_eq!(sites[1].key_prefix.as_deref(), Some("2"));
        assert!(!sites[1].key_admits("containsElement"));
    }

    #[test]
    fn symbol_and_constant_keys_name_what_they_hold() {
        let census = census_js(
            "const KEY = Symbol('key');\n\
             const NAME = 'handle';\n\
             function run(obj) {\n\
             \x20 use(obj[KEY]);\n\
             \x20 use(obj[Symbol.iterator]);\n\
             \x20 const handler = obj[NAME];\n\
             \x20 use(obj[OTHER]);\n\
             }\n\
             function later(obj) { const NAME2 = 'x'; }\n\
             function reads(obj) { use(obj[NAME2]); }\n",
        );
        assert_eq!(
            lookups(&census),
            vec![(6, "obj[OTHER]"), (9, "obj[NAME2]")],
            "a free name holds anything, even one another function binds"
        );
        assert_eq!(
            uses(&census, "handle"),
            vec![(5, OccurrenceContext::Assignment, vec![])]
        );
    }

    #[test]
    fn a_template_key_keeps_every_literal_run() {
        let census = census_js("function run(obj, a, b) { use(obj[`on${a}-${b}Capture`]); }\n");
        let site = lookup(&census, 0);
        assert_eq!(site.key_prefix.as_deref(), Some("on"));
        assert_eq!(site.key_parts, vec!["-".to_string()]);
        assert_eq!(site.key_suffix.as_deref(), Some("Capture"));
        assert!(!site.key_admits("onClickCapture"));
    }

    #[test]
    fn a_lookup_records_what_its_receiver_is() {
        let census = census_js(
            "import * as ns from './ns';\n\
             const required = require('./required');\n\
             class Store {}\n\
             function run(obj, key) {\n\
             \x20 use(Store[key]);\n\
             \x20 use(ns[key]);\n\
             \x20 use(required[key]);\n\
             \x20 use(obj[key]);\n\
             \x20 use(this[key]);\n\
             \x20 new ns[key]();\n\
             }\n",
        );
        assert_eq!(
            lookup(&census, 4).receiver,
            Some(LookupReceiver::Declared {
                name: "Store".into(),
                line: 2
            })
        );
        assert_eq!(lookup(&census, 5).receiver, Some(LookupReceiver::Module));
        // A module's exports may be an instance of anything.
        assert_eq!(lookup(&census, 6).receiver, None);
        assert_eq!(lookup(&census, 7).receiver, None);
        assert_eq!(lookup(&census, 8).reach, ReflectiveReach::Members);
        let constructed = lookup(&census, 9);
        assert!(constructed.called, "`new ns[key]()` calls what it looks up");
    }

    #[test]
    fn a_key_from_its_own_receivers_keys_yields_only_enumerable_properties() {
        let census = census_js(
            "function run(obj, other) {\n\
             \x20 for (const key in obj) { use(obj[key]); }\n\
             \x20 for (const key in obj) { use(other[key]); }\n\
             \x20 Object.keys(obj).forEach((key) => use(obj[key]));\n\
             \x20 for (const key of Object.keys(obj)) { use(obj[key]); }\n\
             \x20 for (const key in obj) { obj = other; use(obj[key]); }\n\
             \x20 use(Object.values(obj));\n\
             \x20 use(Object.getOwnPropertyDescriptors(obj));\n\
             }\n",
        );
        let own: Vec<(u32, bool)> = census
            .record
            .reflective
            .iter()
            .map(|site| (site.line, site.enumerable_only))
            .collect();
        assert_eq!(
            own,
            vec![
                (1, true),
                (2, false),
                (3, true),
                (4, true),
                (5, false),
                (6, true),
                (7, false),
            ]
        );
    }

    #[test]
    fn class_body_methods_are_marked_non_enumerable() {
        let source = "class Store {\n\
             \x20 containsElement(id) { return true; }\n\
             \x20 get size() { return 1; }\n\
             \x20 static create() { return new Store(); }\n\
             \x20 handler = () => {};\n\
             }\n\
             const literal = { method() {} };\n";
        let census = census_js(source);
        let marked: Vec<&str> = census
            .non_enumerable
            .iter()
            .map(|stamp| stamp.name.as_str())
            .collect();
        assert_eq!(marked, vec!["containsElement", "size", "create"]);
    }

    #[test]
    fn a_class_records_the_base_it_extends() {
        let census = census_js(
            "import React from 'react';\n\
             import {Base as Local} from './base';\n\
             import Default from './default';\n\
             class A extends Local {}\n\
             class B extends A {}\n\
             class C extends React.Component {}\n\
             class D extends Default {}\n\
             class E extends mixin(A) {}\n\
             class F {}\n",
        );
        let classes: Vec<(&str, Vec<&str>, bool)> = census
            .record
            .classes
            .iter()
            .map(|class| {
                (
                    class.name.as_str(),
                    class.bases.iter().map(String::as_str).collect(),
                    class.open,
                )
            })
            .collect();
        assert_eq!(
            classes,
            vec![
                ("A", vec!["Base"], false),
                ("B", vec!["A"], false),
                ("C", vec!["Component"], false),
                ("D", vec![], true),
                ("E", vec![], true),
                ("F", vec![], false),
            ]
        );
    }

    #[test]
    fn call_and_apply_invoke_their_object() {
        let census = census_js(
            "function helper() {}\n\
             function run() { helper.call(null, 1); (0, helper)(2); }\n",
        );
        let invoked: Vec<(u32, ProtocolForm, Option<&str>)> = census
            .record
            .protocol
            .iter()
            .map(|site| (site.line, site.form, site.name.as_deref()))
            .collect();
        assert_eq!(
            invoked,
            vec![
                (1, ProtocolForm::Invoke, Some("helper")),
                (1, ProtocolForm::Invoke, Some("helper")),
            ]
        );
        assert!(uses(&census, "helper").is_empty());
    }

    /// `await parseBody<T>(request)` calls `parseBody` directly. The grammar
    /// reads the callee as `await parseBody`, and a wrapped callee used to be
    /// recorded as an invocation that does not spell its name, so the call
    /// reached no caller row and its site read as a protocol call.
    #[test]
    fn an_awaited_call_with_type_arguments_is_no_protocol_invocation() {
        let census = census_ts(
            "import { parseBody } from './body';\n\
             async function methodOverride(request: Request) {\n\
               const params = await parseBody<Record<string, string>>(request);\n\
               const plain = parseBody<string>(request);\n\
               return (await pick())(request);\n\
             }\n",
        );
        let invoked: Vec<(u32, ProtocolForm, Option<&str>)> = census
            .record
            .protocol
            .iter()
            .map(|site| (site.line, site.form, site.name.as_deref()))
            .collect();
        assert!(
            !invoked
                .iter()
                .any(|(_, _, name)| *name == Some("parseBody")),
            "{invoked:?}"
        );
    }

    /// A value called through syntax that does not name it is still invoked:
    /// `(await helper)(2)` awaits `helper` and calls what it holds, which is
    /// recorded as invoking `helper`, as `(0, helper)(2)` is. Only the
    /// grammar's reading of `await f<T>(x)`, with no parentheses, calls its
    /// name directly.
    #[test]
    fn a_parenthesized_awaited_callee_is_still_an_invocation() {
        for (language, census) in [
            (
                "javascript",
                census_js(
                    "function helper() {}\n\
                     async function run() { return (await helper)(2); }\n",
                ),
            ),
            (
                "typescript",
                census_ts(
                    "function helper(): void {}\n\
                     async function run() { return (await helper)(2); }\n",
                ),
            ),
        ] {
            let invoked: Vec<(u32, ProtocolForm, Option<&str>)> = census
                .record
                .protocol
                .iter()
                .map(|site| (site.line, site.form, site.name.as_deref()))
                .collect();
            assert_eq!(
                invoked,
                vec![(1, ProtocolForm::Invoke, Some("helper"))],
                "{language}"
            );
        }
    }

    #[test]
    fn typescript_types_are_not_reads() {
        let census = census_ts(
            "import type {Store} from './store';\n\
             function row(store: Store, cb: typeof handler): Store { return store as Store; }\n\
             class Box { value: Handler = handler; get size(): number { return 1; } }\n",
        );
        assert!(
            uses(&census, "Store").is_empty(),
            "{:?}",
            census.record.values
        );
        assert_eq!(
            uses(&census, "handler"),
            vec![(2, OccurrenceContext::Alias, vec!["value".into()])]
        );
        let members: Vec<&str> = census
            .protocol_members
            .iter()
            .map(|stamp| stamp.name.as_str())
            .collect();
        assert_eq!(members, vec!["size"]);
    }

    #[test]
    fn require_destructuring_renames_are_import_aliases() {
        let census = census_js("const {setEnabled: enable, plain} = require('./events');\n");
        let alias = &census.record.values["setEnabled"][0];
        assert_eq!(alias.context, OccurrenceContext::Import);
        assert_eq!(alias.aliases, vec!["enable"]);
        assert_eq!(alias.module.as_deref(), Some("./events"));
        assert!(uses(&census, "plain").is_empty());
    }

    /// Every mention of `name`: its line, form, binding and receiver.
    fn mentions(
        census: &FileEscapeCensus,
        name: &str,
    ) -> Vec<(u32, MentionForm, OccurrenceBinding, Option<String>)> {
        census
            .record
            .values
            .get(name)
            .map(|values| {
                values
                    .iter()
                    .filter(|value| value.context == OccurrenceContext::Mention)
                    .map(|value| {
                        assert!(value.aliases.is_empty(), "a mention aliases nothing");
                        (
                            value.line,
                            value.form.expect("a mention carries its form"),
                            value.binding,
                            value.receiver.clone(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn mention<'c>(
        census: &'c FileEscapeCensus,
        name: &str,
        form: MentionForm,
    ) -> &'c ValueOccurrence {
        census
            .record
            .values
            .get(name)
            .and_then(|values| {
                values.iter().find(|value| {
                    value.context == OccurrenceContext::Mention && value.form == Some(form)
                })
            })
            .unwrap_or_else(|| {
                panic!(
                    "a {form:?} mention of {name}: {:?}",
                    census.record.values.get(name)
                )
            })
    }

    #[test]
    fn the_record_lists_its_mentions() {
        assert!(census_js("").record.lists_mentions);
        assert!(census_ts("let a = 1;\n").record.records_mentions());
    }

    #[test]
    fn a_plain_import_is_a_mention_of_the_name_it_imports() {
        let source = "import {f, g as g, h as other} from 'm';\n\
                      export {f} from 'm';\n\
                      const {plain} = require('./events');\n";
        let census = census_js(source);
        let import = mention(&census, "f", MentionForm::Import);
        assert_eq!(import.line, 0);
        assert_eq!(import.binding, OccurrenceBinding::Import);
        assert_eq!(import.module.as_deref(), Some("m"));
        let statement = source.find(";\n").unwrap() as u32 + 1;
        assert_eq!(import.import, Some((0, statement)));
        assert_eq!(import.receiver, None);
        // An alias equal to the name is a plain import too.
        assert_eq!(
            mentions(&census, "g"),
            vec![(0, MentionForm::Import, OccurrenceBinding::Import, None)]
        );
        // A renaming import stays an import alias and is no mention.
        assert!(mentions(&census, "h").is_empty());
        assert_eq!(
            uses(&census, "h"),
            vec![(0, OccurrenceContext::Import, vec!["other".into()])]
        );
        // The re-export is already recorded as an export of the import.
        assert_eq!(
            uses(&census, "f"),
            vec![(1, OccurrenceContext::Export, vec!["f".into()])]
        );
        let required = mention(&census, "plain", MentionForm::Import);
        assert_eq!(required.line, 2);
        assert_eq!(required.binding, OccurrenceBinding::Import);
        assert_eq!(required.module.as_deref(), Some("./events"));
        assert!(required.import.is_some());
    }

    #[test]
    fn inert_reads_are_mentions_with_the_form_of_their_position() {
        let census = census_js(
            "import {Foo} from './foo';\n\
             class Base {}\n\
             function run(x, list) {\n\
             \x20 if (Foo) {}\n\
             \x20 return x === Base || x instanceof Foo;\n\
             }\n\
             const label = `${Base}!`;\n\
             while (!Foo) {}\n\
             for (const item of Base) {}\n\
             Base;\n",
        );
        assert_eq!(
            mentions(&census, "Foo"),
            vec![
                (0, MentionForm::Import, OccurrenceBinding::Import, None),
                (3, MentionForm::Condition, OccurrenceBinding::Import, None),
                (4, MentionForm::Comparison, OccurrenceBinding::Import, None),
                (7, MentionForm::Condition, OccurrenceBinding::Import, None),
            ]
        );
        let condition = mention(&census, "Foo", MentionForm::Condition);
        assert_eq!(condition.module.as_deref(), Some("./foo"));
        assert!(condition.import.is_some());
        assert_eq!(
            mentions(&census, "Base"),
            vec![
                (4, MentionForm::Comparison, OccurrenceBinding::Module, None),
                (6, MentionForm::Other, OccurrenceBinding::Module, None),
                (8, MentionForm::Other, OccurrenceBinding::Module, None),
                (9, MentionForm::Other, OccurrenceBinding::Module, None),
            ]
        );
        assert_eq!(
            mention(&census, "Base", MentionForm::Comparison).target_line,
            Some(1)
        );
        // Nothing flows anywhere, and the parameters are not recorded.
        assert!(uses(&census, "Foo").is_empty());
        assert!(uses(&census, "Base").is_empty());
        assert!(!census.record.values.contains_key("x"));
        assert!(!census.record.values.contains_key("list"));
    }

    #[test]
    fn plain_locals_and_parameters_are_not_mentioned() {
        let census = census_js(
            "function run(a, b) {\n\
             \x20 const c = 1;\n\
             \x20 let d;\n\
             \x20 if (a && b) { d = c; }\n\
             \x20 if (a === c ? `${d}` : a.size) { d++; b.n += 1; }\n\
             }\n",
        );
        for name in ["a", "b", "c", "d"] {
            assert!(
                !census.record.values.contains_key(name),
                "{name}: {:?}",
                census.record.values
            );
        }
        // A member of a local is still a member read.
        assert_eq!(
            mentions(&census, "size"),
            vec![(
                4,
                MentionForm::Condition,
                OccurrenceBinding::Member,
                Some("a".into())
            )]
        );
        assert_eq!(mentions(&census, "n")[0].1, MentionForm::Store);
    }

    #[test]
    fn receivers_and_inert_member_reads_are_mentions() {
        let census = census_js(
            "import * as ns from './ns';\n\
             class Foo {}\n\
             function run(obj, k) {\n\
             \x20 use(Foo.bar);\n\
             \x20 if (obj.flag) {}\n\
             \x20 ns.helpers.make();\n\
             \x20 this.store.items.clear();\n\
             \x20 new ns.Widget();\n\
             \x20 use(Foo[k], obj?.maybe === 1);\n\
             }\n",
        );
        assert_eq!(
            mentions(&census, "Foo"),
            vec![
                (3, MentionForm::Receiver, OccurrenceBinding::Module, None),
                (8, MentionForm::Subscript, OccurrenceBinding::Module, None),
            ]
        );
        // The value read of `bar` is unchanged.
        assert_eq!(
            uses(&census, "bar"),
            vec![(3, OccurrenceContext::Argument, vec![])]
        );
        assert_eq!(
            mentions(&census, "flag"),
            vec![(
                4,
                MentionForm::Condition,
                OccurrenceBinding::Member,
                Some("obj".into())
            )]
        );
        let helpers = mention(&census, "helpers", MentionForm::Receiver);
        assert_eq!(helpers.receiver.as_deref(), Some("ns"));
        assert_eq!(helpers.receiver_kind, Some(ReceiverKind::Namespace));
        assert_eq!(
            mentions(&census, "ns"),
            vec![
                (5, MentionForm::Receiver, OccurrenceBinding::Import, None),
                (7, MentionForm::Receiver, OccurrenceBinding::Import, None),
            ]
        );
        // `make` and `Widget` are called: not mentions.
        assert!(!census.record.values.contains_key("make"));
        assert!(!census.record.values.contains_key("Widget"));
        // `this.store.items.clear()`: `items` off `this.store`, `store` off
        // the instance.
        assert_eq!(
            mention(&census, "items", MentionForm::Receiver)
                .receiver
                .as_deref(),
            Some("this.store")
        );
        assert_eq!(
            mention(&census, "store", MentionForm::Receiver).receiver_kind,
            Some(ReceiverKind::Instance)
        );
        assert_eq!(
            mentions(&census, "maybe"),
            vec![(
                8,
                MentionForm::Comparison,
                OccurrenceBinding::Member,
                Some("obj".into())
            )]
        );
    }

    #[test]
    fn member_stores_are_mentions_of_the_member() {
        let census = census_js(
            "const registry = {};\n\
             function run(value) {\n\
             \x20 registry.handler = value;\n\
             \x20 registry.count += 1;\n\
             \x20 registry.count++;\n\
             \x20 registry['named'] = value;\n\
             }\n\
             registry.alias = run;\n",
        );
        assert_eq!(
            mentions(&census, "handler"),
            vec![(
                2,
                MentionForm::Store,
                OccurrenceBinding::Member,
                Some("registry".into())
            )]
        );
        let count: Vec<(u32, MentionForm)> = mentions(&census, "count")
            .into_iter()
            .map(|(line, form, _, _)| (line, form))
            .collect();
        assert_eq!(
            count,
            vec![(3, MentionForm::Store), (4, MentionForm::Store)]
        );
        assert_eq!(mentions(&census, "named")[0].1, MentionForm::Store);
        // The binding a store from a name makes is kept beside the mention.
        assert_eq!(
            uses(&census, "alias"),
            vec![(7, OccurrenceContext::Binding, vec![])]
        );
        assert_eq!(mentions(&census, "alias")[0].1, MentionForm::Store);
        let receivers: Vec<(u32, MentionForm)> = mentions(&census, "registry")
            .into_iter()
            .map(|(line, form, _, _)| (line, form))
            .collect();
        assert_eq!(
            receivers,
            vec![
                (2, MentionForm::Receiver),
                (3, MentionForm::Receiver),
                (4, MentionForm::Receiver),
                (5, MentionForm::Subscript),
                (7, MentionForm::Receiver),
            ]
        );
    }

    #[test]
    fn decorators_bases_and_jsx_names_are_mentions_beside_their_sites() {
        let census = census_ts(
            "import {observer, ns} from './deco';\n\
             class Base {}\n\
             @observer\n\
             class A extends Base {\n\
             \x20 @ns.track method() {}\n\
             \x20 @observer() other() {}\n\
             }\n\
             const view = <ns.Row><Cell /></ns.Row>;\n",
        );
        assert_eq!(
            mentions(&census, "observer"),
            vec![
                (0, MentionForm::Import, OccurrenceBinding::Import, None),
                (2, MentionForm::Decorator, OccurrenceBinding::Import, None),
            ],
            "a decorator call stays a call"
        );
        assert_eq!(
            mentions(&census, "track"),
            vec![(
                4,
                MentionForm::Decorator,
                OccurrenceBinding::Member,
                Some("ns".into())
            )]
        );
        let protocol: Vec<(u32, ProtocolForm, Option<&str>)> = census
            .record
            .protocol
            .iter()
            .map(|site| (site.line, site.form, site.name.as_deref()))
            .collect();
        assert_eq!(
            protocol,
            vec![
                (2, ProtocolForm::Decorator, Some("observer")),
                (4, ProtocolForm::Decorator, Some("track")),
                (7, ProtocolForm::Jsx, Some("Row")),
                (7, ProtocolForm::Jsx, Some("Cell")),
            ]
        );
        assert_eq!(
            mentions(&census, "Row"),
            vec![(
                7,
                MentionForm::Other,
                OccurrenceBinding::Member,
                Some("ns".into())
            )],
            "the closing tag is the same element"
        );
        assert_eq!(mentions(&census, "Cell")[0].1, MentionForm::Other);
        // TypeScript already records `extends Base` as a value read.
        assert_eq!(
            uses(&census, "Base"),
            vec![(3, OccurrenceContext::Other, vec![])]
        );
        let js = census_js(
            "import * as ns from './ns';\n\
             class Base {}\n\
             class A extends Base {}\n\
             class B extends ns.Widget {}\n\
             @decorate class C {}\n\
             const host = <div />;\n\
             class D { @tracked method() {} @field x = 1; }\n",
        );
        assert_eq!(
            mentions(&js, "Base"),
            vec![(2, MentionForm::Base, OccurrenceBinding::Module, None)]
        );
        assert_eq!(mention(&js, "Base", MentionForm::Base).target_line, Some(1));
        let widget = mention(&js, "Widget", MentionForm::Base);
        assert_eq!(widget.binding, OccurrenceBinding::Member);
        assert_eq!(widget.receiver_kind, Some(ReceiverKind::Namespace));
        assert_eq!(mentions(&js, "ns")[0].1, MentionForm::Receiver);
        assert_eq!(mentions(&js, "decorate")[0].1, MentionForm::Decorator);
        assert!(!js.record.values.contains_key("div"));
        // JavaScript holds a member's decorators on the member.
        assert_eq!(mentions(&js, "tracked")[0].1, MentionForm::Decorator);
        assert_eq!(mentions(&js, "field")[0].1, MentionForm::Decorator);
        let decorated: Vec<Option<&str>> = js
            .record
            .protocol
            .iter()
            .filter(|site| site.form == ProtocolForm::Decorator)
            .map(|site| site.name.as_deref())
            .collect();
        assert_eq!(
            decorated,
            vec![Some("decorate"), Some("tracked"), Some("field")]
        );
    }

    /// A type position reads the type namespace: a file's own interface,
    /// type alias or enum binds there even when it is not exported, and a
    /// parameter or variable of the same name does not hide it.
    #[test]
    fn type_names_resolve_in_the_type_namespace() {
        let census = census_ts(
            "interface View { id: number }\n\
             type Options = { view: View };\n\
             enum Mode { On, Off }\n\
             function render(View: View, opts: Options): Mode {\n\
             \x20 const Options = 1;\n\
             \x20 let local: Options;\n\
             \x20 return opts.view.id > 0 ? Mode.On : Mode.Off;\n\
             }\n",
        );
        let view: Vec<(u32, OccurrenceBinding, Option<u32>)> = census.record.values["View"]
            .iter()
            .map(|value| (value.line, value.binding, value.target_line))
            .collect();
        assert_eq!(
            view,
            vec![
                (1, OccurrenceBinding::Module, Some(0)),
                (3, OccurrenceBinding::Module, Some(0)),
            ],
            "the parameter `View` binds no type"
        );
        let options: Vec<(u32, OccurrenceBinding, Option<u32>)> = census.record.values["Options"]
            .iter()
            .map(|value| (value.line, value.binding, value.target_line))
            .collect();
        assert_eq!(
            options,
            vec![
                (3, OccurrenceBinding::Module, Some(1)),
                (5, OccurrenceBinding::Module, Some(1)),
            ],
            "the local `const Options` binds no type"
        );
        // An enum is a value too: its mention in an expression falls back
        // on the enum the file declares.
        let mode = mention(&census, "Mode", MentionForm::Receiver);
        assert_eq!(mode.binding, OccurrenceBinding::Module);
        assert_eq!(mode.target_line, Some(2));
        assert_eq!(
            mention(&census, "Mode", MentionForm::Annotation).target_line,
            Some(2)
        );
    }

    #[test]
    fn typescript_type_positions_are_annotation_mentions() {
        let census = census_ts(
            "import type {Foo} from './foo';\n\
             import * as ns from './ns';\n\
             let a: Foo;\n\
             function f<T extends Foo>(x: T, y: typeof handler): ns.Bar { return x as Baz<T>; }\n\
             interface Shape extends Base<Foo> { size: Size; get(k: Key): Value; }\n\
             type Pick<K> = { [P in keyof K]: K[P] extends infer U ? U : Fallback };\n\
             class Impl implements Shape { run(): void {} }\n\
             enum Mode { A = Limits.max, B = 1 }\n\
             declare const config: Config;\n\
             const handler = 1;\n",
        );
        let lines = |name: &str| -> Vec<(u32, MentionForm)> {
            mentions(&census, name)
                .into_iter()
                .map(|(line, form, _, _)| (line, form))
                .collect()
        };
        assert_eq!(
            lines("Foo"),
            vec![
                (0, MentionForm::Import),
                (2, MentionForm::Annotation),
                (3, MentionForm::Annotation),
                (4, MentionForm::Annotation),
            ]
        );
        assert_eq!(
            mention(&census, "Foo", MentionForm::Annotation).binding,
            OccurrenceBinding::Import
        );
        // `ns.Bar`: a mention of `ns` and of the member `Bar` off it.
        let bar = mention(&census, "Bar", MentionForm::Annotation);
        assert_eq!(bar.line, 3);
        assert_eq!(bar.binding, OccurrenceBinding::Member);
        assert_eq!(bar.receiver.as_deref(), Some("ns"));
        assert_eq!(bar.receiver_kind, Some(ReceiverKind::Namespace));
        assert_eq!(lines("ns"), vec![(3, MentionForm::Annotation)]);
        assert_eq!(lines("handler"), vec![(3, MentionForm::Annotation)]);
        for name in [
            "Baz", "Base", "Size", "Key", "Value", "Fallback", "Shape", "Config",
        ] {
            assert!(
                lines(name)
                    .contains(&(census.record.values[name][0].line, MentionForm::Annotation)),
                "{name}: {:?}",
                census.record.values.get(name)
            );
        }
        // Type parameters, declared names, members and parameters are not
        // reads.
        for name in [
            "T", "K", "P", "U", "Pick", "Mode", "size", "get", "k", "x", "run",
        ] {
            assert!(
                !census.record.values.contains_key(name),
                "{name}: {:?}",
                census.record.values.get(name)
            );
        }
        // An enum member's initializer runs, and its value goes nowhere.
        assert_eq!(lines("Limits"), vec![(7, MentionForm::Receiver)]);
        assert_eq!(lines("max"), vec![(7, MentionForm::Other)]);
        // Nothing here is a value flow.
        assert!(uses(&census, "Foo").is_empty());
        assert!(uses(&census, "max").is_empty());
    }
}
