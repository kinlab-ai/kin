// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! JavaScript, TypeScript and Flow value origins: declared parameter, local and
//! return types, props destructured from a declared object type, React context
//! reads, and the contexts a module creates.
//!
//! React answers `useContext(C)` with the value the nearest `C.Provider` above
//! the caller supplies, and the one type the source declares for that value is
//! the one `C` was created with: `createContext<Store>(...)`, or the
//! `ReactContext<Store>` annotation on the binding. So a module-level
//! `const C = createContext<T>(...)` provides `T` under the name `C`, and a
//! component's `const store = useContext(C)` reads the provider bound to `C`
//! in its own file: the one the file declares, or the one an import of `C`
//! reaches. Only a hook reached through an import of `'react'` counts, because
//! a `useContext` from anywhere else could return anything.
//!
//! A Flow file is parsed by a TypeScript grammar (see
//! [`crate::languages::javascript::parse_javascript_family`]) and walked by the
//! TypeScript extractor, so this reader takes a tree from either grammar and
//! mirrors the extractor the adapter ran over it: facts go to the entities that
//! extractor mints, keyed by the spans it gives them.
//!
//! A local is recorded only when the callable's subtree binds its name exactly
//! once, nested functions included, and every reference to the name in that
//! subtree sits where the binding is visible. Counting is enough for a
//! parameter or a function-scoped `var`, but `let` and `const` are scoped to
//! their block, and a read outside the block names some other binding.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use tree_sitter::{Node, Tree};

use super::{
    DeclaredReturn, ExternalReach, FileFacts, ProviderLookup, ScopeFacts, ValueOrigin,
    ValueProvider,
};

/// The namespace React context providers are provided in.
pub(crate) const REACT_CONTEXT_KIND: &str = "react_context";

/// The namespace typed injection keys are provided in: Vue's
/// `InjectionKey<T>` and Angular's `InjectionToken<T>`.
pub(crate) const INJECTION_KEY_KIND: &str = "injection_key";

/// Built-in constructors whose `new` makes a value of exactly that class,
/// which no file of the repository declares.
const BUILTIN_CONSTRUCTORS: &[&str] = &[
    "Array",
    "Date",
    "Error",
    "Map",
    "RangeError",
    "RegExp",
    "Set",
    "TypeError",
    "URL",
    "URLSearchParams",
    "WeakMap",
    "WeakSet",
];

/// Built-in functions whose result is exactly an `Array`.
const ARRAY_FACTORIES: &[&str] = &[
    "Array.from",
    "Array.of",
    "Object.entries",
    "Object.keys",
    "Object.values",
];

/// Built-in collection types an annotation can name, by the class a value of
/// the type is shaped like. TypeScript and Flow check shape, not class, so a
/// value declared as one may be any object that fits.
const STRUCTURAL_COLLECTIONS: &[(&str, &str)] = &[
    ("$ReadOnlyArray", "Array"),
    ("Array", "Array"),
    ("Map", "Map"),
    ("ReadonlyArray", "Array"),
    ("ReadonlyMap", "Map"),
    ("ReadonlySet", "Set"),
    ("Set", "Set"),
    ("WeakMap", "WeakMap"),
    ("WeakSet", "WeakSet"),
];

/// Primitive types, by the built-in class whose prototype their methods live
/// on. A primitive value has no members of its own.
const PRIMITIVE_TYPES: &[(&str, &str)] = &[
    ("bigint", "BigInt"),
    ("boolean", "Boolean"),
    ("number", "Number"),
    ("string", "String"),
];

/// The modules whose `inject` answers with the value registered for a token.
const ANGULAR_CORE: &str = "@angular/core";
const VUE_MODULES: &[&str] = &["vue", "@vue/runtime-core"];

/// The module React's hooks and `createContext` come from.
const REACT_MODULE: &str = "react";

/// Type names that say nothing about which class a value is an instance of.
const OPAQUE_TYPES: &[&str] = &[
    "any",
    "mixed",
    "unknown",
    "never",
    "void",
    "empty",
    "null",
    "undefined",
    "string",
    "number",
    "boolean",
    "symbol",
    "bigint",
    "object",
    "Object",
    "Function",
    "String",
    "Number",
    "Boolean",
    "Symbol",
    "BigInt",
];

/// Wrappers Flow and TypeScript put around an object type without changing
/// which properties it declares or their types.
const OBJECT_WRAPPERS: &[&str] = &["$ReadOnly", "$Exact", "Readonly"];

/// Identifiers that never name a value a scope's binding could describe.
const NOT_BINDINGS: &[&str] = &["undefined", "NaN", "Infinity", "arguments", "require"];

/// How deep a chain of same-file type aliases is followed to its object type.
const ALIAS_DEPTH: usize = 4;

pub(super) fn read_file(tree: &Tree, source: &[u8], _file_path: &str) -> FileFacts {
    let root = tree.root_node();
    let file = FileContext::read(root, source);
    let reader = Reader {
        file: &file,
        source,
        typescript: crate::languages::javascript::tree_is_typescript_family(tree),
    };
    let mut facts = FileFacts::default();
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        reader.statement(child, None, &mut facts);
        reader.module_providers(child, &mut facts.module.provides);
    }
    // A module has one default export. Two are a syntax error, and then
    // neither is the one an import means.
    if let [one] = facts.default_exports.as_slice() {
        facts.module.default_export = (!one.is_empty()).then(|| one.clone());
    }
    facts
}

/// What the file's top level says about every scope in it.
struct FileContext<'tree> {
    /// Local names an `import * as ns` binds, from any module.
    namespaces: HashSet<String>,
    /// Local names bound to React's module object, by `import * as React` or
    /// `import React` from `'react'`.
    react_objects: HashSet<String>,
    /// Local names `import {useContext [as x]} from 'react'` binds.
    use_context: HashSet<String>,
    /// Local names `import {createContext [as x]} from 'react'` binds.
    create_context: HashSet<String>,
    /// The type a top-level `type X = ...` or `interface X {...}` declares, by
    /// name. A name the file declares as a type more than once, anywhere, or
    /// uses as a type parameter anywhere, is absent: a reference to it may not
    /// mean this declaration.
    object_types: HashMap<String, Node<'tree>>,
    /// Every name the file declares as a type anywhere, type parameters
    /// included, which a type annotation may then mean instead of a built-in.
    declared_types: HashSet<String>,
    /// Every value name the module scope binds: an import, a declaration, a
    /// top-level variable.
    module_bindings: HashSet<String>,
    /// Local names `import {inject} from '@angular/core'` binds.
    angular_inject: HashSet<String>,
    /// Local names `import {InjectionToken} from '@angular/core'` binds.
    injection_token: HashSet<String>,
    /// Local names `import {inject} from 'vue'` binds.
    vue_inject: HashSet<String>,
    /// Module-level constants declared as typed injection keys.
    injection_keys: HashSet<String>,
}

impl<'tree> FileContext<'tree> {
    fn read(root: Node<'tree>, source: &[u8]) -> Self {
        let mut context = Self {
            namespaces: HashSet::new(),
            react_objects: HashSet::new(),
            use_context: HashSet::new(),
            create_context: HashSet::new(),
            object_types: HashMap::new(),
            declared_types: HashSet::new(),
            module_bindings: HashSet::new(),
            angular_inject: HashSet::new(),
            injection_token: HashSet::new(),
            vue_inject: HashSet::new(),
            injection_keys: HashSet::new(),
        };
        let declared = type_declaration_counts(root, source);
        context.declared_types = declared.keys().map(|name| name.to_string()).collect();
        let mut cursor = root.walk();
        for child in root.named_children(&mut cursor) {
            let declaration = match child.kind() {
                "export_statement" => child.child_by_field_name("declaration"),
                _ => Some(child),
            };
            let Some(declaration) = declaration else {
                continue;
            };
            context.bind_declaration(declaration, source);
            let value = match declaration.kind() {
                "import_statement" => {
                    context.read_import(declaration, source);
                    continue;
                }
                "type_alias_declaration" => declaration.child_by_field_name("value"),
                "interface_declaration" => declaration.child_by_field_name("body"),
                _ => continue,
            };
            if declaration.child_by_field_name("type_parameters").is_some() {
                continue;
            }
            let (Some(name), Some(value)) = (declaration.child_by_field_name("name"), value) else {
                continue;
            };
            let name = text(name, source);
            if declared.get(name) == Some(&1) {
                context.object_types.insert(name.to_string(), value);
            }
        }
        context.read_injection_keys(root, source);
        context
    }

    /// Module-level `const` keys declared as typed injection keys: annotated
    /// `InjectionKey<T>`, or constructed by Angular's `InjectionToken`.
    fn read_injection_keys(&mut self, root: Node<'_>, source: &[u8]) {
        let mut cursor = root.walk();
        for child in root.named_children(&mut cursor) {
            let declaration = match child.kind() {
                "export_statement" => child.child_by_field_name("declaration"),
                _ => Some(child),
            };
            let Some(declaration) = declaration.filter(|node| {
                node.kind() == "lexical_declaration"
                    && node
                        .child_by_field_name("kind")
                        .is_some_and(|kind| text(kind, source) == "const")
            }) else {
                continue;
            };
            let mut declarators = declaration.walk();
            for declarator in declaration.named_children(&mut declarators) {
                let Some(name) = declarator
                    .child_by_field_name("name")
                    .filter(|name| name.kind() == "identifier")
                else {
                    continue;
                };
                let annotated = declarator
                    .child_by_field_name("type")
                    .is_some_and(|annotation| injection_key_argument(annotation, source).is_some());
                let constructed = declarator
                    .child_by_field_name("value")
                    .map(effective_value)
                    .filter(|value| value.kind() == "new_expression")
                    .and_then(|value| value.child_by_field_name("constructor"))
                    .is_some_and(|constructor| {
                        self.injection_token.contains(text(constructor, source))
                    });
                if annotated || constructed {
                    self.injection_keys.insert(text(name, source).to_string());
                }
            }
        }
    }

    /// The value names a top-level declaration binds.
    fn bind_declaration(&mut self, declaration: Node<'_>, source: &[u8]) {
        match declaration.kind() {
            "function_declaration"
            | "generator_function_declaration"
            | "class_declaration"
            | "abstract_class_declaration"
            | "enum_declaration"
            | "internal_module"
            | "module" => {
                if let Some(name) = declaration.child_by_field_name("name") {
                    self.module_bindings.insert(text(name, source).to_string());
                }
            }
            "lexical_declaration" | "variable_declaration" => {
                let mut cursor = declaration.walk();
                for declarator in declaration.named_children(&mut cursor) {
                    if let Some(name) = declarator.child_by_field_name("name") {
                        let mut stack = vec![name];
                        while let Some(node) = stack.pop() {
                            if matches!(
                                node.kind(),
                                "identifier" | "shorthand_property_identifier_pattern"
                            ) {
                                self.module_bindings.insert(text(node, source).to_string());
                            }
                            let mut walker = node.walk();
                            stack.extend(node.named_children(&mut walker));
                        }
                    }
                }
            }
            "ambient_declaration" => {
                let mut cursor = declaration.walk();
                for child in declaration.named_children(&mut cursor) {
                    self.bind_declaration(child, source);
                }
            }
            _ => {}
        }
    }

    /// Whether `name` still means the built-in global of that name as a value
    /// in a scope whose census is `census`.
    fn value_unbound(&self, name: &str, census: &Census<'_>) -> bool {
        !self.module_bindings.contains(name) && census.count(name) == 0
    }

    /// Whether `name` still means the built-in type of that name in a type
    /// position.
    fn type_unbound(&self, name: &str) -> bool {
        !self.module_bindings.contains(name) && !self.declared_types.contains(name)
    }

    fn read_import(&mut self, statement: Node<'_>, source: &[u8]) {
        // Every local an import binds shadows a global of its name, types
        // included.
        let mut names = statement.walk();
        if let Some(clause) = statement
            .named_children(&mut names)
            .find(|child| child.kind() == "import_clause")
        {
            let mut stack = vec![clause];
            while let Some(node) = stack.pop() {
                match node.kind() {
                    "identifier" => {
                        self.module_bindings.insert(text(node, source).to_string());
                    }
                    "import_specifier" => {
                        let local = node
                            .child_by_field_name("alias")
                            .or_else(|| node.child_by_field_name("name"));
                        if let Some(local) = local {
                            self.module_bindings.insert(text(local, source).to_string());
                        }
                        continue;
                    }
                    _ => {}
                }
                let mut walker = node.walk();
                stack.extend(node.named_children(&mut walker));
            }
        }
        // `import type` and `import typeof` bind types, never a value.
        if has_token(statement, &["type", "typeof"]) {
            return;
        }
        let module = statement
            .child_by_field_name("source")
            .map(|module| string_value(module, source))
            .unwrap_or_default();
        if module == ANGULAR_CORE || VUE_MODULES.contains(&module) {
            let mut cursor = statement.walk();
            let mut stack: Vec<Node<'_>> = statement.named_children(&mut cursor).collect();
            while let Some(node) = stack.pop() {
                if node.kind() == "import_specifier" && !has_token(node, &["type", "typeof"]) {
                    let (Some(name), local) = (
                        node.child_by_field_name("name"),
                        node.child_by_field_name("alias")
                            .or_else(|| node.child_by_field_name("name")),
                    ) else {
                        continue;
                    };
                    let local = local.map(|local| text(local, source).to_string());
                    match (module == ANGULAR_CORE, text(name, source), local) {
                        (true, "inject", Some(local)) => {
                            self.angular_inject.insert(local);
                        }
                        (true, "InjectionToken", Some(local)) => {
                            self.injection_token.insert(local);
                        }
                        (false, "inject", Some(local)) => {
                            self.vue_inject.insert(local);
                        }
                        _ => {}
                    }
                    continue;
                }
                let mut walker = node.walk();
                stack.extend(node.named_children(&mut walker));
            }
        }
        let react = module == REACT_MODULE;
        let mut cursor = statement.walk();
        let Some(clause) = statement
            .named_children(&mut cursor)
            .find(|child| child.kind() == "import_clause")
        else {
            return;
        };
        let mut parts = clause.walk();
        for part in clause.named_children(&mut parts) {
            match part.kind() {
                // The default import.
                "identifier" if react => {
                    self.react_objects.insert(text(part, source).to_string());
                }
                "namespace_import" => {
                    let mut names = part.walk();
                    let local = part
                        .named_children(&mut names)
                        .find(|name| name.kind() == "identifier");
                    if let Some(local) = local {
                        let local = text(local, source).to_string();
                        if react {
                            self.react_objects.insert(local.clone());
                        }
                        self.namespaces.insert(local);
                    }
                }
                "named_imports" if react => {
                    let mut specifiers = part.walk();
                    for specifier in part.named_children(&mut specifiers) {
                        if specifier.kind() != "import_specifier"
                            || has_token(specifier, &["type", "typeof"])
                        {
                            continue;
                        }
                        let Some(name) = specifier.child_by_field_name("name") else {
                            continue;
                        };
                        let local = specifier.child_by_field_name("alias").unwrap_or(name);
                        let local = text(local, source).to_string();
                        match text(name, source) {
                            "useContext" => {
                                self.use_context.insert(local);
                            }
                            "createContext" => {
                                self.create_context.insert(local);
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

/// How often each type name is declared anywhere in the file, type
/// parameters included.
fn type_declaration_counts<'s>(root: Node<'_>, source: &'s [u8]) -> HashMap<&'s str, usize> {
    let mut counts = HashMap::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if matches!(
            node.kind(),
            "type_alias_declaration"
                | "interface_declaration"
                | "class_declaration"
                | "abstract_class_declaration"
                | "enum_declaration"
                | "type_parameter"
        ) {
            if let Some(name) = node.child_by_field_name("name") {
                *counts.entry(text(name, source)).or_default() += 1;
            }
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    counts
}

/// What a callable's return may be read as.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Callable {
    /// A free function, or a function-valued property of an object literal.
    Function,
    /// A class member with a body, whose `this` is its receiver.
    Method,
    /// A member declared with no body: its return type is all it says.
    Signature,
}

struct Reader<'a, 'tree> {
    file: &'a FileContext<'tree>,
    source: &'a [u8],
    /// Whether the tree came from a TypeScript-family grammar, which is what
    /// makes the adapter walk it with the TypeScript extractor.
    typescript: bool,
}

impl<'a, 'tree> Reader<'a, 'tree> {
    /// One statement, as `extract_js_node` or `extract_ts_node` reads it.
    /// `export` is the `export_statement` around it, when there is one.
    fn statement(&self, node: Node<'tree>, export: Option<Node<'tree>>, facts: &mut FileFacts) {
        match node.kind() {
            "function_declaration" | "function" => {
                let Some(name) = node.child_by_field_name("name") else {
                    return;
                };
                let mut ranges = vec![range(node)];
                ranges.extend(export.map(range));
                let scope = self.scope(node, node, Callable::Function);
                self.push(facts, ranges, text(name, self.source), scope);
            }
            "class_declaration" => self.class(node, facts),
            "abstract_class_declaration" if self.typescript => self.class(node, facts),
            "interface_declaration" if self.typescript => self.interface(node, facts),
            "expression_statement" => self.assignment(node, facts),
            "lexical_declaration" | "variable_declaration" => self.declarators(node, export, facts),
            "export_statement" => {
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    self.statement(child, Some(node), facts);
                }
                self.default_export(node, facts);
            }
            // A namespace body's declarations are the file's own, by their
            // plain names, and `declare` wraps a plain declaration.
            "internal_module" | "module" if self.typescript => {
                if let Some(body) = node.child_by_field_name("body") {
                    let mut cursor = body.walk();
                    for child in body.children(&mut cursor) {
                        self.statement(child, None, facts);
                    }
                }
            }
            "ambient_declaration" if self.typescript => {
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    self.statement(child, None, facts);
                }
            }
            _ => {}
        }
    }

    fn push(
        &self,
        facts: &mut FileFacts,
        ranges: Vec<(usize, usize)>,
        name: &str,
        scope: ScopeFacts,
    ) {
        let leaf = leaf_of(name);
        if !leaf.is_empty() {
            facts.by_node.push((ranges, leaf.to_string(), scope));
        }
    }

    /// `const f = () => {}`, `const Foo = class {}`, `const utils = { a() {} }`
    /// and, in the TypeScript walk, `const C = memo((props) => ...)`.
    fn declarators(&self, node: Node<'tree>, export: Option<Node<'tree>>, facts: &mut FileFacts) {
        let mut cursor = node.walk();
        for declarator in node.children(&mut cursor) {
            if declarator.kind() != "variable_declarator" {
                continue;
            }
            let name = if self.typescript {
                declarator.child_by_field_name("name")
            } else {
                declarator
                    .child_by_field_name("name")
                    .or_else(|| declarator.named_child(0))
            };
            let (Some(name), Some(value)) = (name, declarator.child_by_field_name("value")) else {
                continue;
            };
            let name = text(name, self.source);
            let value = if self.typescript {
                effective_value(value)
            } else {
                value
            };
            let mut ranges = vec![range(declarator), range(node)];
            ranges.extend(export.map(range));
            if value.kind() == "class" {
                if !name.is_empty() {
                    self.class(value, facts);
                }
            } else if is_function(value) {
                let scope = self.scope(value, value, Callable::Function);
                self.push(facts, ranges, name, scope);
            } else if self.typescript && is_ts_function_like(value) {
                // The adapter mints a function for a wrapper call around a
                // function and attributes every call in the wrapper to it. The
                // wrapped function binds the names only when it is the one.
                let mut wrapped = Vec::new();
                wrapped_functions(value, &mut wrapped);
                if let [function] = wrapped.as_slice() {
                    let scope = self.scope(value, *function, Callable::Function);
                    self.push(facts, ranges, name, scope);
                }
            } else if value.kind() == "object" {
                self.object_methods(value, facts);
            }
        }
    }

    /// `a.b = function () {}`, chained targets included, and
    /// `module.exports = { a() {} }`. Each target names the function a leaf the
    /// adapter may have given it; one no entity carries matches nothing.
    fn assignment(&self, statement: Node<'tree>, facts: &mut FileFacts) {
        let mut cursor = statement.walk();
        let Some(assignment) = statement
            .children(&mut cursor)
            .find(|child| child.kind() == "assignment_expression")
        else {
            return;
        };
        let mut targets = Vec::new();
        let mut current = assignment;
        let value = loop {
            let (Some(left), Some(right)) = (
                current.child_by_field_name("left"),
                current.child_by_field_name("right"),
            ) else {
                return;
            };
            targets.push(left);
            if right.kind() == "assignment_expression" {
                current = right;
            } else {
                break right;
            }
        };
        if is_function(value) {
            let mut leaves = BTreeSet::new();
            if let Some(own) = value.child_by_field_name("name") {
                leaves.insert(leaf_of(text(own, self.source)));
            }
            for target in targets {
                let spelled = match target.kind() {
                    "member_expression" => target
                        .child_by_field_name("property")
                        .map(|property| text(property, self.source))
                        .unwrap_or(""),
                    _ => text(target, self.source),
                };
                leaves.insert(leaf_of(spelled));
            }
            let scope = self.scope(value, value, Callable::Function);
            for leaf in leaves {
                self.push(facts, vec![range(statement)], leaf, scope.clone());
            }
        } else if value.kind() == "object" {
            self.object_methods(value, facts);
        }
    }

    /// The function-valued properties of an object literal, each spanned by
    /// its own function node.
    fn object_methods(&self, object: Node<'tree>, facts: &mut FileFacts) {
        let mut cursor = object.walk();
        for child in object.children(&mut cursor) {
            match child.kind() {
                "method_definition" => {
                    if let Some(name) = child.child_by_field_name("name") {
                        let scope = self.scope(child, child, Callable::Function);
                        self.push(facts, vec![range(child)], text(name, self.source), scope);
                    }
                }
                "pair" => {
                    let (Some(key), Some(value)) = (
                        child.child_by_field_name("key"),
                        child.child_by_field_name("value"),
                    ) else {
                        continue;
                    };
                    if !is_function(value) {
                        continue;
                    }
                    let name = text(key, self.source).trim_matches(['"', '\'', '`']);
                    let scope = self.scope(value, value, Callable::Function);
                    self.push(facts, vec![range(value)], name, scope);
                }
                _ => {}
            }
        }
    }

    /// `export default function () {}` and `export default () => {}`, which
    /// the adapter mints as a function called `default` spanning the export,
    /// and the binding any default export names: `export default class
    /// Store {}`, `export default Store` or `export {Store as default}`.
    fn default_export(&self, statement: Node<'tree>, facts: &mut FileFacts) {
        // Only the file's own top level exports the file's default: one
        // inside `declare module 'x' { ... }` is another module's.
        let top_level = statement
            .parent()
            .is_some_and(|parent| parent.kind() == "program");
        // `export {Store as default}` names a binding of this module; with a
        // `from` clause it names one of another module, which is not read.
        if top_level && statement.child_by_field_name("source").is_none() {
            let mut cursor = statement.walk();
            for clause in statement.named_children(&mut cursor) {
                if !matches!(clause.kind(), "export_clause" | "named_exports") {
                    continue;
                }
                let mut specifiers = clause.walk();
                for specifier in clause.named_children(&mut specifiers) {
                    let (Some(name), Some(alias)) = (
                        specifier.child_by_field_name("name"),
                        specifier.child_by_field_name("alias"),
                    ) else {
                        continue;
                    };
                    if specifier.kind() == "export_specifier"
                        && name.kind() == "identifier"
                        && text(alias, self.source) == "default"
                    {
                        facts
                            .default_exports
                            .push(text(name, self.source).to_string());
                    }
                }
            }
        }
        let mut cursor = statement.walk();
        if !statement
            .children(&mut cursor)
            .any(|child| child.kind() == "default")
        {
            return;
        }
        let mut cursor = statement.walk();
        let value = statement.named_children(&mut cursor).find(|child| {
            !matches!(
                child.kind(),
                "export_clause" | "named_exports" | "decorator"
            )
        });
        let Some(value) = value else {
            return;
        };
        let named = match value.kind() {
            "identifier" => Some(value),
            "class_declaration"
            | "abstract_class_declaration"
            | "class"
            | "interface_declaration"
            | "function_declaration"
            | "generator_function_declaration"
            | "function"
            | "function_expression"
            | "generator_function" => value.child_by_field_name("name"),
            _ => None,
        };
        if top_level {
            match named {
                Some(name) => facts
                    .default_exports
                    .push(text(name, self.source).to_string()),
                // An anonymous default export still takes the one default
                // slot, so no binding of the module is the one an import
                // means.
                None => facts.default_exports.push(String::new()),
            }
        }
        if is_function(value) {
            let scope = self.scope(value, value, Callable::Function);
            self.push(facts, vec![range(statement)], "default", scope);
        }
    }

    fn class(&self, class: Node<'tree>, facts: &mut FileFacts) {
        let Some(body) = class.child_by_field_name("body") else {
            return;
        };
        if self.typescript {
            self.ts_class_body(body, facts);
        } else {
            self.js_class_body(body, facts);
        }
    }

    /// Methods and function-valued fields, as `extract_js_class_like` reads
    /// them: a field's name is its first named part and its value its last.
    fn js_class_body(&self, body: Node<'tree>, facts: &mut FileFacts) {
        let mut cursor = body.walk();
        for member in body.children(&mut cursor) {
            match member.kind() {
                "method_definition" => {
                    if let Some(name) = member.child_by_field_name("name") {
                        let scope = self.scope(member, member, Callable::Method);
                        self.push(facts, vec![range(member)], text(name, self.source), scope);
                    }
                }
                "field_definition" => {
                    let mut walker = member.walk();
                    let parts: Vec<Node<'tree>> = member.named_children(&mut walker).collect();
                    if let (Some(name), Some(value)) = (parts.first(), parts.last()) {
                        if is_function(*value) {
                            let scope = self.scope(member, *value, Callable::Method);
                            self.push(facts, vec![range(member)], text(*name, self.source), scope);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// Class members as `extract_ts_class_like` reads them. An overload
    /// signature collapses onto the implementation, and a bodiless set with no
    /// implementation onto its first signature, whose return then speaks for
    /// only one overload and is not recorded.
    fn ts_class_body(&self, body: Node<'tree>, facts: &mut FileFacts) {
        let mut implemented = HashSet::new();
        let mut signatures: HashMap<&str, usize> = HashMap::new();
        let mut cursor = body.walk();
        for member in body.children(&mut cursor) {
            let Some(name) = member.child_by_field_name("name") else {
                continue;
            };
            let name = text(name, self.source);
            match member.kind() {
                "method_definition" => {
                    implemented.insert(name);
                }
                "method_signature" | "abstract_method_signature" => {
                    *signatures.entry(name).or_default() += 1;
                }
                _ => {}
            }
        }
        let instance = self.instance_members(body);
        let with_members = |member: Node<'tree>, mut scope: ScopeFacts| {
            if !has_token(member, &["static"]) && !rebinds_this(member) {
                for (name, origin) in &instance {
                    scope
                        .bindings
                        .insert(format!("this.{name}"), origin.clone());
                }
            }
            scope
        };
        let mut seen = HashSet::new();
        let mut cursor = body.walk();
        for member in body.children(&mut cursor) {
            let Some(name) = member
                .child_by_field_name("name")
                .map(|name| text(name, self.source))
            else {
                continue;
            };
            match member.kind() {
                "method_signature" | "abstract_method_signature" => {
                    if implemented.contains(name) || !seen.insert(name) {
                        continue;
                    }
                    if signatures.get(name) == Some(&1) {
                        let scope = self.scope(member, member, Callable::Signature);
                        self.push(facts, vec![range(member)], name, scope);
                    }
                }
                "method_definition" => {
                    let scope = with_members(member, self.scope(member, member, Callable::Method));
                    self.push(facts, vec![range(member)], name, scope);
                }
                "public_field_definition" => {
                    let value = member
                        .child_by_field_name("value")
                        .map(effective_value)
                        .filter(|value| is_function(*value));
                    if let Some(value) = value {
                        let scope = self.scope(member, value, Callable::Method);
                        let scope = if value.kind() == "arrow_function" {
                            with_members(member, scope)
                        } else {
                            scope
                        };
                        self.push(facts, vec![range(member)], name, scope);
                    }
                }
                _ => {}
            }
        }
    }

    /// What each instance member of a TypeScript class declares it holds, by
    /// name: a field's annotation, the class its initializer constructs or
    /// injects, or a constructor parameter property's annotation. TypeScript
    /// checks every later assignment against that type, so it holds for every
    /// read. A name the class declares more than once is left out.
    fn instance_members(&self, body: Node<'tree>) -> BTreeMap<String, ValueOrigin> {
        let mut counts: HashMap<String, usize> = HashMap::new();
        let mut members = BTreeMap::new();
        let mut cursor = body.walk();
        for member in body.named_children(&mut cursor) {
            if let Some(name) = member.child_by_field_name("name") {
                *counts
                    .entry(text(name, self.source).to_string())
                    .or_default() += 1;
            }
            match member.kind() {
                "public_field_definition" if !has_token(member, &["static"]) => {
                    let Some(name) = member
                        .child_by_field_name("name")
                        .filter(|name| name.kind() == "property_identifier")
                    else {
                        continue;
                    };
                    let origin = member
                        .child_by_field_name("type")
                        .and_then(|annotation| self.annotation_origin(annotation))
                        .or_else(|| {
                            let value = effective_value(member.child_by_field_name("value")?);
                            self.initializer_origin(value, member)
                        });
                    if let Some(origin) = origin {
                        members.insert(text(name, self.source).to_string(), origin);
                    }
                }
                "method_definition"
                    if member
                        .child_by_field_name("name")
                        .is_some_and(|name| text(name, self.source) == "constructor") =>
                {
                    let Some(parameters) = member.child_by_field_name("parameters") else {
                        continue;
                    };
                    let mut params = parameters.walk();
                    for parameter in parameters.named_children(&mut params) {
                        if !matches!(
                            parameter.kind(),
                            "required_parameter" | "optional_parameter"
                        ) || !declares_property(parameter)
                        {
                            continue;
                        }
                        let (Some(pattern), Some(annotation)) = (
                            parameter
                                .child_by_field_name("pattern")
                                .filter(|pattern| pattern.kind() == "identifier"),
                            parameter.child_by_field_name("type"),
                        ) else {
                            continue;
                        };
                        let name = text(pattern, self.source).to_string();
                        *counts.entry(name.clone()).or_default() += 1;
                        if let Some(origin) = self.annotation_origin(annotation) {
                            members.insert(name, origin);
                        }
                    }
                }
                _ => {}
            }
        }
        members.retain(|name, _| counts.get(name) == Some(&1));
        members
    }

    /// The type TypeScript gives a field from its initializer, when it names
    /// one class: `new Service()`, `inject(Service)`, a built-in collection.
    fn initializer_origin(&self, value: Node<'tree>, member: Node<'tree>) -> Option<ValueOrigin> {
        let census = Census::of(member, self.source);
        match value.kind() {
            "new_expression" => {
                let constructor = value.child_by_field_name("constructor")?;
                let path = dotted_path(constructor, self.source)?;
                if BUILTIN_CONSTRUCTORS.contains(&path.as_str())
                    && self.file.value_unbound(&path, &census)
                {
                    // A later assignment need only fit the type's shape.
                    return Some(structural(&path));
                }
                Some(ValueOrigin::Declared { type_name: path })
            }
            "array" => Some(structural("Array")),
            "call_expression" => self.inject_origin(value, &census).flatten(),
            _ => None,
        }
    }
    /// Interface method signatures: each declares a return and nothing else.
    fn interface(&self, interface: Node<'tree>, facts: &mut FileFacts) {
        let Some(body) = interface.child_by_field_name("body") else {
            return;
        };
        let mut counts: HashMap<&str, usize> = HashMap::new();
        let mut cursor = body.walk();
        for member in body.children(&mut cursor) {
            if matches!(member.kind(), "method_signature" | "property_signature") {
                if let Some(name) = member.child_by_field_name("name") {
                    *counts.entry(text(name, self.source)).or_default() += 1;
                }
            }
        }
        let mut cursor = body.walk();
        for member in body.children(&mut cursor) {
            if member.kind() != "method_signature" {
                continue;
            }
            let Some(name) = member
                .child_by_field_name("name")
                .map(|name| text(name, self.source))
            else {
                continue;
            };
            if counts.get(name) == Some(&1) {
                let scope = self.scope(member, member, Callable::Signature);
                self.push(facts, vec![range(member)], name, scope);
            }
        }
    }

    /// The facts of one callable. `root` is the subtree the adapter attributes
    /// calls from, and `function` the callable whose parameters and body bind
    /// the names: the same node, except for a class field or a wrapper call
    /// around a function.
    fn scope(&self, root: Node<'tree>, function: Node<'tree>, callable: Callable) -> ScopeFacts {
        let mut scope = ScopeFacts {
            returns: function
                .child_by_field_name("return_type")
                .and_then(|annotation| {
                    self.declared_return(annotation, callable != Callable::Function)
                }),
            ..ScopeFacts::default()
        };
        if callable == Callable::Signature {
            return scope;
        }
        let census = Census::of(root, self.source);
        if census.blocked {
            return scope;
        }
        self.parameter_origins(function, &census, &mut scope.bindings);
        if let Some(body) = function.child_by_field_name("body") {
            self.local_origins(body, &census, &mut scope.bindings);
        }
        // A recorded binding is its name's only one in the scope, so the line
        // the census saw it bound on is where it is written.
        scope.binding_lines = scope
            .bindings
            .keys()
            .filter_map(|name| Some((name.clone(), census.line(name)?)))
            .collect();
        scope
    }

    /// `(store: Store)` and `({store}: Props)`. A parameter with a default
    /// still holds a value of its declared type.
    fn parameter_origins(
        &self,
        function: Node<'tree>,
        census: &Census<'a>,
        bindings: &mut BTreeMap<String, ValueOrigin>,
    ) {
        let Some(parameters) = function.child_by_field_name("parameters") else {
            return;
        };
        let mut cursor = parameters.walk();
        for parameter in parameters.named_children(&mut cursor) {
            if !matches!(
                parameter.kind(),
                "required_parameter" | "optional_parameter"
            ) {
                continue;
            }
            let (Some(pattern), Some(annotation)) = (
                parameter.child_by_field_name("pattern"),
                parameter.child_by_field_name("type"),
            ) else {
                continue;
            };
            match pattern.kind() {
                "identifier" => {
                    let name = text(pattern, self.source);
                    if !census.single(name, function) {
                        continue;
                    }
                    if let Some(origin) = self
                        .annotation_origin(annotation)
                        .and_then(|origin| census.unpatched(name, origin))
                    {
                        bindings.insert(name.to_string(), origin);
                    }
                }
                "object_pattern" => {
                    self.pattern_origins(pattern, annotation, function, census, bindings);
                }
                _ => {}
            }
        }
    }

    /// Every `var`, `let` and `const` the body declares outside a nested scope.
    fn local_origins(
        &self,
        body: Node<'tree>,
        census: &Census<'a>,
        bindings: &mut BTreeMap<String, ValueOrigin>,
    ) {
        let mut stack = vec![body];
        while let Some(node) = stack.pop() {
            if node.id() != body.id() && opens_scope(node) {
                continue;
            }
            // `var` is visible in the whole body; `let` and `const` only in
            // the block, loop or case that declares them.
            let region = match node.kind() {
                "variable_declaration" => Some(body),
                "lexical_declaration" => node.parent(),
                _ => None,
            };
            if let Some(region) = region {
                self.declaration_origins(node, region, census, bindings);
            }
            let mut cursor = node.walk();
            stack.extend(node.named_children(&mut cursor));
        }
    }

    fn declaration_origins(
        &self,
        declaration: Node<'tree>,
        region: Node<'tree>,
        census: &Census<'a>,
        bindings: &mut BTreeMap<String, ValueOrigin>,
    ) {
        let mut cursor = declaration.walk();
        for declarator in declaration.named_children(&mut cursor) {
            if declarator.kind() != "variable_declarator" {
                continue;
            }
            let Some(name) = declarator.child_by_field_name("name") else {
                continue;
            };
            let annotation = declarator.child_by_field_name("type");
            match name.kind() {
                "identifier" => {
                    let spelled = text(name, self.source);
                    if !census.single(spelled, region) {
                        continue;
                    }
                    let declared =
                        annotation.and_then(|annotation| self.annotation_origin(annotation));
                    let origin = declared.or_else(|| {
                        declarator
                            .child_by_field_name("value")
                            .and_then(|value| self.expression_origin(value, census))
                    });
                    if let Some(origin) =
                        origin.and_then(|origin| census.unpatched(spelled, origin))
                    {
                        bindings.insert(spelled.to_string(), origin);
                    }
                }
                "object_pattern" => {
                    if let Some(annotation) = annotation {
                        self.pattern_origins(name, annotation, region, census, bindings);
                    }
                }
                _ => {}
            }
        }
    }

    /// The locals an object pattern binds, typed by the properties of the
    /// object type its annotation names. `{a}` binds `a` to property `a`,
    /// `{b: local}` binds `local` to property `b`, and an entry with a default
    /// may hold the default instead, so it gets nothing.
    fn pattern_origins(
        &self,
        pattern: Node<'tree>,
        annotation: Node<'tree>,
        region: Node<'tree>,
        census: &Census<'a>,
        bindings: &mut BTreeMap<String, ValueOrigin>,
    ) {
        let Some(object) = sole_named(annotation).and_then(|ty| self.object_type(ty, 0)) else {
            return;
        };
        let mut cursor = pattern.walk();
        for entry in pattern.named_children(&mut cursor) {
            let (property, local) = match entry.kind() {
                "shorthand_property_identifier_pattern" => (text(entry, self.source), entry),
                "pair_pattern" => {
                    let (Some(key), Some(value)) = (
                        entry.child_by_field_name("key"),
                        entry.child_by_field_name("value"),
                    ) else {
                        continue;
                    };
                    if key.kind() != "property_identifier" || value.kind() != "identifier" {
                        continue;
                    }
                    (text(key, self.source), value)
                }
                _ => continue,
            };
            let local = text(local, self.source);
            if !census.single(local, region) {
                continue;
            }
            if let Some(type_name) = property_type(object, property, self.source) {
                bindings.insert(local.to_string(), ValueOrigin::Declared { type_name });
            }
        }
    }

    /// The object type `ty` spells: inline, through a same-file alias or
    /// interface, or inside `$ReadOnly<...>`, `$Exact<...>` or `Readonly<...>`.
    fn object_type(&self, ty: Node<'tree>, depth: usize) -> Option<Node<'tree>> {
        if depth > ALIAS_DEPTH {
            return None;
        }
        match ty.kind() {
            "object_type" | "interface_body" => Some(ty),
            "parenthesized_type" => self.object_type(sole_named(ty)?, depth + 1),
            "generic_type" => {
                let name = ty.child_by_field_name("name")?;
                if !OBJECT_WRAPPERS.contains(&text(name, self.source)) {
                    return None;
                }
                let argument = sole_named(ty.child_by_field_name("type_arguments")?)?;
                self.object_type(argument, depth + 1)
            }
            "type_identifier" => {
                let declared = *self.file.object_types.get(text(ty, self.source))?;
                self.object_type(declared, depth + 1)
            }
            _ => None,
        }
    }

    /// The origin of the value an expression evaluates to, when it is one the
    /// linker can follow.
    fn expression_origin(
        &self,
        expression: Node<'tree>,
        census: &Census<'a>,
    ) -> Option<ValueOrigin> {
        match expression.kind() {
            "parenthesized_expression" => {
                // `(value: T)` is a Flow cast, which asserts a type the way
                // `as` does rather than declaring one.
                if expression.child_by_field_name("type").is_some() {
                    return None;
                }
                self.expression_origin(sole_named(expression)?, census)
            }
            "identifier" => {
                let name = text(expression, self.source);
                (!NOT_BINDINGS.contains(&name)).then(|| ValueOrigin::Local {
                    name: name.to_string(),
                })
            }
            "new_expression" => {
                let constructor = expression.child_by_field_name("constructor")?;
                let path = dotted_path(constructor, self.source)?;
                let root = path.split('.').next()?;
                // A constructor the scope binds itself is a local value, not
                // the file-level name the linker would resolve.
                if census.count(root) > 0 || NOT_BINDINGS.contains(&root) {
                    return None;
                }
                if BUILTIN_CONSTRUCTORS.contains(&path.as_str())
                    && self.file.value_unbound(&path, census)
                {
                    return Some(exact(&path));
                }
                Some(ValueOrigin::Call { callee: path })
            }
            "call_expression" => self.call_origin(expression, census),
            "array" => Some(exact("Array")),
            "string" | "template_string" => Some(exact("String")),
            "object" => object_literal(expression, self.source),
            _ => None,
        }
    }

    /// What an annotation declares a value to be: a class the linker locates,
    /// or a built-in type the repository does not declare.
    fn annotation_origin(&self, annotation: Node<'_>) -> Option<ValueOrigin> {
        let ty = match annotation.kind() {
            "type_annotation" => sole_named(annotation)?,
            _ => annotation,
        };
        self.type_origin(ty)
    }

    fn type_origin(&self, ty: Node<'_>) -> Option<ValueOrigin> {
        match ty.kind() {
            "parenthesized_type" | "flow_maybe_type" => self.type_origin(sole_named(ty)?),
            "union_type" => {
                let mut members = Vec::new();
                union_members(ty, &mut members);
                let named: Vec<Node<'_>> = members
                    .into_iter()
                    .filter(|member| !is_nullish(*member, self.source))
                    .collect();
                match named.as_slice() {
                    [one] => self.type_origin(*one),
                    _ => None,
                }
            }
            "predefined_type" => {
                let spelled = text(ty, self.source).trim();
                let (_, class) = PRIMITIVE_TYPES.iter().find(|(name, _)| *name == spelled)?;
                Some(exact(class))
            }
            "array_type" => Some(structural("Array")),
            "readonly_type" => {
                let inner = sole_named(ty)?;
                (inner.kind() == "array_type").then(|| structural("Array"))
            }
            "generic_type" => {
                let name = text(ty.child_by_field_name("name")?, self.source);
                let (_, class) = STRUCTURAL_COLLECTIONS
                    .iter()
                    .find(|(spelled, _)| *spelled == name)?;
                self.file.type_unbound(name).then(|| structural(class))
            }
            "type_identifier" => {
                let spelled = text(ty, self.source);
                if let Some((_, class)) = STRUCTURAL_COLLECTIONS
                    .iter()
                    .find(|(name, _)| *name == spelled)
                {
                    return self.file.type_unbound(spelled).then(|| structural(class));
                }
                normalize_type(ty, self.source).map(|type_name| ValueOrigin::Declared { type_name })
            }
            _ => {
                normalize_type(ty, self.source).map(|type_name| ValueOrigin::Declared { type_name })
            }
        }
    }

    /// A callable's declared return. A member declared to return `this`
    /// returns its receiver.
    fn declared_return(&self, annotation: Node<'_>, member: bool) -> Option<DeclaredReturn> {
        // A type predicate or an assertion signature says nothing about a value.
        if annotation.kind() != "type_annotation" {
            return None;
        }
        let ty = sole_named(annotation)?;
        if member && ty.kind() == "this_type" {
            return Some(DeclaredReturn::Receiver);
        }
        match self.type_origin(ty)? {
            ValueOrigin::Declared { type_name } => Some(DeclaredReturn::Type { type_name }),
            ValueOrigin::External {
                type_name, reach, ..
            } => Some(DeclaredReturn::External { type_name, reach }),
            _ => None,
        }
    }

    /// What `inject(token)` answers with, through an import of Angular's or
    /// Vue's `inject`: the type argument when the call writes one, the value a
    /// typed key provides, or for Angular an instance of the class the token
    /// names.
    ///
    /// `None` when the call is not such an `inject`; `Some(None)` when it is
    /// and the reader cannot type what it answers with.
    fn inject_origin(&self, call: Node<'tree>, census: &Census<'a>) -> Option<Option<ValueOrigin>> {
        let function = call.child_by_field_name("function")?;
        if function.kind() != "identifier" {
            return None;
        }
        let callee = text(function, self.source);
        if census.count(callee) > 0 {
            return None;
        }
        let angular = self.file.angular_inject.contains(callee);
        if !angular && !self.file.vue_inject.contains(callee) {
            return None;
        }
        Some(self.injected(call, census, angular))
    }

    fn injected(
        &self,
        call: Node<'tree>,
        census: &Census<'a>,
        angular: bool,
    ) -> Option<ValueOrigin> {
        if let Some(arguments) = call.child_by_field_name("type_arguments") {
            return self.type_origin(sole_named(arguments)?);
        }
        let arguments = call.child_by_field_name("arguments")?;
        let mut cursor = arguments.walk();
        let token = arguments
            .named_children(&mut cursor)
            .find(|argument| argument.kind() != "comment")?;
        if token.kind() != "identifier" {
            return None;
        }
        let token = text(token, self.source);
        if census.count(token) > 0 {
            return None;
        }
        if !angular || self.file.module_bindings.contains(token) && self.declares_key(token) {
            return Some(ValueOrigin::Provided {
                lookup: ProviderLookup::Binding {
                    kind: INJECTION_KEY_KIND.into(),
                },
                name: token.to_string(),
            });
        }
        Some(ValueOrigin::Declared {
            type_name: token.to_string(),
        })
    }

    /// Whether the module declares `name` as a typed injection key.
    fn declares_key(&self, name: &str) -> bool {
        self.file.injection_keys.contains(name)
    }

    fn call_origin(&self, call: Node<'tree>, census: &Census<'a>) -> Option<ValueOrigin> {
        // `f?.()` may be `undefined`, and a tagged template is not a call to
        // the value it names.
        if has_optional_chain(call) {
            return None;
        }
        let arguments = call
            .child_by_field_name("arguments")
            .filter(|arguments| arguments.kind() == "arguments")?;
        let function = call.child_by_field_name("function")?;
        if self.names_react(function, "useContext", &self.file.use_context, Some(census)) {
            let context = sole_named(arguments).filter(|node| node.kind() == "identifier")?;
            let name = text(context, self.source);
            if census.count(name) > 0 {
                return None;
            }
            return Some(ValueOrigin::Provided {
                lookup: ProviderLookup::Binding {
                    kind: REACT_CONTEXT_KIND.into(),
                },
                name: name.to_string(),
            });
        }
        // A `useContext` an import of React does not reach could return
        // anything at all.
        if spells_member(function, "useContext", self.source) {
            return None;
        }
        if let Some(injected) = self.inject_origin(call, census) {
            return injected;
        }
        if let Some(path) = dotted_path(function, self.source) {
            let root = path.split('.').next().unwrap_or_default();
            if ARRAY_FACTORIES.contains(&path.as_str()) && self.file.value_unbound(root, census) {
                return Some(exact("Array"));
            }
        }
        match function.kind() {
            "identifier" => {
                let callee = text(function, self.source);
                if census.count(callee) > 0 || NOT_BINDINGS.contains(&callee) {
                    return None;
                }
                Some(ValueOrigin::Call {
                    callee: callee.to_string(),
                })
            }
            "member_expression" => {
                if has_optional_chain(function) {
                    return None;
                }
                let method = function
                    .child_by_field_name("property")
                    .filter(|property| property.kind() == "property_identifier")?;
                // `ns.connect()` through `import * as ns` calls a module's
                // export by its qualified name rather than a method on a value.
                if let Some(path) = dotted_path(function, self.source) {
                    let root = path.split('.').next().unwrap_or_default();
                    if self.file.namespaces.contains(root) && census.count(root) == 0 {
                        return Some(ValueOrigin::Call { callee: path });
                    }
                }
                let receiver =
                    self.expression_origin(function.child_by_field_name("object")?, census)?;
                Some(ValueOrigin::MethodCall {
                    receiver: Box::new(receiver),
                    method: text(method, self.source).to_string(),
                })
            }
            _ => None,
        }
    }

    /// Whether `callee` names React's `export` through an import of `'react'`:
    /// a name `named` holds, or a member of React's module object. With a
    /// census, a name the scope rebinds does not count.
    fn names_react(
        &self,
        callee: Node<'_>,
        export: &str,
        named: &HashSet<String>,
        census: Option<&Census<'_>>,
    ) -> bool {
        let unbound = |name: &str| census.is_none_or(|census| census.count(name) == 0);
        match callee.kind() {
            "identifier" => {
                let name = text(callee, self.source);
                named.contains(name) && unbound(name)
            }
            "member_expression" => {
                if has_optional_chain(callee) {
                    return false;
                }
                let (Some(object), Some(property)) = (
                    callee.child_by_field_name("object"),
                    callee.child_by_field_name("property"),
                ) else {
                    return false;
                };
                let object = text(object, self.source);
                self.file.react_objects.contains(object)
                    && unbound(object)
                    && text(property, self.source) == export
            }
            _ => false,
        }
    }

    /// `const C = createContext<T>(...)` at the top level, exported or not.
    fn module_providers(&self, statement: Node<'tree>, provides: &mut Vec<ValueProvider>) {
        let declaration = match statement.kind() {
            "export_statement" => statement.child_by_field_name("declaration"),
            _ => Some(statement),
        };
        let Some(declaration) = declaration.filter(|node| node.kind() == "lexical_declaration")
        else {
            return;
        };
        // A `let` could be reassigned to another context.
        if declaration
            .child_by_field_name("kind")
            .map(|kind| text(kind, self.source))
            != Some("const")
        {
            return;
        }
        let mut cursor = declaration.walk();
        for declarator in declaration.named_children(&mut cursor) {
            if declarator.kind() == "variable_declarator" {
                provides.extend(self.context_provider(declarator));
                provides.extend(self.injection_key_provider(declarator));
            }
        }
    }

    /// The typed key one declarator creates, with the type the value injected
    /// under it has: `const K: InjectionKey<T> = Symbol()` (Vue) or
    /// `const K = new InjectionToken<T>(...)` (Angular).
    fn injection_key_provider(&self, declarator: Node<'tree>) -> Option<ValueProvider> {
        let name = declarator
            .child_by_field_name("name")
            .filter(|name| name.kind() == "identifier")?;
        let spelled = text(name, self.source);
        if !self.file.injection_keys.contains(spelled) {
            return None;
        }
        let from_annotation = declarator
            .child_by_field_name("type")
            .and_then(|annotation| injection_key_argument(annotation, self.source));
        let from_constructor = declarator
            .child_by_field_name("value")
            .map(effective_value)
            .filter(|value| value.kind() == "new_expression")
            .and_then(|value| value.child_by_field_name("type_arguments"))
            .and_then(sole_named);
        let ty = match (from_annotation, from_constructor) {
            (Some(one), None) | (None, Some(one)) => Some(one),
            (Some(annotation), Some(constructor)) => (text(annotation, self.source)
                == text(constructor, self.source))
            .then_some(annotation),
            (None, None) => None,
        };
        Some(ValueProvider {
            kind: INJECTION_KEY_KIND.into(),
            name: spelled.to_string(),
            owner: None,
            value: ty.and_then(|ty| self.type_origin(ty)),
            every_name: false,
        })
    }

    /// The context one declarator creates, with the type it is created with:
    /// the call's type argument or the `ReactContext<T>` annotation's, and
    /// none when they disagree or neither names one type.
    fn context_provider(&self, declarator: Node<'tree>) -> Option<ValueProvider> {
        let name = declarator
            .child_by_field_name("name")
            .filter(|name| name.kind() == "identifier")?;
        let call = declarator
            .child_by_field_name("value")
            .filter(|value| value.kind() == "call_expression")?;
        if has_optional_chain(call) {
            return None;
        }
        let callee = call.child_by_field_name("function")?;
        if !self.names_react(callee, "createContext", &self.file.create_context, None) {
            return None;
        }
        let from_call = call
            .child_by_field_name("type_arguments")
            .map(|arguments| sole_type_argument(arguments, self.source));
        let from_annotation = declarator
            .child_by_field_name("type")
            .and_then(|annotation| context_annotation(annotation, self.source));
        let type_name = match (from_call, from_annotation) {
            (Some(call), Some(annotation)) => call.filter(|call| Some(call) == annotation.as_ref()),
            (Some(one), None) | (None, Some(one)) => one,
            (None, None) => None,
        };
        Some(ValueProvider {
            kind: REACT_CONTEXT_KIND.into(),
            name: text(name, self.source).to_string(),
            owner: None,
            value: type_name.map(|type_name| ValueOrigin::Declared { type_name }),
            every_name: false,
        })
    }
}

/// `T` of an annotation `ReactContext<T>`, `React.Context<T>` or any other
/// generic whose name ends in `Context`: `Some(None)` when that is the shape
/// but its argument names no single type.
fn context_annotation(annotation: Node<'_>, source: &[u8]) -> Option<Option<String>> {
    let ty = sole_named(annotation).filter(|ty| ty.kind() == "generic_type")?;
    let name = text(ty.child_by_field_name("name")?, source);
    if !name.rsplit('.').next().unwrap_or(name).ends_with("Context") {
        return None;
    }
    Some(sole_type_argument(
        ty.child_by_field_name("type_arguments")?,
        source,
    ))
}

fn sole_type_argument(arguments: Node<'_>, source: &[u8]) -> Option<String> {
    normalize_type(sole_named(arguments)?, source)
}

/// The type of property `property` in an object type, when it declares the
/// property exactly once, as a plain property.
fn property_type(object: Node<'_>, property: &str, source: &[u8]) -> Option<String> {
    let mut found = None;
    let mut cursor = object.walk();
    for member in object.named_children(&mut cursor) {
        let Some(name) = member.child_by_field_name("name") else {
            continue;
        };
        if text(name, source) != property {
            continue;
        }
        if member.kind() != "property_signature" || found.is_some() {
            return None;
        }
        found = Some(member);
    }
    annotation_type(found?.child_by_field_name("type")?, source)
}

/// `T` of an annotation `InjectionKey<T>` or `InjectionToken<T>`.
fn injection_key_argument<'t>(annotation: Node<'t>, source: &[u8]) -> Option<Node<'t>> {
    let ty = sole_named(annotation).filter(|ty| ty.kind() == "generic_type")?;
    let name = text(ty.child_by_field_name("name")?, source);
    let leaf = name.rsplit('.').next().unwrap_or(name);
    if leaf != "InjectionKey" && leaf != "InjectionToken" {
        return None;
    }
    sole_named(ty.child_by_field_name("type_arguments")?)
}

/// Whether a member's body holds a function or class that gives `this` a
/// value of its own, so a `this.x` read inside it may not be the instance's.
fn rebinds_this(member: Node<'_>) -> bool {
    let mut stack = Vec::new();
    let mut cursor = member.walk();
    stack.extend(member.named_children(&mut cursor));
    while let Some(node) = stack.pop() {
        if matches!(
            node.kind(),
            "function_expression"
                | "function"
                | "generator_function"
                | "function_declaration"
                | "generator_function_declaration"
                | "class"
                | "class_declaration"
                | "abstract_class_declaration"
                | "method_definition"
        ) {
            return true;
        }
        let mut walker = node.walk();
        stack.extend(node.named_children(&mut walker));
    }
    false
}

/// Whether a constructor parameter declares a property of the instance: it
/// carries an accessibility modifier or `readonly`.
fn declares_property(parameter: Node<'_>) -> bool {
    let mut cursor = parameter.walk();
    let found = parameter
        .children(&mut cursor)
        .any(|child| matches!(child.kind(), "accessibility_modifier" | "readonly"));
    found
}

/// The one type a `: T` annotation names.
fn annotation_type(annotation: Node<'_>, source: &[u8]) -> Option<String> {
    let ty = match annotation.kind() {
        "type_annotation" => sole_named(annotation)?,
        _ => annotation,
    };
    normalize_type(ty, source)
}

/// A value whose class is exactly the built-in `class`.
fn exact(class: &str) -> ValueOrigin {
    ValueOrigin::External {
        type_name: class.to_string(),
        reach: ExternalReach::Exact,
        own_members: Vec::new(),
    }
}

/// A value shaped like the built-in `class`, as a declared type says.
fn structural(class: &str) -> ValueOrigin {
    ValueOrigin::External {
        type_name: class.to_string(),
        reach: ExternalReach::Structural,
        own_members: Vec::new(),
    }
}

/// An object literal: exactly an `Object`, holding the members its keys
/// write. `None` when the literal can hold a member no key spells: a spread,
/// a computed key, or a `__proto__` key that replaces its prototype.
fn object_literal(object: Node<'_>, source: &[u8]) -> Option<ValueOrigin> {
    let mut members = Vec::new();
    let mut cursor = object.walk();
    for entry in object.named_children(&mut cursor) {
        let key = match entry.kind() {
            "comment" => continue,
            "pair" | "method_definition" => entry.child_by_field_name(match entry.kind() {
                "pair" => "key",
                _ => "name",
            })?,
            "shorthand_property_identifier" => entry,
            _ => return None,
        };
        let spelled = match key.kind() {
            "property_identifier" | "shorthand_property_identifier" | "identifier" => {
                text(key, source).to_string()
            }
            "string" => string_value(key, source).to_string(),
            "number" => text(key, source).to_string(),
            _ => return None,
        };
        if spelled == "__proto__" {
            return None;
        }
        members.push(spelled);
    }
    members.sort();
    members.dedup();
    Some(ValueOrigin::External {
        type_name: "Object".into(),
        reach: ExternalReach::Exact,
        own_members: members,
    })
}

/// A type name or dotted name, through parentheses, Flow's `?T`, and a union
/// with `null`, `undefined` or `void`. `None` for every other shape, a union of
/// two real types above all, and for a primitive or a type that admits any
/// object, because none of them says which class the value is.
fn normalize_type(ty: Node<'_>, source: &[u8]) -> Option<String> {
    match ty.kind() {
        "parenthesized_type" | "flow_maybe_type" => normalize_type(sole_named(ty)?, source),
        "union_type" => {
            let mut members = Vec::new();
            union_members(ty, &mut members);
            let named: Vec<Node<'_>> = members
                .into_iter()
                .filter(|member| !is_nullish(*member, source))
                .collect();
            match named.as_slice() {
                [one] => normalize_type(*one, source),
                _ => None,
            }
        }
        "type_identifier" | "nested_type_identifier" => {
            let spelled: String = text(ty, source).split_whitespace().collect();
            let plain = spelled.split('.').all(is_identifier);
            (plain && !OPAQUE_TYPES.contains(&spelled.as_str())).then_some(spelled)
        }
        _ => None,
    }
}

fn union_members<'t>(ty: Node<'t>, out: &mut Vec<Node<'t>>) {
    let mut cursor = ty.walk();
    for member in ty.named_children(&mut cursor) {
        match member.kind() {
            "comment" => {}
            "union_type" => union_members(member, out),
            _ => out.push(member),
        }
    }
}

fn is_nullish(ty: Node<'_>, source: &[u8]) -> bool {
    matches!(
        ty.kind(),
        "literal_type" | "predefined_type" | "type_identifier"
    ) && matches!(text(ty, source).trim(), "null" | "undefined" | "void")
}

/// How many times each name is bound anywhere in one callable's subtree, and
/// where every identifier that could read one of them sits.
struct Census<'a> {
    counts: HashMap<&'a str, usize>,
    /// The line of the file each name is first bound on.
    lines: HashMap<&'a str, u32>,
    references: HashMap<&'a str, Vec<(usize, usize)>>,
    /// Names whose value the scope writes a member of: by a named member
    /// (`x.pop = f`, `Object.assign(x, ...)`) or by a computed one (`x[k] = f`).
    member_writes: HashMap<&'a str, (bool, bool)>,
    /// `eval` or `with` can bind a name no syntax shows, and a write through
    /// `arguments[i]` can replace a parameter's value.
    blocked: bool,
}

impl<'a> Census<'a> {
    fn of(root: Node<'_>, source: &'a [u8]) -> Self {
        let mut census = Self {
            counts: HashMap::new(),
            lines: HashMap::new(),
            references: HashMap::new(),
            member_writes: HashMap::new(),
            blocked: false,
        };
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            census.visit(node, root, source);
            let mut cursor = node.walk();
            stack.extend(node.named_children(&mut cursor));
        }
        census
    }

    fn count(&self, name: &str) -> usize {
        self.counts.get(name).copied().unwrap_or(0)
    }

    /// `origin`, unless it is a value of an external type whose members the
    /// scope may replace: a named member written on it, or any member
    /// written on an object literal, can put a repository function where the
    /// type's own method was.
    fn unpatched(&self, name: &str, origin: ValueOrigin) -> Option<ValueOrigin> {
        let ValueOrigin::External { type_name, .. } = &origin else {
            return Some(origin);
        };
        match self.member_writes.get(name) {
            Some((true, _)) => None,
            Some((_, true)) if type_name == "Object" => None,
            _ => Some(origin),
        }
    }

    fn write_member(&mut self, target: Node<'_>, source: &'a [u8]) {
        let (object, named) = match target.kind() {
            "member_expression" => (target.child_by_field_name("object"), true),
            "subscript_expression" => (target.child_by_field_name("object"), false),
            _ => return,
        };
        let Some(object) = object.filter(|object| object.kind() == "identifier") else {
            return;
        };
        let entry = self.member_writes.entry(text(object, source)).or_default();
        if named {
            entry.0 = true;
        } else {
            entry.1 = true;
        }
    }

    /// Whether `name` is bound once and every identifier spelling it sits
    /// inside `region`, where that one binding is the one it reads.
    fn single(&self, name: &str, region: Node<'_>) -> bool {
        self.count(name) == 1
            && self.references.get(name).is_none_or(|sites| {
                sites
                    .iter()
                    .all(|(start, end)| *start >= region.start_byte() && *end <= region.end_byte())
            })
    }

    fn bind(&mut self, name: &'a str, at: Node<'_>) {
        *self.counts.entry(name).or_default() += 1;
        let line = u32::try_from(at.start_position().row).unwrap_or(u32::MAX);
        self.lines
            .entry(name)
            .and_modify(|first| *first = (*first).min(line))
            .or_insert(line);
    }

    /// The line of the file `name` is first bound on, when the scope binds it.
    fn line(&self, name: &str) -> Option<u32> {
        self.lines.get(name).copied()
    }

    fn visit(&mut self, node: Node<'_>, root: Node<'_>, source: &'a [u8]) {
        match node.kind() {
            "identifier" | "shorthand_property_identifier" => {
                self.references
                    .entry(text(node, source))
                    .or_default()
                    .push((node.start_byte(), node.end_byte()));
            }
            "with_statement" => self.blocked = true,
            "call_expression" => {
                if node
                    .child_by_field_name("function")
                    .is_some_and(|function| {
                        function.kind() == "identifier" && text(function, source) == "eval"
                    })
                {
                    self.blocked = true;
                }
                // `Object.assign(x, ...)` and its kin write members of `x`.
                let patches = node
                    .child_by_field_name("function")
                    .is_some_and(|function| {
                        matches!(
                            text(function, source),
                            "Object.assign"
                                | "Object.defineProperty"
                                | "Object.defineProperties"
                                | "Object.setPrototypeOf"
                                | "Reflect.set"
                                | "Reflect.defineProperty"
                                | "Reflect.setPrototypeOf"
                        )
                    });
                if patches {
                    let first = node.child_by_field_name("arguments").and_then(|arguments| {
                        let mut cursor = arguments.walk();
                        let first = arguments
                            .named_children(&mut cursor)
                            .find(|argument| argument.kind() != "comment");
                        first
                    });
                    if let Some(first) = first.filter(|first| first.kind() == "identifier") {
                        self.member_writes.entry(text(first, source)).or_default().0 = true;
                    }
                }
            }
            // A declaration binds its name in the scope around it, which for
            // the root itself lies outside the subtree.
            "function_declaration"
            | "generator_function_declaration"
            | "class_declaration"
            | "abstract_class_declaration" => {
                if node.id() != root.id() {
                    self.bind_field(node, "name", source);
                }
            }
            // A named function or class expression binds its name inside
            // itself.
            "function_expression"
            | "function"
            | "generator_function"
            | "class"
            | "enum_declaration"
            | "internal_module" => self.bind_field(node, "name", source),
            "variable_declarator" => self.targets_field(node, "name", source),
            "catch_clause" => self.targets_field(node, "parameter", source),
            "for_in_statement" => self.targets_field(node, "left", source),
            "assignment_expression" | "augmented_assignment_expression" => {
                if let Some(left) = node.child_by_field_name("left") {
                    if writes_arguments(left, source) {
                        self.blocked = true;
                    }
                    self.write_member(left, source);
                    self.targets(left, source);
                }
            }
            "update_expression" => self.targets_field(node, "argument", source),
            "formal_parameters" => {
                let mut cursor = node.walk();
                for parameter in node.named_children(&mut cursor) {
                    self.targets(parameter, source);
                }
            }
            "arrow_function" => self.targets_field(node, "parameter", source),
            _ => {}
        }
    }

    fn bind_field(&mut self, node: Node<'_>, field: &str, source: &'a [u8]) {
        if let Some(name) = node.child_by_field_name(field) {
            self.bind(text(name, source), name);
        }
    }

    fn targets_field(&mut self, node: Node<'_>, field: &str, source: &'a [u8]) {
        if let Some(target) = node.child_by_field_name(field) {
            self.targets(target, source);
        }
    }

    /// Every name a binding target holds, counted once each. A member or
    /// subscript target binds no name, and a default is an expression the
    /// walk reaches on its own.
    fn targets(&mut self, target: Node<'_>, source: &'a [u8]) {
        match target.kind() {
            "identifier" | "shorthand_property_identifier_pattern" => {
                self.bind(text(target, source), target);
            }
            "pair_pattern" => self.targets_field(target, "value", source),
            "assignment_pattern" | "object_assignment_pattern" => {
                self.targets_field(target, "left", source);
            }
            "required_parameter" | "optional_parameter" => {
                self.targets_field(target, "pattern", source);
            }
            "object_pattern"
            | "array_pattern"
            | "rest_pattern"
            | "parenthesized_expression"
            | "non_null_expression" => {
                let mut cursor = target.walk();
                for child in target.named_children(&mut cursor) {
                    self.targets(child, source);
                }
            }
            _ => {}
        }
    }
}

/// `arguments[i] = ...`, which rebinds a parameter in a sloppy-mode function.
fn writes_arguments(left: Node<'_>, source: &[u8]) -> bool {
    left.kind() == "subscript_expression"
        && left
            .child_by_field_name("object")
            .is_some_and(|object| text(object, source) == "arguments")
}

/// A node whose own bindings belong to a scope nested in the one being read.
fn opens_scope(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "function_declaration"
            | "generator_function_declaration"
            | "function_expression"
            | "function"
            | "generator_function"
            | "arrow_function"
            | "method_definition"
            | "class_body"
            | "class_static_block"
    )
}

/// A function literal: the value kinds the adapter mints a function for.
fn is_function(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "function_expression" | "function" | "arrow_function" | "generator_function"
    )
}

/// The TypeScript extractor's test for a function-valued binding, which also
/// takes a wrapper call around a function (`memo(...)`, `forwardRef(...)`,
/// `styled('div')(...)`).
fn is_ts_function_like(node: Node<'_>) -> bool {
    match node.kind() {
        "arrow_function" | "function" | "function_expression" | "generator_function" => true,
        "call_expression" => {
            let Some(function) = node.child_by_field_name("function") else {
                return false;
            };
            if let Some(arguments) = node.child_by_field_name("arguments") {
                let mut cursor = arguments.walk();
                let wraps = arguments
                    .children(&mut cursor)
                    .any(|argument| argument.is_named() && is_ts_function_like(argument));
                if wraps {
                    return true;
                }
            }
            function.kind() == "call_expression" && is_ts_function_like(function)
        }
        _ => false,
    }
}

/// Every function literal a wrapper call wraps, by the same recursion
/// [`is_ts_function_like`] takes.
fn wrapped_functions<'t>(node: Node<'t>, out: &mut Vec<Node<'t>>) {
    if is_function(node) {
        out.push(node);
        return;
    }
    if node.kind() != "call_expression" {
        return;
    }
    if let Some(arguments) = node.child_by_field_name("arguments") {
        let mut cursor = arguments.walk();
        for argument in arguments.named_children(&mut cursor) {
            if is_ts_function_like(argument) {
                wrapped_functions(argument, out);
            }
        }
    }
    if let Some(function) = node
        .child_by_field_name("function")
        .filter(|function| function.kind() == "call_expression" && is_ts_function_like(*function))
    {
        wrapped_functions(function, out);
    }
}

/// The expression a declarator binds with TypeScript's type-only and grouping
/// wrappers removed, as the TypeScript extractor reads it.
fn effective_value(node: Node<'_>) -> Node<'_> {
    let mut current = node;
    loop {
        let inner = match current.kind() {
            "as_expression"
            | "satisfies_expression"
            | "parenthesized_expression"
            | "non_null_expression" => current.named_child(0),
            _ => None,
        };
        match inner {
            Some(next) => current = next,
            None => return current,
        }
    }
}

/// `a.b.c` when the expression is identifiers joined by plain member access.
fn dotted_path(node: Node<'_>, source: &[u8]) -> Option<String> {
    match node.kind() {
        "identifier" => Some(text(node, source).to_string()),
        "member_expression" => {
            if has_optional_chain(node) {
                return None;
            }
            let property = node
                .child_by_field_name("property")
                .filter(|property| property.kind() == "property_identifier")?;
            let object = dotted_path(node.child_by_field_name("object")?, source)?;
            Some(format!("{object}.{}", text(property, source)))
        }
        _ => None,
    }
}

/// Whether `callee` spells `name` itself or a member called `name`.
fn spells_member(callee: Node<'_>, name: &str, source: &[u8]) -> bool {
    match callee.kind() {
        "identifier" => text(callee, source) == name,
        "member_expression" => callee
            .child_by_field_name("property")
            .is_some_and(|property| text(property, source) == name),
        _ => false,
    }
}

fn has_optional_chain(node: Node<'_>) -> bool {
    let mut cursor = node.walk();
    let found = node
        .children(&mut cursor)
        .any(|child| matches!(child.kind(), "optional_chain" | "?."));
    found
}

/// Whether one of `node`'s anonymous tokens is one of `tokens`.
fn has_token(node: Node<'_>, tokens: &[&str]) -> bool {
    let mut cursor = node.walk();
    let found = node
        .children(&mut cursor)
        .any(|child| !child.is_named() && tokens.contains(&child.kind()));
    found
}

/// The one named child of `node` that is not a comment.
fn sole_named(node: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = node.walk();
    let mut children = node
        .named_children(&mut cursor)
        .filter(|child| child.kind() != "comment");
    let first = children.next()?;
    children.next().is_none().then_some(first)
}

/// The last `.` or `:` segment of an entity name, which is what an entity is
/// matched on.
fn leaf_of(name: &str) -> &str {
    name.rsplit(['.', ':']).next().unwrap_or(name)
}

fn range(node: Node<'_>) -> (usize, usize) {
    (node.start_byte(), node.end_byte())
}

fn string_value<'a>(node: Node<'_>, source: &'a [u8]) -> &'a str {
    text(node, source).trim_matches(['"', '\''])
}

fn is_identifier(segment: &str) -> bool {
    let mut chars = segment.chars();
    chars
        .next()
        .is_some_and(|first| first.is_alphabetic() || first == '_' || first == '$')
        && chars.all(|ch| ch.is_alphanumeric() || ch == '_' || ch == '$')
}

fn text<'a>(node: Node<'_>, source: &'a [u8]) -> &'a str {
    node.utf8_text(source).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use kin_model::{Entity, EntityKind, FilePathId};

    use super::*;
    use crate::languages::javascript::{parse_javascript_family, tree_is_typescript_family};
    use crate::value_origins::{attach_value_origins, read_value_origins};
    use crate::{JavaScriptAdapter, LanguageAdapter, TypeScriptAdapter};

    fn parse(source: &str, language: tree_sitter_language::LanguageFn) -> Tree {
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&language.into()).unwrap();
        let tree = parser.parse(source, None).unwrap();
        assert!(
            !tree.root_node().has_error(),
            "the fixture must parse cleanly, or no fact is attached: {}",
            tree.root_node().to_sexp()
        );
        tree
    }

    fn typescript(source: &str) -> FileFacts {
        let tree = parse(source, kin_grammar_typescript::LANGUAGE_TYPESCRIPT);
        read_file(&tree, source.as_bytes(), "src/file.ts")
    }

    fn javascript(source: &str) -> FileFacts {
        let tree = parse(source, tree_sitter_javascript::LANGUAGE);
        assert!(!tree_is_typescript_family(&tree));
        read_file(&tree, source.as_bytes(), "src/file.js")
    }

    /// A Flow `.js` file, parsed the way the JavaScript adapter parses it.
    fn flow(source: &str) -> FileFacts {
        let tree = parse_javascript_family(source.as_bytes()).unwrap();
        assert!(
            !tree.root_node().has_error(),
            "{}",
            tree.root_node().to_sexp()
        );
        assert!(
            tree_is_typescript_family(&tree),
            "a Flow fixture must take the TypeScript path"
        );
        read_file(&tree, source.as_bytes(), "src/file.js")
    }

    fn scope<'a>(facts: &'a FileFacts, leaf: &str) -> &'a ScopeFacts {
        &facts
            .by_node
            .iter()
            .find(|(_, name, _)| name == leaf)
            .unwrap_or_else(|| panic!("no scope for {leaf}"))
            .2
    }

    fn declared(type_name: &str) -> ValueOrigin {
        ValueOrigin::Declared {
            type_name: type_name.into(),
        }
    }

    fn context(name: &str) -> ValueOrigin {
        ValueOrigin::Provided {
            lookup: ProviderLookup::Binding {
                kind: REACT_CONTEXT_KIND.into(),
            },
            name: name.into(),
        }
    }

    fn call(callee: &str) -> ValueOrigin {
        ValueOrigin::Call {
            callee: callee.into(),
        }
    }

    fn local(name: &str) -> ValueOrigin {
        ValueOrigin::Local { name: name.into() }
    }

    fn method(receiver: ValueOrigin, method: &str) -> ValueOrigin {
        ValueOrigin::MethodCall {
            receiver: Box::new(receiver),
            method: method.into(),
        }
    }

    const REDUCE_OWNERS: &str = r#"/**
 * @flow
 */

import type Store from '../../store';

type State = {|
  ownerID: number | null,
  selectedElementIndex: number | null,
|};

type Action = {|
  type: 'SELECT_OWNER',
  payload: number,
|};

function reduceOwnersState(store: Store, state: State, action: Action): State {
  const {ownerID} = state;
  switch (action.type) {
    case 'SELECT_OWNER':
      store.getElementByID(action.payload);
      return state;
    default:
      return state;
  }
}
"#;

    const SUSPENDED_BY: &str = r#"/**
 * @flow
 */

import * as React from 'react';
import type {FrontendBridge} from 'react-devtools-shared/src/bridge';
import type Store from '../../store';
import type {Element, SerializedAsyncInfo} from '../../types';

type RowProps = {
  bridge: FrontendBridge,
  element: Element,
  store: Store,
  asyncInfo: SerializedAsyncInfo,
  index: number,
  owner?: ?Element,
};

function SuspendedByRow({
  bridge,
  element = null,
  store: rowStore,
  asyncInfo,
  index,
  owner,
  ...rest
}: RowProps): React.Node {
  rowStore.containsElement(owner.id);
  return null;
}
"#;

    const OWNERS_STACK: &str = r#"/**
 * @flow
 */

import * as React from 'react';
import {useCallback, useContext} from 'react';
import {BridgeContext, StoreContext} from '../context';

export default function OwnersStack(): React.Node {
  const store = useContext(StoreContext);
  const bridge = React.useContext(BridgeContext);
  const selectOwner = useCallback(
    (id: number) => {
      if (store.containsElement(id)) {
        bridge.send('selectFiber', id);
      }
    },
    [bridge, store],
  );
  return null;
}

function ShadowedOwners(): React.Node {
  const store = useContext(StoreContext);
  const pick = (store: Store) => store.getElementByID(1);
  return null;
}
"#;

    const CONTEXT: &str = r#"/**
 * @flow
 */

import {createContext} from 'react';
import type {ReactContext} from 'shared/ReactTypes';

import Store from '../store';
import type {FrontendBridge} from '../../bridge';

export const BridgeContext: ReactContext<FrontendBridge> =
  createContext<FrontendBridge>(((null: any): FrontendBridge));
BridgeContext.displayName = 'BridgeContext';

export const StoreContext: ReactContext<Store> = createContext<Store>(
  null as any as Store,
);
StoreContext.displayName = 'StoreContext';

export const OptionsContext: ReactContext<Options> = createContext(null);
export const MismatchedContext: ReactContext<Store> = createContext<FrontendBridge>(null);
let ReassignableContext = createContext<Store>(null);
export const ForeignContext: ReactContext<Store> = factory.createContext<Store>(null);
"#;

    fn builtin(class: &str) -> ValueOrigin {
        exact(class)
    }

    fn shaped(class: &str) -> ValueOrigin {
        structural(class)
    }

    #[test]
    fn literals_constructors_and_builtin_annotations_are_external_types() {
        let facts = typescript(
            "function work(items: number[], name: string, m: Map<string, number>, r: ReadonlyArray<string> | null) {\n\
             \x20 const arr = [1, 2];\n\
             \x20 const map = new Map();\n\
             \x20 const set = new Set<number>();\n\
             \x20 const obj = { a: 1, pop() { return 1; } };\n\
             \x20 const spread = { ...items };\n\
             \x20 const keys = Object.keys(obj);\n\
             \x20 const s = `x`;\n\
             \x20 arr.pop();\n\
             \x20 return map.get('a');\n\
             }\n",
        );
        let work = scope(&facts, "work");
        assert_eq!(work.bindings.get("items"), Some(&shaped("Array")));
        assert_eq!(work.bindings.get("name"), Some(&builtin("String")));
        assert_eq!(work.bindings.get("m"), Some(&shaped("Map")));
        assert_eq!(work.bindings.get("r"), Some(&shaped("Array")));
        assert_eq!(work.bindings.get("arr"), Some(&builtin("Array")));
        assert_eq!(work.bindings.get("map"), Some(&builtin("Map")));
        assert_eq!(work.bindings.get("set"), Some(&builtin("Set")));
        assert_eq!(
            work.bindings.get("obj"),
            Some(&ValueOrigin::External {
                type_name: "Object".into(),
                reach: ExternalReach::Exact,
                own_members: vec!["a".into(), "pop".into()],
            })
        );
        assert_eq!(
            work.bindings.get("spread"),
            None,
            "a spread can copy any member"
        );
        assert_eq!(work.bindings.get("keys"), Some(&builtin("Array")));
        assert_eq!(work.bindings.get("s"), Some(&builtin("String")));
        let facts = javascript("function plain() {\n  const stack = [];\n  stack.pop();\n}\n");
        assert_eq!(
            scope(&facts, "plain").bindings.get("stack"),
            Some(&builtin("Array"))
        );
    }

    #[test]
    fn a_rebound_builtin_is_not_the_builtin() {
        let facts = typescript(
            "import { Map } from 'immutable';\n\
             function f() {\n  const m = new Map();\n  return m;\n}\n\
             function g(Set: any) {\n  const s = new Set();\n  return s;\n}\n\
             interface Array2 {}\n\
             type Set2 = number;\n\
             function h(items: Map<string, number>) {\n  return items;\n}\n",
        );
        assert_eq!(scope(&facts, "f").bindings.get("m"), Some(&call("Map")));
        assert_eq!(scope(&facts, "g").bindings.get("s"), None);
        assert_eq!(
            scope(&facts, "h").bindings.get("items"),
            None,
            "an imported Map is not the built-in one"
        );
    }

    #[test]
    fn a_member_the_scope_writes_withdraws_the_external_type() {
        let facts = javascript(
            "function patched() {\n  const arr = [];\n  arr.pop = () => 1;\n  arr.pop();\n}\n\
             function indexed() {\n  const arr = [];\n  arr[0] = 1;\n  arr.pop();\n}\n\
             function keyed(k) {\n  const obj = {};\n  obj[k] = 1;\n  obj.pop();\n}\n\
             function assigned(x) {\n  const map = new Map();\n  Object.assign(map, x);\n  map.get(1);\n}\n",
        );
        assert_eq!(scope(&facts, "patched").bindings.get("arr"), None);
        assert_eq!(
            scope(&facts, "indexed").bindings.get("arr"),
            Some(&builtin("Array")),
            "an element write replaces no method"
        );
        assert_eq!(scope(&facts, "keyed").bindings.get("obj"), None);
        assert_eq!(scope(&facts, "assigned").bindings.get("map"), None);
    }

    #[test]
    fn a_typescript_class_member_is_what_its_declaration_says_in_every_method() {
        let facts = typescript(
            "import { inject } from '@angular/core';\n\
             class Panel {\n\
             \x20 private store = inject(Store);\n\
             \x20 cache = new Map<string, number>();\n\
             \x20 items: string[] = [];\n\
             \x20 twice: Store;\n\
             \x20 static shared: Store;\n\
             \x20 constructor(private svc: Service, readonly logger: Logger, plain: Other) {}\n\
             \x20 run() {\n    this.svc.load();\n  }\n\
             \x20 static make() {\n    return 1;\n  }\n\
             \x20 later() {\n    const f = function () { return this; };\n    return f;\n  }\n\
             \x20 twice() {\n    return 1;\n  }\n\
             \x20 onClick = () => {\n    this.store.load();\n  };\n\
             }\n",
        );
        let run = scope(&facts, "run");
        assert_eq!(run.bindings.get("this.store"), Some(&declared("Store")));
        assert_eq!(run.bindings.get("this.cache"), Some(&shaped("Map")));
        assert_eq!(run.bindings.get("this.items"), Some(&shaped("Array")));
        assert_eq!(run.bindings.get("this.svc"), Some(&declared("Service")));
        assert_eq!(run.bindings.get("this.logger"), Some(&declared("Logger")));
        assert_eq!(run.bindings.get("this.plain"), None);
        assert_eq!(
            run.bindings.get("this.twice"),
            None,
            "a name the class declares twice holds no one value"
        );
        assert_eq!(run.bindings.get("this.shared"), None);
        assert!(scope(&facts, "make").bindings.is_empty());
        assert!(
            !scope(&facts, "later")
                .bindings
                .keys()
                .any(|key| key.starts_with("this.")),
            "a nested function gives `this` another value"
        );
        assert_eq!(
            scope(&facts, "onClick").bindings.get("this.store"),
            Some(&declared("Store"))
        );
    }

    #[test]
    fn vue_and_angular_inject_read_what_a_typed_key_provides() {
        let facts = typescript(
            "import { inject, InjectionKey } from 'vue';\n\
             export const storeKey: InjectionKey<Store> = Symbol('store');\n\
             export function useStore() {\n\
             \x20 const store = inject(storeKey);\n\
             \x20 const other = inject<Other>('other');\n\
             \x20 const loose = inject('loose');\n\
             \x20 return [store, other, loose];\n\
             }\n",
        );
        let provides = &facts.module.provides;
        assert!(provides
            .iter()
            .any(|provider| provider.kind == INJECTION_KEY_KIND
                && provider.name == "storeKey"
                && provider.value == Some(declared("Store"))));
        let using = scope(&facts, "useStore");
        assert_eq!(
            using.bindings.get("store"),
            Some(&ValueOrigin::Provided {
                lookup: ProviderLookup::Binding {
                    kind: INJECTION_KEY_KIND.into(),
                },
                name: "storeKey".into(),
            })
        );
        assert_eq!(using.bindings.get("other"), Some(&declared("Other")));
        assert_eq!(using.bindings.get("loose"), None);

        let facts = typescript(
            "import { inject, InjectionToken } from '@angular/core';\n\
             export const API = new InjectionToken<ApiClient>('api');\n\
             export function guard() {\n\
             \x20 const api = inject(API);\n\
             \x20 const auth = inject(AuthService);\n\
             \x20 return [api, auth];\n\
             }\n",
        );
        assert!(facts.module.provides.iter().any(
            |provider| provider.name == "API" && provider.value == Some(declared("ApiClient"))
        ));
        let guard = scope(&facts, "guard");
        assert!(matches!(
            guard.bindings.get("api"),
            Some(ValueOrigin::Provided { name, .. }) if name == "API"
        ));
        assert_eq!(guard.bindings.get("auth"), Some(&declared("AuthService")));
    }

    #[test]
    fn an_inject_from_elsewhere_is_any_call() {
        let facts = typescript(
            "import { inject } from './di';\n\
             export function guard() {\n  const auth = inject(AuthService);\n  return auth;\n}\n",
        );
        assert_eq!(
            scope(&facts, "guard").bindings.get("auth"),
            Some(&call("inject"))
        );
    }

    #[test]
    fn an_annotated_parameter_is_its_declared_type() {
        let facts = flow(REDUCE_OWNERS);
        let reduce = scope(&facts, "reduceOwnersState");
        assert_eq!(reduce.bindings.get("store"), Some(&declared("Store")));
        assert_eq!(reduce.bindings.get("state"), Some(&declared("State")));
        assert_eq!(reduce.bindings.get("action"), Some(&declared("Action")));
        assert!(!reduce.bindings.contains_key("ownerID"));
        assert_eq!(
            reduce.returns,
            Some(DeclaredReturn::Type {
                type_name: "State".into()
            })
        );

        let facts = flow(
            "// @flow\nfunction shapes(a: ?Store, b: Store | null, c: Store | Bridge, d: string, e: Array<Store>, f: React.Node, g: mixed, h: void | Store, i: Object): Promise<Store> {}\n",
        );
        let shapes = scope(&facts, "shapes");
        assert_eq!(shapes.bindings.get("a"), Some(&declared("Store")));
        assert_eq!(shapes.bindings.get("b"), Some(&declared("Store")));
        assert_eq!(shapes.bindings.get("f"), Some(&declared("React.Node")));
        assert_eq!(shapes.bindings.get("h"), Some(&declared("Store")));
        // A primitive and a built-in collection are types no file of the
        // repository declares.
        assert_eq!(shapes.bindings.get("d"), Some(&builtin("String")));
        assert_eq!(shapes.bindings.get("e"), Some(&shaped("Array")));
        for refused in ["c", "g", "i"] {
            assert!(!shapes.bindings.contains_key(refused), "{refused}");
        }
        assert_eq!(shapes.returns, None);
    }

    #[test]
    fn destructured_props_take_their_types_from_the_object_type_they_are_annotated_with() {
        let facts = flow(SUSPENDED_BY);
        let row = scope(&facts, "SuspendedByRow");
        assert_eq!(
            row.bindings.get("bridge"),
            Some(&declared("FrontendBridge"))
        );
        assert_eq!(row.bindings.get("rowStore"), Some(&declared("Store")));
        assert_eq!(
            row.bindings.get("asyncInfo"),
            Some(&declared("SerializedAsyncInfo"))
        );
        assert_eq!(row.bindings.get("owner"), Some(&declared("Element")));
        // A default may be what the local holds, a primitive names no class,
        // `store` is a key rather than a local here, and a rest element is a
        // fresh object.
        for refused in ["element", "index", "store", "rest"] {
            assert!(!row.bindings.contains_key(refused), "{refused}");
        }

        let facts = flow(
            "// @flow\nfunction Inline({owner, label}: {owner: Owner, label: string}) {}\nfunction Exact({owner}: {| owner: Owner |}) {}\nfunction ReadOnly({owner}: $ReadOnly<{owner: Owner}>) {}\nfunction ThroughInterface({owner}: OwnerProps) {}\nexport interface OwnerProps { owner: Owner }\nfunction ThroughAlias({owner}: Aliased) {}\ntype Aliased = WrappedProps;\nexport type WrappedProps = $ReadOnly<{|owner: Owner|}>;\n",
        );
        for leaf in [
            "Inline",
            "Exact",
            "ReadOnly",
            "ThroughInterface",
            "ThroughAlias",
        ] {
            assert_eq!(
                scope(&facts, leaf).bindings.get("owner"),
                Some(&declared("Owner")),
                "{leaf}"
            );
        }
        assert!(!scope(&facts, "Inline").bindings.contains_key("label"));
    }

    #[test]
    fn a_generic_repeated_or_shadowed_type_name_is_not_read() {
        let facts = typescript(
            "type Props<T> = { owner: T };\nfunction Generic({owner}: Props<Owner>) {}\ntype Twice = { owner: Owner };\nnamespace N { type Twice = { owner: Other }; }\nfunction Repeated({owner}: Twice) {}\ntype Param = { owner: Owner };\nfunction Shadowed<Param>({owner}: Param) {}\nfunction Method({owner}: { owner(): Owner }) {}\n",
        );
        for leaf in ["Generic", "Repeated", "Shadowed", "Method"] {
            assert!(scope(&facts, leaf).bindings.is_empty(), "{leaf}");
        }
    }

    #[test]
    fn use_context_through_react_reads_the_context_provider() {
        let facts = flow(OWNERS_STACK);
        let stack = scope(&facts, "OwnersStack");
        assert_eq!(stack.bindings.get("store"), Some(&context("StoreContext")));
        assert_eq!(
            stack.bindings.get("bridge"),
            Some(&context("BridgeContext"))
        );
        assert_eq!(
            stack.bindings.get("selectOwner"),
            Some(&call("useCallback"))
        );
        assert_eq!(
            stack.returns,
            Some(DeclaredReturn::Type {
                type_name: "React.Node".into()
            })
        );
        // The nested arrow's `store` is a second binding of the name, so a
        // read the adapter attributes to the component could mean either.
        assert!(!scope(&facts, "ShadowedOwners")
            .bindings
            .contains_key("store"));

        let facts = typescript(
            "import React from 'react';\nimport {useContext as useCtx} from 'react';\nfunction Panel() {\n  const store = useCtx(StoreContext);\n  const bridge = React.useContext(BridgeContext);\n}\n",
        );
        let panel = scope(&facts, "Panel");
        assert_eq!(panel.bindings.get("store"), Some(&context("StoreContext")));
        assert_eq!(
            panel.bindings.get("bridge"),
            Some(&context("BridgeContext"))
        );
    }

    #[test]
    fn a_use_context_react_does_not_supply_is_not_a_context_read() {
        let facts = typescript(
            "import {useContext} from './hooks';\nimport * as Hooks from './hooks';\nfunction Foreign() {\n  const store = useContext(StoreContext);\n  const bridge = Hooks.useContext(BridgeContext);\n}\n",
        );
        assert!(scope(&facts, "Foreign").bindings.is_empty());

        let facts = typescript(
            "import {useContext} from 'react';\nfunction Local() {\n  function useContext(c: unknown) { return c; }\n  const store = useContext(StoreContext);\n}\nfunction Rebound(StoreContext: unknown) {\n  const store = useContext(StoreContext);\n}\nfunction Computed() {\n  const store = useContext(contexts.store);\n}\n",
        );
        for leaf in ["Local", "Rebound", "Computed"] {
            assert!(
                !scope(&facts, leaf).bindings.contains_key("store"),
                "{leaf}"
            );
        }
    }

    #[test]
    fn a_module_context_provides_the_type_it_is_created_with() {
        let facts = flow(CONTEXT);
        let provided: Vec<(&str, Option<&ValueOrigin>)> = facts
            .module
            .provides
            .iter()
            .map(|provider| {
                assert_eq!(provider.kind, REACT_CONTEXT_KIND);
                assert_eq!(provider.owner, None);
                (provider.name.as_str(), provider.value.as_ref())
            })
            .collect();
        assert_eq!(
            provided,
            vec![
                ("BridgeContext", Some(&declared("FrontendBridge"))),
                ("StoreContext", Some(&declared("Store"))),
                ("OptionsContext", Some(&declared("Options"))),
                ("MismatchedContext", None),
            ]
        );

        // A plain JavaScript context has no type to read, but it still
        // shadows every farther context of its name.
        let facts = javascript(
            "import * as React from 'react';\nexport const ThemeContext = React.createContext(null);\n",
        );
        assert_eq!(facts.module.provides.len(), 1);
        assert_eq!(facts.module.provides[0].name, "ThemeContext");
        assert_eq!(facts.module.provides[0].value, None);
    }

    #[test]
    fn expression_origins_follow_constructors_calls_methods_and_locals() {
        let facts = typescript(
            "import * as api from './api';\nasync function build(factory: Factory) {\n  const a = new Thing(1);\n  const b = makeThing();\n  const c = a.child();\n  const d = makeThing().child();\n  const e = api.connect();\n  const f = a;\n  const g = (a);\n  const h = await load();\n  const i = a as Thing;\n  const j: Store = makeThing();\n  const k = ready ? a : b;\n  const l = a.field;\n  const m = new api.Client();\n  const n = factory.create();\n  const o = this.make();\n  const p = a?.child();\n  const q = require('./q');\n}\n",
        );
        let build = scope(&facts, "build");
        let expected = [
            ("factory", declared("Factory")),
            ("a", call("Thing")),
            ("b", call("makeThing")),
            ("c", method(local("a"), "child")),
            ("d", method(call("makeThing"), "child")),
            ("e", call("api.connect")),
            ("f", local("a")),
            ("g", local("a")),
            ("j", declared("Store")),
            ("m", call("api.Client")),
            ("n", method(local("factory"), "create")),
        ];
        for (name, origin) in &expected {
            assert_eq!(build.bindings.get(*name), Some(origin), "{name}");
        }
        assert_eq!(build.bindings.len(), expected.len(), "{:?}", build.bindings);
    }

    #[test]
    fn a_name_bound_twice_or_read_outside_its_block_carries_no_fact() {
        let facts = typescript(
            "function twice() {\n  let x = make();\n  x = other();\n  x.run();\n}\nfunction counted() {\n  for (let i = make(); i < 3; i++) {}\n  const local = makeLocal();\n  function makeLocal() { return new Thing(); }\n}\nfunction blocked(cond: boolean) {\n  if (cond) {\n    const inner = make();\n    inner.run();\n  }\n  inner.run();\n}\nfunction nested(cond: boolean) {\n  if (cond) {\n    const inner = make();\n    inner.run();\n    var hoisted = make();\n  }\n  hoisted.run();\n}\nfunction defaulted(read = store) {\n  const store = make();\n}\nfunction evaluated(store: Store) {\n  eval('store = null');\n}\nfunction caught() {\n  const err = make();\n  try {} catch (err) {}\n}\n",
        );
        assert!(scope(&facts, "twice").bindings.is_empty());
        assert!(scope(&facts, "counted").bindings.is_empty());
        assert!(!scope(&facts, "blocked").bindings.contains_key("inner"));
        let nested = scope(&facts, "nested");
        assert_eq!(nested.bindings.get("inner"), Some(&call("make")));
        assert_eq!(nested.bindings.get("hoisted"), Some(&call("make")));
        assert!(scope(&facts, "defaulted").bindings.is_empty());
        assert!(scope(&facts, "evaluated").bindings.is_empty());
        assert!(scope(&facts, "caught").bindings.is_empty());
    }

    #[test]
    fn class_members_fields_wrappers_and_interfaces_are_scoped() {
        let source = "class Tree {\n  select(store: Store): this { return this; }\n  handle = (bridge: Bridge) => { bridge.send(); };\n  lookup(id: number): Element | null { return null; }\n}\ninterface Api {\n  fetch(): Store;\n  get(key: string): A;\n  get(key: number): B;\n  self(): this;\n}\ndeclare class Remote {\n  open(): Channel;\n  load(a: string): A;\n  load(a: number): B;\n}\nconst Memo = memo(function Inner({store}: {store: Store}) { return null; });\nconst Both = combine((a: Store) => a, (b: Store) => b);\nexport const Arrow = (store: Store): Store => store;\nexport default (bridge: Bridge) => bridge.send();\n";
        let facts = typescript(source);
        let select = scope(&facts, "select");
        assert_eq!(select.bindings.get("store"), Some(&declared("Store")));
        assert_eq!(select.returns, Some(DeclaredReturn::Receiver));
        assert_eq!(
            scope(&facts, "handle").bindings.get("bridge"),
            Some(&declared("Bridge"))
        );
        assert_eq!(
            scope(&facts, "lookup").returns,
            Some(DeclaredReturn::Type {
                type_name: "Element".into()
            })
        );
        assert_eq!(
            scope(&facts, "fetch").returns,
            Some(DeclaredReturn::Type {
                type_name: "Store".into()
            })
        );
        assert!(scope(&facts, "fetch").bindings.is_empty());
        assert_eq!(
            scope(&facts, "self").returns,
            Some(DeclaredReturn::Receiver)
        );
        assert_eq!(
            scope(&facts, "open").returns,
            Some(DeclaredReturn::Type {
                type_name: "Channel".into()
            })
        );
        // An overload set speaks for more than one return.
        assert!(facts
            .by_node
            .iter()
            .all(|(_, leaf, _)| leaf != "get" && leaf != "load"));
        assert_eq!(
            scope(&facts, "Memo").bindings.get("store"),
            Some(&declared("Store"))
        );
        assert!(facts.by_node.iter().all(|(_, leaf, _)| leaf != "Both"));
        let arrow = facts
            .by_node
            .iter()
            .find(|(_, leaf, _)| leaf == "Arrow")
            .unwrap();
        let declarator = source.find("Arrow = ").unwrap();
        assert!(arrow.0.iter().any(|(start, _)| *start == declarator));
        assert_eq!(arrow.2.bindings.get("store"), Some(&declared("Store")));
        assert_eq!(
            scope(&facts, "default").bindings.get("bridge"),
            Some(&declared("Bridge"))
        );
    }

    #[test]
    fn plain_javascript_reads_origins_without_types() {
        let facts = javascript(
            "import {useContext} from 'react';\nimport {StoreContext} from './context';\nexport function Panel() {\n  const store = useContext(StoreContext);\n  const view = new View(store);\n  return view.render();\n}\nWidget.prototype.draw = function draw() {\n  const ctx = this.canvas.getContext();\n  const shape = new Shape();\n};\nmodule.exports = { helper() { const result = new Result(); } };\nclass Widget {\n  render() { const node = createNode(); }\n  onClick = () => { const event = new Event(); };\n}\n",
        );
        let panel = scope(&facts, "Panel");
        assert_eq!(panel.bindings.get("store"), Some(&context("StoreContext")));
        assert_eq!(panel.bindings.get("view"), Some(&call("View")));
        let draw = scope(&facts, "draw");
        assert_eq!(draw.bindings.get("shape"), Some(&call("Shape")));
        assert!(!draw.bindings.contains_key("ctx"));
        assert_eq!(
            scope(&facts, "helper").bindings.get("result"),
            Some(&call("Result"))
        );
        assert_eq!(
            scope(&facts, "render").bindings.get("node"),
            Some(&call("createNode"))
        );
        assert_eq!(
            scope(&facts, "onClick").bindings.get("event"),
            Some(&call("Event"))
        );
    }

    /// Run the real adapter over `source`, convert its entities the way the
    /// pipeline does, and attach the facts.
    fn attached(adapter: &dyn LanguageAdapter, path: &str, source: &str) -> Vec<Entity> {
        let bytes = source.as_bytes();
        let tree = adapter.parse(bytes).unwrap();
        assert!(
            !tree.root_node().has_error(),
            "the fixture must parse cleanly: {}",
            tree.root_node().to_sexp()
        );
        let file_id = FilePathId(path.to_string());
        let output = adapter.extract(&tree, bytes, &file_id).unwrap();
        let language = adapter.language_id();
        let mut entities: Vec<Entity> = output
            .entities
            .into_iter()
            .map(|entity| entity.into_entity(language, &file_id))
            .collect();
        attach_value_origins(&tree, bytes, language, path, &mut entities);
        entities
    }

    fn flow_attached(path: &str, source: &str) -> Vec<Entity> {
        let tree = JavaScriptAdapter.parse(source.as_bytes()).unwrap();
        assert!(
            tree_is_typescript_family(&tree),
            "a Flow fixture must take the TypeScript path"
        );
        attached(&JavaScriptAdapter, path, source)
    }

    fn facts_on(entities: &[Entity], kind: EntityKind, name: &str) -> ScopeFacts {
        let entity = entities
            .iter()
            .find(|entity| entity.kind == kind && entity.name == name)
            .unwrap_or_else(|| {
                panic!(
                    "no {kind:?} {name} in {:?}",
                    entities
                        .iter()
                        .map(|entity| (entity.kind, entity.name.as_str()))
                        .collect::<Vec<_>>()
                )
            });
        read_value_origins(entity).unwrap_or_else(|| panic!("no facts on {name}"))
    }

    fn carriers(entities: &[Entity]) -> Vec<(EntityKind, &str)> {
        entities
            .iter()
            .filter(|entity| read_value_origins(entity).is_some())
            .map(|entity| (entity.kind, entity.name.as_str()))
            .collect()
    }

    #[test]
    fn the_adapter_carries_a_flow_components_context_read_to_its_entity() {
        let entities = flow_attached(
            "packages/react-devtools-shared/src/devtools/views/Components/OwnersStack.js",
            OWNERS_STACK,
        );
        let stack = facts_on(&entities, EntityKind::Function, "OwnersStack");
        assert_eq!(stack.bindings.get("store"), Some(&context("StoreContext")));
        assert_eq!(
            stack.bindings.get("bridge"),
            Some(&context("BridgeContext"))
        );
        let shadowed = facts_on(&entities, EntityKind::Function, "ShadowedOwners");
        assert!(!shadowed.bindings.contains_key("store"));
    }

    #[test]
    fn the_adapter_carries_typed_props_and_parameters_to_their_entities() {
        let entities = flow_attached(
            "packages/react-devtools-shared/src/devtools/views/Components/InspectedElementSuspendedBy.js",
            SUSPENDED_BY,
        );
        let row = facts_on(&entities, EntityKind::Function, "SuspendedByRow");
        assert_eq!(
            row.bindings.get("bridge"),
            Some(&declared("FrontendBridge"))
        );
        assert_eq!(row.bindings.get("rowStore"), Some(&declared("Store")));
        assert_eq!(
            carriers(&entities),
            vec![(EntityKind::Function, "SuspendedByRow")]
        );

        let entities = flow_attached(
            "packages/react-devtools-shared/src/devtools/views/Components/OwnersListContext.js",
            REDUCE_OWNERS,
        );
        let reduce = facts_on(&entities, EntityKind::Function, "reduceOwnersState");
        assert_eq!(reduce.bindings.get("store"), Some(&declared("Store")));
        assert_eq!(
            reduce.returns,
            Some(DeclaredReturn::Type {
                type_name: "State".into()
            })
        );
    }

    #[test]
    fn the_context_module_entity_carries_the_contexts_it_creates() {
        let entities = flow_attached(
            "packages/react-devtools-shared/src/devtools/views/context.js",
            CONTEXT,
        );
        let module = facts_on(&entities, EntityKind::Module, "context");
        let store = module
            .provides
            .iter()
            .find(|provider| provider.name == "StoreContext")
            .unwrap();
        assert_eq!(store.kind, REACT_CONTEXT_KIND);
        assert_eq!(store.value, Some(declared("Store")));
        assert_eq!(module.provides.len(), 4);
        assert_eq!(carriers(&entities), vec![(EntityKind::Module, "context")]);
    }

    #[test]
    fn the_javascript_adapter_carries_facts_for_every_function_shape_it_mints() {
        let source = "import {useContext} from 'react';\nimport {StoreContext} from './context';\nexport function Panel() {\n  const store = useContext(StoreContext);\n}\nconst make = () => { const made = new Made(); };\nView.prototype.lookup = function lookup(name) { const found = new Found(name); };\nres.contentType = res.type = function contentType(t) { const header = makeHeader(t); };\nmodule.exports = { helper() { const result = new Result(); } };\nclass Widget {\n  render() { const node = createNode(); }\n  onClick = () => { const event = new Event(); };\n}\nexport default function () { const fallback = new Fallback(); }\n";
        let entities = attached(&JavaScriptAdapter, "src/widgets.js", source);
        let expected = [
            (
                EntityKind::Function,
                "Panel",
                "store",
                context("StoreContext"),
            ),
            (EntityKind::Function, "make", "made", call("Made")),
            (EntityKind::Method, "View.lookup", "found", call("Found")),
            (
                EntityKind::Method,
                "res.contentType",
                "header",
                call("makeHeader"),
            ),
            (EntityKind::Method, "res.type", "header", call("makeHeader")),
            (EntityKind::Function, "helper", "result", call("Result")),
            (
                EntityKind::Method,
                "Widget.render",
                "node",
                call("createNode"),
            ),
            (EntityKind::Method, "Widget.onClick", "event", call("Event")),
            (
                EntityKind::Function,
                "default",
                "fallback",
                call("Fallback"),
            ),
        ];
        for (kind, name, binding, origin) in &expected {
            assert_eq!(
                facts_on(&entities, *kind, name).bindings.get(*binding),
                Some(origin),
                "{name}"
            );
        }
        assert_eq!(
            carriers(&entities).len(),
            expected.len(),
            "{:?}",
            carriers(&entities)
        );
    }

    #[test]
    fn the_typescript_adapter_carries_member_arrow_and_wrapper_facts() {
        let source = "import {memo} from 'react';\nexport class Tree {\n  select(store: Store): this { return this; }\n  handle = (bridge: Bridge) => { bridge.send(); };\n}\nexport const Arrow = (store: Store): Store => store;\nexport const Memo = memo(function Inner({store}: {store: Store}) { return null; });\nexport function plain(store: Store) { store.flush(); }\nexport default (bridge: Bridge) => bridge.send();\nconst utils = { parse(input: Input) { input.read(); } };\n";
        let entities = attached(&TypeScriptAdapter, "src/tree.ts", source);
        let select = facts_on(&entities, EntityKind::Method, "Tree.select");
        assert_eq!(select.returns, Some(DeclaredReturn::Receiver));
        assert_eq!(
            facts_on(&entities, EntityKind::Method, "Tree.handle")
                .bindings
                .get("bridge"),
            Some(&declared("Bridge"))
        );
        for name in ["Arrow", "Memo", "plain"] {
            assert_eq!(
                facts_on(&entities, EntityKind::Function, name)
                    .bindings
                    .get("store"),
                Some(&declared("Store")),
                "{name}"
            );
        }
        assert_eq!(
            facts_on(&entities, EntityKind::Function, "default")
                .bindings
                .get("bridge"),
            Some(&declared("Bridge"))
        );
        assert_eq!(
            facts_on(&entities, EntityKind::Method, "utils.parse")
                .bindings
                .get("input"),
            Some(&declared("Input"))
        );
    }

    #[test]
    fn a_module_records_the_binding_its_one_default_export_names() {
        for (source, expected) in [
            (
                "export type Config = {name: string};\nexport default class Store {\n  contains(id: number): boolean { return id > 0; }\n}\n",
                Some("Store"),
            ),
            (
                "class Model {}\nexport default Model;\n",
                Some("Model"),
            ),
            ("class Model {}\nexport {Model as default};\n", Some("Model")),
            (
                "export default function makeStore() { return null; }\n",
                Some("makeStore"),
            ),
            // Anonymous, re-exported from elsewhere, written twice, or a
            // value that is no binding: no binding of this module is meant.
            ("export default class {}\n", None),
            ("export {Store as default} from './store';\n", None),
            ("class A {}\nclass B {}\nexport default A;\nexport {B as default};\n", None),
            ("export default connect(mapState)(View);\n", None),
            (
                "export default interface Options {\n  verbose: boolean;\n}\n",
                Some("Options"),
            ),
            // Another module's default, declared in this file.
            (
                "declare module 'store' {\n  export default class Store {}\n}\n",
                None,
            ),
        ] {
            assert_eq!(
                typescript(source).module.default_export.as_deref(),
                expected,
                "{source}"
            );
        }
    }

    #[test]
    fn the_module_entity_carries_its_default_export() {
        let entities = attached(
            &TypeScriptAdapter,
            "src/devtools/store.ts",
            "export type Config = {name: string};\nexport default class Store {\n  contains(id: number): boolean { return id > 0; }\n}\n",
        );
        let module = facts_on(&entities, EntityKind::Module, "store");
        assert_eq!(module.default_export.as_deref(), Some("Store"));
        assert!(module.binding_lines.is_empty());
    }

    #[test]
    fn a_binding_records_its_line_inside_the_entity() {
        let source = "import {useContext} from 'react';\nimport {StoreContext} from './context';\n\nexport function Panel({id}: {id: number}) {\n  const unrelated = 1;\n  const store = useContext(StoreContext);\n  return store.contains(id);\n}\n";
        let entities = attached(&TypeScriptAdapter, "src/Panel.tsx", source);
        let panel = facts_on(&entities, EntityKind::Function, "Panel");
        assert_eq!(panel.bindings.get("store"), Some(&context("StoreContext")));
        // `export function Panel` opens on line 3 of the file; the binding is
        // two lines into it.
        assert_eq!(panel.binding_lines.get("store"), Some(&2));
        assert!(panel
            .binding_lines
            .keys()
            .all(|name| panel.bindings.contains_key(name)));
    }
}
