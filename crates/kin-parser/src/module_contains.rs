// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! A source unit's module owning the declarations written at its top.
//!
//! Every adapter that gives a unit a module entity of its own says which
//! declarations that module holds the same way: a `Contains` relation from
//! the module to each declaration no other declaration encloses and no type
//! already owns, so a reader can place a top-level function in its module the
//! way it places a method in its class. The relation names its child by the
//! child's exact span and carries the [`RelationSyntacticRole::ModuleContains`]
//! role, because a module and the declaration it is named after often share a
//! name (`Button.tsx` declares `Button`), and only the span and the module's
//! kind tell them apart when the linker resolves the pair.
//!
//! An adapter opts in by appending [`module_contains`]'s relations to what it
//! extracted. The rule reads spans and kinds only, so it is the same for every
//! language that calls it.

use std::collections::HashSet;

use kin_model::{EntityKind, FilePathId, RelationKind, SourceSpan};

use crate::extract::{ExtractedEntity, ExtractedRelation, RelationSite, RelationSyntacticRole};

/// Whether `entity` is `file`'s own module: the whole-unit module, package
/// or namespace entity an adapter mints for the unit, whose signature names
/// the unit's path.
pub fn is_unit_module(entity: &ExtractedEntity, file: &FilePathId) -> bool {
    matches!(entity.kind, EntityKind::Module | EntityKind::Package)
        && ["module ", "package ", "namespace "].iter().any(|prefix| {
            entity
                .signature
                .strip_prefix(prefix)
                .is_some_and(|tail| tail == file.0)
        })
}

/// The site a containment relation names its child by: the child's whole
/// span, with the role that says which kind of owner holds it.
pub fn containment_site(span: &SourceSpan, role: RelationSyntacticRole) -> RelationSite {
    RelationSite {
        start_byte: span.start_byte,
        end_byte: span.end_byte,
        start_line: span.start_line,
        start_col: span.start_col,
        end_line: span.end_line,
        end_col: span.end_col,
        syntactic_role: Some(role),
    }
}

/// The `Contains` relations from `file`'s own module to each declaration
/// written at the unit's top: every entity whose span no other entity's span
/// encloses, which no `Contains` in `relations` already gives an owner (a Go
/// method its receiver type), other than the module itself. Empty when the
/// unit has no module entity.
pub fn module_contains(
    entities: &[ExtractedEntity],
    relations: &[ExtractedRelation],
    file: &FilePathId,
) -> Vec<ExtractedRelation> {
    let mut modules = entities
        .iter()
        .enumerate()
        .filter(|(_, entity)| is_unit_module(entity, file));
    let Some((module_index, module)) = modules.next() else {
        return Vec::new();
    };
    if modules.next().is_some() {
        // Two whole-unit modules name no single owner.
        return Vec::new();
    }
    let owned: HashSet<&str> = relations
        .iter()
        .filter(|relation| relation.kind == RelationKind::Contains)
        .map(|relation| relation.dst_name.as_str())
        .collect();
    // Ascending by start and, among equal starts, widest first, so every
    // entity that could enclose one comes before it. Entities that share a
    // span enclose one another and none of them is nested for it.
    let mut order: Vec<usize> = (0..entities.len())
        .filter(|index| {
            *index != module_index
                && entities[*index].kind != EntityKind::File
                && entities[*index].span.start_byte < entities[*index].span.end_byte
        })
        .collect();
    order.sort_by_key(|index| {
        let span = &entities[*index].span;
        (span.start_byte, std::cmp::Reverse(span.end_byte))
    });
    let mut top = vec![false; entities.len()];
    let mut furthest_end = 0usize;
    let mut at = 0;
    while at < order.len() {
        let span = &entities[order[at]].span;
        let group_end = order[at..]
            .iter()
            .position(|index| {
                let other = &entities[*index].span;
                (other.start_byte, other.end_byte) != (span.start_byte, span.end_byte)
            })
            .map_or(order.len(), |offset| at + offset);
        let nested = at > 0 && furthest_end >= span.end_byte;
        for index in &order[at..group_end] {
            top[*index] = !nested;
        }
        furthest_end = furthest_end.max(span.end_byte);
        at = group_end;
    }
    entities
        .iter()
        .enumerate()
        .filter(|(index, entity)| top[*index] && !owned.contains(entity.name.as_str()))
        .map(|(_, entity)| ExtractedRelation {
            kind: RelationKind::Contains,
            src_name: module.name.clone(),
            dst_name: entity.name.clone(),
            import_source: None,
            call_shape: None,
            receiver: None,
            site: Some(containment_site(
                &entity.span,
                RelationSyntacticRole::ModuleContains,
            )),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use kin_model::{FingerprintAlgorithm, Hash256, SemanticFingerprint, Visibility};

    fn entity(
        kind: EntityKind,
        name: &str,
        signature: &str,
        span: (usize, usize),
    ) -> ExtractedEntity {
        ExtractedEntity {
            kind,
            name: name.into(),
            signature: signature.into(),
            visibility: Visibility::Public,
            doc_summary: None,
            fingerprint: SemanticFingerprint {
                algorithm: FingerprintAlgorithm::V1TreeSitter,
                ast_hash: Hash256::from_bytes([0; 32]),
                signature_hash: Hash256::from_bytes([0; 32]),
                behavior_hash: Hash256::from_bytes([0; 32]),
                equivalence_hash: Hash256::from_bytes([0; 32]),
                stability_score: 0.0,
            },
            span: SourceSpan {
                file: FilePathId::new("src/Button.tsx"),
                start_byte: span.0,
                end_byte: span.1,
                start_line: 0,
                start_col: span.0 as u32,
                end_line: 0,
                end_col: span.1 as u32,
            },
            declaration_line: None,
        }
    }

    fn contains(src: &str, dst: &str) -> ExtractedRelation {
        ExtractedRelation {
            kind: RelationKind::Contains,
            src_name: src.into(),
            dst_name: dst.into(),
            import_source: None,
            call_shape: None,
            receiver: None,
            site: None,
        }
    }

    #[test]
    fn a_module_holds_what_no_declaration_encloses_or_type_owns() {
        let file = FilePathId::new("src/Button.tsx");
        let entities = vec![
            entity(
                EntityKind::Function,
                "Button",
                "function Button()",
                (10, 40),
            ),
            entity(EntityKind::Class, "Store", "class Store", (50, 120)),
            entity(EntityKind::Method, "Store.get", "get()", (60, 80)),
            entity(
                EntityKind::Function,
                "Store.<locals>.helper",
                "helper()",
                (85, 95),
            ),
            entity(EntityKind::Method, "Store.put", "put()", (130, 140)),
            entity(EntityKind::Constant, "LIMIT", "LIMIT = 3", (150, 160)),
            entity(
                EntityKind::Module,
                "Button",
                "module src/Button.tsx",
                (0, 200),
            ),
        ];
        // A method written outside its type, as Go writes one, keeps the
        // type as its owner.
        let relations = vec![
            contains("Store", "Store.get"),
            contains("Store", "Store.put"),
        ];
        let found: Vec<(String, String, (usize, usize))> =
            module_contains(&entities, &relations, &file)
                .into_iter()
                .map(|relation| {
                    let site = relation.site.unwrap();
                    assert_eq!(
                        site.syntactic_role,
                        Some(RelationSyntacticRole::ModuleContains)
                    );
                    (
                        relation.src_name,
                        relation.dst_name,
                        (site.start_byte, site.end_byte),
                    )
                })
                .collect();
        assert_eq!(
            found,
            vec![
                ("Button".into(), "Button".into(), (10, 40)),
                ("Button".into(), "Store".into(), (50, 120)),
                ("Button".into(), "LIMIT".into(), (150, 160)),
            ]
        );
    }

    #[test]
    fn a_unit_without_its_own_module_holds_nothing() {
        let file = FilePathId::new("src/Button.tsx");
        let entities = vec![
            entity(EntityKind::Function, "f", "function f()", (0, 10)),
            // A namespace header is a module, but not the unit's own.
            entity(EntityKind::Module, "N", "namespace N", (20, 40)),
        ];
        assert!(module_contains(&entities, &[], &file).is_empty());
    }

    #[test]
    fn declarations_that_share_a_span_are_both_at_the_top() {
        let file = FilePathId::new("src/Button.tsx");
        let entities = vec![
            entity(
                EntityKind::Module,
                "Button",
                "module src/Button.tsx",
                (0, 50),
            ),
            entity(EntityKind::Function, "default", "export default", (0, 20)),
            entity(EntityKind::Function, "Button", "function Button()", (0, 20)),
            entity(EntityKind::Function, "inner", "function inner()", (5, 15)),
        ];
        let names: Vec<String> = module_contains(&entities, &[], &file)
            .into_iter()
            .map(|relation| relation.dst_name)
            .collect();
        assert_eq!(names, vec!["default", "Button"]);
    }
}
