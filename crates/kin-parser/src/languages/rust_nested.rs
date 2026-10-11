// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Rust definitions nested in function bodies, and the scoping facts the Rust
//! adapter's readers share.
//!
//! A Rust function body can declare items: helper functions, structs, enums,
//! traits and the impls that give them methods, consts, statics and macros.
//! Tests lean on it (a fake `struct` and its `impl Trait` inside a `#[test]`),
//! and so does code that keeps a helper beside its only caller. An item in a
//! block is visible to the whole block, before its declaration too, and inside
//! every item nested in that block, but never outside the block.
//!
//! The plan here names each such item by the rule in [`crate::nested`]:
//! `outer.<locals>.helper`, `outer.<locals>.Fake`, and a method of a nested
//! type keeps the adapter's member rule on that name,
//! `outer.<locals>.Fake::matched`. An impl for a type declared outside the
//! function keeps the type's ordinary name, since its methods are that type's
//! methods wherever the impl is written. The first item in source order to
//! claim a local name keeps it. A second one, a method of a nested type that
//! was not admitted, and a module declared in a function body are recorded as
//! not admitted, and whatever they hold stays with the entity around them.
//!
//! The extractor, the value-origin reader and the value-escape collector read
//! one plan, so all three agree on which nested items are entities and what
//! they are called.

use std::collections::{HashMap, HashSet};

use kin_model::EntityKind;
use tree_sitter::{Node, Tree};

use crate::nested::{
    is_local_scope_name, local_scope_name, LocalScopeNames, NestedDefinitionCensus,
    UnadmittedNestedDefinition,
};

/// Whether `kind` is an item a Rust block, module, impl or trait can declare.
pub(crate) fn is_item_kind(kind: &str) -> bool {
    matches!(
        kind,
        "function_item"
            | "function_signature_item"
            | "struct_item"
            | "union_item"
            | "enum_item"
            | "trait_item"
            | "impl_item"
            | "type_item"
            | "const_item"
            | "static_item"
            | "macro_definition"
            | "mod_item"
            | "foreign_mod_item"
            | "associated_type"
    )
}

/// The node's text, or an empty string.
pub(crate) fn text<'s>(node: Node<'_>, source: &'s [u8]) -> &'s str {
    node.utf8_text(source).unwrap_or("")
}

/// The plain name an item declares. A name a macro metavariable spells is
/// none, since it is not known until the macro expands.
pub(crate) fn item_name<'s>(item: Node<'_>, source: &'s [u8]) -> Option<&'s str> {
    let name = item.child_by_field_name("name")?;
    if !matches!(name.kind(), "identifier" | "type_identifier") {
        return None;
    }
    let spelled = text(name, source);
    (!spelled.is_empty()).then_some(spelled)
}

/// Whether `node` is a callable declared as an async function, whose call
/// yields a future rather than the type its signature names.
pub(crate) fn is_async(function: Node<'_>, source: &[u8]) -> bool {
    let mut cursor = function.walk();
    let found = function
        .children(&mut cursor)
        .filter(|child| child.kind() == "function_modifiers")
        .any(|modifiers| {
            text(modifiers, source)
                .split_whitespace()
                .any(|word| word == "async")
        });
    found
}

/// Whether a `declaration_list` is the body of a module rather than of an
/// impl, a trait or an extern block.
pub(crate) fn is_module_body(node: Node<'_>) -> bool {
    node.kind() == "source_file"
        || (node.kind() == "declaration_list"
            && node
                .parent()
                .is_some_and(|parent| parent.kind() == "mod_item"))
}

/// The body of the module that encloses the inline module whose body is
/// `scope`, when `scope` is an inline module's body.
pub(crate) fn parent_module_body(scope: Node<'_>) -> Option<Node<'_>> {
    let module = scope
        .parent()
        .filter(|parent| parent.kind() == "mod_item")?;
    let mut current = module.parent();
    while let Some(node) = current {
        if is_module_body(node) {
            return Some(node);
        }
        current = node.parent();
    }
    None
}

/// One definition nested in a function body that the adapter admits.
pub(crate) struct NestedItem<'tree> {
    pub(crate) node: Node<'tree>,
    /// The entity name.
    pub(crate) name: String,
    pub(crate) kind: EntityKind,
    /// For a method of a nested impl or trait, the owner as the member rule
    /// names it.
    pub(crate) owner: Option<String>,
    /// For a method of `impl Trait for Type`, the trait it satisfies.
    pub(crate) implements: Option<String>,
    /// The blocks whose items are visible inside the definition, outermost
    /// first.
    pub(crate) inherited: Vec<usize>,
}

/// Every nested definition of one file, named and admitted or recorded.
#[derive(Default)]
pub(crate) struct RustPlan<'tree> {
    /// Admitted nested definitions, in source order.
    pub(crate) nested: Vec<NestedItem<'tree>>,
    /// Node ids of the admitted nested definitions.
    admitted: HashSet<usize>,
    /// The items each block declares directly, by block node id: the raw name
    /// to the entity name, or `None` when the item was not admitted.
    block_items: HashMap<usize, HashMap<String, Option<String>>>,
    /// Entity kinds of the admitted local-scope names.
    kinds: HashMap<String, EntityKind>,
    /// `(owner, trait)` for every nested `impl Trait for Type`.
    pub(crate) trait_impls: Vec<(String, String)>,
    /// Definitions found nested and not admitted.
    pub(crate) census: NestedDefinitionCensus,
}

impl<'tree> RustPlan<'tree> {
    /// Whether the extractor admits `node` as a nested entity of its own.
    pub(crate) fn is_admitted(&self, node: Node<'_>) -> bool {
        self.admitted.contains(&node.id())
    }

    /// Whether any block of the file declares an item, so a call could name
    /// a nested one.
    pub(crate) fn has_block_items(&self) -> bool {
        !self.block_items.is_empty()
    }

    /// Whether the block declares any item, so a walk has a scope to push.
    pub(crate) fn declares_items(&self, block: Node<'_>) -> bool {
        self.block_items.contains_key(&block.id())
    }

    /// What `name` reads as inside the blocks `stack` (outermost first):
    /// `Some(Some(entity))` for an admitted nested item, `Some(None)` for one
    /// the plan did not admit, `None` when no block in the stack declares it.
    pub(crate) fn lookup(&self, stack: &[usize], name: &str) -> Option<Option<&str>> {
        stack.iter().rev().find_map(|block| {
            self.block_items
                .get(block)
                .and_then(|items| items.get(name))
                .map(|entity| entity.as_deref())
        })
    }

    /// The local entity a type name reads as inside `stack`, when the
    /// nearest declaration of the name is an admitted type.
    pub(crate) fn lookup_type(&self, stack: &[usize], name: &str) -> Option<Option<&str>> {
        self.lookup(stack, name).map(|entity| {
            entity.filter(|entity| {
                matches!(
                    self.kinds.get(*entity),
                    Some(
                        EntityKind::Class
                            | EntityKind::EnumDef
                            | EntityKind::TraitDef
                            | EntityKind::TypeAlias
                    )
                )
            })
        })
    }

    /// The entity kind of an admitted local-scope name.
    pub(crate) fn kind_of(&self, entity: &str) -> Option<EntityKind> {
        self.kinds.get(entity).copied()
    }

    /// The block ids enclosing `node` inside callable bodies, outermost first:
    /// the stack a lookup from `node` reads.
    pub(crate) fn blocks_around(&self, node: Node<'_>) -> Vec<usize> {
        let mut blocks = Vec::new();
        let mut current = node.parent();
        while let Some(parent) = current {
            if parent.kind() == "block" && self.block_items.contains_key(&parent.id()) {
                blocks.push(parent.id());
            }
            current = parent.parent();
        }
        blocks.reverse();
        blocks
    }
}

/// Plan every nested definition of a parsed Rust file.
pub(crate) fn plan<'tree>(tree: &'tree Tree, source: &[u8]) -> RustPlan<'tree> {
    let mut state = PlanState {
        source,
        plan: RustPlan::default(),
        names: LocalScopeNames::default(),
    };
    state.walk_module(tree.root_node());
    state
        .plan
        .census
        .not_admitted
        .sort_by_key(|entry| entry.start_line);
    state.plan
}

struct PlanState<'s, 'tree> {
    source: &'s [u8],
    plan: RustPlan<'tree>,
    names: LocalScopeNames,
}

impl<'tree> PlanState<'_, 'tree> {
    /// Module scope: the callables here keep the adapter's ordinary names.
    fn walk_module(&mut self, module: Node<'tree>) {
        let mut cursor = module.walk();
        let children: Vec<Node<'tree>> = module.named_children(&mut cursor).collect();
        for child in children {
            match child.kind() {
                "function_item" => {
                    if let Some(name) = item_name(child, self.source) {
                        let name = name.to_string();
                        self.walk_callable(child, &name, &mut Vec::new());
                    }
                }
                "impl_item" => {
                    let owner = child
                        .child_by_field_name("type")
                        .map(|ty| text(ty, self.source).to_string())
                        .unwrap_or_default();
                    for member in members(child, &["function_item"]) {
                        let Some(method) = item_name(member, self.source) else {
                            continue;
                        };
                        let name = if owner.is_empty() {
                            method.to_string()
                        } else {
                            format!("{owner}::{method}")
                        };
                        self.walk_callable(member, &name, &mut Vec::new());
                    }
                }
                "trait_item" => {
                    let Some(owner) = item_name(child, self.source) else {
                        continue;
                    };
                    let owner = owner.to_string();
                    for member in members(child, &["function_item"]) {
                        if let Some(method) = item_name(member, self.source) {
                            let name = format!("{owner}::{method}");
                            self.walk_callable(member, &name, &mut Vec::new());
                        }
                    }
                }
                "mod_item" => {
                    if let Some(body) = child.child_by_field_name("body") {
                        self.walk_module(body);
                    }
                }
                _ => {}
            }
        }
    }

    fn walk_callable(&mut self, callable: Node<'tree>, entity: &str, blocks: &mut Vec<usize>) {
        if let Some(body) = callable.child_by_field_name("body") {
            self.walk_node(body, entity, blocks);
        }
    }

    fn walk_node(&mut self, node: Node<'tree>, entity: &str, blocks: &mut Vec<usize>) {
        if node.kind() == "block" {
            self.walk_block(node, entity, blocks);
            return;
        }
        let mut cursor = node.walk();
        let children: Vec<Node<'tree>> = node.children(&mut cursor).collect();
        for child in children {
            if !is_item_kind(child.kind()) {
                self.walk_node(child, entity, blocks);
            }
        }
    }

    fn walk_block(&mut self, block: Node<'tree>, entity: &str, blocks: &mut Vec<usize>) {
        let mut cursor = block.walk();
        let children: Vec<Node<'tree>> = block.named_children(&mut cursor).collect();
        // Name every item first: an item is visible to its whole block, so an
        // impl may name a struct declared after it.
        let mut items: HashMap<String, Option<String>> = HashMap::new();
        let mut admitted: Vec<(usize, String, EntityKind)> = Vec::new();
        for child in &children {
            let kind = match child.kind() {
                "function_item" => EntityKind::Function,
                "struct_item" | "union_item" => EntityKind::Class,
                "enum_item" => EntityKind::EnumDef,
                "trait_item" => EntityKind::TraitDef,
                "type_item" => EntityKind::TypeAlias,
                "const_item" => EntityKind::Constant,
                "static_item" => EntityKind::StaticVar,
                "macro_definition" => EntityKind::Macro,
                "mod_item" => EntityKind::Module,
                _ => continue,
            };
            let Some(raw) = item_name(*child, self.source) else {
                continue;
            };
            let local = local_scope_name(entity, raw);
            if kind != EntityKind::Module && self.names.claim(&local) {
                items.entry(raw.to_string()).or_insert(Some(local.clone()));
                self.plan.kinds.insert(local.clone(), kind);
                admitted.push((child.id(), local, kind));
            } else {
                items.entry(raw.to_string()).or_insert(None);
                self.not_admitted(local, kind, *child, entity);
            }
        }
        if !items.is_empty() {
            self.plan.block_items.insert(block.id(), items);
            blocks.push(block.id());
        }
        for child in children {
            if let Some((_, local, kind)) = admitted.iter().find(|(id, _, _)| *id == child.id()) {
                let (local, kind) = (local.clone(), *kind);
                self.admit(child, local.clone(), kind, None, None, blocks);
                match child.kind() {
                    "function_item" => self.walk_callable(child, &local, blocks),
                    "trait_item" => self.walk_nested_trait(child, &local, blocks),
                    _ => {}
                }
            } else if child.kind() == "impl_item" {
                self.walk_nested_impl(child, entity, blocks);
            } else if !is_item_kind(child.kind()) {
                self.walk_node(child, entity, blocks);
            }
        }
        if self.plan.block_items.contains_key(&block.id()) {
            blocks.pop();
        }
    }

    fn walk_nested_trait(&mut self, item: Node<'tree>, owner: &str, blocks: &mut Vec<usize>) {
        for member in members(item, &["function_item", "function_signature_item"]) {
            let Some(method) = item_name(member, self.source) else {
                continue;
            };
            let name = format!("{owner}::{method}");
            if !self.names.claim(&name) {
                self.not_admitted(name, EntityKind::Method, member, owner);
                continue;
            }
            self.admit(
                member,
                name.clone(),
                EntityKind::Method,
                Some(owner.to_string()),
                None,
                blocks,
            );
            if member.kind() == "function_item" {
                self.walk_callable(member, &name, blocks);
            }
        }
    }

    fn walk_nested_impl(&mut self, item: Node<'tree>, entity: &str, blocks: &mut Vec<usize>) {
        let owner = self.impl_owner(item, blocks);
        let implements = item
            .child_by_field_name("trait")
            .and_then(|trait_node| self.local_or_written(trait_node, blocks, false));
        if let (Some(owner), Some(implements)) = (&owner, &implements) {
            self.plan
                .trait_impls
                .push((owner.clone(), implements.clone()));
        }
        for member in members(item, &["function_item"]) {
            let Some(method) = item_name(member, self.source) else {
                continue;
            };
            let Some(owner) = owner.as_deref() else {
                let written = item
                    .child_by_field_name("type")
                    .map(|ty| text(ty, self.source))
                    .unwrap_or("");
                self.not_admitted(
                    format!("{written}::{method}"),
                    EntityKind::Method,
                    member,
                    entity,
                );
                continue;
            };
            let name = format!("{owner}::{method}");
            if is_local_scope_name(&name) && !self.names.claim(&name) {
                self.not_admitted(name, EntityKind::Method, member, entity);
                continue;
            }
            self.admit(
                member,
                name.clone(),
                EntityKind::Method,
                Some(owner.to_string()),
                implements.clone(),
                blocks,
            );
            self.walk_callable(member, &name, blocks);
        }
    }

    /// The owner an impl in a function body gives its methods: the local
    /// type it names, or the type as written when no enclosing block declares
    /// it. `None` when it names a local type that was not admitted.
    fn impl_owner(&self, item: Node<'_>, blocks: &[usize]) -> Option<String> {
        let ty = item.child_by_field_name("type")?;
        self.local_or_written(ty, blocks, true)
    }

    /// A type or trait path as the member rule spells it from inside
    /// `blocks`: a local type's entity name (keeping any generic arguments
    /// when `keep_arguments`), or the text as written.
    fn local_or_written(
        &self,
        ty: Node<'_>,
        blocks: &[usize],
        keep_arguments: bool,
    ) -> Option<String> {
        let (base, arguments) = match ty.kind() {
            "type_identifier" => (ty, ""),
            "generic_type" => match ty.child_by_field_name("type") {
                Some(base) if base.kind() == "type_identifier" => (
                    base,
                    ty.child_by_field_name("type_arguments")
                        .map(|arguments| text(arguments, self.source))
                        .unwrap_or(""),
                ),
                _ => return Some(text(ty, self.source).to_string()),
            },
            _ => return Some(text(ty, self.source).to_string()),
        };
        match self.plan.lookup_type(blocks, text(base, self.source)) {
            Some(Some(local)) if keep_arguments => Some(format!("{local}{arguments}")),
            Some(Some(local)) => Some(local.to_string()),
            Some(None) => None,
            None => Some(text(ty, self.source).to_string()),
        }
    }

    fn admit(
        &mut self,
        node: Node<'tree>,
        name: String,
        kind: EntityKind,
        owner: Option<String>,
        implements: Option<String>,
        blocks: &[usize],
    ) {
        self.plan.admitted.insert(node.id());
        self.plan.nested.push(NestedItem {
            node,
            name,
            kind,
            owner,
            implements,
            inherited: blocks.to_vec(),
        });
    }

    fn not_admitted(&mut self, name: String, kind: EntityKind, node: Node<'_>, enclosing: &str) {
        self.plan.census.record(UnadmittedNestedDefinition {
            name,
            kind,
            start_line: node.start_position().row as u32,
            enclosing: Some(enclosing.to_string()),
            bases: Vec::new(),
        });
    }
}

/// The members of an impl or trait body of the given kinds.
pub(crate) fn members<'tree>(item: Node<'tree>, kinds: &[&str]) -> Vec<Node<'tree>> {
    let Some(body) = item.child_by_field_name("body") else {
        return Vec::new();
    };
    let mut cursor = body.walk();
    let found: Vec<Node<'tree>> = body
        .named_children(&mut cursor)
        .filter(|member| kinds.contains(&member.kind()))
        .collect();
    found
}

// ── Patterns ──────────────────────────────────────────────────────────────

/// Every identifier a pattern binds. A path in a pattern (`Foo::Bar`, the
/// `Some` of `Some(x)`, a range bound) names something rather than binding
/// it; a plain identifier is read as a binding even when it names a unit
/// variant or a constant, which only ever makes a reader more careful.
pub(crate) fn pattern_bindings<'tree>(pattern: Node<'tree>, out: &mut Vec<Node<'tree>>) {
    match pattern.kind() {
        "identifier" | "shorthand_field_identifier" => out.push(pattern),
        "scoped_identifier" | "range_pattern" | "macro_invocation" | "field_identifier" => {}
        "tuple_struct_pattern" | "struct_pattern" => {
            let mut cursor = pattern.walk();
            let children: Vec<Node<'tree>> = pattern.named_children(&mut cursor).collect();
            for child in children {
                if !is_field(pattern, "type", child) {
                    pattern_bindings(child, out);
                }
            }
        }
        "match_pattern" => {
            let mut cursor = pattern.walk();
            let children: Vec<Node<'tree>> = pattern.named_children(&mut cursor).collect();
            for child in children {
                if !is_field(pattern, "condition", child) {
                    pattern_bindings(child, out);
                }
            }
        }
        _ => {
            let mut cursor = pattern.walk();
            let children: Vec<Node<'tree>> = pattern.named_children(&mut cursor).collect();
            for child in children {
                pattern_bindings(child, out);
            }
        }
    }
}

/// Where a binding site's names are visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Binder {
    /// A `let` statement: its block, after the statement.
    Let,
    /// A function or closure parameter: the whole body.
    Parameter,
    /// A match arm, `if let`, `while let` or `for` pattern: the arm or body.
    Scoped,
}

/// Every name a callable subtree binds through a pattern, with the node that
/// binds it and where the binding is visible. `skip` prunes subtrees, such as
/// nested items that are entities of their own.
pub(crate) fn binding_sites<'tree>(
    node: Node<'tree>,
    skip: &dyn Fn(Node<'_>) -> bool,
    out: &mut Vec<(Node<'tree>, Node<'tree>, Binder)>,
) {
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if current.id() != node.id() && skip(current) {
            continue;
        }
        let mut bound = Vec::new();
        let binder = match current.kind() {
            "let_declaration" => {
                if let Some(pattern) = current.child_by_field_name("pattern") {
                    pattern_bindings(pattern, &mut bound);
                }
                Binder::Let
            }
            "parameter" => {
                if let Some(pattern) = current.child_by_field_name("pattern") {
                    pattern_bindings(pattern, &mut bound);
                }
                Binder::Parameter
            }
            "closure_parameters" => {
                let mut cursor = current.walk();
                for child in current.named_children(&mut cursor) {
                    if child.kind() != "parameter" {
                        pattern_bindings(child, &mut bound);
                    }
                }
                Binder::Parameter
            }
            "match_arm" | "let_condition" | "for_expression" => {
                if let Some(pattern) = current.child_by_field_name("pattern") {
                    pattern_bindings(pattern, &mut bound);
                }
                Binder::Scoped
            }
            _ => Binder::Scoped,
        };
        for name in bound {
            out.push((name, current, binder));
        }
        let mut cursor = current.walk();
        stack.extend(current.named_children(&mut cursor));
    }
}

/// Whether `child` is the node `parent` holds under `field`.
pub(crate) fn is_field(parent: Node<'_>, field: &str, child: Node<'_>) -> bool {
    parent
        .child_by_field_name(field)
        .is_some_and(|held| held.id() == child.id())
}

// ── Imports ───────────────────────────────────────────────────────────────

/// One name a `use` declaration binds, or a glob it opens.
#[derive(Debug, Clone)]
pub(crate) struct UseBinding {
    /// The name bound in scope. Empty for a glob.
    pub(crate) local: String,
    /// The name the path ends in.
    pub(crate) original: String,
    /// The path before the name, as written.
    pub(crate) module: String,
    /// Whether the path starts at `crate`, `self` or `super`, so it can only
    /// name an item of this crate.
    pub(crate) internal: bool,
    /// Whether this is `path::*`.
    pub(crate) glob: bool,
    /// The byte range of the whole `use` declaration.
    pub(crate) statement: (u32, u32),
}

/// Every binding a `use` declaration makes.
pub(crate) fn use_bindings(declaration: Node<'_>, source: &[u8]) -> Vec<UseBinding> {
    let mut out = Vec::new();
    let statement = (
        declaration.start_byte() as u32,
        declaration.end_byte() as u32,
    );
    if let Some(argument) = declaration.child_by_field_name("argument") {
        read_use_tree(argument, "", source, statement, &mut out);
    }
    out
}

fn join_path(prefix: &str, path: &str) -> String {
    match (prefix.is_empty(), path.is_empty()) {
        (true, _) => path.to_string(),
        (_, true) => prefix.to_string(),
        _ => format!("{prefix}::{path}"),
    }
}

fn is_internal_root(path: &str) -> bool {
    let root = path.split("::").next().unwrap_or("").trim();
    matches!(root, "crate" | "self" | "super")
}

fn read_use_tree(
    node: Node<'_>,
    prefix: &str,
    source: &[u8],
    statement: (u32, u32),
    out: &mut Vec<UseBinding>,
) {
    let push = |out: &mut Vec<UseBinding>, full: &str, local: &str| {
        let (module, original) = match full.rsplit_once("::") {
            Some((module, original)) => (module.to_string(), original.to_string()),
            None => (String::new(), full.to_string()),
        };
        out.push(UseBinding {
            local: local.to_string(),
            original,
            internal: is_internal_root(full),
            module,
            glob: false,
            statement,
        });
    };
    match node.kind() {
        "identifier" | "scoped_identifier" => {
            let full = join_path(prefix, text(node, source));
            let local = full.rsplit("::").next().unwrap_or("").to_string();
            push(out, &full, &local);
        }
        "self" => {
            // `use a::b::{self}` binds the module `b`.
            let local = prefix.rsplit("::").next().unwrap_or("").to_string();
            if !local.is_empty() {
                push(out, prefix, &local);
            }
        }
        "use_as_clause" => {
            let (Some(path), Some(alias)) = (
                node.child_by_field_name("path"),
                node.child_by_field_name("alias"),
            ) else {
                return;
            };
            let full = join_path(prefix, text(path, source));
            push(out, &full, text(alias, source));
        }
        "scoped_use_list" => {
            let path = node
                .child_by_field_name("path")
                .map(|path| text(path, source))
                .unwrap_or("");
            let prefix = join_path(prefix, path);
            if let Some(list) = node.child_by_field_name("list") {
                read_use_tree(list, &prefix, source, statement, out);
            }
        }
        "use_list" => {
            let mut cursor = node.walk();
            let children: Vec<Node<'_>> = node.named_children(&mut cursor).collect();
            for child in children {
                read_use_tree(child, prefix, source, statement, out);
            }
        }
        "use_wildcard" => {
            let spelled = text(node, source);
            let path = spelled.trim_end_matches('*').trim_end_matches("::");
            let module = join_path(prefix, path);
            out.push(UseBinding {
                local: String::new(),
                original: String::new(),
                internal: is_internal_root(&module),
                module,
                glob: true,
                statement,
            });
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::LanguageAdapter;

    fn with_plan(source: &str, check: impl FnOnce(&RustPlan<'_>)) {
        let tree = crate::languages::RustAdapter
            .parse(source.as_bytes())
            .unwrap();
        let plan = plan(&tree, source.as_bytes());
        check(&plan);
    }

    fn names(plan: &RustPlan<'_>) -> Vec<(String, EntityKind)> {
        plan.nested
            .iter()
            .map(|item| (item.name.clone(), item.kind))
            .collect()
    }

    #[test]
    fn items_in_a_test_body_are_named_under_the_function() {
        let source = "\
fn outer() {
    struct Fake;
    impl Sink for Fake {
        fn matched(&mut self) -> bool { helper() }
    }
    fn helper() -> bool { true }
    impl Store {
        fn extra(&self) {}
    }
}
";
        with_plan(source, |plan| {
            assert_eq!(
                names(plan),
                vec![
                    ("outer.<locals>.Fake".to_string(), EntityKind::Class),
                    (
                        "outer.<locals>.Fake::matched".to_string(),
                        EntityKind::Method
                    ),
                    ("outer.<locals>.helper".to_string(), EntityKind::Function),
                    ("Store::extra".to_string(), EntityKind::Method),
                ]
            );
            assert_eq!(
                plan.trait_impls,
                vec![("outer.<locals>.Fake".to_string(), "Sink".to_string())]
            );
            assert!(plan.census.not_admitted.is_empty());
        });
    }

    #[test]
    fn a_second_item_under_one_local_name_and_a_nested_module_are_recorded() {
        let source = "\
fn outer() {
    { fn helper() {} }
    { fn helper() {} }
    mod inner { pub fn f() {} }
}
";
        with_plan(source, |plan| {
            assert_eq!(
                names(plan),
                vec![("outer.<locals>.helper".to_string(), EntityKind::Function)]
            );
            let recorded: Vec<(&str, u32)> = plan
                .census
                .not_admitted
                .iter()
                .map(|entry| (entry.name.as_str(), entry.start_line))
                .collect();
            assert_eq!(
                recorded,
                vec![("outer.<locals>.helper", 2), ("outer.<locals>.inner", 3)]
            );
        });
    }

    #[test]
    fn a_method_body_nests_under_the_method_and_deeper_items_chain() {
        let source = "\
impl<T> Store<T> {
    fn load(&self) {
        fn parse() {
            struct Row;
        }
    }
}
";
        with_plan(source, |plan| {
            assert_eq!(
                names(plan),
                vec![
                    (
                        "Store<T>::load.<locals>.parse".to_string(),
                        EntityKind::Function
                    ),
                    (
                        "Store<T>::load.<locals>.parse.<locals>.Row".to_string(),
                        EntityKind::Class
                    ),
                ]
            );
        });
    }

    #[test]
    fn use_trees_list_every_binding_with_its_root() {
        let source = "use crate::store::{Store, Cache as C, self};\nuse std::io::*;\nuse serde::Deserialize;\n";
        let tree = crate::languages::RustAdapter
            .parse(source.as_bytes())
            .unwrap();
        let root = tree.root_node();
        let mut cursor = root.walk();
        let bindings: Vec<(String, String, bool, bool)> = root
            .named_children(&mut cursor)
            .flat_map(|declaration| use_bindings(declaration, source.as_bytes()))
            .map(|binding| {
                (
                    binding.local,
                    binding.module,
                    binding.internal,
                    binding.glob,
                )
            })
            .collect();
        assert_eq!(
            bindings,
            vec![
                ("Store".into(), "crate::store".into(), true, false),
                ("C".into(), "crate::store".into(), true, false),
                ("store".into(), "crate".into(), true, false),
                (String::new(), "std::io".into(), false, true),
                ("Deserialize".into(), "serde".into(), false, false),
            ]
        );
    }
}
