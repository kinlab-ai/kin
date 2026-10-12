// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Java's facts for the value-escape record (see [`crate::value_escape`]).
//!
//! A Java method is a value only through a method reference, and a class's
//! constructor only through a constructor reference: reading a field, naming
//! a class or writing a lambda never hands a method on without a call that
//! spells it, and a lambda's calls are call sites of the method around it. So
//! the record is short and exact.
//!
//! Recorded:
//!
//! - method references (`Store::load`, `store::load`, `this::load`,
//!   `super::load`) as member occurrences of the method, and constructor
//!   references (`Store::new`) as occurrences of the class, with where the
//!   value goes;
//! - reflective method lookups: `getMethod` and `getDeclaredMethod`, and the
//!   `MethodHandles.Lookup` finders `findVirtual`, `findStatic` and
//!   `findSpecial`. A literal name is a member occurrence of that name, a
//!   computed one a reflective site that reaches members, with the literals
//!   the name provably holds. A lookup on a class literal of a class the file
//!   declares (`Store.class.getMethod(k)`) reads that class's methods and its
//!   bases', and the record lists every class the file declares with the
//!   supertypes it names so the census can tell which. `getMethods()` and
//!   `getDeclaredMethods()` enumerate members, and `Class.forName` by a
//!   computed name loads a class, which reaches nothing by itself;
//! - protocol members: every method that overrides, or may override, a
//!   supertype's method. A call typed by the supertype binds to the
//!   supertype's declaration and reaches this one by virtual dispatch without
//!   spelling this class, which the census reports as implicit dispatch. A
//!   method annotated `@Override`, one of `Object`'s overridable methods, and
//!   any instance method of a type that names a supertype qualify, since Java
//!   does not require the annotation.
//!
//! A file's declarations are reachable from another file only when their
//! visibility allows, so the module scope is closed and nothing is exported
//! beside the declarations themselves.

use std::collections::HashMap;

use kin_model::{
    ClassBases, FileEscapeRecord, LookupReceiver, ModuleScope, OccurrenceBinding,
    OccurrenceContext, ProtocolMember, ReceiverKind, ReflectiveForm, ReflectiveReach,
    ReflectiveSite, ValueOccurrence, EXCERPT_MAX, RECEIVER_TEXT_MAX,
};
use tree_sitter::{Node, Tree};

use crate::languages::java::{java_declares_supertype, java_erased_type_name};
use crate::value_escape::{
    collapsed_text, is_field, line_of, text_of, unparsed_lines, FileEscapeCensus, KeyLiterals,
    ProtocolStamp,
};

/// The words the census reports for a method reached by virtual dispatch.
pub(crate) const VIRTUAL_DISPATCH: &str = "virtual dispatch from a supertype";

/// `Object`'s methods a class can override, which every class inherits.
const OBJECT_METHODS: &[&str] = &["toString", "equals", "hashCode", "clone", "finalize"];

/// Reflective lookups of one method by its name, with the argument that holds
/// the name.
const METHOD_LOOKUPS: &[(&str, usize)] = &[
    ("getMethod", 0),
    ("getDeclaredMethod", 0),
    ("findVirtual", 1),
    ("findStatic", 1),
    ("findSpecial", 1),
];

/// Take the census of one Java file.
pub(crate) fn census(tree: &Tree, source: &[u8]) -> FileEscapeCensus {
    let root = tree.root_node();
    let mut walker = Walker {
        source,
        record: FileEscapeRecord::new(ModuleScope::Closed),
        protocol_members: Vec::new(),
        types: FileTypes::read(root, source),
    };
    walker.record.unparsed = unparsed_lines(tree, source);
    walker.record.classes = walker.types.classes.clone();
    walker.visit(root);
    FileEscapeCensus {
        record: walker.record,
        protocol_members: walker.protocol_members,
        non_enumerable: Vec::new(),
    }
}

/// The type names a file binds: the types it declares, with the lines they
/// are declared on, and the ones a single-type import names.
struct FileTypes {
    /// Types declared at file or member scope.
    declared: HashMap<String, Vec<u32>>,
    /// Types declared inside a method body.
    local: HashMap<String, Vec<u32>>,
    /// `import a.b.Store;` binds `Store` to module `a.b`, with the statement's
    /// byte range.
    imported: HashMap<String, (String, (u32, u32))>,
    /// Every type declared at file or member scope, with the supertypes it
    /// names, erased and by their simple names.
    classes: Vec<ClassBases>,
}

impl FileTypes {
    fn read(root: Node<'_>, source: &[u8]) -> Self {
        let mut types = FileTypes {
            declared: HashMap::new(),
            local: HashMap::new(),
            imported: HashMap::new(),
            classes: Vec::new(),
        };
        let mut stack = vec![(root, false)];
        while let Some((node, in_callable)) = stack.pop() {
            match node.kind() {
                "class_declaration"
                | "record_declaration"
                | "interface_declaration"
                | "annotation_type_declaration"
                | "enum_declaration" => {
                    if let Some(name) = node.child_by_field_name("name") {
                        let name = text_of(name, source).to_string();
                        if !in_callable {
                            types.classes.push(ClassBases {
                                name: name.clone(),
                                line: line_of(node),
                                bases: java_supertype_leaves(node, source),
                                open: false,
                                typing_only: false,
                                structural: false,
                                dynamic: false,
                            });
                        }
                        let table = if in_callable {
                            &mut types.local
                        } else {
                            &mut types.declared
                        };
                        table.entry(name).or_default().push(line_of(node));
                    }
                }
                "import_declaration" => {
                    let written = text_of(node, source);
                    let is_static = {
                        let mut cursor = node.walk();
                        let found = node
                            .children(&mut cursor)
                            .any(|child| child.kind() == "static");
                        found
                    };
                    if !is_static && !written.trim_end_matches(';').trim_end().ends_with('*') {
                        let mut cursor = node.walk();
                        let path = node
                            .named_children(&mut cursor)
                            .find(|child| child.kind() == "scoped_identifier");
                        if let Some(path) = path {
                            let full: String = text_of(path, source).split_whitespace().collect();
                            if let Some((module, name)) = full.rsplit_once('.') {
                                types.imported.insert(
                                    name.to_string(),
                                    (
                                        module.to_string(),
                                        (node.start_byte() as u32, node.end_byte() as u32),
                                    ),
                                );
                            }
                        }
                    }
                    continue;
                }
                _ => {}
            }
            let inside = in_callable || opens_callable(node);
            let mut cursor = node.walk();
            stack.extend(
                node.named_children(&mut cursor)
                    .map(|child| (child, inside)),
            );
        }
        types
    }

    /// How a type name written in the file binds, with the declaration line
    /// or import it binds to.
    fn occurrence(&self, name: &str, line: u32, context: OccurrenceContext) -> ValueOccurrence {
        let mut occurrence = ValueOccurrence {
            line,
            context,
            binding: OccurrenceBinding::Free,
            receiver: None,
            receiver_kind: None,
            aliases: Vec::new(),
            module: None,
            import: None,
            imported: None,
            target_line: None,
            form: None,
        };
        if let Some(lines) = self.local.get(name) {
            occurrence.binding = OccurrenceBinding::Local;
            if let [one] = lines.as_slice() {
                occurrence.target_line = Some(*one);
            }
        } else if let Some(lines) = self.declared.get(name) {
            occurrence.binding = OccurrenceBinding::Module;
            if let [one] = lines.as_slice() {
                occurrence.target_line = Some(*one);
            }
        } else if let Some((module, range)) = self.imported.get(name) {
            occurrence.binding = OccurrenceBinding::Import;
            occurrence.module = Some(module.clone());
            occurrence.import = Some(*range);
        }
        occurrence
    }

    /// The one class the file declares at file or member scope under `name`,
    /// as a lookup receiver.
    fn declared_receiver(&self, name: &str) -> Option<LookupReceiver> {
        if self.local.contains_key(name) {
            return None;
        }
        match self.declared.get(name)?.as_slice() {
            [line] => Some(LookupReceiver::Declared {
                name: name.to_string(),
                line: *line,
            }),
            _ => None,
        }
    }
}

/// The simple names of the supertypes a type declaration names: its
/// superclass, its interfaces, and for an interface the ones it extends.
fn java_supertype_leaves(node: Node<'_>, source: &[u8]) -> Vec<String> {
    fn collect(clause: Node<'_>, source: &[u8], leaves: &mut Vec<String>) {
        let mut cursor = clause.walk();
        for part in clause.named_children(&mut cursor) {
            if part.kind() == "type_list" {
                collect(part, source, leaves);
            } else if let Some(name) = java_erased_type_name(part, source) {
                let leaf = name.rsplit('.').next().unwrap_or(&name).to_string();
                if !leaves.contains(&leaf) {
                    leaves.push(leaf);
                }
            }
        }
    }
    let mut leaves = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if matches!(
            child.kind(),
            "superclass" | "super_interfaces" | "extends_interfaces"
        ) {
            collect(child, source, &mut leaves);
        }
    }
    leaves
}

/// A node whose body runs as a callable, so a type declared in it is local.
fn opens_callable(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "method_declaration"
            | "constructor_declaration"
            | "compact_constructor_declaration"
            | "lambda_expression"
            | "static_initializer"
    ) || (node.kind() == "block"
        && node
            .parent()
            .is_some_and(|parent| matches!(parent.kind(), "class_body" | "enum_body_declarations")))
}

struct Walker<'s> {
    source: &'s [u8],
    record: FileEscapeRecord,
    protocol_members: Vec<ProtocolStamp>,
    types: FileTypes,
}

impl Walker<'_> {
    fn visit(&mut self, node: Node<'_>) {
        match node.kind() {
            "method_reference" => self.method_reference(node),
            "method_invocation" => self.invocation(node),
            "method_declaration" => self.mark_protocol_member(node),
            _ => {}
        }
        let mut cursor = node.walk();
        let children: Vec<Node<'_>> = node.named_children(&mut cursor).collect();
        for child in children {
            self.visit(child);
        }
    }

    /// `Store::load`, `this::load`, `Store::new`.
    fn method_reference(&mut self, node: Node<'_>) {
        let Some(receiver) = node.named_child(0) else {
            return;
        };
        let context = flow_of(node);
        let constructs = {
            let mut cursor = node.walk();
            let found = node
                .children(&mut cursor)
                .any(|child| child.kind() == "new");
            found
        };
        if constructs {
            // `int[]::new` makes an array; a generic type's constructor is
            // its erased class's.
            let Some(class) = java_erased_type_name(receiver, self.source).or_else(|| {
                (receiver.kind() == "identifier" || receiver.kind() == "scoped_identifier")
                    .then(|| text_of(receiver, self.source).split_whitespace().collect())
            }) else {
                return;
            };
            let leaf = class.rsplit('.').next().unwrap_or(&class).to_string();
            let root = class.split('.').next().unwrap_or(&class);
            let mut occurrence = self.types.occurrence(root, line_of(node), context);
            if root != leaf {
                // `Outer.Inner::new` reaches `Inner` through `Outer`.
                occurrence.binding = OccurrenceBinding::Member;
                occurrence.receiver = Some(class[..class.len() - leaf.len() - 1].to_string());
                occurrence.receiver_kind = Some(ReceiverKind::Other);
                occurrence.module = None;
                occurrence.import = None;
                occurrence.target_line = None;
            }
            self.push_value(leaf, occurrence);
            return;
        }
        let mut cursor = node.walk();
        let Some(name) = node
            .named_children(&mut cursor)
            .skip(1)
            .find(|child| child.kind() == "identifier")
        else {
            return;
        };
        let receiver_kind = match receiver.kind() {
            "this" | "super" => ReceiverKind::Instance,
            _ => ReceiverKind::Other,
        };
        self.push_value(
            text_of(name, self.source).to_string(),
            ValueOccurrence {
                line: line_of(name),
                context,
                binding: OccurrenceBinding::Member,
                receiver: Some(collapsed_text(receiver, self.source, RECEIVER_TEXT_MAX)),
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

    /// Reflective method lookups and computed class loads.
    fn invocation(&mut self, node: Node<'_>) {
        let Some(method) = node.child_by_field_name("name") else {
            return;
        };
        let method = text_of(method, self.source);
        let arguments: Vec<Node<'_>> = node
            .child_by_field_name("arguments")
            .map(|arguments| {
                let mut cursor = arguments.walk();
                let nodes: Vec<Node<'_>> = arguments.named_children(&mut cursor).collect();
                nodes
            })
            .unwrap_or_default();
        let object = node.child_by_field_name("object");
        if let Some(&(_, at)) = METHOD_LOOKUPS.iter().find(|(name, _)| *name == method) {
            let Some(key) = arguments.get(at) else {
                return;
            };
            match key_of(*key, self.source) {
                Key::Literal(name) => {
                    // A lookup whose result is dropped hands nothing on.
                    let Some(context) = lookup_flow(node) else {
                        return;
                    };
                    self.push_value(
                        name,
                        ValueOccurrence {
                            line: line_of(*key),
                            context,
                            binding: OccurrenceBinding::Member,
                            receiver: object.map(|object| {
                                collapsed_text(object, self.source, RECEIVER_TEXT_MAX)
                            }),
                            receiver_kind: Some(ReceiverKind::Other),
                            aliases: Vec::new(),
                            module: None,
                            import: None,
                            imported: None,
                            target_line: None,
                            form: None,
                        },
                    );
                }
                Key::Computed(key) => {
                    // `findVirtual(Store.class, name, type)` names its class
                    // first; `Store.class.getMethod(k)` is called on it.
                    let class = if at == 0 {
                        object
                    } else {
                        arguments.first().copied()
                    };
                    let receiver = class
                        .filter(|class| class.kind() == "class_literal")
                        .and_then(|class| class.named_child(0))
                        .and_then(|ty| java_erased_type_name(ty, self.source))
                        .filter(|name| !name.contains('.'))
                        .and_then(|name| self.types.declared_receiver(&name));
                    let reach = if receiver.is_some() {
                        ReflectiveReach::Any
                    } else {
                        ReflectiveReach::Members
                    };
                    self.push_reflective(
                        node,
                        ReflectiveForm::ComputedMember,
                        reach,
                        receiver,
                        key,
                    );
                }
            }
            return;
        }
        match method {
            "getMethods" | "getDeclaredMethods" if arguments.is_empty() => {
                self.push_reflective(
                    node,
                    ReflectiveForm::Enumerate,
                    ReflectiveReach::Members,
                    None,
                    KeyLiterals::default(),
                );
            }
            "forName"
                if object.is_some_and(|object| {
                    matches!(text_of(object, self.source), "Class" | "java.lang.Class")
                }) =>
            {
                if let Some(key) = arguments.first() {
                    if matches!(key_of(*key, self.source), Key::Computed(_)) {
                        self.push_reflective(
                            node,
                            ReflectiveForm::DynamicImport,
                            ReflectiveReach::Nothing,
                            None,
                            KeyLiterals::default(),
                        );
                    }
                }
            }
            _ => {}
        }
    }

    fn mark_protocol_member(&mut self, method: Node<'_>) {
        let Some(name) = method.child_by_field_name("name") else {
            return;
        };
        if !may_override(method, self.source) {
            return;
        }
        self.protocol_members.push(ProtocolStamp {
            name: text_of(name, self.source).to_string(),
            at: name.start_byte(),
            member: ProtocolMember::Implicit(VIRTUAL_DISPATCH.into()),
        });
    }

    fn push_value(&mut self, name: String, occurrence: ValueOccurrence) {
        self.record.values.entry(name).or_default().push(occurrence);
    }

    fn push_reflective(
        &mut self,
        node: Node<'_>,
        form: ReflectiveForm,
        reach: ReflectiveReach,
        receiver: Option<LookupReceiver>,
        key: KeyLiterals,
    ) {
        self.record.reflective.push(ReflectiveSite {
            line: line_of(node),
            form,
            reach,
            receiver,
            enumerable_only: false,
            key_prefix: key.prefix,
            key_suffix: key.suffix,
            called: false,
            excerpt: collapsed_text(node, self.source, EXCERPT_MAX),
            key_parts: key.parts,
        });
    }
}

/// Whether a method overrides, or may override, a supertype's method: it is
/// an instance method that is not private, and it says `@Override`, is one of
/// `Object`'s, or belongs to a type that names a supertype. An enum
/// constant's body and an anonymous class always have one.
pub(crate) fn may_override(method: Node<'_>, source: &[u8]) -> bool {
    let name = method
        .child_by_field_name("name")
        .map(|name| text_of(name, source))
        .unwrap_or_default();
    let mut annotated = false;
    let mut cursor = method.walk();
    for child in method.children(&mut cursor) {
        if child.kind() != "modifiers" {
            continue;
        }
        let mut inner = child.walk();
        for modifier in child.children(&mut inner) {
            match modifier.kind() {
                "static" | "private" => return false,
                "marker_annotation" | "annotation" => {
                    let spelled = modifier
                        .child_by_field_name("name")
                        .map(|name| text_of(name, source))
                        .unwrap_or_default();
                    if matches!(spelled, "Override" | "java.lang.Override") {
                        annotated = true;
                    }
                }
                _ => {}
            }
        }
    }
    if annotated || OBJECT_METHODS.contains(&name) {
        return true;
    }
    let Some(body) = method.parent() else {
        return false;
    };
    let owner = match body.kind() {
        "class_body" | "interface_body" => body.parent(),
        "enum_body_declarations" => body.parent().and_then(|enum_body| enum_body.parent()),
        _ => None,
    };
    owner.is_some_and(|owner| {
        owner.kind() == "enum_constant" || java_declares_supertype(&owner, source)
    })
}

/// Where the value an expression evaluates to goes.
fn flow_of(node: Node<'_>) -> OccurrenceContext {
    let mut current = node;
    loop {
        let Some(parent) = current.parent() else {
            return OccurrenceContext::Other;
        };
        match parent.kind() {
            "parenthesized_expression" | "cast_expression" => {}
            "ternary_expression" if !is_field(parent, "condition", current) => {}
            "argument_list" => return OccurrenceContext::Argument,
            "variable_declarator" => return OccurrenceContext::Assignment,
            "assignment_expression" => {
                return match parent.child_by_field_name("left").map(|left| left.kind()) {
                    Some("array_access") => OccurrenceContext::Collection,
                    _ => OccurrenceContext::Assignment,
                }
            }
            "return_statement" | "lambda_expression" => return OccurrenceContext::Return,
            "array_initializer" => return OccurrenceContext::Collection,
            _ => return OccurrenceContext::Other,
        }
        current = parent;
    }
}

/// Where a reflective lookup's result goes, or `None` when it is dropped. A
/// lookup whose result is invoked or adjusted on the spot
/// (`getMethod("x").invoke(o)`) hands the method on through that call.
fn lookup_flow(node: Node<'_>) -> Option<OccurrenceContext> {
    let parent = node.parent()?;
    if parent.kind() == "expression_statement" {
        return None;
    }
    Some(flow_of(node))
}

/// What a lookup's name argument provably is.
enum Key {
    Literal(String),
    Computed(KeyLiterals),
}

/// A string literal's text when it holds no escape.
fn string_literal(node: Node<'_>, source: &[u8]) -> Option<String> {
    if node.kind() != "string_literal" {
        return None;
    }
    let mut cursor = node.walk();
    let mut out = String::new();
    for child in node.named_children(&mut cursor) {
        if child.kind() != "string_fragment" {
            return None;
        }
        out.push_str(text_of(child, source));
    }
    Some(out)
}

/// The name a lookup's argument spells: a literal, or a `+` concatenation of
/// literals and computed parts, read for the literal runs it provably holds.
fn key_of(node: Node<'_>, source: &[u8]) -> Key {
    let mut pieces = Vec::new();
    flatten_concatenation(node, source, &mut pieces);
    if pieces.iter().all(Option::is_some) {
        return Key::Literal(pieces.into_iter().flatten().collect());
    }
    Key::Computed(KeyLiterals::from_pieces(pieces))
}

fn flatten_concatenation(node: Node<'_>, source: &[u8], out: &mut Vec<Option<String>>) {
    match node.kind() {
        "parenthesized_expression" => match node.named_child(0) {
            Some(inner) => flatten_concatenation(inner, source, out),
            None => out.push(None),
        },
        "binary_expression"
            if node
                .child_by_field_name("operator")
                .is_some_and(|operator| text_of(operator, source) == "+") =>
        {
            match (
                node.child_by_field_name("left"),
                node.child_by_field_name("right"),
            ) {
                (Some(left), Some(right)) => {
                    flatten_concatenation(left, source, out);
                    flatten_concatenation(right, source, out);
                }
                _ => out.push(None),
            }
        }
        _ => out.push(string_literal(node, source)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::LanguageAdapter;

    fn census_of(source: &str) -> FileEscapeCensus {
        let tree = crate::languages::JavaAdapter
            .parse(source.as_bytes())
            .unwrap();
        census(&tree, source.as_bytes())
    }

    const SERVICE: &str = r#"package app;

import app.store.Store;

public class Service implements Runnable {
    private final Runnable task = this::load;

    void wire(List<Store> stores) {
        stores.forEach(Store::load);
        Supplier<Store> make = Store::new;
        executor.submit(() -> refresh());
        store.load();
        Method named = Store.class.getMethod("load");
        getClass().getDeclaredMethod("visit" + kind + "Node", Node.class).invoke(this);
        Method[] all = type.getDeclaredMethods();
        Class<?> loaded = Class.forName(className);
        Class<?> known = Class.forName("app.store.Store");
    }

    void load() {}

    @Override
    public void run() {}

    public String toString() { return ""; }

    private void helper() {}

    static void util() {}
}
"#;

    #[test]
    fn method_and_constructor_references_are_member_and_class_occurrences() {
        let census = census_of(SERVICE);
        let loads = &census.record.values["load"];
        let shapes: Vec<_> = loads
            .iter()
            .map(|occurrence| {
                (
                    occurrence.line,
                    occurrence.context,
                    occurrence.receiver.as_deref(),
                    occurrence.receiver_kind,
                )
            })
            .collect();
        assert_eq!(
            shapes,
            vec![
                (
                    5,
                    OccurrenceContext::Assignment,
                    Some("this"),
                    Some(ReceiverKind::Instance)
                ),
                (
                    8,
                    OccurrenceContext::Argument,
                    Some("Store"),
                    Some(ReceiverKind::Other)
                ),
                (
                    12,
                    OccurrenceContext::Assignment,
                    Some("Store.class"),
                    Some(ReceiverKind::Other)
                ),
            ],
            "a lambda's call and a plain call are call sites, not values"
        );
        assert!(loads
            .iter()
            .all(|occurrence| occurrence.binding == OccurrenceBinding::Member));

        let store = &census.record.values["Store"];
        assert_eq!(store.len(), 1, "a constructor reference: {store:?}");
        assert_eq!(store[0].context, OccurrenceContext::Assignment);
        assert_eq!(store[0].binding, OccurrenceBinding::Import);
        assert_eq!(store[0].module.as_deref(), Some("app.store"));
        assert!(
            !census.record.values.contains_key("refresh"),
            "a lambda holds a call, which its method's call sites already count"
        );
        assert_eq!(census.record.scope, ModuleScope::Closed);
    }

    #[test]
    fn computed_lookups_are_reflective_sites_with_their_literals() {
        let census = census_of(SERVICE);
        let sites: Vec<_> = census
            .record
            .reflective
            .iter()
            .map(|site| {
                (
                    site.line,
                    site.form,
                    site.reach,
                    site.key_prefix.as_deref(),
                    site.key_suffix.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            sites,
            vec![
                (
                    13,
                    ReflectiveForm::ComputedMember,
                    ReflectiveReach::Members,
                    Some("visit"),
                    Some("Node")
                ),
                (
                    14,
                    ReflectiveForm::Enumerate,
                    ReflectiveReach::Members,
                    None,
                    None
                ),
                (
                    15,
                    ReflectiveForm::DynamicImport,
                    ReflectiveReach::Nothing,
                    None,
                    None
                ),
            ]
        );
        assert!(census.record.reflective[0].key_admits("visitClassNode"));
        assert!(!census.record.reflective[0].key_admits("load"));
    }

    #[test]
    fn overriding_and_possibly_overriding_methods_are_virtual_dispatch_members() {
        let census = census_of(SERVICE);
        let mut marked: Vec<&str> = census
            .protocol_members
            .iter()
            .map(|stamp| stamp.name.as_str())
            .collect();
        marked.sort_unstable();
        assert_eq!(
            marked,
            vec!["load", "run", "toString", "wire"],
            "an instance method of a type that names a supertype may override; a private \
             or static one cannot"
        );

        let plain = census_of(
            "class Plain {\n    void load() {}\n    @Override public String toString() { return \"\"; }\n    public boolean equals(Object o) { return false; }\n}\n",
        );
        let mut marked: Vec<&str> = plain
            .protocol_members
            .iter()
            .map(|stamp| stamp.name.as_str())
            .collect();
        marked.sort_unstable();
        assert_eq!(
            marked,
            vec!["equals", "toString"],
            "a type that names no supertype overrides only Object's methods"
        );
    }

    #[test]
    fn a_lookup_on_a_declared_class_literal_reads_that_class_and_its_bases() {
        let census = census_of(
            "class Handlers extends Base implements Visitor {\n    void pick(String kind) {\n        Handlers.class.getDeclaredMethod(\"on\" + kind);\n        MethodHandles.lookup().findVirtual(Handlers.class, kind, type);\n        Other.class.getMethod(kind);\n    }\n}\n",
        );
        let classes: Vec<_> = census
            .record
            .classes
            .iter()
            .map(|class| (class.name.as_str(), class.line, class.bases.clone()))
            .collect();
        assert_eq!(
            classes,
            vec![(
                "Handlers",
                0,
                vec!["Base".to_string(), "Visitor".to_string()]
            )]
        );
        let declared = Some(LookupReceiver::Declared {
            name: "Handlers".into(),
            line: 0,
        });
        let sites: Vec<_> = census
            .record
            .reflective
            .iter()
            .map(|site| (site.line, site.reach, site.receiver.clone()))
            .collect();
        assert_eq!(
            sites,
            vec![
                (2, ReflectiveReach::Any, declared.clone()),
                (3, ReflectiveReach::Any, declared),
                (4, ReflectiveReach::Members, None,),
            ],
            "a class this file does not declare may be any class's members"
        );
        assert_eq!(
            census.record.reflective[0].key_prefix.as_deref(),
            Some("on")
        );
    }

    #[test]
    fn a_plain_method_has_nothing_to_list_and_an_unparsed_region_is_recorded() {
        let census = census_of("class Plain {\n    void load() { other(); }\n}\n");
        assert!(
            census.record.values.is_empty()
                && census.record.reflective.is_empty()
                && census.record.protocol.is_empty()
                && census.protocol_members.is_empty(),
            "{:?}",
            census.record
        );
        assert_eq!(census.record.classes.len(), 1);
        let broken = census_of("class Broken {\n    void load( { store::load }\n}\n");
        assert!(!broken.record.unparsed.is_empty());
    }
}
