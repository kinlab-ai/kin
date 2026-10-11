// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Python's facts for the value-escape record (see [`crate::value_escape`]).
//!
//! One walk over the tree, with Python's own scoping: a name assigned
//! anywhere in a function is local to the whole function unless declared
//! `global` or `nonlocal`, a class body is a scope its methods do not see,
//! and a comprehension binds its targets in a scope of its own. A read of a
//! plain local or parameter is never recorded, since it cannot be a
//! declaration of another file; a read of a nested `def` or `class` is.
//!
//! Recorded:
//!
//! - every read of a name or attribute whose value flows on (an argument, an
//!   assignment, a return, a container, an export), never a call's callee or
//!   a declaration;
//! - every other read of a name or attribute as a mention
//!   ([`OccurrenceContext::Mention`]): a receiver, an inert use (a condition,
//!   a comparison, an f-string, a `with` or `for` operand, an argument a
//!   builtin only inspects), a type annotation (a quoted forward reference
//!   included), a decorator, a class base, an except clause, a subscript, a
//!   match pattern, a plain `from m import f`, and a store into a member
//!   (`obj.f = value`); the record says so with
//!   [`FileEscapeRecord::lists_mentions`];
//! - `from m import f as g` as an import alias, and module- or class-scope
//!   targets that rebind a name to another name (`pop = _fail`);
//! - reflective sites: `getattr`/`operator.attrgetter`/`methodcaller` by a
//!   computed name, `__dict__` and `vars(obj)`, `globals()` and module-level
//!   `locals()`/`vars()`, `eval`/`exec`, `__import__` and `import_module` by
//!   a computed name, and `inspect.getmembers`, each with what its receiver
//!   is (a class or function the file declares, or a whole-module import) and
//!   every literal run of its key (an f-string, `+`, `%` or `.format`);
//!   `compile` alone runs nothing and is recorded as reaching nothing;
//! - every class with the bases and metaclass it names;
//! - protocol sites: `with` and `async with` items and `raise Name`;
//! - protocol members: dunder methods and properties.
//!
//! A bare decorator is not a protocol site here: the adapter already emits a
//! call relation for it.

use std::collections::{HashMap, HashSet};

use kin_model::{
    ClassBases, FileEscapeRecord, LookupReceiver, MentionForm, ModuleScope, OccurrenceBinding,
    OccurrenceContext, ProtocolForm, ProtocolMember, ProtocolSite, ReceiverKind, ReflectiveForm,
    ReflectiveReach, ReflectiveSite, ValueOccurrence, EXCERPT_MAX, RECEIVER_TEXT_MAX,
};
use tree_sitter::{Node, Tree};

use crate::value_escape::{
    collapsed_text, is_field, line_of, plain_string_literal, push_template, text_of,
    unparsed_lines, FileEscapeCensus, KeyLiterals, ProtocolStamp,
};

/// Builtins whose arguments are inspected rather than kept or called, so
/// passing a declaration to one does not let it escape.
const INERT_ARGUMENT_BUILTINS: &[&str] = &[
    "isinstance",
    "issubclass",
    "callable",
    "id",
    "hash",
    "repr",
    "str",
    "type",
    "print",
    "len",
    "bool",
    "hasattr",
];

/// Members that hand out the underlying function of a bound method or a
/// wrapper, so reading one reads the function itself.
const FUNCTION_EXPOSING: &[&str] = &["__func__", "__get__", "__wrapped__"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScopeKind {
    Module,
    Class,
    Function,
    Comprehension,
}

/// What a scope binds a name to.
#[derive(Debug, Clone, Default)]
struct Binds {
    /// Lines of `def` and `class` statements binding the name.
    definitions: Vec<u32>,
    /// Whether anything else (an assignment, a loop target, a parameter)
    /// binds it.
    other: bool,
    /// Import statements binding it: the module, the imported name, and the
    /// statement's byte range.
    imports: Vec<(String, String, (u32, u32))>,
    /// How many of `imports` bind a whole module object (`import a.b`,
    /// `import a.b as c`), never a name from inside one.
    module_imports: usize,
}

#[derive(Debug)]
struct Scope {
    kind: ScopeKind,
    names: HashMap<String, Binds>,
    /// `global` and `nonlocal` names, which bind in an outer scope.
    outward: HashSet<String>,
    /// Assignments that load a module by a literal name, each waiting for
    /// the scope's whole table before it is read as an import or as any
    /// other assignment.
    loads: Vec<ModuleLoad>,
}

/// `target = loader.function("module", ...)`, an assignment that binds
/// `target` to the module a library function imports by name.
#[derive(Debug)]
struct ModuleLoad {
    target: String,
    /// The name the loading function is read off (`pytest`, `importlib`).
    loader: &'static str,
    module: String,
    range: (u32, u32),
}

/// Functions that return the module their first argument names:
/// `pytest.importorskip("tomllib")` and `importlib.import_module("tomllib")`,
/// as (the module they are read off, the function).
const MODULE_LOADERS: [(&str, &str); 2] =
    [("pytest", "importorskip"), ("importlib", "import_module")];

/// Take the census of one Python file.
pub(crate) fn census(tree: &Tree, source: &[u8]) -> FileEscapeCensus {
    let root = tree.root_node();
    let mut walker = Walker {
        source,
        record: FileEscapeRecord::new(ModuleScope::Open),
        protocol_members: Vec::new(),
        scopes: Vec::new(),
    };
    walker.record.unparsed = unparsed_lines(tree, source);
    walker.record.lists_mentions = true;
    let module = walker.scope_of(root, ScopeKind::Module);
    walker.scopes.push(module);
    walker.visit_children(root);
    walker.scopes.pop();
    FileEscapeCensus {
        record: walker.record,
        protocol_members: walker.protocol_members,
        non_enumerable: Vec::new(),
    }
}

/// Whether the class `class` names `Protocol` among its bases, bare,
/// subscripted or read off a module (`t.Protocol[T]`): the class is a
/// structural type.
pub(crate) fn names_protocol_base(class: Node<'_>, source: &[u8]) -> bool {
    fn is_protocol(base: Node<'_>, source: &[u8]) -> bool {
        match base.kind() {
            "identifier" => text_of(base, source) == "Protocol",
            "attribute" => base
                .child_by_field_name("attribute")
                .is_some_and(|attribute| text_of(attribute, source) == "Protocol"),
            "subscript" => base
                .child_by_field_name("value")
                .is_some_and(|value| is_protocol(value, source)),
            _ => false,
        }
    }
    class
        .child_by_field_name("superclasses")
        .is_some_and(|list| {
            let mut cursor = list.walk();
            let named = list
                .named_children(&mut cursor)
                .any(|base| is_protocol(base, source));
            named
        })
}

/// The members through which a Python class answers an attribute read by
/// its name, whatever its hierarchy declares.
const ATTRIBUTE_HOOKS: [&str; 2] = ["__getattr__", "__getattribute__"];

/// Whether the class `class` binds one of [`ATTRIBUTE_HOOKS`] in its own
/// body, by a `def` (decorated or not) or an assignment.
pub(crate) fn declares_attribute_hook(class: Node<'_>, source: &[u8]) -> bool {
    let Some(body) = class.child_by_field_name("body") else {
        return false;
    };
    let hook = |node: Option<Node<'_>>| {
        node.is_some_and(|name| ATTRIBUTE_HOOKS.contains(&text_of(name, source)))
    };
    let mut cursor = body.walk();
    let found = body.named_children(&mut cursor).any(|statement| {
        let definition = if statement.kind() == "decorated_definition" {
            statement.child_by_field_name("definition")
        } else {
            Some(statement)
        };
        match definition.map(|node| (node.kind(), node)) {
            Some(("function_definition", function)) => hook(function.child_by_field_name("name")),
            Some(("expression_statement", expression)) => expression
                .named_child(0)
                .filter(|assignment| assignment.kind() == "assignment")
                .is_some_and(|assignment| hook(assignment.child_by_field_name("left"))),
            _ => false,
        }
    });
    found
}

/// Whether `node` sits in the body of an `if TYPE_CHECKING:` (or
/// `if <module>.TYPE_CHECKING:`), which runs for a type checker and never
/// when the program does. The `else` of such an `if` does run.
pub(crate) fn under_type_checking(node: Node<'_>, source: &[u8]) -> bool {
    let mut inner = node;
    while let Some(parent) = inner.parent() {
        if parent.kind() == "if_statement"
            && parent
                .child_by_field_name("consequence")
                .is_some_and(|body| body.id() == inner.id())
            && parent.child_by_field_name("condition").is_some_and(|test| {
                let test = text_of(test, source);
                test == "TYPE_CHECKING" || test.ends_with(".TYPE_CHECKING")
            })
        {
            return true;
        }
        inner = parent;
    }
    false
}

struct Walker<'s> {
    source: &'s [u8],
    record: FileEscapeRecord,
    protocol_members: Vec<ProtocolStamp>,
    scopes: Vec<Scope>,
}

impl<'s> Walker<'s> {
    fn text(&self, node: Node<'_>) -> &'s str {
        text_of(node, self.source)
    }

    // ── Scopes ────────────────────────────────────────────────────────────

    /// The binding table of the scope `node` opens: its parameters and every
    /// name its body binds, without entering nested scopes.
    fn scope_of(&self, node: Node<'_>, kind: ScopeKind) -> Scope {
        let mut scope = Scope {
            kind,
            names: HashMap::new(),
            outward: HashSet::new(),
            loads: Vec::new(),
        };
        match kind {
            ScopeKind::Function => {
                if let Some(params) = node.child_by_field_name("parameters") {
                    self.bind_parameters(params, &mut scope);
                }
                if let Some(body) = node.child_by_field_name("body") {
                    self.collect_binds(body, &mut scope);
                }
            }
            ScopeKind::Comprehension => {
                let mut cursor = node.walk();
                for clause in node.named_children(&mut cursor) {
                    if clause.kind() == "for_in_clause" {
                        if let Some(left) = clause.child_by_field_name("left") {
                            self.bind_targets(left, &mut scope, None);
                        }
                    }
                }
            }
            ScopeKind::Class => {
                if let Some(body) = node.child_by_field_name("body") {
                    self.collect_binds(body, &mut scope);
                }
            }
            ScopeKind::Module => self.collect_binds(node, &mut scope),
        }
        self.settle_loads(&mut scope);
        scope
    }

    /// Read each module-loading assignment of a scope as an import of the
    /// module it names, when the name its loader is read off is that library
    /// module and nothing else; as any other assignment when it is not.
    fn settle_loads(&self, scope: &mut Scope) {
        for load in std::mem::take(&mut scope.loads) {
            let is_loader = |binds: &Binds| {
                binds.definitions.is_empty()
                    && !binds.other
                    && binds.module_imports == 1
                    && matches!(binds.imports.as_slice(), [(module, _, _)] if module == load.loader)
            };
            let loader = match scope.names.get(load.loader) {
                Some(binds) => is_loader(binds),
                // Not bound here: the nearest enclosing scope that binds it,
                // never a class body, which code nested in it does not see.
                None => self
                    .scopes
                    .iter()
                    .rev()
                    .filter(|outer| outer.kind != ScopeKind::Class)
                    .find_map(|outer| {
                        if outer.outward.contains(load.loader) {
                            return None;
                        }
                        outer.names.get(load.loader)
                    })
                    .is_some_and(is_loader),
            };
            let binds = scope.names.entry(load.target).or_default();
            if loader {
                binds
                    .imports
                    .push((load.module.clone(), load.module, load.range));
                binds.module_imports += 1;
            } else {
                binds.other = true;
            }
        }
    }

    /// The module-loading assignment `assignment` is, when it is one: a
    /// single name assigned what a call of one of [`MODULE_LOADERS`] returns,
    /// whose first argument is a plain string naming a module absolutely and
    /// whose other arguments are keywords that do not make the name relative.
    fn module_load(&self, assignment: Node<'_>) -> Option<ModuleLoad> {
        let left = assignment.child_by_field_name("left")?;
        let right = assignment.child_by_field_name("right")?;
        if assignment.kind() != "assignment"
            || left.kind() != "identifier"
            || right.kind() != "call"
        {
            return None;
        }
        let function = right.child_by_field_name("function")?;
        if function.kind() != "attribute" {
            return None;
        }
        let object = function.child_by_field_name("object")?;
        let attribute = self.text(function.child_by_field_name("attribute")?);
        let (loader, _) = MODULE_LOADERS.iter().find(|(loader, function)| {
            object.kind() == "identifier" && self.text(object) == *loader && attribute == *function
        })?;
        let arguments = right.child_by_field_name("arguments")?;
        let mut cursor = arguments.walk();
        let mut module = None;
        for (index, argument) in arguments.named_children(&mut cursor).enumerate() {
            match argument.kind() {
                "string" if index == 0 => {
                    let mut pieces = Vec::new();
                    if !self.key_pieces(argument, &mut pieces) {
                        return None;
                    }
                    let [Some(name)] = pieces.as_slice() else {
                        return None;
                    };
                    module = Some(name.clone());
                }
                "keyword_argument" if index > 0 => {
                    let keyword = argument.child_by_field_name("name")?;
                    if self.text(keyword) == "package" {
                        return None;
                    }
                }
                _ => return None,
            }
        }
        let module = module.filter(|module| {
            module
                .split('.')
                .all(|segment| !segment.is_empty() && is_plain_identifier(segment))
        })?;
        Some(ModuleLoad {
            target: self.text(left).to_string(),
            loader,
            module,
            range: (assignment.start_byte() as u32, assignment.end_byte() as u32),
        })
    }

    fn bind_parameters(&self, params: Node<'_>, scope: &mut Scope) {
        let mut cursor = params.walk();
        for param in params.named_children(&mut cursor) {
            match param.kind() {
                "identifier" => self.bind_name(param, scope, None),
                "default_parameter" | "typed_default_parameter" => {
                    if let Some(name) = param.child_by_field_name("name") {
                        self.bind_name(name, scope, None);
                    }
                }
                "typed_parameter" | "list_splat_pattern" | "dictionary_splat_pattern" => {
                    let mut inner = param.walk();
                    for child in param.named_children(&mut inner) {
                        if child.kind() == "identifier" {
                            self.bind_name(child, scope, None);
                            break;
                        }
                        if matches!(
                            child.kind(),
                            "list_splat_pattern" | "dictionary_splat_pattern"
                        ) {
                            self.bind_parameters_of(child, scope);
                        }
                    }
                }
                _ => self.bind_targets(param, scope, None),
            }
        }
    }

    fn bind_parameters_of(&self, node: Node<'_>, scope: &mut Scope) {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.kind() == "identifier" {
                self.bind_name(child, scope, None);
            }
        }
    }

    fn bind_name(&self, node: Node<'_>, scope: &mut Scope, definition: Option<u32>) {
        let name = self.text(node);
        if name.is_empty() {
            return;
        }
        let binds = scope.names.entry(name.to_string()).or_default();
        match definition {
            Some(line) => binds.definitions.push(line),
            None => binds.other = true,
        }
    }

    /// Bind every identifier a target writes: a name, or the names of a
    /// tuple or list pattern. An attribute or subscript target binds none.
    fn bind_targets(&self, node: Node<'_>, scope: &mut Scope, definition: Option<u32>) {
        match node.kind() {
            "identifier" => self.bind_name(node, scope, definition),
            "attribute" | "subscript" => {}
            _ => {
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    self.bind_targets(child, scope, definition);
                }
            }
        }
    }

    /// Every name the statements under `node` bind in its scope, without
    /// entering a nested function, class, lambda or comprehension.
    fn collect_binds(&self, node: Node<'_>, scope: &mut Scope) {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            match child.kind() {
                "function_definition" | "class_definition" => {
                    if let Some(name) = child.child_by_field_name("name") {
                        self.bind_name(name, scope, Some(line_of(child)));
                    }
                    // Decorators, defaults and bases are evaluated here, but
                    // bind nothing a walrus would not also reach.
                }
                "decorated_definition" => self.collect_binds(child, scope),
                "lambda"
                | "list_comprehension"
                | "set_comprehension"
                | "dictionary_comprehension"
                | "generator_expression" => {
                    // A walrus inside a comprehension binds in this scope.
                    self.collect_walrus(child, scope);
                }
                "assignment" | "augmented_assignment" => {
                    // `a = b = value` nests an assignment on the right.
                    let mut current = Some(child);
                    while let Some(node) = current {
                        current = None;
                        if let Some(load) = self.module_load(node) {
                            scope.loads.push(load);
                            continue;
                        }
                        if let Some(left) = node.child_by_field_name("left") {
                            self.bind_targets(left, scope, None);
                        }
                        match node.child_by_field_name("right") {
                            Some(right)
                                if matches!(
                                    right.kind(),
                                    "assignment" | "augmented_assignment"
                                ) =>
                            {
                                current = Some(right)
                            }
                            Some(right) => self.collect_binds_in(right, scope),
                            None => {}
                        }
                    }
                }
                "for_statement" => {
                    if let Some(left) = child.child_by_field_name("left") {
                        self.bind_targets(left, scope, None);
                    }
                    self.collect_binds(child, scope);
                }
                "named_expression" => {
                    if let Some(name) = child.child_by_field_name("name") {
                        self.bind_name(name, scope, None);
                    }
                    self.collect_binds(child, scope);
                }
                "as_pattern" => {
                    if let Some(alias) = child.child_by_field_name("alias") {
                        self.bind_targets(alias, scope, None);
                    }
                    self.collect_binds(child, scope);
                }
                "with_item" => {
                    // `with open() as fh`: the target sits in an as_pattern.
                    self.collect_binds(child, scope);
                }
                "except_clause" => {
                    self.collect_except_alias(child, scope);
                    self.collect_binds(child, scope);
                }
                "import_statement" => self.bind_import(child, scope),
                "import_from_statement" => self.bind_from_import(child, scope),
                "global_statement" | "nonlocal_statement" => {
                    let mut inner = child.walk();
                    for name in child.named_children(&mut inner) {
                        if name.kind() == "identifier" {
                            scope.outward.insert(self.text(name).to_string());
                        }
                    }
                }
                "delete_statement" => {}
                "case_pattern" | "case_clause" | "match_statement" => {
                    self.collect_match_captures(child, scope);
                    self.collect_binds(child, scope);
                }
                _ => self.collect_binds(child, scope),
            }
        }
    }

    fn collect_binds_in(&self, node: Node<'_>, scope: &mut Scope) {
        if node.kind() == "named_expression" {
            if let Some(name) = node.child_by_field_name("name") {
                self.bind_name(name, scope, None);
            }
        }
        if matches!(
            node.kind(),
            "lambda"
                | "list_comprehension"
                | "set_comprehension"
                | "dictionary_comprehension"
                | "generator_expression"
        ) {
            self.collect_walrus(node, scope);
            return;
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            self.collect_binds_in(child, scope);
        }
    }

    fn collect_walrus(&self, node: Node<'_>, scope: &mut Scope) {
        if node.kind() == "named_expression" {
            if let Some(name) = node.child_by_field_name("name") {
                self.bind_name(name, scope, None);
            }
        }
        if matches!(node.kind(), "function_definition" | "class_definition") {
            return;
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            self.collect_walrus(child, scope);
        }
    }

    fn collect_except_alias(&self, clause: Node<'_>, scope: &mut Scope) {
        // `except E as e`: older grammars put the alias straight on the
        // clause after an `as` token.
        let mut cursor = clause.walk();
        let mut after_as = false;
        for child in clause.children(&mut cursor) {
            if child.kind() == "as" {
                after_as = true;
                continue;
            }
            if after_as && child.kind() == "identifier" {
                self.bind_name(child, scope, None);
                after_as = false;
            }
        }
    }

    fn collect_match_captures(&self, node: Node<'_>, scope: &mut Scope) {
        if node.kind() == "case_pattern" {
            let mut stack = vec![node];
            while let Some(current) = stack.pop() {
                if current.kind() == "identifier" {
                    self.bind_name(current, scope, None);
                    continue;
                }
                let mut cursor = current.walk();
                stack.extend(current.named_children(&mut cursor));
            }
        }
    }

    fn bind_import(&self, statement: Node<'_>, scope: &mut Scope) {
        let range = (statement.start_byte() as u32, statement.end_byte() as u32);
        let mut cursor = statement.walk();
        for child in statement.named_children(&mut cursor) {
            match child.kind() {
                // `import a.b` binds `a`.
                "dotted_name" => {
                    let full = self.text(child).to_string();
                    let head = full.split('.').next().unwrap_or("").to_string();
                    let binds = scope.names.entry(head.clone()).or_default();
                    binds.imports.push((full, head, range));
                    binds.module_imports += 1;
                }
                "aliased_import" => {
                    let module = child
                        .child_by_field_name("name")
                        .map(|name| self.text(name).to_string())
                        .unwrap_or_default();
                    if let Some(alias) = child.child_by_field_name("alias") {
                        let binds = scope.names.entry(self.text(alias).to_string()).or_default();
                        binds.imports.push((module.clone(), module, range));
                        binds.module_imports += 1;
                    }
                }
                _ => {}
            }
        }
    }

    fn bind_from_import(&self, statement: Node<'_>, scope: &mut Scope) {
        let range = (statement.start_byte() as u32, statement.end_byte() as u32);
        let module = statement
            .child_by_field_name("module_name")
            .map(|node| self.text(node).to_string())
            .unwrap_or_default();
        let mut cursor = statement.walk();
        for child in statement.children_by_field_name("name", &mut cursor) {
            match child.kind() {
                "dotted_name" => {
                    let name = self.text(child).to_string();
                    scope.names.entry(name.clone()).or_default().imports.push((
                        module.clone(),
                        name,
                        range,
                    ));
                }
                "aliased_import" => {
                    let imported = child
                        .child_by_field_name("name")
                        .map(|name| self.text(name).to_string())
                        .unwrap_or_default();
                    if let Some(alias) = child.child_by_field_name("alias") {
                        scope
                            .names
                            .entry(self.text(alias).to_string())
                            .or_default()
                            .imports
                            .push((module.clone(), imported, range));
                    }
                }
                _ => {}
            }
        }
    }

    /// How a read of `name` in the current scope binds, or `None` for a plain
    /// local, parameter or comprehension target.
    fn resolve(&self, name: &str) -> Option<(OccurrenceBinding, Option<&Binds>)> {
        let innermost = self.scopes.len().checked_sub(1)?;
        for (depth, scope) in self.scopes.iter().enumerate().rev() {
            // A class body is visible only to code directly in it.
            if scope.kind == ScopeKind::Class && depth != innermost {
                continue;
            }
            if scope.outward.contains(name) {
                continue;
            }
            let Some(binds) = scope.names.get(name) else {
                continue;
            };
            return match scope.kind {
                ScopeKind::Module => Some((
                    if binds.definitions.is_empty() && !binds.other {
                        OccurrenceBinding::Import
                    } else {
                        OccurrenceBinding::Module
                    },
                    Some(binds),
                )),
                ScopeKind::Class => Some((OccurrenceBinding::Class, Some(binds))),
                ScopeKind::Function => {
                    if !binds.definitions.is_empty() {
                        Some((OccurrenceBinding::Local, Some(binds)))
                    } else if !binds.other && !binds.imports.is_empty() {
                        // A name a function binds only by importing it reads
                        // what the import brings in, as a module's does.
                        Some((OccurrenceBinding::Import, Some(binds)))
                    } else {
                        None
                    }
                }
                ScopeKind::Comprehension => None,
            };
        }
        Some((OccurrenceBinding::Free, None))
    }

    fn current_kind(&self) -> ScopeKind {
        self.scopes
            .last()
            .map_or(ScopeKind::Module, |scope| scope.kind)
    }

    // ── The walk ──────────────────────────────────────────────────────────

    fn visit_children(&mut self, node: Node<'_>) {
        let mut cursor = node.walk();
        let children: Vec<Node<'_>> = node.children(&mut cursor).collect();
        for child in children {
            self.visit(child);
        }
    }

    fn visit(&mut self, node: Node<'_>) {
        match node.kind() {
            "identifier" => self.read_name(node),
            "attribute" => self.visit_attribute(node),
            "call" => self.visit_call(node),
            "function_definition" => self.visit_function(node),
            "class_definition" => self.visit_class(node),
            "decorated_definition" => {
                let mut cursor = node.walk();
                let children: Vec<Node<'_>> = node.named_children(&mut cursor).collect();
                for child in children {
                    if child.kind() == "decorator" {
                        // A bare `@name` or `@a.name` is a call relation the
                        // adapter already emits, and a mention of the name
                        // here; a decorator call is visited as any call, so
                        // its arguments are read.
                        let mut inner = child.walk();
                        let expressions: Vec<Node<'_>> = child.named_children(&mut inner).collect();
                        for expression in expressions {
                            match expression.kind() {
                                "call" => self.visit(expression),
                                "identifier" | "attribute" => {
                                    self.visit_inert(expression, MentionForm::Decorator)
                                }
                                "comment" => {}
                                _ => self.visit(expression),
                            }
                        }
                    } else {
                        self.visit(child);
                    }
                }
            }
            "lambda" => {
                let scope = self.scope_of(node, ScopeKind::Function);
                if let Some(params) = node.child_by_field_name("parameters") {
                    self.visit_defaults(params);
                }
                self.scopes.push(scope);
                if let Some(body) = node.child_by_field_name("body") {
                    self.visit(body);
                }
                self.scopes.pop();
            }
            "list_comprehension"
            | "set_comprehension"
            | "dictionary_comprehension"
            | "generator_expression" => self.visit_comprehension(node),
            "assignment" => self.visit_assignment(node),
            "augmented_assignment" => {
                if let Some(right) = node.child_by_field_name("right") {
                    self.visit(right);
                }
                if let Some(left) = node.child_by_field_name("left") {
                    // `name += 1` reads the name it writes.
                    if left.kind() == "identifier" {
                        self.push_name_mention(left, MentionForm::Other);
                    }
                    self.visit_target(left);
                }
            }
            "named_expression" => {
                if let Some(value) = node.child_by_field_name("value") {
                    self.visit(value);
                }
            }
            "for_statement" | "for_in_clause" => {
                if let Some(left) = node.child_by_field_name("left") {
                    self.visit_target(left);
                }
                let mut cursor = node.walk();
                let children: Vec<Node<'_>> = node.named_children(&mut cursor).collect();
                for child in children {
                    if !is_field(node, "left", child) {
                        self.visit(child);
                    }
                }
            }
            "with_statement" => {
                let asynchronous = {
                    let mut cursor = node.walk();
                    let found = node
                        .children(&mut cursor)
                        .any(|child| child.kind() == "async");
                    found
                };
                self.mark_with_items(node, asynchronous);
                self.visit_children(node);
            }
            "raise_statement" => {
                self.mark_raise(node);
                self.visit_children(node);
            }
            "import_statement"
            | "future_import_statement"
            | "global_statement"
            | "nonlocal_statement" => {}
            "import_from_statement" => self.visit_from_import(node),
            "keyword_argument" => {
                if let Some(value) = node.child_by_field_name("value") {
                    self.visit(value);
                }
            }
            "as_pattern" => {
                let mut cursor = node.walk();
                let children: Vec<Node<'_>> = node.named_children(&mut cursor).collect();
                for child in children {
                    if !is_field(node, "alias", child) {
                        self.visit(child);
                    }
                }
            }
            "except_clause" => {
                // Exception types are matched, never called or kept: each is
                // a mention. The name an `as` binds is not a read.
                let mut cursor = node.walk();
                let children: Vec<Node<'_>> = node.children(&mut cursor).collect();
                let mut after_as = false;
                for child in children {
                    if child.kind() == "as" {
                        after_as = true;
                        continue;
                    }
                    if !child.is_named() {
                        continue;
                    }
                    match child.kind() {
                        "block" => self.visit(child),
                        "identifier" if after_as => after_as = false,
                        "as_pattern" => {
                            let mut inner = child.walk();
                            let parts: Vec<Node<'_>> = child.named_children(&mut inner).collect();
                            for part in parts {
                                if !is_field(child, "alias", part) {
                                    self.visit_inert(part, MentionForm::Except);
                                }
                            }
                        }
                        "comment" => {}
                        _ => self.visit_inert(child, MentionForm::Except),
                    }
                }
            }
            "type" => self.visit_inert(node, MentionForm::Annotation),
            "case_pattern" => self.visit_case_pattern(node),
            "delete_statement" => {
                let mut cursor = node.walk();
                let children: Vec<Node<'_>> = node.named_children(&mut cursor).collect();
                for child in children {
                    self.visit_inert(child, MentionForm::Other);
                }
            }
            "type_alias_statement" => {
                // `type Name = value`: the name is declared, the value read.
                let mut cursor = node.walk();
                let children: Vec<Node<'_>> = node.named_children(&mut cursor).collect();
                let value = node
                    .child_by_field_name("right")
                    .or_else(|| children.get(1).copied());
                if let Some(value) = value {
                    self.visit_inert(value, MentionForm::Annotation);
                }
            }
            "string" | "concatenated_string" | "comment" | "dotted_name" | "type_parameter" => {
                if matches!(node.kind(), "string" | "concatenated_string") {
                    // f-string interpolations may hold calls whose arguments
                    // are reads.
                    self.visit_interpolations(node);
                }
            }
            "subscript" => self.visit_subscript(node),
            _ => self.visit_children(node),
        }
    }

    fn visit_interpolations(&mut self, node: Node<'_>) {
        let mut cursor = node.walk();
        let children: Vec<Node<'_>> = node.named_children(&mut cursor).collect();
        for child in children {
            if child.kind() == "interpolation" {
                self.visit_children(child);
            } else if matches!(child.kind(), "string") {
                self.visit_interpolations(child);
            }
        }
    }

    fn visit_defaults(&mut self, params: Node<'_>) {
        let mut cursor = params.walk();
        let children: Vec<Node<'_>> = params.named_children(&mut cursor).collect();
        for param in children {
            if matches!(
                param.kind(),
                "default_parameter" | "typed_default_parameter"
            ) {
                if let Some(value) = param.child_by_field_name("value") {
                    self.visit(value);
                }
            }
        }
    }

    fn visit_function(&mut self, node: Node<'_>) {
        if let Some(params) = node.child_by_field_name("parameters") {
            self.visit_defaults(params);
            // Annotations are read where the definition is, as defaults are.
            self.visit_parameter_annotations(params);
        }
        if let Some(returns) = node.child_by_field_name("return_type") {
            self.visit_inert(returns, MentionForm::Annotation);
        }
        if let Some(parameters) = node.child_by_field_name("type_parameters") {
            self.visit_type_parameters(parameters);
        }
        if self.current_kind() == ScopeKind::Class {
            self.mark_protocol_member(node);
        }
        let scope = self.scope_of(node, ScopeKind::Function);
        self.scopes.push(scope);
        if let Some(body) = node.child_by_field_name("body") {
            self.visit(body);
        }
        self.scopes.pop();
    }

    fn visit_class(&mut self, node: Node<'_>) {
        self.record_class(node);
        if let Some(bases) = node.child_by_field_name("superclasses") {
            // Bases are inherited from, never kept, so a named base is a
            // mention; `metaclass=M` and other keywords are arguments.
            let mut cursor = bases.walk();
            let children: Vec<Node<'_>> = bases.named_children(&mut cursor).collect();
            for child in children {
                if child.kind() == "keyword_argument" {
                    self.visit(child);
                } else if matches!(child.kind(), "identifier" | "attribute") {
                    self.visit_inert(child, MentionForm::Base);
                } else {
                    self.visit(child);
                }
            }
        }
        if let Some(parameters) = node.child_by_field_name("type_parameters") {
            self.visit_type_parameters(parameters);
        }
        let scope = self.scope_of(node, ScopeKind::Class);
        self.scopes.push(scope);
        if let Some(body) = node.child_by_field_name("body") {
            self.visit(body);
        }
        self.scopes.pop();
    }

    /// Record the class `node` declares and the bases it names, read in the
    /// scope around it as its base list is.
    fn record_class(&mut self, node: Node<'_>) {
        let Some(name) = node.child_by_field_name("name") else {
            return;
        };
        // A decorator may hand back any value in place of the class.
        let mut open = node
            .parent()
            .is_some_and(|parent| parent.kind() == "decorated_definition");
        let mut bases = Vec::new();
        if let Some(list) = node.child_by_field_name("superclasses") {
            let mut cursor = list.walk();
            let children: Vec<Node<'_>> = list.named_children(&mut cursor).collect();
            for child in children {
                let base = match child.kind() {
                    "keyword_argument" => {
                        // `metaclass=M` supplies members a lookup on the
                        // class reads; other keywords are arguments.
                        let is_metaclass = child
                            .child_by_field_name("name")
                            .is_some_and(|name| self.text(name) == "metaclass");
                        if !is_metaclass {
                            continue;
                        }
                        child.child_by_field_name("value")
                    }
                    _ => Some(child),
                };
                match base.and_then(|base| self.base_name(base)) {
                    Some(name) => bases.push(name),
                    None => open = true,
                }
            }
        }
        self.record.classes.push(ClassBases {
            name: self.text(name).to_string(),
            line: line_of(node),
            bases,
            open,
            typing_only: under_type_checking(node, self.source),
            structural: names_protocol_base(node, self.source),
            dynamic: declares_attribute_hook(node, self.source),
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
                    Some((_, Some(binds))) if !binds.other && binds.imports.is_empty() => {
                        (!binds.definitions.is_empty()).then(|| name.to_string())
                    }
                    Some((_, Some(binds)))
                        if !binds.other
                            && binds.definitions.is_empty()
                            && binds.module_imports == 0 =>
                    {
                        // `from m import Base as Local`: the name `m` exports.
                        match binds.imports.as_slice() {
                            [(_, imported, _)] => Some(imported.clone()),
                            _ => None,
                        }
                    }
                    _ => None,
                }
            }
            // `module.Base`: the member the module object holds.
            "attribute" => base
                .child_by_field_name("attribute")
                .map(|attribute| self.text(attribute).to_string()),
            // `Base[T]`: the generic class itself.
            "subscript" => base
                .child_by_field_name("value")
                .and_then(|value| self.base_name(value)),
            _ => None,
        }
    }

    fn visit_comprehension(&mut self, node: Node<'_>) {
        // The first iterable is evaluated in the enclosing scope.
        let mut cursor = node.walk();
        let children: Vec<Node<'_>> = node.named_children(&mut cursor).collect();
        let first_clause = children
            .iter()
            .find(|child| child.kind() == "for_in_clause")
            .copied();
        if let Some(right) = first_clause.and_then(|clause| clause.child_by_field_name("right")) {
            self.visit(right);
        }
        let scope = self.scope_of(node, ScopeKind::Comprehension);
        self.scopes.push(scope);
        for child in children {
            if Some(child.id()) == first_clause.map(|clause| clause.id()) {
                let mut inner = child.walk();
                let parts: Vec<Node<'_>> = child.named_children(&mut inner).collect();
                for part in parts {
                    if !is_field(child, "left", part) && !is_field(child, "right", part) {
                        self.visit(part);
                    }
                }
                continue;
            }
            self.visit(child);
        }
        self.scopes.pop();
    }

    fn visit_assignment(&mut self, node: Node<'_>) {
        if let Some(annotation) = node.child_by_field_name("type") {
            self.visit_inert(annotation, MentionForm::Annotation);
        }
        if let Some(right) = node.child_by_field_name("right") {
            self.visit(right);
        }
        if let Some(left) = node.child_by_field_name("left") {
            self.visit_target(left);
            // `pop = _fail` at module or class scope, or `obj.pop = handler`,
            // binds a same-named value a call spelling the name may reach.
            let mut right = node.child_by_field_name("right");
            while let Some(inner) = right.filter(|right| right.kind() == "assignment") {
                right = inner.child_by_field_name("right");
            }
            let aliases_a_name =
                right.is_some_and(|right| matches!(right.kind(), "identifier" | "attribute"));
            if aliases_a_name {
                self.mark_binding_target(left);
            }
        }
    }

    /// Record a target that binds a name to another name's value.
    fn mark_binding_target(&mut self, target: Node<'_>) {
        match target.kind() {
            "identifier" if matches!(self.current_kind(), ScopeKind::Module | ScopeKind::Class) => {
                let name = self.text(target).to_string();
                self.push_value(
                    name,
                    ValueOccurrence {
                        line: line_of(target),
                        context: OccurrenceContext::Binding,
                        binding: if self.current_kind() == ScopeKind::Class {
                            OccurrenceBinding::Class
                        } else {
                            OccurrenceBinding::Module
                        },
                        receiver: None,
                        receiver_kind: None,
                        aliases: Vec::new(),
                        module: None,
                        import: None,
                        imported: None,
                        target_line: None,
                        form: None,
                    },
                );
            }
            "attribute" => {
                let (Some(object), Some(attribute)) = (
                    target.child_by_field_name("object"),
                    target.child_by_field_name("attribute"),
                ) else {
                    return;
                };
                let name = self.text(attribute).to_string();
                let receiver_kind = self.receiver_kind(object);
                self.push_value(
                    name,
                    ValueOccurrence {
                        line: line_of(attribute),
                        context: OccurrenceContext::Binding,
                        binding: OccurrenceBinding::Member,
                        receiver: Some(collapsed_text(object, self.source, RECEIVER_TEXT_MAX)),
                        receiver_kind: Some(receiver_kind),
                        aliases: Vec::new(),
                        module: None,
                        import: None,
                        imported: None,
                        target_line: None,
                        form: None,
                    },
                );
            }
            _ => {}
        }
    }

    /// Visit an assignment target: names bind, but the object of an
    /// attribute target and the pieces of a subscript target are reads.
    fn visit_target(&mut self, node: Node<'_>) {
        match node.kind() {
            "identifier" => {}
            "attribute" => {
                // A store into the member is a mention of it, and its object
                // is read.
                if let (Some(object), Some(attribute)) = (
                    node.child_by_field_name("object"),
                    node.child_by_field_name("attribute"),
                ) {
                    self.push_member_mention(attribute, object, MentionForm::Store);
                }
                if let Some(object) = node.child_by_field_name("object") {
                    self.visit_receiver(object, MentionForm::Receiver);
                }
            }
            "subscript" => {
                if let Some(value) = node.child_by_field_name("value") {
                    self.visit_written_container(value);
                }
                let mut cursor = node.walk();
                let indexes: Vec<Node<'_>> = node
                    .children_by_field_name("subscript", &mut cursor)
                    .collect();
                for index in indexes {
                    self.visit(index);
                }
            }
            _ => {
                let mut cursor = node.walk();
                let children: Vec<Node<'_>> = node.named_children(&mut cursor).collect();
                for child in children {
                    self.visit_target(child);
                }
            }
        }
    }

    /// Visit an expression whose value is only a receiver: its own name is
    /// not a value use, only a mention of `form`, and anything computed
    /// inside it is read as usual. A member read on the way (`a.b` in
    /// `a.b.c`) is a mention of the member, and its object a receiver.
    fn visit_receiver(&mut self, node: Node<'_>, form: MentionForm) {
        match node.kind() {
            "identifier" => self.push_name_mention(node, form),
            "attribute" => {
                if let Some(object) = node.child_by_field_name("object") {
                    let attribute = node.child_by_field_name("attribute");
                    let dict =
                        attribute.is_some_and(|attribute| self.text(attribute) == "__dict__");
                    if dict {
                        self.mark_dict(node, object);
                    } else if let Some(attribute) = attribute {
                        self.push_member_mention(attribute, object, form);
                    }
                    self.visit_receiver(object, MentionForm::Receiver);
                }
            }
            _ => self.visit(node),
        }
    }

    /// The object of a written subscript: `obj.__dict__[k] = v` writes, so its
    /// `__dict__` is not read; any other member written through is
    /// mentioned.
    fn visit_written_container(&mut self, node: Node<'_>) {
        match node.kind() {
            "attribute" => {
                if let Some(object) = node.child_by_field_name("object") {
                    if let Some(attribute) = node
                        .child_by_field_name("attribute")
                        .filter(|attribute| self.text(*attribute) != "__dict__")
                    {
                        self.push_member_mention(attribute, object, MentionForm::Subscript);
                    }
                    self.visit_receiver(object, MentionForm::Receiver);
                }
            }
            _ => self.visit_receiver(node, MentionForm::Subscript),
        }
    }

    fn visit_subscript(&mut self, node: Node<'_>) {
        if let Some(value) = node.child_by_field_name("value") {
            self.visit_receiver(value, MentionForm::Subscript);
        }
        let mut cursor = node.walk();
        let indexes: Vec<Node<'_>> = node
            .children_by_field_name("subscript", &mut cursor)
            .collect();
        for index in indexes {
            self.visit(index);
        }
    }

    fn visit_from_import(&mut self, node: Node<'_>) {
        let range = (node.start_byte() as u32, node.end_byte() as u32);
        let module = node
            .child_by_field_name("module_name")
            .map(|name| self.text(name).to_string());
        let mut cursor = node.walk();
        let names: Vec<Node<'_>> = node.children_by_field_name("name", &mut cursor).collect();
        for child in names {
            if child.kind() == "dotted_name" {
                // `from m import f` binds `f` under its own name: a mention.
                self.push_import_mention(child, module.clone(), range);
                continue;
            }
            if child.kind() != "aliased_import" {
                continue;
            }
            let (Some(name), Some(alias)) = (
                child.child_by_field_name("name"),
                child.child_by_field_name("alias"),
            ) else {
                continue;
            };
            let imported = self.text(name).to_string();
            let local = self.text(alias).to_string();
            if imported == local {
                self.push_import_mention(name, module.clone(), range);
                continue;
            }
            self.push_value(
                imported,
                ValueOccurrence {
                    line: line_of(name),
                    context: OccurrenceContext::Import,
                    binding: OccurrenceBinding::Import,
                    receiver: None,
                    receiver_kind: None,
                    aliases: vec![local],
                    module: module.clone(),
                    import: Some(range),
                    imported: None,
                    target_line: None,
                    form: None,
                },
            );
        }
    }

    // ── Reads ─────────────────────────────────────────────────────────────

    fn read_name(&mut self, node: Node<'_>) {
        let name = self.text(node);
        if name.is_empty() {
            return;
        }
        let Some(flow) = self.read_flow(node) else {
            return;
        };
        let Some((binding, binds)) = self.resolve(name) else {
            return;
        };
        let (context, aliases, form) = match flow {
            Flow::Value(context, aliases) => (context, aliases, None),
            Flow::Inert(form) => (OccurrenceContext::Mention, Vec::new(), Some(form)),
            Flow::Invoke => {
                self.push_protocol(node, ProtocolForm::Invoke, Some(name.to_string()));
                return;
            }
        };
        let occurrence = name_occurrence(node, name, binding, binds, context, aliases, form);
        self.push_value(name.to_string(), occurrence);
    }

    /// Record a mention of the name `node` spells, read in the current scope:
    /// nothing for a plain local, parameter or comprehension target.
    fn push_name_mention(&mut self, node: Node<'_>, form: MentionForm) {
        let name = self.text(node);
        if name.is_empty() {
            return;
        }
        let Some((binding, binds)) = self.resolve(name) else {
            return;
        };
        let occurrence = name_occurrence(
            node,
            name,
            binding,
            binds,
            OccurrenceContext::Mention,
            Vec::new(),
            Some(form),
        );
        self.push_value(name.to_string(), occurrence);
    }

    /// Record a mention of the member `attribute` names on `object`.
    fn push_member_mention(&mut self, attribute: Node<'_>, object: Node<'_>, form: MentionForm) {
        let name = self.text(attribute).to_string();
        if name.is_empty() {
            return;
        }
        let receiver_kind = self.receiver_kind(object);
        self.push_value(
            name,
            ValueOccurrence {
                line: line_of(attribute),
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

    /// Record `from m import name` binding `name` under its own name.
    fn push_import_mention(&mut self, name: Node<'_>, module: Option<String>, range: (u32, u32)) {
        let text = self.text(name).to_string();
        if text.is_empty() {
            return;
        }
        self.push_value(
            text,
            ValueOccurrence {
                line: line_of(name),
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

    /// Visit an expression read where nothing it holds flows on (an
    /// annotation, an exception type, a decorator, a base): every name and
    /// member it reads is a mention of `form`, and a call or a nested scope
    /// inside it is visited as usual. In an annotation, a string that holds
    /// names is a forward reference and each name in it is mentioned too.
    fn visit_inert(&mut self, node: Node<'_>, form: MentionForm) {
        match node.kind() {
            "identifier" => self.push_name_mention(node, form),
            "attribute" => {
                let (Some(object), Some(attribute)) = (
                    node.child_by_field_name("object"),
                    node.child_by_field_name("attribute"),
                ) else {
                    return;
                };
                self.push_member_mention(attribute, object, form);
                match object.kind() {
                    "identifier" | "attribute" => {
                        self.visit_receiver(object, MentionForm::Receiver)
                    }
                    _ => self.visit_inert(object, form),
                }
            }
            "string" | "concatenated_string" => {
                if form == MentionForm::Annotation && !self.inside_literal_type(node) {
                    self.mention_forward_reference(node);
                } else {
                    self.visit_interpolations(node);
                }
            }
            "call"
            | "lambda"
            | "list_comprehension"
            | "set_comprehension"
            | "dictionary_comprehension"
            | "generator_expression"
            | "named_expression"
            | "conditional_expression" => self.visit(node),
            "comment" | "integer" | "float" | "true" | "false" | "none" | "ellipsis" => {}
            "keyword_argument" => {
                if let Some(value) = node.child_by_field_name("value") {
                    self.visit_inert(value, form);
                }
            }
            _ => {
                let mut cursor = node.walk();
                let children: Vec<Node<'_>> = node.named_children(&mut cursor).collect();
                for child in children {
                    self.visit_inert(child, form);
                }
            }
        }
    }

    /// Whether `node` sits in the arguments of a `Literal[...]` annotation,
    /// where a string is a value rather than a forward reference.
    fn inside_literal_type(&self, node: Node<'_>) -> bool {
        let mut current = node;
        while let Some(parent) = current.parent() {
            let head = match parent.kind() {
                "subscript" => parent.child_by_field_name("value"),
                "generic_type" => parent.named_child(0),
                "type"
                | "type_parameter"
                | "tuple"
                | "expression_list"
                | "union_type"
                | "binary_operator"
                | "parenthesized_expression" => {
                    current = parent;
                    continue;
                }
                _ => return false,
            };
            if head.is_some_and(|head| {
                head.id() != current.id() && {
                    let text = self.text(head);
                    text == "Literal" || text.ends_with(".Literal")
                }
            }) {
                return true;
            }
            current = parent;
        }
        false
    }

    /// Mention every name a quoted annotation spells: the first name of each
    /// dotted chain as read in this scope, and each later one as a member of
    /// what comes before it. Nothing in the string is evaluated here, so
    /// nothing in it flows on.
    fn mention_forward_reference(&mut self, node: Node<'_>) {
        let mut stack = vec![node];
        while let Some(current) = stack.pop() {
            if current.kind() == "string_content" {
                let text = self.text(current);
                let line = line_of(current);
                for chain in dotted_chains(text) {
                    self.push_chain_mention(&chain, line, MentionForm::Annotation);
                }
                continue;
            }
            if current.kind() == "interpolation" {
                self.visit_children(current);
                continue;
            }
            let mut cursor = current.walk();
            stack.extend(current.named_children(&mut cursor));
        }
    }

    /// Mention a dotted chain read on `line`: `a.b.c` mentions `a` as read in
    /// this scope, `b` as a member of `a` and `c` as a member of `a.b`.
    fn push_chain_mention(&mut self, chain: &[&str], line: u32, form: MentionForm) {
        let Some((head, members)) = chain.split_first() else {
            return;
        };
        if let Some((binding, binds)) = self.resolve(head) {
            let mut occurrence = ValueOccurrence {
                line,
                context: OccurrenceContext::Mention,
                binding,
                receiver: None,
                receiver_kind: None,
                aliases: Vec::new(),
                module: None,
                import: None,
                imported: None,
                target_line: None,
                form: Some(form),
            };
            fill_from_binds(&mut occurrence, head, binds);
            self.push_value((*head).to_string(), occurrence);
        }
        let head_kind = self.receiver_kind_of_name(head);
        for (index, member) in members.iter().enumerate() {
            let receiver = chain[..=index].join(".");
            self.push_value(
                (*member).to_string(),
                ValueOccurrence {
                    line,
                    context: OccurrenceContext::Mention,
                    binding: OccurrenceBinding::Member,
                    receiver_kind: Some(if index == 0 {
                        head_kind
                    } else {
                        ReceiverKind::Other
                    }),
                    receiver: Some(receiver.chars().take(RECEIVER_TEXT_MAX).collect()),
                    aliases: Vec::new(),
                    module: None,
                    import: None,
                    imported: None,
                    target_line: None,
                    form: Some(form),
                },
            );
        }
    }

    /// The names in a parameter list's annotations, as mentions.
    fn visit_parameter_annotations(&mut self, params: Node<'_>) {
        let mut cursor = params.walk();
        let children: Vec<Node<'_>> = params.named_children(&mut cursor).collect();
        for param in children {
            if let Some(annotation) = param.child_by_field_name("type") {
                self.visit_inert(annotation, MentionForm::Annotation);
            }
        }
    }

    /// A type parameter list (`[T: Bound, *Ts]`): each parameter's name is
    /// declared, and every bound or constraint it names is read.
    fn visit_type_parameters(&mut self, node: Node<'_>) {
        let mut cursor = node.walk();
        let children: Vec<Node<'_>> = node.named_children(&mut cursor).collect();
        for child in children {
            let declared = if child.kind() == "type" {
                child.named_child(0)
            } else {
                Some(child)
            };
            let Some(declared) = declared else { continue };
            if declared.kind() == "constrained_type" {
                // `T: Bound`: the first part names the parameter.
                let mut inner = declared.walk();
                let parts: Vec<Node<'_>> = declared.named_children(&mut inner).collect();
                for part in parts.into_iter().skip(1) {
                    self.visit_inert(part, MentionForm::Annotation);
                }
            }
        }
    }

    /// A match pattern: a class pattern names its class, and a dotted value
    /// pattern reads a member; a bare name captures, a keyword names an
    /// attribute, and neither is a read.
    fn visit_case_pattern(&mut self, node: Node<'_>) {
        let mut stack = vec![node];
        while let Some(current) = stack.pop() {
            let mut cursor = current.walk();
            let children: Vec<Node<'_>> = current.named_children(&mut cursor).collect();
            match current.kind() {
                "class_pattern" => {
                    for (index, child) in children.into_iter().enumerate() {
                        if index == 0 && child.kind() == "dotted_name" {
                            let text = self.text(child);
                            let chain: Vec<&str> = text.split('.').map(str::trim).collect();
                            self.push_chain_mention(&chain, line_of(child), MentionForm::Pattern);
                        } else {
                            stack.push(child);
                        }
                    }
                }
                "dotted_name" => {
                    let text = self.text(current);
                    if text.contains('.') {
                        let chain: Vec<&str> = text.split('.').map(str::trim).collect();
                        self.push_chain_mention(&chain, line_of(current), MentionForm::Pattern);
                    }
                }
                "keyword_pattern" => {
                    stack.extend(children.into_iter().skip(1));
                }
                _ => stack.extend(children),
            }
        }
    }

    fn visit_attribute(&mut self, node: Node<'_>) {
        let (Some(object), Some(attribute)) = (
            node.child_by_field_name("object"),
            node.child_by_field_name("attribute"),
        ) else {
            self.visit_children(node);
            return;
        };
        let name = self.text(attribute).to_string();
        if name == "__dict__" {
            self.mark_dict(node, object);
        } else if let Some(flow) = self.read_flow(node) {
            match flow {
                Flow::Value(context, aliases) => {
                    let receiver_kind = self.receiver_kind(object);
                    self.push_value(
                        name,
                        ValueOccurrence {
                            line: line_of(attribute),
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
                Flow::Inert(form) => self.push_member_mention(attribute, object, form),
                Flow::Invoke => self.push_protocol(attribute, ProtocolForm::Invoke, Some(name)),
            }
        }
        self.visit_receiver(object, MentionForm::Receiver);
    }

    fn visit_call(&mut self, node: Node<'_>) {
        let function = node.child_by_field_name("function");
        let arguments = node.child_by_field_name("arguments");
        if let Some(function) = function {
            let callee = self.callee_name(function);
            if let Some(callee) = callee.as_deref() {
                self.mark_reflective_call(node, callee, arguments);
            }
            match function.kind() {
                "identifier" => {}
                "attribute" => {
                    let member = function
                        .child_by_field_name("attribute")
                        .map(|attribute| self.text(attribute))
                        .unwrap_or("");
                    if let Some(object) = function.child_by_field_name("object") {
                        if member == "__call__" || FUNCTION_EXPOSING.contains(&member) {
                            // The object is invoked, or handed out as a value.
                            self.visit(object);
                        } else {
                            self.visit_receiver(object, MentionForm::Receiver);
                        }
                    }
                }
                _ => self.visit(function),
            }
        }
        if let Some(arguments) = arguments {
            self.visit(arguments);
        }
    }

    /// The dotted text of a callee made of names and attributes.
    fn callee_name(&self, function: Node<'_>) -> Option<String> {
        match function.kind() {
            "identifier" => Some(self.text(function).to_string()),
            "attribute" => {
                let object = function.child_by_field_name("object")?;
                let attribute = function.child_by_field_name("attribute")?;
                Some(format!(
                    "{}.{}",
                    self.callee_name(object)?,
                    self.text(attribute)
                ))
            }
            _ => None,
        }
    }

    fn receiver_kind(&self, object: Node<'_>) -> ReceiverKind {
        match object.kind() {
            "identifier" => self.receiver_kind_of_name(self.text(object)),
            "call" => {
                let is_super = object
                    .child_by_field_name("function")
                    .is_some_and(|function| self.text(function) == "super");
                if is_super {
                    ReceiverKind::Instance
                } else {
                    ReceiverKind::Other
                }
            }
            _ => ReceiverKind::Other,
        }
    }

    /// What a receiver spelled by a bare `name` is.
    fn receiver_kind_of_name(&self, name: &str) -> ReceiverKind {
        if matches!(name, "self" | "cls" | "mcs" | "metacls") {
            return ReceiverKind::Instance;
        }
        match self.resolve(name) {
            Some((OccurrenceBinding::Import | OccurrenceBinding::Module, Some(binds)))
                if !binds.imports.is_empty() && binds.definitions.is_empty() =>
            {
                ReceiverKind::Namespace
            }
            _ => ReceiverKind::Other,
        }
    }

    /// Where the value `node` reads goes, walking up through the syntax that
    /// passes a value through unchanged, or `None` when it goes nowhere a
    /// later call could reach it through. A read whose value goes nowhere is
    /// left out here; [`Self::read_flow`] names it.
    fn context_of(&self, node: Node<'_>) -> Option<Flow> {
        self.read_flow(node)
            .filter(|flow| !matches!(flow, Flow::Inert(_)))
    }

    /// [`Self::context_of`], with a read whose value goes nowhere a later
    /// call could reach it through named as [`Flow::Inert`] by where it is
    /// read. `None` means `node` is no read at all: a call's callee, a
    /// target, a keyword's name.
    fn read_flow(&self, node: Node<'_>) -> Option<Flow> {
        let mut child = node;
        let mut wrapped = false;
        loop {
            let parent = child.parent()?;
            match parent.kind() {
                "parenthesized_expression" | "await" => {}
                "boolean_operator" => {}
                "conditional_expression" => {
                    // `a if cond else b`: the condition is inert.
                    let mut cursor = parent.walk();
                    let named: Vec<Node<'_>> = parent.named_children(&mut cursor).collect();
                    if named
                        .get(1)
                        .is_some_and(|condition| condition.id() == child.id())
                    {
                        return Some(Flow::Inert(MentionForm::Condition));
                    }
                }
                "argument_list" => {
                    let owner = parent.parent()?;
                    return match owner.kind() {
                        "call" => {
                            if self.inert_callee(owner) {
                                Some(Flow::Inert(MentionForm::Argument))
                            } else {
                                Some(Flow::value(OccurrenceContext::Argument))
                            }
                        }
                        // Bases of a class are inherited from.
                        "class_definition" => Some(Flow::Inert(MentionForm::Base)),
                        _ => Some(Flow::value(OccurrenceContext::Argument)),
                    };
                }
                "keyword_argument" => {
                    if !is_field(parent, "value", child) {
                        return None;
                    }
                    let call = parent.parent().and_then(|list| list.parent());
                    return match call {
                        Some(call) if call.kind() == "call" && self.inert_callee(call) => {
                            Some(Flow::Inert(MentionForm::Argument))
                        }
                        Some(owner) if owner.kind() == "class_definition" => {
                            Some(Flow::value(OccurrenceContext::Argument))
                        }
                        _ => Some(Flow::value(OccurrenceContext::Argument)),
                    };
                }
                "list" | "set" | "tuple" | "dictionary" | "pair" | "expression_list" => {
                    return Some(Flow::value(OccurrenceContext::Collection));
                }
                "list_splat" | "dictionary_splat" | "parenthesized_list_splat" => {
                    return Some(Flow::Inert(MentionForm::Other))
                }
                "return_statement" | "yield" => {
                    return Some(Flow::value(OccurrenceContext::Return))
                }
                "lambda" => {
                    return is_field(parent, "body", child)
                        .then(|| Flow::value(OccurrenceContext::Return));
                }
                "assignment" => {
                    if !is_field(parent, "right", child) {
                        return None;
                    }
                    return Some(self.assignment_flow(parent));
                }
                "augmented_assignment" => {
                    return is_field(parent, "right", child)
                        .then(|| Flow::value(OccurrenceContext::Other));
                }
                "named_expression" => {
                    if !is_field(parent, "value", child) {
                        return None;
                    }
                    return Some(match self.current_kind() {
                        ScopeKind::Module | ScopeKind::Class => Flow::Value(
                            OccurrenceContext::Alias,
                            parent
                                .child_by_field_name("name")
                                .map(|name| vec![self.text(name).to_string()])
                                .unwrap_or_default(),
                        ),
                        _ => Flow::value(OccurrenceContext::Assignment),
                    });
                }
                "default_parameter" | "typed_default_parameter" => {
                    return is_field(parent, "value", child)
                        .then(|| Flow::value(OccurrenceContext::Assignment));
                }
                "call" => {
                    // A callee is a call, never a value use; one reached
                    // through a wrapper, `(a or b)()`, is invoked unspelled.
                    return (wrapped && is_field(parent, "function", child))
                        .then_some(Flow::Invoke);
                }
                "attribute" => {
                    if !is_field(parent, "object", child) {
                        return None;
                    }
                    let member = parent
                        .child_by_field_name("attribute")
                        .map(|name| self.text(name))
                        .unwrap_or("");
                    if member == "__call__" {
                        let called = parent.parent().is_some_and(|call| {
                            call.kind() == "call" && is_field(call, "function", parent)
                        });
                        return Some(if called {
                            Flow::Invoke
                        } else {
                            Flow::Inert(MentionForm::Receiver)
                        });
                    }
                    if FUNCTION_EXPOSING.contains(&member) {
                        // `f.__get__(obj)` hands out a bound copy: follow the
                        // call's value.
                        let call = parent.parent().filter(|call| {
                            call.kind() == "call" && is_field(*call, "function", parent)
                        });
                        child = match (member, call) {
                            ("__get__", Some(call)) => call,
                            _ => parent,
                        };
                        wrapped = false;
                        continue;
                    }
                    return Some(Flow::Inert(MentionForm::Receiver));
                }
                "list_comprehension"
                | "set_comprehension"
                | "generator_expression"
                | "dictionary_comprehension" => {
                    return Some(Flow::value(OccurrenceContext::Collection));
                }
                "global_statement" | "nonlocal_statement" => return None,
                "subscript" | "slice" => return Some(Flow::Inert(MentionForm::Subscript)),
                "if_clause" | "if_statement" | "elif_clause" | "while_statement"
                | "assert_statement" | "not_operator" | "case_clause" => {
                    return Some(Flow::Inert(MentionForm::Condition))
                }
                "comparison_operator" => return Some(Flow::Inert(MentionForm::Comparison)),
                "except_clause" => return Some(Flow::Inert(MentionForm::Except)),
                "case_pattern" => return Some(Flow::Inert(MentionForm::Pattern)),
                "type" => return Some(Flow::Inert(MentionForm::Annotation)),
                "decorator" => return Some(Flow::Inert(MentionForm::Decorator)),
                "for_statement"
                | "for_in_clause"
                | "with_item"
                | "with_clause"
                | "binary_operator"
                | "unary_operator"
                | "interpolation"
                | "format_expression"
                | "raise_statement"
                | "delete_statement"
                | "match_statement"
                | "expression_statement"
                | "print_statement"
                | "block"
                | "module"
                | "string"
                | "concatenated_string"
                | "as_pattern"
                | "exec_statement" => return Some(Flow::Inert(MentionForm::Other)),
                _ => return Some(Flow::value(OccurrenceContext::Other)),
            }
            child = parent;
            wrapped = true;
        }
    }

    /// Where the right side of an assignment goes, by its targets: a name at
    /// module or class scope aliases the value, a name in a function or a
    /// destructuring binds it where no record follows, an attribute stores it
    /// under a name, and a subscript stores it in a container.
    fn assignment_flow(&self, assignment: Node<'_>) -> Flow {
        let mut targets = Vec::new();
        let mut current = Some(assignment);
        // `a = b = value` nests: every left side up the chain is a target.
        while let Some(node) = current {
            if node.kind() != "assignment" {
                break;
            }
            if let Some(left) = node.child_by_field_name("left") {
                targets.push(left);
            }
            current = node
                .parent()
                .filter(|parent| parent.kind() == "assignment" && is_field(*parent, "right", node));
        }
        let mut aliases = Vec::new();
        let mut escapes = None;
        for target in targets {
            match target.kind() {
                "identifier" => match self.current_kind() {
                    ScopeKind::Module | ScopeKind::Class => {
                        aliases.push(self.text(target).to_string())
                    }
                    _ => escapes = Some(OccurrenceContext::Assignment),
                },
                "attribute" => {
                    if let Some(attribute) = target.child_by_field_name("attribute") {
                        aliases.push(self.text(attribute).to_string());
                    }
                }
                "subscript" => escapes = Some(OccurrenceContext::Collection),
                _ => escapes = Some(OccurrenceContext::Assignment),
            }
        }
        match escapes {
            Some(context) => Flow::Value(context, aliases),
            None => Flow::Value(OccurrenceContext::Alias, aliases),
        }
    }

    /// Whether a call's callee is a builtin that only inspects its arguments.
    fn inert_callee(&self, call: Node<'_>) -> bool {
        call.child_by_field_name("function")
            .is_some_and(|function| {
                function.kind() == "identifier" && {
                    let name = self.text(function);
                    INERT_ARGUMENT_BUILTINS.contains(&name)
                        && matches!(self.resolve(name), Some((OccurrenceBinding::Free, _)))
                }
            })
    }

    // ── Reflective and protocol sites ─────────────────────────────────────

    fn mark_reflective_call(&mut self, call: Node<'_>, callee: &str, arguments: Option<Node<'_>>) {
        let leaf = callee.rsplit('.').next().unwrap_or(callee);
        let free = !callee.contains('.')
            && matches!(self.resolve(callee), Some((OccurrenceBinding::Free, _)));
        let positional: Vec<Node<'_>> = arguments
            .map(|arguments| {
                let mut cursor = arguments.walk();
                let nodes: Vec<Node<'_>> = arguments
                    .named_children(&mut cursor)
                    .filter(|argument| argument.kind() != "keyword_argument")
                    .collect();
                nodes
            })
            .unwrap_or_default();
        let called = call
            .parent()
            .is_some_and(|parent| parent.kind() == "call" && is_field(parent, "function", call));
        let flows = self.context_of(call).is_some() || called;
        match (free, leaf) {
            (true, "getattr") => {
                let (Some(object), Some(key)) = (positional.first(), positional.get(1)) else {
                    return;
                };
                if let Some(name) = plain_string_literal(*key, self.source) {
                    // A literal name reads a member exactly as `object.name`.
                    if called {
                        return;
                    }
                    if let Some(Flow::Value(context, aliases)) = self.context_of(call) {
                        let receiver_kind = self.receiver_kind(*object);
                        self.push_value(
                            name,
                            ValueOccurrence {
                                line: line_of(*key),
                                context,
                                binding: OccurrenceBinding::Member,
                                receiver: Some(collapsed_text(
                                    *object,
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
                    return;
                }
                if !flows {
                    return;
                }
                let (reach, receiver) = self.lookup_reach(*object);
                let key = self.key_literals(*key);
                self.push_lookup(
                    call,
                    ReflectiveForm::ComputedMember,
                    reach,
                    receiver,
                    key,
                    called,
                );
            }
            (_, "attrgetter" | "methodcaller") if callee.starts_with("operator.") || free => {
                let literal = positional
                    .first()
                    .and_then(|key| plain_string_literal(*key, self.source));
                if literal.is_none() {
                    self.push_reflective(
                        call,
                        ReflectiveForm::ComputedMember,
                        ReflectiveReach::Any,
                        None,
                        None,
                        false,
                    );
                }
            }
            (true, "vars") => {
                if positional.is_empty() {
                    if self.current_kind() == ScopeKind::Module {
                        self.push_reflective(
                            call,
                            ReflectiveForm::ScopeTable,
                            ReflectiveReach::OwnModule,
                            None,
                            None,
                            called,
                        );
                    }
                } else if flows || self.subscripted_or_enumerated(call) {
                    // `vars(obj)[k]` and `vars(obj).values()` read through the
                    // table as much as passing it on does.
                    if self.receiver_kind(positional[0]) == ReceiverKind::Instance
                        && self.text(positional[0]) == "self"
                    {
                        // An instance's own dict holds its attributes, never
                        // a method or a module-level declaration.
                        return;
                    }
                    let (reach, receiver) = self.lookup_reach(positional[0]);
                    self.push_lookup(
                        call,
                        ReflectiveForm::ComputedMember,
                        reach,
                        receiver,
                        KeyLiterals::default(),
                        called,
                    );
                }
            }
            (true, "globals") => {
                self.push_reflective(
                    call,
                    ReflectiveForm::ScopeTable,
                    ReflectiveReach::OwnModule,
                    None,
                    None,
                    called,
                );
            }
            (true, "locals") => {
                if self.current_kind() == ScopeKind::Module {
                    self.push_reflective(
                        call,
                        ReflectiveForm::ScopeTable,
                        ReflectiveReach::OwnModule,
                        None,
                        None,
                        called,
                    );
                }
            }
            (true, "eval" | "exec") => {
                self.push_reflective(
                    call,
                    ReflectiveForm::Evaluate,
                    ReflectiveReach::Any,
                    None,
                    None,
                    false,
                );
            }
            (true, "compile") => {
                // `compile` builds a code object and runs nothing: the `eval`
                // or `exec` that runs it is its own site.
                self.push_reflective(
                    call,
                    ReflectiveForm::Evaluate,
                    ReflectiveReach::Nothing,
                    None,
                    None,
                    false,
                );
            }
            (true, "__import__") | (_, "import_module") => {
                let literal = positional
                    .first()
                    .and_then(|key| plain_string_literal(*key, self.source));
                if literal.is_none() {
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
            (_, "getmembers") if callee == "inspect.getmembers" || callee == "getmembers" => {
                let (reach, receiver) = match positional.first() {
                    Some(object) => self.lookup_reach(*object),
                    None => (ReflectiveReach::Any, None),
                };
                self.push_lookup(
                    call,
                    ReflectiveForm::Enumerate,
                    reach,
                    receiver,
                    KeyLiterals::default(),
                    false,
                );
            }
            _ => {}
        }
    }

    /// `obj.__dict__` read as a value: the attributes of `obj`.
    fn mark_dict(&mut self, node: Node<'_>, object: Node<'_>) {
        if self.context_of(node).is_none() && !self.subscripted_or_enumerated(node) {
            return;
        }
        if self.receiver_kind(object) == ReceiverKind::Instance && self.text(object) == "self" {
            return;
        }
        let (reach, receiver) = self.lookup_reach(object);
        self.push_lookup(
            node,
            ReflectiveForm::ComputedMember,
            reach,
            receiver,
            KeyLiterals::default(),
            false,
        );
    }

    /// Whether `node` is read through: subscripted, or has a member called on
    /// it (`d.__dict__[k]`, `d.__dict__.values()`).
    fn subscripted_or_enumerated(&self, node: Node<'_>) -> bool {
        node.parent().is_some_and(|parent| {
            (parent.kind() == "subscript" && is_field(parent, "value", node))
                || (parent.kind() == "attribute" && is_field(parent, "object", node))
        })
    }

    /// What a lookup's receiver is and so what it can read: members of the
    /// enclosing instance, a class or function this file declares, a whole
    /// module object, or anything.
    fn lookup_reach(&self, object: Node<'_>) -> (ReflectiveReach, Option<LookupReceiver>) {
        if self.receiver_kind(object) == ReceiverKind::Instance {
            return (ReflectiveReach::Members, None);
        }
        if object.kind() != "identifier" {
            return (ReflectiveReach::Any, None);
        }
        let name = self.text(object);
        let receiver = match self.resolve(name) {
            // The one binding is a `def` or `class` statement.
            Some((_, Some(binds)))
                if binds.definitions.len() == 1 && !binds.other && binds.imports.is_empty() =>
            {
                Some(LookupReceiver::Declared {
                    name: name.to_string(),
                    line: binds.definitions[0],
                })
            }
            // Every binding imports a whole module.
            Some((OccurrenceBinding::Import, Some(binds)))
                if binds.module_imports > 0
                    && binds.module_imports == binds.imports.len()
                    && binds.definitions.is_empty()
                    && !binds.other =>
            {
                Some(LookupReceiver::Module)
            }
            _ => None,
        };
        (ReflectiveReach::Any, receiver)
    }

    /// The literals a computed key provably holds: the pieces of an f-string,
    /// of a `+` chain of strings, of `"..." % x` and of `"...".format(x)`.
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
        match key.kind() {
            "string" => {
                let mut cursor = key.walk();
                let parts: Vec<Node<'_>> = key.named_children(&mut cursor).collect();
                let prefix = parts
                    .first()
                    .filter(|part| part.kind() == "string_start")
                    .map(|part| self.text(*part).to_ascii_lowercase())
                    .unwrap_or_default();
                // A bytes literal is no attribute name, and escapes are
                // not read.
                if prefix.contains('b') {
                    return false;
                }
                for part in parts {
                    match part.kind() {
                        "string_start" | "string_end" => {}
                        "string_content" => {
                            let text = self.text(part);
                            if text.contains('\\') {
                                pieces.push(None);
                            } else {
                                pieces.push(Some(text.to_string()));
                            }
                        }
                        _ => pieces.push(None),
                    }
                }
                true
            }
            "concatenated_string" => {
                let mut cursor = key.walk();
                let parts: Vec<Node<'_>> = key.named_children(&mut cursor).collect();
                parts.into_iter().all(|part| self.key_pieces(part, pieces))
            }
            "parenthesized_expression" => key
                .named_child(0)
                .is_some_and(|inner| self.key_pieces(inner, pieces)),
            "binary_operator" => {
                let operator = key
                    .child_by_field_name("operator")
                    .map(|operator| self.text(operator));
                let (Some(left), Some(right)) = (
                    key.child_by_field_name("left"),
                    key.child_by_field_name("right"),
                ) else {
                    return false;
                };
                match operator {
                    // `"a" + x + "b"`: a string whenever either side is one.
                    Some("+") => {
                        let mut left_pieces = Vec::new();
                        let mut right_pieces = Vec::new();
                        let left_string = self.key_pieces(left, &mut left_pieces);
                        let right_string = self.key_pieces(right, &mut right_pieces);
                        if !left_string && !right_string {
                            return false;
                        }
                        pieces.extend(if left_string { left_pieces } else { vec![None] });
                        pieces.extend(if right_string {
                            right_pieces
                        } else {
                            vec![None]
                        });
                        true
                    }
                    // `"visit_%s" % kind`: the template with each conversion
                    // computed.
                    Some("%") => {
                        let Some(template) = plain_string_literal(left, self.source) else {
                            return false;
                        };
                        push_template(&template, '%', pieces);
                        true
                    }
                    _ => false,
                }
            }
            "call" => {
                // `"visit_{}".format(kind)`.
                let Some(function) = key.child_by_field_name("function") else {
                    return false;
                };
                if function.kind() != "attribute"
                    || function
                        .child_by_field_name("attribute")
                        .is_none_or(|attribute| self.text(attribute) != "format")
                {
                    return false;
                }
                let Some(template) = function
                    .child_by_field_name("object")
                    .and_then(|object| plain_string_literal(object, self.source))
                else {
                    return false;
                };
                push_template(&template, '{', pieces);
                true
            }
            _ => false,
        }
    }

    fn mark_with_items(&mut self, statement: Node<'_>, asynchronous: bool) {
        let form = if asynchronous {
            ProtocolForm::AsyncWith
        } else {
            ProtocolForm::With
        };
        let mut stack = vec![statement];
        while let Some(node) = stack.pop() {
            if node.kind() == "with_item" {
                self.push_protocol(node, form, None);
                continue;
            }
            if node.kind() == "block" {
                continue;
            }
            let mut cursor = node.walk();
            stack.extend(node.named_children(&mut cursor));
        }
    }

    fn mark_raise(&mut self, statement: Node<'_>) {
        let Some(raised) = statement.named_child(0) else {
            return;
        };
        let name = match raised.kind() {
            "identifier" => {
                let name = self.text(raised);
                // A caught exception held in a local is re-raised, not
                // instantiated.
                if self.resolve(name).is_none() {
                    return;
                }
                name.to_string()
            }
            "attribute" => raised
                .child_by_field_name("attribute")
                .map(|attribute| self.text(attribute).to_string())
                .unwrap_or_default(),
            _ => return,
        };
        if !name.is_empty() {
            self.push_protocol(raised, ProtocolForm::Raise, Some(name));
        }
    }

    fn mark_protocol_member(&mut self, function: Node<'_>) {
        let Some(name_node) = function.child_by_field_name("name") else {
            return;
        };
        let name = self.text(name_node).to_string();
        let decorators: Vec<String> = function
            .parent()
            .filter(|parent| parent.kind() == "decorated_definition")
            .map(|parent| {
                let mut cursor = parent.walk();
                let names: Vec<String> = parent
                    .named_children(&mut cursor)
                    .filter(|child| child.kind() == "decorator")
                    .map(|decorator| {
                        self.text(decorator)
                            .trim_start_matches('@')
                            .trim()
                            .to_string()
                    })
                    .collect();
                names
            })
            .unwrap_or_default();
        let member = if decorators.iter().any(|decorator| {
            let leaf = decorator.rsplit('.').next().unwrap_or(decorator);
            matches!(
                leaf,
                "property" | "cached_property" | "setter" | "getter" | "deleter"
            )
        }) {
            Some(ProtocolMember::Implicit("attribute access".into()))
        } else {
            dunder_protocol(&name)
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

    fn push_value(&mut self, name: String, occurrence: ValueOccurrence) {
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
    fn push_lookup(
        &mut self,
        node: Node<'_>,
        form: ReflectiveForm,
        reach: ReflectiveReach,
        receiver: Option<LookupReceiver>,
        key: KeyLiterals,
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
            enumerable_only: false,
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

/// Where a read goes.
enum Flow {
    Value(OccurrenceContext, Vec<String>),
    /// Invoked through `__call__`.
    Invoke,
    /// Read where its value goes nowhere a later call could reach it
    /// through: a mention of this form.
    Inert(MentionForm),
}

/// A name read occurrence of `context`, with what the scope's bindings say
/// about it.
fn name_occurrence(
    node: Node<'_>,
    name: &str,
    binding: OccurrenceBinding,
    binds: Option<&Binds>,
    context: OccurrenceContext,
    aliases: Vec<String>,
    form: Option<MentionForm>,
) -> ValueOccurrence {
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
        form,
    };
    fill_from_binds(&mut occurrence, name, binds);
    occurrence
}

/// Fill what a read of `name` binds to: the one declaration a module, class
/// or local binding names, and the one import statement that binds it.
fn fill_from_binds(occurrence: &mut ValueOccurrence, name: &str, binds: Option<&Binds>) {
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

/// Whether `name` is one identifier: letters, digits and underscores, not
/// starting with a digit.
fn is_plain_identifier(name: &str) -> bool {
    let mut characters = name.chars();
    characters
        .next()
        .is_some_and(|first| first == '_' || first.is_alphabetic())
        && characters.all(|character| character == '_' || character.is_alphanumeric())
}

/// Every dotted chain of Python identifiers `text` spells (`t.Optional`,
/// `Flask`), each as its names in order.
fn dotted_chains(text: &str) -> Vec<Vec<&str>> {
    let is_start = |c: char| c == '_' || c.is_alphabetic();
    let is_part = |c: char| c == '_' || c.is_alphanumeric();
    let mut chains = Vec::new();
    let mut chain: Vec<&str> = Vec::new();
    let mut index = 0;
    let bytes = text.char_indices().collect::<Vec<_>>();
    let mut previous: Option<char> = None;
    while index < bytes.len() {
        let (at, c) = bytes[index];
        if is_start(c) && !previous.is_some_and(is_part) {
            let mut end = index;
            while end < bytes.len() && is_part(bytes[end].1) {
                end += 1;
            }
            let stop = bytes.get(end).map_or(text.len(), |(at, _)| *at);
            chain.push(&text[at..stop]);
            previous = Some(bytes[end - 1].1);
            index = end;
            if bytes.get(index).is_some_and(|(_, c)| *c == '.')
                && bytes.get(index + 1).is_some_and(|(_, c)| is_start(*c))
            {
                previous = Some('.');
                index += 1;
                continue;
            }
            chains.push(std::mem::take(&mut chain));
            continue;
        }
        previous = Some(c);
        index += 1;
    }
    if !chain.is_empty() {
        chains.push(chain);
    }
    chains
}

impl Flow {
    fn value(context: OccurrenceContext) -> Self {
        Self::Value(context, Vec::new())
    }
}

/// How Python reaches a dunder member without a call spelling it. A
/// constructor is reached by spelling its class, which its call names cover.
fn dunder_protocol(name: &str) -> Option<ProtocolMember> {
    if !(name.len() > 4 && name.starts_with("__") && name.ends_with("__")) {
        return None;
    }
    Some(match name {
        "__init__" | "__new__" => return None,
        "__enter__" | "__exit__" => ProtocolMember::Listed(vec![ProtocolForm::With]),
        "__aenter__" | "__aexit__" => ProtocolMember::Listed(vec![ProtocolForm::AsyncWith]),
        "__call__" => ProtocolMember::Implicit("a call of an instance".into()),
        "__iter__" | "__next__" | "__aiter__" | "__anext__" | "__reversed__" => {
            ProtocolMember::Implicit("iteration".into())
        }
        "__getattr__" | "__getattribute__" | "__setattr__" | "__delattr__" | "__get__"
        | "__set__" | "__delete__" | "__set_name__" => {
            ProtocolMember::Implicit("attribute access".into())
        }
        "__getitem__" | "__setitem__" | "__delitem__" | "__missing__" | "__class_getitem__" => {
            ProtocolMember::Implicit("subscription".into())
        }
        "__await__" => ProtocolMember::Implicit("await".into()),
        "__init_subclass__" => ProtocolMember::Implicit("subclassing".into()),
        "__str__" | "__repr__" | "__format__" | "__bytes__" | "__html__" | "__fspath__" => {
            ProtocolMember::Implicit("conversion".into())
        }
        _ => ProtocolMember::Implicit("operators and builtins".into()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::LanguageAdapter;

    fn census_of(source: &str) -> FileEscapeCensus {
        let adapter = crate::languages::PythonAdapter;
        let tree = adapter.parse(source.as_bytes()).unwrap();
        census(&tree, source.as_bytes())
    }

    fn contexts(census: &FileEscapeCensus, name: &str) -> Vec<(u32, OccurrenceContext)> {
        census
            .record
            .values
            .get(name)
            .map(|values| {
                values
                    .iter()
                    .map(|value| (value.line, value.context))
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn a_member_passed_as_an_argument_is_recorded_with_its_receiver() {
        let census = census_of(
            "import flask\n\
             def test_session():\n\
             \x20   expect_exception(flask.session.pop, \"foo\")\n\
             \x20   flask.session.pop(\"foo\")\n",
        );
        let pops = &census.record.values["pop"];
        assert_eq!(pops.len(), 1, "the call is not a value use: {pops:?}");
        assert_eq!(pops[0].line, 2);
        assert_eq!(pops[0].context, OccurrenceContext::Argument);
        assert_eq!(pops[0].receiver.as_deref(), Some("flask.session"));
        assert_eq!(pops[0].binding, OccurrenceBinding::Member);
    }

    /// Flask's `globals.py`: the proxy classes exist for the type checker
    /// alone, and `ProxyMixin` is a protocol.
    /// A class that binds `__getattr__` or `__getattribute__` answers any
    /// attribute read by its name; one that only inherits it says nothing of
    /// its own.
    #[test]
    fn a_class_says_when_it_answers_attribute_reads_by_name() {
        let census = census_of(
            "class Lazy:\n\
             \x20   def __getattr__(self, name):\n\
             \x20       return name\n\
             class Wrapped:\n\
             \x20   @staticmethod\n\
             \x20   def __getattribute__(name):\n\
             \x20       return name\n\
             class Assigned:\n\
             \x20   __getattr__ = make_hook()\n\
             class Plain(Lazy):\n\
             \x20   def get(self):\n\
             \x20       return 1\n",
        );
        let marks: Vec<(&str, bool)> = census
            .record
            .classes
            .iter()
            .map(|class| (class.name.as_str(), class.dynamic))
            .collect();
        assert_eq!(
            marks,
            vec![
                ("Lazy", true),
                ("Wrapped", true),
                ("Assigned", true),
                ("Plain", false),
            ]
        );
    }

    #[test]
    fn a_class_says_when_it_is_for_the_type_checker_alone_or_structural() {
        let census = census_of(
            "import typing as t\n\
             if t.TYPE_CHECKING:\n\
             \x20   class ProxyMixin(t.Protocol[T]):\n\
             \x20       def _get_current_object(self) -> T: ...\n\
             \x20   class SessionMixinProxy(ProxyMixin[SessionMixin], SessionMixin): ...\n\
             else:\n\
             \x20   class Fallback(Base): ...\n\
             class Real(Protocol): ...\n\
             class Plain(SessionMixin): ...\n\
             def make():\n\
             \x20   if TYPE_CHECKING:\n\
             \x20       class Hinted(Base): ...\n",
        );
        let marks: Vec<(&str, bool, bool)> = census
            .record
            .classes
            .iter()
            .map(|class| (class.name.as_str(), class.typing_only, class.structural))
            .collect();
        assert_eq!(
            marks,
            vec![
                ("ProxyMixin", true, true),
                ("SessionMixinProxy", true, false),
                ("Fallback", false, false),
                ("Real", false, true),
                ("Plain", false, false),
                ("Hinted", true, false),
            ]
        );
    }

    #[test]
    fn a_class_scope_rebinding_lists_every_target_and_aliases_the_value() {
        let census = census_of(
            "class NullSession:\n\
             \x20   def _fail(self, *args, **kwargs):\n\
             \x20       raise RuntimeError('x')\n\
             \x20   setitem = setdefault = pop = _fail\n",
        );
        let pop = &census.record.values["pop"];
        assert_eq!(pop.len(), 1);
        assert_eq!(pop[0].context, OccurrenceContext::Binding);
        assert_eq!(pop[0].line, 3);
        let fail = &census.record.values["_fail"];
        assert_eq!(fail[0].context, OccurrenceContext::Alias);
        assert_eq!(fail[0].binding, OccurrenceBinding::Class);
        let mut aliases = fail[0].aliases.clone();
        aliases.sort();
        assert_eq!(aliases, vec!["pop", "setdefault", "setitem"]);
    }

    /// Each occurrence of `name` that is a mention, as (line, form wire).
    fn mentions(census: &FileEscapeCensus, name: &str) -> Vec<(u32, &'static str)> {
        census
            .record
            .values
            .get(name)
            .map(|values| {
                values
                    .iter()
                    .filter(|value| value.context == OccurrenceContext::Mention)
                    .map(|value| (value.line, value.residue_kind()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Each occurrence of `name` that is not a mention.
    fn value_uses(census: &FileEscapeCensus, name: &str) -> Vec<(u32, OccurrenceContext)> {
        contexts(census, name)
            .into_iter()
            .filter(|(_, context)| *context != OccurrenceContext::Mention)
            .collect()
    }

    #[test]
    fn locals_and_calls_are_not_recorded_and_inert_uses_are_mentions_not_values() {
        let census = census_of(
            "def helper():\n    pass\n\
             def run(callback):\n\
             \x20   value = 1\n\
             \x20   register(value)\n\
             \x20   register(callback)\n\
             \x20   helper()\n\
             \x20   if helper is None:\n        pass\n\
             \x20   print(f\"{helper}\")\n\
             \x20   isinstance(x, helper)\n\
             \x20   helper.cache_clear()\n",
        );
        assert!(census.record.lists_mentions);
        assert!(!census.record.values.contains_key("value"));
        assert!(!census.record.values.contains_key("callback"));
        assert!(
            value_uses(&census, "helper").is_empty(),
            "an inert use holds no value: {:?}",
            census.record.values
        );
        // The call at line 6 is a call, never a mention.
        assert_eq!(
            mentions(&census, "helper"),
            vec![
                (7, "comparison"),
                (9, "read"),
                (10, "inert_argument"),
                (11, "receiver"),
            ]
        );
        let compared = &census.record.values["helper"][0];
        assert_eq!(compared.binding, OccurrenceBinding::Module);
        assert_eq!(compared.target_line, Some(0));
    }

    #[test]
    fn a_plain_import_is_a_mention_with_its_statement() {
        let census = census_of(
            "from .app import Flask, _make_timedelta as _make_timedelta\n\
             from . import views\n",
        );
        for (name, line) in [("Flask", 0), ("_make_timedelta", 0), ("views", 1)] {
            let uses = &census.record.values[name];
            assert_eq!(uses.len(), 1, "{name}: {uses:?}");
            assert_eq!(uses[0].context, OccurrenceContext::Mention, "{name}");
            assert_eq!(uses[0].form, Some(MentionForm::Import), "{name}");
            assert_eq!(uses[0].binding, OccurrenceBinding::Import, "{name}");
            assert_eq!(uses[0].line, line, "{name}");
            assert!(uses[0].import.is_some(), "{name}");
            assert!(uses[0].aliases.is_empty(), "{name}: never an alias");
        }
        assert_eq!(
            census.record.values["Flask"][0].module.as_deref(),
            Some(".app")
        );
    }

    #[test]
    fn annotations_decorators_bases_and_except_types_are_mentions() {
        let census = census_of(
            "import typing as t\n\
             from .app import Flask, Base, Other, Bad\n\
             def make(app: t.Optional[Flask], name: \"Flask\" = None) -> Flask:\n\
             \x20   current: Flask = app\n\
             \x20   try:\n\
             \x20       pass\n\
             \x20   except (Bad, KeyError) as error:\n\
             \x20       raise\n\
             @Other\n\
             def wrapped(): pass\n\
             class Child(Base, metaclass=Other):\n\
             \x20   kind: t.Literal[\"Flask\"]\n",
        );
        let flask: Vec<(u32, &str)> = mentions(&census, "Flask")
            .into_iter()
            .filter(|(_, form)| *form != "import")
            .collect();
        assert_eq!(
            flask,
            vec![
                (2, "annotation"),
                (2, "annotation"),
                (2, "annotation"),
                (3, "annotation")
            ],
            "a parameter, a quoted forward reference, a return and a local \
             annotation each mention it, a Literal value does not"
        );
        assert_eq!(
            census.record.values["Optional"]
                .iter()
                .map(|value| (
                    value.binding,
                    value.receiver.as_deref(),
                    value.residue_kind()
                ))
                .collect::<Vec<_>>(),
            vec![(OccurrenceBinding::Member, Some("t"), "annotation")]
        );
        assert!(mentions(&census, "t").contains(&(2, "receiver")));
        assert_eq!(
            mentions(&census, "Bad")
                .into_iter()
                .filter(|(_, form)| *form != "import")
                .collect::<Vec<_>>(),
            vec![(6, "except")]
        );
        assert!(
            !census.record.values.contains_key("error"),
            "an except alias binds"
        );
        assert!(mentions(&census, "Other").contains(&(8, "decorator")));
        assert!(mentions(&census, "Base").contains(&(10, "base")));
        // `metaclass=M` is a keyword argument: a value, as before.
        assert_eq!(
            value_uses(&census, "Other"),
            vec![(10, OccurrenceContext::Argument)]
        );
    }

    #[test]
    fn member_reads_stores_and_subscripts_are_mentions() {
        let census = census_of(
            "import flask\n\
             HANDLERS = {}\n\
             def run(session, key):\n\
             \x20   if session.permanent:\n\
             \x20       pass\n\
             \x20   session.modified = True\n\
             \x20   HANDLERS[key]\n\
             \x20   flask.session.pop(key)\n",
        );
        assert_eq!(mentions(&census, "permanent"), vec![(3, "condition")]);
        let stored = &census.record.values["modified"];
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].form, Some(MentionForm::Store));
        assert_eq!(stored[0].receiver.as_deref(), Some("session"));
        assert_eq!(mentions(&census, "HANDLERS"), vec![(6, "subscript")]);
        // `flask.session.pop(key)` calls `pop`; `session` is read off `flask`.
        assert!(!census.record.values.contains_key("pop"));
        assert_eq!(mentions(&census, "session"), vec![(7, "receiver")]);
        assert_eq!(mentions(&census, "flask"), vec![(7, "receiver")]);
        assert!(
            !census.record.values.contains_key("key"),
            "a parameter is never recorded"
        );
    }

    #[test]
    fn a_dotted_chain_splits_into_names() {
        assert_eq!(
            dotted_chains("t.Optional[Flask], 'x'"),
            vec![vec!["t", "Optional"], vec!["Flask"], vec!["x"]]
        );
        assert_eq!(dotted_chains("a1.b2 3c"), vec![vec!["a1", "b2"]]);
    }

    #[test]
    fn a_module_function_passed_by_name_binds_to_its_definition() {
        let census = census_of(
            "def _make_timedelta(value):\n    return value\n\
             class App:\n\
             \x20   permanent = ConfigAttribute('X', get_converter=_make_timedelta)\n",
        );
        let uses = &census.record.values["_make_timedelta"];
        assert_eq!(uses.len(), 1);
        assert_eq!(uses[0].context, OccurrenceContext::Argument);
        assert_eq!(uses[0].binding, OccurrenceBinding::Module);
        assert_eq!(uses[0].target_line, Some(0));
    }

    #[test]
    fn a_local_rebinding_shadows_the_module_name_everywhere_in_the_function() {
        let census = census_of(
            "def target():\n    pass\n\
             def run():\n\
             \x20   register(target)\n\
             \x20   target = 1\n",
        );
        assert!(!census.record.values.contains_key("target"));
    }

    #[test]
    fn a_renaming_import_is_recorded_as_an_alias() {
        let census = census_of("from .events import set_enabled as emitter_set_enabled\n");
        let import = &census.record.values["set_enabled"][0];
        assert_eq!(import.context, OccurrenceContext::Import);
        assert_eq!(import.aliases, vec!["emitter_set_enabled"]);
        assert_eq!(import.module.as_deref(), Some(".events"));
        assert!(import.import.is_some());
    }

    #[test]
    fn reflective_sites_are_recorded_with_their_reach() {
        let census = census_of(
            "import importlib\n\
             def dispatch(self, obj, kind, name):\n\
             \x20   handler = getattr(self, f\"visit_{kind}\")\n\
             \x20   other = getattr(obj, name)\n\
             \x20   named = getattr(obj, \"known\")\n\
             \x20   getattr(obj, name)()\n\
             \x20   exec(code, obj.__dict__)\n\
             \x20   mod = importlib.import_module(name)\n\
             \x20   return handler, other, named\n\
             table = globals()\n",
        );
        let forms: Vec<(u32, ReflectiveForm, ReflectiveReach, bool)> = census
            .record
            .reflective
            .iter()
            .map(|site| (site.line, site.form, site.reach, site.called))
            .collect();
        assert!(
            forms.contains(&(
                2,
                ReflectiveForm::ComputedMember,
                ReflectiveReach::Members,
                false
            )),
            "{forms:?}"
        );
        assert!(
            forms.contains(&(
                3,
                ReflectiveForm::ComputedMember,
                ReflectiveReach::Any,
                false
            )),
            "{forms:?}"
        );
        assert!(
            forms.contains(&(
                5,
                ReflectiveForm::ComputedMember,
                ReflectiveReach::Any,
                true
            )),
            "{forms:?}"
        );
        assert!(
            forms.contains(&(6, ReflectiveForm::Evaluate, ReflectiveReach::Any, false)),
            "{forms:?}"
        );
        assert!(
            forms.contains(&(
                6,
                ReflectiveForm::ComputedMember,
                ReflectiveReach::Any,
                false
            )),
            "{forms:?}"
        );
        assert!(
            forms.contains(&(
                7,
                ReflectiveForm::DynamicImport,
                ReflectiveReach::Nothing,
                false
            )),
            "{forms:?}"
        );
        assert!(
            forms.contains(&(
                9,
                ReflectiveForm::ScopeTable,
                ReflectiveReach::OwnModule,
                false
            )),
            "{forms:?}"
        );
        let visit = census
            .record
            .reflective
            .iter()
            .find(|site| site.line == 2)
            .unwrap();
        assert_eq!(visit.key_prefix.as_deref(), Some("visit_"));
        // A literal name reads the member it spells.
        assert_eq!(
            contexts(&census, "known"),
            vec![(4, OccurrenceContext::Assignment)]
        );
    }

    #[test]
    fn with_items_and_bare_raises_are_protocol_sites() {
        let census = census_of(
            "def run(app):\n\
             \x20   with app.app_context():\n\
             \x20       raise NotFound\n\
             \x20   try:\n        pass\n\
             \x20   except KeyError as error:\n\
             \x20       raise error\n",
        );
        let sites: Vec<(u32, ProtocolForm, Option<&str>)> = census
            .record
            .protocol
            .iter()
            .map(|site| (site.line, site.form, site.name.as_deref()))
            .collect();
        assert_eq!(
            sites,
            vec![
                (1, ProtocolForm::With, None),
                (2, ProtocolForm::Raise, Some("NotFound")),
            ]
        );
    }

    #[test]
    fn dunder_and_property_methods_are_protocol_members() {
        let census = census_of(
            "class Ctx:\n\
             \x20   def __enter__(self):\n        return self\n\
             \x20   def __eq__(self, other):\n        return True\n\
             \x20   def __init__(self):\n        pass\n\
             \x20   @property\n\
             \x20   def name(self):\n        return 1\n\
             \x20   def pop(self):\n        pass\n",
        );
        let members: Vec<(&str, &ProtocolMember)> = census
            .protocol_members
            .iter()
            .map(|stamp| (stamp.name.as_str(), &stamp.member))
            .collect();
        assert_eq!(
            members,
            vec![
                (
                    "__enter__",
                    &ProtocolMember::Listed(vec![ProtocolForm::With])
                ),
                (
                    "__eq__",
                    &ProtocolMember::Implicit("operators and builtins".into())
                ),
                ("name", &ProtocolMember::Implicit("attribute access".into())),
            ]
        );
    }

    fn lookup(census: &FileEscapeCensus, line: u32) -> &ReflectiveSite {
        census
            .record
            .reflective
            .iter()
            .find(|site| site.line == line)
            .unwrap_or_else(|| panic!("a site on line {line}: {:?}", census.record.reflective))
    }

    /// `getattr(Flask, method.__name__)` in `flask/app.py` reads members of a
    /// class the file declares, never a module-level function; a lookup on a
    /// whole-module import reads that module's bindings.
    #[test]
    fn a_lookup_records_what_its_receiver_is() {
        let census = census_of(
            "import flask.app as appmod\n\
             from flask import json\n\
             class Flask:\n    pass\n\
             def helper():\n    pass\n\
             def run(method, name, obj):\n\
             \x20   a = getattr(Flask, method.__name__)\n\
             \x20   b = getattr(appmod, name)\n\
             \x20   c = getattr(obj, name)\n\
             \x20   d = getattr(json, name)\n\
             \x20   e = getattr(helper, name)\n\
             \x20   return a, b, c, d, e\n",
        );
        assert_eq!(
            lookup(&census, 7).receiver,
            Some(LookupReceiver::Declared {
                name: "Flask".into(),
                line: 2
            })
        );
        assert_eq!(lookup(&census, 8).receiver, Some(LookupReceiver::Module));
        assert_eq!(lookup(&census, 9).receiver, None);
        // `from flask import json` may bind a module or anything else.
        assert_eq!(lookup(&census, 10).receiver, None);
        assert_eq!(
            lookup(&census, 11).receiver,
            Some(LookupReceiver::Declared {
                name: "helper".into(),
                line: 4
            })
        );
        for line in 7..=11 {
            assert_eq!(lookup(&census, line).reach, ReflectiveReach::Any);
        }
    }

    /// What a census reads to tell `json.load` under `import json` from a
    /// same-named declaration of the repository: the member read is classed
    /// as made on a module-level import binding, and the receiver read on its
    /// line carries the one import statement that binds it. A parameter or a
    /// local of the same name carries neither.
    /// `tests/test_config.py` in Flask: `tomllib` is what
    /// `pytest.importorskip("tomllib")` returns, a module, so `tomllib.load`
    /// is read off a module the test imported by name.
    #[test]
    fn a_module_loaded_by_name_is_an_import_of_that_module() {
        let census = census_of(
            "import importlib
             import pytest
             def test_config_from_file_toml(app):
                 tomllib = pytest.importorskip('tomllib', reason='tomllib added in 3.11')
                 app.config.from_file('config.toml', tomllib.load, text=False)
             def loaded(app):
                 codec = importlib.import_module('json.decoder')
                 app.config.from_file('c.json', codec.load)
             def relative(app):
                 codec = importlib.import_module('.decoder', package='json')
                 app.config.from_file('c.json', codec.load)
             def computed(app, name):
                 codec = pytest.importorskip(name)
                 app.config.from_file('c.json', codec.load)
             def rebound(app):
                 codec = pytest.importorskip('tomllib')
                 codec = app
                 app.config.from_file('c.json', codec.load)
             def shadowed(app, pytest):
                 codec = pytest.importorskip('tomllib')
                 app.config.from_file('c.json', codec.load)
",
        );
        let loads = &census.record.values["load"];
        let read = |line: u32| {
            loads
                .iter()
                .find(|value| value.line == line)
                .unwrap_or_else(|| panic!("no read on line {line}: {loads:?}"))
        };
        let receiver = |name: &str, line: u32| {
            census.record.values.get(name).and_then(|values| {
                values
                    .iter()
                    .find(|value| value.line == line && value.form == Some(MentionForm::Receiver))
            })
        };
        assert_eq!(read(4).context, OccurrenceContext::Argument);
        assert_eq!(read(4).receiver.as_deref(), Some("tomllib"));
        assert_eq!(read(4).receiver_kind, Some(ReceiverKind::Namespace));
        let tomllib = receiver("tomllib", 4).expect("the receiver read");
        assert_eq!(tomllib.binding, OccurrenceBinding::Import);
        assert_eq!(tomllib.module.as_deref(), Some("tomllib"));

        assert_eq!(read(7).receiver_kind, Some(ReceiverKind::Namespace));
        let codec = receiver("codec", 7).expect("the receiver read");
        assert_eq!(codec.module.as_deref(), Some("json.decoder"));

        // A relative or computed name, a second binding of the target, and a
        // loader name the function binds itself: a plain local, as before.
        for line in [10, 13, 17, 20] {
            assert_eq!(
                read(line).receiver_kind,
                Some(ReceiverKind::Other),
                "{line}"
            );
            assert!(receiver("codec", line).is_none(), "{line}");
        }
    }

    /// A function that imports a name and passes it on hands over what the
    /// import brings in, exactly as a module-level import would.
    #[test]
    fn a_name_a_function_imports_is_read_as_that_import() {
        let census = census_of(
            "def configure(app):
                 from .handlers import on_error
                 import json
                 app.register_error_handler(500, on_error)
                 app.config.from_file('c.json', json.load)
             def local(app, on_error):
                 app.register_error_handler(500, on_error)
",
        );
        let handlers = &census.record.values["on_error"];
        let passed = handlers
            .iter()
            .find(|value| value.line == 3)
            .unwrap_or_else(|| panic!("the argument on line 3: {handlers:?}"));
        assert_eq!(passed.context, OccurrenceContext::Argument);
        assert_eq!(passed.binding, OccurrenceBinding::Import);
        assert_eq!(passed.module.as_deref(), Some(".handlers"));
        assert!(
            handlers.iter().all(|value| value.line != 6),
            "a parameter is a plain local: {handlers:?}"
        );
        let load = census.record.values["load"]
            .iter()
            .find(|value| value.line == 4)
            .expect("the member read");
        assert_eq!(load.receiver_kind, Some(ReceiverKind::Namespace));
    }

    #[test]
    fn a_member_read_on_an_import_records_the_import_on_its_receiver() {
        let census = census_of(
            "import json
             import json as _json
             from flask import json as fjson
             def load(fp):
                 return _json.load(fp)
             def test(app):
                 app.config.from_file('c.json', json.load)
                 app.config.from_file('c.json', load=fjson.load)
             def shadowed(json, tomllib):
                 _json = json
                 return [json.load, tomllib.load, _json.load]
",
        );
        let loads = &census.record.values["load"];
        let read = |line: u32| {
            loads
                .iter()
                .find(|value| value.line == line)
                .unwrap_or_else(|| panic!("no read on line {line}: {loads:?}"))
        };
        let receiver = |name: &str, line: u32| {
            census.record.values.get(name).and_then(|values| {
                values
                    .iter()
                    .find(|value| value.line == line && value.form == Some(MentionForm::Receiver))
            })
        };
        assert_eq!(read(6).context, OccurrenceContext::Argument);
        assert_eq!(read(6).binding, OccurrenceBinding::Member);
        assert_eq!(read(6).receiver.as_deref(), Some("json"));
        assert_eq!(read(6).receiver_kind, Some(ReceiverKind::Namespace));
        let json = receiver("json", 6).expect("the receiver read");
        assert_eq!(json.binding, OccurrenceBinding::Import);
        assert_eq!(json.module.as_deref(), Some("json"));

        assert_eq!(read(7).receiver_kind, Some(ReceiverKind::Namespace));
        let fjson = receiver("fjson", 7).expect("the receiver read");
        assert_eq!(fjson.binding, OccurrenceBinding::Import);
        assert_eq!(fjson.module.as_deref(), Some("flask"));

        // The call through `_json` is no value use, and its receiver read
        // still names the import.
        let called = receiver("_json", 4).expect("the receiver read of a call");
        assert_eq!(called.binding, OccurrenceBinding::Import);
        assert_eq!(called.module.as_deref(), Some("json"));

        // Inside `shadowed` every receiver is a parameter or a local.
        let shadowed: Vec<_> = loads.iter().filter(|value| value.line == 10).collect();
        assert_eq!(shadowed.len(), 3, "{loads:?}");
        for value in shadowed {
            assert_eq!(value.receiver_kind, Some(ReceiverKind::Other), "{value:?}");
        }
        for name in ["json", "tomllib", "_json"] {
            assert!(receiver(name, 10).is_none(), "{name}");
        }
    }

    #[test]
    fn a_subscripted_vars_table_is_a_lookup_and_compile_alone_runs_nothing() {
        let census = census_of(
            "def run(obj, name, src):\n\
             \x20   vars(obj)[name]()\n\
             \x20   for value in vars(obj).values():\n        pass\n\
             \x20   code = compile(src, 'x', 'exec')\n\
             \x20   exec(code)\n",
        );
        assert_eq!(lookup(&census, 1).form, ReflectiveForm::ComputedMember);
        assert_eq!(lookup(&census, 2).form, ReflectiveForm::ComputedMember);
        let compiled = lookup(&census, 4);
        assert_eq!(compiled.form, ReflectiveForm::Evaluate);
        assert_eq!(compiled.reach, ReflectiveReach::Nothing);
        assert_eq!(lookup(&census, 5).reach, ReflectiveReach::Any);
    }

    #[test]
    fn a_computed_name_keeps_every_literal_run_it_holds() {
        let census = census_of(
            "def run(o, a, b, k):\n\
             \x20   w = getattr(o, f\"on_{a}_{b}_done\")\n\
             \x20   x = getattr(o, \"visit_%s\" % k)\n\
             \x20   y = getattr(o, \"handle_{}_now\".format(k))\n\
             \x20   z = getattr(o, \"pre\" + k + \"-\" + a + \"post\")\n\
             \x20   return w, x, y, z\n",
        );
        let on = lookup(&census, 1);
        assert_eq!(on.key_prefix.as_deref(), Some("on_"));
        assert_eq!(on.key_parts, vec!["_".to_string()]);
        assert_eq!(on.key_suffix.as_deref(), Some("_done"));
        assert!(on.key_admits("on_click_x_done"));
        assert!(!on.key_admits("on_done"));
        assert_eq!(lookup(&census, 2).key_prefix.as_deref(), Some("visit_"));
        let handle = lookup(&census, 3);
        assert_eq!(handle.key_prefix.as_deref(), Some("handle_"));
        assert_eq!(handle.key_suffix.as_deref(), Some("_now"));
        let joined = lookup(&census, 4);
        assert_eq!(joined.key_prefix.as_deref(), Some("pre"));
        assert_eq!(joined.key_parts, vec!["-".to_string()]);
        assert_eq!(joined.key_suffix.as_deref(), Some("post"));
        // A dash is in no identifier, so no call name can be the key.
        assert!(!joined.key_admits("preXpost"));
    }

    #[test]
    fn a_class_records_the_bases_a_lookup_on_it_reads() {
        let census = census_of(
            "import mod\n\
             from base import Scaffold as Local\n\
             class App(Local, metaclass=Meta):\n    pass\n\
             class Flask(App):\n    pass\n\
             class Typed(mod.Base, Generic[T]):\n    pass\n\
             Alias = Flask\n\
             class Made(make_base(), Alias):\n    pass\n\
             @decorate\n\
             class Wrapped:\n    pass\n",
        );
        let classes: Vec<(&str, u32, Vec<&str>, bool)> = census
            .record
            .classes
            .iter()
            .map(|class| {
                (
                    class.name.as_str(),
                    class.line,
                    class.bases.iter().map(String::as_str).collect(),
                    class.open,
                )
            })
            .collect();
        assert_eq!(
            classes,
            vec![
                ("App", 2, vec!["Scaffold", "Meta"], false),
                ("Flask", 4, vec!["App"], false),
                ("Typed", 6, vec!["Base", "Generic"], false),
                ("Made", 9, vec![], true),
                ("Wrapped", 12, vec![], true),
            ]
        );
    }

    #[test]
    fn an_unparsed_region_is_recorded() {
        let census = census_of("def ok():\n    pass\ndef broken(:\n    pass\n");
        assert!(!census.record.unparsed.is_empty());
    }
}
