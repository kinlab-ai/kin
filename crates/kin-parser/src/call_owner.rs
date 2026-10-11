// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The caller of a call written where no declaration's own walk reads it.
//!
//! Every adapter charges a call to the declaration whose body its walk is
//! reading, and that walk starts only at a declaration it admits. A call
//! written anywhere else reached no relation at all:
//!
//! - file-scope code, such as a test file's
//!   `describe('x', () => { it('y', () => { foo() }) })`, Python's
//!   `if __name__ == "__main__": main()`, a Go package-level `var c = New()`
//!   or a PHP script's top-level statements;
//! - class-body code outside any member, such as a Java static initializer or
//!   a Kotlin `init` block;
//! - the body of a nested definition an adapter recorded rather than admitted
//!   (see [`crate::nested`]), when nothing else walked it.
//!
//! A file made of such code read as a file with no calls, and an adapter that
//! counts its call sites off its relations stamped that zero on every entity.
//!
//! So each adapter runs its own call walk once more over the whole tree, with
//! no declaration context, and hands the result to [`adopt_unowned_calls`]. A
//! call whose site a declaration walk already accounted for is dropped, because
//! that walk named it with context only it has (the owner a `this.m()` call
//! qualifies to, the frames a local shadows). Every other call is charged to the
//! innermost admitted entity whose span holds it: a nested definition that was
//! not admitted leaves its calls with the entity around it, and file-scope code
//! leaves them with the file's module entity. That is the owner Python already
//! gives module-scope references and the one an import-time call has in impact
//! analysis. The linker resolves a relation's source by name and site together,
//! so a module and a declaration that share a name still resolve apart.
//!
//! Nothing here admits an entity: which definitions become entities is each
//! adapter's rule, and this pass only makes sure no call is lost to it.

use std::collections::HashSet;

use kin_model::{EntityKind, RelationKind};

use crate::extract::{
    is_scoped_call_extraction_incomplete_marker, ExtractedEntity, ExtractedRelation, RelationSite,
};

/// A call site's byte range, the identity two walks agree on for one call.
pub type SiteKey = (usize, usize);

/// The byte range of `site`.
pub fn site_key(site: &RelationSite) -> SiteKey {
    (site.start_byte, site.end_byte)
}

/// The call sites the declaration walks already accounted for: the site of
/// every `Calls` relation and of every scoped coverage marker, which is how a
/// walk reports a call it saw and could not name.
pub fn accounted_call_sites(relations: &[ExtractedRelation]) -> HashSet<SiteKey> {
    relations
        .iter()
        .filter(|relation| {
            relation.kind == RelationKind::Calls
                || is_scoped_call_extraction_incomplete_marker(relation)
        })
        .filter_map(|relation| relation.site.as_ref())
        .map(site_key)
        .collect()
}

/// The innermost admitted entity whose span holds `site`.
///
/// Of two entities with the same span a declaration wins over a module, and
/// the earlier entity wins otherwise, so the choice depends on nothing but the
/// entity list.
pub fn innermost_owner<'a>(
    entities: &'a [ExtractedEntity],
    site: &RelationSite,
) -> Option<&'a ExtractedEntity> {
    entities
        .iter()
        .filter(|entity| {
            entity.span.start_byte <= site.start_byte && site.end_byte <= entity.span.end_byte
        })
        .min_by_key(|entity| {
            (
                entity.span.end_byte - entity.span.start_byte,
                entity.kind == EntityKind::Module,
            )
        })
}

/// Charge the calls of a whole-file walk that no declaration walk accounted
/// for to the innermost admitted entity holding each one, and append them.
///
/// `swept` is the adapter's own call walk over the whole tree. Only its `Calls`
/// relations and scoped coverage markers are read, and only at a site outside
/// `accounted`; everything else it produced (value references, records at a
/// site that is not a call) is discarded, because the declaration walks
/// already emitted what they own.
///
/// A call no admitted entity holds is left out, as it was before this pass.
/// Only a file without a module entity can have one, which means C and C++,
/// and there it is almost always a macro that expands to a declaration
/// (`EXPORT_SYMBOL(f);`, `module_init(f);`, a test-registration macro). On a
/// sample of 732 Linux kernel files, 445 held one. Marking each such file as
/// not wholly extracted would withdraw its call count over syntax that makes
/// no runtime call.
///
/// Returns the indices into `swept` of the relations appended, in order, for
/// an adapter that carries side data per relation.
pub fn adopt_unowned_calls(
    entities: &[ExtractedEntity],
    accounted: &HashSet<SiteKey>,
    swept: Vec<ExtractedRelation>,
    relations: &mut Vec<ExtractedRelation>,
) -> Vec<usize> {
    let mut adopted = Vec::new();
    for (index, mut relation) in swept.into_iter().enumerate() {
        if relation.kind != RelationKind::Calls
            && !is_scoped_call_extraction_incomplete_marker(&relation)
        {
            continue;
        }
        let Some(site) = relation.site.as_ref() else {
            continue;
        };
        if accounted.contains(&site_key(site)) {
            continue;
        }
        let Some(owner) = innermost_owner(entities, site) else {
            continue;
        };
        relation.src_name = owner.name.clone();
        relations.push(relation);
        adopted.push(index);
    }
    adopted
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::LanguageAdapter;
    use kin_model::{FilePathId, SourceSpan, Visibility};

    fn entity(kind: EntityKind, name: &str, start: usize, end: usize) -> ExtractedEntity {
        let tree = crate::languages::JavaScriptAdapter
            .parse(b"x")
            .expect("a one-token file parses");
        ExtractedEntity {
            kind,
            name: name.to_string(),
            signature: String::new(),
            visibility: Visibility::Public,
            doc_summary: None,
            fingerprint: crate::adapter::compute_fingerprint(&tree.root_node(), b"x"),
            span: SourceSpan {
                file: FilePathId::new("a.ts"),
                start_byte: start,
                end_byte: end,
                start_line: 0,
                start_col: 0,
                end_line: 0,
                end_col: 0,
            },
            declaration_line: None,
        }
    }

    fn call(src: &str, dst: &str, start: usize, end: usize) -> ExtractedRelation {
        ExtractedRelation {
            site: Some(RelationSite {
                start_byte: start,
                end_byte: end,
                start_line: 0,
                start_col: 0,
                end_line: 0,
                end_col: 0,
                syntactic_role: None,
            }),
            receiver: None,
            call_shape: None,
            kind: RelationKind::Calls,
            src_name: src.to_string(),
            dst_name: dst.to_string(),
            import_source: None,
        }
    }

    #[test]
    fn an_unowned_call_goes_to_the_innermost_entity_and_a_walked_one_stays() {
        let entities = vec![
            entity(EntityKind::Function, "run", 10, 40),
            entity(EntityKind::Module, "a", 0, 100),
        ];
        let mut relations = vec![call("run", "walked", 12, 20)];
        let accounted = accounted_call_sites(&relations);
        let swept = vec![
            call("", "walked", 12, 20),
            call("", "in_run", 22, 30),
            call("", "at_file_scope", 50, 60),
        ];
        let adopted = adopt_unowned_calls(&entities, &accounted, swept, &mut relations);
        assert_eq!(adopted, vec![1, 2]);
        let edges: Vec<_> = relations
            .iter()
            .map(|r| (r.src_name.as_str(), r.dst_name.as_str()))
            .collect();
        assert_eq!(
            edges,
            vec![("run", "walked"), ("run", "in_run"), ("a", "at_file_scope")]
        );
    }

    #[test]
    fn a_declaration_wins_a_tie_with_a_module_of_the_same_span() {
        let entities = vec![
            entity(EntityKind::Module, "a", 0, 100),
            entity(EntityKind::Class, "Holder", 0, 100),
        ];
        let site = call("", "f", 5, 9).site.unwrap();
        assert_eq!(innermost_owner(&entities, &site).unwrap().name, "Holder");
    }

    #[test]
    fn a_call_no_entity_holds_is_left_out() {
        let entities = vec![entity(EntityKind::Function, "run", 10, 40)];
        let mut relations = Vec::new();
        let swept = vec![call("", "EXPORT_SYMBOL", 50, 55)];
        let adopted = adopt_unowned_calls(&entities, &HashSet::new(), swept, &mut relations);
        assert!(adopted.is_empty());
        assert!(relations.is_empty());
    }
}
