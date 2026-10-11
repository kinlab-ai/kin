// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Which declarations a file's grammar shows, against the entities extraction
//! admitted from it.
//!
//! Name search answers from the entity index, so a declaration extraction did
//! not admit cannot be found by name, and nothing used to say so: an empty or
//! short answer could not separate a name the repository lacks from one the
//! extractor skipped. An adapter that takes a census lists every declaration
//! node its grammar shows, with the scope it sits in. The comparison here is
//! language agnostic: it pairs each admitted entity with the declaration it was
//! extracted from and lists every declaration left over, with its name, kind,
//! span and the reason it stayed out. A reader can then say an answer is
//! complete, or complete except the listed declarations that match it.
//!
//! What counts as a declaration is each adapter's contract, stated on its
//! walker: callables, classes and type declarations at any depth, the members
//! of classes and object literals, and bindings at module and class scope.
//! Local variables inside a callable are not declarations a name search is
//! asked to find, so no adapter counts them.

use serde::{Deserialize, Serialize};

use crate::extract::{ExtractedEntity, RelationSite};
use kin_model::EntityKind;

/// The kind of a declaration the grammar shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeclarationKind {
    Function,
    Method,
    Class,
    Interface,
    TypeAlias,
    Enum,
    Namespace,
    Variable,
    Attribute,
}

impl DeclarationKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Function => "function",
            Self::Method => "method",
            Self::Class => "class",
            Self::Interface => "interface",
            Self::TypeAlias => "type_alias",
            Self::Enum => "enum",
            Self::Namespace => "namespace",
            Self::Variable => "variable",
            Self::Attribute => "attribute",
        }
    }

    pub fn from_label(label: &str) -> Option<Self> {
        Some(match label {
            "function" => Self::Function,
            "method" => Self::Method,
            "class" => Self::Class,
            "interface" => Self::Interface,
            "type_alias" => Self::TypeAlias,
            "enum" => Self::Enum,
            "namespace" => Self::Namespace,
            "variable" => Self::Variable,
            "attribute" => Self::Attribute,
            _ => return None,
        })
    }

    /// Whether an entity of `kind` could have been admitted from a declaration
    /// of this kind. Used by readers that filter a residue by the kind a query
    /// asked for; conservative, so a doubtful pairing counts as a match.
    pub fn may_be_admitted_as(self, kind: EntityKind) -> bool {
        match self {
            Self::Function => matches!(
                kind,
                EntityKind::Function | EntityKind::Method | EntityKind::Test
            ),
            Self::Method => matches!(
                kind,
                EntityKind::Method | EntityKind::Function | EntityKind::Test
            ),
            Self::Class => matches!(
                kind,
                EntityKind::Class | EntityKind::EnumDef | EntityKind::Schema
            ),
            Self::Interface => matches!(kind, EntityKind::Interface | EntityKind::TraitDef),
            Self::TypeAlias => matches!(kind, EntityKind::TypeAlias | EntityKind::Schema),
            Self::Enum => matches!(kind, EntityKind::EnumDef | EntityKind::Class),
            Self::Namespace => matches!(kind, EntityKind::Module | EntityKind::Package),
            Self::Variable => matches!(
                kind,
                EntityKind::Constant | EntityKind::StaticVar | EntityKind::Function
            ),
            Self::Attribute => matches!(
                kind,
                EntityKind::Field | EntityKind::Constant | EntityKind::Method
            ),
        }
    }
}

/// Where a declaration sits, which decides the reason it stayed out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeclarationScope {
    /// At the top level of the file.
    Module,
    /// Inside a control block at the top level of the file, such as a guard
    /// or an exception handler, but not inside a callable or a class.
    ModuleBlock,
    /// Directly in a class body.
    Class,
    /// Directly in an object literal.
    Object,
}

/// One declaration node the grammar shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeenDeclaration {
    pub name: String,
    pub kind: DeclarationKind,
    pub scope: DeclarationScope,
    /// Whether any enclosing node is a callable body. A declaration inside a
    /// callable is local to it however it is nested.
    pub in_callable: bool,
    /// The nearest enclosing named declaration, for a reader locating it.
    pub owner: Option<String>,
    /// The declaration node.
    pub site: RelationSite,
}

/// A declaration the grammar shows that no admitted entity came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnadmittedDeclaration {
    pub name: String,
    pub kind: DeclarationKind,
    /// A stable label; see [`unadmitted_reason`].
    pub reason: String,
    pub owner: Option<String>,
    pub site: RelationSite,
}

/// One file's census.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DeclarationCensus {
    /// Declarations the grammar shows.
    pub seen: usize,
    /// Of those, the ones an admitted entity came from.
    pub admitted: usize,
    /// The rest, each listed.
    pub unadmitted: Vec<UnadmittedDeclaration>,
}

/// Local to a callable: a nested function, class, or bound callable.
pub const REASON_NESTED_IN_CALLABLE: &str = "nested_in_callable";
/// Inside a top-level control block the extractor does not walk.
pub const REASON_IN_MODULE_BLOCK: &str = "in_module_block";
/// A class attribute or field extraction does not admit as an entity.
pub const REASON_CLASS_MEMBER: &str = "class_member_not_admitted";
/// A member of an object literal the extractor does not treat as a namespace.
pub const REASON_OBJECT_MEMBER: &str = "object_literal_member";
/// A top-level binding the extractor's admission rule filtered out.
pub const REASON_MODULE_BINDING: &str = "module_binding_filtered";
/// Any other declaration extraction did not admit.
pub const REASON_NOT_ADMITTED: &str = "not_admitted";

/// Why a declaration in this position stayed out, as a stable label.
pub fn unadmitted_reason(declaration: &SeenDeclaration) -> &'static str {
    if declaration.in_callable {
        return REASON_NESTED_IN_CALLABLE;
    }
    match declaration.scope {
        DeclarationScope::ModuleBlock => REASON_IN_MODULE_BLOCK,
        DeclarationScope::Class => REASON_CLASS_MEMBER,
        DeclarationScope::Object => REASON_OBJECT_MEMBER,
        DeclarationScope::Module
            if matches!(
                declaration.kind,
                DeclarationKind::Variable | DeclarationKind::Attribute
            ) =>
        {
            REASON_MODULE_BINDING
        }
        DeclarationScope::Module => REASON_NOT_ADMITTED,
    }
}

/// The last segment of an admitted name, the part a declaration node spells.
fn leaf(name: &str) -> &str {
    name.rsplit(['.', ':', '#']).next().unwrap_or(name)
}

/// Entities that stand for a whole file or a synthesized surface rather than a
/// declaration node, so no census entry pairs with them.
fn pairs_with_a_declaration(kind: EntityKind) -> bool {
    !matches!(
        kind,
        EntityKind::Module | EntityKind::Package | EntityKind::File | EntityKind::DocumentNode
    )
}

/// Pair admitted entities with the declarations they came from and list the
/// rest.
///
/// An entity pairs with a declaration of the same leaf name whose node overlaps
/// the entity's span: a decorated definition's entity spans its decorators, and
/// a class bound to a name spans the class node inside the binding, so exact
/// span equality would miss both. Among overlapping candidates the one starting
/// nearest the entity's own start wins, which keeps an outer declaration from
/// being claimed by a same-named inner one. Each declaration pairs at most once.
pub fn take_census(
    mut seen: Vec<SeenDeclaration>,
    admitted: &[ExtractedEntity],
) -> DeclarationCensus {
    seen.sort_by_key(|declaration| (declaration.site.start_byte, declaration.site.end_byte));
    let mut paired = vec![false; seen.len()];
    let mut admitted_count = 0;
    for entity in admitted {
        if !pairs_with_a_declaration(entity.kind) {
            continue;
        }
        let name = leaf(&entity.name);
        let (start, end) = (entity.span.start_byte, entity.span.end_byte);
        let best = seen
            .iter()
            .enumerate()
            .filter(|(index, declaration)| {
                !paired[*index]
                    && declaration.name == name
                    && declaration.site.start_byte < end.max(start + 1)
                    && start
                        < declaration
                            .site
                            .end_byte
                            .max(declaration.site.start_byte + 1)
            })
            .min_by_key(|(_, declaration)| declaration.site.start_byte.abs_diff(start))
            .map(|(index, _)| index);
        if let Some(index) = best {
            paired[index] = true;
            admitted_count += 1;
        }
    }
    let total = seen.len();
    let unadmitted = seen
        .into_iter()
        .zip(paired)
        .filter(|(_, paired)| !paired)
        .map(|(declaration, _)| UnadmittedDeclaration {
            reason: unadmitted_reason(&declaration).to_string(),
            name: declaration.name,
            kind: declaration.kind,
            owner: declaration.owner,
            site: declaration.site,
        })
        .collect();
    DeclarationCensus {
        seen: total,
        admitted: admitted_count,
        unadmitted,
    }
}

/// The identifier text of `node`, when it is a plain name the census can list.
pub(crate) fn plain_name(node: &tree_sitter::Node, source: &[u8]) -> Option<String> {
    let text = node.utf8_text(source).ok()?.trim();
    let text = text.trim_matches(|ch| ch == '"' || ch == '\'');
    let valid = !text.is_empty()
        && text
            .chars()
            .all(|ch| ch == '_' || ch == '$' || ch == '#' || ch.is_alphanumeric());
    valid.then(|| text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use kin_model::{FilePathId, SemanticFingerprint, SourceSpan, Visibility};

    fn site(start: usize, end: usize) -> RelationSite {
        RelationSite {
            start_byte: start,
            end_byte: end,
            start_line: 0,
            start_col: 0,
            end_line: 0,
            end_col: 0,
            syntactic_role: None,
        }
    }

    fn seen(name: &str, start: usize, end: usize, in_callable: bool) -> SeenDeclaration {
        SeenDeclaration {
            name: name.into(),
            kind: DeclarationKind::Function,
            scope: DeclarationScope::Module,
            in_callable,
            owner: None,
            site: site(start, end),
        }
    }

    fn entity(name: &str, kind: EntityKind, start: usize, end: usize) -> ExtractedEntity {
        ExtractedEntity {
            kind,
            name: name.into(),
            signature: String::new(),
            visibility: Visibility::Public,
            doc_summary: None,
            fingerprint: SemanticFingerprint {
                algorithm: kin_model::FingerprintAlgorithm::V1TreeSitter,
                ast_hash: kin_model::Hash256::from_bytes([0; 32]),
                signature_hash: kin_model::Hash256::from_bytes([0; 32]),
                behavior_hash: kin_model::Hash256::from_bytes([0; 32]),
                equivalence_hash: kin_model::Hash256::from_bytes([0; 32]),
                stability_score: 1.0,
            },
            span: SourceSpan {
                file: FilePathId::new("a.py"),
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

    #[test]
    fn nested_declarations_are_listed_and_same_named_outer_ones_pair_first() {
        let census = take_census(
            vec![
                seen("outer", 0, 100, false),
                seen("outer", 20, 40, true),
                seen("helper", 50, 60, true),
            ],
            &[
                entity("outer", EntityKind::Function, 0, 100),
                entity("mod", EntityKind::Module, 0, 100),
            ],
        );
        assert_eq!(census.seen, 3);
        assert_eq!(census.admitted, 1);
        let names: Vec<_> = census
            .unadmitted
            .iter()
            .map(|d| (d.name.as_str(), d.site.start_byte, d.reason.as_str()))
            .collect();
        assert_eq!(
            names,
            vec![
                ("outer", 20, REASON_NESTED_IN_CALLABLE),
                ("helper", 50, REASON_NESTED_IN_CALLABLE)
            ]
        );
    }

    #[test]
    fn a_decorated_or_bound_entity_pairs_with_the_node_inside_its_span() {
        let census = take_census(
            vec![seen("view", 10, 50, false), seen("Store", 70, 90, false)],
            &[
                entity("view", EntityKind::Function, 0, 50),
                entity("Store", EntityKind::Class, 75, 90),
                entity("Store.get", EntityKind::Method, 80, 88),
            ],
        );
        assert_eq!(census.admitted, 2);
        assert!(census.unadmitted.is_empty());
    }
}
