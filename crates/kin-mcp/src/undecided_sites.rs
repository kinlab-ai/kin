// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Reference sites a language server left undecided, named in every answer
//! that reads them.
//!
//! A unit whose References no pass could decide over unchanged inputs is
//! settled rather than owed forever: each such site keeps its record on its
//! edge under [`kin_model::LSP_REFERENCE_UNDECIDED_RULE`], so the reference is
//! still served. Nothing proves it, so an answer that counts it cannot be
//! complete because of it. This reads those records for one focal and lists
//! each site by its caller and its line inside that caller, which is what lets
//! the verdict say the answer is complete except those sites, or partial when
//! the list is cut.

use kin_model::call_site_reading::UNDECIDED_REFERENCE_SITES_KEY;
use kin_model::graph::GraphStore;
use kin_model::{Entity, GraphNodeId, RelationKind, RelationOrigin};
use serde_json::{json, Value};

/// The most undecided sites one answer lists. A larger count is served with
/// the list cut, and a cut list makes the answer partial.
pub const UNDECIDED_SITES_LISTED_MAX: usize = 50;

/// The `undecided_reference_sites` block for `target`, or `None` when the
/// answer asks for no References or none of its References is undecided.
///
/// `{"count": N, "sites": [{"caller", "caller_name", "line_in_entity",
/// "callee"}]}`, the sites ordered by caller and line. A store that cannot be
/// read answers with `count: null` and the reason, never with silence.
pub fn undecided_reference_sites<G: GraphStore>(
    store: &G,
    target: &Entity,
    relation_kinds: &[RelationKind],
) -> Option<Value> {
    if !relation_kinds.contains(&RelationKind::References) {
        return None;
    }
    let relations = match store.get_all_relations_for_entity(&target.id) {
        Ok(relations) => relations,
        Err(_) => {
            return Some(json!({
                "count": null,
                "sites": [],
                "unreadable": "the graph could not read this entity's references",
            }))
        }
    };
    let mut sites: Vec<(String, u32, Value)> = Vec::new();
    for relation in &relations {
        let GraphNodeId::Entity(caller_id) = relation.src else {
            continue;
        };
        if relation.kind != RelationKind::References
            || relation.origin != RelationOrigin::Lsp
            || relation.dst != GraphNodeId::Entity(target.id)
        {
            continue;
        }
        let undecided: Vec<_> = relation
            .evidence
            .iter()
            .filter(|record| {
                record.parser_rule.as_deref() == Some(kin_model::LSP_REFERENCE_UNDECIDED_RULE)
            })
            .collect();
        if undecided.is_empty() {
            continue;
        }
        let caller = store.get_entity(&caller_id).ok().flatten();
        let caller_start = caller.as_ref().and_then(|caller| caller.span.as_ref());
        for record in undecided {
            // The site's line inside its caller, counted from 0 at the caller's
            // first line, the address every reference site is served by.
            let line_in_entity = match (record.source_span.as_ref(), caller_start) {
                (Some(site), Some(start)) if site.start_line >= start.start_line => {
                    Some(site.start_line - start.start_line)
                }
                _ => None,
            };
            let caller_name = caller
                .as_ref()
                .map(|caller| caller.name.clone())
                .unwrap_or_default();
            sites.push((
                caller_id.to_string(),
                line_in_entity.unwrap_or(u32::MAX),
                json!({
                    "caller": caller_id.to_string(),
                    "caller_name": caller_name,
                    "line_in_entity": line_in_entity,
                    "callee": record.token,
                }),
            ));
        }
    }
    if sites.is_empty() {
        return None;
    }
    sites.sort_by(|left, right| (&left.0, left.1).cmp(&(&right.0, right.1)));
    let count = sites.len();
    let listed: Vec<Value> = sites
        .into_iter()
        .take(UNDECIDED_SITES_LISTED_MAX)
        .map(|(_, _, site)| site)
        .collect();
    Some(json!({ "count": count, "sites": listed }))
}

/// Add the block for `target` to a reference answer, when there is one.
pub fn disclose_undecided_reference_sites<G: GraphStore>(
    result: &mut Value,
    store: &G,
    target: &Entity,
    relation_kinds: &[RelationKind],
) {
    if let Some(block) = undecided_reference_sites(store, target, relation_kinds) {
        result[UNDECIDED_REFERENCE_SITES_KEY] = block;
    }
}
