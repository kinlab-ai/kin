// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Rust value origins: declared parameter, local and field types, struct
//! literals, associated-function and method calls, and declared returns.
//!
//! Rust writes a value's type down more often than any other language this
//! crate reads, and never changes it: `let x: Store`, `store: &Store`,
//! `Store { .. }`, `Store::new(..)` with `-> Self`, a field declared
//! `store: Store`, `impl Store` giving `self` its type. Each becomes a fact the
//! linker follows to one type and then asks that type for the method.
//!
//! A type name becomes a fact only when this file settles which declaration it
//! names. Looked up from where it is written, through the enclosing blocks and
//! then the enclosing module (Rust never looks further out without a `use`):
//!
//! - a struct, enum, union or trait the file declares, at module scope under a
//!   name no other type of the file shares, or in an enclosing block, where it
//!   is the nested entity `outer.<locals>.Name`;
//! - a name a `use` from `crate::`, `self::` or `super::` binds under its own
//!   name, when the file declares no type of that name: it can only be a type
//!   of this crate.
//!
//! Anything else emits nothing: a type alias, a name a glob or an external
//! `use` may bind (the linker could otherwise settle it on a same-named
//! repository type), the prelude's `Box`, `Vec`, `Option` and friends, a
//! qualified path, and every wrapper type, since `Arc<Store>::clone` is not
//! `Store::clone`.
//!
//! A trait object (`&dyn Sink`, `&mut dyn Sink`), an `impl Sink` parameter or
//! return, and a generic parameter bound only by `Sink` (marker traits aside)
//! type the value as the trait itself, so a call through it binds the trait's
//! own method and never a guessed implementation.
//!
//! A local is recorded only when the callable binds its name exactly once,
//! counting every pattern, closure parameter and nested item in its subtree
//! and every `let` a macro spells, and when every other mention of the name in
//! the callable sits after that `let` and inside its block. A fact about a
//! receiver never rests on a guess: a method call on a value no fact types
//! records nothing.
//!
//! A call written `self.store.load()` arrives at the linker with the receiver
//! `self.store`. The callable records that receiver under the same text, typed
//! by the field's declaration in this file, and a chain through a field
//! (`let r = self.store.reader()`) carries the field's type inline.

use std::collections::{BTreeMap, HashMap, HashSet};

use tree_sitter::{Node, Tree};

use super::{DeclaredReturn, FileFacts, ScopeFacts, ValueOrigin};
use crate::languages::rust_nested::{
    is_async, is_module_body, item_name, members, parent_module_body, pattern_bindings, plan, text,
    use_bindings, Binder, RustPlan, UseBinding,
};

/// Traits that declare no method a call could reach, so a bound on one says
/// nothing about which trait a method call names.
const MARKER_TRAITS: &[&str] = &[
    "Send",
    "Sync",
    "Sized",
    "Unpin",
    "Copy",
    "UnwindSafe",
    "RefUnwindSafe",
];

/// Modules whose `Result` alias keeps the success type first, so `?` on one
/// yields its first argument.
const RESULT_MODULES: &[&str] = &["std", "core", "io", "anyhow", "eyre", "color_eyre"];

pub(super) fn read_file(tree: &Tree, source: &[u8], _file_path: &str) -> FileFacts {
    let file = FileContext::read(tree, source);
    let mut facts = FileFacts::default();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        let callable = node.kind() == "function_item"
            || (node.kind() == "function_signature_item"
                && node
                    .parent()
                    .and_then(|list| list.parent())
                    .is_some_and(|owner| owner.kind() == "trait_item"));
        if callable && file.is_entity(node) {
            if let Some(name) = item_name(node, source) {
                let scope = Callable::new(&file, node).facts();
                let start = crate::languages::rust_lang::rust_item_start(&node, source).0;
                let ranges = vec![
                    (start, node.end_byte()),
                    (node.start_byte(), node.end_byte()),
                ];
                facts.by_node.push((ranges, name.to_string(), scope));
            }
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    facts
}

/// A type declaration a module scope holds.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Declared {
    /// A struct, enum, union or trait: a type the linker locates.
    Type,
    /// A type alias, which the linker cannot follow.
    Alias,
}

#[derive(Default)]
struct ScopeImports {
    uses: Vec<UseBinding>,
    glob: bool,
}

/// What the whole file says, read once.
struct FileContext<'s, 'tree> {
    source: &'s [u8],
    plan: RustPlan<'tree>,
    /// Types each module scope declares, by the scope's node id.
    module_types: HashMap<usize, HashMap<String, Declared>>,
    /// `use` bindings by the module or block node that holds them.
    imports: HashMap<usize, ScopeImports>,
    /// How many module-scope type declarations of the file carry each name.
    type_counts: HashMap<String, usize>,
    /// Free functions each module scope declares, by the scope's node id.
    module_functions: HashMap<usize, HashSet<String>>,
    /// How many module-scope free functions of the file carry each name.
    function_counts: HashMap<String, usize>,
    /// Struct declarations by the name a fact spells for them.
    structs: HashMap<String, Node<'tree>>,
    /// Module-scope free functions by name, every module of the file.
    function_nodes: HashMap<String, Vec<Node<'tree>>>,
    /// Methods of inherent impls by method name, with their impl.
    inherent_methods: HashMap<String, Vec<(Node<'tree>, Node<'tree>)>>,
    /// Whether the file declares its own `Result` or `Option` type, alias
    /// aside.
    shadows_std: HashSet<String>,
    /// A same-file `type Result<T, ..> = ...Result<T, ..>` alias keeps the
    /// success type first.
    result_alias_ok: Option<bool>,
}

impl<'s, 'tree> FileContext<'s, 'tree> {
    fn read(tree: &'tree Tree, source: &'s [u8]) -> Self {
        let mut context = Self {
            source,
            plan: plan(tree, source),
            module_types: HashMap::new(),
            imports: HashMap::new(),
            type_counts: HashMap::new(),
            module_functions: HashMap::new(),
            function_counts: HashMap::new(),
            structs: HashMap::new(),
            function_nodes: HashMap::new(),
            inherent_methods: HashMap::new(),
            shadows_std: HashSet::new(),
            result_alias_ok: None,
        };
        let mut stack = vec![tree.root_node()];
        while let Some(node) = stack.pop() {
            let scope = node.id();
            let module = is_module_body(node);
            if module || node.kind() == "block" {
                let mut cursor = node.walk();
                let children: Vec<Node<'tree>> = node.named_children(&mut cursor).collect();
                for child in children {
                    match child.kind() {
                        "use_declaration" => {
                            let imports = context.imports.entry(scope).or_default();
                            for binding in use_bindings(child, source) {
                                imports.glob |= binding.glob;
                                imports.uses.push(binding);
                            }
                        }
                        "struct_item" | "union_item" | "enum_item" | "trait_item" | "type_item"
                            if module =>
                        {
                            let Some(name) = item_name(child, source) else {
                                continue;
                            };
                            let declared = if child.kind() == "type_item" {
                                Declared::Alias
                            } else {
                                Declared::Type
                            };
                            context
                                .module_types
                                .entry(scope)
                                .or_default()
                                .insert(name.to_string(), declared);
                            *context.type_counts.entry(name.to_string()).or_default() += 1;
                            if child.kind() == "struct_item" {
                                context.structs.insert(name.to_string(), child);
                            }
                            if matches!(name, "Result" | "Option") {
                                if declared == Declared::Type {
                                    context.shadows_std.insert(name.to_string());
                                } else if name == "Result" {
                                    context.result_alias_ok = Some(
                                        context.result_alias_ok.unwrap_or(true)
                                            && result_alias_keeps_ok_first(child, source),
                                    );
                                }
                            }
                        }
                        "impl_item" if child.child_by_field_name("trait").is_none() => {
                            for member in members(child, &["function_item"]) {
                                if let Some(name) = item_name(member, source) {
                                    context
                                        .inherent_methods
                                        .entry(name.to_string())
                                        .or_default()
                                        .push((child, member));
                                }
                            }
                        }
                        "function_item" if module => {
                            if let Some(name) = item_name(child, source) {
                                context
                                    .function_nodes
                                    .entry(name.to_string())
                                    .or_default()
                                    .push(child);
                                context
                                    .module_functions
                                    .entry(scope)
                                    .or_default()
                                    .insert(name.to_string());
                                *context.function_counts.entry(name.to_string()).or_default() += 1;
                            }
                        }
                        _ => {}
                    }
                }
            }
            let mut cursor = node.walk();
            stack.extend(node.named_children(&mut cursor));
        }
        for item in &context.plan.nested {
            if item.node.kind() == "struct_item" {
                context.structs.insert(item.name.clone(), item.node);
            }
        }
        // A struct name two module-scope types share is no struct a fact can
        // spell.
        let counts = &context.type_counts;
        context
            .structs
            .retain(|name, _| counts.get(name).is_none_or(|count| *count == 1));
        context
    }

    /// Whether a function item is an entity the adapter mints: one outside
    /// every callable body, or a nested one the plan admits.
    fn is_entity(&self, function: Node<'_>) -> bool {
        if self.plan.is_admitted(function) {
            return true;
        }
        let mut current = function.parent();
        while let Some(parent) = current {
            if matches!(
                parent.kind(),
                "function_item" | "closure_expression" | "const_item" | "static_item"
            ) {
                return false;
            }
            current = parent.parent();
        }
        true
    }

    /// The type a name written at `at` names, as a fact spells it.
    fn named_type(&self, name: &str, at: Node<'_>) -> Option<String> {
        if matches!(name, "Self" | "_") {
            return None;
        }
        let mut current = at.parent();
        while let Some(node) = current {
            if node.kind() == "block" {
                if let Some(found) = self.plan.lookup_type(&[node.id()], name) {
                    return found.map(str::to_string);
                }
                if let Some(imports) = self.imports.get(&node.id()) {
                    if let Some(binding) = imports.uses.iter().find(|binding| binding.local == name)
                    {
                        return self.imported_type(name, binding);
                    }
                    if imports.glob {
                        return None;
                    }
                }
            } else if is_module_body(node) {
                return self.module_type(node, name, 0);
            }
            current = node.parent();
        }
        None
    }

    /// The type `name` names in a module scope: one it declares, one it
    /// imports, or, through `use super::*` in an inline module, one the
    /// parent module in this file names. Any other glob could bind anything.
    fn module_type(&self, module: Node<'_>, name: &str, depth: usize) -> Option<String> {
        if let Some(declared) = self
            .module_types
            .get(&module.id())
            .and_then(|types| types.get(name))
        {
            let unique = self.type_counts.get(name) == Some(&1);
            return (*declared == Declared::Type && unique).then(|| name.to_string());
        }
        let imports = self.imports.get(&module.id())?;
        if let Some(binding) = imports.uses.iter().find(|binding| binding.local == name) {
            return self.imported_type(name, binding);
        }
        let parent = parent_through_globs(module, &imports.uses, depth)?;
        self.module_type(parent, name, depth + 1)
    }

    fn imported_type(&self, name: &str, binding: &UseBinding) -> Option<String> {
        (binding.internal
            && !binding.glob
            && binding.original == name
            && !self.type_counts.contains_key(name))
        .then(|| name.to_string())
    }

    /// Whether a free function written `name` at `at` is the one module-scope
    /// function of that name this file declares, or a nested one: the entity
    /// name the linker resolves it by.
    fn named_function(&self, name: &str, at: Node<'_>) -> Option<String> {
        let mut current = at.parent();
        while let Some(node) = current {
            if node.kind() == "block" {
                if let Some(found) = self.plan.lookup(&[node.id()], name) {
                    let found = found?;
                    return (self.plan.kind_of(found) == Some(kin_model::EntityKind::Function))
                        .then(|| found.to_string());
                }
                if self.imports.get(&node.id()).is_some_and(|imports| {
                    imports.glob || imports.uses.iter().any(|binding| binding.local == name)
                }) {
                    return None;
                }
            } else if is_module_body(node) {
                return self.module_function(node, name, 0);
            }
            current = node.parent();
        }
        None
    }

    /// The free function `name` names in a module scope, through
    /// `use super::*` in an inline module as for types.
    fn module_function(&self, module: Node<'_>, name: &str, depth: usize) -> Option<String> {
        let declared = self
            .module_functions
            .get(&module.id())
            .is_some_and(|functions| functions.contains(name));
        if declared {
            return (self.function_counts.get(name) == Some(&1)).then(|| name.to_string());
        }
        let imports = self.imports.get(&module.id())?;
        if imports.uses.iter().any(|binding| binding.local == name) {
            return None;
        }
        let parent = parent_through_globs(module, &imports.uses, depth)?;
        self.module_function(parent, name, depth + 1)
    }

    /// Whether `Result<T, ..>` written at `at` yields `T` through `?`.
    fn result_keeps_ok_first(&self, wrapper: Node<'_>, at: Node<'_>) -> bool {
        match wrapper.kind() {
            "type_identifier" => {
                let name = text(wrapper, self.source);
                if self.shadows_std.contains(name) {
                    return false;
                }
                if name == "Result" {
                    if let Some(alias) = self.result_alias_ok {
                        return alias;
                    }
                }
                // The prelude's own, or one a `use` from a module whose alias
                // keeps the success type first binds.
                match self.use_in_scope(name, at) {
                    None => true,
                    Some(binding) => {
                        !binding.glob
                            && binding.original == name
                            && RESULT_MODULES
                                .contains(&binding.module.split("::").next().unwrap_or(""))
                    }
                }
            }
            "scoped_type_identifier" => {
                let spelled = text(wrapper, self.source);
                let root = spelled.split("::").next().unwrap_or("");
                spelled.ends_with("::Result") && RESULT_MODULES.contains(&root)
            }
            _ => false,
        }
    }

    /// The `use` binding `name` resolves to at `at`, when a `use` in an
    /// enclosing block or the enclosing module binds it. A glob in the way
    /// answers as a glob binding.
    fn use_in_scope(&self, name: &str, at: Node<'_>) -> Option<UseBinding> {
        let mut current = at.parent();
        while let Some(node) = current {
            if let Some(imports) = self.imports.get(&node.id()) {
                if let Some(binding) = imports.uses.iter().find(|binding| binding.local == name) {
                    return Some(binding.clone());
                }
                if imports.glob {
                    return imports.uses.iter().find(|binding| binding.glob).cloned();
                }
            }
            if is_module_body(node) {
                return None;
            }
            current = node.parent();
        }
        None
    }
}

/// The parent module a scope's globs read, when every glob it has is
/// `use super::*` in an inline module of this file.
fn parent_through_globs<'n>(
    module: Node<'n>,
    uses: &[UseBinding],
    depth: usize,
) -> Option<Node<'n>> {
    let mut globs = uses.iter().filter(|binding| binding.glob).peekable();
    globs.peek()?;
    if depth >= 8 || !globs.all(|glob| glob.module == "super") {
        return None;
    }
    parent_module_body(module)
}

/// Whether a same-file `type Result<T, ..> = X<T, ..>` alias keeps its first
/// parameter as the success type of a `Result`.
fn result_alias_keeps_ok_first(alias: Node<'_>, source: &[u8]) -> bool {
    let first_parameter = alias
        .child_by_field_name("type_parameters")
        .and_then(|parameters| {
            let mut cursor = parameters.walk();
            let first = parameters
                .named_children(&mut cursor)
                .find(|parameter| parameter.kind() == "type_parameter");
            first
        })
        .and_then(|parameter| parameter.child_by_field_name("name"))
        .map(|name| text(name, source));
    let Some(target) = alias.child_by_field_name("type") else {
        return false;
    };
    let (Some(base), Some(arguments)) = (
        target.child_by_field_name("type"),
        target.child_by_field_name("type_arguments"),
    ) else {
        return false;
    };
    let base_is_result = text(base, source).rsplit("::").next() == Some("Result");
    let first_argument = {
        let mut cursor = arguments.walk();
        let first = arguments
            .named_children(&mut cursor)
            .next()
            .map(|argument| text(argument, source));
        first
    };
    base_is_result && first_parameter.is_some() && first_argument == first_parameter
}

/// One callable being read.
struct Callable<'f, 's, 'tree> {
    file: &'f FileContext<'s, 'tree>,
    node: Node<'tree>,
    /// The impl or trait the callable is a member of.
    container: Option<Node<'tree>>,
    /// How many times each name is bound anywhere in the subtree.
    counts: HashMap<&'s str, usize>,
    /// Every binding site of the subtree, by the name it binds.
    sites: HashMap<&'s str, (Node<'tree>, Node<'tree>, Binder)>,
    /// The callable's own `let` declarations, outside closures and nested
    /// items, in source order.
    lets: Vec<Node<'tree>>,
    /// The receivers `self.<field>` that calls in the body are written
    /// through, with the field each names.
    field_receivers: Vec<(&'s str, &'s str)>,
}

impl<'f, 's, 'tree> Callable<'f, 's, 'tree> {
    fn new(file: &'f FileContext<'s, 'tree>, node: Node<'tree>) -> Self {
        let source = file.source;
        let container = node
            .parent()
            .filter(|parent| parent.kind() == "declaration_list")
            .and_then(|list| list.parent())
            .filter(|owner| matches!(owner.kind(), "impl_item" | "trait_item"));
        // One walk counts every binding in the subtree: patterns, an item
        // nested in the body, which binds its name there too, and a `let` a
        // macro spells, which may bind one the tree cannot show.
        let mut counts: HashMap<&'s str, usize> = HashMap::new();
        let mut sites = HashMap::new();
        let mut bound = Vec::new();
        let mut lets = Vec::new();
        let mut field_receivers: Vec<(&'s str, &'s str)> = Vec::new();
        // Each node with whether it sits in a closure and in a nested item.
        let mut stack = vec![(node, false, false)];
        while let Some((current, in_closure, in_item)) = stack.pop() {
            if !in_item {
                if current.kind() == "let_declaration" && !in_closure {
                    lets.push(current);
                }
                if let Some((receiver, field)) = self_field_receiver(current, source) {
                    if !field_receivers.iter().any(|(seen, _)| *seen == receiver) {
                        field_receivers.push((receiver, field));
                    }
                }
            }
            let binder = match current.kind() {
                "let_declaration" => Some(Binder::Let),
                "parameter" | "closure_parameters" => Some(Binder::Parameter),
                "match_arm" | "let_condition" | "for_expression" => Some(Binder::Scoped),
                "function_item" if current.id() != node.id() => {
                    if let Some(name) = item_name(current, source) {
                        *counts.entry(name).or_default() += 1;
                    }
                    None
                }
                "token_tree" => {
                    let mut cursor = current.walk();
                    let tokens: Vec<Node<'tree>> = current.children(&mut cursor).collect();
                    for pair in tokens.windows(2) {
                        if matches!(pair[0].kind(), "let" | "mut" | "ref" | "mutable_specifier")
                            && pair[1].kind() == "identifier"
                        {
                            *counts.entry(text(pair[1], source)).or_default() += 1;
                        }
                    }
                    None
                }
                _ => None,
            };
            if let Some(binder) = binder {
                bound.clear();
                if current.kind() == "closure_parameters" {
                    let mut cursor = current.walk();
                    for child in current.named_children(&mut cursor) {
                        if child.kind() != "parameter" {
                            pattern_bindings(child, &mut bound);
                        }
                    }
                } else if let Some(pattern) = current.child_by_field_name("pattern") {
                    pattern_bindings(pattern, &mut bound);
                }
                for name in &bound {
                    let spelled = text(*name, source);
                    *counts.entry(spelled).or_default() += 1;
                    sites.insert(spelled, (*name, current, binder));
                }
            }
            let inside_closure = in_closure || current.kind() == "closure_expression";
            let inside_item = in_item
                || (current.id() != node.id()
                    && (current.kind() == "function_item" || file.plan.is_admitted(current)));
            let mut cursor = current.walk();
            stack.extend(
                current
                    .children(&mut cursor)
                    .map(|child| (child, inside_closure, inside_item)),
            );
        }
        lets.sort_by_key(|declaration| declaration.start_byte());
        Self {
            file,
            node,
            container,
            counts,
            sites,
            lets,
            field_receivers,
        }
    }

    /// A callable read only for its signature: its declared types and
    /// return, with no binding census.
    fn shallow(file: &'f FileContext<'s, 'tree>, node: Node<'tree>) -> Self {
        let container = node
            .parent()
            .filter(|parent| parent.kind() == "declaration_list")
            .and_then(|list| list.parent())
            .filter(|owner| matches!(owner.kind(), "impl_item" | "trait_item"));
        Self {
            file,
            node,
            container,
            counts: HashMap::new(),
            sites: HashMap::new(),
            lets: Vec::new(),
            field_receivers: Vec::new(),
        }
    }

    fn source(&self) -> &'s [u8] {
        self.file.source
    }

    fn facts(&self) -> ScopeFacts {
        let mut scope = ScopeFacts {
            returns: self.declared_return(),
            ..ScopeFacts::default()
        };
        let mut bindings: BTreeMap<String, ValueOrigin> = BTreeMap::new();
        if let Some(parameters) = self.node.child_by_field_name("parameters") {
            let mut cursor = parameters.walk();
            let list: Vec<Node<'tree>> = parameters.named_children(&mut cursor).collect();
            for parameter in list {
                if parameter.kind() != "parameter" {
                    continue;
                }
                let (Some(pattern), Some(ty)) = (
                    parameter.child_by_field_name("pattern"),
                    parameter.child_by_field_name("type"),
                ) else {
                    continue;
                };
                if pattern.kind() != "identifier" {
                    continue;
                }
                let name = text(pattern, self.source());
                if self.counts.get(name) != Some(&1) {
                    continue;
                }
                if let Some(type_name) = self.type_origin(ty, self.node) {
                    bindings.insert(name.to_string(), ValueOrigin::Declared { type_name });
                }
            }
        }
        // Lets in source order, so a later one can read an earlier one.
        let lets = &self.lets;
        let candidates: HashSet<&str> = lets
            .iter()
            .filter_map(|declaration| declaration.child_by_field_name("pattern"))
            .filter(|pattern| pattern.kind() == "identifier")
            .map(|pattern| text(pattern, self.source()))
            .filter(|name| self.counts.get(name) == Some(&1))
            .collect();
        let mentions = self.mentions_of(&candidates);
        for &declaration in lets {
            let Some(pattern) = declaration.child_by_field_name("pattern") else {
                continue;
            };
            if pattern.kind() != "identifier" {
                continue;
            }
            let name = text(pattern, self.source());
            if self.counts.get(name) != Some(&1)
                || !self.visible_only_after(name, declaration, &mentions)
            {
                continue;
            }
            let origin = match declaration.child_by_field_name("type") {
                Some(ty) => self
                    .type_origin(ty, declaration)
                    .map(|type_name| ValueOrigin::Declared { type_name }),
                None => declaration
                    .child_by_field_name("value")
                    .and_then(|value| self.expression_origin(value, &bindings)),
            };
            if let Some(origin) = origin {
                bindings.insert(name.to_string(), origin);
            }
        }
        if self.has_self_parameter() {
            for (receiver, field) in &self.field_receivers {
                if let Some(type_name) = self.field_type(field) {
                    bindings.insert(receiver.to_string(), ValueOrigin::Declared { type_name });
                }
            }
        }
        scope.bindings = bindings;
        scope
    }

    /// Every mention of the `names` as a plain identifier in the callable:
    /// byte range and node id. A path's leaf and a lifetime are not one.
    fn mentions_of<'n>(
        &self,
        names: &HashSet<&'n str>,
    ) -> HashMap<&'n str, Vec<(usize, usize, usize)>> {
        let mut mentions: HashMap<&'n str, Vec<(usize, usize, usize)>> = HashMap::new();
        if names.is_empty() {
            return mentions;
        }
        let mut stack = vec![self.node];
        while let Some(current) = stack.pop() {
            if current.kind() == "identifier" {
                if let Some(name) = names.get(text(current, self.source())) {
                    if !is_path_leaf(current)
                        && !current
                            .parent()
                            .is_some_and(|parent| matches!(parent.kind(), "lifetime" | "label"))
                    {
                        mentions.entry(*name).or_default().push((
                            current.start_byte(),
                            current.end_byte(),
                            current.id(),
                        ));
                    }
                }
                continue;
            }
            let mut cursor = current.walk();
            stack.extend(current.children(&mut cursor));
        }
        mentions
    }

    /// Whether every mention of `name` in the callable, other than the `let`
    /// that binds it, sits after that statement and inside its block.
    fn visible_only_after(
        &self,
        name: &str,
        declaration: Node<'tree>,
        mentions: &HashMap<&str, Vec<(usize, usize, usize)>>,
    ) -> bool {
        let Some((binding, _, Binder::Let)) = self.sites.get(name) else {
            return false;
        };
        let Some(block) = declaration.parent() else {
            return false;
        };
        let (from, to) = (declaration.end_byte(), block.end_byte());
        mentions.get(name).is_none_or(|mentions| {
            mentions
                .iter()
                .all(|(start, end, id)| *id == binding.id() || (from <= *start && *end <= to))
        })
    }

    /// The origin of a value an expression evaluates to, when a fact can name
    /// it without a guess.
    fn expression_origin(
        &self,
        expression: Node<'tree>,
        bindings: &BTreeMap<String, ValueOrigin>,
    ) -> Option<ValueOrigin> {
        match expression.kind() {
            "parenthesized_expression" => {
                self.expression_origin(expression.named_child(0)?, bindings)
            }
            "reference_expression" => {
                self.expression_origin(expression.child_by_field_name("value")?, bindings)
            }
            "identifier" => {
                let name = text(expression, self.source());
                bindings.contains_key(name).then(|| ValueOrigin::Local {
                    name: name.to_string(),
                })
            }
            "self" => self
                .self_type()
                .map(|type_name| ValueOrigin::Declared { type_name }),
            "field_expression" => {
                let value = expression.child_by_field_name("value")?;
                let field = expression.child_by_field_name("field")?;
                if value.kind() != "self" {
                    return None;
                }
                self.field_type(text(field, self.source()))
                    .map(|type_name| ValueOrigin::Declared { type_name })
            }
            "struct_expression" => {
                let name = expression.child_by_field_name("name")?;
                let type_name = match name.kind() {
                    "type_identifier" => self.named(text(name, self.source()), expression)?,
                    "generic_type_with_turbofish" => {
                        let base = name.child_by_field_name("type")?;
                        (base.kind() == "type_identifier")
                            .then(|| self.named(text(base, self.source()), expression))??
                    }
                    _ => return None,
                };
                Some(ValueOrigin::Declared { type_name })
            }
            "call_expression" => self.call_origin(expression, bindings),
            "try_expression" => self
                .unwrapped(expression.named_child(0)?)
                .map(|type_name| ValueOrigin::Declared { type_name }),
            _ => None,
        }
    }

    fn call_origin(
        &self,
        call: Node<'tree>,
        bindings: &BTreeMap<String, ValueOrigin>,
    ) -> Option<ValueOrigin> {
        let mut function = call.child_by_field_name("function")?;
        if function.kind() == "generic_function" {
            function = function.child_by_field_name("function")?;
        }
        match function.kind() {
            "identifier" => {
                let callee = text(function, self.source());
                if self.counts.contains_key(callee)
                    && self.file.plan.lookup(&self.blocks(call), callee).is_none()
                {
                    // A local value, such as a closure, not an item.
                    return None;
                }
                if let Some(type_name) = self.named(callee, call) {
                    // A tuple struct's constructor.
                    return Some(ValueOrigin::Call { callee: type_name });
                }
                // A function this file declares, whose declared return names a
                // type: any other call's value is one no fact can follow.
                let callee = self.file.named_function(callee, call)?;
                let declaration = self.function_declaration(&callee)?;
                Callable::shallow(self.file, declaration)
                    .declared_return()
                    .map(|_| ValueOrigin::Call { callee })
            }
            "scoped_identifier" => {
                let path = function.child_by_field_name("path")?;
                let method = text(function.child_by_field_name("name")?, self.source());
                let owner = match path.kind() {
                    "identifier" if text(path, self.source()) == "Self" => self.self_type()?,
                    "identifier" => self.named(text(path, self.source()), call)?,
                    "generic_type" => {
                        let base = path.child_by_field_name("type")?;
                        (base.kind() == "type_identifier")
                            .then(|| self.named(text(base, self.source()), call))??
                    }
                    _ => return None,
                };
                Some(ValueOrigin::MethodCall {
                    receiver: Box::new(ValueOrigin::Declared { type_name: owner }),
                    method: method.to_string(),
                })
            }
            "field_expression" => {
                let receiver = function.child_by_field_name("value")?;
                let method = text(function.child_by_field_name("field")?, self.source());
                if matches!(method, "unwrap" | "expect") {
                    if let Some(type_name) = self.unwrapped(receiver) {
                        return Some(ValueOrigin::Declared { type_name });
                    }
                }
                let receiver = self.expression_origin(receiver, bindings)?;
                Some(ValueOrigin::MethodCall {
                    receiver: Box::new(receiver),
                    method: method.to_string(),
                })
            }
            _ => None,
        }
    }

    /// The success type of a fallible call this file declares the callee of:
    /// `Store::open(p)?` where `impl Store { fn open(..) -> Result<Self, E> }`
    /// sits in this file, or a free function declared here returning
    /// `Option<Store>`. `?`, `unwrap` and `expect` yield that type.
    fn unwrapped(&self, expression: Node<'tree>) -> Option<String> {
        if expression.kind() != "call_expression" {
            return None;
        }
        let mut function = expression.child_by_field_name("function")?;
        if function.kind() == "generic_function" {
            function = function.child_by_field_name("function")?;
        }
        let declaration = match function.kind() {
            "identifier" => {
                let callee = text(function, self.source());
                if self.counts.contains_key(callee) {
                    return None;
                }
                let entity = self.file.named_function(callee, expression)?;
                self.function_declaration(&entity)?
            }
            "scoped_identifier" => {
                let path = function.child_by_field_name("path")?;
                let method = text(function.child_by_field_name("name")?, self.source());
                let owner = match text(path, self.source()) {
                    "Self" => self.self_type()?,
                    written if path.kind() == "identifier" => self.named(written, expression)?,
                    _ => return None,
                };
                self.inherent_method(&owner, method)?
            }
            _ => return None,
        };
        if is_async(declaration, self.source()) {
            return None;
        }
        let returns = declaration.child_by_field_name("return_type")?;
        if returns.kind() != "generic_type" {
            return None;
        }
        let wrapper = returns.child_by_field_name("type")?;
        let leaf = text(wrapper, self.source())
            .rsplit("::")
            .next()
            .unwrap_or("");
        let admitted = match leaf {
            "Result" => self.file.result_keeps_ok_first(wrapper, declaration),
            "Option" => {
                wrapper.kind() == "type_identifier"
                    && !self.file.shadows_std.contains("Option")
                    && self.file.use_in_scope("Option", declaration).is_none()
            }
            _ => false,
        };
        if !admitted {
            return None;
        }
        let arguments = returns.child_by_field_name("type_arguments")?;
        let mut cursor = arguments.walk();
        let first = arguments.named_children(&mut cursor).next()?;
        Callable::shallow(self.file, declaration).type_origin(first, declaration)
    }

    /// The function item a free-function entity name names in this file.
    fn function_declaration(&self, entity: &str) -> Option<Node<'tree>> {
        if let Some(item) = self
            .file
            .plan
            .nested
            .iter()
            .find(|item| item.name == entity)
        {
            return Some(item.node);
        }
        match self.file.function_nodes.get(entity)?.as_slice() {
            [one] => Some(*one),
            _ => None,
        }
    }

    /// The one method `name` an inherent impl of `owner` in this file
    /// declares. An inherent method is the one a path call reaches, and two
    /// inherent methods of one name do not compile, so the file's own is the
    /// only candidate when it has one.
    fn inherent_method(&self, owner: &str, name: &str) -> Option<Node<'tree>> {
        let mut found = None;
        for (item, member) in self.file.inherent_methods.get(name)? {
            let written = item.child_by_field_name("type").and_then(|ty| {
                let base = match ty.kind() {
                    "generic_type" => ty.child_by_field_name("type")?,
                    _ => ty,
                };
                (base.kind() == "type_identifier")
                    .then(|| self.named(text(base, self.source()), *item))
                    .flatten()
            });
            if written.as_deref() == Some(owner) {
                if found.is_some() {
                    return None;
                }
                found = Some(*member);
            }
        }
        found
    }

    fn has_self_parameter(&self) -> bool {
        let Some(parameters) = self.node.child_by_field_name("parameters") else {
            return false;
        };
        let mut cursor = parameters.walk();
        let found = parameters.named_children(&mut cursor).any(|parameter| {
            parameter.kind() == "self_parameter"
                || (parameter.kind() == "parameter"
                    && parameter
                        .child_by_field_name("pattern")
                        .is_some_and(|pattern| pattern.kind() == "self"))
        });
        found
    }

    /// The type a field of the callable's own struct is declared with.
    fn field_type(&self, field: &str) -> Option<String> {
        let owner = self.self_type()?;
        let declaration = *self.file.structs.get(&owner)?;
        let body = declaration.child_by_field_name("body")?;
        let generics: HashSet<&str> = type_parameter_names(declaration, self.source());
        let mut cursor = body.walk();
        let found = body
            .named_children(&mut cursor)
            .filter(|member| member.kind() == "field_declaration")
            .find(|member| {
                member
                    .child_by_field_name("name")
                    .is_some_and(|name| text(name, self.source()) == field)
            })?;
        let ty = found.child_by_field_name("type")?;
        if mentions_any(ty, self.source(), &generics) {
            return None;
        }
        let at_struct = Callable::shallow(self.file, declaration);
        at_struct.type_origin(ty, declaration)
    }

    /// The type `Self` names in this callable: the impl's own type. `None` in
    /// a trait, where `Self` is whatever implements it, and outside any impl.
    fn self_type(&self) -> Option<String> {
        let container = self.container?;
        if container.kind() != "impl_item" {
            return None;
        }
        let ty = container.child_by_field_name("type")?;
        let base = match ty.kind() {
            "type_identifier" => ty,
            "generic_type" => ty.child_by_field_name("type")?,
            _ => return None,
        };
        if base.kind() != "type_identifier" {
            return None;
        }
        let name = text(base, self.source());
        // A blanket impl's `Self` is any type the bound admits.
        if type_parameter_names(container, self.source()).contains(name) {
            return None;
        }
        self.file.named_type(name, container)
    }

    fn declared_return(&self) -> Option<DeclaredReturn> {
        let returns = self.node.child_by_field_name("return_type")?;
        if is_async(self.node, self.source()) {
            return None;
        }
        let mut target = returns;
        while target.kind() == "reference_type" {
            target = target.child_by_field_name("type")?;
        }
        if target.kind() == "type_identifier" && text(target, self.source()) == "Self" {
            return (self.container?.kind() == "impl_item" && self.self_type().is_some())
                .then_some(DeclaredReturn::Receiver);
        }
        self.type_origin(returns, self.node)
            .map(|type_name| DeclaredReturn::Type { type_name })
    }

    /// The type a type expression names, as a fact spells it.
    fn type_origin(&self, ty: Node<'tree>, at: Node<'tree>) -> Option<String> {
        match ty.kind() {
            "reference_type" => self.type_origin(ty.child_by_field_name("type")?, at),
            "type_identifier" => self.named(text(ty, self.source()), at),
            "generic_type" => {
                let base = ty.child_by_field_name("type")?;
                (base.kind() == "type_identifier")
                    .then(|| self.named(text(base, self.source()), at))?
            }
            "dynamic_type" | "abstract_type" | "bounded_type" => {
                let mut bounds = Vec::new();
                bound_list(ty, &mut bounds);
                self.single_trait(&bounds, at)
            }
            "tuple_type" => {
                // `&(dyn Sink + Send)` parenthesizes one type.
                let mut cursor = ty.walk();
                let inner: Vec<Node<'tree>> = ty.named_children(&mut cursor).collect();
                match inner.as_slice() {
                    [one] if matches!(one.kind(), "bounded_type" | "dynamic_type") => {
                        self.type_origin(*one, at)
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// A type name written at `at`: `Self`, a generic parameter read through
    /// its bound, or a type the file settles.
    fn named(&self, name: &str, at: Node<'tree>) -> Option<String> {
        if name == "Self" {
            return self.self_type();
        }
        if let Some(bounds) = self.generic_bounds(name, at) {
            return self.single_trait(&bounds, at);
        }
        self.file.named_type(name, at)
    }

    /// The trait a bound list names once marker traits and lifetimes are set
    /// aside, when exactly one is left and the file settles it.
    fn single_trait(&self, bounds: &[Node<'tree>], at: Node<'tree>) -> Option<String> {
        let mut traits = bounds.iter().filter(|bound| {
            !matches!(bound.kind(), "lifetime" | "removed_trait_bound")
                && !MARKER_TRAITS.contains(&bound_base_name(**bound, self.source()).unwrap_or(""))
        });
        let only = traits.next()?;
        if traits.next().is_some() {
            return None;
        }
        let base = match only.kind() {
            "type_identifier" => *only,
            "generic_type" => only.child_by_field_name("type")?,
            _ => return None,
        };
        (base.kind() == "type_identifier")
            .then(|| self.file.named_type(text(base, self.source()), at))?
    }

    /// The bounds a generic parameter named `name` carries where `at` sits,
    /// or `None` when no enclosing generic list declares it. Methods see
    /// their impl's or trait's parameters; a nested item sees none of the
    /// callable around it.
    fn generic_bounds(&self, name: &str, at: Node<'tree>) -> Option<Vec<Node<'tree>>> {
        let mut current = Some(at);
        while let Some(node) = current {
            match node.kind() {
                "function_item" | "impl_item" | "trait_item" | "struct_item" | "enum_item"
                | "union_item" | "type_item" => {
                    if let Some(bounds) = self.declared_bounds(node, name) {
                        return Some(bounds);
                    }
                    let member_of_container = node.kind() == "function_item"
                        && node
                            .parent()
                            .filter(|parent| parent.kind() == "declaration_list")
                            .and_then(|list| list.parent())
                            .is_some_and(|owner| {
                                matches!(owner.kind(), "impl_item" | "trait_item")
                            });
                    if !member_of_container {
                        return None;
                    }
                }
                _ => {}
            }
            current = node.parent();
        }
        None
    }

    /// The bounds `item`'s own generic list and where clause give `name`,
    /// when the list declares it.
    fn declared_bounds(&self, item: Node<'tree>, name: &str) -> Option<Vec<Node<'tree>>> {
        let parameters = item.child_by_field_name("type_parameters")?;
        let mut cursor = parameters.walk();
        let declared = parameters
            .named_children(&mut cursor)
            .filter(|parameter| parameter.kind() == "type_parameter")
            .find(|parameter| {
                parameter
                    .child_by_field_name("name")
                    .is_some_and(|spelled| text(spelled, self.source()) == name)
            })?;
        let mut bounds = Vec::new();
        if let Some(list) = declared.child_by_field_name("bounds") {
            collect_bounds(list, &mut bounds);
        }
        let mut cursor = item.walk();
        let clauses: Vec<Node<'tree>> = item
            .named_children(&mut cursor)
            .filter(|child| child.kind() == "where_clause")
            .collect();
        for clause in clauses {
            let mut predicates = clause.walk();
            let predicates: Vec<Node<'tree>> = clause.named_children(&mut predicates).collect();
            for predicate in predicates {
                let left = predicate.child_by_field_name("left");
                if left.is_some_and(|left| {
                    left.kind() == "type_identifier" && text(left, self.source()) == name
                }) {
                    if let Some(list) = predicate.child_by_field_name("bounds") {
                        collect_bounds(list, &mut bounds);
                    }
                }
            }
        }
        Some(bounds)
    }

    /// The blocks around `node` that declare items.
    fn blocks(&self, node: Node<'tree>) -> Vec<usize> {
        self.file.plan.blocks_around(node)
    }
}

/// The receiver `self.<field>` a method call is written through, with the
/// field it names: `self.store` and `store` for `self.store.load()`.
fn self_field_receiver<'s>(call: Node<'_>, source: &'s [u8]) -> Option<(&'s str, &'s str)> {
    if call.kind() != "call_expression" {
        return None;
    }
    let receiver = call
        .child_by_field_name("function")
        .filter(|function| function.kind() == "field_expression")?
        .child_by_field_name("value")
        .filter(|value| value.kind() == "field_expression")?;
    let object = receiver.child_by_field_name("value")?;
    let field = receiver.child_by_field_name("field")?;
    (object.kind() == "self" && field.kind() == "field_identifier")
        .then(|| (text(receiver, source), text(field, source)))
}

/// Whether an identifier is the leaf of a path, `x` in `a::x`, which names an
/// item and never a local.
fn is_path_leaf(identifier: Node<'_>) -> bool {
    identifier.parent().is_some_and(|parent| {
        matches!(
            parent.kind(),
            "scoped_identifier" | "scoped_type_identifier"
        ) && parent
            .child_by_field_name("name")
            .is_some_and(|name| name.id() == identifier.id())
    })
}

/// The entries of a `trait_bounds` list.
fn collect_bounds<'tree>(list: Node<'tree>, out: &mut Vec<Node<'tree>>) {
    let mut cursor = list.walk();
    out.extend(list.named_children(&mut cursor));
}

/// The traits a `dyn`, `impl` or `+` type names, flattened.
fn bound_list<'tree>(ty: Node<'tree>, out: &mut Vec<Node<'tree>>) {
    match ty.kind() {
        "dynamic_type" | "abstract_type" => {
            if let Some(bound) = ty.child_by_field_name("trait") {
                out.push(bound);
            }
        }
        "bounded_type" => {
            let mut cursor = ty.walk();
            let parts: Vec<Node<'tree>> = ty.named_children(&mut cursor).collect();
            for part in parts {
                bound_list(part, out);
            }
        }
        _ => out.push(ty),
    }
}

/// The name a bound's trait path ends in, generic arguments aside.
fn bound_base_name<'s>(bound: Node<'_>, source: &'s [u8]) -> Option<&'s str> {
    let base = match bound.kind() {
        "generic_type" => bound.child_by_field_name("type")?,
        _ => bound,
    };
    Some(text(base, source).rsplit("::").next().unwrap_or(""))
}

/// The generic type parameter names an item declares.
fn type_parameter_names<'s>(item: Node<'_>, source: &'s [u8]) -> HashSet<&'s str> {
    let mut names = HashSet::new();
    if let Some(parameters) = item.child_by_field_name("type_parameters") {
        let mut cursor = parameters.walk();
        for parameter in parameters.named_children(&mut cursor) {
            if let Some(name) = parameter.child_by_field_name("name") {
                names.insert(text(name, source));
            }
        }
    }
    names
}

/// Whether a type mentions any of `names`.
fn mentions_any(ty: Node<'_>, source: &[u8], names: &HashSet<&str>) -> bool {
    if names.is_empty() {
        return false;
    }
    let mut stack = vec![ty];
    while let Some(node) = stack.pop() {
        if node.kind() == "type_identifier" && names.contains(text(node, source)) {
            return true;
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    false
}

#[cfg(test)]
mod tests {
    use kin_model::{Entity, EntityKind, FilePathId};

    use crate::adapter::LanguageAdapter;
    use crate::languages::RustAdapter;
    use crate::value_origins::{
        attach_value_origins, read_value_origins, DeclaredReturn, ScopeFacts, ValueOrigin,
    };

    fn attached(path: &str, source: &str) -> Vec<Entity> {
        let bytes = source.as_bytes();
        let tree = RustAdapter.parse(bytes).unwrap();
        assert!(
            !tree.root_node().has_error(),
            "{}",
            tree.root_node().to_sexp()
        );
        let file_id = FilePathId::new(path);
        let output = RustAdapter.extract(&tree, bytes, &file_id).unwrap();
        let mut entities: Vec<Entity> = output
            .entities
            .into_iter()
            .map(|entity| entity.into_entity(kin_model::LanguageId::Rust, &file_id))
            .collect();
        attach_value_origins(
            &tree,
            bytes,
            kin_model::LanguageId::Rust,
            path,
            &mut entities,
        );
        entities
    }

    fn facts_on(entities: &[Entity], name: &str) -> ScopeFacts {
        let entity = entities
            .iter()
            .find(|entity| {
                matches!(entity.kind, EntityKind::Function | EntityKind::Method)
                    && entity.name == name
            })
            .unwrap_or_else(|| {
                panic!(
                    "no callable {name} in {:?}",
                    entities.iter().map(|e| e.name.as_str()).collect::<Vec<_>>()
                )
            });
        read_value_origins(entity).unwrap_or_default()
    }

    fn declared(type_name: &str) -> ValueOrigin {
        ValueOrigin::Declared {
            type_name: type_name.into(),
        }
    }

    fn method_call(receiver: ValueOrigin, method: &str) -> ValueOrigin {
        ValueOrigin::MethodCall {
            receiver: Box::new(receiver),
            method: method.into(),
        }
    }

    const STORE: &str = r#"use std::io;
use std::sync::Arc;
use crate::cache::Cache;
use reqwest::Client;

pub trait Sink {
    fn matched(&mut self) -> bool;
    fn child(&self) -> Store;
    fn boxed(&self) -> Self where Self: Sized;
}

pub struct Store {
    cache: Cache,
    shared: Arc<Cache>,
    client: Client,
}

impl Store {
    pub fn new(path: &str) -> Self {
        let _ = path;
        Store { cache: Cache::default(), shared: Arc::default(), client: Client::new() }
    }

    pub fn open(path: &str) -> io::Result<Self> {
        Ok(Self::new(path))
    }

    pub fn reader(&self) -> &Cache {
        &self.cache
    }

    pub fn with_limit(&mut self, limit: usize) -> &mut Self {
        let _ = limit;
        self
    }

    pub fn load(&self) -> Vec<u8> {
        self.cache.load();
        self.shared.load();
        self.client.load();
        let r = self.reader();
        r.load();
        Vec::new()
    }
}

pub fn run(store: &Store, sink: &mut dyn Sink, cache: Arc<Cache>, client: Client) -> io::Result<()> {
    let a: Store = Store::new("a");
    let b = Store::new("b");
    let c = Store { cache: Cache::default(), shared: cache, client };
    let d = Store::open("d")?;
    let e = Store::open("e").expect("opens");
    let x = b.reader();
    for item in [&a, &c, &d, &e] {
        item.load();
    }
    x.load();
    store.load();
    sink.matched();
    Ok(())
}

pub fn generic<S: Sink + Send>(s: &mut S, t: impl Sink, u: &(dyn Sink + Send), w: &mut dyn Sink) {
    s.matched();
    let _ = (t, u, w);
}

pub fn two_bounds<S: Sink + Clone>(s: S) -> S {
    s.clone()
}

pub fn opaque() -> impl Sink {
    unimplemented!()
}
"#;

    #[test]
    fn declared_types_constructors_and_returns_become_facts() {
        let entities = attached("src/store.rs", STORE);
        let run = facts_on(&entities, "run");
        assert_eq!(run.bindings.get("store"), Some(&declared("Store")));
        assert_eq!(run.bindings.get("sink"), Some(&declared("Sink")));
        assert_eq!(run.bindings.get("a"), Some(&declared("Store")));
        assert_eq!(
            run.bindings.get("b"),
            Some(&method_call(declared("Store"), "new"))
        );
        assert_eq!(run.bindings.get("c"), Some(&declared("Store")));
        assert_eq!(
            run.bindings.get("d"),
            Some(&declared("Store")),
            "`?` on a same-file `io::Result<Self>` yields the type"
        );
        assert_eq!(run.bindings.get("e"), Some(&declared("Store")));
        assert_eq!(
            run.bindings.get("x"),
            Some(&method_call(
                ValueOrigin::Local { name: "b".into() },
                "reader"
            ))
        );
        for untyped in ["cache", "client", "item"] {
            assert!(
                !run.bindings.contains_key(untyped),
                "{untyped} must carry no fact: {:?}",
                run.bindings
            );
        }

        assert_eq!(
            facts_on(&entities, "Store::new").returns,
            Some(DeclaredReturn::Receiver)
        );
        assert_eq!(facts_on(&entities, "Store::open").returns, None);
        assert_eq!(
            facts_on(&entities, "Store::reader").returns,
            Some(DeclaredReturn::Type {
                type_name: "Cache".into()
            }),
            "a type an internal `use` binds under its own name"
        );
        assert_eq!(
            facts_on(&entities, "Store::with_limit").returns,
            Some(DeclaredReturn::Receiver)
        );
        assert_eq!(
            facts_on(&entities, "Sink::child").returns,
            Some(DeclaredReturn::Type {
                type_name: "Store".into()
            })
        );
        assert_eq!(
            facts_on(&entities, "Sink::boxed").returns,
            None,
            "a trait's `Self` is whatever implements it"
        );
        assert_eq!(
            facts_on(&entities, "opaque").returns,
            Some(DeclaredReturn::Type {
                type_name: "Sink".into()
            })
        );
    }

    #[test]
    fn a_self_field_receiver_is_typed_by_the_struct_declaration() {
        let entities = attached("src/store.rs", STORE);
        let load = facts_on(&entities, "Store::load");
        assert_eq!(load.bindings.get("self.cache"), Some(&declared("Cache")));
        assert!(
            !load.bindings.contains_key("self.shared"),
            "`Arc<Cache>::clone` is not `Cache::clone`, so a wrapper types nothing"
        );
        assert!(
            !load.bindings.contains_key("self.client"),
            "a type an external `use` binds may share a repository type's name"
        );
        assert_eq!(
            load.bindings.get("r"),
            Some(&method_call(declared("Store"), "reader"))
        );
    }

    #[test]
    fn trait_objects_and_bounded_generics_are_typed_as_the_trait() {
        let entities = attached("src/store.rs", STORE);
        let generic = facts_on(&entities, "generic");
        for name in ["s", "t", "u", "w"] {
            assert_eq!(
                generic.bindings.get(name),
                Some(&declared("Sink")),
                "{name}"
            );
        }
        assert!(
            facts_on(&entities, "two_bounds").bindings.is_empty(),
            "two traits that declare methods leave the receiver undecided"
        );
    }

    #[test]
    fn a_rebinding_an_earlier_mention_or_a_macro_let_withholds_the_fact() {
        let entities = attached(
            "src/work.rs",
            r#"pub struct Store;
impl Store {
    pub fn new() -> Self { Store }
    pub fn load(&self) {}
}
pub struct Cache;
static s: Store = Store;

pub fn shadowed() {
    let s = Store::new();
    let s = Cache;
    s.load();
}

pub fn mentioned_before() {
    s.load();
    let s = Store::new();
    s.load();
}

pub fn closure_parameter(items: &[Cache]) {
    let s = Store::new();
    items.iter().for_each(|s| drop(s));
    s.load();
}

pub fn macro_let() {
    let s = Store::new();
    bind!(let s = Cache);
    s.load();
}

pub fn in_an_assertion() {
    let s = Store::new();
    assert!(s.load() == ());
}
"#,
        );
        for name in [
            "shadowed",
            "mentioned_before",
            "closure_parameter",
            "macro_let",
        ] {
            assert!(
                !facts_on(&entities, name).bindings.contains_key("s"),
                "{name}"
            );
        }
        assert_eq!(
            facts_on(&entities, "in_an_assertion").bindings.get("s"),
            Some(&method_call(declared("Store"), "new"))
        );
    }

    #[test]
    fn names_the_file_cannot_settle_carry_no_fact() {
        let entities = attached(
            "src/tests.rs",
            r#"use super::*;
use std::io::Error;
use crate::store::Store as Db;

type Alias = Thing;
pub struct Thing;
mod inner { pub struct Thing; }

pub fn globbed(x: Widget, e: Error, d: Db, a: Alias, t: Thing, v: Vec<Thing>, p: crate::store::Store) {
    let _ = (x, e, d, a, t, v, p);
}
"#,
        );
        let facts = facts_on(&entities, "globbed");
        assert!(facts.bindings.is_empty(), "{:?}", facts.bindings);
    }

    #[test]
    fn a_test_module_reads_its_parents_types_through_its_glob() {
        let entities = attached(
            "src/store.rs",
            r#"use crate::cache::Cache;

pub struct Store;

impl Store {
    pub fn new() -> Self { Store }
    pub fn load(&self) {}
}

fn build() -> Store { Store }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads() {
        let store = Store::new();
        let built = build();
        let cache: Cache = Cache::default();
        store.load();
        let _ = (built, cache);
    }
}
"#,
        );
        let test = facts_on(&entities, "loads");
        assert_eq!(
            test.bindings.get("store"),
            Some(&method_call(declared("Store"), "new"))
        );
        assert_eq!(
            test.bindings.get("built"),
            Some(&ValueOrigin::Call {
                callee: "build".into()
            })
        );
        assert_eq!(test.bindings.get("cache"), Some(&declared("Cache")));
    }

    #[test]
    fn a_struct_and_impl_in_a_test_body_type_its_locals() {
        let entities = attached(
            "src/lib.rs",
            r#"pub trait Sink { fn hit(&mut self); }

#[test]
fn counts_hits() {
    struct Fake { hits: u32 }
    impl Fake {
        fn new() -> Self { Fake { hits: 0 } }
    }
    impl Sink for Fake {
        fn hit(&mut self) { self.hits += 1; }
    }
    let mut fake = Fake::new();
    fake.hit();
    let other = Fake { hits: 1 };
    let _ = other;
}
"#,
        );
        let test = facts_on(&entities, "counts_hits");
        assert_eq!(
            test.bindings.get("fake"),
            Some(&method_call(declared("counts_hits.<locals>.Fake"), "new"))
        );
        assert_eq!(
            test.bindings.get("other"),
            Some(&declared("counts_hits.<locals>.Fake"))
        );
        assert_eq!(
            facts_on(&entities, "counts_hits.<locals>.Fake::new").returns,
            Some(DeclaredReturn::Receiver)
        );
    }
}
