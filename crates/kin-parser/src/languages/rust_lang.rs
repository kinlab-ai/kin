// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

use std::collections::HashSet;

use kin_model::{EntityKind, FilePathId, LanguageId, ParseState, SourceSpan, Visibility};
use tree_sitter::Tree;

use super::rust_nested::{NestedItem, RustPlan};

use crate::adapter::{
    collect_error_ranges, compute_fingerprint, make_parser, EditHint, LanguageAdapter,
};
use crate::error::Result;
use crate::extract::{
    ExtractedEntity, ExtractedRelation, ExtractedTest, ExtractedTestKind, FileImport, ImportedName,
    ParseOutput,
};

pub struct RustAdapter;

impl LanguageAdapter for RustAdapter {
    fn language_id(&self) -> LanguageId {
        LanguageId::Rust
    }

    fn declaration_lifecycle(
        &self,
    ) -> Option<&dyn crate::declaration_lifecycle::DeclarationLifecycle> {
        Some(&super::rust_edit::RustLifecycle)
    }

    fn member_placement(&self) -> Option<&dyn crate::member_placement::MemberPlacement> {
        Some(&super::rust_edit::RustPlacement)
    }

    fn unit_editor(&self) -> Option<&dyn crate::unit_editor::UnitEditor> {
        Some(&super::rust_edit::RUST_UNITS)
    }

    fn name_syntax(&self) -> Option<crate::name_syntax::NameSyntax> {
        Some(crate::name_syntax::RUST)
    }

    fn file_extensions(&self) -> &[&str] {
        &["rs"]
    }

    fn project_manifest(&self) -> Option<crate::project_manifest::ProjectManifest> {
        Some(crate::project_manifest::CARGO_TOML)
    }

    fn parse(&self, source: &[u8]) -> Result<Tree> {
        let mut parser = make_parser(&tree_sitter_rust::LANGUAGE)?;
        parser
            .parse(source, None)
            .ok_or_else(|| crate::error::ParseError::ParseFailed {
                file: String::new(),
                reason: "tree-sitter returned None".into(),
            })
    }

    fn parse_incremental(&self, source: &[u8], old_tree: &Tree, edit: &EditHint) -> Result<Tree> {
        let mut tree = old_tree.clone();
        tree.edit(&tree_sitter::InputEdit {
            start_byte: edit.start_byte,
            old_end_byte: edit.old_end_byte,
            new_end_byte: edit.new_end_byte,
            start_position: tree_sitter::Point { row: 0, column: 0 },
            old_end_position: tree_sitter::Point { row: 0, column: 0 },
            new_end_position: tree_sitter::Point { row: 0, column: 0 },
        });
        let mut parser = make_parser(&tree_sitter_rust::LANGUAGE)?;
        parser
            .parse(source, Some(&tree))
            .ok_or_else(|| crate::error::ParseError::ParseFailed {
                file: String::new(),
                reason: "incremental parse failed".into(),
            })
    }

    fn declaration_census(
        &self,
        tree: &Tree,
        source: &[u8],
    ) -> Option<Vec<crate::declaration_census::SeenDeclaration>> {
        Some(super::rust_declarations::rust_declaration_census(
            tree, source,
        ))
    }

    fn extract(&self, tree: &Tree, source: &[u8], file_id: &FilePathId) -> Result<ParseOutput> {
        self.extract_in(
            tree,
            source,
            file_id,
            &crate::project_manifest::UnitContext::default(),
        )
    }

    /// A crate root's module takes its crate's name from the Cargo manifests
    /// of its tree, and a `lib.rs` or `main.rs` they root no target with is a
    /// plain module (see [`super::rust_crates`]).
    fn unit_context(
        &self,
        file_id: &FilePathId,
        project: &dyn crate::project_manifest::ProjectFiles,
    ) -> std::result::Result<
        crate::project_manifest::UnitContext,
        crate::project_manifest::UnreadableManifest,
    > {
        super::rust_crates::unit_context(file_id, project)
    }

    fn extract_in(
        &self,
        tree: &Tree,
        source: &[u8],
        file_id: &FilePathId,
        unit: &crate::project_manifest::UnitContext,
    ) -> Result<ParseOutput> {
        let error_ranges = collect_error_ranges(tree);
        let parse_state = if error_ranges.is_empty() {
            ParseState::Valid
        } else {
            ParseState::Incomplete { error_ranges }
        };

        // Definitions nested in function bodies are named and admitted before
        // the walk, so every callable's call walk knows which nested bodies
        // now belong to an entity of their own.
        let mut plan = super::rust_nested::plan(tree, source);
        let mut entities = Vec::new();
        let mut relations = Vec::new();
        let mut imports = Vec::new();
        let mut tests = Vec::new();
        let root = tree.root_node();
        // The file's own module surface is pushed before the walk, and import
        // linking selects it by its whole-unit signature, not entity order. A
        // declared module that carries the unit's own name, `mod defaults` in
        // `defaults.rs` or `mod foo;` in the root of crate `foo`, is a second
        // module inside the unit and stays one: merging it into the surface
        // replaced the unit's span and signature with that one line, and the
        // unit then contained nothing. Where the two derive one identity, the
        // pipeline separates them. A file that declares and imports nothing
        // drops the surface again below.
        let surface = unit
            .module_name
            .clone()
            .or_else(|| crate::adapter::file_module_surface_name(None, file_id))
            .inspect(|name| {
                entities.push(crate::adapter::file_module_surface_entity(
                    name.clone(),
                    format!("module {}", file_id.0),
                    &root,
                    source,
                    file_id,
                ));
            });
        let mut cursor = root.walk();

        for child in root.children(&mut cursor) {
            extract_rust_node(
                &child,
                source,
                file_id,
                &plan,
                &mut entities,
                &mut relations,
            );
            if child.kind() == "use_declaration" {
                if let Some(import) = extract_rust_use(&child, source) {
                    imports.push(import);
                }
            }
            // Detect #[test] functions
            if child.kind() == "function_item" && has_test_attribute(&child, source) {
                if let Some(name_node) = child.child_by_field_name("name") {
                    let name = name_node.utf8_text(source).unwrap_or("").to_string();
                    tests.push(ExtractedTest {
                        name,
                        kind: ExtractedTestKind::Unit,
                        runner: "cargo".to_string(),
                    });
                }
            }
            // Detect #[test] methods inside impl blocks and mod tests
            extract_rust_tests_from_block(&child, source, &mut tests);
        }
        for item in &plan.nested {
            emit_nested_item(item, source, file_id, &plan, &mut entities, &mut relations);
        }
        for (owner, trait_name) in &plan.trait_impls {
            relations.push(ExtractedRelation {
                site: None,
                receiver: None,
                call_shape: None,
                kind: kin_model::RelationKind::Implements,
                src_name: owner.clone(),
                dst_name: trait_name.clone(),
                import_source: None,
            });
        }
        // Calls no declaration walk reached, such as the ones an item macro
        // (`thread_local!`, `lazy_static!`) writes at module scope, go to the
        // innermost entity holding each one, the module surface for module
        // scope. The walk leaves admitted nested items to their own entities
        // exactly as a callable's walk does.
        {
            let accounted = crate::call_owner::accounted_call_sites(&relations);
            let mut swept = Vec::new();
            extract_callable_calls(&root, source, "", None, &plan, &[], &mut swept);
            crate::call_owner::adopt_unowned_calls(&entities, &accounted, swept, &mut relations);
        }
        let nested_definitions = std::mem::take(&mut plan.census);

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

        crate::import_witness::append_rust_import_witness(
            tree,
            source,
            file_id,
            &imports,
            &mut relations,
        );
        // A comment-only file declares nothing and imports nothing, so it
        // contributes no entity. Counting the synthetic module there made parse
        // coverage report rust 3/3 for files whose bytes produced no
        // declaration. The surface is withdrawn rather than never pushed,
        // because the walk above needs it in place to merge a same-named
        // `mod` declaration into.
        //
        // It is hollow only when the walk added nothing beside it. A file
        // holding just `pub mod thing {}` under `thing.rs` keeps both its
        // surface and that declaration, which is a module of its own.
        //
        // A file whose only content is an item macro (`thread_local!`,
        // `lazy_static!`) declares nothing either, but its value-escape record
        // lists what the macro calls and holds, and that record needs an entity
        // to ride on, as do the calls the macro makes, which the surface owns.
        //
        // The record's unparsed regions do not count. They say where the
        // grammar read nothing, so bytes that parse to a lone error node would
        // otherwise keep a module entity minted from that error, and parse
        // coverage would count a file whose bytes produced no declaration.
        let hollow = surface.as_deref().is_some_and(|surface| {
            imports.is_empty()
                && entities.len() == 1
                && entities[0].declaration_line.is_none()
                && !relations.iter().any(|relation| {
                    relation.kind == kin_model::RelationKind::Calls && relation.src_name == surface
                })
                && {
                    let mut record = super::rust_escape::census(tree, source).record;
                    record.unparsed.clear();
                    record.is_empty()
                }
        });
        if hollow {
            entities.clear();
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
            nested_definitions: Some(nested_definitions),
        })
    }
}

fn extract_rust_node(
    node: &tree_sitter::Node,
    source: &[u8],
    file_id: &FilePathId,
    plan: &RustPlan<'_>,
    entities: &mut Vec<ExtractedEntity>,
    relations: &mut Vec<ExtractedRelation>,
) {
    match node.kind() {
        "function_item" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = name_node.utf8_text(source).unwrap_or("").to_string();
                entities.push(ExtractedEntity {
                    kind: EntityKind::Function,
                    name: name.clone(),
                    signature: node_signature(node, source),
                    visibility: detect_rust_visibility(node, source),
                    doc_summary: extract_doc_comment(node, source),
                    fingerprint: compute_fingerprint(node, source),
                    span: rust_item_span(node, file_id, source),
                    declaration_line: Some(node.start_position().row as u32),
                });
                extract_callable_calls(node, source, &name, None, plan, &[], relations);
            }
        }
        "struct_item" | "union_item" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = name_node.utf8_text(source).unwrap_or("").to_string();
                entities.push(ExtractedEntity {
                    kind: EntityKind::Class,
                    name: name.clone(),
                    signature: node_signature(node, source),
                    visibility: detect_rust_visibility(node, source),
                    doc_summary: extract_doc_comment(node, source),
                    fingerprint: compute_fingerprint(node, source),
                    span: rust_item_span(node, file_id, source),
                    declaration_line: Some(node.start_position().row as u32),
                });
                // Emit Implements relations for #[derive(...)] traits.
                for trait_name in extract_derive_traits(node, source) {
                    relations.push(ExtractedRelation {
                        site: None,
                        receiver: None,
                        call_shape: None,
                        kind: kin_model::RelationKind::Implements,
                        src_name: name.clone(),
                        dst_name: trait_name,
                        import_source: None,
                    });
                }
                extract_rust_fields(node, &name, source, file_id, entities, relations);
            }
        }
        "enum_item" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let enum_name = name_node.utf8_text(source).unwrap_or("").to_string();
                entities.push(ExtractedEntity {
                    kind: EntityKind::EnumDef,
                    name: enum_name.clone(),
                    signature: node_signature(node, source),
                    visibility: detect_rust_visibility(node, source),
                    doc_summary: extract_doc_comment(node, source),
                    fingerprint: compute_fingerprint(node, source),
                    span: rust_item_span(node, file_id, source),
                    declaration_line: Some(node.start_position().row as u32),
                });

                // Emit Implements relations for #[derive(...)] traits.
                for trait_name in extract_derive_traits(node, source) {
                    relations.push(ExtractedRelation {
                        site: None,
                        receiver: None,
                        call_shape: None,
                        kind: kin_model::RelationKind::Implements,
                        src_name: enum_name.clone(),
                        dst_name: trait_name,
                        import_source: None,
                    });
                }

                // Extract individual enum variants as EnumVariant entities.
                if let Some(body) = node.child_by_field_name("body") {
                    let mut body_cursor = body.walk();
                    for variant in body.children(&mut body_cursor) {
                        if variant.kind() == "enum_variant" {
                            if let Some(vname) = variant.child_by_field_name("name") {
                                let variant_name =
                                    vname.utf8_text(source).unwrap_or("").to_string();
                                let qualified = format!("{}::{}", enum_name, variant_name);
                                entities.push(ExtractedEntity {
                                    kind: EntityKind::EnumVariant,
                                    name: qualified.clone(),
                                    signature: node_signature(&variant, source),
                                    visibility: detect_rust_visibility(node, source),
                                    doc_summary: None,
                                    fingerprint: compute_fingerprint(&variant, source),
                                    span: rust_item_span(&variant, file_id, source),
                                    declaration_line: Some(variant.start_position().row as u32),
                                });
                                relations.push(ExtractedRelation {
                                    site: None,
                                    receiver: None,
                                    call_shape: None,
                                    kind: kin_model::RelationKind::Contains,
                                    src_name: enum_name.clone(),
                                    dst_name: qualified,
                                    import_source: None,
                                });
                            }
                        }
                    }
                }
            }
        }
        "trait_item" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = name_node.utf8_text(source).unwrap_or("").to_string();
                entities.push(ExtractedEntity {
                    kind: EntityKind::TraitDef,
                    name: name.clone(),
                    signature: node_signature(node, source),
                    visibility: detect_rust_visibility(node, source),
                    doc_summary: extract_doc_comment(node, source),
                    fingerprint: compute_fingerprint(node, source),
                    span: rust_item_span(node, file_id, source),
                    declaration_line: Some(node.start_position().row as u32),
                });
                // The methods a trait declares, required or provided, are what
                // a call through a trait object or a bounded generic reaches:
                // its receiver's type is the trait, never one implementation.
                // A provided method's body is a callable of its own, where
                // `self.m()` names the trait's own `m`.
                for member in super::rust_nested::members(
                    *node,
                    &["function_item", "function_signature_item"],
                ) {
                    let Some(method) = super::rust_nested::item_name(member, source) else {
                        continue;
                    };
                    let qualified = format!("{name}::{method}");
                    entities.push(ExtractedEntity {
                        kind: EntityKind::Method,
                        name: qualified.clone(),
                        signature: node_signature(&member, source),
                        visibility: detect_rust_visibility(node, source),
                        doc_summary: extract_doc_comment(&member, source),
                        fingerprint: compute_fingerprint(&member, source),
                        span: rust_item_span(&member, file_id, source),
                        declaration_line: Some(member.start_position().row as u32),
                    });
                    if member.kind() == "function_item" {
                        extract_callable_calls(
                            &member,
                            source,
                            &qualified,
                            Some(name.as_str()),
                            plan,
                            &[],
                            relations,
                        );
                    }
                    relations.push(ExtractedRelation {
                        site: None,
                        receiver: None,
                        call_shape: None,
                        kind: kin_model::RelationKind::Contains,
                        src_name: name.clone(),
                        dst_name: qualified,
                        import_source: None,
                    });
                }
            }
        }
        "type_item" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = name_node.utf8_text(source).unwrap_or("").to_string();
                entities.push(ExtractedEntity {
                    kind: EntityKind::TypeAlias,
                    name,
                    signature: node_signature(node, source),
                    visibility: detect_rust_visibility(node, source),
                    doc_summary: extract_doc_comment(node, source),
                    fingerprint: compute_fingerprint(node, source),
                    span: rust_item_span(node, file_id, source),
                    declaration_line: Some(node.start_position().row as u32),
                });
            }
        }
        "const_item" | "static_item" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = name_node.utf8_text(source).unwrap_or("").to_string();
                entities.push(ExtractedEntity {
                    kind: if node.kind() == "const_item" {
                        EntityKind::Constant
                    } else {
                        EntityKind::StaticVar
                    },
                    name: name.clone(),
                    signature: node_signature(node, source),
                    visibility: detect_rust_visibility(node, source),
                    doc_summary: extract_doc_comment(node, source),
                    fingerprint: compute_fingerprint(node, source),
                    span: rust_item_span(node, file_id, source),
                    declaration_line: Some(node.start_position().row as u32),
                });
                // An initializer runs code (`Lazy::new(|| load())`, a `const fn`
                // call); its calls are the item's, not nobody's.
                extract_callable_calls(node, source, &name, None, plan, &[], relations);
            }
        }
        "impl_item" => {
            // Extract methods from impl blocks
            let type_name = node
                .child_by_field_name("type")
                .map(|t| t.utf8_text(source).unwrap_or("").to_string())
                .unwrap_or_default();

            // Check for trait impl
            let trait_name = node
                .child_by_field_name("trait")
                .map(|t| t.utf8_text(source).unwrap_or("").to_string());

            if let Some(ref trait_n) = trait_name {
                if !trait_n.is_empty() && !type_name.is_empty() {
                    relations.push(ExtractedRelation {
                        site: None,
                        receiver: None,
                        call_shape: None,
                        kind: kin_model::RelationKind::Implements,
                        src_name: type_name.clone(),
                        dst_name: trait_n.clone(),
                        import_source: None,
                    });
                }
            }

            if let Some(body) = node.child_by_field_name("body") {
                let mut body_cursor = body.walk();
                for member in body.children(&mut body_cursor) {
                    if member.kind() == "function_item" {
                        if let Some(name_node) = member.child_by_field_name("name") {
                            let method_name = name_node.utf8_text(source).unwrap_or("").to_string();
                            let qualified = if type_name.is_empty() {
                                method_name
                            } else {
                                format!("{}::{}", type_name, method_name)
                            };
                            entities.push(ExtractedEntity {
                                kind: EntityKind::Method,
                                name: qualified.clone(),
                                signature: node_signature(&member, source),
                                visibility: detect_rust_visibility(&member, source),
                                doc_summary: extract_doc_comment(&member, source),
                                fingerprint: compute_fingerprint(&member, source),
                                span: rust_item_span(&member, file_id, source),
                                declaration_line: Some(member.start_position().row as u32),
                            });
                            extract_callable_calls(
                                &member,
                                source,
                                &qualified,
                                (!type_name.is_empty()).then_some(type_name.as_str()),
                                plan,
                                &[],
                                relations,
                            );
                            // A method declared in `impl Trait for Type` satisfies
                            // the trait's contract, so a caller holding the trait
                            // reaches it without any edge naming the method. Only
                            // the implementing type carried that fact before, which
                            // left no way to tell a trait method apart from an
                            // inherent one that nothing calls.
                            if let Some(ref trait_n) = trait_name {
                                if !trait_n.is_empty() {
                                    relations.push(ExtractedRelation {
                                        site: None,
                                        receiver: None,
                                        call_shape: None,
                                        kind: kin_model::RelationKind::Implements,
                                        src_name: qualified.clone(),
                                        dst_name: trait_n.clone(),
                                        import_source: None,
                                    });
                                }
                            }
                            if !type_name.is_empty() {
                                relations.push(ExtractedRelation {
                                    site: None,
                                    receiver: None,
                                    call_shape: None,
                                    kind: kin_model::RelationKind::Contains,
                                    src_name: type_name.clone(),
                                    dst_name: qualified,
                                    import_source: None,
                                });
                            }
                        }
                    }
                }
            }
        }
        "mod_item" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = name_node.utf8_text(source).unwrap_or("").to_string();
                // A module declared twice under one name, as two `cfg` arms
                // of `mod imp` are, is one module. The unit's own surface,
                // minted with no declaration line, is never such a
                // declaration: a `mod` carrying the unit's name is a module
                // inside it.
                if let Some(existing) = entities.iter_mut().find(|e| {
                    e.kind == EntityKind::Module && e.name == name && e.declaration_line.is_some()
                }) {
                    existing.signature = node_signature(node, source);
                    existing.visibility = detect_rust_visibility(node, source);
                    existing.doc_summary = extract_doc_comment(node, source);
                    existing.fingerprint = compute_fingerprint(node, source);
                    existing.span = rust_item_span(node, file_id, source);
                    existing.declaration_line = Some(node.start_position().row as u32);
                } else {
                    entities.push(ExtractedEntity {
                        kind: EntityKind::Module,
                        name,
                        signature: node_signature(node, source),
                        visibility: detect_rust_visibility(node, source),
                        doc_summary: extract_doc_comment(node, source),
                        fingerprint: compute_fingerprint(node, source),
                        span: rust_item_span(node, file_id, source),
                        declaration_line: Some(node.start_position().row as u32),
                    });
                }
            }
            // Descend into an inline module body so functions, impls, and nested
            // modules declared inside `mod m { ... }` are extracted with their own
            // entities and call edges. Without this, everything below a module is
            // dropped, and calls made from module-scoped functions never reach
            // refs/impact — attribution effectively collapses onto the file/module
            // instead of the innermost enclosing function. `mod m;` (no body) has
            // nothing to descend into.
            if let Some(body) = node.child_by_field_name("body") {
                let mut cursor = body.walk();
                for child in body.children(&mut cursor) {
                    extract_rust_node(&child, source, file_id, plan, entities, relations);
                }
            }
        }
        "macro_definition" => {
            // `macro_rules! name { ... }`. The declarative-macro definition is a
            // first-class symbol; capturing it keeps the macro name graph-visible
            // so lookups resolve at ingestion rather than missing silently. The
            // rule bodies are intentionally not expanded (expansion would fabricate
            // entities at synthetic spans), mirroring the C/C++ macro boundary.
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = name_node.utf8_text(source).unwrap_or("").to_string();
                if !name.is_empty() {
                    let visibility = if has_macro_export(node, source) {
                        Visibility::Public
                    } else {
                        detect_rust_visibility(node, source)
                    };
                    entities.push(ExtractedEntity {
                        kind: EntityKind::Macro,
                        name,
                        signature: node_signature(node, source),
                        visibility,
                        doc_summary: extract_doc_comment(node, source),
                        fingerprint: compute_fingerprint(node, source),
                        span: rust_item_span(node, file_id, source),
                        declaration_line: Some(node.start_position().row as u32),
                    });
                }
            }
        }
        "use_declaration" => {}
        _ => {}
    }
}

/// Emit one definition nested in a function body under the name the plan
/// gave it, with the calls its own body makes.
fn emit_nested_item(
    item: &NestedItem<'_>,
    source: &[u8],
    file_id: &FilePathId,
    plan: &RustPlan<'_>,
    entities: &mut Vec<ExtractedEntity>,
    relations: &mut Vec<ExtractedRelation>,
) {
    let node = &item.node;
    let visibility = if crate::nested::is_local_scope_name(&item.name) {
        // A name declared in a block is visible in that block and nowhere
        // else, whatever visibility it is written with.
        Visibility::Private
    } else {
        detect_rust_visibility(node, source)
    };
    entities.push(ExtractedEntity {
        kind: item.kind,
        name: item.name.clone(),
        signature: node_signature(node, source),
        visibility,
        doc_summary: extract_doc_comment(node, source),
        fingerprint: compute_fingerprint(node, source),
        span: rust_item_span(node, file_id, source),
        declaration_line: Some(node.start_position().row as u32),
    });
    let relation = |kind, src_name: &str, dst_name: &str| ExtractedRelation {
        site: None,
        receiver: None,
        call_shape: None,
        kind,
        src_name: src_name.to_string(),
        dst_name: dst_name.to_string(),
        import_source: None,
    };
    match node.kind() {
        "function_item" | "const_item" | "static_item" => extract_callable_calls(
            node,
            source,
            &item.name,
            item.owner.as_deref(),
            plan,
            &item.inherited,
            relations,
        ),
        "struct_item" | "union_item" | "enum_item" => {
            for trait_name in extract_derive_traits(node, source) {
                relations.push(relation(
                    kin_model::RelationKind::Implements,
                    &item.name,
                    &trait_name,
                ));
            }
        }
        _ => {}
    }
    if node.kind() == "enum_item" {
        if let Some(body) = node.child_by_field_name("body") {
            let mut cursor = body.walk();
            for variant in body.children(&mut cursor) {
                let Some(variant_name) = (variant.kind() == "enum_variant")
                    .then(|| variant.child_by_field_name("name"))
                    .flatten()
                else {
                    continue;
                };
                let qualified = format!(
                    "{}::{}",
                    item.name,
                    variant_name.utf8_text(source).unwrap_or("")
                );
                entities.push(ExtractedEntity {
                    kind: EntityKind::EnumVariant,
                    name: qualified.clone(),
                    signature: node_signature(&variant, source),
                    visibility,
                    doc_summary: None,
                    fingerprint: compute_fingerprint(&variant, source),
                    span: rust_item_span(&variant, file_id, source),
                    declaration_line: Some(variant.start_position().row as u32),
                });
                relations.push(relation(
                    kin_model::RelationKind::Contains,
                    &item.name,
                    &qualified,
                ));
            }
        }
    }
    if let Some(owner) = &item.owner {
        relations.push(relation(
            kin_model::RelationKind::Contains,
            owner,
            &item.name,
        ));
    }
    if let Some(trait_name) = &item.implements {
        relations.push(relation(
            kin_model::RelationKind::Implements,
            &item.name,
            trait_name,
        ));
    }
}

/// Extract trait names from `#[derive(Trait1, Trait2)]` attributes preceding
/// a struct or enum. Returns a list of trait name strings.
fn extract_derive_traits(node: &tree_sitter::Node, source: &[u8]) -> Vec<String> {
    let mut traits = Vec::new();
    for trivia in leading_trivia(node, source) {
        if trivia.kind() == "attribute_item" {
            derive_traits_in(trivia.utf8_text(source).unwrap_or(""), &mut traits);
        }
    }
    // Also check child attributes (tree-sitter sometimes nests them)
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "attribute_item" {
            derive_traits_in(child.utf8_text(source).unwrap_or(""), &mut traits);
        }
    }
    traits
}

/// The trait names inside one attribute's text when it is a `#[derive(..)]`.
fn derive_traits_in(text: &str, into: &mut Vec<String>) {
    if let Some(start) = text.find("derive(") {
        let after = &text[start + 7..];
        if let Some(end) = after.find(')') {
            for t in after[..end].split(',') {
                let t = t.trim();
                if !t.is_empty() {
                    into.push(t.to_string());
                }
            }
        }
    }
}

fn detect_rust_visibility(node: &tree_sitter::Node, source: &[u8]) -> Visibility {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "visibility_modifier" {
            let text = child.utf8_text(source).unwrap_or("");
            if text.contains("pub(crate)") {
                return Visibility::Crate;
            } else if text.contains("pub(super)") || text.contains("pub(in") {
                return Visibility::Internal;
            } else if text == "pub" {
                return Visibility::Public;
            }
        }
    }
    Visibility::Private
}

/// Detect a `#[macro_export]` attribute preceding a `macro_rules!` definition.
/// An exported macro is crate-public regardless of its module position, whereas
/// an unexported `macro_rules!` is module-local (Private).
fn has_macro_export(node: &tree_sitter::Node, source: &[u8]) -> bool {
    let mut prev = node.prev_sibling();
    while let Some(p) = prev {
        match p.kind() {
            "attribute_item" => {
                if p.utf8_text(source).unwrap_or("").contains("macro_export") {
                    return true;
                }
            }
            // Doc comments may sit between the attribute and the macro; skip them.
            "line_comment" | "block_comment" => {}
            _ => break,
        }
        prev = p.prev_sibling();
    }
    false
}

fn node_signature(node: &tree_sitter::Node, source: &[u8]) -> String {
    crate::adapter::declaration_signature(node, source)
}

/// An outer line doc comment is exactly three slashes. `////` is a plain
/// comment in Rust, however often it is used to draw a rule above an item.
fn is_outer_doc_line(text: &str) -> bool {
    text.starts_with("///") && !text.starts_with("////")
}

/// An outer block doc comment opens with `/**` followed by anything but
/// another `*` or an immediate close: `/***` and `/**/` are plain comments.
fn is_outer_doc_block(text: &str) -> bool {
    text.starts_with("/**") && !text.starts_with("/***") && text != "/**/"
}

/// Every named field of a struct or union, as a field entity of its own:
/// owner-qualified the way a method is (`Account::balance`), contained by its
/// owner, and spanning its documentation and attributes. A tuple struct's
/// fields have no names and stay out.
fn extract_rust_fields(
    node: &tree_sitter::Node,
    owner: &str,
    source: &[u8],
    file_id: &FilePathId,
    entities: &mut Vec<ExtractedEntity>,
    relations: &mut Vec<ExtractedRelation>,
) {
    let Some(body) = node.child_by_field_name("body") else {
        return;
    };
    let mut cursor = body.walk();
    for field in body.children(&mut cursor) {
        if field.kind() != "field_declaration" {
            continue;
        }
        let Some(field_name) = field
            .child_by_field_name("name")
            .and_then(|name| name.utf8_text(source).ok())
            .filter(|name| !name.is_empty())
        else {
            continue;
        };
        let qualified = format!("{owner}::{field_name}");
        entities.push(ExtractedEntity {
            kind: EntityKind::Field,
            name: qualified.clone(),
            signature: node_signature(&field, source),
            visibility: detect_rust_visibility(&field, source),
            doc_summary: extract_doc_comment(&field, source),
            fingerprint: compute_fingerprint(&field, source),
            span: rust_item_span(&field, file_id, source),
            declaration_line: Some(field.start_position().row as u32),
        });
        relations.push(ExtractedRelation {
            site: None,
            receiver: None,
            call_shape: None,
            kind: kin_model::RelationKind::Contains,
            src_name: owner.to_string(),
            dst_name: qualified,
            import_source: None,
        });
    }
}

/// The doc comments and outer attributes that belong to an item, in source
/// order, read from the siblings above it.
///
/// One walk serves the span, the doc summary and the derive list, so the
/// three cannot disagree about which lines belong to an item. It walks past
/// an ordinary comment sitting between a doc comment and its item, because
/// rustc attaches the doc comment across it, but an ordinary comment never
/// starts the run: the first node returned is always a doc comment or an
/// attribute. Inner doc comments (`//!`, `/*!`) and inner attributes
/// (`#![..]`, a different node kind) belong to the enclosing item and end
/// the walk.
fn leading_trivia<'a>(node: &tree_sitter::Node<'a>, source: &[u8]) -> Vec<tree_sitter::Node<'a>> {
    use crate::declaration_extent::{leading_run, Leading};
    leading_run(node, false, |sibling| {
        let text = sibling.utf8_text(source).unwrap_or("");
        match sibling.kind() {
            "attribute_item" => Leading::Owned,
            "line_comment" if text.starts_with("//!") => Leading::Stop,
            "line_comment" if is_outer_doc_line(text) => Leading::Owned,
            "line_comment" => Leading::Passable,
            "block_comment" if text.starts_with("/*!") => Leading::Stop,
            "block_comment" if is_outer_doc_block(text) => Leading::Owned,
            "block_comment" => Leading::Passable,
            _ => Leading::Stop,
        }
    })
}

/// The span of a Rust item including the doc comments and outer attributes
/// above it, so a body read or written through the entity carries its
/// documentation. The entity's id does not key on this span's start; see
/// `declaration_line` on `ExtractedEntity`.
fn rust_item_span(node: &tree_sitter::Node, file_id: &FilePathId, source: &[u8]) -> SourceSpan {
    crate::declaration_extent::span_with_leading(node, &leading_trivia(node, source), file_id)
}

/// Where an item's entity span starts: its first doc comment or outer
/// attribute, or the item itself. Returns (byte, line, column). A reader that
/// keys a fact to the entity's span starts from here.
pub(crate) fn rust_item_start(node: &tree_sitter::Node, source: &[u8]) -> (usize, u32, u32) {
    let first = leading_trivia(node, source)
        .first()
        .copied()
        .unwrap_or(*node);
    let position = first.start_position();
    (
        first.start_byte(),
        position.row as u32,
        position.column as u32,
    )
}

fn extract_doc_comment(node: &tree_sitter::Node, source: &[u8]) -> Option<String> {
    let mut comments = Vec::new();
    for trivia in leading_trivia(node, source) {
        let text = trivia.utf8_text(source).unwrap_or("");
        match trivia.kind() {
            "line_comment" if is_outer_doc_line(text) => {
                comments.push(text.trim_start_matches('/').trim().to_string());
            }
            "block_comment" if is_outer_doc_block(text) => {
                let cleaned = text.trim_start_matches("/**").trim_end_matches("*/").trim();
                comments.push(cleaned.to_string());
            }
            _ => {}
        }
    }
    if comments.is_empty() {
        None
    } else {
        Some(comments.join(" "))
    }
}

/// What a walk of one callable's calls carries.
struct CallScope<'a, 'tree> {
    /// The entity the calls are attributed to.
    context_name: &'a str,
    /// The impl or trait owner `self` and `Self` name.
    owner: Option<&'a str>,
    plan: &'a RustPlan<'tree>,
    /// Names a pattern in the callable binds. A plain call spelling one calls
    /// the local value, never a nested item of the same name.
    bound: HashSet<String>,
}

impl CallScope<'_, '_> {
    /// A callee as the items nested in enclosing blocks name it: `helper()`
    /// calls `outer.<locals>.helper` when a block around the call declares it,
    /// and `Local::new()` calls `outer.<locals>.Local::new`.
    fn localize(&self, callee: String, stack: &[usize]) -> String {
        let (first, rest) = match callee.find("::") {
            Some(at) if !callee[at..].starts_with("::<") => callee.split_at(at),
            Some(at) => (&callee[..at], ""),
            None => (callee.as_str(), ""),
        };
        let entity = if rest.is_empty() {
            if self.bound.contains(first) {
                None
            } else {
                self.plan.lookup(stack, first).flatten()
            }
        } else {
            self.plan.lookup_type(stack, first).flatten()
        };
        match entity {
            Some(entity) => {
                let tail = if rest.is_empty() {
                    &callee[first.len()..]
                } else {
                    rest
                };
                format!("{entity}{tail}")
            }
            None => callee,
        }
    }
}

/// Extract every call a callable's body makes, leaving the bodies of nested
/// definitions the plan admits to their own entities.
fn extract_callable_calls(
    node: &tree_sitter::Node,
    source: &[u8],
    context_name: &str,
    owner: Option<&str>,
    plan: &RustPlan<'_>,
    inherited: &[usize],
    relations: &mut Vec<ExtractedRelation>,
) {
    // Only a file that declares nested items has a call to rewrite, and only
    // then can a local of the same name shadow one.
    let mut sites = Vec::new();
    if plan.has_block_items() {
        super::rust_nested::binding_sites(*node, &|inner| plan.is_admitted(inner), &mut sites);
    }
    let bound = sites
        .into_iter()
        .map(|(name, _, _)| name.utf8_text(source).unwrap_or("").to_string())
        .collect();
    let scope = CallScope {
        context_name,
        owner,
        plan,
        bound,
    };
    let mut stack = inherited.to_vec();
    extract_calls_from_context(node, source, &scope, &mut stack, relations);
}

/// Extract all function/method calls within a function body.
fn extract_calls_from_context(
    node: &tree_sitter::Node,
    source: &[u8],
    scope: &CallScope<'_, '_>,
    stack: &mut Vec<usize>,
    relations: &mut Vec<ExtractedRelation>,
) {
    let context_name = scope.context_name;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        // A nested definition the plan admits is an entity of its own, and
        // what is written inside it is its own.
        if scope.plan.is_admitted(child) {
            continue;
        }
        if child.kind() == "call_expression" {
            // The callee is the `function` field
            if let Some(function) = child.child_by_field_name("function") {
                let (callee_name, receiver) =
                    rust_callee_and_receiver(&function, source, scope, stack);
                if is_valid_callee_name(&callee_name) {
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
                        dst_name: callee_name,
                        import_source: None,
                    });
                }
            }
        } else if child.kind() == "macro_invocation" {
            // Macro arguments are token soup: tree-sitter does not parse
            // expressions inside token trees, so `format!("{}", total(x))`
            // carries no call_expression node for `total(x)`. Without this,
            // calls made inside format!/println!/assert!-style invocations
            // never become graph relations and downstream consumers vanish
            // from cross-file adjacency. Reconstruct call-shaped token runs
            // from the delimited token tree instead.
            let mut macro_cursor = child.walk();
            for macro_child in child.children(&mut macro_cursor) {
                if macro_child.kind() == "token_tree" {
                    extract_calls_from_token_tree(&macro_child, source, scope, stack, relations);
                }
            }
        }
        // Recurse into child nodes, with the items a block declares in scope.
        let opens_scope = child.kind() == "block" && scope.plan.declares_items(child);
        if opens_scope {
            stack.push(child.id());
        }
        extract_calls_from_context(&child, source, scope, stack, relations);
        if opens_scope {
            stack.pop();
        }
    }
}

/// The site covering two tokens and everything between them.
///
/// `site_from_node` takes one node, which is all an adapter needs wherever the
/// grammar gives the whole construct a node of its own. Inside a macro's token
/// tree it does not: a call is a flat run of sibling tokens, so the run's
/// extent is the first token's start and the last token's end.
fn site_spanning(
    first: &tree_sitter::Node,
    last: &tree_sitter::Node,
) -> crate::extract::RelationSite {
    let start = first.start_position();
    let end = last.end_position();
    crate::extract::RelationSite {
        start_byte: first.start_byte(),
        end_byte: last.end_byte(),
        start_line: start.row as u32,
        start_col: start.column as u32,
        end_line: end.row as u32,
        end_col: end.column as u32,
        syntactic_role: None,
    }
}

/// Extract call-shaped token runs from a macro-invocation token tree.
///
/// Inside a token tree an invocation like `billing::compute_total(amount)`
/// is a flat token run: `identifier ("::" identifier)*` followed by a
/// parenthesized `token_tree`. Treat that shape as a call, mirroring the
/// `call_expression` extraction rules:
/// - method calls (`x.method(..)`) collapse to the bare method name, exactly
///   like the `field_expression` arm above;
/// - qualified paths keep their `a::b` text, exactly like scoped callees;
/// - `name!(..)` runs are nested macro uses, not calls (the identifier is
///   followed by `!`, never directly by the token tree);
/// - bracket/brace groups (`v[i]`, `S { .. }`) are not argument lists.
///
/// Nested token trees are scanned recursively so calls inside argument lists
/// and nested macro bodies are also captured.
fn extract_calls_from_token_tree(
    node: &tree_sitter::Node,
    source: &[u8],
    scope: &CallScope<'_, '_>,
    stack: &[usize],
    relations: &mut Vec<ExtractedRelation>,
) {
    let (context_name, owner) = (scope.context_name, scope.owner);
    let mut cursor = node.walk();
    let tokens: Vec<tree_sitter::Node> = node.children(&mut cursor).collect();

    for (index, token) in tokens.iter().enumerate() {
        if token.kind() == "token_tree" {
            // `#[cfg(test)]` and `#![doc(...)]` are attributes, whose
            // parenthesized groups are arguments to the attribute and not
            // calls, the way they are outside a macro.
            if !is_token_tree_attribute(&tokens, index) {
                extract_calls_from_token_tree(token, source, scope, stack, relations);
            }
            continue;
        }
        if token.kind() != "identifier" {
            continue;
        }
        // A declaration's name and an interpolated token are not a known
        // callee: `fn name(..)` and a tuple struct's `struct Name(..)` declare
        // one, and `#name` or `$name` stands in for one. Comments are trivia
        // even inside a token tree, so inspect the preceding non-comment token
        // rather than only the sibling.
        let previous = tokens[..index]
            .iter()
            .rev()
            .find(|previous| !matches!(previous.kind(), "line_comment" | "block_comment"));
        if previous.is_some_and(|previous| matches!(previous.kind(), "fn" | "struct" | "#" | "$")) {
            continue;
        }
        // A call needs a parenthesized group immediately after the name.
        let Some(next) = tokens.get(index + 1) else {
            continue;
        };
        if next.kind() != "token_tree" || next.child(0).map(|open| open.kind()) != Some("(") {
            continue;
        }
        // `x.method(..)`: bare method name, matching the field_expression arm.
        // The token before the dot is the receiver, so a macro-body dispatch
        // carries the same receiver evidence a `call_expression` one does. With
        // no such token there is nothing to attribute the call to, and recording
        // it receiverless would present a dispatch as a bare call.
        let is_method_call = index > 0 && tokens[index - 1].kind() == ".";
        let mut receiver_start = index.saturating_sub(2);
        let receiver = if is_method_call {
            if index < 2 {
                continue;
            }
            // The whole receiver chain, `self.store` in `self.store.ok()`, not
            // just the token before the dot: a receiver recorded as `store`
            // would read as a local of that name to every tier that types one.
            receiver_start = token_tree_receiver_start(&tokens, index - 2);
            let text = &source[tokens[receiver_start].start_byte()..tokens[index - 2].end_byte()];
            Some(String::from_utf8_lossy(text).into_owned())
        } else {
            None
        };
        let mut callee_name = token.utf8_text(source).unwrap_or("").to_string();
        // The leftmost token of the call run, so the recorded site covers the
        // call as written rather than only the leaf identifier. A method call
        // starts at its receiver, which the arm above proved present before it
        // could read the receiver's text; anything else starts at the first
        // segment of its path.
        let call_start = if is_method_call {
            receiver_start
        } else {
            // Reconstruct a leading `a::b::` path so qualified callees keep
            // the same shape the call_expression arm extracts.
            let mut start = index;
            while start >= 2
                && tokens[start - 1].kind() == "::"
                && tokens[start - 2].kind() == "identifier"
            {
                start -= 2;
                callee_name = format!(
                    "{}::{}",
                    tokens[start].utf8_text(source).unwrap_or(""),
                    callee_name
                );
            }
            start
        };
        let (callee_name, receiver) = if receiver.is_some() {
            fold_settled_rust_receiver(callee_name, receiver, owner)
        } else {
            (
                fold_self_path(scope.localize(callee_name, stack), owner),
                None,
            )
        };
        if is_valid_callee_name(&callee_name) {
            relations.push(ExtractedRelation {
                // The token run the call is written as, from its leftmost token
                // through the closing parenthesis of its argument group. A
                // macro body carries no `call_expression` node to hand to
                // `site_from_node`, and a call inside one is still a line a
                // reference row has to be able to report.
                site: Some(site_spanning(&tokens[call_start], next)),
                receiver,
                call_shape: None,
                kind: kin_model::RelationKind::Calls,
                src_name: context_name.to_string(),
                dst_name: callee_name,
                import_source: None,
            });
        }
    }
}

/// Whether the token tree at `index` is the bracketed body of an attribute,
/// `#[..]` or `#![..]`.
fn is_token_tree_attribute(tokens: &[tree_sitter::Node], index: usize) -> bool {
    let opens_with_bracket = tokens[index].child(0).map(|open| open.kind()) == Some("[");
    let kind_at = |at: Option<usize>| at.and_then(|at| tokens.get(at)).map(|token| token.kind());
    opens_with_bracket
        && (kind_at(index.checked_sub(1)) == Some("#")
            || (kind_at(index.checked_sub(1)) == Some("!")
                && kind_at(index.checked_sub(2)) == Some("#")))
}

/// The first token of the receiver chain that ends at `last` inside a macro
/// token tree: `self . store` for `self.store.ok()`, `a . b (..)` for
/// `a.b().c()`. A chain link is a name or a delimited group, joined by dots.
fn token_tree_receiver_start(tokens: &[tree_sitter::Node], last: usize) -> usize {
    let is_link = |token: &tree_sitter::Node| {
        matches!(
            token.kind(),
            "identifier" | "self" | "super" | "crate" | "token_tree" | "integer_literal"
        )
    };
    let mut start = last;
    loop {
        // A call or index group belongs to the name before it: `b (..)`.
        if tokens[start].kind() == "token_tree"
            && start > 0
            && matches!(tokens[start - 1].kind(), "identifier" | "self")
        {
            start -= 1;
            continue;
        }
        if start >= 2 && tokens[start - 1].kind() == "." && is_link(&tokens[start - 2]) {
            start -= 2;
            continue;
        }
        return start;
    }
}

/// The callee name and receiver one Rust call site records.
///
/// A method reached through an object keeps its bare leaf and carries the
/// receiver as written, which is what lets the linker tell a dispatch from a
/// free-function call: without it, `builder.multi_line(..)` and a module-level
/// `multi_line(..)` are the same relation, and every same-named method in the
/// repository is an equally good answer.
///
/// A receiver the syntax settles is folded into the callee instead. Inside
/// `impl Type`, `self.m()` and `Self::m()` can only reach `Type`'s own `m`, and
/// `Type::m` is the exact key the method entity is stored under, so the call
/// resolves to that definition rather than fanning out by name. `self.field.m()`
/// is deliberately not folded: it dispatches on the field's type, which the
/// syntax does not settle.
fn rust_callee_and_receiver(
    function: &tree_sitter::Node,
    source: &[u8],
    scope: &CallScope<'_, '_>,
    stack: &[usize],
) -> (String, Option<String>) {
    let owner = scope.owner;
    if function.kind() == "field_expression" {
        let method = function
            .child_by_field_name("field")
            .map(|field| field.utf8_text(source).unwrap_or("").to_string())
            .unwrap_or_default();
        let receiver = function
            .child_by_field_name("value")
            .map(|value| value.utf8_text(source).unwrap_or("").to_string());
        return fold_settled_rust_receiver(method, receiver, owner);
    }
    let name = function.utf8_text(source).unwrap_or("").to_string();
    (fold_self_path(scope.localize(name, stack), owner), None)
}

/// Fold a `self.m()` receiver into `Owner::m`, or keep the receiver as written.
///
/// Shared by the `call_expression` and macro-token-tree extraction paths so a
/// call inside `assert!(..)` records the same shape as the identical call
/// outside it.
fn fold_settled_rust_receiver(
    method: String,
    receiver: Option<String>,
    owner: Option<&str>,
) -> (String, Option<String>) {
    match (owner, receiver.as_deref()) {
        (Some(owner), Some("self")) if !method.is_empty() => (format!("{owner}::{method}"), None),
        _ => (method, receiver.filter(|value| !value.is_empty())),
    }
}

/// Rewrite a `Self::m` path callee as `Owner::m`.
///
/// `Self` names the impl's own type, so the call reaches that type's method and
/// nothing else. Left alone outside an impl, and for every other path.
fn fold_self_path(name: String, owner: Option<&str>) -> String {
    let Some(owner) = owner else {
        return name;
    };
    match name.strip_prefix("Self::") {
        Some(rest) if !rest.is_empty() => format!("{owner}::{rest}"),
        _ => name,
    }
}

/// Check if a callee name is valid (not a literal, not empty).
fn is_valid_callee_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('"')
        && !name.starts_with('\'')
        && !name.chars().all(|c| c.is_numeric())
}

/// Extract a `use` declaration into a `FileImport`.
fn extract_rust_use(node: &tree_sitter::Node, source: &[u8]) -> Option<FileImport> {
    // Walk direct children of use_declaration to find the argument
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "scoped_identifier" => {
                // use foo::bar;
                return extract_scoped_identifier_import(&child, source);
            }
            "use_as_clause" => {
                // use foo::bar as baz;
                return extract_use_as_clause_import(&child, source);
            }
            "scoped_use_list" => {
                // use foo::{bar, baz};
                return extract_scoped_use_list_import(&child, source);
            }
            "use_wildcard" => {
                // `use foo::*;` binds every public name in `foo`, and none of
                // them is written here. Recording the module with no specifier
                // is what tells a consumer this file's import list cannot answer
                // "does this file bind that name", the same shape the Python
                // adapter records `from foo import *` as. Dropping the import
                // entirely would leave the file looking name-complete.
                let module_path = child
                    .children(&mut child.walk())
                    .find(|node| {
                        matches!(node.kind(), "scoped_identifier" | "identifier" | "crate")
                    })
                    .map(|node| node.utf8_text(source).unwrap_or("").to_string())
                    .unwrap_or_default();
                return Some(FileImport {
                    site: crate::adapter::site_from_node(node),
                    module_path,
                    specifiers: Vec::new(),
                });
            }
            "identifier" => {
                // use foo;
                let name = child.utf8_text(source).unwrap_or("").to_string();
                if !name.is_empty() {
                    return Some(FileImport {
                        site: crate::adapter::site_from_node(node),
                        module_path: String::new(),
                        specifiers: vec![ImportedName {
                            local_name: name,
                            original_name: None,
                            is_default: false,
                            site: Some(crate::adapter::site_from_node(&child)),
                        }],
                    });
                }
            }
            _ => {}
        }
    }
    None
}

/// Extract import from `use foo::bar;` (scoped_identifier).
fn extract_scoped_identifier_import(node: &tree_sitter::Node, source: &[u8]) -> Option<FileImport> {
    let path_node = node.child_by_field_name("path")?;
    let name_node = node.child_by_field_name("name")?;
    let module_path = path_node.utf8_text(source).unwrap_or("").to_string();
    let local_name = name_node.utf8_text(source).unwrap_or("").to_string();
    if local_name.is_empty() {
        return None;
    }
    Some(FileImport {
        site: crate::adapter::site_from_node(node),
        module_path,
        specifiers: vec![ImportedName {
            local_name,
            original_name: None,
            is_default: false,
            site: Some(crate::adapter::site_from_node(&name_node)),
        }],
    })
}

/// Extract import from `use foo::bar as baz;` (use_as_clause).
fn extract_use_as_clause_import(node: &tree_sitter::Node, source: &[u8]) -> Option<FileImport> {
    let path_node = node.child_by_field_name("path")?;
    let alias_node = node.child_by_field_name("alias")?;

    let full_path = path_node.utf8_text(source).unwrap_or("").to_string();
    let alias = alias_node.utf8_text(source).unwrap_or("").to_string();

    // path_node is a scoped_identifier like foo::bar; split into module + name
    let (module_path, original_name) = if let Some(pos) = full_path.rfind("::") {
        (
            full_path[..pos].to_string(),
            full_path[pos + 2..].to_string(),
        )
    } else {
        (String::new(), full_path)
    };

    if alias.is_empty() {
        return None;
    }

    Some(FileImport {
        site: crate::adapter::site_from_node(node),
        module_path,
        specifiers: vec![ImportedName {
            local_name: alias,
            original_name: Some(original_name),
            is_default: false,
            site: Some(crate::adapter::site_from_node(node)),
        }],
    })
}

/// Extract import from `use foo::{bar, baz};` (scoped_use_list).
fn extract_scoped_use_list_import(node: &tree_sitter::Node, source: &[u8]) -> Option<FileImport> {
    let path_node = node.child_by_field_name("path")?;
    let list_node = node.child_by_field_name("list")?;
    let module_path = path_node.utf8_text(source).unwrap_or("").to_string();

    let mut specifiers = Vec::new();
    let mut cursor = list_node.walk();
    for child in list_node.children(&mut cursor) {
        match child.kind() {
            "identifier" => {
                let name = child.utf8_text(source).unwrap_or("").to_string();
                if !name.is_empty() {
                    specifiers.push(ImportedName {
                        local_name: name,
                        original_name: None,
                        is_default: false,
                        site: Some(crate::adapter::site_from_node(&child)),
                    });
                }
            }
            "scoped_identifier" => {
                // Nested path like `use std::io::{self, Read}` — the `self` case
                // or `use foo::{bar::Baz}`
                let name = child.utf8_text(source).unwrap_or("").to_string();
                if !name.is_empty() {
                    specifiers.push(ImportedName {
                        local_name: name,
                        original_name: None,
                        is_default: false,
                        site: Some(crate::adapter::site_from_node(&child)),
                    });
                }
            }
            "use_as_clause" => {
                if let Some(path) = child.child_by_field_name("path") {
                    if let Some(alias) = child.child_by_field_name("alias") {
                        let orig = path.utf8_text(source).unwrap_or("").to_string();
                        let local = alias.utf8_text(source).unwrap_or("").to_string();
                        if !local.is_empty() {
                            specifiers.push(ImportedName {
                                local_name: local,
                                original_name: Some(orig),
                                is_default: false,
                                site: Some(crate::adapter::site_from_node(&child)),
                            });
                        }
                    }
                }
            }
            _ => {}
        }
    }

    if module_path.is_empty() && specifiers.is_empty() {
        return None;
    }

    Some(FileImport {
        site: crate::adapter::site_from_node(node),
        module_path,
        specifiers,
    })
}

/// Check if a function node has a `#[test]` attribute.
fn has_test_attribute(node: &tree_sitter::Node, source: &[u8]) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "attribute_item" {
            let text = child.utf8_text(source).unwrap_or("");
            if text.contains("test") {
                return true;
            }
        }
    }
    // Also check preceding sibling attributes
    let mut prev = node.prev_sibling();
    while let Some(p) = prev {
        if p.kind() == "attribute_item" {
            let text = p.utf8_text(source).unwrap_or("");
            if text.contains("test") {
                return true;
            }
        } else {
            break;
        }
        prev = p.prev_sibling();
    }
    false
}

/// Recursively extract test functions from mod blocks and impl blocks.
fn extract_rust_tests_from_block(
    node: &tree_sitter::Node,
    source: &[u8],
    tests: &mut Vec<ExtractedTest>,
) {
    if node.kind() == "mod_item" {
        if let Some(body) = node.child_by_field_name("body") {
            let mut cursor = body.walk();
            for child in body.children(&mut cursor) {
                if child.kind() == "function_item" && has_test_attribute(&child, source) {
                    if let Some(name_node) = child.child_by_field_name("name") {
                        let name = name_node.utf8_text(source).unwrap_or("").to_string();
                        tests.push(ExtractedTest {
                            name,
                            kind: ExtractedTestKind::Unit,
                            runner: "cargo".to_string(),
                        });
                    }
                }
                extract_rust_tests_from_block(&child, source, tests);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_macro_rules_definition_as_macro_entity() {
        // A `macro_rules!` definition is a first-class declarative-macro symbol.
        // Without an extraction arm for it the macro name is invisible to the
        // graph, so a query for the macro misses at ingestion time, not ranking.
        let adapter = RustAdapter;
        let source = br#"
macro_rules! assemble_widget {
    ($n:expr) => { $n + 1 };
    ($a:expr, $b:expr) => { $a + $b };
}
"#;
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("src/widget.rs");
        let output = adapter.extract(&tree, source, &file_id).unwrap();

        let macros: Vec<_> = output
            .entities
            .iter()
            .filter(|e| e.kind == EntityKind::Macro)
            .collect();
        assert_eq!(
            macros.len(),
            1,
            "macro_rules! definition should yield exactly one Macro entity, got {:?}",
            output
                .entities
                .iter()
                .map(|e| (&e.kind, &e.name))
                .collect::<Vec<_>>()
        );
        assert_eq!(macros[0].name, "assemble_widget");
        assert!(
            macros[0].signature.contains("assemble_widget"),
            "signature should carry the macro name, got {:?}",
            macros[0].signature
        );
    }

    #[test]
    fn extracts_calls_inside_macro_token_trees() {
        // Macro arguments are token trees, not parsed expressions: without
        // token-run reconstruction, a consumer calling through `format!` has
        // no Calls edge and disappears from cross-file impact entirely.
        let adapter = RustAdapter;
        let source = br#"
pub fn render_invoice(amount: u64) -> String {
    format!("total: {}", compute_total(amount))
}

fn callers() {
    assert_eq!(outer(inner(1)), billing::qualified(2));
    println!("{}", target.method(3));
    let v = vec![element(4)];
    log!("{}", matches!(v, _));
}
"#;
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("src/invoice.rs");
        let output = adapter.extract(&tree, source, &file_id).unwrap();

        let calls: Vec<(&str, &str)> = output
            .relations
            .iter()
            .filter(|r| r.kind == kin_model::RelationKind::Calls)
            .map(|r| (r.src_name.as_str(), r.dst_name.as_str()))
            .collect();

        // The cross-file consumer edge the shadow gate depends on.
        assert!(
            calls.contains(&("render_invoice", "compute_total")),
            "call inside format! must be extracted, got {calls:?}"
        );
        // Nested argument-list calls, qualified paths, method calls, and
        // bracket-delimited macro bodies are all reconstructed.
        for expected in [
            ("callers", "outer"),
            ("callers", "inner"),
            ("callers", "billing::qualified"),
            ("callers", "method"),
            ("callers", "element"),
        ] {
            assert!(
                calls.contains(&expected),
                "expected {expected:?} in {calls:?}"
            );
        }
        // `matches!` is a nested macro use inside the token tree, not a call.
        assert!(
            !calls.iter().any(|(_, dst)| *dst == "matches"),
            "nested macro names must not become Calls edges, got {calls:?}"
        );
    }

    #[test]
    fn macro_token_tree_declarations_are_not_calls() {
        let adapter = RustAdapter;
        let source = br#"
fn build() {
    quote! {
        async fn #name() { assert!(ready()); emit(value()); }
        fn ordinary() { format!("{}", render()); }
        #[inline]
        fn attributed() { body_call(); }
    }
}
"#;
        let tree = adapter.parse(source).unwrap();
        assert!(
            !tree.root_node().has_error(),
            "{}",
            tree.root_node().to_sexp()
        );
        let output = adapter
            .extract(&tree, source, &FilePathId::new("src/quoted.rs"))
            .unwrap();
        let calls: Vec<_> = output
            .relations
            .iter()
            .filter(|r| r.kind == kin_model::RelationKind::Calls)
            .collect();
        for declaration in ["name", "ordinary", "attributed"] {
            assert!(
                !calls.iter().any(|r| r.dst_name == declaration),
                "function declaration {declaration} fabricated a call: {calls:?}"
            );
        }
        for callee in ["ready", "emit", "value", "render", "body_call"] {
            let found: Vec<_> = calls.iter().filter(|r| r.dst_name == callee).collect();
            assert_eq!(
                found.len(),
                1,
                "real nested call {callee} lost/duplicated: {calls:?}"
            );
            assert_eq!(found[0].src_name, "build");
            let site = found[0].site.as_ref().unwrap();
            assert!(source[site.start_byte..site.end_byte].starts_with(callee.as_bytes()));
        }
    }

    #[test]
    fn macro_token_tree_declarations_ignore_intervening_comments() {
        let adapter = RustAdapter;
        let source = br#"
fn build() {
    quote! {
        fn /* declaration */ named() { body_call(); }
        async fn # /* interpolation */ generated() { nested(call()); }
        # /* interpolated tokens, not a known callee */ callback();
    }
}
"#;
        let tree = adapter.parse(source).unwrap();
        assert!(
            !tree.root_node().has_error(),
            "{}",
            tree.root_node().to_sexp()
        );
        let output = adapter
            .extract(&tree, source, &FilePathId::new("src/comments.rs"))
            .unwrap();
        let calls: Vec<_> = output
            .relations
            .iter()
            .filter(|r| r.kind == kin_model::RelationKind::Calls)
            .map(|r| r.dst_name.as_str())
            .collect();
        for declaration in ["named", "generated", "callback"] {
            assert!(
                !calls.contains(&declaration),
                "non-call {declaration} in {calls:?}"
            );
        }
        for callee in ["body_call", "nested", "call"] {
            assert!(
                calls.contains(&callee),
                "real call {callee} missing from {calls:?}"
            );
        }
    }

    #[test]
    fn macro_token_tree_declarations_keep_metavariables_unresolved() {
        let adapter = RustAdapter;
        let source = br#"
fn build() {
    quote! {
        macro_rules! generated {
            ($name:ident, $callback:ident) => {
                fn $name() { $callback(); stable_call(); }
                fn literal() { other_call(); }
            };
        }
    }
}
"#;
        let tree = adapter.parse(source).unwrap();
        assert!(
            !tree.root_node().has_error(),
            "{}",
            tree.root_node().to_sexp()
        );
        let output = adapter
            .extract(&tree, source, &FilePathId::new("src/rules.rs"))
            .unwrap();
        let calls: Vec<_> = output
            .relations
            .iter()
            .filter(|r| r.kind == kin_model::RelationKind::Calls)
            .map(|r| r.dst_name.as_str())
            .collect();
        for declaration in ["name", "$name", "callback", "$callback", "literal"] {
            assert!(
                !calls.contains(&declaration),
                "unresolved/declaration {declaration} in {calls:?}"
            );
        }
        for callee in ["stable_call", "other_call"] {
            assert!(
                calls.contains(&callee),
                "real quoted body call {callee} missing from {calls:?}"
            );
        }
    }

    #[test]
    fn plain_macro_rules_is_module_private() {
        let adapter = RustAdapter;
        let source = b"macro_rules! internal_only { () => {}; }";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("src/lib.rs");
        let output = adapter.extract(&tree, source, &file_id).unwrap();
        let m = output
            .entities
            .iter()
            .find(|e| e.kind == EntityKind::Macro && e.name == "internal_only")
            .expect("macro_rules! should be extracted as a Macro entity");
        assert_eq!(
            m.visibility,
            Visibility::Private,
            "a macro_rules! without #[macro_export] is module-local"
        );
    }

    #[test]
    fn macro_export_promotes_macro_to_public() {
        let adapter = RustAdapter;
        let source = br#"
#[macro_export]
macro_rules! public_api {
    () => {};
}
"#;
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("src/lib.rs");
        let output = adapter.extract(&tree, source, &file_id).unwrap();
        let m = output
            .entities
            .iter()
            .find(|e| e.kind == EntityKind::Macro && e.name == "public_api")
            .expect("exported macro should be extracted");
        assert_eq!(
            m.visibility,
            Visibility::Public,
            "#[macro_export] promotes a macro to crate-public visibility"
        );
    }

    #[test]
    fn parse_rust_function() {
        let adapter = RustAdapter;
        let source = b"pub fn add(a: i32, b: i32) -> i32 { a + b }";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("lib.rs");
        let output = adapter.extract(&tree, source, &file_id).unwrap();
        assert!(matches!(output.parse_state, ParseState::Valid));
        let funcs: Vec<_> = output
            .entities
            .iter()
            .filter(|e| e.kind == EntityKind::Function)
            .collect();
        assert_eq!(funcs.len(), 1);
        assert_eq!(funcs[0].name, "add");
        assert_eq!(funcs[0].visibility, Visibility::Public);
    }

    #[test]
    fn parse_rust_struct_and_impl() {
        let adapter = RustAdapter;
        let source = br#"
pub struct Dog {
    name: String,
}

impl Dog {
    pub fn new(name: String) -> Self {
        Self { name }
    }

    pub fn bark(&self) -> &str {
        "woof"
    }
}
"#;
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("dog.rs");
        let output = adapter.extract(&tree, source, &file_id).unwrap();

        let structs: Vec<_> = output
            .entities
            .iter()
            .filter(|e| e.kind == EntityKind::Class)
            .collect();
        assert_eq!(structs.len(), 1);
        assert_eq!(structs[0].name, "Dog");

        let methods: Vec<_> = output
            .entities
            .iter()
            .filter(|e| e.kind == EntityKind::Method)
            .collect();
        assert_eq!(methods.len(), 2);
    }

    #[test]
    fn parse_rust_trait() {
        let adapter = RustAdapter;
        let source = b"pub trait Animal { fn speak(&self) -> String; }";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("traits.rs");
        let output = adapter.extract(&tree, source, &file_id).unwrap();
        let traits: Vec<_> = output
            .entities
            .iter()
            .filter(|e| e.kind == EntityKind::TraitDef)
            .collect();
        assert_eq!(traits.len(), 1);
        assert_eq!(traits[0].name, "Animal");
    }

    #[test]
    fn parse_rust_enum() {
        let adapter = RustAdapter;
        let source = b"pub enum Color { Red, Green, Blue }";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("color.rs");
        let output = adapter.extract(&tree, source, &file_id).unwrap();
        let enums: Vec<_> = output
            .entities
            .iter()
            .filter(|e| e.kind == EntityKind::EnumDef)
            .collect();
        assert_eq!(enums.len(), 1);
        assert_eq!(enums[0].name, "Color");
    }

    #[test]
    fn parse_rust_function_calls() {
        let adapter = RustAdapter;
        let source = br#"
fn process(data: &str) -> String {
    let result = parse(data);
    helper::transform(result)
}
"#;
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("lib.rs");
        let output = adapter.extract(&tree, source, &file_id).unwrap();

        let calls: Vec<_> = output
            .relations
            .iter()
            .filter(|r| r.kind == kin_model::RelationKind::Calls)
            .collect();
        assert!(!calls.is_empty(), "should extract at least one call");

        let call_names: Vec<&str> = calls.iter().map(|r| r.dst_name.as_str()).collect();
        assert!(call_names.contains(&"parse"), "should find call to parse()");
        assert!(
            call_names.contains(&"helper::transform"),
            "should find call to helper::transform()"
        );

        // All calls should have process as the src_name
        for call in &calls {
            assert_eq!(call.src_name, "process");
        }
    }

    #[test]
    fn parse_rust_method_calls() {
        let adapter = RustAdapter;
        let source = br#"
fn do_work(items: Vec<String>) -> usize {
    items.len()
}
"#;
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("lib.rs");
        let output = adapter.extract(&tree, source, &file_id).unwrap();

        let calls: Vec<_> = output
            .relations
            .iter()
            .filter(|r| r.kind == kin_model::RelationKind::Calls)
            .collect();
        let call_names: Vec<&str> = calls.iter().map(|r| r.dst_name.as_str()).collect();
        assert!(
            call_names.contains(&"len"),
            "should find method call len(), found: {:?}",
            call_names
        );
    }

    #[test]
    fn parse_rust_use_statement() {
        let adapter = RustAdapter;
        let source = b"use std::collections::HashMap;";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("lib.rs");
        let output = adapter.extract(&tree, source, &file_id).unwrap();

        assert_eq!(output.imports.len(), 1);
        let import = &output.imports[0];
        assert_eq!(import.module_path, "std::collections");
        assert_eq!(import.specifiers.len(), 1);
        assert_eq!(import.specifiers[0].local_name, "HashMap");
        assert!(import.specifiers[0].original_name.is_none());
    }

    #[test]
    fn parse_rust_use_group() {
        let adapter = RustAdapter;
        let source = b"use std::io::{Read, Write};";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("lib.rs");
        let output = adapter.extract(&tree, source, &file_id).unwrap();

        assert_eq!(output.imports.len(), 1);
        let import = &output.imports[0];
        assert_eq!(import.module_path, "std::io");
        assert_eq!(import.specifiers.len(), 2);

        let names: Vec<&str> = import
            .specifiers
            .iter()
            .map(|s| s.local_name.as_str())
            .collect();
        assert!(names.contains(&"Read"));
        assert!(names.contains(&"Write"));
    }

    #[test]
    fn parse_rust_enum_variants() {
        let adapter = RustAdapter;
        let source = br#"
pub enum Color {
    Red,
    Green,
    Blue,
}
"#;
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("color.rs");
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
        assert_eq!(variants.len(), 3, "should extract 3 enum variants");
        let variant_names: Vec<&str> = variants.iter().map(|v| v.name.as_str()).collect();
        assert!(variant_names.contains(&"Color::Red"));
        assert!(variant_names.contains(&"Color::Green"));
        assert!(variant_names.contains(&"Color::Blue"));

        // Check Contains relation from enum to variants
        let contains: Vec<_> = output
            .relations
            .iter()
            .filter(|r| r.kind == kin_model::RelationKind::Contains && r.src_name == "Color")
            .collect();
        assert_eq!(contains.len(), 3, "should have 3 Contains relations");
    }

    #[test]
    fn extract_doc_comment_on_function() {
        let adapter = RustAdapter;
        let source = br#"
/// Adds two numbers together.
/// Returns the sum.
pub fn add(a: i32, b: i32) -> i32 { a + b }
"#;
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("lib.rs");
        let output = adapter.extract(&tree, source, &file_id).unwrap();
        let func = output
            .entities
            .iter()
            .find(|e| e.name == "add")
            .expect("should find add");
        assert_eq!(
            func.doc_summary.as_deref(),
            Some("Adds two numbers together. Returns the sum.")
        );
    }

    /// The span covers the docs and attributes; the id keys on the declaration.
    ///
    /// The id assertion is the one the captain's ruling rests on: an entity's id
    /// hashes its file, kind, name and a line, and that line stays the `pub fn`
    /// line however much documentation sits above it, so a store built before
    /// this rule addresses the same entity after it.
    #[test]
    fn the_span_covers_the_docs_while_the_id_keys_on_the_declaration_line() {
        let adapter = RustAdapter;
        let source = br#"
/// Calculates the square of a number.
#[inline]
#[must_use]
pub fn square(x: i32) -> i32 {
    x * x
}
"#;
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("math.rs");
        let output = adapter.extract(&tree, source, &file_id).unwrap();
        let func = output
            .entities
            .iter()
            .find(|e| e.name == "square")
            .expect("should find square");
        assert_eq!(func.span.start_line, 1);
        assert_eq!(func.declaration_line, Some(4));
        let text = std::str::from_utf8(&source[func.span.start_byte..func.span.end_byte]).unwrap();
        assert!(text.starts_with("/// Calculates"), "{text}");
        assert!(
            text.contains("#[must_use]") && text.ends_with('}'),
            "{text}"
        );
        assert_eq!(
            func.doc_summary.as_deref(),
            Some("Calculates the square of a number.")
        );

        let entity = func.clone().into_entity_with_source(
            kin_model::LanguageId::Rust,
            &file_id,
            Some(source),
        );
        let at_declaration = kin_model::EntityId::from_content("math.rs", "square", "Function", 4);
        let at_span_start = kin_model::EntityId::from_content("math.rs", "square", "Function", 1);
        assert_eq!(
            entity.id, at_declaration,
            "the id must key on the pub fn line"
        );
        assert_ne!(entity.id, at_span_start);
        assert_eq!(
            entity
                .metadata
                .extra
                .get(crate::extract::DECLARATION_LINE_KEY),
            Some(&serde_json::Value::from(4u32))
        );
        assert_eq!(entity.span.as_ref().map(|span| span.start_line), Some(1));
    }

    /// An item with nothing above it records no declaration line: its span already
    /// starts there, and the metadata key exists only for the widened case.
    #[test]
    fn an_undocumented_item_keeps_its_span_and_records_no_declaration_line() {
        let adapter = RustAdapter;
        let source = b"pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("math.rs");
        let output = adapter.extract(&tree, source, &file_id).unwrap();
        let func = output.entities.iter().find(|e| e.name == "add").unwrap();
        assert_eq!(func.span.start_line, 0);
        assert_eq!(func.declaration_line, Some(0));
        let entity = func.clone().into_entity_with_source(
            kin_model::LanguageId::Rust,
            &file_id,
            Some(source),
        );
        assert_eq!(
            entity.id,
            kin_model::EntityId::from_content("math.rs", "add", "Function", 0)
        );
        assert!(!entity
            .metadata
            .extra
            .contains_key(crate::extract::DECLARATION_LINE_KEY));
    }

    /// Only real outer doc comments and outer attributes join an item.
    ///
    /// `////` rules, `/**/` and `/***` are plain comments in Rust; `//!` and
    /// `/*!` document the enclosing item; `#![..]` is an inner attribute. None
    /// of them may widen the span or become a summary.
    #[test]
    fn only_outer_doc_comments_and_attributes_join_the_span() {
        let adapter = RustAdapter;
        let cases: [(&[u8], u32); 5] = [
            (b"//// rule above\nfn ruled() {}\n", 1),
            (b"/**/\nfn empty_block() {}\n", 1),
            (b"/*** banner ***/\nfn bannered() {}\n", 1),
            (b"//! inner doc\nfn innered() {}\n", 1),
            (b"#![allow(dead_code)]\nfn inner_attr() {}\n", 1),
        ];
        for (source, expected_line) in cases {
            let tree = adapter.parse(source).unwrap();
            let file_id = FilePathId::new("t.rs");
            let output = adapter.extract(&tree, source, &file_id).unwrap();
            let func = output
                .entities
                .iter()
                .find(|e| e.kind == EntityKind::Function)
                .unwrap();
            let shown = String::from_utf8_lossy(source);
            assert_eq!(func.span.start_line, expected_line, "{shown}");
            assert_eq!(func.declaration_line, Some(expected_line), "{shown}");
            assert_eq!(func.doc_summary, None, "{shown}");
            let text =
                std::str::from_utf8(&source[func.span.start_byte..func.span.end_byte]).unwrap();
            assert!(text.starts_with("fn "), "{shown} -> {text}");
        }
    }

    /// An ordinary comment between a doc comment and its item is walked past by
    /// the span, the summary and the derive list alike, as rustc attaches the doc.
    #[test]
    fn an_ordinary_comment_between_a_doc_comment_and_its_item_is_walked_past() {
        let adapter = RustAdapter;
        let source = br#"/// A point.
// keep the derive list short
#[derive(Clone, Debug)]
/** With a block doc too. */
pub struct Point {
    x: i32,
}
"#;
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("p.rs");
        let output = adapter.extract(&tree, source, &file_id).unwrap();
        let point = output.entities.iter().find(|e| e.name == "Point").unwrap();
        assert_eq!(point.span.start_line, 0);
        assert_eq!(point.declaration_line, Some(4));
        assert_eq!(
            point.doc_summary.as_deref(),
            Some("A point. With a block doc too.")
        );
        let implements: Vec<&str> = output
            .relations
            .iter()
            .filter(|r| r.kind == kin_model::RelationKind::Implements && r.src_name == "Point")
            .map(|r| r.dst_name.as_str())
            .collect();
        assert_eq!(implements, ["Clone", "Debug"]);

        // The control: a plain comment directly above an undocumented item
        // neither starts the span nor becomes a summary.
        let source = b"// not a doc\nfn plain() {}\n";
        let tree = adapter.parse(source).unwrap();
        let output = adapter.extract(&tree, source, &file_id).unwrap();
        let plain = output.entities.iter().find(|e| e.name == "plain").unwrap();
        assert_eq!(plain.span.start_line, 1);
        assert_eq!(plain.doc_summary, None);
    }

    #[test]
    fn no_doc_comment_yields_none() {
        let adapter = RustAdapter;
        let source = b"fn bare() {}";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("lib.rs");
        let output = adapter.extract(&tree, source, &file_id).unwrap();
        let func = output
            .entities
            .iter()
            .find(|e| e.name == "bare")
            .expect("should find bare");
        assert!(func.doc_summary.is_none());
    }

    #[test]
    fn doc_comment_on_struct() {
        let adapter = RustAdapter;
        let source = br#"
/// A point in 2D space.
pub struct Point {
    x: f64,
    y: f64,
}
"#;
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("lib.rs");
        let output = adapter.extract(&tree, source, &file_id).unwrap();
        let s = output
            .entities
            .iter()
            .find(|e| e.name == "Point")
            .expect("should find Point");
        assert_eq!(s.doc_summary.as_deref(), Some("A point in 2D space."));
    }

    #[test]
    fn regular_comment_not_captured_as_doc() {
        let adapter = RustAdapter;
        let source = br#"
// This is a regular comment, not a doc comment.
pub fn helper() {}
"#;
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("lib.rs");
        let output = adapter.extract(&tree, source, &file_id).unwrap();
        let func = output
            .entities
            .iter()
            .find(|e| e.name == "helper")
            .expect("should find helper");
        assert!(
            func.doc_summary.is_none(),
            "regular // comments should not be captured as doc_summary"
        );
    }

    #[test]
    fn detect_derive_macro_implements_relations() {
        let adapter = RustAdapter;
        let source = br#"
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    name: String,
    value: i32,
}

#[derive(PartialEq, Eq, Hash)]
pub enum Status {
    Active,
    Inactive,
}
"#;
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("config.rs");
        let output = adapter.extract(&tree, source, &file_id).unwrap();

        let impls: Vec<_> = output
            .relations
            .iter()
            .filter(|r| r.kind == kin_model::RelationKind::Implements)
            .collect();

        // Config derives Debug, Clone, Serialize, Deserialize
        let config_impls: Vec<&str> = impls
            .iter()
            .filter(|r| r.src_name == "Config")
            .map(|r| r.dst_name.as_str())
            .collect();
        assert!(
            config_impls.contains(&"Debug"),
            "Config should derive Debug"
        );
        assert!(
            config_impls.contains(&"Clone"),
            "Config should derive Clone"
        );
        assert!(
            config_impls.contains(&"Serialize"),
            "Config should derive Serialize"
        );
        assert!(
            config_impls.contains(&"Deserialize"),
            "Config should derive Deserialize"
        );

        // Status derives PartialEq, Eq, Hash
        let status_impls: Vec<&str> = impls
            .iter()
            .filter(|r| r.src_name == "Status")
            .map(|r| r.dst_name.as_str())
            .collect();
        assert!(
            status_impls.contains(&"PartialEq"),
            "Status should derive PartialEq"
        );
        assert!(status_impls.contains(&"Hash"), "Status should derive Hash");
    }

    // ---- Module-scoped extraction ----
    //
    // Inline `mod m { ... }` bodies must be descended into so functions, impls,
    // and nested modules inside them become graph entities and their calls are
    // attributed to the innermost enclosing function. Before this, everything
    // under a module was dropped and module-scoped callers vanished from
    // refs/impact.

    /// Calls written in a nested definition are that definition's, and a call
    /// in the enclosing body that names a nested item spells its entity.
    #[test]
    fn calls_in_nested_definitions_belong_to_them() {
        let source = br#"
fn outer() {
    helper();
    Fake::new().hit();
    struct Fake;
    impl Fake {
        fn new() -> Self { Self::build() }
        fn build() -> Self { Fake }
        fn hit(&self) { self.log(); }
        fn log(&self) {}
    }
    fn helper() { deep(); }
}
fn helper() {}
static CURRENT: u8 = compute();
"#;
        let tree = RustAdapter.parse(source).unwrap();
        let output = RustAdapter
            .extract(&tree, source, &FilePathId::new("src/lib.rs"))
            .unwrap();
        let mut edges = call_edges(&output);
        edges.sort_unstable();
        assert_eq!(
            edges,
            vec![
                ("CURRENT", "compute"),
                ("outer", "hit"),
                ("outer", "outer.<locals>.Fake::new"),
                ("outer", "outer.<locals>.helper"),
                ("outer.<locals>.Fake::hit", "outer.<locals>.Fake::log"),
                ("outer.<locals>.Fake::new", "outer.<locals>.Fake::build"),
                ("outer.<locals>.helper", "deep"),
            ]
        );
        let census = output
            .nested_definitions
            .expect("the adapter takes a census");
        assert!(census.not_admitted.is_empty(), "{census:?}");
    }

    /// A crate root's module takes the crate name its Cargo manifest gives,
    /// read the way Rust code writes it, so a row names which crate's root
    /// holds a declaration: `example_oauth :: main`, never `crate :: main`.
    /// With no manifest known, only a path laid out as a crate root is named
    /// by its crate's directory, and any other `main.rs` or `lib.rs` is a
    /// plain module named by its stem.
    #[test]
    fn a_crate_root_module_is_named_by_its_crate() {
        struct Manifests(Vec<(&'static str, &'static str)>);
        impl crate::project_manifest::ProjectFiles for Manifests {
            fn manifest(
                &self,
                path: &str,
            ) -> std::result::Result<
                Option<std::sync::Arc<str>>,
                crate::project_manifest::UnreadableManifest,
            > {
                Ok(self
                    .0
                    .iter()
                    .find(|(at, _)| *at == path)
                    .map(|(_, text)| std::sync::Arc::from(*text)))
            }
        }
        let manifests = Manifests(vec![
            (
                "examples/oauth/Cargo.toml",
                "[package]\nname = \"example-oauth\"\nedition = \"2021\"\n",
            ),
            (
                "axum-macros/Cargo.toml",
                "[package]\nname = \"axum-macros\"\nedition = \"2021\"\n",
            ),
            (
                "Cargo.toml",
                "[package]\nname = \"tower-http\"\nedition = \"2021\"\n",
            ),
        ]);
        let modules = |path: &str, project: &dyn crate::project_manifest::ProjectFiles| {
            let source = b"mod typed_path;\npub fn derive_typed_path() {}\n";
            let file = FilePathId::new(path);
            let tree = RustAdapter.parse(source).unwrap();
            let unit = RustAdapter.unit_context(&file, project).unwrap();
            RustAdapter
                .extract_in(&tree, source, &file, &unit)
                .unwrap()
                .entities
                .into_iter()
                .filter(|entity| entity.kind == EntityKind::Module)
                .map(|entity| entity.name)
                .collect::<Vec<_>>()
        };
        let none = crate::project_manifest::NoProjectFiles;
        for (path, with_manifests, without) in [
            ("examples/oauth/src/main.rs", "example_oauth", "oauth"),
            ("axum-macros/src/lib.rs", "axum_macros", "axum_macros"),
            ("src/lib.rs", "tower_http", "crate"),
            ("src/bin/tool/main.rs", "tool", "tool"),
            ("src/outer/main.rs", "main", "main"),
            ("axum/src/routing/mod.rs", "routing", "routing"),
        ] {
            assert_eq!(
                modules(path, &manifests),
                vec![with_manifests, "typed_path"],
                "{path} with its manifests"
            );
            assert_eq!(
                modules(path, &none),
                vec![without, "typed_path"],
                "{path} with no manifest known"
            );
        }
    }

    /// A crate root that declares a module carrying its own crate's name
    /// keeps both: the root stays the unit's module, with its whole span and
    /// its whole-unit signature, and still holds its top-level functions, and
    /// `mod foo;` is a second module with an identity of its own.
    #[test]
    fn a_root_declaring_a_module_of_its_own_name_keeps_both_modules() {
        let source = b"mod foo;\npub fn run() {}\npub fn stop() {}\n";
        let file = FilePathId::new("foo/src/lib.rs");
        let tree = RustAdapter.parse(source).unwrap();
        let output = RustAdapter.extract(&tree, source, &file).unwrap();
        let foo: Vec<_> = output
            .entities
            .iter()
            .filter(|entity| entity.kind == EntityKind::Module && entity.name == "foo")
            .collect();
        assert_eq!(foo.len(), 2, "{:?}", output.entities);
        let root = foo
            .iter()
            .find(|entity| crate::module_contains::is_unit_module(entity, &file))
            .expect("the root keeps its whole-unit signature");
        assert_eq!(root.span.start_byte, 0);
        assert_eq!(root.span.end_byte, source.len());
        let contained: Vec<&str> = output
            .relations
            .iter()
            .filter(|relation| {
                relation.kind == kin_model::RelationKind::Contains && relation.src_name == "foo"
            })
            .map(|relation| relation.dst_name.as_str())
            .collect();
        assert!(contained.contains(&"run"), "{contained:?}");
        assert!(contained.contains(&"stop"), "{contained:?}");

        // Both begin on the first line, so they derive one identity; the
        // unit's own module keeps it and the declaration takes its own.
        let mut entities: Vec<_> = output
            .entities
            .iter()
            .cloned()
            .map(|entity| entity.into_entity(LanguageId::Rust, &file))
            .collect();
        crate::separate_shared_identities(&mut entities, &file);
        let ids: Vec<_> = entities
            .iter()
            .filter(|entity| entity.kind == EntityKind::Module && entity.name == "foo")
            .map(|entity| (entity.id, kin_model::is_file_module_surface(entity)))
            .collect();
        assert_eq!(ids.len(), 2);
        assert_ne!(ids[0].0, ids[1].0);
        assert_eq!(ids.iter().filter(|(_, surface)| *surface).count(), 1);
    }

    /// A method call inside a macro records its whole receiver chain, so a
    /// field call is never read as a call through a local of the field's name.
    #[test]
    fn a_macro_method_call_keeps_its_whole_receiver_chain() {
        let source = br#"
impl Engine {
    fn check(&self, store: Cache) {
        assert!(self.store.ok());
        assert!(store.ok());
        assert!(self.pick().ok());
        assert!(self.ready());
    }
}
"#;
        let tree = RustAdapter.parse(source).unwrap();
        let output = RustAdapter
            .extract(&tree, source, &FilePathId::new("src/engine.rs"))
            .unwrap();
        let calls: Vec<(&str, Option<&str>)> = output
            .relations
            .iter()
            .filter(|relation| relation.kind == kin_model::RelationKind::Calls)
            .map(|relation| (relation.dst_name.as_str(), relation.receiver.as_deref()))
            .collect();
        assert_eq!(
            calls,
            vec![
                ("ok", Some("self.store")),
                ("ok", Some("store")),
                ("Engine::pick", None),
                ("ok", Some("self.pick()")),
                ("Engine::ready", None),
            ]
        );
    }

    fn call_edges(output: &crate::extract::ParseOutput) -> Vec<(&str, &str)> {
        output
            .relations
            .iter()
            .filter(|r| r.kind == kin_model::RelationKind::Calls)
            .map(|r| (r.src_name.as_str(), r.dst_name.as_str()))
            .collect()
    }

    fn entity_names(output: &crate::extract::ParseOutput, kind: EntityKind) -> Vec<&str> {
        output
            .entities
            .iter()
            .filter(|e| e.kind == kind)
            .map(|e| e.name.as_str())
            .collect()
    }

    #[test]
    fn comment_only_rust_file_produces_no_entity() {
        let adapter = RustAdapter;
        let source = b"// nothing is declared here\n";
        let tree = adapter.parse(source).unwrap();
        let out = adapter
            .extract(&tree, source, &FilePathId::new("lib/silent0.rs"))
            .unwrap();
        assert!(
            out.entities.is_empty(),
            "a comment-only file produced {:?}",
            entity_names(&out, EntityKind::Module)
        );
        assert!(out.imports.is_empty());
    }

    /// Bytes that parse to nothing but an error node declare nothing either.
    /// The escape record lists the unparsed region, and that alone must not
    /// keep a module entity: one minted from an error would let parse coverage
    /// count a file whose bytes produced no declaration.
    #[test]
    fn an_unparseable_rust_file_produces_no_entity() {
        let source = b"fn (((\n";
        let tree = RustAdapter.parse(source).unwrap();
        assert!(tree.root_node().has_error());
        let out = RustAdapter
            .extract(&tree, source, &FilePathId::new("broken.rs"))
            .unwrap();
        assert!(
            out.entities.is_empty(),
            "an unparseable file produced {:?}",
            entity_names(&out, EntityKind::Module)
        );
        assert!(!matches!(out.parse_state, ParseState::Valid));
    }

    /// A file holding only an item macro declares nothing, but its escape
    /// record lists the call the macro hides, so the file keeps the module
    /// surface that record rides on.
    #[test]
    fn an_item_macro_only_file_keeps_the_surface_its_escape_record_needs() {
        let source = b"thread_local! {\n    static CURRENT: Store = make_store();\n}\n";
        let tree = RustAdapter.parse(source).unwrap();
        let out = RustAdapter
            .extract(&tree, source, &FilePathId::new("src/current.rs"))
            .unwrap();
        assert_eq!(entity_names(&out, EntityKind::Module), vec!["current"]);
    }

    /// `mod defaults` inside `defaults.rs` is a module inside the unit, beside
    /// the unit's own module `defaults`, not merged into it.
    ///
    /// Merging gave the unit's module the declaration's span and signature, so
    /// the unit contained nothing and its top-level functions lost their
    /// module. The declaration keeps its own span, excluding its siblings, and
    /// where the two derive one identity the pipeline separates them.
    #[test]
    fn a_module_declared_under_its_own_file_name_is_a_module_inside_the_unit() {
        let adapter = RustAdapter;
        let source =
            b"// leading sibling context\npub mod defaults {\n    pub fn commands() -> String {\n        String::new()\n    }\n}\npub fn outside() { sibling_only(); }\n";
        let file = FilePathId::new("src/defaults.rs");
        let tree = adapter.parse(source).unwrap();
        let out = adapter.extract(&tree, source, &file).unwrap();
        assert_eq!(
            entity_names(&out, EntityKind::Module),
            vec!["defaults", "defaults"]
        );
        let unit = out
            .entities
            .iter()
            .find(|entity| crate::module_contains::is_unit_module(entity, &file))
            .expect("the unit keeps its own module");
        assert_eq!(unit.span.start_byte, 0);
        assert_eq!(unit.span.end_byte, source.len());
        let declared = out
            .entities
            .iter()
            .find(|entity| entity.kind == EntityKind::Module && entity.declaration_line.is_some())
            .expect("the declaration is a module of its own");
        assert!(entity_names(&out, EntityKind::Function).contains(&"commands"));
        let span = &declared.span;
        let body = std::str::from_utf8(&source[span.start_byte..span.end_byte]).unwrap();
        assert!(body.starts_with("pub mod defaults {"), "{body}");
        assert!(body.ends_with("\n}"), "{body}");
        assert!(!body.contains("sibling_only"), "{body}");
        assert!(!body.contains("leading sibling"), "{body}");
        let held: Vec<&str> = out
            .relations
            .iter()
            .filter(|relation| {
                relation.kind == kin_model::RelationKind::Contains
                    && relation.src_name == "defaults"
                    && relation.site.as_ref().is_some_and(|site| {
                        site.syntactic_role
                            == Some(crate::extract::RelationSyntacticRole::ModuleContains)
                    })
            })
            .map(|relation| relation.dst_name.as_str())
            .collect();
        assert!(held.contains(&"outside"), "{held:?}");
    }

    /// The parse-hole rule still holds for a file whose only entity would be a
    /// surface nothing declared: a file declaring one module keeps it, beside
    /// the unit's own.
    #[test]
    fn a_module_only_file_keeps_its_declaration_when_nothing_else_is_declared() {
        let adapter = RustAdapter;
        let source = b"pub mod thing {}\n";
        let tree = adapter.parse(source).unwrap();
        let out = adapter
            .extract(&tree, source, &FilePathId::new("src/thing.rs"))
            .unwrap();
        assert_eq!(
            entity_names(&out, EntityKind::Module),
            vec!["thing", "thing"],
            "a real declaration is not a hollow surface"
        );
        assert!(out
            .entities
            .iter()
            .any(|entity| entity.kind == EntityKind::Module && entity.declaration_line.is_some()));
    }

    #[test]
    fn a_declared_function_still_carries_the_file_module_first() {
        let adapter = RustAdapter;
        let source = b"pub fn answer() -> u8 { 1 }\n";
        let tree = adapter.parse(source).unwrap();
        let out = adapter
            .extract(&tree, source, &FilePathId::new("lib/loud.rs"))
            .unwrap();
        assert_eq!(out.entities[0].kind, EntityKind::Module);
        assert_eq!(out.entities[0].name, "loud");
        assert!(entity_names(&out, EntityKind::Function).contains(&"answer"));
    }

    #[test]
    fn mod_nested_function_and_call_are_extracted() {
        let adapter = RustAdapter;
        let source = br#"
mod handlers {
    pub fn review() {
        do_work(1);
    }
}
"#;
        let tree = adapter.parse(source).unwrap();
        let out = adapter
            .extract(&tree, source, &FilePathId::new("src/lib.rs"))
            .unwrap();

        assert!(
            entity_names(&out, EntityKind::Function).contains(&"review"),
            "module-scoped fn `review` should be an entity, got {:?}",
            entity_names(&out, EntityKind::Function)
        );
        assert!(
            call_edges(&out).contains(&("review", "do_work")),
            "the call should attribute to the enclosing fn `review`, got {:?}",
            call_edges(&out)
        );
    }

    #[test]
    fn deeply_nested_mod_functions_are_extracted() {
        let adapter = RustAdapter;
        let source = br#"
mod outer {
    mod inner {
        fn deep() {
            leaf_call(1);
        }
    }
}
"#;
        let tree = adapter.parse(source).unwrap();
        let out = adapter
            .extract(&tree, source, &FilePathId::new("src/lib.rs"))
            .unwrap();

        assert!(
            entity_names(&out, EntityKind::Function).contains(&"deep"),
            "doubly-nested fn `deep` should be extracted, got {:?}",
            entity_names(&out, EntityKind::Function)
        );
        assert!(
            call_edges(&out).contains(&("deep", "leaf_call")),
            "call in a doubly-nested fn should attribute to `deep`, got {:?}",
            call_edges(&out)
        );
        let mods = entity_names(&out, EntityKind::Module);
        assert!(mods.contains(&"outer") && mods.contains(&"inner"));
    }

    #[test]
    fn impl_methods_inside_mod_are_extracted() {
        let adapter = RustAdapter;
        let source = br#"
mod model {
    struct Widget;
    impl Widget {
        fn make() {
            build(1);
        }
    }
}
"#;
        let tree = adapter.parse(source).unwrap();
        let out = adapter
            .extract(&tree, source, &FilePathId::new("src/lib.rs"))
            .unwrap();

        assert!(
            entity_names(&out, EntityKind::Method).contains(&"Widget::make"),
            "method inside a module impl should be extracted, got {:?}",
            entity_names(&out, EntityKind::Method)
        );
        assert!(
            call_edges(&out).contains(&("Widget::make", "build")),
            "the method's call should attribute to `Widget::make`, got {:?}",
            call_edges(&out)
        );
    }

    #[test]
    fn qualified_call_inside_mod_keeps_full_path() {
        let adapter = RustAdapter;
        let source = br#"
mod handlers {
    fn review() {
        crate::impact::analyze_impact(1);
    }
}
"#;
        let tree = adapter.parse(source).unwrap();
        let out = adapter
            .extract(&tree, source, &FilePathId::new("src/lib.rs"))
            .unwrap();

        assert!(
            call_edges(&out).contains(&("review", "crate::impact::analyze_impact")),
            "a qualified call inside a module fn should be emitted with its full \
             path for the linker to resolve, got {:?}",
            call_edges(&out)
        );
    }

    #[test]
    fn mod_without_body_is_still_a_module_entity() {
        // `mod other;` (declaration only) must not panic and still yields the
        // module entity without a body to descend into.
        let adapter = RustAdapter;
        let source = b"mod other;\nfn top() { local(1); }\n";
        let tree = adapter.parse(source).unwrap();
        let out = adapter
            .extract(&tree, source, &FilePathId::new("src/lib.rs"))
            .unwrap();

        assert!(entity_names(&out, EntityKind::Module).contains(&"other"));
        assert!(call_edges(&out).contains(&("top", "local")));
    }

    #[test]
    fn trait_impl_methods_declare_the_trait_they_satisfy() {
        let adapter = RustAdapter;
        let source = br#"
pub struct Summary;

impl Sink for Summary {
    fn matched(&mut self) -> bool { true }
    fn finish(&mut self) {}
}

impl Summary {
    fn helper(&self) -> u32 { 1 }
}
"#;
        let tree = adapter.parse(source).unwrap();
        let out = adapter
            .extract(&tree, source, &FilePathId::new("src/summary.rs"))
            .unwrap();

        let implements: Vec<(&str, &str)> = out
            .relations
            .iter()
            .filter(|r| r.kind == kin_model::RelationKind::Implements)
            .map(|r| (r.src_name.as_str(), r.dst_name.as_str()))
            .collect();

        assert!(
            implements.contains(&("Summary::matched", "Sink")),
            "a trait method must name the trait it satisfies: {implements:?}"
        );
        assert!(
            implements.contains(&("Summary::finish", "Sink")),
            "every method of the impl block satisfies the trait: {implements:?}"
        );
        assert!(
            implements.contains(&("Summary", "Sink")),
            "the implementing type keeps its own edge: {implements:?}"
        );
        assert!(
            !implements.iter().any(|(src, _)| *src == "Summary::helper"),
            "an inherent method satisfies no trait, so nothing shields it from a \
             dead-code report: {implements:?}"
        );
    }

    /// A named struct field is a field entity of its own, owner-qualified,
    /// contained by its struct, and spanning its documentation and
    /// attributes; a tuple struct's unnamed fields stay out.
    #[test]
    fn named_struct_fields_are_field_entities() {
        let source = b"pub struct Account {\n    /// The balance.\n    #[serde(default)]\n    pub balance: i64,\n    owner: String,\n}\n\npub struct Pair(i64, i64);\n";
        let tree = RustAdapter.parse(source).unwrap();
        let output = RustAdapter
            .extract(&tree, source, &FilePathId::new("src/lib.rs"))
            .unwrap();
        let fields: Vec<_> = output
            .entities
            .iter()
            .filter(|entity| entity.kind == EntityKind::Field)
            .collect();
        let names: Vec<&str> = fields.iter().map(|field| field.name.as_str()).collect();
        assert_eq!(names, ["Account::balance", "Account::owner"]);
        let balance = fields[0];
        assert_eq!(
            &source[balance.span.start_byte..balance.span.end_byte],
            b"/// The balance.\n    #[serde(default)]\n    pub balance: i64"
        );
        assert_eq!(balance.visibility, Visibility::Public);
        assert_eq!(fields[1].visibility, Visibility::Private);
        assert!(output.relations.iter().any(|relation| {
            relation.kind == kin_model::RelationKind::Contains
                && relation.src_name == "Account"
                && relation.dst_name == "Account::owner"
        }));
    }
}
