// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! C++ value origins: declared parameter, local and field types, explicit
//! constructions, and declared returns.
//!
//! C++ fixes a variable's static type at its declaration, so a name declared
//! `Store s`, `Store& s`, `Store* s` or `std::unique_ptr<Store> s` holds a
//! `Store` at every read the declaration reaches, whatever is assigned to it
//! later. A name declared `auto` holds whatever its initializer's static type
//! is: an explicit construction (`Store{...}`, `new Store`,
//! `std::make_unique<Store>()`) says it outright, and a call, a method call
//! or another name says it through the declarations the linker follows.
//!
//! Scoping is by block, so a name is recorded for a callable only when every
//! declaration of it anywhere in the callable's subtree (parameters, locals,
//! loop and condition variables, lambda parameters and captures, fields of a
//! local class) records the same origin, and every read of it in that subtree
//! sits inside one of those declarations' scopes, after the declarator. A read
//! outside every scope names a member or a global instead, and the name is
//! left out. A name the callable never declares reads the enclosing class's
//! field of that name when the class body in this file declares one.
//!
//! Macros can declare names the grammar cannot see. A callable holding a
//! macro-shaped block (a function definition directly inside a body, which is
//! how the grammar reads `FOREACH(x, items) { ... }`), a local macro
//! definition, or a region the grammar could not parse records nothing, and a
//! name passed bare to a macro-like call (an ALL_CAPS callee or a function-like
//! macro this file defines) is left out.
//!
//! The same reading types receivers inside the adapter ([`CppScopeReader`]),
//! so a call through a receiver the file alone settles is emitted
//! owner-qualified (`Store::save`).
//!
//! A receiver only another file can settle (`auto s = registry.lookup()`,
//! with `Registry` declared in a header) keeps the bare call shape, and with
//! it the name candidates the linker offers. The linker's value-origin tier
//! asks a class for a method in the file that declares the class, and a C++
//! method is usually defined out of line in another file, where the tier does
//! not look; a C++ prototype in a class body is not admitted as an entity at
//! all. Handing it the receiver would turn those candidates into no edge. The
//! facts are recorded all the same, on out-of-line definitions too, so the
//! tier can follow them once it finds a member's definition outside its
//! class's file.

use std::collections::{BTreeMap, HashMap, HashSet};

use tree_sitter::{Node, Tree};

use super::{DeclaredReturn, FileFacts, ScopeFacts, ValueOrigin};

/// Standard library class templates a smart pointer names its pointee with.
const SMART_POINTERS: &[&str] = &["unique_ptr", "shared_ptr"];

/// Standard factories whose one template argument is the class they build.
const SMART_FACTORIES: &[&str] = &["make_unique", "make_shared"];

/// Casts whose template argument is the static type of the result.
const CASTS: &[&str] = &[
    "static_cast",
    "dynamic_cast",
    "reinterpret_cast",
    "const_cast",
];

/// Unqualified names a `using namespace std;` file reaches the standard
/// library through. With that directive in force such a spelling is no
/// evidence for a repository class of the same name.
const STD_NAMES: &[&str] = &[
    "string",
    "wstring",
    "string_view",
    "vector",
    "map",
    "unordered_map",
    "multimap",
    "set",
    "unordered_set",
    "multiset",
    "list",
    "forward_list",
    "deque",
    "array",
    "optional",
    "variant",
    "function",
    "pair",
    "tuple",
    "stack",
    "queue",
    "priority_queue",
    "thread",
    "mutex",
    "atomic",
    "future",
    "promise",
    "ostream",
    "istream",
    "iostream",
    "ostringstream",
    "istringstream",
    "stringstream",
    "ifstream",
    "ofstream",
    "fstream",
    "weak_ptr",
    "any",
    "bitset",
    "span",
];

/// Lowercase macros that iterate and declare their loop variable.
const LOWERCASE_LOOP_MACROS: &[&str] = &["foreach", "forever"];

pub(super) fn read_file(tree: &Tree, source: &[u8], _file_path: &str) -> FileFacts {
    let preprocessed = crate::languages::cpp_lang::preprocess_cpp_source(source);
    let source = preprocessed.as_slice();
    let root = tree.root_node();
    let file = CppFileTypes::read(root, source);
    let mut facts = FileFacts::default();
    walk(root, source, None, &file, &mut facts);
    facts
}

/// Walk the declarations `extract_cpp_node` admits, in the same shape, and
/// record each callable's facts under the node its entity spans.
fn walk(
    node: Node<'_>,
    source: &[u8],
    class: Option<&str>,
    file: &CppFileTypes,
    facts: &mut FileFacts,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "function_definition" => {
                let Some(name) = function_name(child, source) else {
                    continue;
                };
                let reader = CppScopeReader::for_function(child, source, class, &name, file);
                let scope = ScopeFacts {
                    bindings: reader.bindings.clone(),
                    returns: declared_return(child, source, file),
                    ..ScopeFacts::default()
                };
                facts.by_node.push((
                    vec![(child.start_byte(), child.end_byte())],
                    leaf(&name),
                    scope,
                ));
            }
            "declaration" => {
                let Some(declarator) = child.child_by_field_name("declarator") else {
                    continue;
                };
                if !has_function_declarator(declarator) {
                    continue;
                }
                let Some(name) = declarator_function_name(declarator, source) else {
                    continue;
                };
                let scope = ScopeFacts {
                    returns: declared_return(child, source, file),
                    ..ScopeFacts::default()
                };
                facts.by_node.push((
                    vec![(child.start_byte(), child.end_byte())],
                    leaf(&name),
                    scope,
                ));
            }
            "class_specifier" | "struct_specifier" | "union_specifier" => {
                let Some(name) = child
                    .child_by_field_name("name")
                    .map(|name| text(name, source).to_string())
                    .filter(|name| !name.is_empty())
                else {
                    continue;
                };
                if let Some(body) = child.child_by_field_name("body") {
                    let mut members = body.walk();
                    for member in body.children(&mut members) {
                        if matches!(
                            member.kind(),
                            "function_definition" | "declaration" | "template_declaration"
                        ) {
                            walk_member(member, source, &name, file, facts);
                        }
                    }
                }
            }
            "namespace_definition" => {
                if let Some(body) = child.child_by_field_name("body") {
                    walk(body, source, None, file, facts);
                }
            }
            "template_declaration" => walk(child, source, class, file, facts),
            "preproc_ifdef"
            | "preproc_if"
            | "preproc_else"
            | "preproc_elif"
            | "linkage_specification"
            | "declaration_list" => walk(child, source, class, file, facts),
            _ => {}
        }
    }
}

/// One member of a class body, with the class as context.
fn walk_member(
    member: Node<'_>,
    source: &[u8],
    class: &str,
    file: &CppFileTypes,
    facts: &mut FileFacts,
) {
    match member.kind() {
        "template_declaration" => {
            let mut cursor = member.walk();
            for child in member.children(&mut cursor) {
                if matches!(child.kind(), "function_definition" | "declaration") {
                    walk_member(child, source, class, file, facts);
                }
            }
        }
        "function_definition" => {
            let Some(name) = function_name(member, source) else {
                return;
            };
            let qualified = format!("{class}::{name}");
            let reader =
                CppScopeReader::for_function(member, source, Some(class), &qualified, file);
            let scope = ScopeFacts {
                bindings: reader.bindings.clone(),
                returns: declared_return(member, source, file),
                ..ScopeFacts::default()
            };
            facts.by_node.push((
                vec![(member.start_byte(), member.end_byte())],
                leaf(&name),
                scope,
            ));
        }
        "declaration" => {
            let Some(declarator) = member.child_by_field_name("declarator") else {
                return;
            };
            if !has_function_declarator(declarator) {
                return;
            }
            let Some(name) = declarator_function_name(declarator, source) else {
                return;
            };
            let scope = ScopeFacts {
                returns: declared_return(member, source, file),
                ..ScopeFacts::default()
            };
            facts.by_node.push((
                vec![(member.start_byte(), member.end_byte())],
                leaf(&name),
                scope,
            ));
        }
        _ => {}
    }
}

/// The last segment of an entity name, which is what the attach step matches.
fn leaf(name: &str) -> String {
    name.rsplit(['.', ':']).next().unwrap_or(name).to_string()
}

// ── What the whole file declares ──────────────────────────────────────────

/// A declared return every declaration of one name agrees on.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Returned {
    Class(String),
    /// Declarations disagree, or one returns no class.
    Mixed,
}

impl Returned {
    fn merge(slot: &mut Option<Returned>, next: Option<String>) {
        let next = match next {
            Some(class) => Returned::Class(class),
            None => Returned::Mixed,
        };
        match slot {
            None => *slot = Some(next),
            Some(existing) if *existing == next => {}
            Some(existing) => *existing = Returned::Mixed,
        }
    }

    fn class(slot: Option<&Returned>) -> Option<&str> {
        match slot? {
            Returned::Class(class) => Some(class.as_str()),
            Returned::Mixed => None,
        }
    }
}

/// A declared type, as a value-origin fact reads it and as the adapter has
/// always spelled a receiver's owner.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Typed {
    /// The class the fact names ([`CppFileTypes::class_type`]).
    class: Option<String>,
    /// The type's own leaf as written, a standard library or template
    /// parameter type included, which the adapter keeps qualifying a call
    /// with (`vector::push_back`) so a call through such a receiver never
    /// reads as a bare name any same-named method could take.
    spelled: Option<String>,
}

/// The types one C++ file declares, which is all the adapter can settle a
/// receiver with before the linker sees the rest of the repository.
#[derive(Debug, Default)]
pub(crate) struct CppFileTypes {
    /// Whether a file-scope `using namespace std;` is in force.
    using_std: bool,
    /// How many class, struct or union bodies each name has in the file.
    class_bodies: HashMap<String, usize>,
    /// The fields each class body declares, by field name. A class with two
    /// bodies in the file keeps no entry, and a field declared twice keeps no
    /// type.
    class_fields: HashMap<String, HashMap<String, Option<Typed>>>,
    /// Free functions by the name a call spells, with the class every
    /// declaration of the name agrees it returns.
    functions: HashMap<String, Option<Returned>>,
    /// Methods by (class, method), the same way.
    methods: HashMap<(String, String), Option<Returned>>,
    /// Function-like macros the file defines, which can declare names.
    macros: HashSet<String>,
}

impl CppFileTypes {
    pub(crate) fn read(root: Node<'_>, source: &[u8]) -> Self {
        let mut types = Self::default();
        let mut cursor = root.walk();
        for child in root.children(&mut cursor) {
            if child.kind() == "using_declaration" {
                let spelled: String = text(child, source).split_whitespace().collect();
                if spelled == "usingnamespacestd;" || spelled == "usingnamespace::std;" {
                    types.using_std = true;
                }
            }
        }
        let mut classes = Vec::new();
        let mut callables = Vec::new();
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            match node.kind() {
                "preproc_function_def" | "preproc_def" => {
                    if let Some(name) = node.child_by_field_name("name") {
                        types.macros.insert(text(name, source).to_string());
                    }
                }
                "class_specifier" | "struct_specifier" | "union_specifier" => classes.push(node),
                "function_definition" | "declaration" | "field_declaration" => callables.push(node),
                _ => {}
            }
            let mut cursor = node.walk();
            stack.extend(node.children(&mut cursor));
        }
        for class in classes {
            types.read_class(class, source);
        }
        for callable in callables {
            let class = member_owner(callable, source);
            types.read_declaration(callable, source, class.as_deref());
        }
        types
    }

    /// Record a class body's fields. Its methods are read with every other
    /// callable.
    fn read_class(&mut self, class: Node<'_>, source: &[u8]) {
        let Some(name) = class
            .child_by_field_name("name")
            .map(|name| text(name, source).to_string())
            .filter(|name| !name.is_empty())
        else {
            return;
        };
        let Some(body) = class.child_by_field_name("body") else {
            return;
        };
        let bodies = self.class_bodies.entry(name.clone()).or_default();
        *bodies += 1;
        if *bodies > 1 {
            self.class_fields.remove(&name);
            return;
        }
        let mut fields: HashMap<String, Option<Typed>> = HashMap::new();
        let params = template_params(class, source);
        let mut cursor = body.walk();
        for member in body.children(&mut cursor) {
            if member.kind() != "field_declaration" {
                continue;
            }
            let declarators = declarators_of(member);
            let typed = member.child_by_field_name("type").map(|ty| Typed {
                class: self.class_type(ty, source, &params),
                spelled: spelled_type(ty, source),
            });
            for declarator in declarators {
                let Some(field) = declarator_name(declarator, source) else {
                    continue;
                };
                match fields.get_mut(&field) {
                    Some(slot) => *slot = None,
                    None => {
                        fields.insert(field, typed.clone());
                    }
                }
            }
        }
        self.class_fields.insert(name, fields);
    }

    /// A function definition or prototype: record what it returns under the
    /// name a call spells.
    fn read_declaration(&mut self, node: Node<'_>, source: &[u8], class: Option<&str>) {
        let Some(declarator) = node.child_by_field_name("declarator") else {
            return;
        };
        if !has_function_declarator(declarator) {
            return;
        }
        let Some(name) = declarator_function_name(declarator, source) else {
            return;
        };
        let returned = self.return_class(node, source);
        match (class, name.rsplit_once("::")) {
            (Some(class), _) => {
                let key = (class.to_string(), name);
                Returned::merge(self.methods.entry(key).or_default(), returned);
            }
            (None, Some((owner, method))) => {
                let owner = owner.rsplit("::").next().unwrap_or(owner);
                let owner = strip_template_args(owner);
                let key = (owner, method.to_string());
                Returned::merge(self.methods.entry(key).or_default(), returned);
            }
            (None, None) => {
                Returned::merge(self.functions.entry(name).or_default(), returned);
            }
        }
    }

    fn return_class(&self, node: Node<'_>, source: &[u8]) -> Option<String> {
        match declared_return(node, source, self)? {
            DeclaredReturn::Type { type_name } => Some(type_name),
            DeclaredReturn::Receiver | DeclaredReturn::External { .. } => None,
        }
    }

    /// The class a type node names, as Kin names classes: by leaf. `None` for
    /// primitives, `auto`, template parameters, standard library types other
    /// than a smart pointer's pointee, and anything dependent.
    fn class_type(
        &self,
        node: Node<'_>,
        source: &[u8],
        template_params: &HashSet<String>,
    ) -> Option<String> {
        match node.kind() {
            "type_identifier" => {
                let name = text(node, source);
                if name.is_empty()
                    || template_params.contains(name)
                    || (self.using_std && STD_NAMES.contains(&name))
                {
                    return None;
                }
                Some(name.to_string())
            }
            "template_type" => {
                let name = node.child_by_field_name("name")?;
                let spelled = text(name, source);
                if SMART_POINTERS.contains(&spelled) && self.using_std {
                    return self.first_template_argument(node, source, template_params);
                }
                self.class_type(name, source, template_params)
            }
            "qualified_identifier" => {
                let scope = node.child_by_field_name("scope");
                let name = node.child_by_field_name("name")?;
                let std = scope.is_some_and(|scope| text(scope, source) == "std");
                if std {
                    if name.kind() == "template_type" {
                        let spelled = name
                            .child_by_field_name("name")
                            .map(|name| text(name, source))
                            .unwrap_or("");
                        if SMART_POINTERS.contains(&spelled) {
                            return self.first_template_argument(name, source, template_params);
                        }
                    }
                    return None;
                }
                if scope.is_some_and(|scope| scope.kind() == "template_type")
                    || name.kind() == "dependent_name"
                {
                    return None;
                }
                self.class_type(name, source, template_params)
            }
            "struct_specifier" | "class_specifier" | "union_specifier" => {
                // An elaborated `struct Store s`, not a definition.
                if node.child_by_field_name("body").is_some() {
                    return None;
                }
                let name = node.child_by_field_name("name")?;
                self.class_type(name, source, template_params)
            }
            _ => None,
        }
    }

    fn first_template_argument(
        &self,
        template: Node<'_>,
        source: &[u8],
        template_params: &HashSet<String>,
    ) -> Option<String> {
        let arguments = template.child_by_field_name("arguments")?;
        if arguments.named_child_count() != 1 {
            return None;
        }
        let descriptor = arguments.named_child(0)?;
        if descriptor.kind() != "type_descriptor"
            || descriptor.child_by_field_name("declarator").is_some()
        {
            return None;
        }
        self.class_type(
            descriptor.child_by_field_name("type")?,
            source,
            template_params,
        )
    }

    fn function_return(&self, name: &str) -> Option<&str> {
        Returned::class(self.functions.get(name)?.as_ref())
    }

    fn method_return(&self, class: &str, method: &str) -> Option<&str> {
        Returned::class(
            self.methods
                .get(&(class.to_string(), method.to_string()))?
                .as_ref(),
        )
    }

    fn is_file_class(&self, name: &str) -> bool {
        self.class_bodies.get(name) == Some(&1) && !self.functions.contains_key(name)
    }

    fn field_type(&self, class: &str, field: &str) -> Option<&Typed> {
        self.class_fields.get(class)?.get(field)?.as_ref()
    }
}

// ── Declared returns ──────────────────────────────────────────────────────

/// The class a function definition or prototype declares it returns: its
/// type through pointer and reference declarators, or a trailing return type.
/// `None` for a constructor, a destructor, a conversion, `auto` without a
/// trailing type, a function returning a function pointer, and any type
/// [`CppFileTypes::class_type`] does not name.
fn declared_return(node: Node<'_>, source: &[u8], file: &CppFileTypes) -> Option<DeclaredReturn> {
    let params = template_params(node, source);
    let mut declarator = node.child_by_field_name("declarator")?;
    loop {
        match declarator.kind() {
            "pointer_declarator" | "reference_declarator" | "parenthesized_declarator" => {
                declarator = declarator.child_by_field_name("declarator").or_else(|| {
                    declarator
                        .named_child(0)
                        .filter(|n| n.kind() != "type_qualifier")
                })?;
            }
            "function_declarator" => break,
            _ => return None,
        }
    }
    // A function returning a function pointer nests a second function
    // declarator inside the first.
    let inner = declarator.child_by_field_name("declarator")?;
    if has_function_declarator(inner) {
        return None;
    }
    let mut trailing = None;
    let mut cursor = declarator.walk();
    for child in declarator.children(&mut cursor) {
        if child.kind() == "trailing_return_type" {
            trailing = child
                .named_child(0)
                .filter(|descriptor| descriptor.kind() == "type_descriptor")
                .filter(|descriptor| {
                    descriptor
                        .child_by_field_name("declarator")
                        .is_none_or(|d| {
                            matches!(
                                d.kind(),
                                "abstract_pointer_declarator" | "abstract_reference_declarator"
                            )
                        })
                })
                .and_then(|descriptor| descriptor.child_by_field_name("type"));
        }
    }
    let ty = match trailing {
        Some(ty) => ty,
        None => node.child_by_field_name("type")?,
    };
    file.class_type(ty, source, &params)
        .map(|type_name| DeclaredReturn::Type { type_name })
}

// ── One callable's bindings ───────────────────────────────────────────────

/// One declaration of a name inside a callable.
#[derive(Debug, Clone)]
struct Declared {
    origin: Option<ValueOrigin>,
    /// The explicit type's own leaf, for the adapter's owner-qualified name.
    spelled: Option<String>,
    /// The bytes the declaration's scope covers.
    scope: (usize, usize),
    /// The first byte a read of the name can see the declaration from.
    visible_from: usize,
}

/// A callable's bindings, read once and shared by the adapter (to type
/// receivers) and by the value-origin facts.
#[derive(Debug, Default)]
pub(crate) struct CppScopeReader {
    /// Every name the callable reads to one origin at every read.
    pub(crate) bindings: BTreeMap<String, ValueOrigin>,
    /// Every name the callable reads to one explicitly spelled type at every
    /// read, as the adapter qualifies a call through it.
    spelled: BTreeMap<String, String>,
    /// The class `this` points at, when the callable is a member.
    owner: Option<String>,
}

impl CppScopeReader {
    /// Read the bindings of `function` (a `function_definition`), a member of
    /// `class` when it is declared in a class body, entity-named `name`.
    pub(crate) fn for_function(
        function: Node<'_>,
        source: &[u8],
        class: Option<&str>,
        name: &str,
        file: &CppFileTypes,
    ) -> Self {
        let owner = class.map(str::to_string).or_else(|| {
            name.rsplit_once("::")
                .map(|(owner, _)| strip_template_args(owner.rsplit("::").next().unwrap_or(owner)))
        });
        let mut reader = Self {
            bindings: BTreeMap::new(),
            spelled: BTreeMap::new(),
            owner,
        };
        if function.has_error() || holds_macro_shapes(function) {
            return reader;
        }
        let params = template_params(function, source);
        let mut census = Census::default();
        census.collect(
            function,
            function,
            source,
            file,
            &params,
            reader.owner.as_deref(),
        );
        let (bindings, spelled) = census.settle(file, reader.owner.as_deref());
        reader.bindings = bindings;
        reader.spelled = spelled;
        reader
    }

    /// The class a receiver name holds, when this file alone settles it.
    pub(crate) fn class_of(&self, receiver: &str, file: &CppFileTypes) -> Option<String> {
        self.bindings
            .get(receiver)
            .and_then(|origin| self.resolve(origin, file, 0))
            .or_else(|| self.spelled.get(receiver).cloned())
    }

    fn resolve(&self, origin: &ValueOrigin, file: &CppFileTypes, depth: usize) -> Option<String> {
        if depth > 8 {
            return None;
        }
        match origin {
            ValueOrigin::Declared { type_name } => Some(type_name.clone()),
            ValueOrigin::Local { name } => self.resolve(self.bindings.get(name)?, file, depth + 1),
            ValueOrigin::Call { callee } => {
                if file.is_file_class(callee) {
                    return Some(callee.clone());
                }
                if let Some((owner, method)) = callee.rsplit_once("::") {
                    let owner = owner.rsplit("::").next().unwrap_or(owner);
                    return file.method_return(owner, method).map(str::to_string);
                }
                file.function_return(callee).map(str::to_string)
            }
            ValueOrigin::MethodCall { receiver, method } => {
                let class = self.resolve(receiver, file, depth + 1)?;
                file.method_return(&class, method).map(str::to_string)
            }
            ValueOrigin::Entered { .. }
            | ValueOrigin::Provided { .. }
            | ValueOrigin::External { .. }
            | ValueOrigin::Supplied { .. }
            | ValueOrigin::ImportedProvider { .. }
            | ValueOrigin::ImportedCall { .. }
            | ValueOrigin::Global { .. }
            | ValueOrigin::Module { .. }
            | ValueOrigin::ModuleMember { .. }
            | ValueOrigin::Enclosing { .. } => None,
        }
    }
}

#[derive(Debug, Default)]
struct Census {
    declared: HashMap<String, Vec<Declared>>,
    /// Every read of a name: its byte offset.
    reads: HashMap<String, Vec<usize>>,
    /// Names passed bare to a macro-like call, which may declare them.
    macro_arguments: HashSet<String>,
}

impl Census {
    fn declare(&mut self, name: &str, declared: Declared) {
        self.declared
            .entry(name.to_string())
            .or_default()
            .push(declared);
    }

    #[allow(clippy::too_many_arguments)]
    fn collect(
        &mut self,
        function: Node<'_>,
        node: Node<'_>,
        source: &[u8],
        file: &CppFileTypes,
        params: &HashSet<String>,
        owner: Option<&str>,
    ) {
        let mut stack = vec![node];
        let mut declarator_names: HashSet<(usize, usize)> = HashSet::new();
        if let Some((_, at)) = function
            .child_by_field_name("declarator")
            .and_then(|declarator| declared_identifier(declarator, source))
        {
            declarator_names.insert(at);
        }
        while let Some(node) = stack.pop() {
            match node.kind() {
                "parameter_declaration" | "optional_parameter_declaration" => {
                    if let Some(declarator) = node.child_by_field_name("declarator") {
                        if let Some((name, at)) = declared_identifier(declarator, source) {
                            declarator_names.insert(at);
                            let scope = parameter_scope(node);
                            let function_typed = declarator_is_function(declarator);
                            let ty = node.child_by_field_name("type").filter(|_| !function_typed);
                            let origin = ty
                                .and_then(|ty| file.class_type(ty, source, params))
                                .map(|type_name| ValueOrigin::Declared { type_name });
                            self.declare(
                                &name,
                                Declared {
                                    origin,
                                    spelled: ty.and_then(|ty| spelled_type(ty, source)),
                                    scope: (scope.start_byte(), scope.end_byte()),
                                    visible_from: scope.start_byte(),
                                },
                            );
                        }
                    }
                }
                "variadic_parameter_declaration" => {
                    if let Some(declarator) = node.child_by_field_name("declarator") {
                        if let Some((name, at)) = declared_identifier(declarator, source) {
                            declarator_names.insert(at);
                            let scope = parameter_scope(node);
                            self.declare(
                                &name,
                                Declared {
                                    origin: None,
                                    spelled: None,
                                    scope: (scope.start_byte(), scope.end_byte()),
                                    visible_from: scope.start_byte(),
                                },
                            );
                        }
                    }
                }
                "declaration" | "field_declaration" => {
                    let scope = declaration_scope(node);
                    let explicit_node = node
                        .child_by_field_name("type")
                        .filter(|ty| ty.kind() != "placeholder_type_specifier");
                    let explicit = explicit_node.map(|ty| file.class_type(ty, source, params));
                    let spelled = explicit_node.and_then(|ty| spelled_type(ty, source));
                    for declarator in declarators_of(node) {
                        for (name, at) in declared_identifiers(declarator, source) {
                            declarator_names.insert(at);
                            let plain = !declarator_is_function(declarator)
                                && declarator.kind() != "structured_binding_declarator"
                                && !is_structured(declarator);
                            let origin = if !plain {
                                None
                            } else {
                                match &explicit {
                                    Some(class) => class
                                        .clone()
                                        .map(|type_name| ValueOrigin::Declared { type_name }),
                                    None => init_value(declarator).and_then(|value| {
                                        expression_origin(value, source, file, params, owner)
                                    }),
                                }
                            };
                            let visible_from = if node.kind() == "field_declaration" {
                                scope.0
                            } else {
                                at.1
                            };
                            self.declare(
                                &name,
                                Declared {
                                    origin,
                                    spelled: spelled.clone().filter(|_| plain),
                                    scope,
                                    visible_from,
                                },
                            );
                        }
                    }
                }
                "for_range_loop" => {
                    if let Some(declarator) = node.child_by_field_name("declarator") {
                        let explicit_node = node
                            .child_by_field_name("type")
                            .filter(|ty| ty.kind() != "placeholder_type_specifier");
                        let plain = !is_structured(declarator);
                        let explicit = explicit_node
                            .and_then(|ty| file.class_type(ty, source, params))
                            .filter(|_| plain);
                        let spelled = explicit_node
                            .and_then(|ty| spelled_type(ty, source))
                            .filter(|_| plain);
                        for (name, at) in declared_identifiers(declarator, source) {
                            declarator_names.insert(at);
                            self.declare(
                                &name,
                                Declared {
                                    origin: explicit
                                        .clone()
                                        .map(|type_name| ValueOrigin::Declared { type_name }),
                                    spelled: spelled.clone(),
                                    scope: (node.start_byte(), node.end_byte()),
                                    visible_from: at.1,
                                },
                            );
                        }
                    }
                }
                "lambda_capture_initializer" => {
                    if let Some(left) = node.child_by_field_name("left") {
                        if left.kind() == "identifier" {
                            declarator_names.insert((left.start_byte(), left.end_byte()));
                            let lambda =
                                ancestor_of_kind(node, "lambda_expression").unwrap_or(node);
                            self.declare(
                                text(left, source),
                                Declared {
                                    origin: None,
                                    spelled: None,
                                    scope: (lambda.start_byte(), lambda.end_byte()),
                                    visible_from: left.end_byte(),
                                },
                            );
                        }
                    }
                }
                "call_expression" => {
                    if let Some(callee) = node.child_by_field_name("function") {
                        let spelled = text(callee, source);
                        let macro_like = callee.kind() == "identifier"
                            && (is_all_caps(spelled)
                                || file.macros.contains(spelled)
                                || LOWERCASE_LOOP_MACROS.contains(&spelled));
                        if macro_like {
                            if let Some(arguments) = node.child_by_field_name("arguments") {
                                let mut cursor = arguments.walk();
                                for argument in arguments.named_children(&mut cursor) {
                                    if argument.kind() == "identifier" {
                                        self.macro_arguments
                                            .insert(text(argument, source).to_string());
                                    }
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
            let mut cursor = node.walk();
            stack.extend(node.children(&mut cursor));
        }
        // Reads: every identifier that is not a declarator's name and not the
        // name part of a qualified identifier.
        let mut stack = vec![function];
        while let Some(node) = stack.pop() {
            if node.kind() == "identifier"
                && !declarator_names.contains(&(node.start_byte(), node.end_byte()))
                && !node.parent().is_some_and(|parent| {
                    parent.kind() == "qualified_identifier" && is_field(parent, "name", node)
                })
            {
                self.reads
                    .entry(text(node, source).to_string())
                    .or_default()
                    .push(node.start_byte());
            }
            let mut cursor = node.walk();
            stack.extend(node.children(&mut cursor));
        }
    }

    /// The bindings every read agrees on, and the explicitly spelled types.
    fn settle(
        self,
        file: &CppFileTypes,
        owner: Option<&str>,
    ) -> (BTreeMap<String, ValueOrigin>, BTreeMap<String, String>) {
        let mut bindings = BTreeMap::new();
        let mut spelled = BTreeMap::new();
        for (name, declarations) in &self.declared {
            if self.macro_arguments.contains(name) {
                continue;
            }
            let covered = self.reads.get(name).is_none_or(|reads| {
                reads.iter().all(|&at| {
                    declarations
                        .iter()
                        .any(|declared| declared.visible_from <= at && at < declared.scope.1)
                })
            });
            if !covered {
                continue;
            }
            if let Some(first) = agreed(declarations.iter().map(|d| d.origin.clone())) {
                bindings.insert(name.clone(), first);
            }
            if let Some(first) = agreed(declarations.iter().map(|d| d.spelled.clone())) {
                spelled.insert(name.clone(), first);
            }
        }
        // A name the callable reads and never declares is the enclosing
        // class's field, when the class body in this file declares it.
        if let Some(owner) = owner {
            for name in self.reads.keys() {
                if self.declared.contains_key(name) || self.macro_arguments.contains(name) {
                    continue;
                }
                let Some(typed) = file.field_type(owner, name) else {
                    continue;
                };
                if let Some(type_name) = &typed.class {
                    bindings.insert(
                        name.clone(),
                        ValueOrigin::Declared {
                            type_name: type_name.clone(),
                        },
                    );
                }
                if let Some(written) = &typed.spelled {
                    spelled.insert(name.clone(), written.clone());
                }
            }
        }
        (bindings, spelled)
    }
}

/// The one value every item holds, when every item holds the same one.
fn agreed<T: PartialEq>(mut items: impl Iterator<Item = Option<T>>) -> Option<T> {
    let first = items.next()??;
    for item in items {
        if item.as_ref() != Some(&first) {
            return None;
        }
    }
    Some(first)
}

/// What an `auto` declarator's initializer holds, when it is a shape the
/// linker can follow.
fn expression_origin(
    value: Node<'_>,
    source: &[u8],
    file: &CppFileTypes,
    params: &HashSet<String>,
    owner: Option<&str>,
) -> Option<ValueOrigin> {
    match value.kind() {
        "parenthesized_expression" => {
            expression_origin(value.named_child(0)?, source, file, params, owner)
        }
        "identifier" => Some(ValueOrigin::Local {
            name: text(value, source).to_string(),
        }),
        "new_expression" => {
            if value
                .child_by_field_name("declarator")
                .is_some_and(|d| d.kind() == "new_declarator")
            {
                // `new Store[n]` is an array.
                return None;
            }
            let type_name = file.class_type(value.child_by_field_name("type")?, source, params)?;
            Some(ValueOrigin::Declared { type_name })
        }
        "compound_literal_expression" => {
            let type_name = file.class_type(value.child_by_field_name("type")?, source, params)?;
            Some(ValueOrigin::Declared { type_name })
        }
        "call_expression" => {
            let function = value.child_by_field_name("function")?;
            match function.kind() {
                "identifier" => {
                    let callee = text(function, source);
                    if params.contains(callee) {
                        return None;
                    }
                    Some(ValueOrigin::Call {
                        callee: callee.to_string(),
                    })
                }
                "template_function" | "qualified_identifier" => {
                    let (scope, template) = match function.kind() {
                        "qualified_identifier" => (
                            function
                                .child_by_field_name("scope")
                                .map(|s| text(s, source)),
                            function.child_by_field_name("name")?,
                        ),
                        _ => (None, function),
                    };
                    if template.kind() == "template_function" {
                        let name = text(template.child_by_field_name("name")?, source);
                        let standard = scope == Some("std") || (scope.is_none() && file.using_std);
                        if (standard && SMART_FACTORIES.contains(&name))
                            || (scope.is_none() && CASTS.contains(&name))
                        {
                            let type_name =
                                file.first_template_argument_of(template, source, params)?;
                            return Some(ValueOrigin::Declared { type_name });
                        }
                        return None;
                    }
                    if scope == Some("std") || function.kind() != "qualified_identifier" {
                        return None;
                    }
                    if function.has_error() {
                        return None;
                    }
                    let spelled: String = text(function, source).split_whitespace().collect();
                    Some(ValueOrigin::Call { callee: spelled })
                }
                "field_expression" => {
                    let method = function.child_by_field_name("field")?;
                    if method.kind() != "field_identifier" {
                        return None;
                    }
                    let argument = function.child_by_field_name("argument")?;
                    let receiver = match argument.kind() {
                        "this" => ValueOrigin::Declared {
                            type_name: owner?.to_string(),
                        },
                        "identifier" => ValueOrigin::Local {
                            name: text(argument, source).to_string(),
                        },
                        _ => expression_origin(argument, source, file, params, owner)?,
                    };
                    Some(ValueOrigin::MethodCall {
                        receiver: Box::new(receiver),
                        method: text(method, source).to_string(),
                    })
                }
                _ => None,
            }
        }
        _ => None,
    }
}

impl CppFileTypes {
    /// The class a template's one pointer, reference or plain type argument
    /// names (`make_unique<Store>`, `static_cast<Store*>`).
    fn first_template_argument_of(
        &self,
        template: Node<'_>,
        source: &[u8],
        params: &HashSet<String>,
    ) -> Option<String> {
        let arguments = template.child_by_field_name("arguments")?;
        if arguments.named_child_count() != 1 {
            return None;
        }
        let descriptor = arguments.named_child(0)?;
        if descriptor.kind() != "type_descriptor" {
            return None;
        }
        if descriptor
            .child_by_field_name("declarator")
            .is_some_and(|d| {
                !matches!(
                    d.kind(),
                    "abstract_pointer_declarator" | "abstract_reference_declarator"
                )
            })
        {
            return None;
        }
        self.class_type(descriptor.child_by_field_name("type")?, source, params)
    }
}

/// The class whose body declares a member callable directly (or through a
/// template declaration), by name.
fn member_owner(node: Node<'_>, source: &[u8]) -> Option<String> {
    let mut parent = node.parent()?;
    if parent.kind() == "template_declaration" {
        parent = parent.parent()?;
    }
    if parent.kind() != "field_declaration_list" {
        return None;
    }
    let class = parent.parent()?;
    if !matches!(
        class.kind(),
        "class_specifier" | "struct_specifier" | "union_specifier"
    ) {
        return None;
    }
    class
        .child_by_field_name("name")
        .map(|name| text(name, source).to_string())
        .filter(|name| !name.is_empty())
}

// ── Syntax helpers ────────────────────────────────────────────────────────

/// Whether a callable body holds a construct only a macro produces, or one
/// that defines a macro, either of which can declare names the grammar
/// cannot see.
fn holds_macro_shapes(function: Node<'_>) -> bool {
    let Some(body) = function.child_by_field_name("body") else {
        return false;
    };
    let mut stack = vec![body];
    while let Some(node) = stack.pop() {
        match node.kind() {
            "preproc_def" | "preproc_function_def" => return true,
            "function_definition" => {
                // A member function of a local class is real; one directly in
                // a block is how the grammar reads a macro-driven block.
                if node
                    .parent()
                    .is_none_or(|parent| parent.kind() != "field_declaration_list")
                {
                    return true;
                }
            }
            _ => {}
        }
        let mut cursor = node.walk();
        stack.extend(node.children(&mut cursor));
    }
    false
}

/// The node a parameter's name is visible in: the function, lambda or catch
/// clause it belongs to, or the prototype's own declarator.
fn parameter_scope(parameter: Node<'_>) -> Node<'_> {
    let Some(list) = parameter.parent() else {
        return parameter;
    };
    let Some(holder) = list.parent() else {
        return list;
    };
    match holder.kind() {
        "catch_clause" => holder,
        "abstract_function_declarator" => holder
            .parent()
            .filter(|parent| parent.kind() == "lambda_expression")
            .unwrap_or(holder),
        "function_declarator" => {
            let mut current = holder;
            while let Some(parent) = current.parent() {
                match parent.kind() {
                    "pointer_declarator" | "reference_declarator" | "parenthesized_declarator" => {
                        current = parent;
                    }
                    "function_definition" => return parent,
                    _ => break,
                }
            }
            holder
        }
        _ => holder,
    }
}

/// The bytes a local declaration's names are visible in.
fn declaration_scope(declaration: Node<'_>) -> (usize, usize) {
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

fn ancestor_of_kind<'tree>(node: Node<'tree>, kind: &str) -> Option<Node<'tree>> {
    let mut current = node.parent();
    while let Some(parent) = current {
        if parent.kind() == kind {
            return Some(parent);
        }
        current = parent.parent();
    }
    None
}

/// Every `declarator` field of a declaration.
fn declarators_of(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.children_by_field_name("declarator", &mut cursor)
        .collect()
}

/// The initializer an `init_declarator` holds.
fn init_value(declarator: Node<'_>) -> Option<Node<'_>> {
    (declarator.kind() == "init_declarator").then(|| declarator.child_by_field_name("value"))?
}

/// The one identifier a declarator declares, with its byte range.
fn declared_identifier(node: Node<'_>, source: &[u8]) -> Option<(String, (usize, usize))> {
    let mut current = node;
    for _ in 0..16 {
        match current.kind() {
            "identifier" | "field_identifier" => {
                return Some((
                    text(current, source).to_string(),
                    (current.start_byte(), current.end_byte()),
                ))
            }
            "pointer_declarator"
            | "array_declarator"
            | "init_declarator"
            | "function_declarator"
            | "attributed_declarator" => {
                current = current.child_by_field_name("declarator")?;
            }
            "reference_declarator" | "parenthesized_declarator" => {
                current = current.named_child(0)?;
            }
            _ => return None,
        }
    }
    None
}

/// Every identifier a declarator declares: one, or each of a structured
/// binding's.
fn declared_identifiers(node: Node<'_>, source: &[u8]) -> Vec<(String, (usize, usize))> {
    let target = if node.kind() == "init_declarator" {
        node.child_by_field_name("declarator").unwrap_or(node)
    } else {
        node
    };
    let target = if target.kind() == "reference_declarator" {
        target.named_child(0).unwrap_or(target)
    } else {
        target
    };
    if target.kind() == "structured_binding_declarator" {
        let mut cursor = target.walk();
        return target
            .named_children(&mut cursor)
            .filter(|child| child.kind() == "identifier")
            .map(|child| {
                (
                    text(child, source).to_string(),
                    (child.start_byte(), child.end_byte()),
                )
            })
            .collect();
    }
    declared_identifier(node, source).into_iter().collect()
}

/// The declared name of a variable declarator, for field tables.
fn declarator_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    if declarator_is_function(node) {
        return None;
    }
    declared_identifier(node, source).map(|(name, _)| name)
}

fn declarator_is_function(node: Node<'_>) -> bool {
    match node.kind() {
        "function_declarator" => true,
        "pointer_declarator"
        | "reference_declarator"
        | "init_declarator"
        | "array_declarator"
        | "parenthesized_declarator"
        | "attributed_declarator" => node
            .child_by_field_name("declarator")
            .or_else(|| node.named_child(0))
            .is_some_and(declarator_is_function),
        _ => false,
    }
}

fn has_function_declarator(node: Node<'_>) -> bool {
    if node.kind() == "function_declarator" {
        return true;
    }
    let mut cursor = node.walk();
    let found = node
        .children(&mut cursor)
        .any(|child| child.kind() == "function_declarator" || has_function_declarator(child));
    found
}

/// The name a function definition's entity carries, exactly as the adapter
/// spells it.
fn function_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    let declarator = node.child_by_field_name("declarator")?;
    declarator_function_name(declarator, source)
}

fn declarator_function_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    let mut current = node;
    for _ in 0..16 {
        match current.kind() {
            "identifier" | "field_identifier" | "destructor_name" | "qualified_identifier" => {
                let spelled = text(current, source);
                return (!spelled.is_empty()).then(|| spelled.to_string());
            }
            "function_declarator" | "pointer_declarator" | "reference_declarator" => {
                current = current
                    .child_by_field_name("declarator")
                    .or_else(|| current.named_child(0))?;
            }
            _ => {
                current = current.child_by_field_name("declarator")?;
            }
        }
    }
    None
}

/// Every type parameter an enclosing template declares.
fn template_params(node: Node<'_>, source: &[u8]) -> HashSet<String> {
    let mut params = HashSet::new();
    let mut current = Some(node);
    while let Some(ancestor) = current {
        if ancestor.kind() == "template_declaration" {
            if let Some(list) = ancestor.child_by_field_name("parameters") {
                let mut cursor = list.walk();
                for param in list.named_children(&mut cursor) {
                    let mut inner = param.walk();
                    for part in param.named_children(&mut inner) {
                        if part.kind() == "type_identifier" {
                            params.insert(text(part, source).to_string());
                        }
                    }
                    if let Some(name) = param.child_by_field_name("name") {
                        params.insert(text(name, source).to_string());
                    }
                }
            }
        }
        current = ancestor.parent();
    }
    params
}

/// A type's own leaf as the adapter has always spelled a receiver's owner:
/// the identifier of a plain, template or qualified type. `None` for
/// primitives, `auto` and an elaborated `struct` name.
fn spelled_type(node: Node<'_>, source: &[u8]) -> Option<String> {
    match node.kind() {
        "type_identifier" | "identifier" => {
            let spelled = text(node, source);
            (!spelled.is_empty()).then(|| spelled.to_string())
        }
        "template_type" | "qualified_identifier" => {
            spelled_type(node.child_by_field_name("name")?, source)
        }
        _ => None,
    }
}

/// Whether a declarator binds a structured binding's names.
fn is_structured(declarator: Node<'_>) -> bool {
    let target = if declarator.kind() == "init_declarator" {
        declarator.child_by_field_name("declarator")
    } else {
        Some(declarator)
    };
    let target = target.map(|target| {
        if target.kind() == "reference_declarator" {
            target.named_child(0).unwrap_or(target)
        } else {
            target
        }
    });
    target.is_some_and(|target| target.kind() == "structured_binding_declarator")
}

fn strip_template_args(raw: &str) -> String {
    raw.split('<').next().unwrap_or(raw).trim().to_string()
}

fn is_all_caps(name: &str) -> bool {
    name.chars().any(|c| c.is_ascii_uppercase())
        && name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

fn is_field(parent: Node<'_>, field: &str, child: Node<'_>) -> bool {
    parent
        .child_by_field_name(field)
        .is_some_and(|held| held.id() == child.id())
}

fn text<'a>(node: Node<'_>, source: &'a [u8]) -> &'a str {
    node.utf8_text(source).unwrap_or("")
}

#[cfg(test)]
mod tests;
