// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

use kin_model::{EntityKind, FilePathId, LanguageId, ParseState, Visibility};
use tree_sitter::Tree;

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
use crate::value_origins::swift::{SwiftFileTypes, SwiftScopeReader};

pub struct SwiftAdapter;

impl LanguageAdapter for SwiftAdapter {
    fn language_id(&self) -> LanguageId {
        LanguageId::Swift
    }

    fn file_extensions(&self) -> &[&str] {
        &["swift"]
    }

    fn parse(&self, source: &[u8]) -> Result<Tree> {
        let mut parser = make_parser(&tree_sitter_swift::LANGUAGE)?;
        parser
            .parse(source, None)
            .ok_or_else(|| crate::error::ParseError::ParseFailed {
                file: String::new(),
                reason: "tree-sitter returned None".into(),
            })
    }

    fn declaration_census(
        &self,
        tree: &Tree,
        source: &[u8],
    ) -> Option<Vec<crate::declaration_census::SeenDeclaration>> {
        Some(super::swift_declarations::census(tree, source))
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
        let types = SwiftFileTypes::read(root, source);
        let mut nested = SwiftNestedState::default();
        let mut cursor = root.walk();

        for child in root.children(&mut cursor) {
            extract_swift_node(
                &child,
                source,
                file_id,
                &types,
                None,
                &mut entities,
                &mut relations,
                &mut nested,
            );
            if child.kind() == "import_declaration" {
                if let Some(file_import) = extract_swift_import(&child, source) {
                    imports.push(file_import);
                }
            }
        }

        // The file module owns entity-level import edges, and it has to sit
        // first so `module_entity_by_file` reads it rather than a declaration
        // that happens to sit above the rest. A file that declared nothing and
        // imported nothing contributes no entity: minting the synthetic module
        // there reported the file as parsed, which is what kept a comment-only
        // or unreadable file out of the parse-coverage census.
        if !entities.is_empty() || !imports.is_empty() {
            if let Some(module_name) = crate::adapter::file_module_surface_name(None, file_id) {
                entities.insert(
                    0,
                    crate::adapter::file_module_surface_entity(
                        module_name,
                        format!("module {}", file_id.0),
                        &root,
                        source,
                        file_id,
                    ),
                );
            }
        }

        // Calls no callable's walk reached (top-level code, a global or stored
        // property's initializer, a closure passed there) go to the innermost
        // entity holding each one: the property, else the type, else the
        // file's module surface.
        {
            let accounted = crate::call_owner::accounted_call_sites(&relations);
            let mut swept = Vec::new();
            let reader = SwiftScopeReader::for_callable(root, source, None, &types);
            let scope = CallScope {
                owner: None,
                reader: &reader,
                types: &types,
            };
            collect_calls(
                &root,
                source,
                "",
                &scope,
                &NestedCalls::default(),
                &mut swept,
            );
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
                } else if let Some(prefix) = rel.dst_name.split('.').next() {
                    if let Some(&module) = import_map.get(prefix) {
                        rel.import_source = Some(module.to_string());
                    }
                }
            }
        }

        // Detect XCTest methods (func testXxx()) and @Test-annotated functions
        let mut tests = Vec::new();
        for ent in &entities {
            if ent.kind == EntityKind::Method && ent.name.contains('.') {
                let method_name = ent.name.rsplit('.').next().unwrap_or(&ent.name);
                if method_name.starts_with("test") {
                    tests.push(ExtractedTest {
                        name: ent.name.clone(),
                        kind: ExtractedTestKind::Unit,
                        runner: "xctest".to_string(),
                    });
                }
            }
        }

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
            nested_definitions: Some(nested.census),
        })
    }
}

#[allow(clippy::too_many_arguments)]
fn extract_swift_node(
    node: &tree_sitter::Node,
    source: &[u8],
    file_id: &FilePathId,
    types: &SwiftFileTypes,
    class_ctx: Option<&str>,
    entities: &mut Vec<ExtractedEntity>,
    relations: &mut Vec<ExtractedRelation>,
    nested: &mut SwiftNestedState,
) {
    match node.kind() {
        "class_declaration" => {
            // tree-sitter-swift uses class_declaration for class, struct, enum,
            // extension, and actor. The declaration_kind field distinguishes them.
            let decl_kind = node
                .child_by_field_name("declaration_kind")
                .map(|n| n.kind().to_string())
                .unwrap_or_default();

            if let Some(name_node) = node.child_by_field_name("name") {
                let name = extract_name_text(&name_node, source);
                if name.is_empty() {
                    return;
                }

                let entity_kind = match decl_kind.as_str() {
                    "class" => EntityKind::Class,
                    "struct" => EntityKind::Class,
                    "enum" => EntityKind::EnumDef,
                    "actor" => EntityKind::Class,
                    "extension" => {
                        // Extensions add methods to existing types; emit as Module
                        // and recurse into body with the extended type as context.
                        let body = find_child_by_kind(node, "class_body");
                        if let Some(ref body_node) = body {
                            let mut body_cursor = body_node.walk();
                            for member in body_node.children(&mut body_cursor) {
                                extract_swift_node(
                                    &member,
                                    source,
                                    file_id,
                                    types,
                                    Some(&name),
                                    entities,
                                    relations,
                                    nested,
                                );
                            }
                        }
                        return;
                    }
                    _ => EntityKind::Class,
                };
                // A type declared inside a type is named under it
                // (`Point.Inner`), as its members are, so two types'
                // same-named nested types cannot collide in one unit. An
                // extension names the type it extends and is not renamed.
                let name = match class_ctx {
                    Some(owner) => format!("{owner}.{name}"),
                    None => name,
                };

                let vis = detect_swift_visibility(node, source);
                entities.push(ExtractedEntity {
                    kind: entity_kind,
                    name: name.clone(),
                    signature: node_signature(node, source),
                    visibility: vis,
                    doc_summary: extract_preceding_comment(node, source),
                    fingerprint: compute_fingerprint(node, source),
                    span: span_from_node(node, file_id),
                    declaration_line: None,
                });

                // Extract inheritance (superclass, protocol conformance)
                extract_inheritance(node, source, &name, relations);

                // Recurse into class/struct/enum body
                let body_kind = if decl_kind == "enum" {
                    "enum_class_body"
                } else {
                    "class_body"
                };
                if let Some(body_node) = find_child_by_kind(node, body_kind) {
                    let mut body_cursor = body_node.walk();
                    for member in body_node.children(&mut body_cursor) {
                        extract_swift_node(
                            &member,
                            source,
                            file_id,
                            types,
                            Some(&name),
                            entities,
                            relations,
                            nested,
                        );
                    }
                }
            }
        }
        "protocol_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = extract_name_text(&name_node, source);
                if name.is_empty() {
                    return;
                }

                let vis = detect_swift_visibility(node, source);
                entities.push(ExtractedEntity {
                    kind: EntityKind::Interface,
                    name: name.clone(),
                    signature: node_signature(node, source),
                    visibility: vis,
                    doc_summary: extract_preceding_comment(node, source),
                    fingerprint: compute_fingerprint(node, source),
                    span: span_from_node(node, file_id),
                    declaration_line: None,
                });

                // Protocol inheritance
                extract_inheritance(node, source, &name, relations);

                // Recurse into protocol body for method declarations
                if let Some(body_node) = find_child_by_kind(node, "protocol_body") {
                    let mut body_cursor = body_node.walk();
                    for member in body_node.children(&mut body_cursor) {
                        extract_swift_node(
                            &member,
                            source,
                            file_id,
                            types,
                            Some(&name),
                            entities,
                            relations,
                            nested,
                        );
                    }
                }
            }
        }
        "function_declaration" | "protocol_function_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let func_name = extract_name_text(&name_node, source);
                if func_name.is_empty() {
                    return;
                }

                let (kind, qualified) = if let Some(cls) = class_ctx {
                    (EntityKind::Method, format!("{}.{}", cls, func_name))
                } else {
                    (EntityKind::Function, func_name)
                };

                let vis = detect_swift_visibility(node, source);
                entities.push(ExtractedEntity {
                    kind,
                    name: qualified.clone(),
                    signature: node_signature(node, source),
                    visibility: vis,
                    doc_summary: extract_preceding_comment(node, source),
                    fingerprint: compute_fingerprint(node, source),
                    span: span_from_node(node, file_id),
                    declaration_line: None,
                });

                if let Some(cls) = class_ctx {
                    relations.push(ExtractedRelation {
                        site: None,
                        receiver: None,
                        call_shape: None,
                        kind: kin_model::RelationKind::Contains,
                        src_name: cls.to_string(),
                        dst_name: qualified.clone(),
                        import_source: None,
                    });
                }

                extract_callable_body(
                    node, source, file_id, types, &qualified, class_ctx, entities, relations,
                    nested,
                );
            }
        }
        "init_declaration" => {
            let init_name = if let Some(cls) = class_ctx {
                format!("{}.init", cls)
            } else {
                "init".to_string()
            };

            entities.push(ExtractedEntity {
                kind: EntityKind::Method,
                name: init_name.clone(),
                signature: node_signature(node, source),
                visibility: detect_swift_visibility(node, source),
                doc_summary: extract_preceding_comment(node, source),
                fingerprint: compute_fingerprint(node, source),
                span: span_from_node(node, file_id),
                declaration_line: None,
            });

            if let Some(cls) = class_ctx {
                relations.push(ExtractedRelation {
                    site: None,
                    receiver: None,
                    call_shape: None,
                    kind: kin_model::RelationKind::Contains,
                    src_name: cls.to_string(),
                    dst_name: init_name.clone(),
                    import_source: None,
                });
            }

            extract_callable_body(
                node, source, file_id, types, &init_name, class_ctx, entities, relations, nested,
            );
        }
        "deinit_declaration" => {
            if let Some(cls) = class_ctx {
                let deinit_name = format!("{}.deinit", cls);
                entities.push(ExtractedEntity {
                    kind: EntityKind::Method,
                    name: deinit_name.clone(),
                    signature: "deinit".to_string(),
                    visibility: Visibility::Internal,
                    doc_summary: extract_preceding_comment(node, source),
                    fingerprint: compute_fingerprint(node, source),
                    span: span_from_node(node, file_id),
                    declaration_line: None,
                });

                relations.push(ExtractedRelation {
                    site: None,
                    receiver: None,
                    call_shape: None,
                    kind: kin_model::RelationKind::Contains,
                    src_name: cls.to_string(),
                    dst_name: deinit_name.clone(),
                    import_source: None,
                });
                extract_callable_body(
                    node,
                    source,
                    file_id,
                    types,
                    &deinit_name,
                    class_ctx,
                    entities,
                    relations,
                    nested,
                );
            }
        }
        "property_declaration" | "protocol_property_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let prop_name = extract_pattern_name(&name_node, source);
                if prop_name.is_empty() {
                    return;
                }

                let is_constant = is_let_binding(&name_node, source);
                let qualified = if let Some(cls) = class_ctx {
                    format!("{}.{}", cls, prop_name)
                } else {
                    prop_name
                };

                let kind = if class_ctx.is_none() && is_constant {
                    EntityKind::Constant
                } else {
                    EntityKind::StaticVar
                };

                entities.push(ExtractedEntity {
                    kind,
                    name: qualified.clone(),
                    signature: node_signature(node, source),
                    visibility: detect_swift_visibility(node, source),
                    doc_summary: extract_preceding_comment(node, source),
                    fingerprint: compute_fingerprint(node, source),
                    span: span_from_node(node, file_id),
                    declaration_line: None,
                });

                if let Some(cls) = class_ctx {
                    relations.push(ExtractedRelation {
                        site: None,
                        receiver: None,
                        call_shape: None,
                        kind: kin_model::RelationKind::Contains,
                        src_name: cls.to_string(),
                        dst_name: qualified,
                        import_source: None,
                    });
                }
            }
        }
        "typealias_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = extract_name_text(&name_node, source);
                if !name.is_empty() {
                    entities.push(ExtractedEntity {
                        kind: EntityKind::TypeAlias,
                        name,
                        signature: node_signature(node, source),
                        visibility: detect_swift_visibility(node, source),
                        doc_summary: extract_preceding_comment(node, source),
                        fingerprint: compute_fingerprint(node, source),
                        span: span_from_node(node, file_id),
                        declaration_line: None,
                    });
                }
            }
        }
        "enum_entry" => {
            // `case fast, safe` declares two cases, each its own name.
            let mut names = node.walk();
            let name_nodes: Vec<tree_sitter::Node> =
                node.children_by_field_name("name", &mut names).collect();
            for name_node in name_nodes {
                let case_name = extract_name_text(&name_node, source);
                if case_name.is_empty() {
                    continue;
                }
                let qualified = if let Some(cls) = class_ctx {
                    format!("{}.{}", cls, case_name)
                } else {
                    case_name
                };

                entities.push(ExtractedEntity {
                    kind: EntityKind::Constant,
                    name: qualified.clone(),
                    signature: node_signature(node, source),
                    visibility: detect_swift_visibility(node, source),
                    doc_summary: None,
                    fingerprint: compute_fingerprint(node, source),
                    span: span_from_node(node, file_id),
                    declaration_line: None,
                });

                if let Some(cls) = class_ctx {
                    relations.push(ExtractedRelation {
                        site: None,
                        receiver: None,
                        call_shape: None,
                        kind: kin_model::RelationKind::Contains,
                        src_name: cls.to_string(),
                        dst_name: qualified,
                        import_source: None,
                    });
                }
            }
        }
        "import_declaration" => {}
        _ => {}
    }
}

/// Extract the text name from a node, handling both simple_identifier
/// and type_identifier / user_type nodes.
fn extract_name_text(node: &tree_sitter::Node, source: &[u8]) -> String {
    match node.kind() {
        "simple_identifier" | "type_identifier" => node.utf8_text(source).unwrap_or("").to_string(),
        _ => {
            // For composite nodes (user_type, etc.), try to find the first
            // simple_identifier or type_identifier child.
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "simple_identifier" || child.kind() == "type_identifier" {
                    return child.utf8_text(source).unwrap_or("").to_string();
                }
            }
            // Fallback: use the node's own text
            node.utf8_text(source).unwrap_or("").to_string()
        }
    }
}

/// Extract the bound variable name from a pattern node.
/// Swift property_declaration has name -> pattern -> value_binding_pattern -> bound_identifier.
fn extract_pattern_name(node: &tree_sitter::Node, source: &[u8]) -> String {
    // Direct simple_identifier
    if node.kind() == "simple_identifier" {
        return node.utf8_text(source).unwrap_or("").to_string();
    }

    // Look for bound_identifier field or simple_identifier children recursively
    if let Some(bound) = node.child_by_field_name("bound_identifier") {
        return extract_name_text(&bound, source);
    }

    // Walk children to find simple_identifier
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "simple_identifier" {
            return child.utf8_text(source).unwrap_or("").to_string();
        }
        let found = extract_pattern_name(&child, source);
        if !found.is_empty() {
            return found;
        }
    }

    String::new()
}

/// Check if a pattern node represents a let binding (constant).
fn is_let_binding(node: &tree_sitter::Node, source: &[u8]) -> bool {
    // Look for value_binding_pattern with mutability field = "let"
    if node.kind() == "value_binding_pattern" {
        if let Some(mutability) = node.child_by_field_name("mutability") {
            return mutability.utf8_text(source).unwrap_or("") == "let";
        }
    }
    // Check parent signature text as fallback
    if let Some(parent) = node.parent() {
        let text = parent.utf8_text(source).unwrap_or("");
        return text.trim_start().starts_with("let ");
    }
    false
}

/// Extract inheritance/conformance from a class, struct, enum, or protocol declaration.
fn extract_inheritance(
    node: &tree_sitter::Node,
    source: &[u8],
    type_name: &str,
    relations: &mut Vec<ExtractedRelation>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "type_constraints" || child.kind() == "inheritance_specifier" {
            extract_type_names_from_inheritance(&child, source, type_name, relations);
        }
    }
}

fn extract_type_names_from_inheritance(
    node: &tree_sitter::Node,
    source: &[u8],
    type_name: &str,
    relations: &mut Vec<ExtractedRelation>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "user_type" || child.kind() == "type_identifier" {
            let parent_name = extract_name_text(&child, source);
            if !parent_name.is_empty() && parent_name != type_name {
                // In Swift, the first item in the inheritance list for a class
                // could be either a superclass or a protocol. Without type resolution
                // we emit Implements (protocol conformance) for all, which is the
                // safer assumption.
                relations.push(ExtractedRelation {
                    site: None,
                    receiver: None,
                    call_shape: None,
                    kind: kin_model::RelationKind::Implements,
                    src_name: type_name.to_string(),
                    dst_name: parent_name,
                    import_source: None,
                });
            }
        }
        // Recurse for nested type nodes
        if child.kind() != "user_type" && child.kind() != "type_identifier" {
            extract_type_names_from_inheritance(&child, source, type_name, relations);
        }
    }
}

fn detect_swift_visibility(node: &tree_sitter::Node, source: &[u8]) -> Visibility {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "modifiers" {
            let mut mod_cursor = child.walk();
            for modifier in child.children(&mut mod_cursor) {
                if modifier.kind() == "visibility_modifier" {
                    let text = modifier.utf8_text(source).unwrap_or("");
                    return match text {
                        "public" | "open" => Visibility::Public,
                        "private" | "fileprivate" => Visibility::Private,
                        _ => Visibility::Internal,
                    };
                }
            }
        }
    }
    // Swift default visibility is internal
    Visibility::Internal
}

fn node_signature(node: &tree_sitter::Node, source: &[u8]) -> String {
    crate::adapter::declaration_signature(node, source)
}

fn extract_preceding_comment(node: &tree_sitter::Node, source: &[u8]) -> Option<String> {
    let prev = node.prev_sibling()?;
    if prev.kind() == "comment" || prev.kind() == "multiline_comment" {
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

fn find_child_by_kind<'a>(
    node: &'a tree_sitter::Node,
    kind: &str,
) -> Option<tree_sitter::Node<'a>> {
    let mut cursor = node.walk();
    let result = node.children(&mut cursor).find(|c| c.kind() == kind);
    result
}

/// Walk a function, method, initializer or deinitializer body: admit what it
/// defines and emit its calls.
///
/// A call through `self` is emitted owner-qualified (`Store.save`), the shape
/// the linker's inheritance-aware tier reads. A call through a receiver with
/// a value-origin fact the linker can follow (a declared type, an
/// initializer, a same-file function's declared return, a method called on
/// one of those, or the enclosing type's property) carries its receiver and
/// the bare method, so the linker's value-origin tier binds it to the class
/// the declarations name. Every other call keeps the bare rightmost name.
///
/// A function declared in the body becomes `F.<locals>.helper` and a type
/// declared there `F.<locals>.Local` with its members (see
/// [`crate::nested`]); their calls move onto them, and a bare call of the
/// function's name in its scope reaches it.
#[allow(clippy::too_many_arguments)]
fn extract_callable_body(
    node: &tree_sitter::Node,
    source: &[u8],
    file_id: &FilePathId,
    types: &SwiftFileTypes,
    context_name: &str,
    class_ctx: Option<&str>,
    entities: &mut Vec<ExtractedEntity>,
    relations: &mut Vec<ExtractedRelation>,
    nested: &mut SwiftNestedState,
) {
    let reader = SwiftScopeReader::for_callable(*node, source, class_ctx, types);
    let scope = CallScope {
        owner: class_ctx,
        reader: &reader,
        types,
    };
    admit_nested(
        node,
        source,
        file_id,
        types,
        context_name,
        &scope,
        entities,
        relations,
        nested,
    );
}

/// Admit what `node`'s body defines and emit the calls `node` makes itself,
/// read against `scope`, the outermost callable's declarations: a nested
/// function reads its enclosing function's locals.
#[allow(clippy::too_many_arguments)]
fn admit_nested(
    node: &tree_sitter::Node,
    source: &[u8],
    file_id: &FilePathId,
    types: &SwiftFileTypes,
    context_name: &str,
    scope: &CallScope<'_>,
    entities: &mut Vec<ExtractedEntity>,
    relations: &mut Vec<ExtractedRelation>,
    nested: &mut SwiftNestedState,
) {
    let plan = super::swift_nested::plan(*node, source);
    let mut calls = NestedCalls::default();
    let mut functions = Vec::new();
    for function in &plan.functions {
        let entity = local_scope_name(context_name, &function.name);
        let line = function.node.start_position().row as u32;
        if !nested.names.claim(&entity) {
            nested.decline(
                &entity,
                EntityKind::Function,
                line,
                context_name,
                Vec::new(),
            );
            continue;
        }
        calls
            .skip
            .push((function.node.start_byte(), function.node.end_byte()));
        if let Some((from, until)) = function.scope {
            calls
                .renames
                .push((function.name.clone(), from, until, entity.clone()));
        }
        functions.push((entity, function.node));
    }
    let mut local_types = Vec::new();
    for (name, type_node, (from, until)) in &plan.types {
        let entity = local_scope_name(context_name, name);
        let line = type_node.start_position().row as u32;
        if !nested.names.claim(&entity) {
            nested.decline(&entity, EntityKind::Class, line, context_name, Vec::new());
            continue;
        }
        calls
            .skip
            .push((type_node.start_byte(), type_node.end_byte()));
        calls
            .renames
            .push((name.clone(), *from, *until, entity.clone()));
        local_types.push((entity, *type_node));
    }
    for (name, kind, line) in &plan.declined {
        let entity = local_scope_name(context_name, name);
        nested.decline(&entity, *kind, *line, context_name, Vec::new());
    }

    collect_calls(node, source, context_name, scope, &calls, relations);

    for (entity, function) in functions {
        entities.push(ExtractedEntity {
            kind: EntityKind::Function,
            name: entity.clone(),
            signature: node_signature(&function, source),
            visibility: Visibility::Private,
            doc_summary: extract_preceding_comment(&function, source),
            fingerprint: compute_fingerprint(&function, source),
            span: span_from_node(&function, file_id),
            declaration_line: None,
        });
        admit_nested(
            &function, source, file_id, types, &entity, scope, entities, relations, nested,
        );
    }
    for (entity, type_node) in local_types {
        let kind = match type_node
            .child_by_field_name("declaration_kind")
            .map(|kind| kind.kind())
        {
            Some("enum") => EntityKind::EnumDef,
            _ => EntityKind::Class,
        };
        entities.push(ExtractedEntity {
            kind,
            name: entity.clone(),
            signature: node_signature(&type_node, source),
            visibility: Visibility::Private,
            doc_summary: extract_preceding_comment(&type_node, source),
            fingerprint: compute_fingerprint(&type_node, source),
            span: span_from_node(&type_node, file_id),
            declaration_line: None,
        });
        extract_inheritance(&type_node, source, &entity, relations);
        let Some(body) = crate::value_origins::swift::type_body(type_node) else {
            continue;
        };
        let mut cursor = body.walk();
        for member in body.children(&mut cursor) {
            if matches!(member.kind(), "class_declaration" | "protocol_declaration") {
                if let Some(name) =
                    crate::value_origins::swift::type_declaration_name(member, source)
                {
                    nested.decline(
                        &format!("{entity}.{name}"),
                        EntityKind::Class,
                        member.start_position().row as u32,
                        &entity,
                        Vec::new(),
                    );
                }
                continue;
            }
            extract_swift_node(
                &member,
                source,
                file_id,
                types,
                Some(&entity),
                entities,
                relations,
                nested,
            );
        }
    }
}

/// The definitions admitted inside one callable body: the byte ranges whose
/// calls belong to them, and the local names a bare call reaches them by.
#[derive(Default)]
struct NestedCalls {
    skip: Vec<(usize, usize)>,
    /// (name, visible from, visible until, entity name).
    renames: Vec<(String, usize, usize, String)>,
}

impl NestedCalls {
    fn skips(&self, node: &tree_sitter::Node) -> bool {
        self.skip
            .iter()
            .any(|&(start, end)| node.start_byte() == start && node.end_byte() == end)
    }

    fn renamed(&self, name: &str, at: usize) -> Option<&str> {
        self.renames
            .iter()
            .find(|(bound, from, until, _)| bound == name && *from <= at && at < *until)
            .map(|(_, _, _, entity)| entity.as_str())
    }
}

/// What the walk accumulates about definitions nested in callable bodies.
#[derive(Default)]
struct SwiftNestedState {
    census: NestedDefinitionCensus,
    names: LocalScopeNames,
}

impl SwiftNestedState {
    fn decline(
        &mut self,
        name: &str,
        kind: EntityKind,
        line: u32,
        enclosing: &str,
        bases: Vec<String>,
    ) {
        self.census.record(UnadmittedNestedDefinition {
            name: name.to_string(),
            kind,
            start_line: line,
            enclosing: Some(enclosing.to_string()),
            bases,
        });
    }
}

/// What the calls of one callable body are read against.
struct CallScope<'a> {
    owner: Option<&'a str>,
    reader: &'a SwiftScopeReader,
    types: &'a SwiftFileTypes,
}

fn collect_calls(
    node: &tree_sitter::Node,
    source: &[u8],
    context_name: &str,
    scope: &CallScope<'_>,
    nested: &NestedCalls,
    relations: &mut Vec<ExtractedRelation>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if nested.skips(&child) {
            continue;
        }
        if child.kind() == "call_expression" {
            if let Some((callee, callee_node)) = extract_callee(&child, source) {
                if !callee.is_empty() {
                    let (dst_name, receiver) = match (callee_node.kind() == "simple_identifier")
                        .then(|| nested.renamed(&callee, callee_node.start_byte()))
                        .flatten()
                    {
                        Some(entity) => (entity.to_string(), None),
                        None => call_shape(&callee, &callee_node, source, scope),
                    };
                    relations.push(ExtractedRelation {
                        // The call as written, so a reference row can report the
                        // line it sits on. Without it the linker has no span to
                        // store and every consuming surface reports the edge as
                        // having no evidence span.
                        //
                        // It runs from the callee to the end of the call rather
                        // than from the start of the `call_expression` node,
                        // because in `first + compute()` that node begins at
                        // `first`. The span then covers `compute()`, which is
                        // the use, not the whole operator expression around it.
                        site: Some(swift_call_site(&callee_node, &child)),
                        receiver,
                        call_shape: None,
                        kind: kin_model::RelationKind::Calls,
                        src_name: context_name.to_string(),
                        dst_name,
                        import_source: None,
                    });
                }
            }
        }
        collect_calls(&child, source, context_name, scope, nested, relations);
    }
}

/// The callee name and receiver one call is emitted with.
fn call_shape(
    callee: &str,
    callee_node: &tree_sitter::Node,
    source: &[u8],
    scope: &CallScope<'_>,
) -> (String, Option<String>) {
    let bare = (callee.to_string(), None);
    if callee_node.kind() != "navigation_expression" {
        return bare;
    }
    let Some(target) = callee_node.child_by_field_name("target") else {
        return bare;
    };
    match target.kind() {
        "self_expression" => match scope.owner {
            Some(owner) => (format!("{owner}.{callee}"), None),
            None => bare,
        },
        "simple_identifier" => {
            let name = target.utf8_text(source).unwrap_or("");
            if scope.reader.linker_can_follow(name, scope.types) {
                (callee.to_string(), Some(name.to_string()))
            } else {
                bare
            }
        }
        "navigation_expression" => {
            // `self.store.save()` reads the property `store` exactly as an
            // unqualified `store` does when no local shadows it.
            let through_self = target
                .child_by_field_name("target")
                .is_some_and(|inner| inner.kind() == "self_expression");
            let member = target
                .child_by_field_name("suffix")
                .and_then(|suffix| suffix.child_by_field_name("suffix"))
                .and_then(|name| name.utf8_text(source).ok())
                .unwrap_or("");
            if through_self
                && scope.reader.is_member(member)
                && scope.reader.linker_can_follow(member, scope.types)
            {
                (callee.to_string(), Some(member.to_string()))
            } else {
                bare
            }
        }
        _ => bare,
    }
}

/// Extract the callee name from a call_expression.
///
/// For navigation chains like `a.b.c()` tree-sitter-swift produces:
///   (call_expression
///     (navigation_expression
///       target: (navigation_expression
///         target: (simple_identifier "a")
///         suffix: (navigation_suffix suffix: (simple_identifier "b")))
///       suffix: (navigation_suffix suffix: (simple_identifier "c")))
///     (call_suffix ...))
/// We unwrap `suffix -> navigation_suffix -> suffix (simple_identifier)` so
/// `a.b.c()` maps to `"c"` and `obj.method()` maps to `"method"`. This mirrors
/// the Python attribute-call fix and keeps Calls edges keyed on simple names.
/// The callee of a Swift `call_expression`, with the node the name was read
/// from so the recorded site can start there.
///
/// The third arm is the one worth reading. tree-sitter-swift parses
/// `return first + compute()` as a call whose callee is the whole
/// `additive_expression`, so the call suffix binds to `first + compute` rather
/// than to `compute`. Reading only the arms above it, this function returned
/// `None` and the call reached no edge at all: `kin refs` answered that nobody
/// called a function the source calls, and the answer looked clean. Swift reads
/// that line as `first + (compute())`, so the callee is the operator
/// expression's right operand, and the same rule applies through a chain of
/// them.
fn extract_callee<'tree>(
    node: &tree_sitter::Node<'tree>,
    source: &[u8],
) -> Option<(String, tree_sitter::Node<'tree>)> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "simple_identifier" => {
                let raw = child.utf8_text(source).unwrap_or("");
                let stripped = raw.strip_prefix("self.").unwrap_or(raw);
                return Some((stripped.to_string(), child));
            }
            "navigation_expression" => {
                let name = extract_navigation_suffix_name(&child, source)?;
                return Some((name, child));
            }
            "call_suffix" | "value_arguments" | "lambda_literal" => continue,
            _ => {
                if let Some(rhs) = child.child_by_field_name("rhs") {
                    return callee_operand(&rhs, source);
                }
            }
        }
    }
    None
}

/// The callee an operator expression's right operand names, following a chain
/// of operators down to the identifier or member access at its end.
fn callee_operand<'tree>(
    node: &tree_sitter::Node<'tree>,
    source: &[u8],
) -> Option<(String, tree_sitter::Node<'tree>)> {
    match node.kind() {
        "simple_identifier" => {
            let raw = node.utf8_text(source).unwrap_or("");
            let stripped = raw.strip_prefix("self.").unwrap_or(raw);
            Some((stripped.to_string(), *node))
        }
        "navigation_expression" => {
            let name = extract_navigation_suffix_name(node, source)?;
            Some((name, *node))
        }
        _ => {
            let rhs = node.child_by_field_name("rhs")?;
            callee_operand(&rhs, source)
        }
    }
}

/// The site a Swift call records: from the callee to the end of the call.
///
/// For an ordinary `compute()` that is the `call_expression` node itself, since
/// the callee is its first token. For a call the grammar bound to an operator
/// expression it is the part that is really the call, which is what a reference
/// row should point a reader at.
fn swift_call_site(
    callee: &tree_sitter::Node,
    call: &tree_sitter::Node,
) -> crate::extract::RelationSite {
    let start = callee.start_position();
    let end = call.end_position();
    crate::extract::RelationSite {
        start_byte: callee.start_byte(),
        end_byte: call.end_byte(),
        start_line: start.row as u32,
        start_col: start.column as u32,
        end_line: end.row as u32,
        end_col: end.column as u32,
        syntactic_role: None,
    }
}

/// Pull the rightmost identifier out of a tree-sitter-swift `navigation_expression`.
/// The `suffix` field holds a `navigation_suffix` whose own `suffix` field is the
/// trailing `simple_identifier` we want.
fn extract_navigation_suffix_name(node: &tree_sitter::Node, source: &[u8]) -> Option<String> {
    let nav_suffix = node.child_by_field_name("suffix")?;
    let ident = nav_suffix.child_by_field_name("suffix")?;
    let text = ident.utf8_text(source).unwrap_or("").to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// Extract a structured import from a Swift import_declaration node.
/// Swift imports: `import Foundation`, `import class UIKit.UIView`
fn extract_swift_import(node: &tree_sitter::Node, source: &[u8]) -> Option<FileImport> {
    let text = node.utf8_text(source).unwrap_or("").trim().to_string();
    // Strip "import" keyword and optional kind specifier (class, struct, func, etc.)
    let after_import = text.strip_prefix("import")?.trim();

    // Handle `import kind Module.Symbol` form
    let module_path = match after_import.split_whitespace().count() {
        0 => return None,
        1 => after_import.to_string(),
        _ => {
            // Skip the kind specifier (class, struct, func, enum, protocol, typealias, var, let)
            let parts: Vec<&str> = after_import.splitn(2, char::is_whitespace).collect();
            let kind_keywords = [
                "class",
                "struct",
                "func",
                "enum",
                "protocol",
                "typealias",
                "var",
                "let",
            ];
            if kind_keywords.contains(&parts[0]) {
                parts.get(1).unwrap_or(&"").trim().to_string()
            } else {
                after_import.to_string()
            }
        }
    };

    if module_path.is_empty() {
        return None;
    }

    // The local name is the last component (e.g., "Foundation" from "Foundation",
    // "UIView" from "UIKit.UIView")
    let local_name = module_path
        .rsplit('.')
        .next()
        .unwrap_or(&module_path)
        .to_string();

    // The module coordinate as a node, for the specifier's own span. Swift's
    // path is read out of the statement's text rather than out of a grammar
    // field, so the node is recovered by matching that text back: the named
    // child whose text IS the coordinate. A form that exposes no such child
    // leaves the specifier unanchored and the statement's span answers, which
    // is what every Swift import reported before.
    let path_site = node
        .children(&mut node.walk())
        .filter(|child| child.is_named())
        .find(|child| child.utf8_text(source).map(str::trim) == Ok(module_path.as_str()))
        .map(|child| crate::adapter::site_from_node(&child));

    Some(FileImport {
        site: crate::adapter::site_from_node(node),
        module_path,
        specifiers: vec![ImportedName {
            local_name,
            original_name: None,
            is_default: false,
            site: path_site,
        }],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_swift_function() {
        let adapter = SwiftAdapter;
        let source = b"func add(_ a: Int, _ b: Int) -> Int { return a + b }";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("math.swift");
        let output = adapter.extract(&tree, source, &file_id).unwrap();
        assert!(matches!(output.parse_state, ParseState::Valid));
        let funcs: Vec<_> = output
            .entities
            .iter()
            .filter(|e| e.kind == EntityKind::Function)
            .collect();
        assert_eq!(funcs.len(), 1);
        assert_eq!(funcs[0].name, "add");
    }

    #[test]
    fn parse_swift_class() {
        let adapter = SwiftAdapter;
        let source = b"public class Dog {\n    var name: String = \"\"\n    func bark() { print(\"woof\") }\n}";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("Dog.swift");
        let output = adapter.extract(&tree, source, &file_id).unwrap();
        let classes: Vec<_> = output
            .entities
            .iter()
            .filter(|e| e.kind == EntityKind::Class)
            .collect();
        assert_eq!(classes.len(), 1);
        assert_eq!(classes[0].name, "Dog");
        assert_eq!(classes[0].visibility, Visibility::Public);
    }

    #[test]
    fn parse_swift_struct() {
        let adapter = SwiftAdapter;
        let source = b"struct Point {\n    let x: Double\n    let y: Double\n}";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("Point.swift");
        let output = adapter.extract(&tree, source, &file_id).unwrap();
        let structs: Vec<_> = output
            .entities
            .iter()
            .filter(|e| e.kind == EntityKind::Class)
            .collect();
        assert_eq!(structs.len(), 1);
        assert_eq!(structs[0].name, "Point");
    }

    #[test]
    fn parse_swift_protocol() {
        let adapter = SwiftAdapter;
        let source = b"protocol Drawable {\n    func draw()\n}";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("Drawable.swift");
        let output = adapter.extract(&tree, source, &file_id).unwrap();
        let protocols: Vec<_> = output
            .entities
            .iter()
            .filter(|e| e.kind == EntityKind::Interface)
            .collect();
        assert_eq!(protocols.len(), 1);
        assert_eq!(protocols[0].name, "Drawable");
    }

    #[test]
    fn parse_swift_enum() {
        let adapter = SwiftAdapter;
        let source =
            b"enum Direction {\n    case north\n    case south\n    case east\n    case west\n}";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("Direction.swift");
        let output = adapter.extract(&tree, source, &file_id).unwrap();
        let enums: Vec<_> = output
            .entities
            .iter()
            .filter(|e| e.kind == EntityKind::EnumDef)
            .collect();
        assert_eq!(enums.len(), 1);
        assert_eq!(enums[0].name, "Direction");
    }

    #[test]
    fn parse_swift_imports() {
        let adapter = SwiftAdapter;
        let source = b"import Foundation\nimport UIKit\n\nfunc main() {}";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("main.swift");
        let output = adapter.extract(&tree, source, &file_id).unwrap();
        assert_eq!(output.imports.len(), 2);

        let foundation = output
            .imports
            .iter()
            .find(|i| i.module_path == "Foundation")
            .unwrap();
        assert_eq!(foundation.specifiers[0].local_name, "Foundation");

        let uikit = output
            .imports
            .iter()
            .find(|i| i.module_path == "UIKit")
            .unwrap();
        assert_eq!(uikit.specifiers[0].local_name, "UIKit");
    }

    #[test]
    fn parse_swift_method_calls() {
        let adapter = SwiftAdapter;
        let source = b"func greet() {\n    print(\"hello\")\n    doWork()\n}";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("greet.swift");
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
        assert!(dst_names.contains(&"print"));
        assert!(dst_names.contains(&"doWork"));
    }

    #[test]
    fn parse_swift_visibility() {
        let adapter = SwiftAdapter;
        let source =
            b"public func publicFunc() {}\nprivate func privateFunc() {}\nfunc internalFunc() {}";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("vis.swift");
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

        let internal_fn = funcs.iter().find(|f| f.name == "internalFunc").unwrap();
        assert_eq!(internal_fn.visibility, Visibility::Internal);
    }

    /// A file whose bytes declare nothing mints no module surface.
    ///
    /// The surface is synthetic: it stands for the file, not for anything the
    /// file wrote. Minting it unconditionally made a comment-only or unreadable
    /// file count as parsed, so the parse-coverage census read a clean row for a
    /// repository holding a hole and could not name the file. Rust carried this
    /// rule already; these adapters did not.
    #[test]
    fn comment_only_swift_file_mints_no_module_surface() {
        let adapter = SwiftAdapter;
        let source = b"// nothing is declared here\n";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("lib/Silent.swift");
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
    fn swift_file_with_one_declaration_mints_the_module_surface() {
        let adapter = SwiftAdapter;
        let source = b"func add() {}\n";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("lib/math.swift");
        let output = adapter.extract(&tree, source, &file_id).unwrap();
        let modules: Vec<_> = output
            .entities
            .iter()
            .filter(|e| e.kind == EntityKind::Module)
            .collect();
        assert_eq!(modules.len(), 1);
        assert_eq!(modules[0].name, "math");
        assert_eq!(output.entities[0].kind, EntityKind::Module);
    }

    /// An import alone is a surface too, and the edge is sourced at the module.
    #[test]
    fn swift_file_with_only_an_import_mints_the_module_surface() {
        let adapter = SwiftAdapter;
        let source = b"import Foundation\n";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("lib/math.swift");
        let output = adapter.extract(&tree, source, &file_id).unwrap();
        assert!(!output.imports.is_empty());
        assert_eq!(output.entities[0].kind, EntityKind::Module);
    }
}
