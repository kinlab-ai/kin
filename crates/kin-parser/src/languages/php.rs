// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

use kin_model::{EntityKind, FilePathId, LanguageId, ParseState, Visibility};
use tree_sitter::Tree;

use crate::adapter::{
    collect_error_ranges, compute_fingerprint, make_parser, span_from_node, LanguageAdapter,
};
use crate::error::Result;
use std::collections::HashMap;

use crate::extract::{
    ExtractedEntity, ExtractedRelation, ExtractedTest, ExtractedTestKind, FileImport, ImportedName,
    ParseOutput,
};
use crate::nested::{
    local_scope_name, LocalScopeNames, NestedDefinitionCensus, UnadmittedNestedDefinition,
};
use crate::value_origins::php as php_origins;
use crate::value_origins::{ScopeFacts, ValueOrigin};

pub struct PhpAdapter;

impl LanguageAdapter for PhpAdapter {
    fn language_id(&self) -> LanguageId {
        LanguageId::Php
    }

    fn file_extensions(&self) -> &[&str] {
        &["php"]
    }

    fn parse(&self, source: &[u8]) -> Result<Tree> {
        let mut parser = make_parser(&tree_sitter_php::LANGUAGE_PHP)?;
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
        Some(super::php_declarations::census(tree, source))
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
        let mut cx = PhpCx::read(tree, source);
        extract_php_children(
            &root,
            source,
            file_id,
            None,
            &mut entities,
            &mut relations,
            &mut cx,
        );
        extract_php_imports(&root, source, file_id, &mut imports, &mut relations);

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
                        format!("namespace {}", file_id.0),
                        &root,
                        source,
                        file_id,
                    ),
                );
            }
        }

        // Calls no callable's walk reached (a script's top-level statements, a
        // closure passed there such as a Pest `it('...', function () {...})`
        // body) go to the innermost entity holding each one, the file's
        // namespace surface for top-level code.
        {
            let accounted = crate::call_owner::accounted_call_sites(&relations);
            let mut swept = Vec::new();
            let scope = CallScope {
                owner: None,
                base: None,
                facts: None,
                file: &cx.file,
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
                }
            }
        }

        // Detect PHPUnit test methods (methods starting with test or annotated @test)
        let mut tests = Vec::new();
        for ent in &entities {
            if ent.kind == EntityKind::Method {
                let method_name = ent.name.rsplit('.').next().unwrap_or(&ent.name);
                if method_name.starts_with("test") {
                    tests.push(ExtractedTest {
                        name: ent.name.clone(),
                        kind: ExtractedTestKind::Unit,
                        runner: "phpunit".to_string(),
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
            nested_definitions: Some(cx.nested.census),
        })
    }
}

fn extract_php_children(
    node: &tree_sitter::Node,
    source: &[u8],
    file_id: &FilePathId,
    class_ctx: Option<&str>,
    entities: &mut Vec<ExtractedEntity>,
    relations: &mut Vec<ExtractedRelation>,
    cx: &mut PhpCx,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        extract_php_node(&child, source, file_id, class_ctx, entities, relations, cx);
    }
}

fn extract_php_node(
    node: &tree_sitter::Node,
    source: &[u8],
    file_id: &FilePathId,
    class_ctx: Option<&str>,
    entities: &mut Vec<ExtractedEntity>,
    relations: &mut Vec<ExtractedRelation>,
    cx: &mut PhpCx,
) {
    match node.kind() {
        "function_definition" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = name_node.utf8_text(source).unwrap_or("").to_string();
                if !name.is_empty() {
                    entities.push(ExtractedEntity {
                        kind: EntityKind::Function,
                        name: name.clone(),
                        signature: node_signature(node, source),
                        visibility: Visibility::Public,
                        doc_summary: extract_preceding_comment(node, source),
                        fingerprint: compute_fingerprint(node, source),
                        span: span_from_node(node, file_id),
                        declaration_line: None,
                    });
                    extract_callable_body(
                        node, source, file_id, &name, None, entities, relations, cx,
                    );
                }
            }
        }
        "method_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let method_name = name_node.utf8_text(source).unwrap_or("").to_string();
                if !method_name.is_empty() {
                    let qualified = if let Some(cls) = class_ctx {
                        format!("{}.{}", cls, method_name)
                    } else {
                        method_name
                    };
                    let vis = detect_php_visibility(node, source);
                    entities.push(ExtractedEntity {
                        kind: EntityKind::Method,
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
                    // A constructor parameter with a visibility declares a
                    // property of its class too (constructor promotion).
                    if let (Some(cls), Some(parameters)) =
                        (class_ctx, node.child_by_field_name("parameters"))
                    {
                        let mut cursor = parameters.walk();
                        for parameter in parameters.children(&mut cursor) {
                            if parameter.kind() == "property_promotion_parameter" {
                                push_php_field(
                                    &parameter, &parameter, &parameter, cls, source, file_id,
                                    entities, relations,
                                );
                            }
                        }
                    }
                    extract_callable_body(
                        node, source, file_id, &qualified, class_ctx, entities, relations, cx,
                    );
                }
            }
        }
        "class_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = name_node.utf8_text(source).unwrap_or("").to_string();
                if !name.is_empty() {
                    entities.push(ExtractedEntity {
                        kind: EntityKind::Class,
                        name: name.clone(),
                        signature: node_signature(node, source),
                        visibility: Visibility::Public,
                        doc_summary: extract_preceding_comment(node, source),
                        fingerprint: compute_fingerprint(node, source),
                        span: span_from_node(node, file_id),
                        declaration_line: None,
                    });

                    // Extract base class (extends)
                    // `base_clause` is a child kind, not a field: looking it
                    // up as a field found nothing, and no PHP class had an
                    // `Extends` edge.
                    if let Some(base) = child_of_kind(node, "base_clause") {
                        let base_text = base.utf8_text(source).unwrap_or("").to_string();
                        let base_name = base_text
                            .trim()
                            .trim_start_matches("extends")
                            .trim()
                            .split(',')
                            .next()
                            .unwrap_or("")
                            .trim()
                            .to_string();
                        if !base_name.is_empty() {
                            relations.push(ExtractedRelation {
                                site: None,
                                receiver: None,
                                call_shape: None,
                                kind: kin_model::RelationKind::Extends,
                                src_name: name.clone(),
                                dst_name: base_name,
                                import_source: None,
                            });
                        }
                    }

                    // Extract interfaces (implements)
                    extract_php_implements(node, source, &name, relations);

                    // A trait a class uses hands it the trait's methods, which
                    // the class's own methods override: an inherited
                    // implementation, so an `Extends` edge.
                    if let Some(body) = node.child_by_field_name("body") {
                        extract_php_trait_uses(&body, source, &name, relations);
                    }

                    // Recurse into class body
                    if let Some(body) = node.child_by_field_name("body") {
                        extract_php_children(
                            &body,
                            source,
                            file_id,
                            Some(&name),
                            entities,
                            relations,
                            cx,
                        );
                    }
                }
            }
        }
        "interface_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = name_node.utf8_text(source).unwrap_or("").to_string();
                if !name.is_empty() {
                    entities.push(ExtractedEntity {
                        kind: EntityKind::Interface,
                        name: name.clone(),
                        signature: node_signature(node, source),
                        visibility: Visibility::Public,
                        doc_summary: extract_preceding_comment(node, source),
                        fingerprint: compute_fingerprint(node, source),
                        span: span_from_node(node, file_id),
                        declaration_line: None,
                    });

                    // Recurse into interface body
                    if let Some(body) = node.child_by_field_name("body") {
                        extract_php_children(
                            &body,
                            source,
                            file_id,
                            Some(&name),
                            entities,
                            relations,
                            cx,
                        );
                    }
                }
            }
        }
        "trait_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = name_node.utf8_text(source).unwrap_or("").to_string();
                if !name.is_empty() {
                    entities.push(ExtractedEntity {
                        kind: EntityKind::TraitDef,
                        name: name.clone(),
                        signature: node_signature(node, source),
                        visibility: Visibility::Public,
                        doc_summary: extract_preceding_comment(node, source),
                        fingerprint: compute_fingerprint(node, source),
                        span: span_from_node(node, file_id),
                        declaration_line: None,
                    });

                    // Recurse into trait body
                    if let Some(body) = node.child_by_field_name("body") {
                        extract_php_children(
                            &body,
                            source,
                            file_id,
                            Some(&name),
                            entities,
                            relations,
                            cx,
                        );
                    }
                }
            }
        }
        "enum_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = name_node.utf8_text(source).unwrap_or("").to_string();
                if !name.is_empty() {
                    entities.push(ExtractedEntity {
                        kind: EntityKind::EnumDef,
                        name: name.clone(),
                        signature: node_signature(node, source),
                        visibility: Visibility::Public,
                        doc_summary: extract_preceding_comment(node, source),
                        fingerprint: compute_fingerprint(node, source),
                        span: span_from_node(node, file_id),
                        declaration_line: None,
                    });
                    // An enum declares methods and constants like a class.
                    if let Some(body) = node.child_by_field_name("body") {
                        extract_php_children(
                            &body,
                            source,
                            file_id,
                            Some(&name),
                            entities,
                            relations,
                            cx,
                        );
                    }
                }
            }
        }
        "const_declaration" => {
            let mut child_cursor = node.walk();
            for child in node.children(&mut child_cursor) {
                if child.kind() == "const_element" {
                    // The constant's name is a `name` child, not a field:
                    // looking it up as a field found nothing, and no PHP
                    // constant was admitted.
                    if let Some(name_node) = child_of_kind(&child, "name") {
                        let name = name_node.utf8_text(source).unwrap_or("").to_string();
                        if !name.is_empty() {
                            let qualified = if let Some(cls) = class_ctx {
                                format!("{}.{}", cls, name)
                            } else {
                                name
                            };
                            entities.push(ExtractedEntity {
                                kind: EntityKind::Constant,
                                name: qualified,
                                signature: node_signature(node, source),
                                visibility: Visibility::Public,
                                doc_summary: None,
                                fingerprint: compute_fingerprint(node, source),
                                span: span_from_node(node, file_id),
                                declaration_line: None,
                            });
                        }
                    }
                }
            }
        }
        "property_declaration" => {
            // Every property a class declares is a field entity `Class.name`.
            if let Some(cls) = class_ctx {
                let mut cursor = node.walk();
                let elements: Vec<tree_sitter::Node> = node
                    .children(&mut cursor)
                    .filter(|child| child.kind() == "property_element")
                    .collect();
                for element in &elements {
                    let spanned = if elements.len() == 1 { node } else { element };
                    push_php_field(
                        element, spanned, node, cls, source, file_id, entities, relations,
                    );
                }
            }
        }
        "enum_case" => {
            // An enum case is a constant of its enum: `Mode::Fast`.
            if let (Some(cls), Some(name_node)) = (class_ctx, node.child_by_field_name("name")) {
                let name = name_node.utf8_text(source).unwrap_or("").to_string();
                if !name.is_empty() {
                    entities.push(ExtractedEntity {
                        kind: EntityKind::Constant,
                        name: format!("{cls}.{name}"),
                        signature: node_signature(node, source),
                        visibility: Visibility::Public,
                        doc_summary: None,
                        fingerprint: compute_fingerprint(node, source),
                        span: span_from_node(node, file_id),
                        declaration_line: None,
                    });
                }
            }
        }
        "namespace_definition" => {
            // Recurse into namespace body
            if let Some(body) = node.child_by_field_name("body") {
                extract_php_children(&body, source, file_id, class_ctx, entities, relations, cx);
            }
        }
        // The top-level program node wraps everything
        "program" => {
            extract_php_children(node, source, file_id, class_ctx, entities, relations, cx);
        }
        _ => {}
    }
}

/// Extract `implements` relations from a class declaration.
fn extract_php_implements(
    node: &tree_sitter::Node,
    source: &[u8],
    class_name: &str,
    relations: &mut Vec<ExtractedRelation>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "class_interface_clause" {
            let mut iface_cursor = child.walk();
            for iface in child.children(&mut iface_cursor) {
                if iface.is_named() && iface.kind() == "name" || iface.kind() == "qualified_name" {
                    let iface_name = iface.utf8_text(source).unwrap_or("").to_string();
                    if !iface_name.is_empty() {
                        relations.push(ExtractedRelation {
                            site: None,
                            receiver: None,
                            call_shape: None,
                            kind: kin_model::RelationKind::Implements,
                            src_name: class_name.to_string(),
                            dst_name: iface_name,
                            import_source: None,
                        });
                    }
                }
            }
        }
    }
}

fn detect_php_visibility(node: &tree_sitter::Node, source: &[u8]) -> Visibility {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "visibility_modifier" {
            let text = child.utf8_text(source).unwrap_or("");
            return match text {
                "public" => Visibility::Public,
                "private" => Visibility::Private,
                "protected" => Visibility::Internal,
                _ => Visibility::Public,
            };
        }
    }
    // PHP default is public
    Visibility::Public
}

fn node_signature(node: &tree_sitter::Node, source: &[u8]) -> String {
    crate::adapter::declaration_signature(node, source)
}

fn extract_preceding_comment(node: &tree_sitter::Node, source: &[u8]) -> Option<String> {
    let prev = node.prev_sibling()?;
    if prev.kind() == "comment" {
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

fn child_of_kind<'tree>(
    node: &tree_sitter::Node<'tree>,
    kind: &str,
) -> Option<tree_sitter::Node<'tree>> {
    let mut cursor = node.walk();
    let found = node
        .children(&mut cursor)
        .find(|child| child.kind() == kind);
    found
}

/// What the walk of one PHP file reads once and shares with every callable:
/// the file's names, every callable's value-origin facts, each class's base,
/// and the nested-definition census.
struct PhpCx {
    file: php_origins::FileContext,
    facts: HashMap<(usize, usize), ScopeFacts>,
    bases: HashMap<String, String>,
    nested: PhpNestedState,
}

impl PhpCx {
    fn read(tree: &Tree, source: &[u8]) -> Self {
        let root = tree.root_node();
        let file = php_origins::FileContext::read(root, source);
        let mut bases = HashMap::new();
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            if node.kind() == "class_declaration" {
                let name = node
                    .child_by_field_name("name")
                    .and_then(|name| name.utf8_text(source).ok());
                let base = child_of_kind(&node, "base_clause")
                    .and_then(|clause| clause.named_child(0))
                    .and_then(|base| base.utf8_text(source).ok());
                if let (Some(name), Some(base)) = (name, base) {
                    bases.insert(name.to_string(), file.call_owner_leaf(base));
                }
            }
            let mut cursor = node.walk();
            stack.extend(node.named_children(&mut cursor));
        }
        Self {
            facts: php_origins::facts_by_range(tree, source),
            file,
            bases,
            nested: PhpNestedState::default(),
        }
    }
}

/// What the walk accumulates about definitions nested in callable bodies.
#[derive(Default)]
struct PhpNestedState {
    census: NestedDefinitionCensus,
    names: LocalScopeNames,
}

impl PhpNestedState {
    fn decline(&mut self, name: String, kind: EntityKind, line: u32, enclosing: &str) {
        self.census.record(UnadmittedNestedDefinition {
            name,
            kind,
            start_line: line,
            enclosing: Some(enclosing.to_string()),
            bases: Vec::new(),
        });
    }
}

/// Walk a function or method body: admit what it defines and emit its calls.
///
/// `$this->m()`, `self::m()` and `static::m()` are emitted owner-qualified
/// (`Store.m`), the shape the linker's inheritance-aware tier reads, and so
/// are `parent::m()` (on the declared base) and a static call on a named
/// class (`Store::create()`, by the class's leaf through the file's `use`
/// aliases). A call through a variable or a typed property with a
/// value-origin fact the linker can follow carries its receiver (`$store`,
/// `$this->store`) and the bare method, so the linker's value-origin tier
/// binds it to the class the declarations name. Every other call keeps the
/// bare method or function name.
///
/// A closure or arrow function bound once to a local becomes the function
/// `F.<locals>.name`, owning the calls in its body, and `$name(...)` reaches
/// it. A named function or class declared in a body is global in PHP once
/// the enclosing function runs, so it is admitted under its plain name. An
/// anonymous class is recorded as a nested definition not admitted.
#[allow(clippy::too_many_arguments)]
fn extract_callable_body(
    node: &tree_sitter::Node,
    source: &[u8],
    file_id: &FilePathId,
    context_name: &str,
    class_ctx: Option<&str>,
    entities: &mut Vec<ExtractedEntity>,
    relations: &mut Vec<ExtractedRelation>,
    cx: &mut PhpCx,
) {
    let census = php_origins::Census::of(*node, &cx.file, source);
    let mut nested = NestedCalls::default();
    let mut closures = Vec::new();
    let mut declarations = Vec::new();
    if let Some(body) = node.child_by_field_name("body") {
        let mut stack = vec![body];
        while let Some(current) = stack.pop() {
            match current.kind() {
                "anonymous_function" | "arrow_function" => {
                    if let Some(name) = php_origins::bound_closure_name(current, source) {
                        let entity = local_scope_name(context_name, &name);
                        let line = current.start_position().row as u32;
                        let variable = format!("${name}");
                        if census.single(&variable) && cx.nested.names.claim(&entity) {
                            nested.skip.push((current.start_byte(), current.end_byte()));
                            nested.renames.push((variable, entity.clone()));
                            closures.push((entity, current));
                        } else {
                            cx.nested
                                .decline(entity, EntityKind::Function, line, context_name);
                        }
                    }
                    // Its own closures stay with it.
                    continue;
                }
                "function_definition"
                | "class_declaration"
                | "interface_declaration"
                | "trait_declaration"
                | "enum_declaration" => {
                    nested.skip.push((current.start_byte(), current.end_byte()));
                    declarations.push(current);
                    continue;
                }
                "anonymous_class" => {
                    cx.nested.decline(
                        local_scope_name(
                            context_name,
                            &format!("anonymous@{}", current.start_position().row + 1),
                        ),
                        EntityKind::Class,
                        current.start_position().row as u32,
                        context_name,
                    );
                }
                _ => {}
            }
            let mut cursor = current.walk();
            stack.extend(current.named_children(&mut cursor));
        }
    }
    let scope = CallScope {
        owner: class_ctx,
        base: class_ctx.and_then(|class| cx.bases.get(class)).cloned(),
        facts: cx.facts.get(&(node.start_byte(), node.end_byte())),
        file: &cx.file,
    };
    collect_calls(node, source, context_name, &scope, &nested, relations);

    for (entity, closure) in closures {
        entities.push(ExtractedEntity {
            kind: EntityKind::Function,
            name: entity.clone(),
            signature: node_signature(&closure, source),
            visibility: Visibility::Private,
            doc_summary: None,
            fingerprint: compute_fingerprint(&closure, source),
            span: span_from_node(&closure, file_id),
            declaration_line: None,
        });
        // An arrow function reads `$this` of the method around it; a
        // closure's `$this` can be rebound, so it keeps no owner.
        let owner = (closure.kind() == "arrow_function")
            .then_some(class_ctx)
            .flatten();
        let own = CallScope {
            owner,
            base: owner.and_then(|class| cx.bases.get(class)).cloned(),
            facts: cx.facts.get(&(closure.start_byte(), closure.end_byte())),
            file: &cx.file,
        };
        let inner = NestedCalls {
            skip: Vec::new(),
            renames: nested.renames.clone(),
        };
        collect_calls(&closure, source, &entity, &own, &inner, relations);
    }
    for declaration in declarations {
        extract_php_node(&declaration, source, file_id, None, entities, relations, cx);
    }
}

/// The admitted definitions inside one callable body.
#[derive(Default)]
struct NestedCalls {
    /// Bodies whose calls another entity owns.
    skip: Vec<(usize, usize)>,
    /// (`$variable`, entity) for each admitted closure.
    renames: Vec<(String, String)>,
}

impl NestedCalls {
    fn skips(&self, node: &tree_sitter::Node) -> bool {
        self.skip
            .iter()
            .any(|&(start, end)| node.start_byte() == start && node.end_byte() == end)
    }
}

/// What one callable's calls are read against.
struct CallScope<'a> {
    /// The class `$this`, `self` and `static` name.
    owner: Option<&'a str>,
    /// The base `parent` names.
    base: Option<String>,
    facts: Option<&'a ScopeFacts>,
    file: &'a php_origins::FileContext,
}

impl CallScope<'_> {
    /// Whether the linker's value-origin tier can follow the binding a
    /// receiver key names: a declared type, an instance of a class, a
    /// function this file declares (the linker finds a free function only in
    /// the calling file), or a method called on one of those.
    fn followable(&self, key: &str) -> bool {
        let Some(facts) = self.facts else {
            return false;
        };
        fn follows(
            facts: &ScopeFacts,
            file: &php_origins::FileContext,
            origin: &ValueOrigin,
            depth: usize,
        ) -> bool {
            if depth > 8 {
                return false;
            }
            match origin {
                ValueOrigin::Declared { .. } => true,
                ValueOrigin::Local { name } => facts
                    .bindings
                    .get(name)
                    .is_some_and(|bound| follows(facts, file, bound, depth + 1)),
                ValueOrigin::Call { callee } => {
                    file.declares_function(callee)
                        || callee
                            .chars()
                            .next()
                            .is_some_and(|first| first.is_ascii_uppercase())
                }
                ValueOrigin::MethodCall { receiver, .. } => {
                    follows(facts, file, receiver, depth + 1)
                }
                ValueOrigin::Entered { .. }
                | ValueOrigin::Provided { .. }
                | ValueOrigin::External { .. }
                | ValueOrigin::Supplied { .. }
                | ValueOrigin::ImportedProvider { .. }
                | ValueOrigin::ImportedCall { .. }
                | ValueOrigin::Global { .. }
                | ValueOrigin::Enclosing { .. }
                | ValueOrigin::Module { .. }
                | ValueOrigin::ModuleMember { .. } => false,
            }
        }
        facts
            .bindings
            .get(key)
            .is_some_and(|origin| follows(facts, self.file, origin, 0))
    }
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
        if let Some((dst_name, receiver)) = call_shape(&child, source, scope, nested) {
            if !dst_name.is_empty() {
                relations.push(ExtractedRelation {
                    // The call expression itself, so a reference row can
                    // report the line the call is written on. Without it the
                    // linker has no span to store and every consuming
                    // surface reports the edge as having no evidence span.
                    site: Some(crate::adapter::site_from_node(&child)),
                    receiver,
                    call_shape: None,
                    kind: kin_model::RelationKind::Calls,
                    src_name: context_name.to_string(),
                    dst_name,
                    import_source: None,
                });
            }
        }
        collect_calls(&child, source, context_name, scope, nested, relations);
    }
}

/// The callee name and receiver a call node is emitted with, or `None` when
/// the node is no call.
fn call_shape(
    call: &tree_sitter::Node,
    source: &[u8],
    scope: &CallScope<'_>,
    nested: &NestedCalls,
) -> Option<(String, Option<String>)> {
    let text = |node: tree_sitter::Node| node.utf8_text(source).unwrap_or("").to_string();
    match call.kind() {
        "member_call_expression" | "nullsafe_member_call_expression" => {
            let name = call.child_by_field_name("name")?;
            let method = text(name);
            if name.kind() != "name" {
                // `$obj->$method()`: the bare text, as before.
                return Some((method, None));
            }
            let object = call.child_by_field_name("object");
            let spelled = object.map(text).unwrap_or_default();
            if spelled == "$this" {
                if let Some(owner) = scope.owner {
                    return Some((format!("{owner}.{method}"), None));
                }
            }
            let key = object.and_then(|object| match object.kind() {
                "variable_name" if spelled != "$this" => Some(spelled.clone()),
                "member_access_expression" => {
                    php_origins::this_property(object, source).map(|prop| format!("$this->{prop}"))
                }
                _ => None,
            });
            match key {
                Some(key) if scope.followable(&key) => Some((method, Some(key))),
                _ => Some((method, None)),
            }
        }
        "scoped_call_expression" => {
            let name = call.child_by_field_name("name")?;
            let method = text(name);
            if name.kind() != "name" {
                return Some((method, None));
            }
            let scope_node = call.child_by_field_name("scope")?;
            let owner = match scope_node.kind() {
                "relative_scope" => match text(scope_node).to_lowercase().as_str() {
                    "self" | "static" => scope.owner.map(str::to_string),
                    "parent" => scope.base.clone(),
                    _ => None,
                },
                "name" | "qualified_name" | "relative_name" => {
                    Some(scope.file.call_owner_leaf(&text(scope_node)))
                }
                _ => None,
            };
            Some(match owner {
                Some(owner) if !owner.is_empty() => (format!("{owner}.{method}"), None),
                _ => (method, None),
            })
        }
        "function_call_expression" => {
            if let Some(name_node) = call.child_by_field_name("name") {
                return Some((text(name_node), None));
            }
            let function = call.child_by_field_name("function")?;
            let callee = text(function);
            if callee.starts_with('"') || callee.starts_with('\'') {
                return None;
            }
            if function.kind() == "variable_name" {
                if let Some((_, entity)) = nested
                    .renames
                    .iter()
                    .find(|(variable, _)| *variable == callee)
                {
                    return Some((entity.clone(), None));
                }
            }
            Some((callee, None))
        }
        _ => None,
    }
}

/// Extract `use` imports at any nesting level.
#[allow(clippy::only_used_in_recursion)]
fn extract_php_imports(
    node: &tree_sitter::Node,
    source: &[u8],
    file_id: &FilePathId,
    imports: &mut Vec<FileImport>,
    relations: &mut Vec<ExtractedRelation>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "namespace_use_declaration" {
            let text = child.utf8_text(source).unwrap_or("").to_string();
            if !text.is_empty() {
                // Parse the use declaration into FileImport
                if let Some(file_import) = parse_php_use_declaration(&child, source) {
                    imports.push(file_import);
                }
            }
        }
        // Recurse to find use declarations inside namespace bodies
        extract_php_imports(&child, source, file_id, imports, relations);
    }
}

/// Every trait a class body's `use` declarations name, as an `Extends` edge
/// from the class to the trait's name as the file spells it, without a
/// leading namespace.
fn extract_php_trait_uses(
    body: &tree_sitter::Node,
    source: &[u8],
    class: &str,
    relations: &mut Vec<ExtractedRelation>,
) {
    let mut cursor = body.walk();
    for member in body.named_children(&mut cursor) {
        if member.kind() != "use_declaration" {
            continue;
        }
        let mut names = member.walk();
        for name in member.named_children(&mut names) {
            if !matches!(name.kind(), "name" | "qualified_name") {
                continue;
            }
            let spelled = name.utf8_text(source).unwrap_or("").trim();
            let leaf = spelled.rsplit('\\').next().unwrap_or(spelled).trim();
            if leaf.is_empty() {
                continue;
            }
            relations.push(ExtractedRelation {
                site: None,
                receiver: None,
                call_shape: None,
                kind: kin_model::RelationKind::Extends,
                src_name: class.to_string(),
                dst_name: leaf.to_string(),
                import_source: None,
            });
        }
    }
}

/// Parse a `namespace_use_declaration` into a structured `FileImport`.
fn parse_php_use_declaration(node: &tree_sitter::Node, source: &[u8]) -> Option<FileImport> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "namespace_use_clause" {
            // `use App\Models\Record as Row` puts the alias inside the use
            // clause, so the CLAUSE's own text is `App\Models\Record as Row`.
            // Reading it whole made the module path that whole string, which
            // names nothing this repository could resolve, and made the
            // original name the entire statement including the `use` keyword.
            // The coordinate is the clause's qualified name and nothing else.
            let alias_node = child.child_by_field_name("alias");
            let mut clause_cursor = child.walk();
            let path_node = child.children(&mut clause_cursor).find(|node| {
                node.kind() == "qualified_name"
                    || (node.is_named()
                        && node.kind() == "name"
                        && alias_node.is_none_or(|alias| alias.start_byte() != node.start_byte()))
            });
            let full_path = path_node
                .and_then(|node| node.utf8_text(source).ok())
                .unwrap_or("")
                .trim()
                .to_string();
            if full_path.is_empty() {
                continue;
            }

            let alias = alias_node
                .and_then(|node| node.utf8_text(source).ok())
                .map(str::trim)
                .filter(|alias| !alias.is_empty())
                .map(str::to_string);

            let leaf = full_path
                .rsplit('\\')
                .next()
                .unwrap_or(&full_path)
                .to_string();
            let has_alias = alias.is_some();
            let local_name = alias.unwrap_or_else(|| leaf.clone());

            return Some(FileImport {
                site: crate::adapter::site_from_node(node),
                module_path: full_path,
                specifiers: vec![ImportedName {
                    local_name,
                    // The name the TARGET declares, so an edge minted from this
                    // import points at a name some file actually writes.
                    original_name: has_alias.then_some(leaf),
                    is_default: false,
                    // The clause, not the declaration: `use\n    Foo\Bar;`
                    // opens on the `use` line and names the type on the next.
                    site: Some(crate::adapter::site_from_node(&child)),
                }],
            });
        }
    }
    None
}

/// The field entity `class.name` for the property `holder` names (a
/// `property_element` or a promoted constructor parameter), spanning
/// `spanned`, with the visibility `declaration` writes, contained by its
/// class.
#[allow(clippy::too_many_arguments)]
fn push_php_field(
    holder: &tree_sitter::Node,
    spanned: &tree_sitter::Node,
    declaration: &tree_sitter::Node,
    class: &str,
    source: &[u8],
    file_id: &FilePathId,
    entities: &mut Vec<ExtractedEntity>,
    relations: &mut Vec<ExtractedRelation>,
) {
    let mut cursor = holder.walk();
    let Some(variable) = holder
        .children(&mut cursor)
        .find(|child| child.kind() == "variable_name")
    else {
        return;
    };
    let mut inner = variable.walk();
    let Some(name) = variable
        .children(&mut inner)
        .find(|child| child.kind() == "name")
        .and_then(|name| name.utf8_text(source).ok())
        .filter(|name| !name.is_empty())
    else {
        return;
    };
    let qualified = format!("{class}.{name}");
    entities.push(ExtractedEntity {
        kind: EntityKind::Field,
        name: qualified.clone(),
        signature: node_signature(spanned, source),
        visibility: detect_php_visibility(declaration, source),
        doc_summary: extract_preceding_comment(declaration, source),
        fingerprint: compute_fingerprint(spanned, source),
        span: span_from_node(spanned, file_id),
        declaration_line: None,
    });
    relations.push(ExtractedRelation {
        site: None,
        receiver: None,
        call_shape: None,
        kind: kin_model::RelationKind::Contains,
        src_name: class.to_string(),
        dst_name: qualified,
        import_source: None,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_php_function() {
        let adapter = PhpAdapter;
        let source = b"<?php\nfunction greet($name) {\n    return \"Hello $name\";\n}\n";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("test.php");
        let output = adapter.extract(&tree, source, &file_id).unwrap();
        assert!(matches!(output.parse_state, ParseState::Valid));
        // The function, plus the file's own module surface: the entity an
        // entity-level `Imports` edge from this file is sourced at.
        assert_eq!(output.entities.len(), 2);
        let functions: Vec<_> = output
            .entities
            .iter()
            .filter(|entity| entity.kind == EntityKind::Function)
            .collect();
        assert_eq!(functions.len(), 1);
        assert_eq!(functions[0].name, "greet");
        let modules: Vec<_> = output
            .entities
            .iter()
            .filter(|entity| entity.kind == EntityKind::Module)
            .collect();
        assert_eq!(modules.len(), 1);
        assert_eq!(modules[0].name, "test");
    }

    #[test]
    fn parse_php_class() {
        let adapter = PhpAdapter;
        let source = b"<?php\nclass Dog {\n    public function bark() {}\n}\n";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("Dog.php");
        let output = adapter.extract(&tree, source, &file_id).unwrap();
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
    fn parse_php_interface() {
        let adapter = PhpAdapter;
        let source = b"<?php\ninterface Runnable {\n    public function run(): void;\n}\n";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("Runnable.php");
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
    fn parse_php_visibility() {
        let adapter = PhpAdapter;
        let source = b"<?php\nclass Foo {\n    public function pub_method() {}\n    private function priv_method() {}\n    protected function prot_method() {}\n}\n";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("Foo.php");
        let output = adapter.extract(&tree, source, &file_id).unwrap();
        let methods: Vec<_> = output
            .entities
            .iter()
            .filter(|e| e.kind == EntityKind::Method)
            .collect();
        assert_eq!(methods.len(), 3);

        let pub_m = methods.iter().find(|m| m.name == "Foo.pub_method").unwrap();
        assert_eq!(pub_m.visibility, Visibility::Public);

        let priv_m = methods
            .iter()
            .find(|m| m.name == "Foo.priv_method")
            .unwrap();
        assert_eq!(priv_m.visibility, Visibility::Private);

        let prot_m = methods
            .iter()
            .find(|m| m.name == "Foo.prot_method")
            .unwrap();
        assert_eq!(prot_m.visibility, Visibility::Internal);
    }

    #[test]
    fn parse_php_trait() {
        let adapter = PhpAdapter;
        let source = b"<?php\ntrait Loggable {\n    public function log($msg) {}\n}\n";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("Loggable.php");
        let output = adapter.extract(&tree, source, &file_id).unwrap();
        let traits: Vec<_> = output
            .entities
            .iter()
            .filter(|e| e.kind == EntityKind::TraitDef)
            .collect();
        assert_eq!(traits.len(), 1);
        assert_eq!(traits[0].name, "Loggable");
    }

    #[test]
    fn parse_php_test_detection() {
        let adapter = PhpAdapter;
        let source = b"<?php\nclass FooTest {\n    public function testSomething() {}\n    public function helper() {}\n}\n";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("FooTest.php");
        let output = adapter.extract(&tree, source, &file_id).unwrap();
        assert_eq!(output.tests.len(), 1);
        assert_eq!(output.tests[0].name, "FooTest.testSomething");
        assert_eq!(output.tests[0].runner, "phpunit");
    }

    /// A file whose bytes declare nothing mints no module surface.
    ///
    /// The surface is synthetic: it stands for the file, not for anything the
    /// file wrote. Minting it unconditionally made a comment-only or unreadable
    /// file count as parsed, so the parse-coverage census read a clean row for a
    /// repository holding a hole and could not name the file. Rust carried this
    /// rule already; these adapters did not.
    #[test]
    fn comment_only_php_file_mints_no_module_surface() {
        let adapter = PhpAdapter;
        let source = b"<?php\n// nothing is declared here\n";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("lib/Silent.php");
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
    fn php_file_with_one_declaration_mints_the_module_surface() {
        let adapter = PhpAdapter;
        let source = b"<?php\nfunction greet() {}\n";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("lib/greet.php");
        let output = adapter.extract(&tree, source, &file_id).unwrap();
        let modules: Vec<_> = output
            .entities
            .iter()
            .filter(|e| e.kind == EntityKind::Module)
            .collect();
        assert_eq!(modules.len(), 1);
        assert_eq!(modules[0].name, "greet");
        assert_eq!(output.entities[0].kind, EntityKind::Module);
    }

    /// An import alone is a surface too, and the edge is sourced at the module.
    #[test]
    fn php_file_with_only_an_import_mints_the_module_surface() {
        let adapter = PhpAdapter;
        let source = b"<?php\nuse App\\Storage\\Dog;\n";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("lib/uses.php");
        let output = adapter.extract(&tree, source, &file_id).unwrap();
        assert!(!output.imports.is_empty());
        assert_eq!(output.entities[0].kind, EntityKind::Module);
    }
}
