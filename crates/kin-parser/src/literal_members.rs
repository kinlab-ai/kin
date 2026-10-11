// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Adapter-declared named members of literal values.

use tree_sitter::Node;

/// A literal member whose name and complete source extent the adapter proved.
pub struct LiteralMember<'tree> {
    pub name: String,
    pub node: Node<'tree>,
    pub value: Node<'tree>,
}

/// Whether a node has its own source lines. Readers that locate entities by
/// line must not mistake a neighbouring statement or key for this member.
pub(crate) fn owns_lines(node: Node<'_>, source: &[u8]) -> bool {
    let before = &source[..node.start_byte()];
    let prefix = &before[before
        .iter()
        .rposition(|b| *b == b'\n')
        .map_or(0, |i| i + 1)..];
    if !prefix.iter().all(u8::is_ascii_whitespace) {
        return false;
    }
    let after = &source[node.end_byte()..];
    let suffix = &after[..after
        .iter()
        .position(|b| *b == b'\n')
        .unwrap_or(after.len())];
    let suffix = suffix.trim_ascii_start();
    let suffix = suffix
        .strip_prefix(b",")
        .unwrap_or(suffix)
        .trim_ascii_start();
    suffix.is_empty() || suffix.starts_with(b"#")
}
