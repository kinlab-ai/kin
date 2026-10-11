// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

use std::collections::HashSet;

use kin_model::{EntityKind, FilePathId, LanguageId, ParseState, Visibility};
use tree_sitter::{Node, Tree};

use crate::adapter::{
    collect_error_ranges, compute_fingerprint, make_parser, span_from_node, LanguageAdapter,
};
use crate::error::Result;
use crate::extract::{
    ExtractedEntity, ExtractedRelation, ExtractedTest, ExtractedTestKind, FileImport, ImportedName,
    ParseOutput,
};
use crate::nested::{
    local_scope_name, LocalScopeNames, NestedDefinitionCensus, UnadmittedNestedDefinition,
};

pub struct JavaAdapter;

impl LanguageAdapter for JavaAdapter {
    fn language_id(&self) -> LanguageId {
        LanguageId::Java
    }

    fn file_extensions(&self) -> &[&str] {
        &["java"]
    }

    fn parse(&self, source: &[u8]) -> Result<Tree> {
        let mut parser = make_parser(&tree_sitter_java::LANGUAGE)?;
        parser
            .parse(source, None)
            .ok_or_else(|| crate::error::ParseError::ParseFailed {
                file: String::new(),
                reason: "tree-sitter returned None".into(),
            })
    }

    fn extract(&self, tree: &Tree, source: &[u8], file_id: &FilePathId) -> Result<ParseOutput> {
        let error_ranges = collect_error_ranges(tree);
        let parse_state = if error_ranges.is_empty() {
            ParseState::Valid
        } else {
            ParseState::Incomplete { error_ranges }
        };

        let mut entities = Vec::new();
        let mut relations = Vec::new();
        let mut imports = Vec::new();
        let root = tree.root_node();
        let walk = JavaWalk {
            source,
            file_id,
            typed_receivers: !root.has_error(),
        };
        let mut cursor = root.walk();

        for child in root.children(&mut cursor) {
            walk.node(&child, None, &mut entities, &mut relations);
            if child.kind() == "import_declaration" {
                if let Some(file_import) = extract_java_import(&child, source) {
                    imports.push(file_import);
                }
            }
        }

        // Definitions written inside method bodies: local classes and
        // anonymous classes bound to a local are admitted under the
        // `enclosing.<locals>.name` rule, the rest are recorded.
        let nested = walk.nested_definitions(&root, &mut entities, &mut relations);

        // The file module owns entity-level import edges, and it has to sit
        // first so `module_entity_by_file` reads it rather than a declaration
        // that happens to sit above the rest. A file that declared nothing and
        // imported nothing contributes no entity: minting the synthetic module
        // there reported the file as parsed, which is what kept a comment-only
        // or unreadable file out of the parse-coverage census.
        if !entities.is_empty() || !imports.is_empty() {
            if let Some(module_name) = crate::adapter::file_module_surface_name(
                crate::adapter::declared_package_coordinate(&root, source).as_deref(),
                file_id,
            ) {
                entities.insert(
                    0,
                    crate::adapter::file_module_surface_entity(
                        module_name,
                        format!("package {}", file_id.0),
                        &root,
                        source,
                        file_id,
                    ),
                );
            }
        }

        // Calls no declaration walk reached (a static or instance initializer
        // block, a field initializer) go to the innermost entity holding each
        // one: the member, else the class, else the file's package surface.
        // The walk runs with no member's typed receivers, since none of that
        // code is a callable's body.
        {
            let accounted = crate::call_owner::accounted_call_sites(&relations);
            let mut swept = Vec::new();
            extract_calls_from_body(&root, source, "", &HashSet::new(), None, &mut swept);
            crate::call_owner::adopt_unowned_calls(&entities, &accounted, swept, &mut relations);
        }

        // Build import lookup: local_name -> module_path
        let import_map: std::collections::HashMap<&str, &str> = imports
            .iter()
            .flat_map(|imp| {
                imp.specifiers
                    .iter()
                    .map(move |spec| (spec.local_name.as_str(), imp.module_path.as_str()))
            })
            .collect();

        // Annotate Calls/References relations with import_source
        for rel in &mut relations {
            if matches!(
                rel.kind,
                kin_model::RelationKind::Calls | kin_model::RelationKind::References
            ) {
                if let Some(&module) = import_map.get(rel.dst_name.as_str()) {
                    rel.import_source = Some(module.to_string());
                }
            }
        }

        // Detect @Test annotated methods (JUnit)
        let mut tests = Vec::new();
        extract_java_tests(&root, source, &mut tests);

        // The unit's module owns what is declared at its top.
        let held = crate::module_contains::module_contains(&entities, &relations, file_id);
        relations.extend(held);

        Ok(ParseOutput {
            derived_members: Vec::new(),
            entities,
            relations,
            imports,
            tests,
            parse_state,
            parsed_call_sites: None,
            nested_definitions: Some(nested),
        })
    }

    fn declaration_census(
        &self,
        tree: &Tree,
        source: &[u8],
    ) -> Option<Vec<crate::declaration_census::SeenDeclaration>> {
        Some(super::java_declarations::java_declaration_census(
            tree, source,
        ))
    }
}

/// The erased name a Java type node spells, as the source writes it: `Store`,
/// `com.acme.Store` or `Map.Entry` for `Map.Entry<K, V>`. Type arguments are
/// dropped, since a call dispatches on the erased type. `None` for anything
/// that names no class: a primitive, an array, a wildcard, `void` and `var`.
pub(crate) fn java_erased_type_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    match node.kind() {
        "type_identifier" => {
            let text = node.utf8_text(source).ok()?.trim();
            (!text.is_empty() && text != "var" && is_java_identifier(text))
                .then(|| text.to_string())
        }
        "scoped_type_identifier" => {
            let text: String = node
                .utf8_text(source)
                .ok()?
                .split_whitespace()
                .collect::<Vec<_>>()
                .join("");
            text.split('.').all(is_java_identifier).then_some(text)
        }
        "generic_type" => {
            let mut cursor = node.walk();
            let base = node.named_children(&mut cursor).find(|child| {
                matches!(child.kind(), "type_identifier" | "scoped_type_identifier")
            })?;
            java_erased_type_name(base, source)
        }
        _ => None,
    }
}

fn is_java_identifier(text: &str) -> bool {
    let mut chars = text.chars();
    chars
        .next()
        .is_some_and(|first| first.is_alphabetic() || first == '_' || first == '$')
        && chars.all(|ch| ch.is_alphanumeric() || ch == '_' || ch == '$')
}

/// The supertypes a type declaration names, erased: the superclass, the
/// implemented interfaces, and the interfaces an interface extends. The
/// grammar wraps each list in a clause (`extends Base`, `implements A, B`), so
/// the names are read off the type nodes inside it, never the clause text.
fn java_supertypes(node: &Node<'_>, source: &[u8]) -> (Option<String>, Vec<String>, Vec<String>) {
    let superclass = node.child_by_field_name("superclass").and_then(|clause| {
        let mut cursor = clause.walk();
        let found = clause
            .named_children(&mut cursor)
            .find_map(|ty| java_erased_type_name(ty, source));
        found
    });
    let listed = |clause: Option<Node<'_>>| -> Vec<String> {
        let Some(clause) = clause else {
            return Vec::new();
        };
        let mut names = Vec::new();
        let mut cursor = clause.walk();
        for part in clause.named_children(&mut cursor) {
            if part.kind() == "type_list" {
                let mut inner = part.walk();
                names.extend(
                    part.named_children(&mut inner)
                        .filter_map(|ty| java_erased_type_name(ty, source)),
                );
            } else if let Some(name) = java_erased_type_name(part, source) {
                names.push(name);
            }
        }
        names
    };
    let interfaces = listed(node.child_by_field_name("interfaces"));
    let mut extended = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "extends_interfaces" {
            extended.extend(listed(Some(child)));
        }
    }
    (superclass, interfaces, extended)
}

/// Whether a type declaration names any supertype besides `Object`.
pub(crate) fn java_declares_supertype(node: &Node<'_>, source: &[u8]) -> bool {
    if node.kind() == "object_creation_expression" {
        return true;
    }
    let (superclass, interfaces, extended) = java_supertypes(node, source);
    superclass.is_some() || !interfaces.is_empty() || !extended.is_empty()
}

fn relation(kind: kin_model::RelationKind, src: &str, dst: &str) -> ExtractedRelation {
    ExtractedRelation {
        site: None,
        receiver: None,
        call_shape: None,
        kind,
        src_name: src.to_string(),
        dst_name: dst.to_string(),
        import_source: None,
    }
}

/// One Java file's extraction walk.
struct JavaWalk<'a> {
    source: &'a [u8],
    file_id: &'a FilePathId,
    /// Whether the tree parsed without error. The value-origin facts that type
    /// a call's receiver are recorded only then, so only then does a call
    /// carry the receiver those facts speak for.
    typed_receivers: bool,
}

impl JavaWalk<'_> {
    fn node(
        &self,
        node: &Node<'_>,
        class_ctx: Option<&str>,
        entities: &mut Vec<ExtractedEntity>,
        relations: &mut Vec<ExtractedRelation>,
    ) {
        let source = self.source;
        match node.kind() {
            "class_declaration"
            | "record_declaration"
            | "interface_declaration"
            | "annotation_type_declaration"
            | "enum_declaration" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = name_node.utf8_text(source).unwrap_or("").to_string();
                    if !name.is_empty() {
                        // A member type is named under the type that declares
                        // it (`Outer.Inner`), as its methods are, so two
                        // classes' same-named member types (`Builder`,
                        // `Entry`) cannot collide in one unit. That includes
                        // a member of a class declared in a method body,
                        // which stays inside that class's local-scope name.
                        let name = match class_ctx {
                            Some(owner) => format!("{owner}.{name}"),
                            None => name,
                        };
                        self.type_declaration(node, &name, entities, relations);
                    }
                }
            }
            "method_declaration"
            | "constructor_declaration"
            | "annotation_type_element_declaration" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let method_name = name_node.utf8_text(source).unwrap_or("").to_string();
                    let qualified = if let Some(cls) = class_ctx {
                        format!("{}.{}", cls, method_name)
                    } else {
                        method_name
                    };
                    entities.push(ExtractedEntity {
                        kind: EntityKind::Method,
                        name: qualified.clone(),
                        signature: node_signature(node, source),
                        visibility: detect_java_visibility(node, source),
                        doc_summary: extract_preceding_comment(node, source),
                        fingerprint: compute_fingerprint(node, source),
                        span: span_from_node(node, self.file_id),
                        declaration_line: None,
                    });
                    if let Some(cls) = class_ctx {
                        relations.push(relation(
                            kin_model::RelationKind::Contains,
                            cls,
                            &qualified,
                        ));
                    }
                    self.method_calls(node, &qualified, relations);
                }
            }
            "field_declaration" | "constant_declaration" => {
                // Extract constant fields (static final); an interface's
                // fields are implicitly both.
                let text = node.utf8_text(source).unwrap_or("");
                if node.kind() == "constant_declaration"
                    || (text.contains("static") && text.contains("final"))
                {
                    let mut cursor = node.walk();
                    for child in node.children(&mut cursor) {
                        if child.kind() == "variable_declarator" {
                            if let Some(name_node) = child.child_by_field_name("name") {
                                let name = name_node.utf8_text(source).unwrap_or("").to_string();
                                let qualified = if let Some(cls) = class_ctx {
                                    format!("{}.{}", cls, name)
                                } else {
                                    name
                                };
                                entities.push(ExtractedEntity {
                                    kind: EntityKind::Constant,
                                    name: qualified,
                                    signature: node_signature(node, source),
                                    visibility: detect_java_visibility(node, source),
                                    doc_summary: None,
                                    fingerprint: compute_fingerprint(node, source),
                                    span: span_from_node(node, self.file_id),
                                    declaration_line: None,
                                });
                            }
                        }
                    }
                } else if let Some(cls) = class_ctx {
                    self.fields(node, cls, entities, relations);
                }
            }
            "import_declaration" => {
                // Import handling is done via FileImport records (extract_java_import).
                // No ExtractedRelation needed here: the linker creates Imports edges
                // from FileImport specifiers in its Step 4 pass.
            }
            "program" => {
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    self.node(&child, class_ctx, entities, relations);
                }
            }
            _ => {}
        }
    }

    /// Every variable an instance or mutable field declaration declares, as a
    /// field entity `Owner.name` contained by its class. A declaration of
    /// one variable spans its modifiers and annotations; one of several gives
    /// each variable its own declarator's span.
    fn fields(
        &self,
        node: &Node<'_>,
        class: &str,
        entities: &mut Vec<ExtractedEntity>,
        relations: &mut Vec<ExtractedRelation>,
    ) {
        let source = self.source;
        let mut cursor = node.walk();
        let declarators: Vec<Node<'_>> = node
            .children(&mut cursor)
            .filter(|child| child.kind() == "variable_declarator")
            .collect();
        for declarator in &declarators {
            let Some(name) = declarator
                .child_by_field_name("name")
                .and_then(|name| name.utf8_text(source).ok())
                .filter(|name| !name.is_empty())
            else {
                continue;
            };
            let qualified = format!("{class}.{name}");
            let spanned = if declarators.len() == 1 {
                node
            } else {
                declarator
            };
            entities.push(ExtractedEntity {
                kind: EntityKind::Field,
                name: qualified.clone(),
                signature: node_signature(node, source),
                visibility: detect_java_visibility(node, source),
                doc_summary: extract_preceding_comment(node, source),
                fingerprint: compute_fingerprint(spanned, source),
                span: span_from_node(spanned, self.file_id),
                declaration_line: None,
            });
            relations.push(relation(
                kin_model::RelationKind::Contains,
                class,
                &qualified,
            ));
        }
    }

    /// A class, record, interface, annotation type or enum, admitted under
    /// `name`, with its supertypes and its members.
    fn type_declaration(
        &self,
        node: &Node<'_>,
        name: &str,
        entities: &mut Vec<ExtractedEntity>,
        relations: &mut Vec<ExtractedRelation>,
    ) {
        let source = self.source;
        let kind = match node.kind() {
            "interface_declaration" | "annotation_type_declaration" => EntityKind::Interface,
            "enum_declaration" => EntityKind::EnumDef,
            // A record is modeled as a Class; its components live in the
            // signature via node_signature (the record header line).
            _ => EntityKind::Class,
        };
        entities.push(ExtractedEntity {
            kind,
            name: name.to_string(),
            signature: node_signature(node, source),
            visibility: detect_java_visibility(node, source),
            doc_summary: extract_preceding_comment(node, source),
            fingerprint: compute_fingerprint(node, source),
            span: span_from_node(node, self.file_id),
            declaration_line: None,
        });
        self.supertype_relations(node, name, relations);
        let Some(body) = node.child_by_field_name("body") else {
            return;
        };
        let mut body_cursor = body.walk();
        for member in body.children(&mut body_cursor) {
            match member.kind() {
                "enum_constant" => self.enum_constant(&member, name, entities, relations),
                // An enum's own fields, methods and nested types follow its
                // constants in one wrapper node.
                "enum_body_declarations" => {
                    let mut inner = member.walk();
                    for declaration in member.children(&mut inner) {
                        self.node(&declaration, Some(name), entities, relations);
                    }
                }
                _ => self.node(&member, Some(name), entities, relations),
            }
        }
    }

    fn supertype_relations(
        &self,
        node: &Node<'_>,
        name: &str,
        relations: &mut Vec<ExtractedRelation>,
    ) {
        let (superclass, interfaces, extended) = java_supertypes(node, self.source);
        if let Some(parent) = superclass {
            relations.push(relation(kin_model::RelationKind::Extends, name, &parent));
        }
        for interface in interfaces {
            relations.push(relation(
                kin_model::RelationKind::Implements,
                name,
                &interface,
            ));
        }
        // An interface extends the interfaces it names.
        for parent in extended {
            relations.push(relation(kin_model::RelationKind::Extends, name, &parent));
        }
    }

    /// An enum constant, as a variant of its enum, with the methods its own
    /// class body declares.
    fn enum_constant(
        &self,
        member: &Node<'_>,
        enum_name: &str,
        entities: &mut Vec<ExtractedEntity>,
        relations: &mut Vec<ExtractedRelation>,
    ) {
        let source = self.source;
        let Some(const_name) = member.child_by_field_name("name") else {
            return;
        };
        let variant_name = const_name.utf8_text(source).unwrap_or("").to_string();
        let qualified = format!("{}.{}", enum_name, variant_name);
        entities.push(ExtractedEntity {
            kind: EntityKind::EnumVariant,
            name: qualified.clone(),
            signature: member
                .utf8_text(source)
                .unwrap_or("")
                .lines()
                .next()
                .unwrap_or("")
                .to_string(),
            visibility: Visibility::Public,
            doc_summary: extract_preceding_comment(member, source),
            fingerprint: compute_fingerprint(member, source),
            span: span_from_node(member, self.file_id),
            declaration_line: None,
        });
        relations.push(relation(
            kin_model::RelationKind::Contains,
            enum_name,
            &qualified,
        ));
        if let Some(body) = member.child_by_field_name("body") {
            let mut cursor = body.walk();
            for declaration in body.children(&mut cursor) {
                self.node(&declaration, Some(&qualified), entities, relations);
            }
        }
    }

    /// The calls a method or constructor body makes, charged to `context_name`.
    fn method_calls(
        &self,
        method: &Node<'_>,
        context_name: &str,
        relations: &mut Vec<ExtractedRelation>,
    ) {
        let receivers = if self.typed_receivers {
            crate::value_origins::java::receiver_names(*method, self.source)
        } else {
            HashSet::new()
        };
        extract_calls_from_body(
            method,
            self.source,
            context_name,
            &receivers,
            context_name.rsplit_once('.').map(|(owner, _)| owner),
            relations,
        );
    }

    // ── Definitions nested in method bodies ───────────────────────────────

    /// Admit the local classes and the anonymous classes bound to a local that
    /// method bodies declare, record every other nested definition, and move
    /// the calls written in admitted members onto them.
    fn nested_definitions(
        &self,
        root: &Node<'_>,
        entities: &mut Vec<ExtractedEntity>,
        relations: &mut Vec<ExtractedRelation>,
    ) -> NestedDefinitionCensus {
        let mut pass = NestedPass {
            walk: self,
            census: NestedDefinitionCensus::default(),
            names: LocalScopeNames::default(),
        };
        pass.visit(root, false, entities, relations);
        pass.census
    }
}

/// A node whose body is Java code that runs as a callable: a definition
/// written inside it is local to it.
fn opens_java_callable(node: &Node<'_>) -> bool {
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

/// Whether an anonymous class creation is the value of a local variable
/// declarator, which is the one position that gives it a name.
fn is_local_declarator_value(creation: &Node<'_>) -> bool {
    creation.parent().is_some_and(|declarator| {
        declarator.kind() == "variable_declarator"
            && declarator
                .child_by_field_name("value")
                .is_some_and(|value| value.id() == creation.id())
            && declarator
                .parent()
                .is_some_and(|statement| statement.kind() == "local_variable_declaration")
    })
}

struct NestedPass<'w, 'a> {
    walk: &'w JavaWalk<'a>,
    census: NestedDefinitionCensus,
    names: LocalScopeNames,
}

impl NestedPass<'_, '_> {
    fn visit(
        &mut self,
        node: &Node<'_>,
        inside_callable: bool,
        entities: &mut Vec<ExtractedEntity>,
        relations: &mut Vec<ExtractedRelation>,
    ) {
        let mut cursor = node.walk();
        let children: Vec<Node> = node.children(&mut cursor).collect();
        for child in children {
            if inside_callable {
                self.consider(&child, entities, relations);
            }
            let inside = inside_callable || opens_java_callable(&child);
            self.visit(&child, inside, entities, relations);
        }
    }

    fn consider(
        &mut self,
        node: &Node<'_>,
        entities: &mut Vec<ExtractedEntity>,
        relations: &mut Vec<ExtractedRelation>,
    ) {
        let source = self.walk.source;
        // A type declared directly in a class body is a member of that class,
        // which the class walk admits along with it.
        let member = node.parent().is_some_and(|parent| {
            matches!(
                parent.kind(),
                "class_body" | "interface_body" | "enum_body_declarations" | "annotation_type_body"
            )
        });
        match node.kind() {
            "class_declaration"
            | "record_declaration"
            | "interface_declaration"
            | "annotation_type_declaration"
            | "enum_declaration"
                if !member =>
            {
                let Some(raw) = node
                    .child_by_field_name("name")
                    .and_then(|name| name.utf8_text(source).ok())
                    .filter(|name| !name.is_empty())
                else {
                    return;
                };
                let (superclass, interfaces, extended) = java_supertypes(node, source);
                let bases: Vec<String> = superclass
                    .into_iter()
                    .chain(interfaces)
                    .chain(extended)
                    .collect();
                let kind = match node.kind() {
                    "interface_declaration" | "annotation_type_declaration" => {
                        EntityKind::Interface
                    }
                    "enum_declaration" => EntityKind::EnumDef,
                    _ => EntityKind::Class,
                };
                let (enclosing, name) = self.local_name(node, raw, entities);
                let Some(enclosing) = enclosing.filter(|_| self.names.claim(&name)) else {
                    self.record(node, raw, kind, bases, entities);
                    return;
                };
                let entity_mark = entities.len();
                let relation_mark = relations.len();
                self.walk.type_declaration(node, &name, entities, relations);
                relations.push(relation(
                    kin_model::RelationKind::Contains,
                    &enclosing,
                    &name,
                ));
                take_member_calls(entities, entity_mark + 1, relations, relation_mark);
            }
            "local_variable_declaration" => {
                let mut cursor = node.walk();
                let declarators: Vec<Node> = node
                    .children(&mut cursor)
                    .filter(|child| child.kind() == "variable_declarator")
                    .collect();
                for declarator in declarators {
                    self.consider_declarator(&declarator, entities, relations);
                }
            }
            "object_creation_expression" => {
                // An anonymous class anywhere else (an argument, a return
                // value, a field of another object) has no name to admit it
                // under, so it is recorded with the type it extends.
                if !has_class_body(node) {
                    return;
                }
                if is_local_declarator_value(node) {
                    return;
                }
                let bases: Vec<String> = node
                    .child_by_field_name("type")
                    .and_then(|ty| java_erased_type_name(ty, source))
                    .into_iter()
                    .collect();
                self.record(node, "<anonymous>", EntityKind::Class, bases, entities);
            }
            _ => {}
        }
    }

    fn consider_declarator(
        &mut self,
        declarator: &Node<'_>,
        entities: &mut Vec<ExtractedEntity>,
        relations: &mut Vec<ExtractedRelation>,
    ) {
        let source = self.walk.source;
        let Some(raw) = declarator
            .child_by_field_name("name")
            .and_then(|name| name.utf8_text(source).ok())
            .filter(|name| !name.is_empty())
            .map(str::to_string)
        else {
            return;
        };
        let Some(value) = declarator.child_by_field_name("value") else {
            return;
        };
        match value.kind() {
            "lambda_expression" => {
                self.record(declarator, &raw, EntityKind::Function, Vec::new(), entities);
            }
            "object_creation_expression" if has_class_body(&value) => {
                let base = value
                    .child_by_field_name("type")
                    .and_then(|ty| java_erased_type_name(ty, source));
                let (enclosing, name) = self.local_name(declarator, &raw, entities);
                let Some(enclosing) = enclosing.filter(|_| self.names.claim(&name)) else {
                    self.record(
                        declarator,
                        &raw,
                        EntityKind::Class,
                        base.into_iter().collect(),
                        entities,
                    );
                    return;
                };
                self.admit_anonymous(
                    declarator, &value, &name, &enclosing, base, entities, relations,
                );
            }
            _ => {}
        }
    }

    /// An anonymous class bound to a local, as a class named by the local,
    /// extending the type it instantiates, with its members.
    ///
    /// The base is recorded as `Extends` whichever it is: the grammar does not
    /// say whether `new T() {...}` names a class or an interface, and either
    /// is a supertype whose methods the class's calls can dispatch to.
    #[allow(clippy::too_many_arguments)]
    fn admit_anonymous(
        &mut self,
        declarator: &Node<'_>,
        creation: &Node<'_>,
        name: &str,
        enclosing: &str,
        base: Option<String>,
        entities: &mut Vec<ExtractedEntity>,
        relations: &mut Vec<ExtractedRelation>,
    ) {
        let source = self.walk.source;
        let entity_mark = entities.len();
        let relation_mark = relations.len();
        entities.push(ExtractedEntity {
            kind: EntityKind::Class,
            name: name.to_string(),
            signature: node_signature(declarator, source),
            visibility: Visibility::Private,
            doc_summary: None,
            fingerprint: compute_fingerprint(declarator, source),
            span: span_from_node(declarator, self.walk.file_id),
            declaration_line: None,
        });
        relations.push(relation(kin_model::RelationKind::Contains, enclosing, name));
        if let Some(base) = base {
            relations.push(relation(kin_model::RelationKind::Extends, name, &base));
        }
        if let Some(body) = class_body_of(creation) {
            let mut cursor = body.walk();
            for member in body.children(&mut cursor) {
                self.walk.node(&member, Some(name), entities, relations);
            }
        }
        take_member_calls(entities, entity_mark + 1, relations, relation_mark);
    }

    /// Record a definition this pass does not admit.
    fn record(
        &mut self,
        declaration: &Node<'_>,
        raw_name: &str,
        kind: EntityKind,
        bases: Vec<String>,
        entities: &[ExtractedEntity],
    ) {
        let (holder, name) = self.holder_and_name(declaration, raw_name, entities);
        let enclosing = holder.map(|(holder, _)| holder);
        self.census.record(UnadmittedNestedDefinition {
            name,
            kind,
            start_line: declaration.start_position().row as u32,
            enclosing,
            bases,
        });
    }

    /// The innermost method entity holding `declaration`, and the name the
    /// definition carries under it. A definition no method holds, such as one
    /// in an initializer block, is named under the class that holds it but
    /// gets no admitting holder, so it is recorded rather than admitted.
    fn local_name(
        &self,
        declaration: &Node<'_>,
        raw_name: &str,
        entities: &[ExtractedEntity],
    ) -> (Option<String>, String) {
        let (holder, name) = self.holder_and_name(declaration, raw_name, entities);
        (
            holder
                .filter(|(_, callable)| *callable)
                .map(|(name, _)| name),
            name,
        )
    }

    /// The innermost entity holding `declaration`, whether it is a callable,
    /// and the name the definition carries under it.
    fn holder_and_name(
        &self,
        declaration: &Node<'_>,
        raw_name: &str,
        entities: &[ExtractedEntity],
    ) -> (Option<(String, bool)>, String) {
        let (start, end) = (declaration.start_byte(), declaration.end_byte());
        let holder = entities
            .iter()
            .filter(|entity| entity.kind != EntityKind::Module)
            .filter(|entity| entity.span.start_byte <= start && end <= entity.span.end_byte)
            .min_by_key(|entity| entity.span.end_byte - entity.span.start_byte);
        match holder {
            Some(entity) => (
                Some((
                    entity.name.clone(),
                    matches!(entity.kind, EntityKind::Method | EntityKind::Function),
                )),
                local_scope_name(&entity.name, raw_name),
            ),
            None => (None, raw_name.to_string()),
        }
    }
}

/// The class body an anonymous class creation declares.
fn class_body_of<'t>(creation: &Node<'t>) -> Option<Node<'t>> {
    let mut cursor = creation.walk();
    let body = creation
        .children(&mut cursor)
        .find(|child| child.kind() == "class_body");
    body
}

fn has_class_body(creation: &Node<'_>) -> bool {
    class_body_of(creation).is_some()
}

/// Move the calls written in newly admitted members onto those members.
///
/// `entities[first_member..]` are the members just admitted, and every call
/// they own was pushed after `relation_mark`. A call recorded before that mark
/// whose site lies inside one of their bodies was charged to the enclosing
/// method only because the member had no entity yet, so it is dropped: the
/// member's own copy replaces it.
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

fn detect_java_visibility(node: &tree_sitter::Node, source: &[u8]) -> Visibility {
    // Check for modifiers
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "modifiers" {
            let text = child.utf8_text(source).unwrap_or("");
            if text.contains("public") {
                return Visibility::Public;
            } else if text.contains("private") {
                return Visibility::Private;
            } else if text.contains("protected") {
                return Visibility::Internal;
            }
        }
    }
    // Java default is package-private
    Visibility::Internal
}

fn node_signature(node: &tree_sitter::Node, source: &[u8]) -> String {
    crate::adapter::declaration_signature(node, source)
}

fn extract_preceding_comment(node: &tree_sitter::Node, source: &[u8]) -> Option<String> {
    let prev = node.prev_sibling()?;
    if prev.kind() == "block_comment" || prev.kind() == "line_comment" {
        let text = prev.utf8_text(source).ok()?;
        let cleaned = text
            .lines()
            .map(|l| {
                l.trim_start_matches('/')
                    .trim_start_matches('*')
                    .trim_end_matches('*')
                    .trim_end_matches('/')
                    .trim()
            })
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        if cleaned.is_empty() {
            None
        } else {
            Some(cleaned)
        }
    } else {
        None
    }
}

/// Recursively walk a method body to find `method_invocation` nodes.
///
/// A `method_invocation` carries the invoked method in its `name` field, so
/// `obj.execute()` and `a.b.execute()` both emit the simple `execute`. Graph
/// edges key on that rightmost name rather than the dotted source text, which
/// no name index could match.
///
/// A call whose receiver is a plain name in `receivers` carries that name as
/// its receiver: those are the names the method's value-origin facts type, so
/// the linker can follow the receiver to its declared class. Every other call
/// keeps no receiver, exactly as before those facts existed, and so does
/// every call inside a class body nested in the method, where an inherited
/// field can shadow any name.
fn extract_calls_from_body(
    node: &tree_sitter::Node,
    source: &[u8],
    context_name: &str,
    receivers: &HashSet<String>,
    self_owner: Option<&str>,
    relations: &mut Vec<ExtractedRelation>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "method_invocation" {
            if let Some(name_node) = child.child_by_field_name("name") {
                let method_name = name_node.utf8_text(source).unwrap_or("");
                if !method_name.is_empty() {
                    let receiver = child.child_by_field_name("object").and_then(|object| {
                        let spelled = match object.kind() {
                            "identifier" => object.utf8_text(source).ok()?.to_string(),
                            // `this.field`, one hop, which the facts key whole.
                            _ => format!(
                                "this.{}",
                                crate::value_origins::java::this_field(object, source)?
                            ),
                        };
                        receivers.contains(&spelled).then_some(spelled)
                    });
                    relations.push(ExtractedRelation {
                        // The invocation itself, so a reference row can report
                        // the line the call is written on. Without it the
                        // linker has no span to store and every consuming
                        // surface reports the edge as having no evidence span.
                        site: Some(crate::adapter::site_from_node(&child)),
                        receiver,
                        call_shape: None,
                        kind: kin_model::RelationKind::Calls,
                        src_name: context_name.to_string(),
                        dst_name: child
                            .child_by_field_name("object")
                            .filter(|object| object.kind() == "this")
                            .and(self_owner)
                            .map(|owner| format!("{owner}.{method_name}"))
                            .unwrap_or_else(|| method_name.to_string()),
                        import_source: None,
                    });
                }
            }
        }
        if matches!(child.kind(), "class_body" | "interface_body" | "enum_body") {
            extract_calls_from_body(
                &child,
                source,
                context_name,
                &HashSet::new(),
                None,
                relations,
            );
        } else {
            extract_calls_from_body(
                &child,
                source,
                context_name,
                receivers,
                self_owner,
                relations,
            );
        }
    }
}

/// Extract a structured import from a Java `import_declaration` node.
fn extract_java_import(node: &tree_sitter::Node, source: &[u8]) -> Option<FileImport> {
    // `import com.example.store.*;` names a PACKAGE and binds every type in it.
    // The grammar puts `com.example.store` in the scoped_identifier and the
    // asterisk beside it, so splitting on the last dot as a type import would
    // record the package `com.example` and the type `store`, which is a package
    // this file never imported and a type that does not exist. The wildcard is
    // recorded the way the Kotlin adapter records its own, with the whole path
    // as the module and `*` as the bound name.
    let is_wildcard = node
        .utf8_text(source)
        .unwrap_or("")
        .trim_end()
        .trim_end_matches(';')
        .trim_end()
        .ends_with(".*");

    // Find the scoped_identifier or identifier child
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "scoped_identifier" {
            let full_path = child.utf8_text(source).unwrap_or("").to_string();
            if full_path.is_empty() {
                return None;
            }
            // The last segment of the qualified name is the type this import
            // binds, and it is the specifier a reader is looking for. The
            // grammar exposes it as the `name` field, so a wrapped
            // `import a.b.\n    C;` anchors on `C`'s line and not on the
            // `import` keyword's. A grammar that stops exposing the field
            // falls back to the whole path, which is still inside the
            // statement.
            let name_site =
                crate::adapter::site_from_node(&child.child_by_field_name("name").unwrap_or(child));
            if is_wildcard {
                return Some(FileImport {
                    site: crate::adapter::site_from_node(node),
                    module_path: full_path,
                    specifiers: vec![ImportedName {
                        local_name: "*".to_string(),
                        original_name: None,
                        is_default: false,
                        // `import a.b.*` writes no type name down; the
                        // statement's span is all there is to cite.
                        site: None,
                    }],
                });
            }
            // Split into module_path (everything before last dot) and local_name (last segment).
            //
            // Both halves are trimmed. Java puts no constraint on whitespace
            // inside a qualified name, so `import java.util.\n    List;`
            // parses, and an untrimmed split recorded the type as
            // `"\n    List"`: a name no file declares, which binds to nothing
            // and reads as an unresolved import rather than as a wrapped one.
            if let Some(dot_pos) = full_path.rfind('.') {
                let module_path = full_path[..dot_pos].trim().to_string();
                let local_name = full_path[dot_pos + 1..].trim().to_string();
                if local_name.is_empty() {
                    return None;
                }
                return Some(FileImport {
                    site: crate::adapter::site_from_node(node),
                    module_path,
                    specifiers: vec![ImportedName {
                        local_name,
                        original_name: None,
                        is_default: false,
                        site: Some(name_site),
                    }],
                });
            } else {
                // No dot — entire path is the name
                return Some(FileImport {
                    site: crate::adapter::site_from_node(node),
                    module_path: String::new(),
                    specifiers: vec![ImportedName {
                        local_name: full_path,
                        original_name: None,
                        is_default: false,
                        site: Some(name_site),
                    }],
                });
            }
        }
    }
    None
}

/// Recursively detect @Test annotated methods in Java source.
fn extract_java_tests(node: &tree_sitter::Node, source: &[u8], tests: &mut Vec<ExtractedTest>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "method_declaration" {
            // Check for @Test annotation on preceding siblings or children
            if has_test_annotation(&child, source) {
                if let Some(name_node) = child.child_by_field_name("name") {
                    let name = name_node.utf8_text(source).unwrap_or("").to_string();
                    if !name.is_empty() {
                        tests.push(ExtractedTest {
                            name,
                            kind: ExtractedTestKind::Unit,
                            runner: "junit".to_string(),
                        });
                    }
                }
            }
        }
        // Recurse into class bodies, etc.
        extract_java_tests(&child, source, tests);
    }
}

/// Check if a method has an @Test annotation (marker_annotation or annotation).
fn has_test_annotation(node: &tree_sitter::Node, source: &[u8]) -> bool {
    // Check children (modifiers node contains annotations)
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "modifiers" {
            let mut mod_cursor = child.walk();
            for m in child.children(&mut mod_cursor) {
                if m.kind() == "marker_annotation" || m.kind() == "annotation" {
                    let text = m.utf8_text(source).unwrap_or("");
                    if text.contains("Test") {
                        return true;
                    }
                }
            }
        }
        if child.kind() == "marker_annotation" || child.kind() == "annotation" {
            let text = child.utf8_text(source).unwrap_or("");
            if text.contains("Test") {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_java_class() {
        let adapter = JavaAdapter;
        let source = b"public class Dog { public void bark() {} }";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("Dog.java");
        let output = adapter.extract(&tree, source, &file_id).unwrap();
        assert!(matches!(output.parse_state, ParseState::Valid));
        let classes: Vec<_> = output
            .entities
            .iter()
            .filter(|e| e.kind == EntityKind::Class)
            .collect();
        assert_eq!(classes.len(), 1);
        assert_eq!(classes[0].name, "Dog");
        let methods: Vec<_> = output
            .entities
            .iter()
            .filter(|e| e.kind == EntityKind::Method)
            .collect();
        assert_eq!(methods.len(), 1);
        assert_eq!(methods[0].name, "Dog.bark");
    }

    #[test]
    fn parse_java_interface() {
        let adapter = JavaAdapter;
        let source = b"public interface Runnable { void run(); }";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("Runnable.java");
        let output = adapter.extract(&tree, source, &file_id).unwrap();
        let ifaces: Vec<_> = output
            .entities
            .iter()
            .filter(|e| e.kind == EntityKind::Interface)
            .collect();
        assert_eq!(ifaces.len(), 1);
        assert_eq!(ifaces[0].name, "Runnable");
    }

    #[test]
    fn parse_java_method_calls() {
        let adapter = JavaAdapter;
        let source =
            b"public class App { public void run() { System.out.println(\"hi\"); doWork(); } }";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("App.java");
        let output = adapter.extract(&tree, source, &file_id).unwrap();
        let calls: Vec<_> = output
            .relations
            .iter()
            .filter(|r| r.kind == kin_model::RelationKind::Calls)
            .collect();
        assert!(
            calls.len() >= 2,
            "expected at least 2 calls, got {}",
            calls.len()
        );
        let dst_names: Vec<&str> = calls.iter().map(|c| c.dst_name.as_str()).collect();
        // System.out.println → narrowed to the rightmost name
        assert!(
            dst_names.contains(&"println"),
            "expected println in {:?}",
            dst_names
        );
        assert!(
            dst_names.contains(&"doWork"),
            "expected doWork in {:?}",
            dst_names
        );
    }

    #[test]
    fn parse_java_imports() {
        let adapter = JavaAdapter;
        let source = b"import java.util.List;\nimport java.io.File;\n\npublic class App {}";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("App.java");
        let output = adapter.extract(&tree, source, &file_id).unwrap();
        assert_eq!(output.imports.len(), 2);

        let list_import = output
            .imports
            .iter()
            .find(|i| i.module_path == "java.util")
            .unwrap();
        assert_eq!(list_import.specifiers.len(), 1);
        assert_eq!(list_import.specifiers[0].local_name, "List");

        let file_import = output
            .imports
            .iter()
            .find(|i| i.module_path == "java.io")
            .unwrap();
        assert_eq!(file_import.specifiers.len(), 1);
        assert_eq!(file_import.specifiers[0].local_name, "File");
    }

    #[test]
    fn parse_java_qualified_method_calls() {
        let adapter = JavaAdapter;
        let source = br#"
public class Service {
    public void process() {
        mapper.writeValue(out, obj);
        this.init();
        super.close();
        helper.nested.transform(data);
        standalone();
    }
}
"#;
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("Service.java");
        let output = adapter.extract(&tree, source, &file_id).unwrap();
        let calls: Vec<_> = output
            .relations
            .iter()
            .filter(|r| r.kind == kin_model::RelationKind::Calls)
            .map(|r| r.dst_name.as_str())
            .collect();
        // mapper.writeValue(...) → rightmost name
        assert!(
            calls.contains(&"writeValue"),
            "expected writeValue in {:?}",
            calls
        );
        // A direct this call retains its lexical class owner.
        assert!(
            calls.contains(&"Service.init"),
            "expected Service.init in {:?}",
            calls
        );
        // super.close() → bare method name
        assert!(
            calls.contains(&"close"),
            "expected close (super stripped) in {:?}",
            calls
        );
        // helper.nested.transform(...) → rightmost name, chained receiver dropped
        assert!(
            calls.contains(&"transform"),
            "expected transform in {:?}",
            calls
        );
        // standalone() → bare method name (no receiver)
        assert!(
            calls.contains(&"standalone"),
            "expected standalone in {:?}",
            calls
        );
        // Only the syntactically proven this owner survives narrowing.
        assert!(
            calls
                .iter()
                .all(|c| !c.contains('.') || *c == "Service.init"),
            "expected only simple callee names in {:?}",
            calls
        );
    }

    #[test]
    fn parse_java_enum_variants() {
        let adapter = JavaAdapter;
        let source = b"public enum Color { RED, GREEN, BLUE }";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("Color.java");
        let output = adapter.extract(&tree, source, &file_id).unwrap();

        let enums: Vec<_> = output
            .entities
            .iter()
            .filter(|e| e.kind == EntityKind::EnumDef)
            .collect();
        assert_eq!(enums.len(), 1);
        assert_eq!(enums[0].name, "Color");

        let variants: Vec<_> = output
            .entities
            .iter()
            .filter(|e| e.kind == EntityKind::EnumVariant)
            .collect();
        assert_eq!(
            variants.len(),
            3,
            "expected 3 enum variants, got {:?}",
            variants.iter().map(|v| &v.name).collect::<Vec<_>>()
        );
        let variant_names: Vec<&str> = variants.iter().map(|v| v.name.as_str()).collect();
        assert!(variant_names.contains(&"Color.RED"));
        assert!(variant_names.contains(&"Color.GREEN"));
        assert!(variant_names.contains(&"Color.BLUE"));

        // Check Contains relations: the enum holds its variants, and the
        // unit's module, here named like the enum, holds the enum.
        let contains: Vec<_> = output
            .relations
            .iter()
            .filter(|r| {
                r.kind == kin_model::RelationKind::Contains
                    && r.src_name == "Color"
                    && r.dst_name != "Color"
            })
            .collect();
        assert_eq!(contains.len(), 3);
    }

    #[test]
    fn parse_java_record() {
        let adapter = JavaAdapter;
        let source =
            b"public record Point(int x, int y) implements Comparable { public int sum() { return x + y; } }";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("Point.java");
        let output = adapter.extract(&tree, source, &file_id).unwrap();

        let classes: Vec<_> = output
            .entities
            .iter()
            .filter(|e| e.kind == EntityKind::Class)
            .collect();
        assert_eq!(classes.len(), 1);
        assert_eq!(classes[0].name, "Point");
        // Record components are carried in the signature.
        assert!(
            classes[0].signature.contains("(int x, int y)"),
            "record signature should include components, got {:?}",
            classes[0].signature
        );

        // Body members are extracted and Contained, mirroring the class arm.
        let methods: Vec<_> = output
            .entities
            .iter()
            .filter(|e| e.kind == EntityKind::Method)
            .collect();
        assert_eq!(methods.len(), 1);
        assert_eq!(methods[0].name, "Point.sum");
        assert!(output
            .relations
            .iter()
            .any(|r| r.kind == kin_model::RelationKind::Contains
                && r.src_name == "Point"
                && r.dst_name == "Point.sum"));

        // Implemented interfaces become Implements relations.
        assert!(output
            .relations
            .iter()
            .any(|r| r.kind == kin_model::RelationKind::Implements
                && r.src_name == "Point"
                && r.dst_name == "Comparable"));
    }

    #[test]
    fn parse_java_annotation_type() {
        let adapter = JavaAdapter;
        let source = b"public @interface Marker { String value(); }";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("Marker.java");
        let output = adapter.extract(&tree, source, &file_id).unwrap();
        let ifaces: Vec<_> = output
            .entities
            .iter()
            .filter(|e| e.kind == EntityKind::Interface)
            .collect();
        assert_eq!(ifaces.len(), 1);
        assert_eq!(ifaces[0].name, "Marker");
        assert_eq!(ifaces[0].visibility, Visibility::Public);
    }

    /// A file whose bytes declare nothing mints no module surface.
    ///
    /// The surface is synthetic: it stands for the file, not for anything the
    /// file wrote. Minting it unconditionally made a comment-only or unreadable
    /// file count as parsed, so the parse-coverage census read a clean row for a
    /// repository holding a hole and could not name the file. Rust carried this
    /// rule already; these adapters did not.
    #[test]
    fn comment_only_java_file_mints_no_module_surface() {
        let adapter = JavaAdapter;
        let source = b"// nothing is declared here\n";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("lib/Silent.java");
        let output = adapter.extract(&tree, source, &file_id).unwrap();
        // A comment is valid source, not a broken file. The census separates
        // the two, and only a file that parses clean and declares nothing is
        // the case this rule is about.
        assert!(matches!(output.parse_state, ParseState::Valid));
        assert!(
            output.entities.is_empty(),
            "expected no entity, got {:?}",
            output
                .entities
                .iter()
                .map(|e| (e.kind, e.name.as_str()))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn java_file_with_one_declaration_mints_the_module_surface() {
        let adapter = JavaAdapter;
        let source = b"package com.example.storage;\n\npublic class Dog {}\n";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("lib/Dog.java");
        let output = adapter.extract(&tree, source, &file_id).unwrap();
        let modules: Vec<_> = output
            .entities
            .iter()
            .filter(|e| e.kind == EntityKind::Module)
            .collect();
        assert_eq!(modules.len(), 1);
        assert_eq!(modules[0].name, "storage");
        assert_eq!(output.entities[0].kind, EntityKind::Module);
    }

    /// An import alone is a surface too, and the edge is sourced at the module.
    #[test]
    fn java_file_with_only_an_import_mints_the_module_surface() {
        let adapter = JavaAdapter;
        let source = b"import java.util.List;\n";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("lib/Dog.java");
        let output = adapter.extract(&tree, source, &file_id).unwrap();
        assert!(!output.imports.is_empty());
        assert_eq!(output.entities[0].kind, EntityKind::Module);
    }

    /// A file holding only its package coordinate mints no module surface
    /// either, and that is deliberate.
    ///
    /// The coordinate names the module; it does not declare anything inside it.
    /// A file that writes only the coordinate produced no declaration and no
    /// import, so it reaches the parse-coverage census as a file holding no
    /// entity, which is what it is. Rust reads the same way for a file holding
    /// only `//! docs`.
    #[test]
    fn package_declaration_only_java_file_mints_no_module_surface() {
        let adapter = JavaAdapter;
        let source = b"package com.example.storage;\n";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("lib/Dog.java");
        let output = adapter.extract(&tree, source, &file_id).unwrap();
        assert!(matches!(output.parse_state, ParseState::Valid));
        assert!(
            output.entities.is_empty(),
            "expected no entity, got {:?}",
            output
                .entities
                .iter()
                .map(|e| (e.kind, e.name.as_str()))
                .collect::<Vec<_>>()
        );
    }
    #[test]
    fn direct_this_calls_keep_owner_but_anonymous_class_calls_do_not_borrow_it() {
        let source = br#"class Outer {
            void ping() {}
            void run() {
                this.ping();
                Runnable closure = () -> this.ping();
                new Runnable() { public void run() { this.ping(); } };
                other.ping();
            }
        }"#;
        let adapter = JavaAdapter;
        let tree = adapter.parse(source).unwrap();
        assert!(
            !tree.root_node().has_error(),
            "{}",
            tree.root_node().to_sexp()
        );
        let output = adapter
            .extract(&tree, source, &FilePathId::new("Outer.java"))
            .unwrap();
        let calls: Vec<_> = output
            .relations
            .iter()
            .filter(|r| r.kind == kin_model::RelationKind::Calls)
            .collect();
        assert_eq!(
            calls.iter().filter(|r| r.dst_name == "Outer.ping").count(),
            2,
            "{calls:?}"
        );
        assert_eq!(
            calls.iter().filter(|r| r.dst_name == "ping").count(),
            2,
            "{calls:?}"
        );
        assert!(calls.iter().all(|r| r.site.is_some()));
    }
}
