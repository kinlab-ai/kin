// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Go value origins: what a once-declared local, parameter or receiver holds,
//! and the type a function or method declares it returns.
//!
//! Go types every variable statically, and an assignment never changes a
//! variable's type. So the origin of a name declared exactly once in a
//! function holds at every read that reaches that declaration, however often
//! the variable is reassigned: a receiver or parameter has its declared type,
//! `s := &Store{}` and `s := Store{}` and `s := new(Store)` hold a `Store`,
//! `var s Store` declares one, `s := NewStore()` holds what `NewStore`
//! declares it returns, and `s := x.(Store)` holds what the assertion names.
//! An interface-typed name is recorded with the interface type, so a call
//! through it reaches the interface's own method and never a guessed
//! implementation.
//!
//! Go scopes by block, so a name is recorded only when the function declares
//! it once, nested function literals included, and every read of the name in
//! the function sits inside that declaration's region
//! ([`FunctionScopes`](crate::languages::go_escape::FunctionScopes)). A read
//! outside it names a package-level binding the fact must not describe.
//!
//! A Go function can return several values. A binding is recorded from a call
//! only at the first position of its left side (`s, err := NewStore()`), and a
//! function's declared return is its first result, so the two always describe
//! the same value. A call on a result is legal only when the callee returns
//! one value, so a chain through `x.Get().Load()` reads the same first result.
//!
//! Nothing is recorded for a type the function itself declares, a type
//! parameter, a predeclared type, a standard library type (no repository
//! declares one, and a same-named repository type must not answer for it), a
//! name in a file that dot-imports a package (the name may be that package's),
//! a field read, or a method call on anything but a recorded binding.

use std::collections::{BTreeMap, HashMap, HashSet};

use tree_sitter::{Node, Tree};

use super::{DeclaredReturn, FileFacts, ScopeFacts, ValueOrigin};
use crate::languages::go_escape::{
    is_standard_import, DeclForm, FunctionScopes, GoFile, GO_UNIVERSE,
};

pub(super) fn read_file(tree: &Tree, source: &[u8], _file_path: &str) -> FileFacts {
    let root = tree.root_node();
    let file = GoFile::read(root, source);
    let mut facts = FileFacts::default();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        if !matches!(child.kind(), "function_declaration" | "method_declaration") {
            continue;
        }
        let Some(name) = child
            .child_by_field_name("name")
            .and_then(|name| name.utf8_text(source).ok())
        else {
            continue;
        };
        let scope = scope_facts(child, source, &file);
        // The entity spans the comments directly above the declaration too.
        let start = crate::languages::go::go_leading_comments(&child, source)
            .first()
            .map_or(child.start_byte(), |comment| comment.start_byte());
        facts.by_node.push((
            vec![
                (start, child.end_byte()),
                (child.start_byte(), child.end_byte()),
            ],
            name.to_string(),
            scope,
        ));
    }
    facts
}

/// The facts of one function or method declaration.
pub(crate) fn scope_facts(function: Node<'_>, source: &[u8], file: &GoFile) -> ScopeFacts {
    let reader = Reader::new(function, source, file);
    ScopeFacts {
        version: 0,
        bindings: reader.bindings(),
        returns: reader.returns(),
        provides: Vec::new(),
        supplies: None,
        ..ScopeFacts::default()
    }
}

struct Reader<'tree, 's, 'f> {
    function: Node<'tree>,
    source: &'s [u8],
    file: &'f GoFile,
    scopes: FunctionScopes<'tree>,
    /// Type parameters of the function, or of the receiver's generic type.
    type_parameters: HashSet<String>,
    /// Types the function declares, at any depth.
    local_types: HashSet<String>,
    /// Where each name is read, by name.
    reads: HashMap<String, Vec<usize>>,
}

impl<'tree, 's, 'f> Reader<'tree, 's, 'f> {
    fn new(function: Node<'tree>, source: &'s [u8], file: &'f GoFile) -> Self {
        let scopes = FunctionScopes::of(function, source);
        let local_types = scopes
            .decls
            .iter()
            .filter(|decl| matches!(decl.form, DeclForm::Type))
            .map(|decl| decl.name.clone())
            .collect();
        let mut reader = Self {
            function,
            source,
            file,
            scopes,
            type_parameters: HashSet::new(),
            local_types,
            reads: HashMap::new(),
        };
        reader.collect_type_parameters();
        reader.collect_reads(function);
        reader
    }

    fn text(&self, node: Node<'_>) -> &'s str {
        node.utf8_text(self.source).unwrap_or("")
    }

    fn collect_type_parameters(&mut self) {
        if let Some(list) = self.function.child_by_field_name("type_parameters") {
            let mut stack = vec![list];
            while let Some(node) = stack.pop() {
                if node.kind() == "type_parameter_declaration" {
                    let mut cursor = node.walk();
                    for name in node.children_by_field_name("name", &mut cursor) {
                        self.type_parameters.insert(self.text(name).to_string());
                    }
                    continue;
                }
                let mut cursor = node.walk();
                stack.extend(node.named_children(&mut cursor));
            }
        }
        // `func (s *Set[T]) Add(v T)`: the receiver's type arguments declare
        // the method's type parameters.
        if let Some(receiver) = self.function.child_by_field_name("receiver") {
            let mut stack = vec![receiver];
            while let Some(node) = stack.pop() {
                if node.kind() == "type_arguments" {
                    let mut inner = vec![node];
                    while let Some(argument) = inner.pop() {
                        if argument.kind() == "type_identifier" {
                            self.type_parameters.insert(self.text(argument).to_string());
                        }
                        let mut cursor = argument.walk();
                        inner.extend(argument.named_children(&mut cursor));
                    }
                    continue;
                }
                let mut cursor = node.walk();
                stack.extend(node.named_children(&mut cursor));
            }
        }
    }

    /// Every identifier in the function that is not a declaring one. Some are
    /// not reads (a struct literal's field key), which only makes the
    /// visibility check stricter.
    fn collect_reads(&mut self, node: Node<'_>) {
        let mut stack = vec![node];
        while let Some(current) = stack.pop() {
            if current.kind() == "identifier" && !self.scopes.is_declaring(current) {
                self.reads
                    .entry(self.text(current).to_string())
                    .or_default()
                    .push(current.start_byte());
                continue;
            }
            let mut cursor = current.walk();
            stack.extend(current.named_children(&mut cursor));
        }
    }

    /// The one declaration of `name` in the function, when every read of the
    /// name in the function sits where that declaration is visible.
    fn single(&self, name: &str) -> Option<&crate::languages::go_escape::LocalDecl<'tree>> {
        let [decl] = self.scopes.declarations(name)[..] else {
            return None;
        };
        let visible = self
            .reads
            .get(name)
            .is_none_or(|reads| reads.iter().all(|at| decl.covers(*at)));
        visible.then_some(decl)
    }

    fn bindings(&self) -> BTreeMap<String, ValueOrigin> {
        let mut candidates: BTreeMap<String, ValueOrigin> = BTreeMap::new();
        let names: HashSet<&str> = self
            .scopes
            .decls
            .iter()
            .map(|decl| decl.name.as_str())
            .collect();
        for name in names {
            let Some(decl) = self.single(name) else {
                continue;
            };
            let origin = match decl.form {
                DeclForm::Typed {
                    type_node,
                    variadic: false,
                    ..
                } => type_node
                    .and_then(|node| self.declared_type(node))
                    .map(|type_name| ValueOrigin::Declared { type_name }),
                DeclForm::Bound {
                    index,
                    count,
                    type_node,
                    values,
                } => match type_node {
                    Some(node) => self
                        .declared_type(node)
                        .map(|type_name| ValueOrigin::Declared { type_name }),
                    None => values.and_then(|values| self.bound_origin(values, index, count)),
                },
                _ => None,
            };
            if let Some(origin) = origin {
                candidates.insert(name.to_string(), origin);
            }
        }
        // A chain through a local another binding could not be recorded for
        // describes nothing, so drop it rather than leave it to be guessed.
        loop {
            let unresolved: Vec<String> = candidates
                .iter()
                .filter(|(_, origin)| {
                    locals_of(origin)
                        .iter()
                        .any(|local| !candidates.contains_key(*local))
                })
                .map(|(name, _)| name.clone())
                .collect();
            if unresolved.is_empty() {
                break;
            }
            for name in unresolved {
                candidates.remove(&name);
            }
        }
        candidates
    }

    /// The origin of the value bound at `index` of `count` names from
    /// `values`.
    fn bound_origin(&self, values: Node<'_>, index: usize, count: usize) -> Option<ValueOrigin> {
        let expressions: Vec<Node<'_>> = if values.kind() == "expression_list" {
            let mut cursor = values.walk();
            let nodes: Vec<Node<'_>> = values.named_children(&mut cursor).collect();
            nodes
        } else {
            vec![values]
        };
        if expressions.len() == count {
            return self.expression_origin(expressions[index]);
        }
        // `s, err := NewStore()` binds the first result, and `s, ok :=
        // x.(*Store)` the asserted value; any other position is unknown.
        match (expressions.as_slice(), index) {
            ([single], 0)
                if matches!(
                    single.kind(),
                    "call_expression" | "type_assertion_expression"
                ) =>
            {
                self.expression_origin(*single)
            }
            _ => None,
        }
    }

    fn expression_origin(&self, expression: Node<'_>) -> Option<ValueOrigin> {
        match expression.kind() {
            "parenthesized_expression" => self.expression_origin(expression.named_child(0)?),
            "unary_expression" => {
                let operator = expression.child_by_field_name("operator")?;
                let operand = expression.child_by_field_name("operand")?;
                (self.text(operator) == "&" && operand.kind() == "composite_literal")
                    .then(|| self.expression_origin(operand))
                    .flatten()
            }
            "composite_literal" => {
                let type_name = self.declared_type(expression.child_by_field_name("type")?)?;
                Some(ValueOrigin::Declared { type_name })
            }
            "type_assertion_expression" | "type_conversion_expression" => {
                let type_name = self.declared_type(expression.child_by_field_name("type")?)?;
                Some(ValueOrigin::Declared { type_name })
            }
            "identifier" => {
                let name = self.text(expression);
                self.single(name)?;
                Some(ValueOrigin::Local {
                    name: name.to_string(),
                })
            }
            "call_expression" => {
                if expression.child_by_field_name("type_arguments").is_some() {
                    return None;
                }
                let function = expression.child_by_field_name("function")?;
                match function.kind() {
                    "identifier" => self.plain_call(expression, function),
                    "selector_expression" => {
                        let operand = function.child_by_field_name("operand")?;
                        let method = self.text(function.child_by_field_name("field")?);
                        if let Some(package) = self.package_of(operand) {
                            let path = self.file.imports.get(package)?;
                            if is_standard_import(path) {
                                return None;
                            }
                            return Some(ValueOrigin::Call {
                                callee: format!("{package}.{method}"),
                            });
                        }
                        let receiver = self.expression_origin(operand)?;
                        Some(ValueOrigin::MethodCall {
                            receiver: Box::new(receiver),
                            method: method.to_string(),
                        })
                    }
                    "parenthesized_expression" => None,
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// `NewStore()`, `new(Store)` or the conversion `Store(x)`.
    fn plain_call(&self, call: Node<'_>, function: Node<'_>) -> Option<ValueOrigin> {
        let callee = self.text(function);
        if !self.scopes.declarations(callee).is_empty() || self.type_parameters.contains(callee) {
            // A local function value, or a local type, holds nothing the
            // linker could name.
            return None;
        }
        let shadowed = self.file.declared.contains_key(callee);
        if callee == "new" && !shadowed {
            let arguments = call.child_by_field_name("arguments")?;
            let argument = arguments.named_child(0)?;
            let type_name = self.declared_type(argument)?;
            return Some(ValueOrigin::Declared { type_name });
        }
        if GO_UNIVERSE.contains(&callee) && !shadowed {
            return None;
        }
        if self.file.dot_import && !shadowed {
            return None;
        }
        Some(ValueOrigin::Call {
            callee: callee.to_string(),
        })
    }

    /// The import alias an operand names, when it names one and nothing in
    /// the function shadows it.
    fn package_of(&self, operand: Node<'_>) -> Option<&'s str> {
        if operand.kind() != "identifier" {
            return None;
        }
        let name = self.text(operand);
        (self.file.imports.contains_key(name)
            && self.scopes.declarations(name).is_empty()
            && !self.file.declared.contains_key(name))
        .then_some(name)
    }

    /// The one named type a type expression names: `Store`, `*Store`,
    /// `store.Store`, `Set[int]` (named `Set`, as its methods are).
    fn declared_type(&self, node: Node<'_>) -> Option<String> {
        match node.kind() {
            "type_identifier" | "identifier" => {
                let name = self.text(node);
                let excluded = GO_UNIVERSE.contains(&name)
                    || self.type_parameters.contains(name)
                    || self.local_types.contains(name)
                    || self.file.dot_import;
                (!name.is_empty() && !excluded).then(|| name.to_string())
            }
            "pointer_type" => {
                let inner = node.named_child(0)?;
                // A method call through `**T` does not dereference twice.
                (inner.kind() != "pointer_type")
                    .then(|| self.declared_type(inner))
                    .flatten()
            }
            "qualified_type" => {
                let package = self.text(node.child_by_field_name("package")?);
                let name = self.text(node.child_by_field_name("name")?);
                let path = self.file.imports.get(package)?;
                if is_standard_import(path) || self.file.declared.contains_key(package) {
                    return None;
                }
                Some(format!("{package}.{name}"))
            }
            "generic_type" => self.declared_type(node.child_by_field_name("type")?),
            "parenthesized_type" => self.declared_type(node.named_child(0)?),
            _ => None,
        }
    }

    /// The type the function declares it returns first.
    fn returns(&self) -> Option<DeclaredReturn> {
        let result = self.function.child_by_field_name("result")?;
        let type_node = if result.kind() == "parameter_list" {
            let mut cursor = result.walk();
            let first = result
                .named_children(&mut cursor)
                .find(|declaration| declaration.kind() == "parameter_declaration")?;
            first.child_by_field_name("type")?
        } else {
            result
        };
        self.declared_type(type_node)
            .map(|type_name| DeclaredReturn::Type { type_name })
    }
}

/// The locals an origin reads.
fn locals_of(origin: &ValueOrigin) -> Vec<&str> {
    match origin {
        ValueOrigin::Local { name } => vec![name.as_str()],
        ValueOrigin::MethodCall { receiver, .. } => locals_of(receiver),
        ValueOrigin::Entered { value, .. } => locals_of(value),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::LanguageAdapter;

    fn facts(source: &str) -> FileFacts {
        let tree = crate::languages::GoAdapter
            .parse(source.as_bytes())
            .unwrap();
        read_file(&tree, source.as_bytes(), "pkg/handler.go")
    }

    fn scope<'a>(facts: &'a FileFacts, name: &str) -> &'a ScopeFacts {
        &facts
            .by_node
            .iter()
            .find(|(_, leaf, _)| leaf == name)
            .unwrap_or_else(|| panic!("no facts for {name}"))
            .2
    }

    fn declared(type_name: &str) -> ValueOrigin {
        ValueOrigin::Declared {
            type_name: type_name.into(),
        }
    }

    const HANDLER: &str = r#"package handler

import (
	"database/sql"
	"net/http"

	"example.com/app/store"
)

type Server struct {
	cache *Cache
}

type Cache struct{}

func (c *Cache) Load(key string) string { return key }

func NewCache() *Cache { return &Cache{} }

func Open(dsn string) (*Server, error) { return &Server{}, nil }

func (s *Server) Handle(w http.ResponseWriter, r *http.Request, db *sql.DB, st *store.Store) {
	var local Cache
	byPointer := &Cache{}
	byValue := Cache{}
	byNew := new(Cache)
	built := NewCache()
	opened, err := Open("x")
	fromStore := store.New()
	copied := built
	asserted, ok := any(nil).(*Cache)
	loaded := built.Load("k")
	_, _, _, _, _, _, _, _, _, _, _ = local, byPointer, byValue, byNew, opened, err, fromStore, copied, asserted, ok, loaded
}
"#;

    #[test]
    fn declared_types_constructors_and_first_results_are_recorded() {
        let facts = facts(HANDLER);
        let handle = scope(&facts, "Handle");
        let bindings = &handle.bindings;
        assert_eq!(bindings.get("s"), Some(&declared("Server")));
        assert_eq!(bindings.get("st"), Some(&declared("store.Store")));
        assert_eq!(bindings.get("local"), Some(&declared("Cache")));
        assert_eq!(bindings.get("byPointer"), Some(&declared("Cache")));
        assert_eq!(bindings.get("byValue"), Some(&declared("Cache")));
        assert_eq!(bindings.get("byNew"), Some(&declared("Cache")));
        assert_eq!(bindings.get("asserted"), Some(&declared("Cache")));
        assert_eq!(
            bindings.get("built"),
            Some(&ValueOrigin::Call {
                callee: "NewCache".into()
            })
        );
        assert_eq!(
            bindings.get("opened"),
            Some(&ValueOrigin::Call {
                callee: "Open".into()
            })
        );
        assert_eq!(
            bindings.get("fromStore"),
            Some(&ValueOrigin::Call {
                callee: "store.New".into()
            })
        );
        assert_eq!(
            bindings.get("copied"),
            Some(&ValueOrigin::Local {
                name: "built".into()
            })
        );
        assert_eq!(
            bindings.get("loaded"),
            Some(&ValueOrigin::MethodCall {
                receiver: Box::new(ValueOrigin::Local {
                    name: "built".into()
                }),
                method: "Load".into(),
            })
        );
        for unknown in ["w", "r", "db", "err", "ok"] {
            assert!(
                !bindings.contains_key(unknown),
                "{unknown} holds a standard library or unnamed type: {bindings:?}"
            );
        }
        assert_eq!(
            scope(&facts, "NewCache").returns,
            Some(DeclaredReturn::Type {
                type_name: "Cache".into()
            })
        );
        assert_eq!(
            scope(&facts, "Open").returns,
            Some(DeclaredReturn::Type {
                type_name: "Server".into()
            }),
            "the first of several results"
        );
        assert_eq!(scope(&facts, "Load").returns, None, "string is predeclared");
    }

    #[test]
    fn a_redeclared_name_or_one_read_outside_its_block_carries_no_fact() {
        let facts = facts(
            "package p\n\nvar c *Cache\n\nfunc run(flag bool) {\n\tif flag {\n\t\tc := NewCache()\n\t\tc.Load()\n\t}\n\tc.Load()\n\tx := NewCache()\n\tx, y := Other()\n\t_ = y\n\tfor _, item := range items {\n\t\titem.Load()\n\t}\n}\n",
        );
        let run = scope(&facts, "run");
        assert!(!run.bindings.contains_key("c"), "{:?}", run.bindings);
        assert!(!run.bindings.contains_key("x"), "{:?}", run.bindings);
        assert!(!run.bindings.contains_key("item"), "{:?}", run.bindings);
        assert!(!run.bindings.contains_key("flag"), "bool is predeclared");
    }

    #[test]
    fn interface_parameters_generics_local_types_and_dot_imports_are_guarded() {
        let facts = facts(
            "package p\n\ntype Loader interface{ Load() }\n\nfunc use[T Loader](l Loader, t T, xs ...Loader) {\n\ttype local struct{}\n\tv := local{}\n\t_ = v\n}\n\nfunc (s *Set[K]) Add(k K) *Set[K] { return s }\n",
        );
        let used = scope(&facts, "use");
        assert_eq!(used.bindings.get("l"), Some(&declared("Loader")));
        assert!(!used.bindings.contains_key("t"), "a type parameter");
        assert!(!used.bindings.contains_key("xs"), "a variadic slice");
        assert!(
            !used.bindings.contains_key("v"),
            "a type the function declares"
        );
        let add = scope(&facts, "Add");
        assert_eq!(add.bindings.get("s"), Some(&declared("Set")));
        assert!(!add.bindings.contains_key("k"));
        assert_eq!(
            add.returns,
            Some(DeclaredReturn::Type {
                type_name: "Set".into()
            })
        );
        let dotted = super::tests::facts(
            "package p\n\nimport . \"example.com/lib\"\n\nfunc run(c Client) { c.Do() }\n",
        );
        assert!(scope(&dotted, "run").bindings.is_empty());
    }

    #[test]
    fn a_closure_parameter_is_the_functions_binding_when_declared_once() {
        let facts =
            facts("package p\n\nfunc run() {\n\tvisit(func(c *Cache) {\n\t\tc.Load()\n\t})\n}\n");
        assert_eq!(
            scope(&facts, "run").bindings.get("c"),
            Some(&declared("Cache"))
        );
    }
}
