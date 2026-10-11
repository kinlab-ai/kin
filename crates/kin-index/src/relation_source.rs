// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

use kin_model::{is_file_module_surface, Entity, EntityKind, RelationKind};
use kin_parser::{ExtractedRelation, RelationSyntacticRole};
use std::collections::HashMap;

/// A borrowed per-file source index. Build once for a complete parse, then
/// resolve each raw occurrence against only its same-name declaration group.
pub struct RelationSourceIndex<'a> {
    by_name: HashMap<&'a str, Vec<&'a Entity>>,
}

impl<'a> RelationSourceIndex<'a> {
    pub fn new(entities: &'a [Entity]) -> Self {
        let mut by_name: HashMap<&str, Vec<&Entity>> = HashMap::new();
        for entity in entities {
            by_name.entry(&entity.name).or_default().push(entity);
        }
        Self { by_name }
    }

    /// Repeated names need an exact site and a unique innermost containing
    /// span. Declaration order and name-map overwrites establish no ownership.
    pub fn resolve(&self, relation: &ExtractedRelation) -> Option<&'a Entity> {
        if is_lexical_contains(relation) {
            return self
                .resolve_lexical_contains(relation)
                .map(|(parent, _)| parent);
        }
        let named = self.by_name.get(relation.src_name.as_str())?;
        let Some(site) = &relation.site else {
            // Preserve ordinary non-Module binding when a module and a real
            // declaration share a name, without choosing between siblings.
            let has_declaration = named.iter().any(|entity| entity.kind != EntityKind::Module);
            let mut candidates = named
                .iter()
                .copied()
                .filter(|entity| !has_declaration || entity.kind != EntityKind::Module);
            let first = candidates.next()?;
            return candidates.next().is_none().then_some(first);
        };
        if site.start_byte >= site.end_byte {
            return None;
        }
        let candidates = || {
            named.iter().copied().filter_map(|entity| {
                let span = entity.span.as_ref()?;
                (span.start_byte <= site.start_byte && site.end_byte <= span.end_byte)
                    .then_some((span.end_byte - span.start_byte, entity))
            })
        };
        let (width, first) = candidates().min_by_key(|(width, _)| *width)?;
        let innermost = first.span.as_ref()?;
        let mut equally_small = 0;
        for (other_width, entity) in candidates() {
            equally_small += usize::from(other_width == width);
            let span = entity.span.as_ref().unwrap();
            if span.start_byte > innermost.start_byte || span.end_byte < innermost.end_byte {
                // Intersecting but non-nested spans establish no lexical owner.
                return None;
            }
        }
        (equally_small == 1).then_some(first)
    }

    /// A containment occurrence carries its child declaration's exact span
    /// and its parent's scope kind: a unit's own module, or a class or
    /// function. Resolve both identities from this admitted parse; a repeated
    /// name slot cannot substitute for either. The rule reads spans, kinds and
    /// the shared source unit, never a language, so every adapter that emits
    /// the roles resolves the same way.
    pub fn resolve_lexical_contains(
        &self,
        relation: &ExtractedRelation,
    ) -> Option<(&'a Entity, &'a Entity)> {
        if !is_lexical_contains(relation) {
            return None;
        }
        let site = relation.site.as_ref()?;
        if site.start_byte >= site.end_byte {
            return None;
        }
        let mut children = self
            .by_name
            .get(relation.dst_name.as_str())?
            .iter()
            .copied()
            .filter(|entity| {
                !is_file_module_surface(entity)
                    && entity.span.as_ref().is_some_and(|span| {
                        span.start_byte == site.start_byte
                            && span.end_byte == site.end_byte
                            && span.start_line == site.start_line
                            && span.end_line == site.end_line
                            && span.start_col == site.start_col
                            && span.end_col == site.end_col
                            && entity.file_origin.as_ref() == Some(&span.file)
                    })
            });
        let child = children.next()?;
        if children.next().is_some() {
            return None;
        }
        let child_span = child.span.as_ref()?;
        let module_parent = site.syntactic_role == Some(RelationSyntacticRole::ModuleContains);
        let candidates = || {
            self.by_name
                .get(relation.src_name.as_str())
                .into_iter()
                .flatten()
                .copied()
                .filter_map(|entity| {
                    let span = entity.span.as_ref()?;
                    (entity.id != child.id
                        && entity.language == child.language
                        && if module_parent {
                            is_file_module_surface(entity)
                        } else {
                            matches!(
                                entity.kind,
                                EntityKind::Class
                                    | EntityKind::EnumDef
                                    | EntityKind::Function
                                    | EntityKind::Method
                            )
                        }
                        && entity.file_origin.is_some()
                        && entity.file_origin == child.file_origin
                        && span.file == child_span.file
                        && span.start_byte <= child_span.start_byte
                        && child_span.end_byte <= span.end_byte)
                        .then_some((span.end_byte - span.start_byte, entity))
                })
        };
        let (width, parent) = candidates().min_by_key(|(width, _)| *width)?;
        let parent_span = parent.span.as_ref()?;
        let mut equally_small = 0;
        for (other_width, entity) in candidates() {
            equally_small += usize::from(other_width == width);
            let span = entity.span.as_ref()?;
            if span.start_byte > parent_span.start_byte || span.end_byte < parent_span.end_byte {
                return None;
            }
        }
        (equally_small == 1).then_some((parent, child))
    }
}

pub(crate) fn is_lexical_contains(relation: &ExtractedRelation) -> bool {
    relation.kind == RelationKind::Contains
        && matches!(
            relation.site.as_ref().and_then(|site| site.syntactic_role),
            Some(RelationSyntacticRole::ModuleContains | RelationSyntacticRole::LexicalContains)
        )
}

/// Resolve one raw occurrence without retaining an index. Callers processing
/// a whole file should reuse [`RelationSourceIndex`] instead.
pub fn relation_source_entity<'a>(
    relation: &ExtractedRelation,
    entities: &'a [Entity],
) -> Option<&'a Entity> {
    RelationSourceIndex::new(entities).resolve(relation)
}
