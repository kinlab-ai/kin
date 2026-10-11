// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Definitions nested in JavaScript and TypeScript function bodies.
//!
//! The declaration walk in `javascript.rs` and `typescript.rs` reads module
//! scope, class bodies and the object literals bound at module scope. Anything
//! declared inside a function body stayed invisible, and every call written in
//! it was charged to the function around it. React's no-op renderer is the
//! shape that made this matter: `createReactNoop` builds its host config as a
//! `const` object literal inside the factory, so `commitTextUpdate` and the
//! rest of that config had no entity, and a question about who implements the
//! host-config contract could not name them.
//!
//! This pass runs after the declaration walk, over the whole tree, and admits
//! two nested shapes under the `enclosing.<locals>.name` rule of
//! [`crate::nested`]:
//!
//! - a class declared in a function body, through the same class walk a
//!   module-level class takes, and
//! - a `const`/`let`/`var` bound to an object literal with function-valued
//!   properties, as a `Constant` owning one `Method` per property.
//!
//! Nested plain functions (function declarations, generators, and functions
//! bound to a local) are not admitted. React alone holds thousands of them, and
//! most are closures no question is ever asked about; each is recorded in the
//! file's [`NestedDefinitionCensus`] instead, with every class and object this
//! pass declines, so an answer can name what it cannot see.
//!
//! A call written in the body of an admitted method moves to that method. The
//! walk is top-down: an outer admission extracts every call in its members,
//! and an admission further in takes the calls inside its own members back
//! from whichever entity held them. Calls elsewhere in a nested class or
//! object, such as a base-class expression or a field initializer, stay with
//! the enclosing entity, which is where they were before.

use kin_model::{EntityKind, FilePathId, Visibility};

use crate::adapter::{compute_fingerprint, span_from_node};
use crate::extract::{ExtractedEntity, ExtractedRelation};
use crate::nested::{
    local_scope_name, LocalScopeNames, NestedDefinitionCensus, UnadmittedNestedDefinition,
};

use super::javascript::{
    extract_js_class_like, extract_preceding_comment, is_js_function_like_node, js_heritage_name,
    js_method_owner, js_object_literal_methods, push_js_object_method,
};

/// Which declaration walk a nested class goes through. The two adapters model
/// a class body differently (TypeScript has signatures, abstract members and
/// typed fields), and a nested class has to come out exactly as a module-level
/// one does in the same file.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum JsDialect {
    JavaScript,
    TypeScript,
}

/// Admit the nested definitions this pass models, record the rest, and move
/// the calls written in admitted members onto them.
///
/// `module_name` is the file's own module entity, which is listed as the
/// holder of anything nested in a callback no entity owns (a `describe` block
/// at module scope, say). It may be empty.
#[allow(clippy::too_many_arguments)]
pub(super) fn extract_js_nested_definitions(
    root: &tree_sitter::Node,
    source: &[u8],
    file_id: &FilePathId,
    dialect: JsDialect,
    module_name: &str,
    entities: &mut Vec<ExtractedEntity>,
    relations: &mut Vec<ExtractedRelation>,
) -> NestedDefinitionCensus {
    let mut pass = NestedPass {
        source,
        file_id,
        dialect,
        module_name,
        census: NestedDefinitionCensus::default(),
        names: LocalScopeNames::default(),
    };
    pass.visit(root, false, entities, relations);
    pass.census
}

struct NestedPass<'a> {
    source: &'a [u8],
    file_id: &'a FilePathId,
    dialect: JsDialect,
    module_name: &'a str,
    census: NestedDefinitionCensus,
    names: LocalScopeNames,
}

/// A node whose body is a function scope: anything declared inside it is
/// nested.
fn opens_function_scope(node: &tree_sitter::Node) -> bool {
    matches!(
        node.kind(),
        "function_declaration"
            | "generator_function_declaration"
            | "function_expression"
            | "function"
            | "arrow_function"
            | "generator_function"
            | "method_definition"
    )
}

/// The name a named local function binds on the path to a nested definition:
/// a function declaration, or a local bound to a function value. It is a scope
/// in the definition's qualified name even though it is not admitted, the way
/// CPython's `__qualname__` spells `outer.<locals>.helper.<locals>.C`, so two
/// helpers each declaring a `C` still mint two names.
fn named_function_scope(node: &tree_sitter::Node, source: &[u8]) -> Option<String> {
    let name = match node.kind() {
        "function_declaration" | "generator_function_declaration" => {
            node.child_by_field_name("name")?
        }
        "variable_declarator" => {
            let value = node.child_by_field_name("value")?;
            if !is_js_function_like_node(&value) {
                return None;
            }
            let name = node.child_by_field_name("name")?;
            if name.kind() != "identifier" {
                return None;
            }
            name
        }
        _ => return None,
    };
    let text = name.utf8_text(source).ok()?;
    (!text.is_empty()).then(|| text.to_string())
}

impl NestedPass<'_> {
    fn visit(
        &mut self,
        node: &tree_sitter::Node,
        inside_function: bool,
        entities: &mut Vec<ExtractedEntity>,
        relations: &mut Vec<ExtractedRelation>,
    ) {
        let mut cursor = node.walk();
        let children: Vec<tree_sitter::Node> = node.children(&mut cursor).collect();
        for child in children {
            if inside_function {
                self.consider(&child, entities, relations);
            }
            let inside = inside_function || opens_function_scope(&child);
            self.visit(&child, inside, entities, relations);
        }
    }

    /// Admit or record the definition `node` declares, if it declares one.
    fn consider(
        &mut self,
        node: &tree_sitter::Node,
        entities: &mut Vec<ExtractedEntity>,
        relations: &mut Vec<ExtractedRelation>,
    ) {
        match node.kind() {
            "class_declaration" | "abstract_class_declaration" => {
                if let Some(name) = self.text_of(node.child_by_field_name("name")) {
                    self.admit_class(node, node, &name, entities, relations);
                }
            }
            "function_declaration" | "generator_function_declaration" => {
                if let Some(name) = self.text_of(node.child_by_field_name("name")) {
                    self.record(node, &name, EntityKind::Function, Vec::new(), entities);
                }
            }
            "lexical_declaration" | "variable_declaration" => {
                let mut cursor = node.walk();
                let declarators: Vec<tree_sitter::Node> = node
                    .children(&mut cursor)
                    .filter(|child| child.kind() == "variable_declarator")
                    .collect();
                for declarator in declarators {
                    self.consider_declarator(&declarator, entities, relations);
                }
            }
            _ => {}
        }
    }

    fn consider_declarator(
        &mut self,
        declarator: &tree_sitter::Node,
        entities: &mut Vec<ExtractedEntity>,
        relations: &mut Vec<ExtractedRelation>,
    ) {
        // A destructuring pattern binds several names and declares none of
        // them a definition.
        let Some(name_node) = declarator
            .child_by_field_name("name")
            .filter(|name| name.kind() == "identifier")
        else {
            return;
        };
        let Some(name) = self.text_of(Some(name_node)) else {
            return;
        };
        let Some(value) = declarator.child_by_field_name("value") else {
            return;
        };
        let value = match self.dialect {
            // `as`, `satisfies`, `!` and parentheses change a value's static
            // type and nothing else.
            JsDialect::TypeScript => super::typescript::ts_effective_value(&value),
            JsDialect::JavaScript => value,
        };
        match value.kind() {
            "class" => self.admit_class(&value, declarator, &name, entities, relations),
            "object" => self.admit_object(declarator, &value, &name, entities, relations),
            _ if is_js_function_like_node(&value) => {
                self.record(
                    declarator,
                    &name,
                    EntityKind::Function,
                    Vec::new(),
                    entities,
                );
            }
            _ => {}
        }
    }

    /// A class declared in a function body, through the dialect's own class
    /// walk under its qualified name.
    fn admit_class(
        &mut self,
        class: &tree_sitter::Node,
        declaration: &tree_sitter::Node,
        raw_name: &str,
        entities: &mut Vec<ExtractedEntity>,
        relations: &mut Vec<ExtractedRelation>,
    ) {
        let bases = self.class_bases(class);
        let (enclosing, name) = self.local_name(declaration, raw_name, entities);
        let Some(enclosing) = enclosing.filter(|_| self.names.claim(&name)) else {
            self.record(declaration, raw_name, EntityKind::Class, bases, entities);
            return;
        };
        let entity_mark = entities.len();
        let relation_mark = relations.len();
        match self.dialect {
            JsDialect::JavaScript => {
                extract_js_class_like(class, &name, self.source, self.file_id, entities, relations)
            }
            JsDialect::TypeScript => super::typescript::extract_ts_class_like(
                class,
                &name,
                self.source,
                self.file_id,
                entities,
                relations,
            ),
        }
        relations.push(contains(&enclosing, &name));
        take_member_calls(entities, entity_mark + 1, relations, relation_mark);
    }

    /// A local bound to an object literal with function-valued properties, as
    /// a `Constant` owning one `Method` per property.
    ///
    /// Every such binding is admitted, not only one that declares a type or
    /// is returned or passed on. Over React's `packages/` tree the narrower
    /// rule admitted 213 entities against this rule's 226, on a base of
    /// 14,092, so the distinction bought nothing worth an extra rule; both
    /// grow the entity set by about one and a half percent.
    ///
    /// It stays a `Constant`. A module-level namespace object is rekinded a
    /// class so a call written `utils.parse()` reaches the linker's
    /// receiver-method tier, but a call through a local binding names the
    /// binding's bare local, which never matches the qualified entity, so the
    /// rekinding would buy nothing here. A method's own `this.m()` calls are
    /// qualified with the owner either way.
    fn admit_object(
        &mut self,
        declarator: &tree_sitter::Node,
        object: &tree_sitter::Node,
        raw_name: &str,
        entities: &mut Vec<ExtractedEntity>,
        relations: &mut Vec<ExtractedRelation>,
    ) {
        let methods = js_object_literal_methods(object, self.source);
        if methods.is_empty() || js_method_owner(raw_name).is_none() {
            return;
        }
        let (enclosing, name) = self.local_name(declarator, raw_name, entities);
        let Some(enclosing) = enclosing.filter(|_| self.names.claim(&name)) else {
            self.record(
                declarator,
                raw_name,
                EntityKind::Constant,
                Vec::new(),
                entities,
            );
            return;
        };
        let statement = declarator.parent().unwrap_or(*declarator);
        let entity_mark = entities.len();
        let relation_mark = relations.len();
        entities.push(ExtractedEntity {
            kind: EntityKind::Constant,
            name: name.clone(),
            signature: crate::adapter::declaration_signature(declarator, self.source),
            visibility: Visibility::Private,
            doc_summary: extract_preceding_comment(&statement, self.source),
            fingerprint: compute_fingerprint(declarator, self.source),
            span: span_from_node(declarator, self.file_id),
            declaration_line: None,
        });
        relations.push(contains(&enclosing, &name));
        for (property, function) in &methods {
            push_js_object_method(
                &name,
                property,
                function,
                self.source,
                self.file_id,
                entities,
                relations,
            );
        }
        take_member_calls(entities, entity_mark + 1, relations, relation_mark);
    }

    /// Record a definition this pass does not admit.
    fn record(
        &mut self,
        declaration: &tree_sitter::Node,
        raw_name: &str,
        kind: EntityKind,
        bases: Vec<String>,
        entities: &[ExtractedEntity],
    ) {
        let (enclosing, name) = self.local_name(declaration, raw_name, entities);
        let enclosing =
            enclosing.or_else(|| (!self.module_name.is_empty()).then(|| self.module_name.into()));
        self.census.record(UnadmittedNestedDefinition {
            name,
            kind,
            start_line: declaration.start_position().row as u32,
            enclosing,
            bases,
        });
    }

    /// The innermost entity holding `declaration`, and the name the
    /// definition carries under it.
    ///
    /// The holder is found by span rather than threaded through the
    /// declaration walk, so every shape that walk admits (a function, a class
    /// method, an object method, a member assignment, a default export) holds
    /// what is nested in it without each arm having to say so. A module or
    /// namespace is not a function body and holds nothing here: a definition
    /// in a callback no entity owns gets no holder, and is recorded rather
    /// than admitted.
    fn local_name(
        &self,
        declaration: &tree_sitter::Node,
        raw_name: &str,
        entities: &[ExtractedEntity],
    ) -> (Option<String>, String) {
        let (start, end) = (declaration.start_byte(), declaration.end_byte());
        let holder = entities
            .iter()
            .filter(|entity| entity.kind != EntityKind::Module)
            .filter(|entity| entity.span.start_byte <= start && end <= entity.span.end_byte)
            .min_by_key(|entity| entity.span.end_byte - entity.span.start_byte);
        let (floor, ceiling) = holder.map_or((0, usize::MAX), |entity| {
            (entity.span.start_byte, entity.span.end_byte)
        });

        // Named local functions between the holder and the definition,
        // innermost first as the climb meets them.
        let mut scopes = Vec::new();
        let mut current = declaration.parent();
        while let Some(ancestor) = current {
            let (a_start, a_end) = (ancestor.start_byte(), ancestor.end_byte());
            if a_start < floor || a_end > ceiling || (a_start == floor && a_end == ceiling) {
                break;
            }
            if let Some(scope) = named_function_scope(&ancestor, self.source) {
                scopes.push(scope);
            }
            current = ancestor.parent();
        }

        let mut qualified: Option<String> = holder.map(|entity| entity.name.clone());
        for scope in scopes.iter().rev() {
            qualified = Some(match qualified {
                Some(outer) => local_scope_name(&outer, scope),
                None => scope.clone(),
            });
        }
        let name = match qualified {
            Some(outer) => local_scope_name(&outer, raw_name),
            None => raw_name.to_string(),
        };
        (holder.map(|entity| entity.name.clone()), name)
    }

    fn class_bases(&self, class: &tree_sitter::Node) -> Vec<String> {
        let mut bases = Vec::new();
        let mut cursor = class.walk();
        for child in class.children(&mut cursor) {
            if child.kind() != "class_heritage" {
                continue;
            }
            // JavaScript puts the base expression straight under the
            // heritage; TypeScript wraps it in an `extends_clause`.
            let mut heritage_cursor = child.walk();
            for part in child.named_children(&mut heritage_cursor) {
                let base = if part.kind() == "extends_clause" {
                    part.named_child(0)
                        .and_then(|value| js_heritage_name(&value, self.source))
                } else {
                    js_heritage_name(&part, self.source)
                };
                bases.extend(base);
            }
        }
        bases
    }

    fn text_of(&self, node: Option<tree_sitter::Node>) -> Option<String> {
        let text = node?.utf8_text(self.source).ok()?;
        (!text.is_empty()).then(|| text.to_string())
    }
}

fn contains(src: &str, dst: &str) -> ExtractedRelation {
    ExtractedRelation {
        site: None,
        receiver: None,
        call_shape: None,
        kind: kin_model::RelationKind::Contains,
        src_name: src.to_string(),
        dst_name: dst.to_string(),
        import_source: None,
    }
}

/// Move the calls written in newly admitted members onto those members.
///
/// `entities[first_member..]` are the members just admitted, and every call
/// they own was pushed after `relation_mark`. A call already recorded before
/// that mark whose site lies inside one of their bodies was attributed to an
/// enclosing entity only because the member had no entity yet, so it is
/// dropped: the member's own copy replaces it. Only callable members count,
/// so the calls in a class's base expression or a data field stay where they
/// were.
fn take_member_calls(
    entities: &[ExtractedEntity],
    first_member: usize,
    relations: &mut Vec<ExtractedRelation>,
    relation_mark: usize,
) {
    let bodies: Vec<(usize, usize)> = entities
        .get(first_member..)
        .unwrap_or_default()
        .iter()
        .filter(|entity| matches!(entity.kind, EntityKind::Method | EntityKind::Function))
        .map(|entity| (entity.span.start_byte, entity.span.end_byte))
        .collect();
    if bodies.is_empty() {
        return;
    }
    let mut index = 0;
    relations.retain(|relation| {
        let earlier = index < relation_mark;
        index += 1;
        !(earlier
            && relation.kind == kin_model::RelationKind::Calls
            && relation.site.as_ref().is_some_and(|site| {
                bodies
                    .iter()
                    .any(|&(start, end)| start <= site.start_byte && site.end_byte <= end)
            }))
    });
}
