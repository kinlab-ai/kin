// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Kotlin's facts for the value-escape record (see [`crate::value_escape`]).
//!
//! A Kotlin function is a value only through a callable reference, and a
//! class's constructor only through a constructor reference: a lambda holds
//! calls, which are call sites of the function around it.
//!
//! Recorded:
//!
//! - callable references (`::helper`, `Store::load`, `store::load`,
//!   `this::load`) as member occurrences of the function, and constructor
//!   references (`::Store`) as occurrences of the class, with where the value
//!   goes. An unqualified reference may name a member of an implicit receiver
//!   or a top-level function, so it is recorded as a member occurrence whose
//!   receiver is not the enclosing instance, which reads as either;
//! - `import a.b.load as ld` as an import alias, and `typealias S = Store` as
//!   an alias of the class;
//! - reflective lookups: Java reflection's `getMethod` and
//!   `getDeclaredMethod`, as the Java record takes them, and Kotlin
//!   reflection's member enumerations (`members`, `functions`,
//!   `declaredFunctions` and their member forms). A lookup on a class literal
//!   of a class the file declares (`Store::class.java.getMethod(k)`) reads
//!   that class's members and its bases', and the record lists every class the
//!   file declares with the supertypes it names;
//! - protocol members: an `override` function, which a call typed by a
//!   supertype reaches by virtual dispatch; an `operator` function, which
//!   operator syntax, indexing, destructuring, iteration and delegation reach;
//!   and an `infix` function, which an infix call reaches without a call
//!   expression.
//!
//! A private top-level declaration is visible in its own file only, so the
//! module scope is closed.

use std::collections::HashMap;

use kin_model::{
    ClassBases, FileEscapeRecord, LookupReceiver, ModuleScope, OccurrenceBinding,
    OccurrenceContext, ProtocolMember, ReceiverKind, ReflectiveForm, ReflectiveReach,
    ReflectiveSite, ValueOccurrence, EXCERPT_MAX, RECEIVER_TEXT_MAX,
};
use tree_sitter::{Node, Tree};

use crate::languages::java_escape::VIRTUAL_DISPATCH;
use crate::value_escape::{
    collapsed_text, line_of, text_of, unparsed_lines, FileEscapeCensus, KeyLiterals, ProtocolStamp,
};

/// Kotlin reflection's properties that enumerate a class's members.
const MEMBER_ENUMERATIONS: &[&str] = &[
    "members",
    "functions",
    "declaredFunctions",
    "memberFunctions",
    "declaredMemberFunctions",
    "staticFunctions",
    "memberExtensionFunctions",
];

/// Take the census of one Kotlin file.
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

/// The names a file binds at its top level and in its type bodies.
struct FileTypes {
    /// Classes, objects and interfaces declared outside any function body.
    declared: HashMap<String, Vec<u32>>,
    /// Every class the file declares outside a function body, with the
    /// supertypes it names.
    classes: Vec<ClassBases>,
}

impl FileTypes {
    fn read(root: Node<'_>, source: &[u8]) -> Self {
        let mut types = FileTypes {
            declared: HashMap::new(),
            classes: Vec::new(),
        };
        let mut stack = vec![(root, false)];
        while let Some((node, in_callable)) = stack.pop() {
            if matches!(node.kind(), "class_declaration" | "object_declaration") && !in_callable {
                if let Some(name) = node
                    .child_by_field_name("name")
                    .map(|name| text_of(name, source).to_string())
                    .filter(|name| !name.is_empty())
                {
                    types.classes.push(ClassBases {
                        name: name.clone(),
                        line: line_of(node),
                        bases: supertype_leaves(node, source),
                        open: false,
                        typing_only: false,
                        structural: false,
                        dynamic: false,
                    });
                    types.declared.entry(name).or_default().push(line_of(node));
                }
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

    /// The one class the file declares under `name`, as a lookup receiver.
    fn declared_receiver(&self, name: &str) -> Option<LookupReceiver> {
        match self.declared.get(name)?.as_slice() {
            [line] => Some(LookupReceiver::Declared {
                name: name.to_string(),
                line: *line,
            }),
            _ => None,
        }
    }
}

/// The simple names of the supertypes a class or object names.
fn supertype_leaves(node: Node<'_>, source: &[u8]) -> Vec<String> {
    let mut leaves = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let specifiers: Vec<Node> = match child.kind() {
            "delegation_specifiers" => {
                let mut inner = child.walk();
                let found = child
                    .named_children(&mut inner)
                    .filter(|spec| spec.kind() == "delegation_specifier")
                    .collect();
                found
            }
            "delegation_specifier" => vec![child],
            _ => continue,
        };
        for specifier in specifiers {
            let mut stack = vec![specifier];
            while let Some(part) = stack.pop() {
                if part.kind() == "user_type" {
                    if let Some(name) =
                        crate::value_origins::kotlin::kotlin_erased_type_name(part, source)
                    {
                        let leaf = name.rsplit('.').next().unwrap_or(&name).to_string();
                        if !leaves.contains(&leaf) {
                            leaves.push(leaf);
                        }
                    }
                    break;
                }
                if part.kind() == "value_arguments" {
                    continue;
                }
                let mut inner = part.walk();
                let children: Vec<Node> = part.named_children(&mut inner).collect();
                stack.extend(children.into_iter().rev());
            }
        }
    }
    leaves
}

/// A node whose body runs as a callable, so a class declared in it is local.
fn opens_callable(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "function_declaration"
            | "secondary_constructor"
            | "anonymous_initializer"
            | "lambda_literal"
            | "anonymous_function"
            | "getter"
            | "setter"
    )
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
            "callable_reference" => self.unqualified_reference(node),
            "navigation_expression" => self.navigation(node),
            "call_expression" => self.call(node),
            "import" => self.import(node),
            "type_alias" => self.type_alias(node),
            "function_declaration" => self.mark_protocol_member(node),
            _ => {}
        }
        let mut cursor = node.walk();
        let children: Vec<Node<'_>> = node.named_children(&mut cursor).collect();
        for child in children {
            self.visit(child);
        }
    }

    /// `::helper`, `::Store`.
    fn unqualified_reference(&mut self, node: Node<'_>) {
        let mut cursor = node.walk();
        let Some(name) = node
            .named_children(&mut cursor)
            .filter(|child| child.kind() == "identifier")
            .last()
        else {
            return;
        };
        let name = text_of(name, self.source).to_string();
        if name.is_empty() {
            return;
        }
        self.push_member(name, node, None, ReceiverKind::Other);
    }

    /// `Store::load`, `this::load`, and the member enumerations of Kotlin
    /// reflection (`Store::class.members`).
    fn navigation(&mut self, node: Node<'_>) {
        let mut cursor = node.walk();
        let parts: Vec<Node<'_>> = node.children(&mut cursor).collect();
        let [receiver, operator, member] = parts.as_slice() else {
            return;
        };
        let member_name = text_of(*member, self.source);
        if operator.kind() == "::" {
            if member_name == "class" || member.kind() != "identifier" {
                return;
            }
            let kind = match receiver.kind() {
                "this_expression" | "super_expression" => ReceiverKind::Instance,
                _ => ReceiverKind::Other,
            };
            self.push_member(member_name.to_string(), node, Some(*receiver), kind);
            return;
        }
        if MEMBER_ENUMERATIONS.contains(&member_name) && !is_callee(node) {
            let receiver = class_literal(*receiver, self.source)
                .and_then(|class| self.types.declared_receiver(&class));
            let reach = if receiver.is_some() {
                ReflectiveReach::Any
            } else {
                ReflectiveReach::Members
            };
            self.push_reflective(
                node,
                ReflectiveForm::Enumerate,
                reach,
                receiver,
                KeyLiterals::default(),
            );
        }
    }

    /// Java reflection called from Kotlin: `getMethod` and
    /// `getDeclaredMethod`, and `Class.forName` by a computed name.
    fn call(&mut self, node: Node<'_>) {
        let Some(callee) = node.named_child(0) else {
            return;
        };
        if callee.kind() != "navigation_expression" {
            return;
        }
        let mut cursor = callee.walk();
        let parts: Vec<Node<'_>> = callee.children(&mut cursor).collect();
        let [object, operator, method] = parts.as_slice() else {
            return;
        };
        if !matches!(operator.kind(), "." | "?.") {
            return;
        }
        let method = text_of(*method, self.source);
        let arguments: Vec<Node<'_>> = {
            let mut cursor = node.walk();
            let found: Vec<Node<'_>> = node
                .named_children(&mut cursor)
                .filter(|child| child.kind() == "value_arguments")
                .flat_map(|arguments| {
                    let mut inner = arguments.walk();
                    let values: Vec<Node<'_>> = arguments
                        .named_children(&mut inner)
                        .filter_map(|argument| {
                            let last = argument.named_child_count().checked_sub(1)?;
                            argument.named_child(last as u32)
                        })
                        .collect();
                    values
                })
                .collect();
            found
        };
        match method {
            "getMethod" | "getDeclaredMethod" => {
                let Some(key) = arguments.first() else {
                    return;
                };
                match key_of(*key, self.source) {
                    Key::Literal(name) => {
                        if node.parent().is_some_and(|parent| {
                            matches!(parent.kind(), "statements" | "block" | "source_file")
                        }) {
                            return;
                        }
                        self.push_member_at(
                            name,
                            *key,
                            flow_of(node),
                            Some(*object),
                            ReceiverKind::Other,
                        );
                    }
                    Key::Computed(key) => {
                        // `Store::class.java.getMethod(k)` looks up on `Store`.
                        let receiver = java_class_of(*object, self.source)
                            .and_then(|class| self.types.declared_receiver(&class));
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
            }
            "forName" if matches!(text_of(*object, self.source), "Class" | "java.lang.Class") => {
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

    /// `import a.b.load as ld`.
    fn import(&mut self, node: Node<'_>) {
        let Some(path) = node
            .named_child(0)
            .filter(|child| child.kind() == "qualified_identifier")
        else {
            return;
        };
        let mut cursor = node.walk();
        let Some(alias) = node
            .named_children(&mut cursor)
            .find(|child| child.kind() == "identifier")
        else {
            return;
        };
        let full: String = text_of(path, self.source).split_whitespace().collect();
        let Some((module, imported)) = full.rsplit_once('.') else {
            return;
        };
        let local = text_of(alias, self.source);
        if local == imported || local.is_empty() {
            return;
        }
        self.push_value(
            imported.to_string(),
            ValueOccurrence {
                line: line_of(path),
                context: OccurrenceContext::Import,
                binding: OccurrenceBinding::Import,
                receiver: None,
                receiver_kind: None,
                aliases: vec![local.to_string()],
                module: Some(module.to_string()),
                import: Some((node.start_byte() as u32, node.end_byte() as u32)),
                imported: None,
                target_line: None,
                form: None,
            },
        );
    }

    /// `typealias S = Store` makes `S(...)` a call of `Store`'s constructor.
    fn type_alias(&mut self, node: Node<'_>) {
        let Some(alias) = node
            .child_by_field_name("type")
            .map(|name| text_of(name, self.source).to_string())
        else {
            return;
        };
        let mut cursor = node.walk();
        let Some(target) = node
            .named_children(&mut cursor)
            .filter(|child| child.kind() == "user_type")
            .last()
            .and_then(|ty| crate::value_origins::kotlin::kotlin_erased_type_name(ty, self.source))
        else {
            return;
        };
        let leaf = target.rsplit('.').next().unwrap_or(&target).to_string();
        if leaf == alias {
            return;
        }
        let target_line = match self.types.declared.get(&leaf).map(Vec::as_slice) {
            Some([line]) => Some(*line),
            _ => None,
        };
        self.push_value(
            leaf,
            ValueOccurrence {
                line: line_of(node),
                context: OccurrenceContext::Alias,
                binding: if target_line.is_some() {
                    OccurrenceBinding::Module
                } else {
                    OccurrenceBinding::Free
                },
                receiver: None,
                receiver_kind: None,
                aliases: vec![alias],
                module: None,
                import: None,
                imported: None,
                target_line,
                form: None,
            },
        );
    }

    fn mark_protocol_member(&mut self, function: Node<'_>) {
        let Some(name) = function.child_by_field_name("name") else {
            return;
        };
        let spelled = text_of(name, self.source);
        if spelled.is_empty() {
            return;
        }
        let mut modifiers = Vec::new();
        let mut cursor = function.walk();
        for child in function.children(&mut cursor) {
            if child.kind() == "modifiers" {
                modifiers.extend(
                    text_of(child, self.source)
                        .split_whitespace()
                        .map(str::to_string),
                );
            }
        }
        let has = |word: &str| modifiers.iter().any(|modifier| modifier == word);
        let member = if has("operator") {
            Some("operators and conventions")
        } else if has("override") {
            Some(VIRTUAL_DISPATCH)
        } else if has("infix") {
            Some("infix calls")
        } else {
            None
        };
        if let Some(through) = member {
            self.protocol_members.push(ProtocolStamp {
                name: spelled.to_string(),
                at: name.start_byte(),
                member: ProtocolMember::Implicit(through.into()),
            });
        }
    }

    fn push_member(
        &mut self,
        name: String,
        node: Node<'_>,
        receiver: Option<Node<'_>>,
        receiver_kind: ReceiverKind,
    ) {
        self.push_member_at(name, node, flow_of(node), receiver, receiver_kind);
    }

    /// A member occurrence written at `node` whose value goes to `context`.
    fn push_member_at(
        &mut self,
        name: String,
        node: Node<'_>,
        context: OccurrenceContext,
        receiver: Option<Node<'_>>,
        receiver_kind: ReceiverKind,
    ) {
        self.push_value(
            name,
            ValueOccurrence {
                line: line_of(node),
                context,
                binding: OccurrenceBinding::Member,
                receiver: receiver
                    .map(|receiver| collapsed_text(receiver, self.source, RECEIVER_TEXT_MAX)),
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

/// Whether a navigation expression is the callee of a call.
fn is_callee(node: Node<'_>) -> bool {
    node.parent().is_some_and(|parent| {
        parent.kind() == "call_expression"
            && parent
                .named_child(0)
                .is_some_and(|callee| callee.id() == node.id())
    })
}

/// The class `Store::class` names.
fn class_literal(node: Node<'_>, source: &[u8]) -> Option<String> {
    if node.kind() != "navigation_expression" {
        return None;
    }
    let mut cursor = node.walk();
    let parts: Vec<Node<'_>> = node.children(&mut cursor).collect();
    let [class, operator, member] = parts.as_slice() else {
        return None;
    };
    (operator.kind() == "::" && text_of(*member, source) == "class" && class.kind() == "identifier")
        .then(|| text_of(*class, source).to_string())
}

/// The class `Store::class.java` names.
fn java_class_of(node: Node<'_>, source: &[u8]) -> Option<String> {
    if node.kind() != "navigation_expression" {
        return None;
    }
    let mut cursor = node.walk();
    let parts: Vec<Node<'_>> = node.children(&mut cursor).collect();
    let [class, operator, member] = parts.as_slice() else {
        return None;
    };
    if operator.kind() != "." || text_of(*member, source) != "java" {
        return None;
    }
    class_literal(*class, source)
}

/// Where the value an expression evaluates to goes.
fn flow_of(node: Node<'_>) -> OccurrenceContext {
    let mut current = node;
    loop {
        let Some(parent) = current.parent() else {
            return OccurrenceContext::Other;
        };
        match parent.kind() {
            "parenthesized_expression" | "as_expression" => {}
            "value_argument" => return OccurrenceContext::Argument,
            "property_declaration" | "variable_declaration" => {
                return OccurrenceContext::Assignment
            }
            "assignment" => {
                return match parent.child_by_field_name("left").map(|left| left.kind()) {
                    Some("index_expression") => OccurrenceContext::Collection,
                    _ => OccurrenceContext::Assignment,
                }
            }
            "return_expression" | "function_body" | "lambda_literal" => {
                return OccurrenceContext::Return
            }
            _ => return OccurrenceContext::Other,
        }
        current = parent;
    }
}

enum Key {
    Literal(String),
    Computed(KeyLiterals),
}

/// A lookup's name argument: a string literal, or a string template or `+`
/// concatenation read for the literal runs it provably holds.
fn key_of(node: Node<'_>, source: &[u8]) -> Key {
    let mut pieces = Vec::new();
    pieces_of(node, source, &mut pieces);
    if pieces.iter().all(Option::is_some) {
        return Key::Literal(pieces.into_iter().flatten().collect());
    }
    Key::Computed(KeyLiterals::from_pieces(pieces))
}

fn pieces_of(node: Node<'_>, source: &[u8], out: &mut Vec<Option<String>>) {
    match node.kind() {
        "parenthesized_expression" => match node.named_child(0) {
            Some(inner) => pieces_of(inner, source, out),
            None => out.push(None),
        },
        "string_literal" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                match child.kind() {
                    "string_content" => {
                        let text = text_of(child, source);
                        if text.contains('\\') {
                            out.push(None);
                        } else {
                            out.push(Some(text.to_string()));
                        }
                    }
                    _ => out.push(None),
                }
            }
        }
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
                    pieces_of(left, source, out);
                    pieces_of(right, source, out);
                }
                _ => out.push(None),
            }
        }
        _ => out.push(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::LanguageAdapter;

    fn census_of(source: &str) -> FileEscapeCensus {
        let tree = crate::languages::KotlinAdapter
            .parse(source.as_bytes())
            .unwrap();
        census(&tree, source.as_bytes())
    }

    const SERVICE: &str = r#"package app

import app.store.Store
import app.util.load as fetch

typealias Repo = Store

class Service(private val store: Store) : Runnable {
    private val handler = ::helper

    fun wire(items: List<Store>) {
        items.forEach(Store::load)
        items.forEach(this::load)
        val make = ::Store
        executor.submit { refresh() }
        store.load()
        val named = Store::class.java.getMethod("load")
        val picked = Service::class.java.getDeclaredMethod("on" + kind)
        val all = Service::class.members
        val loaded = Class.forName(className)
    }

    override fun run() {}
    operator fun plus(other: Service): Service = this
    infix fun and(other: Service): Service = this
    fun load() {}
}
"#;

    #[test]
    fn callable_references_aliases_and_imports_are_recorded() {
        let census = census_of(SERVICE);
        let loads: Vec<_> = census.record.values["load"]
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
            loads,
            vec![
                (3, OccurrenceContext::Import, None, None),
                (
                    11,
                    OccurrenceContext::Argument,
                    Some("Store"),
                    Some(ReceiverKind::Other)
                ),
                (
                    12,
                    OccurrenceContext::Argument,
                    Some("this"),
                    Some(ReceiverKind::Instance)
                ),
                (
                    16,
                    OccurrenceContext::Assignment,
                    Some("Store::class.java"),
                    Some(ReceiverKind::Other)
                ),
            ]
        );
        assert_eq!(
            census.record.values["load"][0].aliases,
            vec!["fetch".to_string()]
        );
        let helper = &census.record.values["helper"];
        assert_eq!(helper[0].context, OccurrenceContext::Assignment);
        assert_eq!(helper[0].binding, OccurrenceBinding::Member);
        let store: Vec<_> = census.record.values["Store"]
            .iter()
            .map(|occurrence| (occurrence.context, occurrence.aliases.clone()))
            .collect();
        assert_eq!(
            store,
            vec![
                (OccurrenceContext::Alias, vec!["Repo".to_string()]),
                (OccurrenceContext::Assignment, Vec::new()),
            ],
            "a type alias and a constructor reference"
        );
        assert!(!census.record.values.contains_key("refresh"));
        assert_eq!(census.record.scope, ModuleScope::Closed);
    }

    #[test]
    fn reflective_lookups_and_enumerations_name_their_declared_class() {
        let census = census_of(SERVICE);
        let declared = Some(LookupReceiver::Declared {
            name: "Service".into(),
            line: 7,
        });
        let sites: Vec<_> = census
            .record
            .reflective
            .iter()
            .map(|site| (site.line, site.form, site.reach, site.receiver.clone()))
            .collect();
        assert_eq!(
            sites,
            vec![
                (
                    17,
                    ReflectiveForm::ComputedMember,
                    ReflectiveReach::Any,
                    declared.clone()
                ),
                (
                    18,
                    ReflectiveForm::Enumerate,
                    ReflectiveReach::Any,
                    declared
                ),
                (
                    19,
                    ReflectiveForm::DynamicImport,
                    ReflectiveReach::Nothing,
                    None
                ),
            ]
        );
        assert_eq!(
            census.record.reflective[0].key_prefix.as_deref(),
            Some("on")
        );
        assert_eq!(
            census.record.classes,
            vec![ClassBases {
                name: "Service".into(),
                line: 7,
                bases: vec!["Runnable".into()],
                open: false,
                typing_only: false,
                structural: false,
                dynamic: false,
            }]
        );
    }

    #[test]
    fn override_operator_and_infix_functions_are_protocol_members() {
        let census = census_of(SERVICE);
        let marked: Vec<_> = census
            .protocol_members
            .iter()
            .map(|stamp| (stamp.name.as_str(), stamp.member.clone()))
            .collect();
        assert_eq!(
            marked,
            vec![
                ("run", ProtocolMember::Implicit(VIRTUAL_DISPATCH.into())),
                (
                    "plus",
                    ProtocolMember::Implicit("operators and conventions".into())
                ),
                ("and", ProtocolMember::Implicit("infix calls".into())),
            ],
            "an open or abstract function is reached by spelling it; only an override is \
             reached through a supertype"
        );
    }
}
