// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Source-owning class attributes and named entries of literal dictionaries.

use std::collections::HashMap;

use tree_sitter::Node;

use super::*;
use crate::literal_members::{owns_lines, LiteralMember};

pub(super) fn literal_members<'tree>(
    literal: Node<'tree>,
    source: &[u8],
) -> Vec<LiteralMember<'tree>> {
    if literal.kind() != "dictionary" {
        return Vec::new();
    }
    let mut cursor = literal.walk();
    let mut members = Vec::new();
    let mut counts = HashMap::new();
    for pair in literal
        .named_children(&mut cursor)
        .filter(|n| n.kind() == "pair")
    {
        let (Some(key), Some(value)) = (
            pair.child_by_field_name("key"),
            pair.child_by_field_name("value"),
        ) else {
            continue;
        };
        let Some(name) = identifier_key(key, source) else {
            continue;
        };
        *counts.entry(name.clone()).or_insert(0) += 1;
        members.push(LiteralMember {
            name,
            node: pair,
            value,
        });
    }
    members.retain(|member| counts[&member.name] == 1 && owns_lines(member.node, source));
    members
}

fn identifier_key(key: Node<'_>, source: &[u8]) -> Option<String> {
    if key.kind() != "string" {
        return None;
    }
    let text = key.utf8_text(source).ok()?;
    let quote = *text.as_bytes().first()?;
    if !matches!(quote, b'\'' | b'"') || text.as_bytes().last() != Some(&quote) {
        return None;
    }
    let name = text.get(1..text.len().checked_sub(1)?)?;
    let mut chars = name.bytes();
    if !chars
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        || !chars.all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return None;
    }
    Some(name.to_string())
}

fn assignment(member: Node<'_>) -> Option<Node<'_>> {
    let node = if member.kind() == "expression_statement" {
        member.named_child(0)?
    } else {
        member
    };
    (node.kind() == "assignment"
        && node.child_by_field_name("left")?.kind() == "identifier"
        && node
            .child_by_field_name("right")
            .is_none_or(|n| n.kind() != "assignment"))
    .then_some(node)
}

pub(super) fn admit_class_fields(
    body: Node<'_>,
    source: &[u8],
    file: &FilePathId,
    class: &str,
    out: &mut PyExtraction,
) {
    let mut cursor = body.walk();
    let nodes: Vec<_> = body
        .named_children(&mut cursor)
        .filter_map(assignment)
        .collect();
    let mut counts = HashMap::new();
    for node in &nodes {
        let name = node
            .child_by_field_name("left")
            .unwrap()
            .utf8_text(source)
            .unwrap_or("");
        *counts.entry(name).or_insert(0) += 1;
    }
    // A method or nested type can replace an earlier attribute binding too.
    // Keep such same-name declarations explicit rather than selecting one.
    let mut cursor = body.walk();
    for member in body.named_children(&mut cursor) {
        let definition = if member.kind() == "decorated_definition" {
            member.child_by_field_name("definition").unwrap_or(member)
        } else {
            member
        };
        if matches!(
            definition.kind(),
            "function_definition" | "class_definition"
        ) {
            if let Some(name) = definition.child_by_field_name("name") {
                if let Ok(name) = name.utf8_text(source) {
                    *counts.entry(name).or_insert(0) += 1;
                }
            }
        }
    }
    for node in nodes {
        let raw = node
            .child_by_field_name("left")
            .unwrap()
            .utf8_text(source)
            .unwrap_or("");
        if raw.is_empty() || counts[raw] != 1 || !owns_lines(node, source) {
            continue;
        }
        let name = format!("{class}.{raw}");
        push_field(node, source, file, class, &name, out);
        if let Some(value) = node.child_by_field_name("right") {
            admit_literal_fields(value, source, file, &name, out);
        }
    }
}

pub(super) fn admit_literal_fields(
    literal: Node<'_>,
    source: &[u8],
    file: &FilePathId,
    owner: &str,
    out: &mut PyExtraction,
) {
    for member in PythonAdapter.literal_members(literal, source) {
        let name = format!("{owner}.{}", member.name);
        push_field(member.node, source, file, owner, &name, out);
        admit_literal_fields(member.value, source, file, &name, out);
    }
}

fn push_field(
    node: Node<'_>,
    source: &[u8],
    file: &FilePathId,
    owner: &str,
    name: &str,
    out: &mut PyExtraction,
) {
    out.entities.push(ExtractedEntity {
        kind: EntityKind::Field,
        name: name.to_string(),
        signature: node_signature(&node, source),
        visibility: if name.rsplit('.').next().is_some_and(|n| n.starts_with('_')) {
            Visibility::Private
        } else {
            Visibility::Public
        },
        doc_summary: None,
        fingerprint: compute_fingerprint(&node, source),
        span: span_from_node(&node, file),
        declaration_line: None,
    });
    out.relations.push(ExtractedRelation {
        kind: kin_model::RelationKind::Contains,
        src_name: owner.to_string(),
        dst_name: name.to_string(),
        site: Some(python_contains_site(&node, false)),
        receiver: None,
        call_shape: None,
        import_source: None,
    });
}

pub(super) fn assign_field_sites(out: &mut PyExtraction) {
    let owner = |site: &RelationSite| -> Option<String> {
        let candidates: Vec<_> = out
            .entities
            .iter()
            .filter(|e| e.span.start_byte <= site.start_byte && site.end_byte <= e.span.end_byte)
            .collect();
        let first = candidates
            .iter()
            .min_by_key(|e| e.span.end_byte - e.span.start_byte)?;
        (first.kind == EntityKind::Field
            && candidates.iter().all(|other| {
                std::ptr::eq(*other, *first)
                    || other.span.end_byte - other.span.start_byte
                        > first.span.end_byte - first.span.start_byte
            }))
        .then(|| first.name.clone())
    };
    for reference in &mut out.value_refs {
        if let Some(name) = owner(&reference.site) {
            reference.src_name = name;
        }
    }
    for relation in &mut out.relations {
        if relation.kind == kin_model::RelationKind::Calls {
            if let Some(name) = relation.site.as_ref().and_then(&owner) {
                relation.src_name = name;
            }
        }
    }
}
