// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Rust's facts for the value-escape record (see [`crate::value_escape`]).
//!
//! One walk over the tree, with Rust's own scoping: a `let` binds from the end
//! of its statement to the end of its block, a parameter binds for the whole
//! body, a closure parameter, a match arm, an `if let` or a `for` pattern binds
//! for what it guards, and an item a block declares is visible to the whole
//! block and to every item nested in it. A read of a local is never recorded,
//! since it cannot be a declaration of another file. A read of an item is.
//!
//! Recorded:
//!
//! - every read of a function, constant or other item whose value flows on (an
//!   argument, a `let`, a return or a closure's body, an array, tuple or
//!   struct literal, an assignment): a plain name with the binding it reads,
//!   and a path (`Store::load`, `Self::load`, `crate::util::parse`) as a member
//!   of what the path names, never a callee;
//! - `use a::helper as h` as an import alias, and a `const` or `static`
//!   initialized with an item as an alias under the item's name;
//! - a name a macro's token tree holds as a value, since the macro can keep it;
//! - calls the source spells where no call relation can carry them, as
//!   invocations: a call-shaped run in a `macro_rules!` body, which runs where
//!   the macro expands, a call in a macro invoked outside any callable
//!   (`thread_local!`, `lazy_static!`), and a function a derive helper
//!   attribute names (`#[serde(default = "default_port")]`,
//!   `#[serde(with = "duration")]`);
//! - protocol members: every method of an `impl Trait for Type` block. Code
//!   that holds the value through the trait (a trait object, a generic, the
//!   standard library's operators, `for`, `?`, `Drop`, `{}` formatting) calls
//!   it without spelling a call to this implementation.
//!
//! Rust has no runtime lookup of a declaration by a computed name, so no
//! reflective site is recorded. A module's private items are reachable from
//! its child modules, which may be other files, so the record's module scope
//! is open.
//!
//! A name in a `macro_rules!` body resolves where the macro expands, which can
//! be any file, so it is recorded as an import binding no statement settles:
//! the census reads it as possibly any declaration of that name.

use std::collections::HashMap;

use kin_model::{
    FileEscapeRecord, ModuleScope, OccurrenceBinding, OccurrenceContext, ProtocolForm,
    ProtocolMember, ProtocolSite, ReceiverKind, ValueOccurrence, RECEIVER_TEXT_MAX,
};
use tree_sitter::{Node, Tree};

use super::rust_nested::{
    is_field, is_item_kind, is_module_body, item_name, parent_module_body, pattern_bindings, plan,
    use_bindings, RustPlan, UseBinding,
};
use crate::value_escape::{
    collapsed_text, is_name, line_of, text_of, unparsed_lines, FileEscapeCensus, ProtocolStamp,
};

/// Attribute keys whose value is a name or text, never a function a derive
/// calls.
const NON_CALL_ATTRIBUTE_KEYS: &[&str] = &[
    "doc",
    "rename",
    "rename_all",
    "alias",
    "tag",
    "content",
    "crate",
    "bound",
    "expecting",
    "note",
    "message",
    "since",
    "reason",
    "feature",
    "name",
    "about",
    "version",
    "author",
    "long",
    "short",
    "env",
    "value_name",
    "help",
    "target_os",
    "target_arch",
    "target_family",
    "target_env",
    "path",
    "label",
];

/// The prelude's enum variants. A read of one that no declaration, `use` or
/// glob in scope could rebind is `Option` or `Result` itself, never a
/// declaration of the repository.
const PRELUDE_VARIANTS: &[&str] = &["None", "Some", "Ok", "Err"];

/// Words the grammar leaves as identifiers inside a macro's token tree that
/// never name a declaration.
const KEYWORDS: &[&str] = &[
    "as",
    "async",
    "await",
    "break",
    "const",
    "continue",
    "dyn",
    "else",
    "enum",
    "extern",
    "false",
    "fn",
    "for",
    "if",
    "impl",
    "in",
    "let",
    "loop",
    "match",
    "mod",
    "move",
    "mut",
    "pub",
    "ref",
    "return",
    "static",
    "struct",
    "trait",
    "true",
    "type",
    "unsafe",
    "use",
    "where",
    "while",
    "yield",
    "union",
    "macro_rules",
];

/// Crates no repository declaration lives in.
const STANDARD_CRATES: &[&str] = &["std", "core", "alloc"];

/// Take the census of one Rust file.
pub(crate) fn census(tree: &Tree, source: &[u8]) -> FileEscapeCensus {
    let root = tree.root_node();
    let mut walker = Walker {
        source,
        plan: plan(tree, source),
        scopes: HashMap::new(),
        lets: HashMap::new(),
        record: FileEscapeRecord::new(ModuleScope::Open),
        protocol_members: Vec::new(),
    };
    walker.record.unparsed = unparsed_lines(tree, source);
    walker.index(root);
    walker.visit(root);
    FileEscapeCensus {
        record: walker.record,
        protocol_members: walker.protocol_members,
        non_enumerable: Vec::new(),
    }
}

/// What one module or block scope binds by declaration and by `use`.
#[derive(Default)]
struct ScopeTable {
    /// Value items (functions, constants, statics, tuple and unit structs)
    /// the scope declares, with each declaration's 0-based line.
    values: HashMap<String, Vec<u32>>,
    uses: Vec<UseBinding>,
}

/// How a read resolves.
struct Resolved {
    binding: OccurrenceBinding,
    target_line: Option<u32>,
    module: Option<String>,
    import: Option<(u32, u32)>,
    imported: Option<String>,
}

impl Resolved {
    fn of(binding: OccurrenceBinding) -> Self {
        Self {
            binding,
            target_line: None,
            module: None,
            import: None,
            imported: None,
        }
    }

    fn import(binding: &UseBinding, name: &str) -> Self {
        Self {
            binding: OccurrenceBinding::Import,
            target_line: None,
            module: Some(binding.module.clone()),
            import: Some(binding.statement),
            imported: (!binding.glob && binding.original != name).then(|| binding.original.clone()),
        }
    }
}

/// What a scope's globs bind a name to.
enum Glob {
    Binds(Resolved),
    /// No glob of the scope binds it, so the lookup goes on outward.
    Misses,
    /// Some glob may bind it; which item, this file cannot say.
    Unknown,
}

/// Where a read goes.
enum Flow {
    Value(OccurrenceContext, Vec<String>),
    /// Called through a wrapper that spells no call name: `(helper)()`.
    Invoke,
}

struct Walker<'s, 'tree> {
    source: &'s [u8],
    plan: RustPlan<'tree>,
    /// Module bodies and blocks, by node id.
    scopes: HashMap<usize, ScopeTable>,
    /// Each block's `let` bindings: by name, the bytes they bind from.
    lets: HashMap<usize, HashMap<String, Vec<usize>>>,
    record: FileEscapeRecord,
    protocol_members: Vec<ProtocolStamp>,
}

impl<'s, 'tree> Walker<'s, 'tree> {
    fn text(&self, node: Node<'_>) -> &'s str {
        text_of(node, self.source)
    }

    // ── Scopes ────────────────────────────────────────────────────────────

    fn index(&mut self, root: Node<'tree>) {
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            if is_module_body(node) || node.kind() == "block" {
                let mut table = ScopeTable::default();
                let mut cursor = node.walk();
                let children: Vec<Node<'tree>> = node.named_children(&mut cursor).collect();
                for child in children {
                    match child.kind() {
                        "use_declaration" => table.uses.extend(use_bindings(child, self.source)),
                        "function_item" | "const_item" | "static_item" => {
                            if let Some(name) = item_name(child, self.source) {
                                table
                                    .values
                                    .entry(name.to_string())
                                    .or_default()
                                    .push(line_of(child));
                            }
                        }
                        "struct_item" => {
                            // A tuple or unit struct's name is its constructor.
                            let braced = child
                                .child_by_field_name("body")
                                .is_some_and(|body| body.kind() == "field_declaration_list");
                            if let (false, Some(name)) = (braced, item_name(child, self.source)) {
                                table
                                    .values
                                    .entry(name.to_string())
                                    .or_default()
                                    .push(line_of(child));
                            }
                        }
                        "foreign_mod_item" => {
                            let mut inner = child.walk();
                            if let Some(body) = child.child_by_field_name("body") {
                                for item in body.named_children(&mut inner) {
                                    if let Some(name) = item_name(item, self.source) {
                                        table
                                            .values
                                            .entry(name.to_string())
                                            .or_default()
                                            .push(line_of(item));
                                    }
                                }
                            }
                        }
                        "let_declaration" => {
                            if let Some(pattern) = child.child_by_field_name("pattern") {
                                let mut bound = Vec::new();
                                pattern_bindings(pattern, &mut bound);
                                let source = self.source;
                                let entry = self.lets.entry(node.id()).or_default();
                                for name in bound {
                                    entry
                                        .entry(text_of(name, source).to_string())
                                        .or_default()
                                        .push(child.end_byte());
                                }
                            }
                        }
                        _ => {}
                    }
                }
                self.scopes.insert(node.id(), table);
            }
            let mut cursor = node.walk();
            stack.extend(node.named_children(&mut cursor));
        }
    }

    /// How a read of `name` at `at` binds, or `None` for a local.
    fn resolve(&self, name: &str, at: Node<'_>) -> Option<Resolved> {
        // Locals stop at the first item boundary; items and imports of the
        // blocks around an item are still visible inside it.
        let mut locals_visible = true;
        let mut child = at;
        let mut current = at.parent();
        while let Some(node) = current {
            match node.kind() {
                "block" => {
                    if locals_visible
                        && self
                            .lets
                            .get(&node.id())
                            .and_then(|lets| lets.get(name))
                            .is_some_and(|froms| froms.iter().any(|from| *from <= at.start_byte()))
                    {
                        return None;
                    }
                    if let Some(found) = self.plan.lookup(&[node.id()], name) {
                        let mut resolved = Resolved::of(OccurrenceBinding::Local);
                        resolved.target_line = found.and_then(|entity| {
                            self.plan
                                .nested
                                .iter()
                                .find(|item| item.name == entity)
                                .map(|item| line_of(item.node))
                        });
                        return Some(resolved);
                    }
                    if let Some(found) = self.imported(node, name) {
                        return Some(found);
                    }
                }
                "closure_expression" if locals_visible => {
                    if let Some(parameters) = node.child_by_field_name("parameters") {
                        if self.binds(parameters, name) {
                            return None;
                        }
                    }
                }
                "match_arm" if locals_visible => {
                    let in_pattern = node.child_by_field_name("pattern").is_some_and(|pattern| {
                        pattern
                            .child_by_field_name("condition")
                            .is_none_or(|guard| {
                                !(guard.start_byte() <= at.start_byte()
                                    && at.end_byte() <= guard.end_byte())
                            })
                            && pattern.start_byte() <= at.start_byte()
                            && at.end_byte() <= pattern.end_byte()
                    });
                    if !in_pattern {
                        if let Some(pattern) = node.child_by_field_name("pattern") {
                            if self.binds(pattern, name) {
                                return None;
                            }
                        }
                    }
                }
                "if_expression" | "while_expression" if locals_visible => {
                    let guarded = node
                        .child_by_field_name("consequence")
                        .or_else(|| node.child_by_field_name("body"))
                        .is_some_and(|body| body.id() == child.id());
                    if let Some(condition) = node.child_by_field_name("condition") {
                        let mut conditions = Vec::new();
                        let_conditions(condition, &mut conditions);
                        for binding in conditions {
                            let after = binding.end_byte() <= at.start_byte()
                                && at.end_byte() <= condition.end_byte();
                            if (guarded || after)
                                && binding
                                    .child_by_field_name("pattern")
                                    .is_some_and(|pattern| self.binds(pattern, name))
                            {
                                return None;
                            }
                        }
                    }
                }
                "for_expression" if locals_visible => {
                    let in_body = node
                        .child_by_field_name("body")
                        .is_some_and(|body| body.id() == child.id());
                    if in_body
                        && node
                            .child_by_field_name("pattern")
                            .is_some_and(|pattern| self.binds(pattern, name))
                    {
                        return None;
                    }
                }
                "function_item" => {
                    if locals_visible {
                        if let Some(parameters) = node.child_by_field_name("parameters") {
                            if self.binds(parameters, name) {
                                return None;
                            }
                        }
                    }
                    locals_visible = false;
                }
                "const_item" | "static_item" => locals_visible = false,
                _ if is_module_body(node) => {
                    let table = self.scopes.get(&node.id());
                    if let Some(lines) = table.and_then(|table| table.values.get(name)) {
                        let mut resolved = Resolved::of(OccurrenceBinding::Module);
                        if let [line] = lines.as_slice() {
                            resolved.target_line = Some(*line);
                        }
                        return Some(resolved);
                    }
                    return Some(
                        self.imported(node, name)
                            .unwrap_or_else(|| Resolved::of(OccurrenceBinding::Free)),
                    );
                }
                _ => {}
            }
            child = node;
            current = node.parent();
        }
        Some(Resolved::of(OccurrenceBinding::Free))
    }

    /// A `use` of the scope that binds `name`, or a glob that may.
    fn imported(&self, scope: Node<'_>, name: &str) -> Option<Resolved> {
        let table = self.scopes.get(&scope.id())?;
        if let Some(binding) = table.uses.iter().find(|binding| binding.local == name) {
            return Some(Resolved::import(binding, name));
        }
        let first_glob = table.uses.iter().find(|binding| binding.glob)?;
        match self.through_globs(scope, name, 0) {
            Glob::Binds(resolved) => Some(resolved),
            Glob::Misses => None,
            Glob::Unknown => Some(Resolved::import(first_glob, name)),
        }
    }

    /// What the globs of a scope bind `name` to. A `use super::*` in an
    /// inline module reads the parent module in this same file, which says
    /// exactly what it declares and imports; any other glob could bind
    /// anything.
    fn through_globs(&self, scope: Node<'_>, name: &str, depth: usize) -> Glob {
        let Some(table) = self.scopes.get(&scope.id()) else {
            return Glob::Unknown;
        };
        let mut found: Option<Resolved> = None;
        for glob in table.uses.iter().filter(|binding| binding.glob) {
            let parent = (glob.module == "super" && depth < 8)
                .then(|| parent_module_body(scope))
                .flatten();
            let Some(parent) = parent else {
                return Glob::Unknown;
            };
            let Some(parent_table) = self.scopes.get(&parent.id()) else {
                return Glob::Unknown;
            };
            let outcome = if let Some(lines) = parent_table.values.get(name) {
                let mut resolved = Resolved::of(OccurrenceBinding::Module);
                if let [line] = lines.as_slice() {
                    resolved.target_line = Some(*line);
                }
                Glob::Binds(resolved)
            } else if let Some(binding) = parent_table
                .uses
                .iter()
                .find(|binding| binding.local == name)
            {
                Glob::Binds(Resolved::import(binding, name))
            } else if parent_table.uses.iter().any(|binding| binding.glob) {
                self.through_globs(parent, name, depth + 1)
            } else {
                Glob::Misses
            };
            match outcome {
                Glob::Unknown => return Glob::Unknown,
                Glob::Binds(resolved) => {
                    if found.is_some() {
                        return Glob::Unknown;
                    }
                    found = Some(resolved);
                }
                Glob::Misses => {}
            }
        }
        found.map_or(Glob::Misses, Glob::Binds)
    }

    /// Whether a path whose first segment is `root`, written at `at`, can
    /// only name an item of the standard library: it starts at `std`,
    /// `core` or `alloc`, or at a name a non-glob `use` from one binds.
    fn standard_path(&self, root: &str, at: Node<'_>) -> bool {
        if STANDARD_CRATES.contains(&root) {
            return true;
        }
        self.resolve(root, at).is_some_and(|resolved| {
            resolved.binding == OccurrenceBinding::Import
                && resolved.import.is_some()
                && resolved.module.as_deref().is_some_and(|module| {
                    STANDARD_CRATES.contains(&module.split("::").next().unwrap_or(""))
                })
                && !self.glob_bound(root, at)
        })
    }

    /// Whether the nearest scope that binds `name` does so only through a
    /// glob.
    fn glob_bound(&self, name: &str, at: Node<'_>) -> bool {
        let mut current = at.parent();
        while let Some(node) = current {
            if let Some(table) = self.scopes.get(&node.id()) {
                if table.uses.iter().any(|binding| binding.local == name) {
                    return false;
                }
                if table.uses.iter().any(|binding| binding.glob) {
                    return true;
                }
            }
            current = node.parent();
        }
        false
    }

    /// Whether a pattern or parameter list binds `name`.
    fn binds(&self, node: Node<'_>, name: &str) -> bool {
        let mut bound = Vec::new();
        if matches!(node.kind(), "parameters" | "closure_parameters") {
            let mut cursor = node.walk();
            for parameter in node.named_children(&mut cursor) {
                match parameter.kind() {
                    "parameter" => {
                        if let Some(pattern) = parameter.child_by_field_name("pattern") {
                            pattern_bindings(pattern, &mut bound);
                        }
                    }
                    "self_parameter" | "attribute_item" | "variadic_parameter" => {}
                    _ => pattern_bindings(parameter, &mut bound),
                }
            }
        } else {
            pattern_bindings(node, &mut bound);
        }
        bound.iter().any(|binding| self.text(*binding) == name)
    }

    // ── The walk ──────────────────────────────────────────────────────────

    fn visit_children(&mut self, node: Node<'tree>) {
        let mut cursor = node.walk();
        let children: Vec<Node<'tree>> = node.named_children(&mut cursor).collect();
        for child in children {
            self.visit(child);
        }
    }

    fn visit_field(&mut self, node: Node<'tree>, field: &str) {
        if let Some(child) = node.child_by_field_name(field) {
            self.visit(child);
        }
    }

    fn visit(&mut self, node: Node<'tree>) {
        let kind = node.kind();
        if kind.ends_with("_type") && !matches!(kind, "bracketed_type")
            || matches!(
                kind,
                "type_arguments"
                    | "type_parameters"
                    | "where_clause"
                    | "trait_bounds"
                    | "visibility_modifier"
                    | "label"
                    | "lifetime"
                    | "string_literal"
                    | "raw_string_literal"
                    | "char_literal"
                    | "line_comment"
                    | "block_comment"
                    | "extern_crate_declaration"
                    | "inner_attribute_item"
                    | "parameters"
                    | "closure_parameters"
                    | "function_signature_item"
                    | "type_identifier"
                    | "scoped_type_identifier"
                    | "generic_type_with_turbofish"
                    | "type_item"
                    | "associated_type"
                    | "foreign_mod_item"
            )
        {
            return;
        }
        match kind {
            "identifier" => self.read_name(node, node),
            "scoped_identifier" => self.read_path(node, node),
            "generic_function" => {
                if let Some(function) = node.child_by_field_name("function") {
                    match function.kind() {
                        "identifier" => self.read_name(function, node),
                        "scoped_identifier" => self.read_path(function, node),
                        _ => self.visit(function),
                    }
                }
            }
            "use_declaration" => self.mark_use(node),
            "attribute_item" => self.mark_attribute(node),
            "call_expression" => {
                if let Some(function) = node.child_by_field_name("function") {
                    match function.kind() {
                        "identifier" | "scoped_identifier" => {}
                        "generic_function" => {
                            // `a.b().collect::<T>()`: the receiver chain is read.
                            if let Some(inner) = function
                                .child_by_field_name("function")
                                .filter(|inner| inner.kind() == "field_expression")
                            {
                                self.visit_field(inner, "value");
                            }
                        }
                        "field_expression" => self.visit_field(function, "value"),
                        _ => self.visit(function),
                    }
                }
                self.visit_field(node, "arguments");
            }
            "field_expression" => self.visit_field(node, "value"),
            "let_declaration" => {
                self.visit_field(node, "value");
                self.visit_field(node, "alternative");
            }
            "closure_expression" => self.visit_field(node, "body"),
            "match_arm" => {
                if let Some(guard) = node
                    .child_by_field_name("pattern")
                    .and_then(|pattern| pattern.child_by_field_name("condition"))
                {
                    self.visit(guard);
                }
                self.visit_field(node, "value");
            }
            "let_condition" => self.visit_field(node, "value"),
            "for_expression" => {
                self.visit_field(node, "value");
                self.visit_field(node, "body");
            }
            "field_initializer" => self.visit_field(node, "value"),
            "macro_invocation" => self.mark_macro(node),
            "macro_definition" => {
                let mut cursor = node.walk();
                let rules: Vec<Node<'tree>> = node.named_children(&mut cursor).collect();
                for rule in rules {
                    if let Some(right) = rule.child_by_field_name("right") {
                        self.mark_token_tree(right, node, TokenPlace::MacroBody);
                    }
                }
            }
            "function_item" | "mod_item" | "trait_item" => self.visit_field(node, "body"),
            "struct_item" | "union_item" | "enum_item" => {
                // Only the attributes on its fields and variants run code.
                let mut attributes = Vec::new();
                let mut stack = vec![node];
                while let Some(current) = stack.pop() {
                    if current.kind() == "attribute_item" {
                        attributes.push(current);
                        continue;
                    }
                    let mut cursor = current.walk();
                    stack.extend(current.named_children(&mut cursor));
                }
                attributes.sort_by_key(|attribute| attribute.start_byte());
                for attribute in attributes {
                    self.mark_attribute(attribute);
                }
            }
            "impl_item" => {
                if let Some(trait_node) = node.child_by_field_name("trait") {
                    self.mark_trait_impl(node, trait_node);
                }
                self.visit_field(node, "body");
            }
            "const_item" | "static_item" => self.visit_field(node, "value"),
            _ => {
                if is_item_kind(kind) {
                    return;
                }
                self.visit_children(node);
            }
        }
    }

    // ── Reads ─────────────────────────────────────────────────────────────

    /// A plain name read as a value. `outer` is the node its value leaves
    /// through: the name itself, or the turbofish around it.
    fn read_name(&mut self, name_node: Node<'_>, outer: Node<'_>) {
        let name = self.text(name_node);
        if !is_name(name) {
            return;
        }
        let Some(flow) = self.context_of(outer) else {
            return;
        };
        let Some(resolved) = self.resolve(name, name_node) else {
            return;
        };
        if resolved.binding == OccurrenceBinding::Free && PRELUDE_VARIANTS.contains(&name) {
            return;
        }
        if resolved.binding == OccurrenceBinding::Import && self.standard_path(name, name_node) {
            return;
        }
        let (context, aliases) = match flow {
            Flow::Value(context, aliases) => (context, aliases),
            Flow::Invoke => {
                self.push_protocol(name_node, ProtocolForm::Invoke, Some(name.to_string()));
                return;
            }
        };
        self.push_value(
            name.to_string(),
            ValueOccurrence {
                line: line_of(name_node),
                context,
                binding: resolved.binding,
                receiver: None,
                receiver_kind: None,
                aliases,
                module: resolved.module,
                import: resolved.import,
                imported: resolved.imported,
                target_line: resolved.target_line,
                form: None,
            },
        );
    }

    /// A path read as a value: the item it ends in, as a member of what the
    /// path before it names.
    fn read_path(&mut self, path: Node<'_>, outer: Node<'_>) {
        let (Some(prefix), Some(leaf)) = (
            path.child_by_field_name("path"),
            path.child_by_field_name("name"),
        ) else {
            return;
        };
        let name = self.text(leaf);
        if !is_name(name) {
            return;
        }
        let Some(flow) = self.context_of(outer) else {
            return;
        };
        let receiver = collapsed_text(prefix, self.source, RECEIVER_TEXT_MAX);
        let root = receiver.split("::").next().unwrap_or("").trim().to_string();
        if self.standard_path(&root, path) {
            return;
        }
        match flow {
            Flow::Invoke => self.push_protocol(leaf, ProtocolForm::Invoke, Some(name.to_string())),
            Flow::Value(context, aliases) => self.push_value(
                name.to_string(),
                ValueOccurrence {
                    line: line_of(leaf),
                    context,
                    binding: OccurrenceBinding::Member,
                    receiver_kind: Some(path_receiver_kind(&receiver)),
                    receiver: Some(receiver),
                    aliases,
                    module: None,
                    import: None,
                    imported: None,
                    target_line: None,
                    form: None,
                },
            ),
        }
    }

    /// Where the value `node` evaluates to goes, walking up through the
    /// syntax that passes a value through unchanged, or `None` when it goes
    /// nowhere a later call could reach it through.
    fn context_of(&self, node: Node<'_>) -> Option<Flow> {
        let mut child = node;
        let mut wrapped = false;
        loop {
            let parent = child.parent()?;
            match parent.kind() {
                "parenthesized_expression"
                | "reference_expression"
                | "try_expression"
                | "await_expression"
                | "else_clause"
                | "match_block"
                | "unsafe_block"
                | "async_block"
                | "loop_expression"
                | "const_block"
                | "try_block" => {}
                "generic_function" => {
                    child = parent;
                    continue;
                }
                "type_cast_expression" => {
                    if !is_field(parent, "value", child) {
                        return None;
                    }
                }
                "arguments" => return Some(Flow::Value(OccurrenceContext::Argument, Vec::new())),
                "array_expression" | "tuple_expression" | "shorthand_field_initializer" => {
                    return Some(Flow::Value(OccurrenceContext::Collection, Vec::new()))
                }
                "field_initializer" => {
                    return is_field(parent, "value", child)
                        .then(|| Flow::Value(OccurrenceContext::Collection, Vec::new()));
                }
                "let_declaration" => {
                    return is_field(parent, "value", child)
                        .then(|| Flow::Value(OccurrenceContext::Assignment, Vec::new()));
                }
                "assignment_expression" => {
                    return is_field(parent, "right", child)
                        .then(|| Flow::Value(OccurrenceContext::Assignment, Vec::new()));
                }
                "return_expression" | "yield_expression" => {
                    return Some(Flow::Value(OccurrenceContext::Return, Vec::new()))
                }
                "closure_expression" => {
                    return is_field(parent, "body", child)
                        .then(|| Flow::Value(OccurrenceContext::Return, Vec::new()));
                }
                "block" => {
                    if !is_tail(parent, child) {
                        return None;
                    }
                    let owner = parent.parent()?;
                    if matches!(owner.kind(), "function_item" | "closure_expression")
                        && is_field(owner, "body", parent)
                    {
                        return Some(Flow::Value(OccurrenceContext::Return, Vec::new()));
                    }
                }
                "if_expression" => {
                    if is_field(parent, "condition", child) {
                        return None;
                    }
                }
                "match_arm" => {
                    if !is_field(parent, "value", child) {
                        return None;
                    }
                }
                "match_expression" => {
                    if is_field(parent, "value", child) {
                        return None;
                    }
                }
                "call_expression" => {
                    return (wrapped && is_field(parent, "function", child))
                        .then_some(Flow::Invoke);
                }
                "const_item" | "static_item" => {
                    let aliases = item_name(parent, self.source)
                        .map(|name| vec![name.to_string()])
                        .unwrap_or_default();
                    return Some(Flow::Value(OccurrenceContext::Alias, aliases));
                }
                "field_expression"
                | "index_expression"
                | "expression_statement"
                | "binary_expression"
                | "unary_expression"
                | "range_expression"
                | "let_condition"
                | "while_expression"
                | "for_expression"
                | "compound_assignment_expr"
                | "macro_invocation"
                | "token_tree" => return None,
                _ => return Some(Flow::Value(OccurrenceContext::Other, Vec::new())),
            }
            child = parent;
            wrapped = true;
        }
    }

    // ── Imports, macros and attributes ────────────────────────────────────

    /// `use a::helper as h` binds `helper`'s value under `h`.
    fn mark_use(&mut self, declaration: Node<'_>) {
        for binding in use_bindings(declaration, self.source) {
            if binding.glob || binding.local == binding.original || binding.local == "_" {
                continue;
            }
            self.push_value(
                binding.original.clone(),
                ValueOccurrence {
                    line: line_of(declaration),
                    context: OccurrenceContext::Import,
                    binding: OccurrenceBinding::Import,
                    receiver: None,
                    receiver_kind: None,
                    aliases: vec![binding.local.clone()],
                    module: Some(binding.module.clone()),
                    import: Some(binding.statement),
                    imported: None,
                    target_line: None,
                    form: None,
                },
            );
        }
    }

    fn mark_macro(&mut self, invocation: Node<'tree>) {
        let place = if inside_extracted_body(invocation) {
            TokenPlace::CallableBody
        } else {
            TokenPlace::Unextracted
        };
        let mut cursor = invocation.walk();
        let trees: Vec<Node<'tree>> = invocation
            .children(&mut cursor)
            .filter(|child| child.kind() == "token_tree")
            .collect();
        for tree in trees {
            self.mark_token_tree(tree, invocation, place);
        }
    }

    /// Calls and values a token tree spells. `at` is the macro node, whose
    /// position names resolve from.
    fn mark_token_tree(&mut self, tree: Node<'tree>, at: Node<'tree>, place: TokenPlace) {
        let mut cursor = tree.walk();
        let tokens: Vec<Node<'tree>> = tree
            .children(&mut cursor)
            .filter(|token| !matches!(token.kind(), "line_comment" | "block_comment"))
            .collect();
        let kind_at =
            |at: Option<usize>| at.and_then(|at| tokens.get(at)).map(|token| token.kind());
        for (index, token) in tokens.iter().enumerate() {
            if token.kind() == "token_tree" {
                self.mark_token_tree(*token, at, place);
                continue;
            }
            if token.kind() != "identifier" {
                continue;
            }
            let previous = kind_at(index.checked_sub(1));
            let next = kind_at(Some(index + 1));
            let called = tokens.get(index + 1).is_some_and(|next| {
                next.kind() == "token_tree" && next.child(0).map(|open| open.kind()) == Some("(")
            });
            let turbofish = next == Some("::") && kind_at(Some(index + 2)) == Some("<");
            // A macro's name, a path's leading segment, or a receiver.
            if matches!(next, Some("!" | ".")) || (next == Some("::") && !turbofish) {
                continue;
            }
            // A generic type (`Vec<Store>`), a return type, or an operand of
            // a comparison or a boolean operator: nothing a later call reaches.
            let inert = ["<", ">", "<=", ">=", "==", "!=", "&&", "||"];
            if next.is_some_and(|next| inert.contains(&next))
                || previous.is_some_and(|previous| inert.contains(&previous) || previous == "->")
                || KEYWORDS.contains(&self.text(*token))
            {
                continue;
            }
            if matches!(next, Some(":") | Some("=")) {
                continue;
            }
            let name = self.text(*token).to_string();
            if matches!(
                previous,
                Some(
                    "$" | "fn"
                        | "struct"
                        | "enum"
                        | "trait"
                        | "type"
                        | "mod"
                        | "const"
                        | "static"
                        | "let"
                        | "mut"
                        | "ref"
                        | "impl"
                        | "use"
                        | "as"
                        | "union"
                        | "macro_rules!"
                )
            ) {
                continue;
            }
            if previous == Some(".") {
                // A method called in a macro body runs wherever it expands.
                if called && place.hides_calls() {
                    self.push_protocol(*token, ProtocolForm::Invoke, Some(name));
                }
                continue;
            }
            if called {
                if place.hides_calls() {
                    self.push_protocol(*token, ProtocolForm::Invoke, Some(name));
                }
                continue;
            }
            if previous == Some("::") {
                let prefix = path_prefix(&tokens, index);
                let receiver = prefix.map(|start| {
                    let text =
                        &self.source[tokens[start].start_byte()..tokens[index - 1].start_byte()];
                    String::from_utf8_lossy(text).trim().to_string()
                });
                let root = prefix.map(|start| self.text(tokens[start]).to_string());
                if root.is_some_and(|root| self.standard_path(&root, at)) {
                    continue;
                }
                let receiver_kind = receiver.as_deref().map(path_receiver_kind);
                self.push_value(
                    name,
                    ValueOccurrence {
                        line: line_of(*token),
                        context: OccurrenceContext::Argument,
                        binding: OccurrenceBinding::Member,
                        receiver,
                        receiver_kind,
                        aliases: Vec::new(),
                        module: None,
                        import: None,
                        imported: None,
                        target_line: None,
                        form: None,
                    },
                );
                continue;
            }
            if !is_name(&name) {
                continue;
            }
            let resolved = if place == TokenPlace::MacroBody {
                // Resolved where the macro expands, which may be any file.
                Some(Resolved::of(OccurrenceBinding::Import))
            } else {
                self.resolve(&name, at)
            };
            let Some(resolved) = resolved else {
                continue;
            };
            if resolved.binding == OccurrenceBinding::Free
                && PRELUDE_VARIANTS.contains(&name.as_str())
            {
                continue;
            }
            if resolved.binding == OccurrenceBinding::Import && self.standard_path(&name, at) {
                continue;
            }
            self.push_value(
                name,
                ValueOccurrence {
                    line: line_of(*token),
                    context: OccurrenceContext::Argument,
                    binding: resolved.binding,
                    receiver: None,
                    receiver_kind: None,
                    aliases: Vec::new(),
                    module: resolved.module,
                    import: resolved.import,
                    imported: resolved.imported,
                    target_line: resolved.target_line,
                    form: None,
                },
            );
        }
    }

    /// A function a derive helper attribute names by `key = "path"` or
    /// `key = path` is called by the code the derive generates.
    fn mark_attribute(&mut self, attribute: Node<'_>) {
        let mut stack = vec![attribute];
        while let Some(node) = stack.pop() {
            if node.kind() == "token_tree" {
                let mut cursor = node.walk();
                let tokens: Vec<Node<'_>> = node.children(&mut cursor).collect();
                for (index, token) in tokens.iter().enumerate() {
                    if token.kind() == "token_tree" {
                        stack.push(*token);
                        continue;
                    }
                    if tokens.get(index + 1).map(|next| next.kind()) != Some("=") {
                        continue;
                    }
                    let key = self.text(*token);
                    if NON_CALL_ATTRIBUTE_KEYS.contains(&key) {
                        continue;
                    }
                    let Some(value) = tokens.get(index + 2) else {
                        continue;
                    };
                    let (path, at) = match value.kind() {
                        "string_literal" => {
                            let mut inner = value.walk();
                            let content = value
                                .named_children(&mut inner)
                                .find(|child| child.kind() == "string_content");
                            match content {
                                Some(content) => (self.text(content).to_string(), content),
                                None => continue,
                            }
                        }
                        "identifier" => {
                            let mut end = index + 2;
                            while tokens.get(end + 1).map(|t| t.kind()) == Some("::")
                                && tokens.get(end + 2).map(|t| t.kind()) == Some("identifier")
                            {
                                end += 2;
                            }
                            let text = &self.source[value.start_byte()..tokens[end].end_byte()];
                            (String::from_utf8_lossy(text).to_string(), *value)
                        }
                        _ => continue,
                    };
                    let segments: Vec<&str> = path.split("::").map(str::trim).collect();
                    if segments.is_empty() || !segments.iter().all(|segment| is_name(segment)) {
                        continue;
                    }
                    let leaf = segments[segments.len() - 1].to_string();
                    if key == "with" {
                        // `with = "module"` calls the module's own pair.
                        for member in ["serialize", "deserialize"] {
                            self.push_protocol(at, ProtocolForm::Invoke, Some(member.to_string()));
                        }
                    }
                    self.push_protocol(at, ProtocolForm::Invoke, Some(leaf));
                }
                continue;
            }
            let mut cursor = node.walk();
            stack.extend(node.children(&mut cursor));
        }
    }

    /// Every method of `impl Trait for Type` is reached through the trait.
    fn mark_trait_impl(&mut self, item: Node<'_>, trait_node: Node<'_>) {
        let trait_name = trait_leaf(trait_node, self.source);
        let member = ProtocolMember::Implicit(dispatch_words(&trait_name));
        let Some(body) = item.child_by_field_name("body") else {
            return;
        };
        let mut cursor = body.walk();
        for function in body.named_children(&mut cursor) {
            if function.kind() != "function_item" {
                continue;
            }
            let Some(name) = function.child_by_field_name("name") else {
                continue;
            };
            self.protocol_members.push(ProtocolStamp {
                name: self.text(name).to_string(),
                at: name.start_byte(),
                member: member.clone(),
            });
        }
    }

    // ── Output ────────────────────────────────────────────────────────────

    fn push_value(&mut self, name: String, occurrence: ValueOccurrence) {
        self.record.values.entry(name).or_default().push(occurrence);
    }

    fn push_protocol(&mut self, node: Node<'_>, form: ProtocolForm, name: Option<String>) {
        self.record.protocol.push(ProtocolSite {
            line: line_of(node),
            form,
            name,
        });
    }
}

/// Where a token tree sits, which decides whether a call it spells has a call
/// relation elsewhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokenPlace {
    /// A macro invoked in a callable body or an initializer: the adapter
    /// extracts its call-shaped runs as calls of that callable.
    CallableBody,
    /// A macro invoked where no callable carries its calls.
    Unextracted,
    /// A `macro_rules!` body, which runs where the macro expands.
    MacroBody,
}

impl TokenPlace {
    fn hides_calls(self) -> bool {
        !matches!(self, Self::CallableBody)
    }
}

/// Whether a macro invocation sits where the adapter extracts its calls: in a
/// function body, or in the initializer of a const or static declared in a
/// module or a block. An associated const's initializer is not walked.
fn inside_extracted_body(invocation: Node<'_>) -> bool {
    let mut current = invocation.parent();
    while let Some(node) = current {
        match node.kind() {
            "function_item" => return true,
            "const_item" | "static_item" => {
                return node
                    .parent()
                    .is_some_and(|parent| is_module_body(parent) || parent.kind() == "block")
            }
            "macro_definition" | "attribute_item" => return false,
            _ => {}
        }
        current = node.parent();
    }
    false
}

/// Whether `child` is the tail expression of `block`, the value it yields.
fn is_tail(block: Node<'_>, child: Node<'_>) -> bool {
    let mut cursor = block.walk();
    let last = block
        .named_children(&mut cursor)
        .filter(|node| !matches!(node.kind(), "line_comment" | "block_comment"))
        .last();
    last.is_some_and(|last| {
        last.id() == child.id()
            && !matches!(
                child.kind(),
                "expression_statement" | "let_declaration" | "empty_statement"
            )
            && !is_item_kind(child.kind())
    })
}

/// The `let` conditions of an `if` or `while` condition, through `&&` chains.
fn let_conditions<'tree>(condition: Node<'tree>, out: &mut Vec<Node<'tree>>) {
    match condition.kind() {
        "let_condition" => out.push(condition),
        "let_chain" | "binary_expression" | "parenthesized_expression" => {
            let mut cursor = condition.walk();
            let children: Vec<Node<'tree>> = condition.named_children(&mut cursor).collect();
            for child in children {
                let_conditions(child, out);
            }
        }
        _ => {}
    }
}

/// The first token of the path a `::` before `index` continues.
fn path_prefix(tokens: &[Node<'_>], index: usize) -> Option<usize> {
    let mut start = index;
    while start >= 2
        && tokens[start - 1].kind() == "::"
        && matches!(
            tokens[start - 2].kind(),
            "identifier" | "self" | "super" | "crate" | "metavariable"
        )
    {
        start -= 2;
    }
    (start < index).then_some(start)
}

/// What a path's prefix names: the enclosing type for `Self`, a module for a
/// `crate`, `self` or `super` path or a lowercase segment, anything else
/// otherwise.
fn path_receiver_kind(prefix: &str) -> ReceiverKind {
    let root = prefix.split("::").next().unwrap_or("").trim();
    if root == "Self" {
        return ReceiverKind::Instance;
    }
    let last = prefix.rsplit("::").next().unwrap_or("").trim();
    if matches!(root, "crate" | "self" | "super" | "$crate")
        && last
            .chars()
            .next()
            .is_some_and(|first| !first.is_uppercase())
        || last.chars().next().is_some_and(char::is_lowercase)
    {
        ReceiverKind::Namespace
    } else {
        ReceiverKind::Other
    }
}

/// A trait path's last segment, generic arguments aside.
fn trait_leaf(trait_node: Node<'_>, source: &[u8]) -> String {
    let base = match trait_node.kind() {
        "generic_type" => trait_node.child_by_field_name("type").unwrap_or(trait_node),
        _ => trait_node,
    };
    text_of(base, source)
        .rsplit("::")
        .next()
        .unwrap_or("")
        .trim()
        .to_string()
}

/// The syntax or code that invokes a method implementing `trait_name`.
fn dispatch_words(trait_name: &str) -> String {
    let words = match trait_name {
        "Drop" => "scope end",
        "Deref" | "DerefMut" => "dereference and method lookup",
        "Index" | "IndexMut" => "indexing",
        "Add" | "Sub" | "Mul" | "Div" | "Rem" | "Neg" | "Not" | "BitAnd" | "BitOr" | "BitXor"
        | "Shl" | "Shr" | "AddAssign" | "SubAssign" | "MulAssign" | "DivAssign" | "RemAssign"
        | "BitAndAssign" | "BitOrAssign" | "BitXorAssign" | "ShlAssign" | "ShrAssign" => {
            "operators"
        }
        "PartialEq" | "Eq" | "PartialOrd" | "Ord" => "comparison operators",
        "Iterator"
        | "IntoIterator"
        | "DoubleEndedIterator"
        | "ExactSizeIterator"
        | "FromIterator"
        | "Extend" => "iteration",
        "Display" | "Debug" | "LowerHex" | "UpperHex" | "Octal" | "Binary" | "LowerExp"
        | "UpperExp" | "Pointer" => "formatting",
        "From" | "Into" | "TryFrom" | "TryInto" | "FromStr" | "AsRef" | "AsMut" | "Borrow"
        | "BorrowMut" | "ToOwned" => "conversion, including `?` and `.into()`",
        "Future" | "IntoFuture" => "await",
        "Hash" => "hashing",
        "Clone" => "cloning",
        "Default" => "defaults",
        "Fn" | "FnMut" | "FnOnce" => "a call of an instance",
        "Error" => "error reporting",
        "Serialize" | "Deserialize" => "serialization",
        other => {
            return format!(
                "dispatch through the `{other}` trait on a generic or trait-object receiver"
            )
        }
    };
    words.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::LanguageAdapter;

    fn census_of(source: &str) -> FileEscapeCensus {
        let tree = crate::languages::RustAdapter
            .parse(source.as_bytes())
            .unwrap();
        census(&tree, source.as_bytes())
    }

    fn uses(
        census: &FileEscapeCensus,
        name: &str,
    ) -> Vec<(u32, OccurrenceContext, OccurrenceBinding)> {
        census
            .record
            .values
            .get(name)
            .map(|values| {
                values
                    .iter()
                    .map(|value| (value.line, value.context, value.binding))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn invoked(census: &FileEscapeCensus) -> Vec<(u32, &str)> {
        census
            .record
            .protocol
            .iter()
            .filter(|site| site.form == ProtocolForm::Invoke)
            .map(|site| (site.line, site.name.as_deref().unwrap_or("")))
            .collect()
    }

    #[test]
    fn function_and_method_paths_used_as_values_are_recorded() {
        let census = census_of(
            r#"use crate::util::parse;

pub struct Store;

impl Store {
    pub fn load(raw: &str) -> Store { let _ = raw; Store }

    pub fn all(raws: &[&str]) -> Vec<Store> {
        raws.iter().map(|raw| Self::load(raw)).collect::<Vec<_>>();
        raws.iter().copied().map(Self::load).collect()
    }
}

fn helper(x: u8) -> u8 { x }

pub fn run(items: Vec<&str>) {
    let loaded: Vec<Store> = items.iter().copied().map(Store::load).collect();
    let callback = helper;
    register(parse, crate::util::render);
    helper(1);
    Store::load("x");
    let _ = (loaded, callback);
}
"#,
        );
        assert_eq!(
            uses(&census, "load"),
            vec![
                (9, OccurrenceContext::Argument, OccurrenceBinding::Member),
                (16, OccurrenceContext::Argument, OccurrenceBinding::Member),
            ],
            "`Self::load` and `Store::load` passed to `map`, never the calls"
        );
        let load = &census.record.values["load"];
        assert_eq!(load[0].receiver_kind, Some(ReceiverKind::Instance));
        assert_eq!(load[1].receiver.as_deref(), Some("Store"));
        assert_eq!(
            uses(&census, "helper"),
            vec![(17, OccurrenceContext::Assignment, OccurrenceBinding::Module)]
        );
        assert_eq!(census.record.values["helper"][0].target_line, Some(13));
        assert_eq!(
            uses(&census, "parse"),
            vec![(18, OccurrenceContext::Argument, OccurrenceBinding::Import)]
        );
        assert_eq!(
            census.record.values["render"][0].receiver_kind,
            Some(ReceiverKind::Namespace)
        );
        for local in ["raw", "raws", "items", "loaded", "callback", "x"] {
            assert!(
                !census.record.values.contains_key(local),
                "{local} is a local: {:?}",
                census.record.values.get(local)
            );
        }
    }

    #[test]
    fn closures_returns_collections_and_statics_carry_the_value() {
        let census = census_of(
            r#"fn helper() {}

static HANDLER: fn() = helper;

pub fn build() -> Vec<fn()> {
    let deferred = move || helper;
    let table = [helper, helper];
    let wrapped = Wrapper { run: helper };
    let _ = (deferred, table, wrapped);
    (helper)();
    vec![helper]
}

pub fn pick() -> fn() {
    helper
}
"#,
        );
        assert_eq!(
            uses(&census, "helper"),
            vec![
                (2, OccurrenceContext::Alias, OccurrenceBinding::Module),
                (5, OccurrenceContext::Return, OccurrenceBinding::Module),
                (6, OccurrenceContext::Collection, OccurrenceBinding::Module),
                (6, OccurrenceContext::Collection, OccurrenceBinding::Module),
                (7, OccurrenceContext::Collection, OccurrenceBinding::Module),
                (10, OccurrenceContext::Argument, OccurrenceBinding::Module),
                (14, OccurrenceContext::Return, OccurrenceBinding::Module),
            ]
        );
        assert_eq!(census.record.values["helper"][0].aliases, vec!["HANDLER"]);
        assert_eq!(invoked(&census), vec![(9, "helper")]);
    }

    #[test]
    fn a_local_shadow_hides_the_item_only_where_it_binds() {
        let census = census_of(
            r#"fn helper() {}

pub fn run(flag: bool) {
    register(helper);
    let helper = 3;
    register(helper);
    match flag {
        true => register(helper),
        false => {}
    }
    for helper in [1] { register(helper); }
    if let Some(helper) = Some(2) { register(helper); } else { register(inner); }
    fn inner() {}
}
"#,
        );
        assert_eq!(
            uses(&census, "helper"),
            vec![(3, OccurrenceContext::Argument, OccurrenceBinding::Module)]
        );
        assert_eq!(
            uses(&census, "inner"),
            vec![(11, OccurrenceContext::Argument, OccurrenceBinding::Local)]
        );
        assert_eq!(census.record.values["inner"][0].target_line, Some(12));
    }

    #[test]
    fn macros_that_hide_calls_are_invocations_and_their_values_are_uses() {
        let census = census_of(
            r#"macro_rules! with_store {
    ($body:expr) => {
        let store = crate::store::open();
        store.flush();
        register(fallback);
        $body
    };
}

thread_local! {
    static CURRENT: Store = make_store();
}

pub fn run(x: u8) {
    assert_eq!(compute(x), LIMIT);
    let _ = vec![handler, crate::util::render];
}
"#,
        );
        assert_eq!(
            invoked(&census),
            vec![
                (2, "open"),
                (3, "flush"),
                (4, "register"),
                (10, "make_store")
            ],
            "calls a macro body or an item-level macro spells have no call relation"
        );
        assert_eq!(
            uses(&census, "fallback"),
            vec![(4, OccurrenceContext::Argument, OccurrenceBinding::Import)]
        );
        assert!(
            !census.record.values.contains_key("compute"),
            "a call inside a body's macro is a call relation, not a value"
        );
        assert_eq!(
            uses(&census, "LIMIT"),
            vec![(14, OccurrenceContext::Argument, OccurrenceBinding::Free)]
        );
        assert_eq!(
            uses(&census, "handler"),
            vec![(15, OccurrenceContext::Argument, OccurrenceBinding::Free)]
        );
        assert_eq!(
            census.record.values["render"][0].receiver.as_deref(),
            Some("crate::util")
        );
        assert!(!census.record.values.contains_key("x"));
    }

    #[test]
    fn a_macro_in_an_associated_const_hides_its_call() {
        let census = census_of(
            "impl Store {\n    const LIMIT: u8 = limit!(compute());\n}\nconst TOP: u8 = limit!(compute());\n",
        );
        assert_eq!(invoked(&census), vec![(1, "compute")]);
    }

    #[test]
    fn derive_helper_attributes_name_the_functions_they_call() {
        let census = census_of(
            r#"#[derive(Deserialize)]
pub struct Config {
    #[serde(default = "default_port", skip_serializing_if = "Option::is_none")]
    port: Option<u16>,
    #[serde(with = "humantime_serde", rename = "timeoutSecs")]
    timeout: Duration,
}
"#,
        );
        assert_eq!(
            invoked(&census),
            vec![
                (2, "default_port"),
                (2, "is_none"),
                (4, "serialize"),
                (4, "deserialize"),
                (4, "humantime_serde"),
            ]
        );
    }

    #[test]
    fn trait_impl_methods_are_protocol_members_and_inherent_ones_are_not() {
        let census = census_of(
            r#"impl std::fmt::Display for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { Ok(()) }
}
impl Sink for Summary {
    fn matched(&mut self) -> bool { true }
}
impl Drop for Guard {
    fn drop(&mut self) {}
}
impl Store {
    fn helper(&self) {}
}
"#,
        );
        let members: Vec<(&str, ProtocolMember)> = census
            .protocol_members
            .iter()
            .map(|stamp| (stamp.name.as_str(), stamp.member.clone()))
            .collect();
        assert_eq!(
            members,
            vec![
                ("fmt", ProtocolMember::Implicit("formatting".into())),
                (
                    "matched",
                    ProtocolMember::Implicit(
                        "dispatch through the `Sink` trait on a generic or trait-object receiver"
                            .into()
                    )
                ),
                ("drop", ProtocolMember::Implicit("scope end".into())),
            ]
        );
    }

    #[test]
    fn standard_library_paths_and_prelude_variants_are_not_recorded() {
        let census = census_of(
            r#"use std::sync::atomic::Ordering;
use std::mem::drop;
use crate::store::Store;

pub fn run(counter: &std::sync::atomic::AtomicU8) {
    counter.load(Ordering::SeqCst);
    register(None, std::convert::identity, drop, Store::load);
    assert_eq!(counter.load(Ordering::Relaxed), 0);
}
"#,
        );
        let names: Vec<&str> = census.record.values.keys().map(String::as_str).collect();
        assert_eq!(names, vec!["load"], "{:?}", census.record.values);
    }

    #[test]
    fn a_test_module_glob_of_its_parent_resolves_through_the_parent() {
        let census = census_of(
            r#"use crate::render::draw;

fn helper() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads() {
        register(helper, draw, None, elsewhere);
    }
}
"#,
        );
        let helper = &census.record.values["helper"][0];
        assert_eq!(helper.binding, OccurrenceBinding::Module);
        assert_eq!(helper.target_line, Some(2));
        let draw = &census.record.values["draw"][0];
        assert_eq!(draw.binding, OccurrenceBinding::Import);
        assert_eq!(draw.module.as_deref(), Some("crate::render"));
        assert_eq!(
            census.record.values["elsewhere"][0].binding,
            OccurrenceBinding::Free
        );
        assert!(!census.record.values.contains_key("None"));
    }

    #[test]
    fn a_renaming_use_is_an_import_alias() {
        let census = census_of("use crate::store::{load as load_store, Store};\n");
        let load = &census.record.values["load"];
        assert_eq!(load[0].context, OccurrenceContext::Import);
        assert_eq!(load[0].aliases, vec!["load_store"]);
        assert_eq!(load[0].module.as_deref(), Some("crate::store"));
        assert!(!census.record.values.contains_key("Store"));
    }

    #[test]
    fn an_unparsed_region_is_recorded() {
        let census = census_of("fn ok() {}\nfn broken( { helper(\n");
        assert!(!census.record.unparsed.is_empty());
        assert!(census
            .record
            .unparsed
            .iter()
            .any(|region| region.names.iter().any(|name| name == "helper")));
    }
}
