// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Calls whose receiver a guard narrowed to a finite set of classes.
//!
//! An adapter that reads `if isinstance(c, AliasPath): c.convert_to_aliases()`
//! names the guard's class as the call's owner, one relation per class, each
//! tagged [`RelationSyntacticRole::PythonGuardedReceiver`]. The guard proves
//! the receiver is an instance of that class or of a subclass, and a subclass
//! may override the member, so the destination this module binds is the
//! member the guard class itself declares or inherits, stamped
//! [`RECEIVER_ORIGIN_NAME_CONFIDENCE`]: a `name_only` possible target, never a
//! proven call, and never counted as a static proof of its site.
//!
//! A guarded call binds through its class or not at all. When the class does
//! not resolve to one declaration, or declares no such member, the call stays
//! unlinked: falling back to the bare member name would let any same-named
//! member in the repository stand in for the class the guard names.

use std::collections::HashMap;

use kin_model::{FilePathId, Relation, RelationEvidence, RelationKind, RelationOrigin};
use kin_parser::file_evidence::ReceiverGuard;
use kin_parser::{ExtractedRelation, RelationSyntacticRole};

use crate::resolution::RECEIVER_ORIGIN_NAME_CONFIDENCE;

/// Evidence rule on the one span-carrying record of a guarded call
/// occurrence. Its token, when the adapter recorded the guard, is a JSON
/// object naming the guard expression and its span.
pub const GUARDED_RECEIVER_RULE: &str = "python_guarded_receiver_v1";

/// Whether a raw relation is a guard-narrowed call.
pub(crate) fn is_guarded_receiver_call(rel: &ExtractedRelation) -> bool {
    rel.site.as_ref().and_then(|site| site.syntactic_role)
        == Some(RelationSyntacticRole::PythonGuardedReceiver)
}

/// The possible-target edge for one guard class's member.
///
/// The call's single evidence record carries its exact site, its shape, and
/// the guard that admitted the class. No aggregation certificate is stamped:
/// a candidate occurrence cannot vouch that every call on the edge was seen
/// with this shape, so rename review reads it as unknown, which fails closed.
pub(super) fn guarded_receiver_relation(
    rel: &ExtractedRelation,
    src: kin_model::EntityId,
    dst: kin_model::EntityId,
    caller_file: &FilePathId,
    guards: &HashMap<(usize, usize), ReceiverGuard>,
) -> Relation {
    let guard = rel
        .site
        .as_ref()
        .and_then(|site| guards.get(&(site.start_byte, site.end_byte)));
    let mut relation = Relation {
        id: super::stable_relation_id(&src, &dst, &RelationKind::Calls),
        kind: RelationKind::Calls,
        src: kin_model::GraphNodeId::Entity(src),
        dst: kin_model::GraphNodeId::Entity(dst),
        confidence: RECEIVER_ORIGIN_NAME_CONFIDENCE,
        origin: RelationOrigin::Inferred,
        created_in: None,
        import_source: None,
        evidence: vec![RelationEvidence {
            source_span: rel
                .site
                .as_ref()
                .map(|site| site.to_source_span(caller_file)),
            parser_rule: Some(GUARDED_RECEIVER_RULE.to_string()),
            token: guard.map(|guard| guard_token(guard, caller_file)),
            call_shape: rel.call_shape.as_ref().map(|shape| {
                kin_model::CallArgShape::new(
                    shape.positional,
                    shape.keywords.clone(),
                    shape.has_var_positional,
                    shape.has_var_keyword,
                )
            }),
            ..RelationEvidence::default()
        }],
    };
    crate::occurrence::stamp_fresh(&mut relation);
    relation
}

fn guard_token(guard: &ReceiverGuard, caller_file: &FilePathId) -> String {
    serde_json::json!({
        "guard": guard.text,
        "span": guard.site.to_source_span(caller_file),
    })
    .to_string()
}

/// The guard expression and span a guarded call edge's occurrence at
/// `span` records, when it records one.
pub fn guard_of_occurrence(record: &RelationEvidence) -> Option<(String, kin_model::SourceSpan)> {
    if record.parser_rule.as_deref() != Some(GUARDED_RECEIVER_RULE) {
        return None;
    }
    let token: serde_json::Value = serde_json::from_str(record.token.as_deref()?).ok()?;
    Some((
        token.get("guard")?.as_str()?.to_string(),
        serde_json::from_value(token.get("span")?.clone()).ok()?,
    ))
}
