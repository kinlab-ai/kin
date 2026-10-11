// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! PHP value origins: typed parameters, single-binding locals, typed
//! properties and declared returns.
//!
//! PHP checks a parameter's declared type only when the call enters the
//! function, so a parameter describes its value only while nothing writes the
//! variable again. A variable is therefore recorded only when the callable
//! binds its name exactly once in its whole subtree: parameters, assignments,
//! `foreach` targets, destructuring, `catch` variables, closure and arrow
//! function parameters, every `&` reference to it, and every bare variable
//! passed where a same-file declaration or a known builtin takes the argument
//! by reference. A variable is also withdrawn when any reference to it sits
//! where the binding is not visible: inside a closure that does not capture it
//! with `use`, or inside a nested function or class, which see none of the
//! enclosing variables. A callable that uses `global`, `static` variables,
//! variable variables, `extract`, `compact`, `get_defined_vars`, one-argument
//! `parse_str`, `eval` or `include`/`require` binds names no syntax shows, so
//! it records no variable at all.
//!
//! A typed property is enforced on every write, including a write through a
//! reference, so `$this->store` holds the declared type in every method of
//! the class or trait body that declares `private Store $store;` or promotes
//! a constructor parameter typed `Store`. A class that declares `__get` is
//! left out: an unset typed property there reads through the magic method.
//!
//! Classes are named by their leaf, the way the adapter names class entities,
//! after resolving the file's `use` aliases and namespace. A reference that
//! resolves to a namespace other than the one a same-file class of that leaf
//! is declared in, a PHP builtin class, and any name in a file holding more
//! than one namespace are left out, because the linker resolves a leaf from
//! the caller's file first.
//!
//! One gap stays open: a function or method declared in another file can take
//! a parameter by reference and rebind a variable passed to it. The census
//! sees only the same-file declarations and the builtins it lists.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;

use tree_sitter::{Node, Tree};

use super::{DeclaredReturn, FileFacts, ScopeFacts, ValueOrigin};

/// Class names PHP itself declares in the global namespace. A reference to
/// one of them names no repository class, whatever leaf a repository class
/// shares with it.
const BUILTIN_CLASSES: &[&str] = &[
    "appenditerator",
    "argumentcounterror",
    "arithmeticerror",
    "arrayaccess",
    "arrayiterator",
    "arrayobject",
    "attribute",
    "backedenum",
    "badfunctioncallexception",
    "badmethodcallexception",
    "cachingiterator",
    "callbackfilteriterator",
    "closure",
    "countable",
    "curlfile",
    "dateinterval",
    "dateperiod",
    "datetime",
    "datetimeimmutable",
    "datetimeinterface",
    "datetimezone",
    "directoryiterator",
    "divisionbyzeroerror",
    "domainexception",
    "domdocument",
    "domelement",
    "domnode",
    "domxpath",
    "emptyiterator",
    "error",
    "errorexception",
    "exception",
    "fiber",
    "filesystemiterator",
    "filteriterator",
    "generator",
    "globiterator",
    "infiniteiterator",
    "invalidargumentexception",
    "iterator",
    "iteratoraggregate",
    "iteratoriterator",
    "jsonexception",
    "jsonserializable",
    "lengthexception",
    "limititerator",
    "logicexception",
    "multipleiterator",
    "mysqli",
    "norewinditerator",
    "outofboundsexception",
    "outofrangeexception",
    "overflowexception",
    "pdo",
    "pdoexception",
    "pdostatement",
    "rangeexception",
    "recursivearrayiterator",
    "recursivedirectoryiterator",
    "recursiveiteratoriterator",
    "reflectionclass",
    "reflectionfunction",
    "reflectionmethod",
    "reflectionnamedtype",
    "reflectionobject",
    "reflectionproperty",
    "regexiterator",
    "runtimeexception",
    "serializable",
    "simplexmlelement",
    "splfileinfo",
    "splfileobject",
    "splfixedarray",
    "splheap",
    "splmaxheap",
    "splminheap",
    "splobjectstorage",
    "splpriorityqueue",
    "splqueue",
    "splstack",
    "spltempfileobject",
    "stdclass",
    "stringable",
    "throwable",
    "traversable",
    "typeerror",
    "underflowexception",
    "unexpectedvalueexception",
    "unitenum",
    "valueerror",
    "weakmap",
    "weakreference",
    "xmlreader",
    "xmlwriter",
    "ziparchive",
];

/// Type names that say nothing about which class a value is an instance of.
const OPAQUE_TYPES: &[&str] = &[
    "array", "bool", "boolean", "callable", "double", "false", "float", "int", "integer",
    "iterable", "mixed", "never", "null", "object", "parent", "resource", "static", "string",
    "true", "void",
];

/// Calls that read or write the local variable table by name, so every count
/// of the callable's bindings is untrustworthy.
const SCOPE_TABLE_CALLS: &[&str] = &["extract", "compact", "get_defined_vars", "eval"];

/// Builtins that take an argument by reference, by lowercase name: the
/// positions that are references, and the first position from which every
/// later argument is one.
const BY_REFERENCE_BUILTINS: &[(&str, &[usize], Option<usize>)] = &[
    ("array_multisort", &[], Some(0)),
    ("array_pop", &[0], None),
    ("array_push", &[0], None),
    ("array_shift", &[0], None),
    ("array_splice", &[0], None),
    ("array_unshift", &[0], None),
    ("array_walk", &[0], None),
    ("array_walk_recursive", &[0], None),
    ("arsort", &[0], None),
    ("asort", &[0], None),
    ("dns_get_record", &[2, 3], None),
    ("each", &[0], None),
    ("end", &[0], None),
    ("exec", &[1, 2], None),
    ("flock", &[2], None),
    ("fscanf", &[], Some(2)),
    ("fsockopen", &[2, 3], None),
    ("getimagesize", &[1], None),
    ("getmxrr", &[1, 2], None),
    ("headers_sent", &[0, 1], None),
    ("is_callable", &[2], None),
    ("krsort", &[0], None),
    ("ksort", &[0], None),
    ("mb_convert_variables", &[], Some(2)),
    ("mb_parse_str", &[1], None),
    ("natcasesort", &[0], None),
    ("natsort", &[0], None),
    ("next", &[0], None),
    ("openssl_open", &[1], None),
    ("openssl_private_decrypt", &[1], None),
    ("openssl_private_encrypt", &[1], None),
    ("openssl_public_decrypt", &[1], None),
    ("openssl_public_encrypt", &[1], None),
    ("openssl_seal", &[1, 2], None),
    ("openssl_sign", &[1], None),
    ("parse_str", &[1], None),
    ("passthru", &[1], None),
    ("preg_match", &[2], None),
    ("preg_match_all", &[2], None),
    ("preg_replace", &[4], None),
    ("preg_replace_callback", &[4], None),
    ("preg_replace_callback_array", &[3], None),
    ("prev", &[0], None),
    ("proc_open", &[2], None),
    ("reset", &[0], None),
    ("rsort", &[0], None),
    ("settype", &[0], None),
    ("shuffle", &[0], None),
    ("similar_text", &[2], None),
    ("socket_select", &[0, 1, 2], None),
    ("sort", &[0], None),
    ("sscanf", &[], Some(2)),
    ("str_ireplace", &[3], None),
    ("str_replace", &[3], None),
    ("stream_select", &[0, 1, 2], None),
    ("stream_socket_client", &[1, 2], None),
    ("stream_socket_server", &[1, 2], None),
    ("system", &[1], None),
    ("uasort", &[0], None),
    ("uksort", &[0], None),
    ("usort", &[0], None),
];

pub(super) fn read_file(tree: &Tree, source: &[u8], _file_path: &str) -> FileFacts {
    let mut facts = FileFacts::default();
    for callable in callable_facts(tree, source) {
        facts
            .by_node
            .push((vec![callable.range], callable.leaf, callable.facts));
    }
    facts
}

/// The facts of one callable the adapter may mint an entity for.
#[derive(Debug, Clone)]
pub(crate) struct CallableFacts {
    /// The byte range of the declaring node: a function or method
    /// declaration, or a closure bound to a local.
    pub(crate) range: (usize, usize),
    /// The leaf name the entity carries: a closure's is its local's name
    /// without the sigil.
    pub(crate) leaf: String,
    pub(crate) facts: ScopeFacts,
}

/// The facts of every callable in the file, keyed by its declaring node's
/// byte range. Empty for a tree with a parse error, exactly as
/// [`super::attach_value_origins`] reads nothing from one, so the adapter's
/// call shapes never rest on a fact the graph does not carry.
pub(crate) fn facts_by_range(tree: &Tree, source: &[u8]) -> HashMap<(usize, usize), ScopeFacts> {
    if tree.root_node().has_error() {
        return HashMap::new();
    }
    callable_facts(tree, source)
        .into_iter()
        .map(|callable| (callable.range, callable.facts))
        .collect()
}

fn callable_facts(tree: &Tree, source: &[u8]) -> Vec<CallableFacts> {
    let root = tree.root_node();
    let file = FileContext::read(root, source);
    let mut out = Vec::new();
    let mut stack: Vec<(Node<'_>, Option<Rc<ClassCtx>>)> = vec![(root, None)];
    while let Some((node, class)) = stack.pop() {
        let mut child_class = class.clone();
        match node.kind() {
            "class_declaration"
            | "interface_declaration"
            | "trait_declaration"
            | "enum_declaration"
            | "anonymous_class" => {
                child_class = Some(Rc::new(ClassCtx::read(node, source)));
            }
            "method_declaration" | "function_definition" => {
                let method_class = if node.kind() == "method_declaration" {
                    class.clone()
                } else {
                    None
                };
                if let Some(leaf) = node
                    .child_by_field_name("name")
                    .map(|name| text(name, source).to_string())
                    .filter(|leaf| !leaf.is_empty())
                {
                    let facts = read_callable(node, method_class.as_deref(), &file, source);
                    out.push(CallableFacts {
                        range: (node.start_byte(), node.end_byte()),
                        leaf,
                        facts,
                    });
                }
                // A class nested in a callable is still a class; anything
                // else nested here belongs to no class.
                child_class = None;
            }
            "anonymous_function" | "arrow_function" => {
                if let Some(leaf) = bound_closure_name(node, source) {
                    let mut facts = read_callable(node, None, &file, source);
                    let enclosing = enclosing_callable(node).and_then(|holder| {
                        out.iter()
                            .find(|callable: &&CallableFacts| {
                                callable.range == (holder.start_byte(), holder.end_byte())
                            })
                            .map(|callable| callable.facts.bindings.clone())
                    });
                    if let Some(outer) = enclosing {
                        inherit_captures(node, &outer, &file, source, &mut facts);
                    }
                    out.push(CallableFacts {
                        range: (node.start_byte(), node.end_byte()),
                        leaf,
                        facts,
                    });
                }
                child_class = None;
            }
            _ => {}
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push((child, child_class.clone()));
        }
    }
    out.sort_by_key(|callable| callable.range);
    out
}

/// The nearest function, method or bound closure around a node.
fn enclosing_callable(node: Node<'_>) -> Option<Node<'_>> {
    let mut current = node.parent();
    while let Some(parent) = current {
        if matches!(
            parent.kind(),
            "function_definition" | "method_declaration" | "anonymous_function" | "arrow_function"
        ) {
            return Some(parent);
        }
        current = parent.parent();
    }
    None
}

/// A closure's captured variables hold what the enclosing scope's hold: a
/// `use ($x)` capture copies the value when the closure is created, and an
/// arrow function reads every enclosing variable it does not bind itself.
/// The enclosing fact is carried over when it stands on its own (a declared
/// type, an instance of a class, a function's return, or a method called on
/// one of those), since a fact naming another enclosing variable would name
/// one this scope does not have. A by-reference capture is shared and gets
/// nothing.
fn inherit_captures(
    closure: Node<'_>,
    outer: &BTreeMap<String, ValueOrigin>,
    file: &FileContext,
    source: &[u8],
    facts: &mut ScopeFacts,
) {
    let census = Census::of(closure, file, source);
    if census.blocked {
        return;
    }
    let captured: Vec<String> = match closure.kind() {
        "anonymous_function" => {
            let Some(clause) = use_clause(closure) else {
                return;
            };
            let mut cursor = clause.walk();
            let names = clause
                .named_children(&mut cursor)
                .filter(|child| child.kind() == "variable_name")
                .map(|child| text(child, source).to_string())
                .collect();
            names
        }
        _ => {
            let mut names = Vec::new();
            let mut stack = vec![closure];
            while let Some(node) = stack.pop() {
                if node.kind() == "variable_name" {
                    let name = text(node, source);
                    if name != "$this" && census.count(name) == 0 {
                        names.push(name.to_string());
                    }
                }
                let mut cursor = node.walk();
                stack.extend(node.named_children(&mut cursor));
            }
            names
        }
    };
    for name in captured {
        if facts.bindings.contains_key(&name) {
            continue;
        }
        let own_binding = closure.kind() == "anonymous_function";
        if own_binding && !census.single(&name) {
            continue;
        }
        if let Some(origin) = outer.get(&name).filter(|origin| self_contained(origin)) {
            facts.bindings.insert(name, origin.clone());
        }
    }
}

fn self_contained(origin: &ValueOrigin) -> bool {
    match origin {
        ValueOrigin::Declared { .. } | ValueOrigin::Call { .. } => true,
        ValueOrigin::MethodCall { receiver, .. } => self_contained(receiver),
        _ => false,
    }
}

/// The local a closure or arrow function is assigned to, without the sigil,
/// when it is the whole right side of `$name = ...`.
pub(crate) fn bound_closure_name(closure: Node<'_>, source: &[u8]) -> Option<String> {
    let assignment = closure.parent()?;
    if assignment.kind() != "assignment_expression"
        || assignment.child_by_field_name("right")?.id() != closure.id()
    {
        return None;
    }
    let left = assignment.child_by_field_name("left")?;
    if left.kind() != "variable_name" {
        return None;
    }
    let name = text(left, source).strip_prefix('$')?;
    (name != "this" && is_identifier(name)).then(|| name.to_string())
}

// ── The file's names ────────────────────────────────────────────────────────

/// What the file's top level says about the names in it.
pub(crate) struct FileContext {
    /// How many `namespace` declarations the file holds. With more than one,
    /// which namespace a name resolves in is not read, so no class fact is
    /// recorded.
    namespaces: usize,
    /// The one namespace the file declares, lowercase, when it declares one.
    namespace: Option<String>,
    /// Class aliases `use` binds, lowercase alias to lowercase full name.
    class_uses: HashMap<String, String>,
    /// Aliases as written, lowercase alias to the full name as written.
    class_uses_written: HashMap<String, String>,
    /// Every class-like declaration in the file, by lowercase leaf: the
    /// lowercase full names it is declared under.
    classes: HashMap<String, Vec<String>>,
    /// Functions and methods declared in the file that take a parameter by
    /// reference, by lowercase name: the positions, and the position from
    /// which a variadic reference takes every later argument.
    by_reference: HashMap<String, (Vec<usize>, Option<usize>)>,
    /// Free functions the file declares, by lowercase name.
    functions: HashSet<String>,
}

impl FileContext {
    pub(crate) fn read(root: Node<'_>, source: &[u8]) -> Self {
        let mut context = Self {
            namespaces: 0,
            namespace: None,
            class_uses: HashMap::new(),
            class_uses_written: HashMap::new(),
            classes: HashMap::new(),
            by_reference: HashMap::new(),
            functions: HashSet::new(),
        };
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            match node.kind() {
                "namespace_definition" => {
                    context.namespaces += 1;
                    context.namespace = node
                        .child_by_field_name("name")
                        .map(|name| text(name, source).trim_start_matches('\\').to_lowercase());
                }
                "namespace_use_declaration" => context.read_use(node, source),
                _ => {}
            }
            let mut cursor = node.walk();
            stack.extend(node.named_children(&mut cursor));
        }
        if context.namespaces > 1 {
            context.namespace = None;
        }
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            match node.kind() {
                "class_declaration"
                | "interface_declaration"
                | "trait_declaration"
                | "enum_declaration" => {
                    if let Some(name) = node.child_by_field_name("name") {
                        let leaf = text(name, source).to_lowercase();
                        let full = context.qualify_declared(&leaf);
                        context.classes.entry(leaf).or_default().push(full);
                    }
                }
                "function_definition" | "method_declaration" => {
                    if node.kind() == "function_definition" {
                        if let Some(name) = node.child_by_field_name("name") {
                            context.functions.insert(text(name, source).to_lowercase());
                        }
                    }
                    if let (Some(name), Some(params)) = (
                        node.child_by_field_name("name"),
                        node.child_by_field_name("parameters"),
                    ) {
                        let positions = reference_positions(params);
                        if !positions.0.is_empty() || positions.1.is_some() {
                            let entry = context
                                .by_reference
                                .entry(text(name, source).to_lowercase())
                                .or_default();
                            entry.0.extend(positions.0);
                            entry.1 = match (entry.1, positions.1) {
                                (Some(a), Some(b)) => Some(a.min(b)),
                                (a, b) => a.or(b),
                            };
                        }
                    }
                }
                _ => {}
            }
            let mut cursor = node.walk();
            stack.extend(node.named_children(&mut cursor));
        }
        context
    }

    fn read_use(&mut self, declaration: Node<'_>, source: &[u8]) {
        let declaration_type = use_type(declaration, source);
        // `use A\{B, C as D};` names a prefix and a group of clauses.
        let mut prefix = String::new();
        let mut clauses = Vec::new();
        let mut cursor = declaration.walk();
        for child in declaration.named_children(&mut cursor) {
            match child.kind() {
                "namespace_name" | "qualified_name" | "name" => {
                    prefix = text(child, source).trim_start_matches('\\').to_string();
                }
                "namespace_use_group" => {
                    let mut inner = child.walk();
                    clauses.extend(
                        child
                            .named_children(&mut inner)
                            .filter(|clause| clause.kind() == "namespace_use_clause"),
                    );
                }
                "namespace_use_clause" => clauses.push(child),
                _ => {}
            }
        }
        for clause in clauses {
            let clause_type = use_type(clause, source).or(declaration_type);
            if clause_type.is_some() {
                // `use function` and `use const` bind no class name.
                continue;
            }
            let Some((path, alias)) = use_clause_parts(clause, source) else {
                continue;
            };
            let full = if prefix.is_empty() {
                path
            } else {
                format!("{prefix}\\{path}")
            };
            let local = alias.unwrap_or_else(|| leaf_of(&full).to_string());
            self.class_uses
                .insert(local.to_lowercase(), full.to_lowercase());
            self.class_uses_written.insert(local.to_lowercase(), full);
        }
    }

    fn qualify_declared(&self, leaf: &str) -> String {
        match &self.namespace {
            Some(namespace) => format!("{namespace}\\{leaf}"),
            None => leaf.to_string(),
        }
    }

    /// The lowercase full name a class reference written `written` resolves
    /// to, by PHP's rules for class names.
    fn qualify_reference(&self, written: &str) -> String {
        if let Some(absolute) = written.strip_prefix('\\') {
            return absolute.to_lowercase();
        }
        if let Some(relative) = written.strip_prefix("namespace\\") {
            return self.qualify_declared(&relative.to_lowercase());
        }
        let (first, rest) = match written.split_once('\\') {
            Some((first, rest)) => (first, Some(rest)),
            None => (written, None),
        };
        if let Some(full) = self.class_uses.get(&first.to_lowercase()) {
            return match rest {
                Some(rest) => format!("{full}\\{}", rest.to_lowercase()),
                None => full.clone(),
            };
        }
        self.qualify_declared(&written.to_lowercase())
    }

    /// The leaf a class reference names, for a fact the linker resolves from
    /// this file, or `None` when the linker could resolve it to the wrong
    /// class: a builtin, a same-file class of that leaf in another namespace,
    /// or any name in a file of several namespaces.
    pub(crate) fn class_leaf(&self, written: &str) -> Option<String> {
        let written = written.trim();
        if written.is_empty() || self.namespaces > 1 {
            return None;
        }
        let full = self.qualify_reference(written);
        let leaf = leaf_of(&full);
        if leaf.is_empty() || OPAQUE_TYPES.contains(&leaf) || leaf == "self" {
            return None;
        }
        if !full.contains('\\') && BUILTIN_CLASSES.contains(&leaf) {
            return None;
        }
        if let Some(declared) = self.classes.get(leaf) {
            if declared.iter().any(|declared| *declared != full) {
                return None;
            }
        }
        // The leaf as the source spells it, which is how the class's own
        // declaration spells it when the file declares it.
        let spelled = leaf_of(written);
        let spelled = if spelled.eq_ignore_ascii_case(leaf) {
            spelled.to_string()
        } else {
            // An alias: the imported name's own leaf.
            self.class_uses_written
                .get(&written.to_lowercase())
                .map(|full| leaf_of(full).to_string())
                .unwrap_or_else(|| leaf.to_string())
        };
        Some(spelled)
    }

    /// The leaf a class reference written in a call names, through the
    /// file's aliases, for the owner-qualified name of a static call. Unlike
    /// [`Self::class_leaf`] this states no fact: the linker matches the name
    /// as an entity name and binds nothing when none carries it.
    pub(crate) fn call_owner_leaf(&self, written: &str) -> String {
        let written = written.trim().trim_start_matches('\\');
        if !written.contains('\\') {
            if let Some(full) = self.class_uses_written.get(&written.to_lowercase()) {
                return leaf_of(full).to_string();
            }
        }
        leaf_of(written).to_string()
    }

    /// Whether the file declares a function of this name.
    pub(crate) fn declares_function(&self, name: &str) -> bool {
        self.functions
            .contains(&name.trim_start_matches('\\').to_lowercase())
    }

    /// Whether the file declares a class-like of this leaf.
    pub(crate) fn declares_class(&self, leaf: &str) -> bool {
        self.classes.contains_key(&leaf.to_lowercase())
    }

    /// Whether the file holds at most one namespace, so an unqualified name
    /// resolves in one known namespace.
    pub(crate) fn single_namespace(&self) -> bool {
        self.namespaces <= 1
    }

    fn by_reference_positions(&self, callee: &str) -> Option<(Vec<usize>, Option<usize>)> {
        let callee = callee.trim_start_matches('\\').to_lowercase();
        let leaf = leaf_of(&callee);
        let builtin = BY_REFERENCE_BUILTINS
            .iter()
            .find(|(name, _, _)| *name == leaf)
            .map(|(_, positions, rest)| (positions.to_vec(), *rest));
        let declared = self.by_reference.get(leaf).cloned();
        match (builtin, declared) {
            (None, None) => None,
            (Some(found), None) | (None, Some(found)) => Some(found),
            (Some(mut a), Some(b)) => {
                a.0.extend(b.0);
                a.1 = match (a.1, b.1) {
                    (Some(x), Some(y)) => Some(x.min(y)),
                    (x, y) => x.or(y),
                };
                Some(a)
            }
        }
    }
}

/// `function` or `const` when a use declaration or clause says so.
fn use_type(node: Node<'_>, source: &[u8]) -> Option<&'static str> {
    let kind = node.child_by_field_name("type")?;
    match text(kind, source) {
        "function" => Some("function"),
        "const" => Some("const"),
        _ => None,
    }
}

/// The path and alias a `use` clause spells.
pub(crate) fn use_clause_parts(
    clause: Node<'_>,
    source: &[u8],
) -> Option<(String, Option<String>)> {
    let alias = clause.child_by_field_name("alias");
    let mut cursor = clause.walk();
    let path = clause.named_children(&mut cursor).find(|node| {
        matches!(node.kind(), "qualified_name" | "name")
            && alias.is_none_or(|alias| alias.id() != node.id())
    })?;
    let path = text(path, source).trim_start_matches('\\').to_string();
    if path.is_empty() {
        return None;
    }
    Some((
        path,
        alias
            .map(|alias| text(alias, source).to_string())
            .filter(|alias| !alias.is_empty()),
    ))
}

/// The positions of a parameter list's by-reference parameters, and the
/// position of a by-reference variadic.
fn reference_positions(params: Node<'_>) -> (Vec<usize>, Option<usize>) {
    let mut positions = Vec::new();
    let mut rest = None;
    let mut cursor = params.walk();
    for (at, param) in params.named_children(&mut cursor).enumerate() {
        if !is_reference_parameter(param) {
            continue;
        }
        if param.kind() == "variadic_parameter" {
            rest = Some(at);
        } else {
            positions.push(at);
        }
    }
    (positions, rest)
}

fn is_reference_parameter(param: Node<'_>) -> bool {
    param.child_by_field_name("reference_modifier").is_some() || {
        let mut cursor = param.walk();
        let found = param
            .children(&mut cursor)
            .any(|child| child.kind() == "reference_modifier");
        found
    }
}

// ── Classes ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClassKind {
    Class,
    Interface,
    Trait,
    Enum,
    Anonymous,
}

/// What a method's class body says about `self` and `$this`.
#[derive(Debug)]
pub(crate) struct ClassCtx {
    pub(crate) name: String,
    pub(crate) kind: ClassKind,
    /// Typed instance properties the body declares, by name without the
    /// sigil, with the type node's byte range for resolving it.
    properties: HashMap<String, (usize, usize)>,
}

impl ClassCtx {
    pub(crate) fn read(node: Node<'_>, source: &[u8]) -> Self {
        let kind = match node.kind() {
            "interface_declaration" => ClassKind::Interface,
            "trait_declaration" => ClassKind::Trait,
            "enum_declaration" => ClassKind::Enum,
            "anonymous_class" => ClassKind::Anonymous,
            _ => ClassKind::Class,
        };
        let name = node
            .child_by_field_name("name")
            .map(|name| text(name, source).to_string())
            .unwrap_or_default();
        let mut properties = HashMap::new();
        let mut magic_get = false;
        let mut declared_twice = HashSet::new();
        if let Some(body) = node.child_by_field_name("body") {
            let mut cursor = body.walk();
            for member in body.named_children(&mut cursor) {
                match member.kind() {
                    "property_declaration" => {
                        if has_child_kind(member, "static_modifier") {
                            continue;
                        }
                        let ty = member.child_by_field_name("type");
                        let mut inner = member.walk();
                        for element in member.named_children(&mut inner) {
                            if element.kind() != "property_element" {
                                continue;
                            }
                            let Some(prop) = element
                                .child_by_field_name("name")
                                .and_then(|name| text(name, source).strip_prefix('$'))
                            else {
                                continue;
                            };
                            if let Some(ty) = ty {
                                if properties
                                    .insert(prop.to_string(), (ty.start_byte(), ty.end_byte()))
                                    .is_some()
                                {
                                    declared_twice.insert(prop.to_string());
                                }
                            } else {
                                declared_twice.insert(prop.to_string());
                            }
                        }
                    }
                    "method_declaration" => {
                        let method = member
                            .child_by_field_name("name")
                            .map(|name| text(name, source).to_lowercase());
                        match method.as_deref() {
                            Some("__get") => magic_get = true,
                            Some("__construct") => {
                                if let Some(params) = member.child_by_field_name("parameters") {
                                    let mut inner = params.walk();
                                    for param in params.named_children(&mut inner) {
                                        if param.kind() != "property_promotion_parameter" {
                                            continue;
                                        }
                                        let (Some(ty), Some(prop)) = (
                                            param.child_by_field_name("type"),
                                            param
                                                .child_by_field_name("name")
                                                .and_then(|n| text(n, source).strip_prefix('$')),
                                        ) else {
                                            continue;
                                        };
                                        if properties
                                            .insert(
                                                prop.to_string(),
                                                (ty.start_byte(), ty.end_byte()),
                                            )
                                            .is_some()
                                        {
                                            declared_twice.insert(prop.to_string());
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
        }
        if magic_get {
            properties.clear();
        }
        for prop in declared_twice {
            properties.remove(&prop);
        }
        Self {
            name,
            kind,
            properties,
        }
    }

    /// The class `self` names in this body: none in a trait, whose `self` is
    /// the class using it, nor in an anonymous class.
    pub(crate) fn self_name(&self) -> Option<&str> {
        match self.kind {
            ClassKind::Class | ClassKind::Interface | ClassKind::Enum if !self.name.is_empty() => {
                Some(&self.name)
            }
            _ => None,
        }
    }
}

// ── One callable ─────────────────────────────────────────────────────────────

fn read_callable(
    callable: Node<'_>,
    class: Option<&ClassCtx>,
    file: &FileContext,
    source: &[u8],
) -> ScopeFacts {
    let census = Census::of(callable, file, source);
    let closure = matches!(callable.kind(), "anonymous_function" | "arrow_function");
    let reader = Reader {
        file,
        class: if closure { None } else { class },
        source,
        census: &census,
        tree_source: source,
    };
    let mut scope = ScopeFacts::default();
    if !census.blocked {
        if let Some(params) = callable.child_by_field_name("parameters") {
            let mut cursor = params.walk();
            for param in params.named_children(&mut cursor) {
                if !matches!(
                    param.kind(),
                    "simple_parameter" | "property_promotion_parameter"
                ) {
                    continue;
                }
                let Some(name) = param.child_by_field_name("name") else {
                    continue;
                };
                let name = text(name, source);
                if !census.single(name) {
                    continue;
                }
                if let Some(type_name) = param
                    .child_by_field_name("type")
                    .and_then(|ty| reader.declared_type(ty))
                {
                    scope
                        .bindings
                        .insert(name.to_string(), ValueOrigin::Declared { type_name });
                }
            }
        }
        if let Some(body) = callable.child_by_field_name("body") {
            reader.collect_locals(body, &mut scope.bindings);
        }
    }
    if let Some(class) = reader.class {
        if let Some(body) = callable.child_by_field_name("body") {
            reader.collect_properties(body, class, &mut scope.bindings);
        }
    }
    if !closure {
        scope.returns = callable
            .child_by_field_name("return_type")
            .and_then(|ty| reader.declared_return(ty));
    }
    scope
}

struct Reader<'a> {
    file: &'a FileContext,
    class: Option<&'a ClassCtx>,
    source: &'a [u8],
    census: &'a Census,
    tree_source: &'a [u8],
}

impl Reader<'_> {
    /// The one class a type node names, for a value of that type.
    fn declared_type(&self, ty: Node<'_>) -> Option<String> {
        match ty.kind() {
            "optional_type" => {
                let inner = ty.named_child(0)?;
                self.declared_type(inner)
            }
            "named_type" => {
                let name = ty.named_child(0)?;
                let written = text(name, self.source);
                if written.eq_ignore_ascii_case("self") {
                    return self.class?.self_name().map(str::to_string);
                }
                self.file.class_leaf(written)
            }
            _ => None,
        }
    }

    fn declared_return(&self, ty: Node<'_>) -> Option<DeclaredReturn> {
        let named = match ty.kind() {
            "optional_type" => ty.named_child(0)?,
            _ => ty,
        };
        if named.kind() == "named_type"
            && named
                .named_child(0)
                .is_some_and(|name| text(name, self.source).eq_ignore_ascii_case("static"))
        {
            return Some(DeclaredReturn::Receiver);
        }
        self.declared_type(ty)
            .map(|type_name| DeclaredReturn::Type { type_name })
    }

    /// Record every variable the callable's own scope binds exactly once to a
    /// value whose origin the reader can name.
    fn collect_locals(&self, body: Node<'_>, bindings: &mut BTreeMap<String, ValueOrigin>) {
        let mut stack = vec![body];
        while let Some(node) = stack.pop() {
            if opens_foreign_scope(node) {
                continue;
            }
            match node.kind() {
                "assignment_expression" => {
                    if let (Some(left), Some(right)) = (
                        node.child_by_field_name("left"),
                        node.child_by_field_name("right"),
                    ) {
                        if left.kind() == "variable_name" {
                            let name = text(left, self.source);
                            if name != "$this" && self.census.single(name) {
                                if let Some(origin) = self.expression_origin(right) {
                                    bindings.insert(name.to_string(), origin);
                                }
                            }
                        }
                    }
                }
                "catch_clause" => {
                    if let (Some(ty), Some(name)) = (
                        node.child_by_field_name("type"),
                        node.child_by_field_name("name"),
                    ) {
                        let name = text(name, self.source);
                        if self.census.single(name) {
                            let mut cursor = ty.walk();
                            let types: Vec<Node<'_>> = ty.named_children(&mut cursor).collect();
                            if let [one] = types.as_slice() {
                                if let Some(type_name) = self.declared_type(*one) {
                                    bindings.insert(
                                        name.to_string(),
                                        ValueOrigin::Declared { type_name },
                                    );
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
            let mut cursor = node.walk();
            stack.extend(node.named_children(&mut cursor));
        }
    }

    /// Bind `$this->prop` for every typed property of the class the method's
    /// own scope reads.
    fn collect_properties(
        &self,
        body: Node<'_>,
        class: &ClassCtx,
        bindings: &mut BTreeMap<String, ValueOrigin>,
    ) {
        if class.properties.is_empty() {
            return;
        }
        let mut stack = vec![body];
        while let Some(node) = stack.pop() {
            if opens_foreign_scope(node) {
                continue;
            }
            if let Some(prop) = this_property(node, self.source) {
                if let Some(type_name) = self.property_type(class, prop) {
                    bindings.insert(
                        format!("$this->{prop}"),
                        ValueOrigin::Declared { type_name },
                    );
                }
            }
            let mut cursor = node.walk();
            stack.extend(node.named_children(&mut cursor));
        }
    }

    fn property_type(&self, class: &ClassCtx, prop: &str) -> Option<String> {
        let (start, end) = *class.properties.get(prop)?;
        let written = self.tree_source.get(start..end)?;
        let written = std::str::from_utf8(written).ok()?.trim();
        let written = written.strip_prefix('?').unwrap_or(written).trim();
        if written.contains(['|', '&', '(', ' ']) {
            return None;
        }
        if written.eq_ignore_ascii_case("self") {
            return class.self_name().map(str::to_string);
        }
        self.file.class_leaf(written)
    }

    /// The origin of the value an expression evaluates to, when it is one the
    /// linker can follow.
    fn expression_origin(&self, expression: Node<'_>) -> Option<ValueOrigin> {
        match expression.kind() {
            "parenthesized_expression" => self.expression_origin(expression.named_child(0)?),
            "clone_expression" => self.expression_origin(expression.named_child(0)?),
            "variable_name" => {
                let name = text(expression, self.source);
                (name != "$this").then(|| ValueOrigin::Local {
                    name: name.to_string(),
                })
            }
            "member_access_expression" => {
                let prop = this_property(expression, self.source)?;
                let class = self.class?;
                self.property_type(class, prop)?;
                Some(ValueOrigin::Local {
                    name: format!("$this->{prop}"),
                })
            }
            "object_creation_expression" => {
                let class = expression.named_child(0)?;
                match class.kind() {
                    "name" | "qualified_name" | "relative_name" => {
                        let written = text(class, self.source);
                        let callee = if written.eq_ignore_ascii_case("self") {
                            self.class?.self_name()?.to_string()
                        } else if written.eq_ignore_ascii_case("static")
                            || written.eq_ignore_ascii_case("parent")
                        {
                            return None;
                        } else {
                            self.file.class_leaf(written)?
                        };
                        Some(ValueOrigin::Call { callee })
                    }
                    _ => None,
                }
            }
            "member_call_expression" | "nullsafe_member_call_expression" => {
                if is_first_class_callable(expression) {
                    return None;
                }
                let method = expression.child_by_field_name("name")?;
                if method.kind() != "name" {
                    return None;
                }
                let object = expression.child_by_field_name("object")?;
                let receiver =
                    if object.kind() == "variable_name" && text(object, self.source) == "$this" {
                        ValueOrigin::Declared {
                            type_name: self.class?.self_name()?.to_string(),
                        }
                    } else {
                        self.expression_origin(object)?
                    };
                Some(ValueOrigin::MethodCall {
                    receiver: Box::new(receiver),
                    method: text(method, self.source).to_string(),
                })
            }
            "scoped_call_expression" => {
                if is_first_class_callable(expression) {
                    return None;
                }
                let method = expression.child_by_field_name("name")?;
                if method.kind() != "name" {
                    return None;
                }
                let scope = expression.child_by_field_name("scope")?;
                let type_name = match scope.kind() {
                    "relative_scope" => {
                        if !text(scope, self.source).eq_ignore_ascii_case("self") {
                            return None;
                        }
                        self.class?.self_name()?.to_string()
                    }
                    "name" | "qualified_name" | "relative_name" => {
                        self.file.class_leaf(text(scope, self.source))?
                    }
                    _ => return None,
                };
                Some(ValueOrigin::MethodCall {
                    receiver: Box::new(ValueOrigin::Declared { type_name }),
                    method: text(method, self.source).to_string(),
                })
            }
            "function_call_expression" => {
                if is_first_class_callable(expression) {
                    return None;
                }
                let function = expression.child_by_field_name("function")?;
                if function.kind() != "name" || !self.file.single_namespace() {
                    return None;
                }
                let callee = text(function, self.source);
                if self.file.declares_class(callee) {
                    return None;
                }
                Some(ValueOrigin::Call {
                    callee: callee.to_string(),
                })
            }
            _ => None,
        }
    }
}

/// `prop` of `$this->prop`, when `node` is that read.
pub(crate) fn this_property<'s>(node: Node<'_>, source: &'s [u8]) -> Option<&'s str> {
    if node.kind() != "member_access_expression" {
        return None;
    }
    let object = node.child_by_field_name("object")?;
    let name = node.child_by_field_name("name")?;
    (object.kind() == "variable_name" && text(object, source) == "$this" && name.kind() == "name")
        .then(|| text(name, source))
}

/// Whether a call is PHP's first-class callable syntax, `f(...)`, which
/// creates a closure rather than calling.
pub(crate) fn is_first_class_callable(call: Node<'_>) -> bool {
    call.child_by_field_name("arguments")
        .is_some_and(|arguments| {
            let mut cursor = arguments.walk();
            let found = arguments
                .named_children(&mut cursor)
                .any(|argument| argument.kind() == "variadic_placeholder");
            found
        })
}

/// A node whose variables belong to another scope: a function literal, or a
/// declaration nested in a body.
fn opens_foreign_scope(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "anonymous_function"
            | "arrow_function"
            | "function_definition"
            | "method_declaration"
            | "class_declaration"
            | "interface_declaration"
            | "trait_declaration"
            | "enum_declaration"
            | "anonymous_class"
    )
}

// ── The binding census ───────────────────────────────────────────────────────

/// Which of the enclosing callable's variables a region of its subtree sees.
#[derive(Clone)]
enum Visible {
    All,
    Only(Rc<HashSet<String>>),
    Nothing,
}

impl Visible {
    fn contains(&self, name: &str) -> bool {
        match self {
            Self::All => true,
            Self::Only(names) => names.contains(name),
            Self::Nothing => false,
        }
    }

    fn narrowed(&self, captured: HashSet<String>) -> Self {
        match self {
            Self::All => Self::Only(Rc::new(captured)),
            Self::Only(names) => Self::Only(Rc::new(
                captured
                    .into_iter()
                    .filter(|name| names.contains(name))
                    .collect(),
            )),
            Self::Nothing => Self::Nothing,
        }
    }
}

/// How many times each variable is bound anywhere in one callable, and which
/// are read where the callable's own binding is not visible.
pub(crate) struct Census {
    counts: HashMap<String, usize>,
    invisible: HashSet<String>,
    /// A form that binds names no syntax shows makes every count
    /// untrustworthy.
    pub(crate) blocked: bool,
}

impl Census {
    pub(crate) fn of(callable: Node<'_>, file: &FileContext, source: &[u8]) -> Self {
        let mut census = Self {
            counts: HashMap::new(),
            invisible: HashSet::new(),
            blocked: false,
        };
        let walker = CensusWalk { file, source };
        if let Some(params) = callable.child_by_field_name("parameters") {
            walker.parameters(params, &mut census, &Visible::All);
        }
        if callable.kind() == "anonymous_function" {
            if let Some(clause) = use_clause(callable) {
                walker.use_clause(clause, &mut census, true);
            }
        }
        if let Some(body) = callable.child_by_field_name("body") {
            walker.walk(body, &mut census, &Visible::All);
        }
        census
    }

    /// How many times `name` is bound.
    pub(crate) fn count(&self, name: &str) -> usize {
        self.counts.get(name).copied().unwrap_or(0)
    }

    /// Whether `name` is bound exactly once and every read of it sees that
    /// binding.
    pub(crate) fn single(&self, name: &str) -> bool {
        !self.blocked && self.count(name) == 1 && !self.invisible.contains(name)
    }

    fn bind(&mut self, name: &str) {
        if name != "$this" {
            *self.counts.entry(name.to_string()).or_default() += 1;
        }
    }

    /// A reference binding: the variable is shared with another name that
    /// can be written without spelling it.
    fn poison(&mut self, name: &str) {
        self.bind(name);
        self.bind(name);
    }
}

pub(crate) fn use_clause(closure: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = closure.walk();
    let found = closure
        .named_children(&mut cursor)
        .find(|child| child.kind() == "anonymous_function_use_clause");
    found
}

struct CensusWalk<'a> {
    file: &'a FileContext,
    source: &'a [u8],
}

impl CensusWalk<'_> {
    fn parameters(&self, params: Node<'_>, census: &mut Census, visible: &Visible) {
        let mut cursor = params.walk();
        for param in params.named_children(&mut cursor) {
            if let Some(name) = param.child_by_field_name("name") {
                let name = text(name, self.source);
                if is_reference_parameter(param) {
                    census.poison(name);
                } else {
                    census.bind(name);
                }
            }
            // A default is an expression the caller's scope never sees, but
            // it can hold a closure that reads nothing of ours.
            if let Some(default) = param.child_by_field_name("default_value") {
                self.walk(default, census, visible);
            }
        }
    }

    /// The variables a closure's `use` clause captures. For the closure's own
    /// census each is a binding; for an enclosing census a by-value capture
    /// is a read and a by-reference one a shared binding.
    fn use_clause(&self, clause: Node<'_>, census: &mut Census, own: bool) -> HashSet<String> {
        let mut captured = HashSet::new();
        let mut cursor = clause.walk();
        for child in clause.named_children(&mut cursor) {
            match child.kind() {
                "variable_name" => {
                    let name = text(child, self.source);
                    if own {
                        census.bind(name);
                    }
                    captured.insert(name.to_string());
                }
                "by_ref" => {
                    if let Some(variable) = child.named_child(0) {
                        census.poison(text(variable, self.source));
                    }
                }
                _ => {}
            }
        }
        captured
    }

    fn walk(&self, node: Node<'_>, census: &mut Census, visible: &Visible) {
        match node.kind() {
            "variable_name" => {
                let name = text(node, self.source);
                if name != "$this" && !visible.contains(name) {
                    census.invisible.insert(name.to_string());
                }
                return;
            }
            "dynamic_variable_name"
            | "global_declaration"
            | "function_static_declaration"
            | "include_expression"
            | "include_once_expression"
            | "require_expression"
            | "require_once_expression" => {
                census.blocked = true;
            }
            "assignment_expression" | "augmented_assignment_expression" => {
                if let Some(left) = node.child_by_field_name("left") {
                    self.targets(left, census, false);
                }
            }
            "reference_assignment_expression" => {
                if let Some(left) = node.child_by_field_name("left") {
                    self.targets(left, census, true);
                }
                if let Some(right) = node.child_by_field_name("right") {
                    if right.kind() == "variable_name" {
                        census.poison(text(right, self.source));
                    }
                }
            }
            "update_expression" => {
                if let Some(argument) = node.child_by_field_name("argument") {
                    self.targets(argument, census, false);
                }
            }
            "foreach_statement" => {
                let mut cursor = node.walk();
                let mut after_as = false;
                for child in node.children(&mut cursor) {
                    if child.kind() == "as" {
                        after_as = true;
                        continue;
                    }
                    if after_as && child.is_named() {
                        if node
                            .child_by_field_name("body")
                            .is_some_and(|body| body.id() == child.id())
                        {
                            break;
                        }
                        self.targets(child, census, false);
                        after_as = false;
                    }
                }
            }
            "catch_clause" => {
                if let Some(name) = node.child_by_field_name("name") {
                    census.bind(text(name, self.source));
                }
            }
            "function_call_expression" => {
                if let Some(function) = node.child_by_field_name("function") {
                    if matches!(function.kind(), "name" | "qualified_name") {
                        let callee = text(function, self.source);
                        let lower = callee.trim_start_matches('\\').to_lowercase();
                        let arguments = arguments_of(node);
                        if SCOPE_TABLE_CALLS.contains(&leaf_of(&lower))
                            || (matches!(leaf_of(&lower), "parse_str" | "mb_parse_str")
                                && arguments.len() < 2)
                        {
                            census.blocked = true;
                        }
                        self.reference_arguments(callee, &arguments, census);
                    }
                }
            }
            "member_call_expression"
            | "nullsafe_member_call_expression"
            | "scoped_call_expression" => {
                if let Some(method) = node
                    .child_by_field_name("name")
                    .filter(|name| name.kind() == "name")
                {
                    let arguments = arguments_of(node);
                    if let Some(found) = self
                        .file
                        .by_reference
                        .get(&text(method, self.source).to_lowercase())
                    {
                        let found = found.clone();
                        self.bind_reference_arguments(&found, &arguments, census);
                    }
                }
            }
            "anonymous_function" => {
                if let Some(params) = node.child_by_field_name("parameters") {
                    self.parameters(params, census, visible);
                }
                let captured = match use_clause(node) {
                    Some(clause) => {
                        let captured = self.use_clause(clause, census, false);
                        for name in &captured {
                            if !visible.contains(name) {
                                census.invisible.insert(name.clone());
                            }
                        }
                        captured
                    }
                    None => HashSet::new(),
                };
                let inner = visible.narrowed(captured);
                if let Some(body) = node.child_by_field_name("body") {
                    self.walk(body, census, &inner);
                }
                return;
            }
            "arrow_function" => {
                if let Some(params) = node.child_by_field_name("parameters") {
                    self.parameters(params, census, visible);
                }
                if let Some(body) = node.child_by_field_name("body") {
                    self.walk(body, census, visible);
                }
                return;
            }
            "function_definition"
            | "method_declaration"
            | "class_declaration"
            | "interface_declaration"
            | "trait_declaration"
            | "enum_declaration"
            | "anonymous_class" => {
                // A nested declaration sees none of this scope's variables;
                // its own bindings still count, which only withdraws facts.
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    if child.kind() == "formal_parameters" {
                        self.parameters(child, census, &Visible::Nothing);
                    } else {
                        self.walk(child, census, &Visible::Nothing);
                    }
                }
                return;
            }
            _ => {}
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            self.walk(child, census, visible);
        }
    }

    /// Count every variable a write target binds. A member, subscript or
    /// static property target binds no variable.
    fn targets(&self, target: Node<'_>, census: &mut Census, reference: bool) {
        match target.kind() {
            "variable_name" => {
                let name = text(target, self.source);
                if reference {
                    census.poison(name);
                } else {
                    census.bind(name);
                }
            }
            "by_ref" => {
                let mut cursor = target.walk();
                for child in target.named_children(&mut cursor) {
                    self.targets(child, census, true);
                }
            }
            "dynamic_variable_name" => census.blocked = true,
            "member_access_expression"
            | "nullsafe_member_access_expression"
            | "subscript_expression"
            | "scoped_property_access_expression" => {}
            _ => {
                let mut cursor = target.walk();
                for child in target.named_children(&mut cursor) {
                    self.targets(child, census, reference);
                }
            }
        }
    }

    fn reference_arguments(&self, callee: &str, arguments: &[Node<'_>], census: &mut Census) {
        if let Some(found) = self.file.by_reference_positions(callee) {
            self.bind_reference_arguments(&found, arguments, census);
        }
    }

    fn bind_reference_arguments(
        &self,
        (positions, rest): &(Vec<usize>, Option<usize>),
        arguments: &[Node<'_>],
        census: &mut Census,
    ) {
        for (at, argument) in arguments.iter().enumerate() {
            let named = argument.child_by_field_name("name").is_some();
            let by_reference = named || positions.contains(&at) || rest.is_some_and(|r| at >= r);
            if !by_reference {
                continue;
            }
            let mut cursor = argument.walk();
            let value = argument
                .named_children(&mut cursor)
                .filter(|child| child.kind() != "name")
                .last();
            if let Some(value) = value {
                self.targets(value, census, true);
            }
        }
    }
}

/// The argument nodes of a call, in order.
pub(crate) fn arguments_of(call: Node<'_>) -> Vec<Node<'_>> {
    let Some(arguments) = call.child_by_field_name("arguments") else {
        return Vec::new();
    };
    let mut cursor = arguments.walk();
    let found = arguments
        .named_children(&mut cursor)
        .filter(|argument| argument.kind() == "argument")
        .collect();
    found
}

fn has_child_kind(node: Node<'_>, kind: &str) -> bool {
    let mut cursor = node.walk();
    let found = node.children(&mut cursor).any(|child| child.kind() == kind);
    found
}

/// The last `\`-separated segment of a name.
pub(crate) fn leaf_of(name: &str) -> &str {
    name.rsplit('\\').next().unwrap_or(name)
}

fn is_identifier(text: &str) -> bool {
    let mut chars = text.chars();
    chars
        .next()
        .is_some_and(|first| first.is_alphabetic() || first == '_')
        && chars.all(|ch| ch.is_alphanumeric() || ch == '_')
}

fn text<'a>(node: Node<'_>, source: &'a [u8]) -> &'a str {
    node.utf8_text(source).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(source: &str) -> Vec<CallableFacts> {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_php::LANGUAGE_PHP.into())
            .unwrap();
        let tree = parser.parse(source, None).unwrap();
        assert!(!tree.root_node().has_error(), "fixture must parse");
        callable_facts(&tree, source.as_bytes())
    }

    fn scope<'a>(facts: &'a [CallableFacts], leaf: &str) -> &'a ScopeFacts {
        &facts
            .iter()
            .find(|callable| callable.leaf == leaf)
            .unwrap_or_else(|| panic!("no facts for {leaf}"))
            .facts
    }

    fn declared(type_name: &str) -> ValueOrigin {
        ValueOrigin::Declared {
            type_name: type_name.into(),
        }
    }

    #[test]
    fn typed_parameters_constructors_and_method_calls_are_recorded() {
        let facts = facts(
            "<?php\nnamespace App\\Services;\n\nuse App\\Models\\Store;\nuse App\\Models\\Registry as Reg;\n\n\
             class OrderService {\n    public function place(Store $store, ?Reg $registry, Store|Cache $either, $plain, Store ...$more): ?Store {\n        \
             $cache = new \\App\\Services\\Cache();\n        $made = new Store(1);\n        $mine = new self();\n        $theirs = new static();\n        \
             $repo = $registry->repository();\n        $copy = $made;\n        $built = make_store();\n        $factory = Store::create();\n        \
             return $made;\n    }\n}\n\nclass Cache {}\n",
        );
        let place = scope(&facts, "place");
        assert_eq!(place.bindings.get("$store"), Some(&declared("Store")));
        assert_eq!(place.bindings.get("$registry"), Some(&declared("Registry")));
        assert_eq!(
            place.bindings.get("$either"),
            None,
            "a union names no one class"
        );
        assert_eq!(place.bindings.get("$plain"), None);
        assert_eq!(place.bindings.get("$more"), None, "a variadic is an array");
        assert_eq!(
            place.bindings.get("$cache"),
            Some(&ValueOrigin::Call {
                callee: "Cache".into()
            })
        );
        assert_eq!(
            place.bindings.get("$made"),
            Some(&ValueOrigin::Call {
                callee: "Store".into()
            })
        );
        assert_eq!(
            place.bindings.get("$mine"),
            Some(&ValueOrigin::Call {
                callee: "OrderService".into()
            })
        );
        assert_eq!(
            place.bindings.get("$theirs"),
            None,
            "new static is late bound"
        );
        assert_eq!(
            place.bindings.get("$repo"),
            Some(&ValueOrigin::MethodCall {
                receiver: Box::new(ValueOrigin::Local {
                    name: "$registry".into()
                }),
                method: "repository".into(),
            })
        );
        assert_eq!(
            place.bindings.get("$copy"),
            Some(&ValueOrigin::Local {
                name: "$made".into()
            })
        );
        assert_eq!(
            place.bindings.get("$built"),
            Some(&ValueOrigin::Call {
                callee: "make_store".into()
            })
        );
        assert_eq!(
            place.bindings.get("$factory"),
            Some(&ValueOrigin::MethodCall {
                receiver: Box::new(declared("Store")),
                method: "create".into(),
            })
        );
        assert_eq!(
            place.returns,
            Some(DeclaredReturn::Type {
                type_name: "Store".into()
            })
        );
    }

    #[test]
    fn declared_returns_read_static_self_and_nothing_opaque() {
        let facts = facts(
            "<?php\nclass Builder {\n    public function fluent(): static { return $this; }\n    \
             public function copy(): self { return clone $this; }\n    public function maybe(): ?Builder { return null; }\n    \
             public function either(): Builder|Other { return $this; }\n    public function nothing(): void {}\n    \
             public function list(): array { return []; }\n    public function any(): mixed { return 1; }\n}\n\
             trait Chains {\n    public function again(): self { return $this; }\n}\n",
        );
        assert_eq!(
            scope(&facts, "fluent").returns,
            Some(DeclaredReturn::Receiver)
        );
        assert_eq!(
            scope(&facts, "copy").returns,
            Some(DeclaredReturn::Type {
                type_name: "Builder".into()
            })
        );
        assert_eq!(
            scope(&facts, "maybe").returns,
            Some(DeclaredReturn::Type {
                type_name: "Builder".into()
            })
        );
        for leaf in ["either", "nothing", "list", "any", "again"] {
            assert_eq!(scope(&facts, leaf).returns, None, "{leaf}");
        }
    }

    #[test]
    fn typed_properties_bind_this_members_in_every_method_of_the_body() {
        let facts = facts(
            "<?php\nclass Checkout {\n    private Store $store;\n    private $untyped;\n    public static Store $shared;\n    \
             public function __construct(private readonly Cache $cache, Store $store) { $this->store = $store; }\n    \
             public function run() {\n        $this->store->save();\n        $this->cache->save();\n        $this->untyped->save();\n        \
             $fn = function () { return $this->store->save(); };\n    }\n}\n\
             class Lazy {\n    private Store $store;\n    public function __get($name) { return null; }\n    public function run() { $this->store->save(); }\n}\n",
        );
        let checkout_run = facts
            .iter()
            .find(|callable| callable.leaf == "run")
            .unwrap();
        assert_eq!(
            checkout_run.facts.bindings.get("$this->store"),
            Some(&declared("Store"))
        );
        assert_eq!(
            checkout_run.facts.bindings.get("$this->cache"),
            Some(&declared("Cache")),
            "a promoted constructor parameter is a typed property"
        );
        assert_eq!(checkout_run.facts.bindings.get("$this->untyped"), None);
        let lazy_run = facts
            .iter()
            .filter(|callable| callable.leaf == "run")
            .nth(1)
            .unwrap();
        assert!(
            lazy_run.facts.bindings.is_empty(),
            "a class with __get can read an unset property through it"
        );
        let closure = scope(&facts, "fn");
        assert!(
            closure.bindings.is_empty(),
            "a closure's $this can be rebound"
        );
    }

    #[test]
    fn a_name_bound_twice_or_by_reference_carries_no_fact() {
        let facts = facts(
            "<?php\nfunction twice(Store $s) {\n    $s->save();\n    $s = other();\n}\n\
             function looped(Store $s, array $items) {\n    foreach ($items as $s) {}\n}\n\
             function caught(Store $e) {\n    try {} catch (Failure $e) {}\n}\n\
             function destructured(Store $a) {\n    [$a, $b] = pair();\n}\n\
             function referenced(Store $s) {\n    $alias = &$s;\n}\n\
             function by_reference_param(Store &$s) {\n    $s->save();\n}\n\
             function out_param(Store $m) {\n    preg_match('/x/', 'y', $m);\n}\n\
             function captured_by_reference(Store $s) {\n    $f = function () use (&$s) {};\n}\n\
             function shadowed(Store $s) {\n    $f = fn(Store $s) => $s;\n}\n\
             function reassign(&$target) { $target = new Other(); }\n\
             function passed(Store $s) {\n    reassign($s);\n    $s->save();\n}\n",
        );
        for leaf in [
            "twice",
            "looped",
            "caught",
            "destructured",
            "referenced",
            "by_reference_param",
            "out_param",
            "captured_by_reference",
            "shadowed",
            "passed",
        ] {
            let bindings = &scope(&facts, leaf).bindings;
            assert!(
                !bindings.contains_key("$s")
                    && !bindings.contains_key("$e")
                    && !bindings.contains_key("$a")
                    && !bindings.contains_key("$m"),
                "{leaf}: {bindings:?}"
            );
        }
    }

    #[test]
    fn a_read_the_binding_does_not_reach_withdraws_it() {
        let facts = facts(
            "<?php\nfunction uncaptured(Store $s) {\n    $f = function () { return $s->save(); };\n}\n\
             function captured(Store $s) {\n    $f = function () use ($s) { return $s->save(); };\n    $g = fn() => $s->save();\n}\n\
             function nested(Store $s) {\n    function inner() { return $s; }\n}\n",
        );
        assert!(!scope(&facts, "uncaptured").bindings.contains_key("$s"));
        assert_eq!(
            scope(&facts, "captured").bindings.get("$s"),
            Some(&declared("Store"))
        );
        assert!(!scope(&facts, "nested").bindings.contains_key("$s"));
    }

    #[test]
    fn a_scope_table_form_blocks_the_whole_callable() {
        for body in [
            "global $config;",
            "static $cache = null;",
            "$$name = 1;",
            "extract($row);",
            "compact('s');",
            "get_defined_vars();",
            "parse_str($query);",
            "eval('$s = 1;');",
            "include $path;",
            "require_once 'helpers.php';",
        ] {
            let facts = facts(&format!(
                "<?php\nfunction f(Store $s) {{\n    $made = new Store();\n    {body}\n    $s->save();\n}}\n"
            ));
            assert!(
                scope(&facts, "f").bindings.is_empty(),
                "{body}: {:?}",
                scope(&facts, "f").bindings
            );
        }
    }

    #[test]
    fn class_names_resolve_through_the_file_and_refuse_ambiguity() {
        let file = facts(
            "<?php\nnamespace App;\n\nclass Store {}\n\n\
             function f(Store $mine, \\Vendor\\Store $theirs, \\DateTime $when, \\Closure $cb, Other\\Thing $thing) {}\n",
        );
        let f = scope(&file, "f");
        assert_eq!(f.bindings.get("$mine"), Some(&declared("Store")));
        assert_eq!(
            f.bindings.get("$theirs"),
            None,
            "the same-file Store is another namespace's"
        );
        assert_eq!(f.bindings.get("$when"), None, "a builtin class");
        assert_eq!(f.bindings.get("$cb"), None);
        assert_eq!(f.bindings.get("$thing"), Some(&declared("Thing")));

        let several = facts(
            "<?php\nnamespace A { function f(Store $s) {} }\nnamespace B { function g(Store $s) {} }\n",
        );
        assert!(scope(&several, "f").bindings.is_empty());
        assert!(scope(&several, "g").bindings.is_empty());
    }

    #[test]
    fn a_bound_closure_has_its_own_scope_facts() {
        let facts = facts(
            "<?php\nfunction outer() {\n    $handler = function (Store $s) use ($x) { $s->save(); };\n    $arrow = fn(Cache $c) => $c->save();\n}\n",
        );
        assert_eq!(
            scope(&facts, "handler").bindings.get("$s"),
            Some(&declared("Store"))
        );
        assert_eq!(
            scope(&facts, "arrow").bindings.get("$c"),
            Some(&declared("Cache"))
        );
        assert!(!scope(&facts, "outer").bindings.contains_key("$s"));
    }
}
