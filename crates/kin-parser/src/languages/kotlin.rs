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

pub struct KotlinAdapter;

impl LanguageAdapter for KotlinAdapter {
    fn language_id(&self) -> LanguageId {
        LanguageId::Kotlin
    }

    fn file_extensions(&self) -> &[&str] {
        &["kt", "kts"]
    }

    fn parse(&self, source: &[u8]) -> Result<Tree> {
        let mut parser = make_parser(&tree_sitter_kotlin_ng::LANGUAGE)?;
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
        let walk = KotlinWalk {
            source,
            file_id,
            typed_receivers: !root.has_error(),
        };
        let mut cursor = root.walk();

        for child in root.children(&mut cursor) {
            walk.node(&child, None, &mut entities, &mut relations);
            // tree-sitter-kotlin-ng emits individual `import` nodes at root level
            // (no `import_list` wrapper)
            if child.kind() == "import" {
                if let Some(file_import) = extract_kotlin_import(&child, source) {
                    imports.push(file_import);
                }
            }
        }

        // Definitions written inside function bodies: local classes and object
        // expressions bound to a local are admitted under the
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

        // Calls no declaration walk reached (a top-level or member property
        // initializer, an `init` block) go to the innermost entity holding
        // each one: the property, else the class, else the file's package
        // surface. The walk runs with no member's typed receivers, since none
        // of that code is a callable's body.
        {
            let accounted = crate::call_owner::accounted_call_sites(&relations);
            let mut swept = Vec::new();
            let (body, lambda, nested) = (HashSet::new(), HashSet::new(), HashSet::new());
            let scopes = CallScopes {
                body: &body,
                lambda: &lambda,
                nested: &nested,
            };
            extract_calls_from_body(&root, source, "", &scopes, scopes.body, None, &mut swept);
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

        // Detect @Test annotated functions (JUnit/KotlinTest)
        let mut tests = Vec::new();
        extract_kotlin_tests(&root, source, &mut tests);

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
        Some(super::kotlin_declarations::kotlin_declaration_census(
            tree, source,
        ))
    }
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

/// One Kotlin file's extraction walk.
struct KotlinWalk<'a> {
    source: &'a [u8],
    file_id: &'a FilePathId,
    /// Whether the tree parsed without error. The value-origin facts that type
    /// a call's receiver are recorded only then, so only then does a call
    /// carry the receiver those facts speak for.
    typed_receivers: bool,
}

impl KotlinWalk<'_> {
    fn node(
        &self,
        node: &Node<'_>,
        class_ctx: Option<&str>,
        entities: &mut Vec<ExtractedEntity>,
        relations: &mut Vec<ExtractedRelation>,
    ) {
        let source = self.source;
        match node.kind() {
            "class_declaration" | "object_declaration" => {
                if let Some(name) = find_named_child_text(node, "identifier", source) {
                    if name.is_empty() {
                        return;
                    }
                    // A nested type is named under the type that declares it
                    // (`Point.Inner`), as its members are, so two classes'
                    // same-named nested types cannot collide in one unit.
                    // That includes a member of a class declared in a
                    // function body, which stays inside its local-scope name.
                    let name = match class_ctx {
                        Some(owner) => format!("{owner}.{name}"),
                        None => name,
                    };
                    self.class_like(node, &name, entities, relations);
                }
            }
            "companion_object" => {
                // Companion objects: use the parent class as context or the companion name
                let companion_name = find_named_child_text(node, "identifier", source);
                let ctx_name = if let Some(ref cname) = companion_name {
                    if let Some(cls) = class_ctx {
                        format!("{}.{}", cls, cname)
                    } else {
                        cname.clone()
                    }
                } else if let Some(cls) = class_ctx {
                    format!("{}.Companion", cls)
                } else {
                    "Companion".to_string()
                };

                if let Some(body) = find_child_by_kind(node, "class_body") {
                    let mut inner = body.walk();
                    for member in body.children(&mut inner) {
                        self.node(&member, Some(&ctx_name), entities, relations);
                    }
                }
            }
            "function_declaration" => {
                if let Some(func_name) = find_named_child_text(node, "identifier", source) {
                    if func_name.is_empty() {
                        return;
                    }

                    let (kind, qualified) = if let Some(cls) = class_ctx {
                        (EntityKind::Method, format!("{}.{}", cls, func_name))
                    } else {
                        (EntityKind::Function, func_name)
                    };

                    let vis = detect_kotlin_visibility(node, source);
                    entities.push(ExtractedEntity {
                        kind,
                        name: qualified.clone(),
                        signature: node_signature(node, source),
                        visibility: vis,
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

                    self.function_calls(node, &qualified, relations);
                }
            }
            "secondary_constructor" => {
                if let Some(cls) = class_ctx {
                    let ctor_name = format!("{}.constructor", cls);
                    entities.push(ExtractedEntity {
                        kind: EntityKind::Method,
                        name: ctor_name.clone(),
                        signature: node_signature(node, source),
                        visibility: detect_kotlin_visibility(node, source),
                        doc_summary: extract_preceding_comment(node, source),
                        fingerprint: compute_fingerprint(node, source),
                        span: span_from_node(node, self.file_id),
                        declaration_line: None,
                    });
                    relations.push(relation(kin_model::RelationKind::Contains, cls, &ctor_name));
                    self.function_calls(node, &ctor_name, relations);
                }
            }
            "property_declaration" => {
                if let Some(prop_name) = find_variable_declaration_name(node, source) {
                    if prop_name.is_empty() {
                        return;
                    }

                    let is_const = has_modifier(node, source, "const");
                    let is_val = has_keyword_child(node, "val");

                    let qualified = if let Some(cls) = class_ctx {
                        format!("{}.{}", cls, prop_name)
                    } else {
                        prop_name
                    };

                    let kind = if class_ctx.is_none() && (is_const || is_val) {
                        EntityKind::Constant
                    } else {
                        EntityKind::StaticVar
                    };

                    entities.push(ExtractedEntity {
                        kind,
                        name: qualified.clone(),
                        signature: node_signature(node, source),
                        visibility: detect_kotlin_visibility(node, source),
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
                }
            }
            "type_alias" => {
                if let Some(name) = find_named_child_text(node, "identifier", source) {
                    if !name.is_empty() {
                        entities.push(ExtractedEntity {
                            kind: EntityKind::TypeAlias,
                            name,
                            signature: node_signature(node, source),
                            visibility: detect_kotlin_visibility(node, source),
                            doc_summary: extract_preceding_comment(node, source),
                            fingerprint: compute_fingerprint(node, source),
                            span: span_from_node(node, self.file_id),
                            declaration_line: None,
                        });
                    }
                }
            }
            _ => {}
        }
    }

    /// A class, interface, enum or object declaration admitted under `name`,
    /// with its supertypes, the properties its primary constructor declares,
    /// and its members.
    fn class_like(
        &self,
        node: &Node<'_>,
        name: &str,
        entities: &mut Vec<ExtractedEntity>,
        relations: &mut Vec<ExtractedRelation>,
    ) {
        let source = self.source;
        let entity_kind = if node.kind() == "object_declaration" {
            EntityKind::Class
        } else if has_keyword_child(node, "interface") {
            // In kotlin-ng grammar, class_declaration covers both `class` and
            // `interface`, told apart by the anonymous keyword child.
            EntityKind::Interface
        } else if has_modifier(node, source, "enum") {
            EntityKind::EnumDef
        } else {
            EntityKind::Class
        };
        entities.push(ExtractedEntity {
            kind: entity_kind,
            name: name.to_string(),
            signature: node_signature(node, source),
            visibility: detect_kotlin_visibility(node, source),
            doc_summary: extract_preceding_comment(node, source),
            fingerprint: compute_fingerprint(node, source),
            span: span_from_node(node, self.file_id),
            declaration_line: None,
        });

        // Extract inheritance from delegation_specifier children
        extract_inheritance(
            node,
            source,
            name,
            entity_kind == EntityKind::Interface,
            relations,
        );
        self.constructor_properties(node, name, entities, relations);

        // Recurse into class_body or enum_class_body
        let mut body_cursor = node.walk();
        for child in node.children(&mut body_cursor) {
            match child.kind() {
                "class_body" => {
                    let mut inner = child.walk();
                    for member in child.children(&mut inner) {
                        self.node(&member, Some(name), entities, relations);
                    }
                }
                "enum_class_body" => {
                    let mut inner = child.walk();
                    for member in child.children(&mut inner) {
                        if member.kind() == "enum_entry" {
                            if let Some(entry_name) =
                                find_child_text_by_kind(&member, "identifier", source)
                            {
                                let qualified = format!("{}.{}", name, entry_name);
                                entities.push(ExtractedEntity {
                                    kind: EntityKind::Constant,
                                    name: qualified.clone(),
                                    signature: node_signature(&member, source),
                                    visibility: Visibility::Public,
                                    doc_summary: None,
                                    fingerprint: compute_fingerprint(&member, source),
                                    span: span_from_node(&member, self.file_id),
                                    declaration_line: None,
                                });
                                relations.push(relation(
                                    kin_model::RelationKind::Contains,
                                    name,
                                    &qualified,
                                ));
                            }
                        } else {
                            // Non-enum-entry members (functions, properties, etc.)
                            self.node(&member, Some(name), entities, relations);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// The properties a primary constructor declares with `val` or `var`,
    /// which are members of the class like any property of its body.
    fn constructor_properties(
        &self,
        node: &Node<'_>,
        class: &str,
        entities: &mut Vec<ExtractedEntity>,
        relations: &mut Vec<ExtractedRelation>,
    ) {
        let source = self.source;
        let Some(constructor) = find_child_by_kind(node, "primary_constructor") else {
            return;
        };
        let Some(parameters) = find_child_by_kind(&constructor, "class_parameters") else {
            return;
        };
        let mut cursor = parameters.walk();
        for parameter in parameters.named_children(&mut cursor) {
            if parameter.kind() != "class_parameter"
                || !(has_keyword_child(&parameter, "val") || has_keyword_child(&parameter, "var"))
            {
                continue;
            }
            let Some(name) = find_child_text_by_kind(&parameter, "identifier", source) else {
                continue;
            };
            let qualified = format!("{class}.{name}");
            entities.push(ExtractedEntity {
                kind: EntityKind::StaticVar,
                name: qualified.clone(),
                signature: node_signature(&parameter, source),
                visibility: detect_kotlin_visibility(&parameter, source),
                doc_summary: None,
                fingerprint: compute_fingerprint(&parameter, source),
                span: span_from_node(&parameter, self.file_id),
                declaration_line: None,
            });
            relations.push(relation(
                kin_model::RelationKind::Contains,
                class,
                &qualified,
            ));
        }
    }

    /// The calls a function or constructor body makes, charged to
    /// `context_name`.
    fn function_calls(
        &self,
        function: &Node<'_>,
        context_name: &str,
        relations: &mut Vec<ExtractedRelation>,
    ) {
        let receivers = if self.typed_receivers {
            crate::value_origins::kotlin::receivers(*function, self.source)
        } else {
            crate::value_origins::kotlin::Receivers {
                anywhere: HashSet::new(),
                in_lambdas: HashSet::new(),
            }
        };
        let empty = HashSet::new();
        let scopes = CallScopes {
            body: &receivers.anywhere,
            lambda: &receivers.in_lambdas,
            nested: &empty,
        };
        extract_calls_from_body(
            function,
            self.source,
            context_name,
            &scopes,
            scopes.body,
            kotlin_self_owner(function, context_name),
            relations,
        );
    }

    // ── Definitions nested in function bodies ─────────────────────────────

    /// Admit the local classes and the object expressions bound to a local
    /// that function bodies declare, record every other nested definition,
    /// and move the calls written in admitted members onto them.
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

/// A node whose body is Kotlin code that runs as a callable: a definition
/// written inside it is local to it.
fn opens_kotlin_callable(node: &Node<'_>) -> bool {
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

/// The value a property declaration initializes its one name with.
fn property_initializer<'t>(declaration: &Node<'t>) -> Option<Node<'t>> {
    let mut cursor = declaration.walk();
    let mut seen_equals = false;
    for child in declaration.children(&mut cursor) {
        if seen_equals && child.is_named() {
            return Some(child);
        }
        if child.kind() == "=" {
            seen_equals = true;
        }
    }
    None
}

/// The supertypes an object expression or class names, as written.
fn kotlin_bases(node: &Node<'_>, source: &[u8]) -> Vec<String> {
    let mut bases = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let specifiers: Vec<Node> = match child.kind() {
            "delegation_specifiers" => {
                let mut inner = child.walk();
                let found = child
                    .children(&mut inner)
                    .filter(|spec| spec.kind() == "delegation_specifier")
                    .collect();
                found
            }
            "delegation_specifier" => vec![child],
            _ => continue,
        };
        for specifier in specifiers {
            if let Some(name) = extract_user_type_name(&specifier, source) {
                bases.push(name);
            }
        }
    }
    bases
}

struct NestedPass<'w, 'a> {
    walk: &'w KotlinWalk<'a>,
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
            let inside = inside_callable || opens_kotlin_callable(&child);
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
        // A function, property or type directly in a class or object body is
        // a member of that body, which the class walk admits or leaves.
        let member = node
            .parent()
            .is_some_and(|parent| matches!(parent.kind(), "class_body" | "enum_class_body"));
        if member
            && matches!(
                node.kind(),
                "function_declaration"
                    | "property_declaration"
                    | "class_declaration"
                    | "object_declaration"
                    | "companion_object"
            )
        {
            return;
        }
        match node.kind() {
            "class_declaration" | "object_declaration" => {
                let Some(raw) = find_named_child_text(node, "identifier", source)
                    .filter(|name| !name.is_empty())
                else {
                    return;
                };
                let kind = if has_keyword_child(node, "interface") {
                    EntityKind::Interface
                } else if has_modifier(node, source, "enum") {
                    EntityKind::EnumDef
                } else {
                    EntityKind::Class
                };
                let bases = kotlin_bases(node, source);
                let (enclosing, name) = self.local_name(node, &raw, entities);
                let Some(enclosing) = enclosing.filter(|_| self.names.claim(&name)) else {
                    self.record(node, &raw, kind, bases, entities);
                    return;
                };
                let entity_mark = entities.len();
                let relation_mark = relations.len();
                self.walk.class_like(node, &name, entities, relations);
                relations.push(relation(
                    kin_model::RelationKind::Contains,
                    &enclosing,
                    &name,
                ));
                take_member_calls(entities, entity_mark + 1, relations, relation_mark);
            }
            // A local function is recorded rather than admitted: its calls
            // stay with the function that holds it, and a bare call to it
            // spells a name no module-level entity carries.
            "function_declaration" => {
                if let Some(raw) = find_named_child_text(node, "identifier", source)
                    .filter(|name| !name.is_empty())
                {
                    self.record(node, &raw, EntityKind::Function, Vec::new(), entities);
                }
            }
            "property_declaration" => {
                let Some(value) = property_initializer(node) else {
                    return;
                };
                let single =
                    find_child_by_kind(node, "variable_declaration").and_then(|declaration| {
                        find_child_text_by_kind(&declaration, "identifier", source)
                    });
                let Some(raw) = single else {
                    return;
                };
                match value.kind() {
                    "lambda_literal" | "anonymous_function" => {
                        self.record(node, &raw, EntityKind::Function, Vec::new(), entities);
                    }
                    "object_literal" => {
                        let (enclosing, name) = self.local_name(node, &raw, entities);
                        let Some(enclosing) = enclosing.filter(|_| self.names.claim(&name)) else {
                            let bases = kotlin_bases(&value, source);
                            self.record(node, &raw, EntityKind::Class, bases, entities);
                            return;
                        };
                        self.admit_object(node, &value, &name, &enclosing, entities, relations);
                    }
                    _ => {}
                }
            }
            "object_literal" => {
                // An object expression anywhere else has no name to admit it
                // under, so it is recorded with the types it implements.
                let bound = node.parent().is_some_and(|parent| {
                    parent.kind() == "property_declaration"
                        && property_initializer(&parent)
                            .is_some_and(|value| value.id() == node.id())
                        && find_child_by_kind(&parent, "variable_declaration").is_some()
                });
                if bound {
                    return;
                }
                let bases = kotlin_bases(node, source);
                self.record(node, "<anonymous>", EntityKind::Class, bases, entities);
            }
            _ => {}
        }
    }

    /// An object expression bound to a local, as a class named by the local,
    /// with the supertypes it names and its members.
    fn admit_object(
        &mut self,
        declaration: &Node<'_>,
        object: &Node<'_>,
        name: &str,
        enclosing: &str,
        entities: &mut Vec<ExtractedEntity>,
        relations: &mut Vec<ExtractedRelation>,
    ) {
        let source = self.walk.source;
        let entity_mark = entities.len();
        let relation_mark = relations.len();
        entities.push(ExtractedEntity {
            kind: EntityKind::Class,
            name: name.to_string(),
            signature: node_signature(declaration, source),
            visibility: Visibility::Private,
            doc_summary: None,
            fingerprint: compute_fingerprint(declaration, source),
            span: span_from_node(declaration, self.walk.file_id),
            declaration_line: None,
        });
        relations.push(relation(kin_model::RelationKind::Contains, enclosing, name));
        extract_inheritance(object, source, name, false, relations);
        if let Some(body) = find_child_by_kind(object, "class_body") {
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
        self.census.record(UnadmittedNestedDefinition {
            name,
            kind,
            start_line: declaration.start_position().row as u32,
            enclosing: holder.map(|(holder, _)| holder),
            bases,
        });
    }

    /// The innermost function entity holding `declaration`, and the name the
    /// definition carries under it. A definition no function holds, such as
    /// one in a property initializer or an `init` block, gets no admitting
    /// holder, and is recorded rather than admitted.
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

/// Move the calls written in newly admitted members onto those members.
///
/// `entities[first_member..]` are the members just admitted, and every call
/// they own was pushed after `relation_mark`. A call recorded before that mark
/// whose site lies inside one of their bodies was charged to the enclosing
/// function only because the member had no entity yet, so it is dropped: the
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

/// Check if a node has an anonymous keyword child with the given kind text.
fn has_keyword_child(node: &tree_sitter::Node, keyword: &str) -> bool {
    let mut cursor = node.walk();
    let result = node
        .children(&mut cursor)
        .any(|c| !c.is_named() && c.kind() == keyword);
    result
}

/// Check if a node has a specific modifier keyword (e.g., "enum", "data", "const").
/// In kotlin-ng, modifiers contain typed children like `class_modifier`, `property_modifier`,
/// `inheritance_modifier`, etc. whose text is the keyword.
fn has_modifier(node: &tree_sitter::Node, source: &[u8], modifier_keyword: &str) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "modifiers" {
            let mut mod_cursor = child.walk();
            for m in child.children(&mut mod_cursor) {
                let text = m.utf8_text(source).unwrap_or("");
                if text == modifier_keyword {
                    return true;
                }
            }
        }
    }
    false
}

/// Find the first child of a given kind and return its text.
fn find_child_text_by_kind(node: &tree_sitter::Node, kind: &str, source: &[u8]) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == kind {
            let text = child.utf8_text(source).unwrap_or("").to_string();
            if !text.is_empty() {
                return Some(text);
            }
        }
    }
    None
}

/// Find the first **named** child of a given kind and return its text.
/// In kotlin-ng, `identifier` is a named node (unlike `simple_identifier` or `type_identifier`
/// which do not exist in this grammar). We use `child_by_field_name` when the grammar uses
/// a field like `name:`, falling back to kind-based search.
fn find_named_child_text(node: &tree_sitter::Node, kind: &str, source: &[u8]) -> Option<String> {
    // First try the "name" field which many kotlin-ng rules use
    if let Some(child) = node.child_by_field_name("name") {
        if child.kind() == kind {
            let text = child.utf8_text(source).unwrap_or("").to_string();
            if !text.is_empty() {
                return Some(text);
            }
        }
    }
    // Also try the "type" field (used by type_alias)
    if let Some(child) = node.child_by_field_name("type") {
        if child.kind() == kind {
            let text = child.utf8_text(source).unwrap_or("").to_string();
            if !text.is_empty() {
                return Some(text);
            }
        }
    }
    // Fallback to first named child matching the kind
    find_child_text_by_kind(node, kind, source)
}

/// Find the first child of a given kind.
fn find_child_by_kind<'a>(
    node: &'a tree_sitter::Node,
    kind: &str,
) -> Option<tree_sitter::Node<'a>> {
    let mut cursor = node.walk();
    let result = node.children(&mut cursor).find(|c| c.kind() == kind);
    result
}

/// Extract the variable name from a `variable_declaration` or `multi_variable_declaration`
/// inside a property_declaration.
/// In kotlin-ng, property_declaration contains `variable_declaration` which has an `identifier` child.
fn find_variable_declaration_name(node: &tree_sitter::Node, source: &[u8]) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "multi_variable_declaration" || child.kind() == "variable_declaration" {
            return find_child_text_by_kind(&child, "identifier", source);
        }
    }
    None
}

fn detect_kotlin_visibility(node: &tree_sitter::Node, source: &[u8]) -> Visibility {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "modifiers" {
            let mut mod_cursor = child.walk();
            for m in child.children(&mut mod_cursor) {
                if m.kind() == "visibility_modifier" {
                    let text = m.utf8_text(source).unwrap_or("");
                    return match text {
                        "public" => Visibility::Public,
                        "private" => Visibility::Private,
                        "protected" => Visibility::Internal,
                        "internal" => Visibility::Internal,
                        _ => Visibility::Public,
                    };
                }
            }
        }
    }
    // Kotlin default visibility is public
    Visibility::Public
}

fn node_signature(node: &tree_sitter::Node, source: &[u8]) -> String {
    crate::adapter::declaration_signature(node, source)
}

fn extract_preceding_comment(node: &tree_sitter::Node, source: &[u8]) -> Option<String> {
    let prev = node.prev_sibling()?;
    if prev.kind() == "multiline_comment"
        || prev.kind() == "comment"
        || prev.kind() == "line_comment"
    {
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

/// Extract inheritance/implementation from delegation_specifiers.
/// In kotlin-ng: class_declaration > delegation_specifiers > delegation_specifier > ...
fn extract_inheritance(
    node: &tree_sitter::Node,
    source: &[u8],
    type_name: &str,
    is_interface: bool,
    relations: &mut Vec<ExtractedRelation>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "delegation_specifiers" => {
                // Wrapper node containing delegation_specifier children
                let mut inner = child.walk();
                for spec in child.children(&mut inner) {
                    if spec.kind() == "delegation_specifier" {
                        extract_delegation_specifier(
                            &spec,
                            source,
                            type_name,
                            is_interface,
                            relations,
                        );
                    }
                }
            }
            "delegation_specifier" => {
                extract_delegation_specifier(&child, source, type_name, is_interface, relations);
            }
            _ => {}
        }
    }
}

fn extract_delegation_specifier(
    node: &tree_sitter::Node,
    source: &[u8],
    type_name: &str,
    is_interface: bool,
    relations: &mut Vec<ExtractedRelation>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "constructor_invocation" => {
                if let Some(parent_name) = extract_user_type_name(&child, source) {
                    relations.push(ExtractedRelation {
                        site: None,
                        receiver: None,
                        call_shape: None,
                        kind: kin_model::RelationKind::Extends,
                        src_name: type_name.to_string(),
                        dst_name: parent_name,
                        import_source: None,
                    });
                }
            }
            "user_type" => {
                let parent_name = extract_user_type_name(&child, source)
                    .unwrap_or_else(|| child.utf8_text(source).unwrap_or("").to_string());
                if !parent_name.is_empty() && parent_name != type_name {
                    let rel_kind = if is_interface {
                        kin_model::RelationKind::Extends
                    } else {
                        kin_model::RelationKind::Implements
                    };
                    relations.push(ExtractedRelation {
                        site: None,
                        receiver: None,
                        call_shape: None,
                        kind: rel_kind,
                        src_name: type_name.to_string(),
                        dst_name: parent_name,
                        import_source: None,
                    });
                }
            }
            _ => {}
        }
    }
}

/// Extract the type name from a user_type or constructor_invocation node.
fn extract_user_type_name(node: &tree_sitter::Node, source: &[u8]) -> Option<String> {
    // Try to find user_type child first (for constructor_invocation)
    if let Some(ut) = find_child_by_kind(node, "user_type") {
        return extract_user_type_name(&ut, source);
    }
    // In kotlin-ng, user_type contains `identifier` nodes
    find_child_text_by_kind(node, "identifier", source)
}

/// The receiver names a call may carry, by where in the function it sits.
struct CallScopes<'r> {
    /// In the function's own body.
    body: &'r HashSet<String>,
    /// Inside a lambda, anonymous function or local function, where an
    /// implicit receiver's members can shadow a property but never a local.
    lambda: &'r HashSet<String>,
    /// Inside a class or object body nested in the function.
    nested: &'r HashSet<String>,
}

/// Recursively walk a function body to find call_expression nodes.
///
/// A call written `receiver.method(...)` or `receiver?.method(...)` whose
/// receiver is a plain name in `receivers` carries that name as its receiver:
/// those are the names the function's value-origin facts type, so the linker
/// can follow the receiver to its declared class. Every other call keeps no
/// receiver, exactly as before those facts existed.
fn extract_calls_from_body(
    node: &tree_sitter::Node,
    source: &[u8],
    context_name: &str,
    scopes: &CallScopes<'_>,
    receivers: &HashSet<String>,
    self_owner: Option<&str>,
    relations: &mut Vec<ExtractedRelation>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "call_expression" {
            if let Some(callee) = extract_callee_name(&child, source) {
                if !callee.is_empty() {
                    relations.push(ExtractedRelation {
                        // The call expression itself, so a reference row can
                        // report the line the call is written on. Without it the
                        // linker has no span to store and every consuming
                        // surface reports the edge as having no evidence span.
                        site: Some(crate::adapter::site_from_node(&child)),
                        receiver: typed_receiver(&child, source, receivers),
                        call_shape: None,
                        kind: kin_model::RelationKind::Calls,
                        src_name: context_name.to_string(),
                        dst_name: find_child_by_kind(&child, "navigation_expression")
                            .and_then(|navigation| navigation.named_child(0))
                            .filter(|object| {
                                object.kind() == "this_expression"
                                    && object.utf8_text(source).ok() == Some("this")
                            })
                            .and(self_owner)
                            .map(|owner| format!("{owner}.{callee}"))
                            .unwrap_or(callee),
                        import_source: None,
                    });
                }
            }
        }
        let inner = match child.kind() {
            "class_body" | "enum_class_body" | "object_literal" => scopes.nested,
            // Entering a lambda or a local function from the function's own
            // body narrows to its locals, since either may have a receiver
            // whose members shadow a property; one inside a nested body stays
            // with that body's.
            "lambda_literal" | "anonymous_function" | "function_declaration"
                if std::ptr::eq(receivers, scopes.body) =>
            {
                scopes.lambda
            }
            _ => receivers,
        };
        let inner_owner = if matches!(
            child.kind(),
            "class_body"
                | "enum_class_body"
                | "object_literal"
                | "lambda_literal"
                | "anonymous_function"
                | "function_declaration"
        ) {
            None
        } else {
            self_owner
        };
        extract_calls_from_body(
            &child,
            source,
            context_name,
            scopes,
            inner,
            inner_owner,
            relations,
        );
    }
}

/// An extension's `this` belongs to the receiver type, not the surrounding
/// class. Refuse that shape here; typed value origins remain the authority.
fn kotlin_self_owner<'a>(function: &Node<'_>, context_name: &'a str) -> Option<&'a str> {
    let name = function.child_by_field_name("name")?;
    let mut cursor = function.walk();
    let extension = function
        .children(&mut cursor)
        .take_while(|child| child.id() != name.id())
        .any(|child| {
            matches!(
                child.kind(),
                "user_type"
                    | "nullable_type"
                    | "function_type"
                    | "parenthesized_type"
                    | "type_reference"
            )
        });
    if extension {
        None
    } else {
        context_name.rsplit_once('.').map(|(owner, _)| owner)
    }
}

/// The receiver of `receiver.method(...)` or `receiver?.method(...)` when it
/// is a plain name the function's facts type.
fn typed_receiver(
    call: &tree_sitter::Node,
    source: &[u8],
    receivers: &HashSet<String>,
) -> Option<String> {
    let callee = find_child_by_kind(call, "navigation_expression")?;
    let mut cursor = callee.walk();
    let parts: Vec<tree_sitter::Node> = callee.children(&mut cursor).collect();
    let [object, operator, _method] = parts.as_slice() else {
        return None;
    };
    if object.kind() != "identifier" || !matches!(operator.kind(), "." | "?.") {
        return None;
    }
    let name = object.utf8_text(source).ok()?;
    receivers.contains(name).then(|| name.to_string())
}

fn extract_callee_name(node: &tree_sitter::Node, source: &[u8]) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "identifier" | "simple_identifier" => {
                let raw = child.utf8_text(source).unwrap_or("");
                let stripped = raw.strip_prefix("this.").unwrap_or(raw);
                return Some(stripped.to_string());
            }
            "navigation_expression" => {
                return extract_trailing_identifier(&child, source);
            }
            "call_suffix" | "value_arguments" | "lambda_literal" | "annotated_lambda" => continue,
            _ => {}
        }
    }
    None
}

/// Walk a kotlin-ng `navigation_expression` to its rightmost `identifier` child.
/// The grammar nests chains left-associatively, so the trailing simple name is
/// always the last named `identifier` at the outermost level.
fn extract_trailing_identifier(node: &tree_sitter::Node, source: &[u8]) -> Option<String> {
    let mut cursor = node.walk();
    let mut last_ident: Option<String> = None;
    for child in node.children(&mut cursor) {
        if child.kind() == "identifier" || child.kind() == "simple_identifier" {
            last_ident = Some(child.utf8_text(source).unwrap_or("").to_string());
        }
    }
    last_ident.filter(|s| !s.is_empty())
}

/// Extract a structured import from a kotlin-ng `import` node.
///
/// Grammar shapes:
///   import kotlin.collections.List       => (import (qualified_identifier (identifier)x3))
///   import kotlin.collections.List as K  => (import (qualified_identifier (identifier)x3) (identifier))
///   import kotlin.collections.*           => (import (qualified_identifier (identifier)x2))
///                                            (wildcard detected by trailing `.*` in source text)
fn extract_kotlin_import(node: &tree_sitter::Node, source: &[u8]) -> Option<FileImport> {
    let qi_node = find_child_by_kind(node, "qualified_identifier")?;
    let full_path = qi_node.utf8_text(source).unwrap_or("").to_string();
    if full_path.is_empty() {
        return None;
    }

    // The last identifier in the qualified name is the type this import binds,
    // and it is the specifier a reader is looking for. Anchoring there means a
    // wrapped `import a.b.\n    C` reports `C`'s line rather than the `import`
    // keyword's. A qualified name with no identifier child falls back to the
    // whole path, which is still inside the statement.
    let name_site = crate::adapter::site_from_node(
        &qi_node
            .children(&mut qi_node.walk())
            .filter(|child| child.kind() == "identifier")
            .last()
            .unwrap_or(qi_node),
    );

    // Check for wildcard: source text of the import node ends with `.*`
    let node_text = node.utf8_text(source).unwrap_or("");
    let is_wildcard = node_text.trim().ends_with(".*");

    // Check for alias: an `identifier` child directly on the import node (not inside
    // qualified_identifier). The qualified_identifier holds the path; a second `identifier`
    // sibling is the alias.
    let alias = {
        let mut cursor = node.walk();
        let mut alias_text = None;
        for child in node.children(&mut cursor) {
            if child.kind() == "identifier" {
                alias_text = Some(child.utf8_text(source).unwrap_or("").to_string());
                break;
            }
        }
        alias_text.filter(|s| !s.is_empty())
    };

    if is_wildcard {
        return Some(FileImport {
            site: crate::adapter::site_from_node(node),
            module_path: full_path,
            specifiers: vec![ImportedName {
                local_name: "*".to_string(),
                original_name: None,
                is_default: false,
                // `import a.b.*` writes no type name down; the statement's
                // span is all there is to cite.
                site: None,
            }],
        });
    }

    // Split qualified path into module_path and imported name
    // Both halves are trimmed, for the reason the Java adapter trims its own:
    // a qualified name may be written across lines, and an untrimmed split
    // records a type name carrying the indentation in front of it.
    if let Some(dot_pos) = full_path.rfind('.') {
        let module_path = full_path[..dot_pos].trim().to_string();
        let original_name = full_path[dot_pos + 1..].trim().to_string();
        if original_name.is_empty() {
            return None;
        }
        let has_alias = alias.is_some();
        let local_name = alias.unwrap_or_else(|| original_name.clone());
        Some(FileImport {
            site: crate::adapter::site_from_node(node),
            module_path,
            specifiers: vec![ImportedName {
                local_name,
                original_name: if has_alias { Some(original_name) } else { None },
                is_default: false,
                site: Some(name_site),
            }],
        })
    } else {
        let local_name = alias.unwrap_or_else(|| full_path.clone());
        Some(FileImport {
            site: crate::adapter::site_from_node(node),
            module_path: String::new(),
            specifiers: vec![ImportedName {
                local_name,
                original_name: None,
                is_default: false,
                site: Some(name_site),
            }],
        })
    }
}

/// Recursively detect @Test annotated functions in Kotlin source.
fn extract_kotlin_tests(node: &tree_sitter::Node, source: &[u8], tests: &mut Vec<ExtractedTest>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "function_declaration" && has_test_annotation(&child, source) {
            if let Some(name) = find_named_child_text(&child, "identifier", source) {
                if !name.is_empty() {
                    tests.push(ExtractedTest {
                        name,
                        kind: ExtractedTestKind::Unit,
                        runner: "junit".to_string(),
                    });
                }
            }
        }
        // Recurse into class bodies, etc.
        extract_kotlin_tests(&child, source, tests);
    }
}

/// Check if a function has an @Test annotation.
fn has_test_annotation(node: &tree_sitter::Node, source: &[u8]) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "modifiers" {
            let mut mod_cursor = child.walk();
            for m in child.children(&mut mod_cursor) {
                if m.kind() == "annotation" || m.kind() == "single_annotation" {
                    let text = m.utf8_text(source).unwrap_or("");
                    if text.contains("Test") {
                        return true;
                    }
                }
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_kotlin_class() {
        let adapter = KotlinAdapter;
        let source = b"class Dog { fun bark() {} }";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("Dog.kt");
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
    fn parse_kotlin_interface() {
        let adapter = KotlinAdapter;
        let source = b"interface Runnable { fun run() }";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("Runnable.kt");
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
    fn parse_kotlin_data_class() {
        let adapter = KotlinAdapter;
        let source = b"data class Point(val x: Int, val y: Int)";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("Point.kt");
        let output = adapter.extract(&tree, source, &file_id).unwrap();
        let classes: Vec<_> = output
            .entities
            .iter()
            .filter(|e| e.kind == EntityKind::Class)
            .collect();
        assert_eq!(classes.len(), 1);
        assert_eq!(classes[0].name, "Point");
    }

    #[test]
    fn parse_kotlin_object() {
        let adapter = KotlinAdapter;
        let source = b"object Singleton { fun instance() {} }";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("Singleton.kt");
        let output = adapter.extract(&tree, source, &file_id).unwrap();
        let objects: Vec<_> = output
            .entities
            .iter()
            .filter(|e| e.kind == EntityKind::Class)
            .collect();
        assert_eq!(objects.len(), 1);
        assert_eq!(objects[0].name, "Singleton");
    }

    #[test]
    fn parse_kotlin_function() {
        let adapter = KotlinAdapter;
        let source = b"fun add(a: Int, b: Int): Int = a + b";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("math.kt");
        let output = adapter.extract(&tree, source, &file_id).unwrap();
        let funcs: Vec<_> = output
            .entities
            .iter()
            .filter(|e| e.kind == EntityKind::Function)
            .collect();
        assert_eq!(funcs.len(), 1);
        assert_eq!(funcs[0].name, "add");
    }

    #[test]
    fn parse_kotlin_method_calls() {
        let adapter = KotlinAdapter;
        let source = b"class App { fun run() { println(\"hi\"); doWork() } }";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("App.kt");
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
        assert!(dst_names.contains(&"println"));
        assert!(dst_names.contains(&"doWork"));
    }

    #[test]
    fn parse_kotlin_imports() {
        let adapter = KotlinAdapter;
        let source = b"import kotlin.collections.List\nimport java.io.File\n\nfun main() {}";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("App.kt");
        let output = adapter.extract(&tree, source, &file_id).unwrap();
        assert_eq!(output.imports.len(), 2);

        let list_import = output
            .imports
            .iter()
            .find(|i| i.module_path == "kotlin.collections")
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
    fn parse_kotlin_visibility() {
        let adapter = KotlinAdapter;
        let source =
            b"public fun publicFunc() {}\nprivate fun privateFunc() {}\nfun defaultFunc() {}";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("vis.kt");
        let output = adapter.extract(&tree, source, &file_id).unwrap();
        let funcs: Vec<_> = output
            .entities
            .iter()
            .filter(|e| e.kind == EntityKind::Function)
            .collect();
        assert_eq!(funcs.len(), 3);

        let public_fn = funcs.iter().find(|f| f.name == "publicFunc").unwrap();
        assert_eq!(public_fn.visibility, Visibility::Public);

        let private_fn = funcs.iter().find(|f| f.name == "privateFunc").unwrap();
        assert_eq!(private_fn.visibility, Visibility::Private);

        // Kotlin default visibility is public
        let default_fn = funcs.iter().find(|f| f.name == "defaultFunc").unwrap();
        assert_eq!(default_fn.visibility, Visibility::Public);
    }

    /// A file whose bytes declare nothing mints no module surface.
    ///
    /// The surface is synthetic: it stands for the file, not for anything the
    /// file wrote. Minting it unconditionally made a comment-only or unreadable
    /// file count as parsed, so the parse-coverage census read a clean row for a
    /// repository holding a hole and could not name the file. Rust carried this
    /// rule already; these adapters did not.
    #[test]
    fn comment_only_kotlin_file_mints_no_module_surface() {
        let adapter = KotlinAdapter;
        let source = b"// nothing is declared here\n";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("lib/Silent.kt");
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
    fn kotlin_file_with_one_declaration_mints_the_module_surface() {
        let adapter = KotlinAdapter;
        let source = b"package com.example.storage\n\nclass Dog { fun bark() {} }\n";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("lib/Dog.kt");
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
    fn kotlin_file_with_only_an_import_mints_the_module_surface() {
        let adapter = KotlinAdapter;
        let source = b"import kotlin.collections.List\n";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("lib/Dog.kt");
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
    fn package_declaration_only_kotlin_file_mints_no_module_surface() {
        let adapter = KotlinAdapter;
        let source = b"package com.example.storage\n";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("lib/Dog.kt");
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
    fn direct_this_calls_keep_owner_but_receiver_scopes_do_not_borrow_it() {
        let source = br#"class Outer {
            fun ping() {}
            fun run(other: Other) {
                this.ping()
                other.ping()
                other.apply { this.ping() }
            }
            fun Other.extend() { this.ping() }
        }
        class Other { fun ping() {}
        }
        "#;
        let adapter = KotlinAdapter;
        let tree = adapter.parse(source).unwrap();
        assert!(
            !tree.root_node().has_error(),
            "{}",
            tree.root_node().to_sexp()
        );
        let output = adapter
            .extract(&tree, source, &FilePathId::new("Outer.kt"))
            .unwrap();
        let calls: Vec<_> = output
            .relations
            .iter()
            .filter(|r| r.kind == kin_model::RelationKind::Calls)
            .collect();
        assert_eq!(
            calls.iter().filter(|r| r.dst_name == "Outer.ping").count(),
            1,
            "{calls:?}; {}",
            tree.root_node().to_sexp()
        );
        assert_eq!(
            calls.iter().filter(|r| r.dst_name == "ping").count(),
            3,
            "{calls:?}"
        );
        assert!(calls.iter().all(|r| r.site.is_some()));
    }
}
