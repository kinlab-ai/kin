// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Go's facts for the value-escape record (see [`crate::value_escape`]), and
//! the lexical scope model the Go value-origin reader shares.
//!
//! Go scopes by block. A local declared inside a function is visible from the
//! end of its declaration to the end of the innermost block holding it (an
//! `if`, `for` or `switch` statement and each case clause count as blocks), a
//! parameter, receiver or named result is visible in the whole body, and a
//! read anywhere else names the package-level binding of the same name. So a
//! read is local exactly when it sits inside the region of some local
//! declaration of its name, and [`FunctionScopes`] computes those regions from
//! the syntax. A region is never larger than Go's own, so a read the model
//! calls package-level may in rare shapes be local, which only lists a use too
//! many.
//!
//! A package spans files: an unexported function declared in one file is
//! called and read from every other file of its package. The record therefore
//! lists every unexported package-level value name of the file under
//! `exports`, since the file hands each of them to the rest of its package,
//! and a read of a name no declaration of the file binds is recorded as
//! [`OccurrenceBinding::Free`], since it may read one declared in a sibling
//! file. Exported names are reachable from every package by their own
//! declaration and need no listing.
//!
//! Recorded:
//!
//! - every read of a package-level name or a selector whose value flows on
//!   (an argument, an assignment, a return, a composite literal element, a
//!   channel send): a function value (`register(handler)`), a method value
//!   (`f := s.Load`), a method expression (`(*Store).Load`), a package member
//!   (`http.HandlerFunc(pkg.Serve)`). Never a call's callee, a declaration, a
//!   receiver, a local, or an inert use (a condition, a comparison, an index);
//! - assignments that bind a package variable or a field to another value
//!   (`defaultHandler = h`, `s.onClose = cleanup`), and `//go:linkname`
//!   directives, which alias a function under another name;
//! - reflective sites: `MethodByName` and `Method(i)` on a reflect value,
//!   `plugin.Open` and `Lookup` by a computed name, `unsafe` pointer
//!   conversions and arithmetic, and template execution, which calls the
//!   exported methods its template text names;
//! - protocol sites: a callee reached through parentheses, `(handler)()`;
//! - protocol members: every method, which any interface value holding its
//!   receiver type dispatches to, including one a library outside the
//!   repository calls, and the functions the runtime or the test runner calls
//!   by name (`init`, `main` of package `main`, `TestXxx`, cgo exports).

use std::collections::{BTreeSet, HashMap, HashSet};

use kin_model::{
    FileEscapeRecord, LookupReceiver, ModuleScope, OccurrenceBinding, OccurrenceContext,
    ProtocolForm, ProtocolMember, ProtocolSite, ReceiverKind, ReflectiveForm, ReflectiveReach,
    ReflectiveSite, ValueOccurrence, EXCERPT_MAX, RECEIVER_TEXT_MAX,
};
use tree_sitter::{Node, Tree};

use crate::value_escape::{
    collapsed_text, is_field, is_name, line_of, text_of, unparsed_lines, FileEscapeCensus,
    ProtocolStamp,
};

/// Names the universe block declares: predeclared constants, functions and
/// types. A read of one that no declaration shadows is not a use of anything
/// the repository declares.
pub(crate) const GO_UNIVERSE: &[&str] = &[
    "true",
    "false",
    "nil",
    "iota",
    "append",
    "cap",
    "clear",
    "close",
    "complex",
    "copy",
    "delete",
    "imag",
    "len",
    "make",
    "max",
    "min",
    "new",
    "panic",
    "print",
    "println",
    "real",
    "recover",
    "any",
    "bool",
    "byte",
    "comparable",
    "complex64",
    "complex128",
    "error",
    "float32",
    "float64",
    "int",
    "int8",
    "int16",
    "int32",
    "int64",
    "rune",
    "string",
    "uint",
    "uint8",
    "uint16",
    "uint32",
    "uint64",
    "uintptr",
];

/// The first path segments of the standard library's packages, and `C`, the
/// cgo pseudo-package. An import path whose first segment holds no dot and is
/// one of these names a package no repository outside the Go tree declares.
const GO_STANDARD_ROOTS: &[&str] = &[
    "C",
    "archive",
    "bufio",
    "builtin",
    "bytes",
    "cmp",
    "compress",
    "container",
    "context",
    "crypto",
    "database",
    "debug",
    "embed",
    "encoding",
    "errors",
    "expvar",
    "flag",
    "fmt",
    "go",
    "hash",
    "html",
    "image",
    "index",
    "internal",
    "io",
    "iter",
    "log",
    "maps",
    "math",
    "mime",
    "net",
    "os",
    "path",
    "plugin",
    "reflect",
    "regexp",
    "runtime",
    "slices",
    "sort",
    "strconv",
    "strings",
    "structs",
    "sync",
    "syscall",
    "testing",
    "text",
    "time",
    "unicode",
    "unique",
    "unsafe",
    "weak",
];

/// Whether an import path names a standard library package.
pub(crate) fn is_standard_import(path: &str) -> bool {
    let first = path.split('/').next().unwrap_or(path);
    !first.contains('.') && GO_STANDARD_ROOTS.contains(&first)
}

/// What a method's protocol mark says: any interface value holding the
/// receiver's type dispatches to it without a call that spells the type.
pub(crate) const INTERFACE_DISPATCH: &str = "an interface value holding its receiver type";

/// The `unsafe` functions that make or move a pointer.
const UNSAFE_POINTER_FUNCTIONS: &[&str] = &[
    "Pointer",
    "Add",
    "Slice",
    "SliceData",
    "String",
    "StringData",
];

// ── The file ──────────────────────────────────────────────────────────────

/// What a Go file's top level declares and imports.
#[derive(Debug, Default)]
pub(crate) struct GoFile {
    /// The name the package clause declares.
    pub(crate) package: Option<String>,
    /// Package-level declarations the file makes (functions, variables,
    /// constants and types), by name, with the 0-based line of each.
    pub(crate) declared: HashMap<String, Vec<u32>>,
    /// The package-level functions, variables and constants among them.
    pub(crate) values: BTreeSet<String>,
    /// Local import names, by the path each imports. The local name is the
    /// explicit one, else the last path segment, the rule the adapter's own
    /// import records use.
    pub(crate) imports: HashMap<String, String>,
    /// Whether the file dot-imports a package, which puts that package's
    /// names in the file's scope under their own spelling.
    pub(crate) dot_import: bool,
}

impl GoFile {
    pub(crate) fn read(root: Node<'_>, source: &[u8]) -> Self {
        let mut file = Self::default();
        let mut cursor = root.walk();
        for child in root.named_children(&mut cursor) {
            match child.kind() {
                "package_clause" => {
                    let mut inner = child.walk();
                    file.package = child
                        .named_children(&mut inner)
                        .find(|node| node.kind() == "package_identifier")
                        .map(|node| text_of(node, source).to_string());
                }
                "import_declaration" => file.read_imports(child, source),
                "function_declaration" => {
                    if let Some(name) = child.child_by_field_name("name") {
                        file.declare(text_of(name, source), line_of(child), true);
                    }
                }
                "var_declaration" | "const_declaration" => {
                    for spec in specs_of(child) {
                        let mut names = spec.walk();
                        for name in spec.children_by_field_name("name", &mut names) {
                            file.declare(text_of(name, source), line_of(spec), true);
                        }
                    }
                }
                "type_declaration" => {
                    let mut specs = child.walk();
                    for spec in child.named_children(&mut specs) {
                        if matches!(spec.kind(), "type_spec" | "type_alias") {
                            if let Some(name) = spec.child_by_field_name("name") {
                                file.declare(text_of(name, source), line_of(spec), false);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        file
    }

    fn declare(&mut self, name: &str, line: u32, value: bool) {
        if name.is_empty() || name == "_" {
            return;
        }
        self.declared
            .entry(name.to_string())
            .or_default()
            .push(line);
        if value {
            self.values.insert(name.to_string());
        }
    }

    fn read_imports(&mut self, declaration: Node<'_>, source: &[u8]) {
        let mut stack = vec![declaration];
        while let Some(node) = stack.pop() {
            if node.kind() != "import_spec" {
                let mut cursor = node.walk();
                stack.extend(node.named_children(&mut cursor));
                continue;
            }
            let Some(path) = node.child_by_field_name("path") else {
                continue;
            };
            let path = text_of(path, source)
                .trim_matches(|character| character == '"' || character == '`')
                .to_string();
            if path.is_empty() {
                continue;
            }
            let local = match node.child_by_field_name("name") {
                Some(name) if name.kind() == "dot" => {
                    self.dot_import = true;
                    continue;
                }
                Some(name) if name.kind() == "blank_identifier" => continue,
                Some(name) => text_of(name, source).to_string(),
                None => path.rsplit('/').next().unwrap_or(&path).to_string(),
            };
            if !local.is_empty() {
                self.imports.insert(local, path);
            }
        }
    }

    /// The local name the file imports `path` under, when it imports it.
    pub(crate) fn alias_of(&self, path: &str) -> Option<&str> {
        self.imports
            .iter()
            .find(|(_, imported)| imported.as_str() == path)
            .map(|(local, _)| local.as_str())
    }

    /// Unexported package-level value names, which the file hands to every
    /// other file of its package.
    fn package_private_values(&self) -> Vec<String> {
        self.values
            .iter()
            .filter(|name| {
                name.chars()
                    .next()
                    .is_some_and(|first| !first.is_uppercase())
                    && name.as_str() != "init"
            })
            .cloned()
            .collect()
    }
}

/// The specs a `var` or `const` declaration holds, grouped or not.
pub(crate) fn specs_of(declaration: Node<'_>) -> Vec<Node<'_>> {
    let mut specs = Vec::new();
    let mut cursor = declaration.walk();
    for child in declaration.named_children(&mut cursor) {
        match child.kind() {
            "var_spec" | "const_spec" => specs.push(child),
            "var_spec_list" => {
                let mut inner = child.walk();
                specs.extend(
                    child
                        .named_children(&mut inner)
                        .filter(|spec| spec.kind() == "var_spec"),
                );
            }
            _ => {}
        }
    }
    specs
}

// ── Scopes ────────────────────────────────────────────────────────────────

/// How one local declaration binds its name.
#[derive(Debug, Clone, Copy)]
pub(crate) enum DeclForm<'tree> {
    /// A receiver, parameter or named result, with its declared type.
    Typed {
        type_node: Option<Node<'tree>>,
        receiver: bool,
        variadic: bool,
    },
    /// One name of `names := values` or `var names [T] = values`: its
    /// position among `count` names, the declared type, and the values.
    Bound {
        index: usize,
        count: usize,
        type_node: Option<Node<'tree>>,
        values: Option<Node<'tree>>,
    },
    /// A type declared in the function.
    Type,
    /// A range or receive variable, a type switch alias or a constant.
    Other,
}

/// One declaration of a name inside a function.
#[derive(Debug, Clone)]
pub(crate) struct LocalDecl<'tree> {
    pub(crate) name: String,
    /// The bytes in which a read of the name reads this declaration.
    pub(crate) region: (usize, usize),
    pub(crate) form: DeclForm<'tree>,
}

impl LocalDecl<'_> {
    pub(crate) fn covers(&self, at: usize) -> bool {
        self.region.0 <= at && at < self.region.1
    }
}

/// Every local declaration in a function, function literal, or any subtree
/// holding function literals, with the region each is visible in.
#[derive(Debug, Default)]
pub(crate) struct FunctionScopes<'tree> {
    pub(crate) decls: Vec<LocalDecl<'tree>>,
    declaring: HashSet<usize>,
}

impl<'tree> FunctionScopes<'tree> {
    pub(crate) fn of(node: Node<'tree>, source: &[u8]) -> Self {
        let mut scopes = Self::default();
        scopes.collect(node, source);
        scopes
    }

    /// Whether a read of `name` at byte `at` reads a local declaration.
    pub(crate) fn local_at(&self, name: &str, at: usize) -> bool {
        self.decls
            .iter()
            .any(|decl| decl.name == name && decl.covers(at))
    }

    /// The declarations of `name`.
    pub(crate) fn declarations(&self, name: &str) -> Vec<&LocalDecl<'tree>> {
        self.decls.iter().filter(|decl| decl.name == name).collect()
    }

    /// Whether `node` is the identifier a declaration declares.
    pub(crate) fn is_declaring(&self, node: Node<'_>) -> bool {
        self.declaring.contains(&node.start_byte())
    }

    /// The method receiver's declaration, when the function is a method.
    pub(crate) fn receiver(&self) -> Option<&LocalDecl<'tree>> {
        self.decls
            .iter()
            .find(|decl| matches!(decl.form, DeclForm::Typed { receiver: true, .. }))
    }

    fn push(
        &mut self,
        name: Node<'tree>,
        source: &[u8],
        region: (usize, usize),
        form: DeclForm<'tree>,
    ) {
        let text = text_of(name, source);
        if text.is_empty() || text == "_" || region.0 >= region.1 {
            return;
        }
        self.declaring.insert(name.start_byte());
        self.decls.push(LocalDecl {
            name: text.to_string(),
            region,
            form,
        });
    }

    fn collect(&mut self, node: Node<'tree>, source: &[u8]) {
        match node.kind() {
            "function_declaration" | "method_declaration" | "func_literal" => {
                if let Some(body) = node.child_by_field_name("body") {
                    let region = (body.start_byte(), body.end_byte());
                    if let Some(receiver) = node.child_by_field_name("receiver") {
                        self.parameters(receiver, source, region, true);
                    }
                    if let Some(parameters) = node.child_by_field_name("parameters") {
                        self.parameters(parameters, source, region, false);
                    }
                    if let Some(result) = node
                        .child_by_field_name("result")
                        .filter(|result| result.kind() == "parameter_list")
                    {
                        self.parameters(result, source, region, false);
                    }
                    self.collect(body, source);
                }
                return;
            }
            "short_var_declaration" => {
                if let Some(end) = scope_end(node) {
                    let names = list_identifiers(node.child_by_field_name("left"));
                    let count = names.len();
                    let values = node.child_by_field_name("right");
                    for (index, name) in names.into_iter().enumerate() {
                        self.push(
                            name,
                            source,
                            (node.end_byte(), end),
                            DeclForm::Bound {
                                index,
                                count,
                                type_node: None,
                                values,
                            },
                        );
                    }
                }
            }
            "var_spec" | "const_spec" => {
                if let Some(end) = scope_end(node) {
                    let mut cursor = node.walk();
                    let names: Vec<Node<'tree>> = node
                        .children_by_field_name("name", &mut cursor)
                        .filter(|name| name.kind() == "identifier")
                        .collect();
                    let count = names.len();
                    for (index, name) in names.into_iter().enumerate() {
                        let form = if node.kind() == "var_spec" {
                            DeclForm::Bound {
                                index,
                                count,
                                type_node: node.child_by_field_name("type"),
                                values: node.child_by_field_name("value"),
                            }
                        } else {
                            DeclForm::Other
                        };
                        self.push(name, source, (node.end_byte(), end), form);
                    }
                }
            }
            "range_clause" | "receive_statement" if declares_short(node) => {
                // A range clause's variables live in the loop, and a select
                // case's received variables in the case.
                let end = if node.kind() == "range_clause" {
                    node.parent()
                        .filter(|parent| parent.kind() == "for_statement")
                        .map(|parent| parent.end_byte())
                } else {
                    scope_end(node)
                };
                if let Some(end) = end {
                    for name in list_identifiers(node.child_by_field_name("left")) {
                        self.push(name, source, (node.end_byte(), end), DeclForm::Other);
                    }
                }
            }
            "type_switch_statement" => {
                if let (Some(value), Some(alias)) = (
                    node.child_by_field_name("value"),
                    node.child_by_field_name("alias"),
                ) {
                    // One declaration per case in Go; each case gives the
                    // alias its own type, so it counts as more than one.
                    for name in list_identifiers(Some(alias)) {
                        let region = (value.end_byte(), node.end_byte());
                        self.push(name, source, region, DeclForm::Other);
                        self.push(name, source, region, DeclForm::Other);
                    }
                }
            }
            "type_spec" | "type_alias" => {
                if let (Some(name), Some(end)) = (node.child_by_field_name("name"), scope_end(node))
                {
                    self.push(name, source, (name.start_byte(), end), DeclForm::Type);
                }
            }
            _ => {}
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            self.collect(child, source);
        }
    }

    fn parameters(
        &mut self,
        list: Node<'tree>,
        source: &[u8],
        region: (usize, usize),
        receiver: bool,
    ) {
        let mut cursor = list.walk();
        for declaration in list.named_children(&mut cursor) {
            let variadic = declaration.kind() == "variadic_parameter_declaration";
            if !variadic && declaration.kind() != "parameter_declaration" {
                continue;
            }
            let type_node = declaration.child_by_field_name("type");
            let mut names = declaration.walk();
            let declared: Vec<Node<'tree>> = declaration
                .children_by_field_name("name", &mut names)
                .collect();
            for name in declared {
                self.push(
                    name,
                    source,
                    region,
                    DeclForm::Typed {
                        type_node,
                        receiver,
                        variadic,
                    },
                );
            }
        }
    }
}

/// The end of the innermost block a declaration sits in: a block, or the
/// implicit block of an `if`, `for` or `switch` statement or a case clause.
pub(crate) fn scope_end(node: Node<'_>) -> Option<usize> {
    let mut current = node;
    while let Some(parent) = current.parent() {
        if matches!(
            parent.kind(),
            "block"
                | "if_statement"
                | "for_statement"
                | "expression_switch_statement"
                | "type_switch_statement"
                | "select_statement"
                | "communication_case"
                | "expression_case"
                | "type_case"
                | "default_case"
        ) {
            return Some(parent.end_byte());
        }
        current = parent;
    }
    None
}

/// Whether a range clause or receive statement declares with `:=`.
fn declares_short(node: Node<'_>) -> bool {
    let mut cursor = node.walk();
    let found = node.children(&mut cursor).any(|child| child.kind() == ":=");
    found
}

/// The plain identifiers an expression list holds, in order.
pub(crate) fn list_identifiers(list: Option<Node<'_>>) -> Vec<Node<'_>> {
    let Some(list) = list else {
        return Vec::new();
    };
    if list.kind() == "identifier" {
        return vec![list];
    }
    let mut cursor = list.walk();
    list.named_children(&mut cursor)
        .filter(|node| node.kind() == "identifier")
        .collect()
}

// ── The census ────────────────────────────────────────────────────────────

/// Take the census of one Go file.
pub(crate) fn census(tree: &Tree, source: &[u8]) -> FileEscapeCensus {
    let root = tree.root_node();
    let file = GoFile::read(root, source);
    let mut walker = Walker {
        source,
        record: FileEscapeRecord::new(ModuleScope::Closed),
        protocol_members: Vec::new(),
        scopes: FunctionScopes::default(),
        in_function: false,
        file: &file,
        imports_reflect: file.alias_of("reflect").is_some(),
        plugin: file.alias_of("plugin").map(str::to_string),
        unsafe_alias: file.alias_of("unsafe").map(str::to_string),
        templates: file.alias_of("text/template").is_some()
            || file.alias_of("html/template").is_some(),
        cgo: file.alias_of("C").is_some(),
    };
    walker.record.unparsed = unparsed_lines(tree, source);
    walker.record.exports = file.package_private_values();
    let mut cursor = root.walk();
    let children: Vec<Node<'_>> = root.children(&mut cursor).collect();
    for child in children {
        match child.kind() {
            "function_declaration" | "method_declaration" => walker.visit_function(child),
            "var_declaration" | "const_declaration" => walker.visit_package_specs(child),
            "comment" => walker.linkname(child),
            _ => {}
        }
    }
    FileEscapeCensus {
        record: walker.record,
        protocol_members: walker.protocol_members,
        // Go reflection enumerates methods and fields alike; no member is
        // hidden from it the way a JavaScript prototype method is.
        non_enumerable: Vec::new(),
    }
}

/// Where a read goes.
enum Flow {
    Value(OccurrenceContext, Vec<String>),
    /// A callee reached through parentheses: invoked, but not spelled as a
    /// call of its name.
    Invoke,
}

impl Flow {
    fn value(context: OccurrenceContext) -> Self {
        Self::Value(context, Vec::new())
    }
}

struct Walker<'s, 'tree, 'f> {
    source: &'s [u8],
    record: FileEscapeRecord,
    protocol_members: Vec<ProtocolStamp>,
    scopes: FunctionScopes<'tree>,
    in_function: bool,
    file: &'f GoFile,
    imports_reflect: bool,
    plugin: Option<String>,
    unsafe_alias: Option<String>,
    templates: bool,
    cgo: bool,
}

impl<'s, 'tree> Walker<'s, 'tree, '_> {
    fn text(&self, node: Node<'_>) -> &'s str {
        text_of(node, self.source)
    }

    // ── Declarations ──────────────────────────────────────────────────────

    fn visit_function(&mut self, node: Node<'tree>) {
        self.stamp(node);
        self.scopes = FunctionScopes::of(node, self.source);
        self.in_function = true;
        if let Some(body) = node.child_by_field_name("body") {
            self.visit(body);
        }
        self.in_function = false;
        self.scopes = FunctionScopes::default();
    }

    fn visit_package_specs(&mut self, declaration: Node<'tree>) {
        for spec in specs_of(declaration) {
            if let Some(value) = spec.child_by_field_name("value") {
                self.scopes = FunctionScopes::of(value, self.source);
                self.visit(value);
                self.scopes = FunctionScopes::default();
            }
        }
    }

    /// Mark a function or method the language reaches without a call that
    /// spells it.
    fn stamp(&mut self, node: Node<'_>) {
        let Some(name_node) = node.child_by_field_name("name") else {
            return;
        };
        let name = self.text(name_node).to_string();
        let member = if node.kind() == "method_declaration" {
            Some(INTERFACE_DISPATCH.to_string())
        } else if self.cgo_exported(node, &name) {
            Some("a cgo export called from C".to_string())
        } else if name == "init" && !has_parameters(node) {
            Some("package initialization".to_string())
        } else if name == "main" && self.file.package.as_deref() == Some("main") {
            Some("program start".to_string())
        } else if self.test_entry_point(node, &name) {
            Some("the go test runner".to_string())
        } else {
            None
        };
        if let Some(through) = member {
            self.protocol_members.push(ProtocolStamp {
                name,
                at: name_node.start_byte(),
                member: ProtocolMember::Implicit(through),
            });
        }
    }

    /// Whether a comment right above the function exports it to C.
    fn cgo_exported(&self, node: Node<'_>, name: &str) -> bool {
        if !self.cgo {
            return false;
        }
        let mut previous = node.prev_sibling();
        while let Some(comment) = previous.filter(|sibling| sibling.kind() == "comment") {
            let text = self.text(comment);
            if text
                .strip_prefix("//export ")
                .is_some_and(|rest| rest.trim() == name)
            {
                return true;
            }
            previous = comment.prev_sibling();
        }
        false
    }

    /// Whether `go test` runs the function by its name: `TestXxx(*testing.T)`,
    /// `BenchmarkXxx(*testing.B)`, `FuzzXxx(*testing.F)`,
    /// `TestMain(*testing.M)` or `ExampleXxx()`.
    fn test_entry_point(&self, node: Node<'_>, name: &str) -> bool {
        let parameter_types: Vec<String> = node
            .child_by_field_name("parameters")
            .map(|list| {
                let mut cursor = list.walk();
                let types: Vec<String> = list
                    .named_children(&mut cursor)
                    .filter_map(|declaration| declaration.child_by_field_name("type"))
                    .map(|type_node| self.text(type_node).split_whitespace().collect())
                    .collect();
                types
            })
            .unwrap_or_default();
        let testing = self.file.alias_of("testing");
        for (prefix, kind) in [
            ("Test", "T"),
            ("Benchmark", "B"),
            ("Fuzz", "F"),
            ("Example", ""),
        ] {
            let Some(rest) = name.strip_prefix(prefix) else {
                continue;
            };
            if rest.chars().next().is_some_and(char::is_lowercase) {
                continue;
            }
            if kind.is_empty() {
                return parameter_types.is_empty();
            }
            let Some(testing) = testing else {
                return false;
            };
            let kind = if name == "TestMain" { "M" } else { kind };
            return parameter_types.as_slice() == [format!("*{testing}.{kind}")];
        }
        false
    }

    /// `//go:linkname local path.name` binds `local` to the function
    /// `path.name`, so a call spelling either name reaches the other.
    fn linkname(&mut self, comment: Node<'_>) {
        let text = self.text(comment);
        let Some(rest) = text.strip_prefix("//go:linkname") else {
            return;
        };
        let parts: Vec<&str> = rest.split_whitespace().collect();
        let [local, target] = parts.as_slice() else {
            return;
        };
        let Some((package, leaf)) = target.rsplit_once('.') else {
            return;
        };
        if !is_name(local) || !is_name(leaf) || package.is_empty() {
            return;
        }
        let line = line_of(comment);
        self.push_value(
            (*leaf).to_string(),
            ValueOccurrence {
                line,
                context: OccurrenceContext::Alias,
                binding: OccurrenceBinding::Member,
                receiver: Some(package.chars().take(RECEIVER_TEXT_MAX).collect()),
                receiver_kind: Some(ReceiverKind::Namespace),
                aliases: vec![(*local).to_string()],
                module: None,
                import: None,
                imported: None,
                target_line: None,
                form: None,
            },
        );
        self.push_value(
            (*local).to_string(),
            ValueOccurrence {
                line,
                context: OccurrenceContext::Alias,
                binding: OccurrenceBinding::Module,
                receiver: None,
                receiver_kind: None,
                aliases: vec![(*leaf).to_string()],
                module: None,
                import: None,
                imported: None,
                target_line: None,
                form: None,
            },
        );
    }

    // ── The walk ──────────────────────────────────────────────────────────

    fn visit_children(&mut self, node: Node<'tree>) {
        let mut cursor = node.walk();
        let children: Vec<Node<'tree>> = node.children(&mut cursor).collect();
        for child in children {
            self.visit(child);
        }
    }

    fn visit(&mut self, node: Node<'tree>) {
        match node.kind() {
            "identifier" => self.read_name(node),
            "selector_expression" => self.visit_selector(node),
            "call_expression" => self.visit_call(node),
            "comment" => self.linkname(node),
            "func_literal" => {
                if let Some(body) = node.child_by_field_name("body") {
                    self.visit(body);
                }
            }
            "short_var_declaration" | "range_clause" | "receive_statement" => {
                if let Some(right) = node.child_by_field_name("right") {
                    self.visit(right);
                }
                if node.kind() != "short_var_declaration" && !declares_short(node) {
                    // `for k, v = range m` and `v = <-ch` assign existing
                    // names: an index or selector target still reads.
                    if let Some(left) = node.child_by_field_name("left") {
                        self.visit_targets(left);
                    }
                }
            }
            "assignment_statement" => {
                if let Some(right) = node.child_by_field_name("right") {
                    self.visit(right);
                }
                if let Some(left) = node.child_by_field_name("left") {
                    self.visit_targets(left);
                    self.mark_bindings(node, left);
                }
            }
            "var_spec" | "const_spec" => {
                if let Some(value) = node.child_by_field_name("value") {
                    self.visit(value);
                }
            }
            "type_switch_statement" => {
                let mut cursor = node.walk();
                let children: Vec<Node<'tree>> = node.named_children(&mut cursor).collect();
                for child in children {
                    if !is_field(node, "alias", child) {
                        self.visit(child);
                    }
                }
            }
            "labeled_statement" => {
                let mut cursor = node.walk();
                let children: Vec<Node<'tree>> = node.named_children(&mut cursor).collect();
                for child in children {
                    if child.kind() != "label_name" {
                        self.visit(child);
                    }
                }
            }
            "type_declaration"
            | "parameter_list"
            | "type_arguments"
            | "type_parameter_list"
            | "type_identifier"
            | "qualified_type"
            | "pointer_type"
            | "array_type"
            | "implicit_length_array_type"
            | "slice_type"
            | "map_type"
            | "channel_type"
            | "function_type"
            | "struct_type"
            | "interface_type"
            | "generic_type"
            | "parenthesized_type"
            | "negated_type"
            | "interpreted_string_literal"
            | "raw_string_literal"
            | "rune_literal"
            | "label_name"
            | "field_identifier"
            | "package_identifier"
            | "import_declaration"
            | "package_clause" => {}
            _ => self.visit_children(node),
        }
    }

    /// Visit the targets of an assignment: a plain name is written, but the
    /// operand of a selector or index target is read to find the slot.
    fn visit_targets(&mut self, left: Node<'tree>) {
        let mut cursor = left.walk();
        let targets: Vec<Node<'tree>> = if left.kind() == "expression_list" {
            left.named_children(&mut cursor).collect()
        } else {
            vec![left]
        };
        for target in targets {
            match target.kind() {
                "identifier" => {}
                "selector_expression" => {
                    if let Some(operand) = target.child_by_field_name("operand") {
                        self.visit_receiver(operand);
                    }
                }
                "index_expression" => {
                    if let Some(operand) = target.child_by_field_name("operand") {
                        self.visit_receiver(operand);
                    }
                    if let Some(index) = target.child_by_field_name("index") {
                        self.visit(index);
                    }
                }
                _ => self.visit(target),
            }
        }
    }

    /// Record the targets an assignment binds to another name's value: a
    /// package variable, or a field. A call spelling that name may then reach
    /// the assigned value instead.
    fn mark_bindings(&mut self, assignment: Node<'tree>, left: Node<'tree>) {
        let is_plain = assignment
            .child_by_field_name("operator")
            .is_none_or(|operator| self.text(operator) == "=");
        if !is_plain {
            return;
        }
        let right: Vec<Node<'tree>> = assignment
            .child_by_field_name("right")
            .map(|right| {
                let mut cursor = right.walk();
                let values: Vec<Node<'tree>> = right.named_children(&mut cursor).collect();
                values
            })
            .unwrap_or_default();
        let mut cursor = left.walk();
        let targets: Vec<Node<'tree>> = left.named_children(&mut cursor).collect();
        for (index, target) in targets.into_iter().enumerate() {
            let names_a_value = right.get(index).is_some_and(|value| {
                matches!(
                    value.kind(),
                    "identifier" | "selector_expression" | "func_literal"
                )
            });
            if !names_a_value {
                continue;
            }
            match target.kind() {
                "identifier" => {
                    let name = self.text(target);
                    if name == "_" || self.scopes.local_at(name, target.start_byte()) {
                        continue;
                    }
                    let binding = if self.file.declared.contains_key(name) {
                        OccurrenceBinding::Module
                    } else {
                        OccurrenceBinding::Free
                    };
                    self.push_value(
                        name.to_string(),
                        occurrence(line_of(target), OccurrenceContext::Binding, binding),
                    );
                }
                "selector_expression" => {
                    let (Some(operand), Some(field)) = (
                        target.child_by_field_name("operand"),
                        target.child_by_field_name("field"),
                    ) else {
                        continue;
                    };
                    let receiver_kind = self.receiver_kind(operand);
                    self.push_value(
                        self.text(field).to_string(),
                        ValueOccurrence {
                            receiver: Some(collapsed_text(operand, self.source, RECEIVER_TEXT_MAX)),
                            receiver_kind: Some(receiver_kind),
                            ..occurrence(
                                line_of(field),
                                OccurrenceContext::Binding,
                                OccurrenceBinding::Member,
                            )
                        },
                    );
                }
                _ => {}
            }
        }
    }

    /// Visit an expression whose value is only a receiver: its own name is
    /// not a use, but anything computed inside it is.
    fn visit_receiver(&mut self, node: Node<'tree>) {
        match node.kind() {
            "identifier" => {}
            "selector_expression" => {
                if let Some(operand) = node.child_by_field_name("operand") {
                    self.visit_receiver(operand);
                }
            }
            "parenthesized_expression" => {
                let mut cursor = node.walk();
                let children: Vec<Node<'tree>> = node.named_children(&mut cursor).collect();
                for child in children {
                    self.visit_receiver(child);
                }
            }
            "index_expression" => {
                if let Some(operand) = node.child_by_field_name("operand") {
                    self.visit_receiver(operand);
                }
                if let Some(index) = node.child_by_field_name("index") {
                    self.visit(index);
                }
            }
            _ => self.visit(node),
        }
    }

    // ── Reads ─────────────────────────────────────────────────────────────

    fn read_name(&mut self, node: Node<'tree>) {
        let name = self.text(node);
        if name.is_empty() || name == "_" || self.scopes.is_declaring(node) {
            return;
        }
        let Some(flow) = self.context_of(node) else {
            return;
        };
        if self.scopes.local_at(name, node.start_byte()) {
            return;
        }
        let (binding, target_line) = match self.file.declared.get(name) {
            Some(lines) => (
                OccurrenceBinding::Module,
                (lines.len() == 1).then(|| lines[0]),
            ),
            None if self.file.imports.contains_key(name) || GO_UNIVERSE.contains(&name) => return,
            None => (OccurrenceBinding::Free, None),
        };
        match flow {
            Flow::Invoke => self.push_protocol(node, ProtocolForm::Invoke, Some(name.to_string())),
            Flow::Value(context, aliases) => {
                let mut value = occurrence(line_of(node), context, binding);
                value.aliases = aliases;
                value.target_line = target_line;
                self.push_value(name.to_string(), value);
            }
        }
    }

    fn visit_selector(&mut self, node: Node<'tree>) {
        let (Some(operand), Some(field)) = (
            node.child_by_field_name("operand"),
            node.child_by_field_name("field"),
        ) else {
            self.visit_children(node);
            return;
        };
        let name = self.text(field).to_string();
        match self.context_of(node) {
            Some(Flow::Value(context, aliases)) => {
                let receiver_kind = self.receiver_kind(operand);
                // A package member that is not exported is no value outside
                // its package.
                let unexported_member = receiver_kind == ReceiverKind::Namespace
                    && name
                        .chars()
                        .next()
                        .is_some_and(|first| !first.is_uppercase());
                if !unexported_member {
                    let mut value = occurrence(line_of(field), context, OccurrenceBinding::Member);
                    value.receiver = Some(collapsed_text(operand, self.source, RECEIVER_TEXT_MAX));
                    value.receiver_kind = Some(receiver_kind);
                    value.aliases = aliases;
                    self.push_value(name, value);
                }
            }
            Some(Flow::Invoke) => self.push_protocol(field, ProtocolForm::Invoke, Some(name)),
            None => {}
        }
        self.visit_receiver(operand);
    }

    fn visit_call(&mut self, node: Node<'tree>) {
        if let Some(function) = node.child_by_field_name("function") {
            match function.kind() {
                "identifier" => {}
                "selector_expression" => {
                    self.mark_reflective(node, function);
                    if let Some(operand) = function.child_by_field_name("operand") {
                        self.visit_receiver(operand);
                    }
                }
                _ => self.visit(function),
            }
        }
        if let Some(arguments) = node.child_by_field_name("arguments") {
            self.visit(arguments);
        }
    }

    /// What a selector's operand is.
    fn receiver_kind(&self, operand: Node<'_>) -> ReceiverKind {
        if operand.kind() != "identifier" {
            return ReceiverKind::Other;
        }
        let name = self.text(operand);
        let at = operand.start_byte();
        if self.scopes.local_at(name, at) {
            let is_receiver = self
                .scopes
                .receiver()
                .is_some_and(|receiver| receiver.name == name && receiver.covers(at))
                && self.scopes.declarations(name).len() == 1;
            return if is_receiver {
                ReceiverKind::Instance
            } else {
                ReceiverKind::Other
            };
        }
        if !self.file.declared.contains_key(name) && self.file.imports.contains_key(name) {
            ReceiverKind::Namespace
        } else {
            ReceiverKind::Other
        }
    }

    /// Where the value `node` reads goes, walking up through the syntax that
    /// passes a value through unchanged, or `None` when it goes nowhere a
    /// later call could reach it through.
    fn context_of(&self, node: Node<'_>) -> Option<Flow> {
        let mut child = node;
        let mut wrapped = false;
        loop {
            let parent = child.parent()?;
            match parent.kind() {
                "parenthesized_expression" | "type_assertion_expression" => {}
                "type_conversion_expression" => {
                    if !is_field(parent, "operand", child) {
                        return None;
                    }
                }
                "unary_expression" => {
                    let operator = parent
                        .child_by_field_name("operator")
                        .map(|operator| self.text(operator));
                    if !matches!(operator, Some("&" | "*")) {
                        return None;
                    }
                }
                "argument_list" | "variadic_argument" => {
                    return Some(Flow::value(OccurrenceContext::Argument));
                }
                "literal_element" => {
                    // A key names a field or is compared, never kept as a
                    // callable; a value is held by the literal.
                    let is_key = parent
                        .parent()
                        .filter(|element| element.kind() == "keyed_element")
                        .is_some_and(|element| is_field(element, "key", parent));
                    return (!is_key).then(|| Flow::value(OccurrenceContext::Collection));
                }
                "literal_value" | "keyed_element" => {
                    return Some(Flow::value(OccurrenceContext::Collection));
                }
                "return_statement" => return Some(Flow::value(OccurrenceContext::Return)),
                "send_statement" => {
                    return is_field(parent, "value", child)
                        .then(|| Flow::value(OccurrenceContext::Other));
                }
                "expression_list" => return self.list_flow(parent, child),
                "call_expression" => {
                    // A callee is a call, never a value use; one reached
                    // through parentheses is invoked unspelled.
                    return (wrapped && is_field(parent, "function", child))
                        .then_some(Flow::Invoke);
                }
                "selector_expression"
                | "index_expression"
                | "slice_expression"
                | "binary_expression"
                | "if_statement"
                | "for_statement"
                | "for_clause"
                | "range_clause"
                | "expression_switch_statement"
                | "type_switch_statement"
                | "expression_case"
                | "type_case"
                | "expression_statement"
                | "inc_statement"
                | "dec_statement"
                | "go_statement"
                | "defer_statement"
                | "composite_literal"
                | "block"
                | "statement_list"
                | "labeled_statement"
                | "parameter_declaration"
                | "variadic_parameter_declaration"
                | "short_var_declaration"
                | "assignment_statement"
                | "var_spec"
                | "const_spec"
                | "receive_statement"
                | "array_type"
                | "source_file" => return None,
                _ => return Some(Flow::value(OccurrenceContext::Other)),
            }
            child = parent;
            wrapped = true;
        }
    }

    /// Where a value in an expression list goes, by what holds the list.
    fn list_flow(&self, list: Node<'_>, item: Node<'_>) -> Option<Flow> {
        let holder = list.parent()?;
        let index = {
            let mut cursor = list.walk();
            let position = list
                .named_children(&mut cursor)
                .position(|node| node.id() == item.id());
            position
        };
        match holder.kind() {
            "return_statement" => Some(Flow::value(OccurrenceContext::Return)),
            "short_var_declaration" => {
                is_field(holder, "right", list).then(|| Flow::value(OccurrenceContext::Assignment))
            }
            "var_spec" | "const_spec" => {
                if !is_field(holder, "value", list) {
                    return None;
                }
                // A spec inside a block, a function literal's in a package
                // initializer included, declares a local.
                if self.in_function || scope_end(holder).is_some() {
                    return Some(Flow::value(OccurrenceContext::Assignment));
                }
                // A package-level variable aliases the value under its name.
                let mut cursor = holder.walk();
                let names: Vec<String> = holder
                    .children_by_field_name("name", &mut cursor)
                    .map(|name| self.text(name).to_string())
                    .filter(|name| name != "_")
                    .collect();
                let aliases = match index.and_then(|index| names.get(index)) {
                    Some(name) => vec![name.clone()],
                    None => names,
                };
                Some(Flow::Value(OccurrenceContext::Alias, aliases))
            }
            "assignment_statement" => {
                if !is_field(holder, "right", list) {
                    return None;
                }
                let left = holder.child_by_field_name("left")?;
                let mut cursor = left.walk();
                let targets: Vec<Node<'_>> = left.named_children(&mut cursor).collect();
                let target = index.and_then(|index| targets.get(index)).copied();
                Some(match target {
                    Some(target) => self.target_flow(target),
                    None => Flow::value(OccurrenceContext::Assignment),
                })
            }
            // Case values are compared, and a range operand is iterated.
            "expression_case" | "range_clause" | "receive_statement" => None,
            _ => Some(Flow::value(OccurrenceContext::Other)),
        }
    }

    /// Where a value assigned to `target` goes: a local holds it where no
    /// record follows, a package variable or a field stores it under a name,
    /// and an index stores it in a container.
    fn target_flow(&self, target: Node<'_>) -> Flow {
        match target.kind() {
            "identifier" => {
                let name = self.text(target);
                if self.scopes.local_at(name, target.start_byte()) || name == "_" {
                    Flow::value(OccurrenceContext::Assignment)
                } else {
                    Flow::Value(OccurrenceContext::Alias, vec![name.to_string()])
                }
            }
            "selector_expression" => match target.child_by_field_name("field") {
                Some(field) => {
                    Flow::Value(OccurrenceContext::Alias, vec![self.text(field).to_string()])
                }
                None => Flow::value(OccurrenceContext::Assignment),
            },
            "index_expression" => Flow::value(OccurrenceContext::Collection),
            _ => Flow::value(OccurrenceContext::Assignment),
        }
    }

    // ── Reflective sites ──────────────────────────────────────────────────

    fn mark_reflective(&mut self, call: Node<'tree>, function: Node<'tree>) {
        let (Some(operand), Some(field)) = (
            function.child_by_field_name("operand"),
            function.child_by_field_name("field"),
        ) else {
            return;
        };
        let member = self.text(field);
        let arguments = call_arguments(call);
        let operand_name = (operand.kind() == "identifier").then(|| self.text(operand));
        let package_call = |alias: Option<&str>| {
            operand_name.is_some_and(|name| {
                Some(name) == alias && !self.scopes.local_at(name, operand.start_byte())
            })
        };
        if package_call(self.unsafe_alias.as_deref()) {
            if UNSAFE_POINTER_FUNCTIONS.contains(&member) {
                // A pointer made or moved by `unsafe` can reinterpret any
                // memory, the method tables an interface value holds among it.
                self.push_reflective(
                    call,
                    ReflectiveForm::ComputedMember,
                    ReflectiveReach::Members,
                    None,
                    None,
                );
            }
            return;
        }
        if package_call(self.plugin.as_deref()) {
            if member == "Open" {
                self.push_reflective(
                    call,
                    ReflectiveForm::DynamicImport,
                    ReflectiveReach::Nothing,
                    None,
                    None,
                );
            }
            return;
        }
        match (member, arguments.as_slice()) {
            ("MethodByName", [key]) => self.by_name(
                call,
                operand,
                *key,
                ReceiverKind::Other,
                ReflectiveReach::Members,
            ),
            ("Method", [index]) if !is_string_literal(*index) => {
                // `reflect.Value.Method(i)` fetches a method by its index.
                self.push_reflective(
                    call,
                    ReflectiveForm::ComputedMember,
                    ReflectiveReach::Members,
                    None,
                    None,
                );
            }
            ("Lookup", [key]) if self.plugin.is_some() => self.by_name(
                call,
                operand,
                *key,
                ReceiverKind::Namespace,
                ReflectiveReach::Any,
            ),
            ("Execute" | "ExecuteTemplate", _) if self.templates => {
                // A template calls the exported methods its text names on
                // the data it is handed.
                self.push_reflective(
                    call,
                    ReflectiveForm::ComputedMember,
                    ReflectiveReach::Members,
                    None,
                    None,
                );
            }
            ("NumMethod", []) if self.imports_reflect => {
                self.push_reflective(
                    call,
                    ReflectiveForm::Enumerate,
                    ReflectiveReach::Members,
                    None,
                    None,
                );
            }
            _ => {}
        }
    }

    /// A lookup by name: a literal name reads that member exactly as a
    /// selector would, and a computed one is a reflective site.
    fn by_name(
        &mut self,
        call: Node<'tree>,
        operand: Node<'tree>,
        key: Node<'tree>,
        receiver_kind: ReceiverKind,
        reach: ReflectiveReach,
    ) {
        if let Some(name) = go_string_literal(key, self.source).filter(|name| is_name(name)) {
            let context = match self.context_of(call) {
                Some(Flow::Value(context, _)) => context,
                _ => OccurrenceContext::Other,
            };
            let mut value = occurrence(line_of(key), context, OccurrenceBinding::Member);
            value.receiver = Some(collapsed_text(operand, self.source, RECEIVER_TEXT_MAX));
            value.receiver_kind = Some(receiver_kind);
            self.push_value(name, value);
            return;
        }
        let (prefix, suffix) = self.key_literals(key);
        self.push_reflective(call, ReflectiveForm::ComputedMember, reach, prefix, suffix);
    }

    /// The literals a computed key provably starts and ends with:
    /// `"prefix" + name` and `name + "suffix"`.
    fn key_literals(&self, key: Node<'_>) -> (Option<String>, Option<String>) {
        if key.kind() != "binary_expression"
            || key
                .child_by_field_name("operator")
                .map(|operator| self.text(operator))
                != Some("+")
        {
            return (None, None);
        }
        let end = |field: &str| {
            key.child_by_field_name(field)
                .and_then(|side| go_string_literal(side, self.source))
                .filter(|text| !text.is_empty())
        };
        (end("left"), end("right"))
    }

    // ── Output ────────────────────────────────────────────────────────────

    fn push_value(&mut self, name: String, value: ValueOccurrence) {
        self.record.values.entry(name).or_default().push(value);
    }

    fn push_reflective(
        &mut self,
        node: Node<'_>,
        form: ReflectiveForm,
        reach: ReflectiveReach,
        key_prefix: Option<String>,
        key_suffix: Option<String>,
    ) {
        // A plugin symbol lookup reads the plugin package's module-level
        // bindings, never a member of a type.
        let receiver = (form == ReflectiveForm::ComputedMember && reach == ReflectiveReach::Any)
            .then_some(LookupReceiver::Module);
        // Never `called`: the Go adapter records no dynamic call sites, so a
        // site is listed here or nowhere.
        self.record.reflective.push(ReflectiveSite {
            line: line_of(node),
            form,
            reach,
            key_prefix,
            key_suffix,
            called: false,
            excerpt: collapsed_text(node, self.source, EXCERPT_MAX),
            key_parts: Vec::new(),
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

fn occurrence(
    line: u32,
    context: OccurrenceContext,
    binding: OccurrenceBinding,
) -> ValueOccurrence {
    ValueOccurrence {
        line,
        context,
        binding,
        receiver: None,
        receiver_kind: None,
        aliases: Vec::new(),
        module: None,
        import: None,
        imported: None,
        target_line: None,
        form: None,
    }
}

fn has_parameters(function: Node<'_>) -> bool {
    function
        .child_by_field_name("parameters")
        .is_some_and(|list| list.named_child_count() > 0)
}

/// A call's arguments, in order.
fn call_arguments(call: Node<'_>) -> Vec<Node<'_>> {
    call.child_by_field_name("arguments")
        .map(|arguments| {
            let mut cursor = arguments.walk();
            let nodes: Vec<Node<'_>> = arguments.named_children(&mut cursor).collect();
            nodes
        })
        .unwrap_or_default()
}

fn is_string_literal(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "interpreted_string_literal" | "raw_string_literal"
    )
}

/// The text of a Go string literal with no escape sequence.
pub(crate) fn go_string_literal(node: Node<'_>, source: &[u8]) -> Option<String> {
    match node.kind() {
        "interpreted_string_literal" => {
            let mut cursor = node.walk();
            if node
                .named_children(&mut cursor)
                .any(|child| child.kind() == "escape_sequence")
            {
                return None;
            }
            let text = text_of(node, source);
            Some(text.strip_prefix('"')?.strip_suffix('"')?.to_string())
        }
        "raw_string_literal" => {
            let text = text_of(node, source);
            Some(text.strip_prefix('`')?.strip_suffix('`')?.to_string())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::LanguageAdapter;

    fn census_of(source: &str) -> FileEscapeCensus {
        let adapter = crate::languages::GoAdapter;
        let tree = adapter.parse(source.as_bytes()).unwrap();
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

    const SERVER: &str = r#"package server

import (
	"net/http"
	"sort"

	"example.com/app/store"
)

type Server struct {
	store   *store.Store
	onClose func()
}

func (s *Server) Load(name string) error { return nil }

func (s *Server) routes(mux *http.ServeMux) {
	mux.HandleFunc("/health", health)
	mux.Handle("/items", http.HandlerFunc(s.items))
	load := s.Load
	_ = load
	s.onClose = cleanup
	sort.Slice(nil, less)
	s.Load("x")
	health(nil, nil)
}

func (s *Server) items(w http.ResponseWriter, r *http.Request) {}

func health(w http.ResponseWriter, r *http.Request) {}

func cleanup() {}

func less(i, j int) bool { return i < j }

var defaultHandler = health

func install() {
	defaultHandler = cleanup
	if handler := pick(); handler != nil {
		handler(nil, nil)
	}
	register(store.Open)
}
"#;

    #[test]
    fn function_and_method_values_are_recorded_and_calls_are_not() {
        let census = census_of(SERVER);
        assert_eq!(
            uses(&census, "health"),
            vec![
                (17, OccurrenceContext::Argument, OccurrenceBinding::Module),
                (35, OccurrenceContext::Alias, OccurrenceBinding::Module),
            ],
            "the call on line 24 is no value use"
        );
        let items = &census.record.values["items"];
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].binding, OccurrenceBinding::Member);
        assert_eq!(items[0].receiver.as_deref(), Some("s"));
        assert_eq!(items[0].receiver_kind, Some(ReceiverKind::Instance));
        let loads = &census.record.values["Load"];
        assert_eq!(
            loads.len(),
            1,
            "the call s.Load(\"x\") is no use: {loads:?}"
        );
        assert_eq!(loads[0].context, OccurrenceContext::Assignment);
        assert_eq!(
            uses(&census, "less"),
            vec![(22, OccurrenceContext::Argument, OccurrenceBinding::Module)]
        );
        let open = &census.record.values["Open"];
        assert_eq!(open[0].receiver_kind, Some(ReceiverKind::Namespace));
    }

    #[test]
    fn stores_under_a_name_alias_the_value_and_list_the_binding() {
        let census = census_of(SERVER);
        let cleanup = &census.record.values["cleanup"];
        let contexts: Vec<(OccurrenceContext, Vec<String>)> = cleanup
            .iter()
            .map(|value| (value.context, value.aliases.clone()))
            .collect();
        assert_eq!(
            contexts,
            vec![
                (OccurrenceContext::Alias, vec!["onClose".to_string()]),
                (OccurrenceContext::Alias, vec!["defaultHandler".to_string()]),
            ]
        );
        let bindings: Vec<OccurrenceContext> = census.record.values["onClose"]
            .iter()
            .map(|value| value.context)
            .collect();
        assert_eq!(bindings, vec![OccurrenceContext::Binding]);
        assert_eq!(
            uses(&census, "defaultHandler"),
            vec![(38, OccurrenceContext::Binding, OccurrenceBinding::Module)]
        );
    }

    #[test]
    fn locals_are_not_recorded_but_a_shadowed_region_ends_with_its_block() {
        let census = census_of(
            "package p\n\nfunc handler() {}\n\nfunc run() {\n\tif ok {\n\t\thandler := 1\n\t\tuse(handler)\n\t}\n\tuse(handler)\n}\n",
        );
        assert_eq!(
            uses(&census, "handler"),
            vec![(9, OccurrenceContext::Argument, OccurrenceBinding::Module)],
            "only the read after the block reads the package function"
        );
    }

    #[test]
    fn a_name_no_declaration_of_the_file_binds_is_free_and_unexported_names_are_listed() {
        let census = census_of(
            "package p\n\nfunc helper() {}\n\nfunc Exported() {}\n\nvar table = map[string]func(){\"a\": sibling}\n",
        );
        assert_eq!(
            uses(&census, "sibling"),
            vec![(6, OccurrenceContext::Collection, OccurrenceBinding::Free)]
        );
        assert_eq!(census.record.exports, vec!["helper", "table"]);
        assert_eq!(census.record.scope, ModuleScope::Closed);
    }

    #[test]
    fn reflect_unsafe_plugin_and_templates_are_reflective_sites() {
        let census = census_of(
            "package p\n\nimport (\n\t\"plugin\"\n\t\"reflect\"\n\t\"text/template\"\n\t\"unsafe\"\n)\n\nfunc run(x any, name string, t *template.Template) {\n\tv := reflect.ValueOf(x)\n\tv.MethodByName(name).Call(nil)\n\tv.MethodByName(\"Load\").Call(nil)\n\tv.Method(0)\n\tp, _ := plugin.Open(name)\n\tp.Lookup(\"Handler\")\n\tp.Lookup(\"On\" + name)\n\t_ = unsafe.Pointer(&x)\n\t_ = unsafe.Sizeof(x)\n\tt.Execute(nil, x)\n}\n",
        );
        let sites: Vec<(u32, ReflectiveForm, ReflectiveReach, Option<String>)> = census
            .record
            .reflective
            .iter()
            .map(|site| (site.line, site.form, site.reach, site.key_prefix.clone()))
            .collect();
        assert_eq!(
            sites,
            vec![
                (
                    11,
                    ReflectiveForm::ComputedMember,
                    ReflectiveReach::Members,
                    None
                ),
                (
                    13,
                    ReflectiveForm::ComputedMember,
                    ReflectiveReach::Members,
                    None
                ),
                (
                    14,
                    ReflectiveForm::DynamicImport,
                    ReflectiveReach::Nothing,
                    None
                ),
                (
                    16,
                    ReflectiveForm::ComputedMember,
                    ReflectiveReach::Any,
                    Some("On".into())
                ),
                (
                    17,
                    ReflectiveForm::ComputedMember,
                    ReflectiveReach::Members,
                    None
                ),
                (
                    19,
                    ReflectiveForm::ComputedMember,
                    ReflectiveReach::Members,
                    None
                ),
            ]
        );
        assert!(census.record.reflective.iter().all(|site| !site.called));
        let load = &census.record.values["Load"];
        assert_eq!(load[0].binding, OccurrenceBinding::Member);
        assert_eq!(
            census.record.values["Handler"][0].receiver_kind,
            Some(ReceiverKind::Namespace)
        );
    }

    #[test]
    fn methods_and_runtime_entry_points_are_protocol_members() {
        let census = census_of(
            "package main\n\nimport \"testing\"\n\ntype T struct{}\n\nfunc (T) String() string { return \"\" }\n\nfunc init() {}\n\nfunc main() {}\n\nfunc TestLoad(t *testing.T) {}\n\nfunc Testable() {}\n\nfunc helper() {}\n",
        );
        let marked: Vec<(&str, String)> = census
            .protocol_members
            .iter()
            .map(|stamp| {
                let ProtocolMember::Implicit(through) = &stamp.member else {
                    panic!("go marks are implicit");
                };
                (stamp.name.as_str(), through.clone())
            })
            .collect();
        assert_eq!(
            marked,
            vec![
                ("String", INTERFACE_DISPATCH.to_string()),
                ("init", "package initialization".to_string()),
                ("main", "program start".to_string()),
                ("TestLoad", "the go test runner".to_string()),
            ]
        );
    }

    #[test]
    fn linkname_aliases_both_names_and_a_parenthesized_callee_is_invoked() {
        let census = census_of(
            "package p\n\nimport _ \"unsafe\"\n\n//go:linkname runtimeNano runtime.nanotime\nfunc runtimeNano() int64\n\nfunc run() {\n\t(helper)()\n}\n\nfunc helper() {}\n",
        );
        let nanotime = &census.record.values["nanotime"];
        assert_eq!(nanotime[0].aliases, vec!["runtimeNano"]);
        assert_eq!(nanotime[0].receiver.as_deref(), Some("runtime"));
        assert_eq!(
            census.record.values["runtimeNano"][0].aliases,
            vec!["nanotime"]
        );
        assert_eq!(
            census.record.protocol,
            vec![ProtocolSite {
                line: 8,
                form: ProtocolForm::Invoke,
                name: Some("helper".into())
            }]
        );
    }

    #[test]
    fn a_var_in_a_package_initializers_literal_is_a_local() {
        let census = census_of(
            "package p\n\nfunc handler() {}\n\nvar table = func() func() {\n\tvar h = handler\n\treturn h\n}\n\nvar Exported = handler\n",
        );
        let contexts: Vec<(OccurrenceContext, Vec<String>)> = census.record.values["handler"]
            .iter()
            .map(|value| (value.context, value.aliases.clone()))
            .collect();
        assert_eq!(
            contexts,
            vec![
                (OccurrenceContext::Assignment, Vec::new()),
                (OccurrenceContext::Alias, vec!["Exported".to_string()]),
            ]
        );
    }

    #[test]
    fn scopes_follow_go_blocks() {
        let source = "package p\n\nfunc (s *S) run(a int) (out int) {\n\tx := 1\n\tfor i := range a {\n\t\t_ = i\n\t}\n\tf := func(y int) { _ = y }\n\t_ = f\n}\n";
        let tree = crate::languages::GoAdapter
            .parse(source.as_bytes())
            .unwrap();
        let function = tree.root_node().named_child(1).unwrap();
        let scopes = FunctionScopes::of(function, source.as_bytes());
        let body = function.child_by_field_name("body").unwrap();
        let names: Vec<&str> = scopes.decls.iter().map(|decl| decl.name.as_str()).collect();
        assert_eq!(names, vec!["s", "a", "out", "x", "i", "f", "y"]);
        assert!(scopes
            .receiver()
            .is_some_and(|receiver| receiver.name == "s"));
        let at = |needle: &str| source.find(needle).unwrap();
        assert!(scopes.local_at("x", at("_ = i")));
        assert!(!scopes.local_at("i", at("f := func")));
        assert!(!scopes.local_at("y", at("_ = f")));
        assert!(scopes.local_at("a", body.start_byte()));
        assert!(
            !scopes.local_at("x", at("x := 1")),
            "a local starts after its declaration"
        );
    }

    #[test]
    fn standard_imports_are_told_from_module_paths() {
        assert!(is_standard_import("net/http"));
        assert!(is_standard_import("context"));
        assert!(!is_standard_import("github.com/x/y"));
        assert!(
            !is_standard_import("kin/store"),
            "a dotless module path is not standard"
        );
    }
}
