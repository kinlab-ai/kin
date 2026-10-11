// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! What a modification asks of the entities that rely on the one modified.
//!
//! A consumer binds to an entity's declaration: its name, its kind, its
//! signature and whether it can see it. It runs the entity's body. A modification
//! that leaves both alone asks nothing of it, one that changes only the body
//! may change what it gets back, and one that changes the declaration can
//! leave it calling something that is no longer there.
//!
//! The recorded signature is declaration text, cut from the source with its
//! layout read past. A comment written inside the declaration, between two
//! parameters, is cut in with it, so two records of one declaration can carry
//! different signature text. Text alone cannot settle whether a declaration
//! changed. The graph's fingerprint can: the parser hashes every token of the
//! entity's own node and skips comments and whitespace, so equal hashes on
//! both sides say the entity reads the same token for token, and its
//! signature with it. Where an entity's language records a behaviour
//! equivalence class, equal classes say the body changed only in ways that
//! cannot change what it does, such as a docstring.
//!
//! Every reading here is the fingerprint's or a recorded field's. Nothing is
//! parsed again and no language is named, so a language's own idea of what a
//! comment or a docstring is stays with its adapter, which wrote the hashes.

use kin_model::{Entity, Hash256, Visibility};

/// How a modification left what an entity's consumers rely on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ContractChange {
    /// Every token of the entity is what it was: only comments, whitespace
    /// or layout moved, including a documentation comment.
    CommentsOrLayout,
    /// The declaration held and the body changed only in ways the graph's
    /// behaviour equivalence class records as unable to change what it does,
    /// such as a docstring.
    BehaviorEquivalent,
    /// The entity became visible to more of the code than before, and its
    /// code otherwise reads as it did. Nothing that reached it loses it.
    VisibilityWidened,
    /// The declaration held and where the checkout holds the entity moved,
    /// so a consumer that names it by where it lives may need to follow it.
    Moved,
    /// The declaration held and the body changed.
    Body,
    /// The entity stopped being public, so a consumer outside what can still
    /// see it no longer can.
    VisibilityReduced,
    /// The declared signature changed, or the name or kind it declares.
    Signature,
}

impl ContractChange {
    /// Classify the modification from `old` to `new`.
    pub fn of(old: &Entity, new: &Entity) -> Self {
        if old.name != new.name || old.kind != new.kind || signature_changed(old, new) {
            return Self::Signature;
        }
        if old.visibility == Visibility::Public && new.visibility != Visibility::Public {
            return Self::VisibilityReduced;
        }
        if place(old) != place(new) {
            return Self::Moved;
        }
        let body = if tokens_unchanged(old, new) {
            Self::CommentsOrLayout
        } else if behavior_equivalent(old, new) {
            Self::BehaviorEquivalent
        } else {
            Self::Body
        };
        if body.consumers_unaffected() && old.visibility != new.visibility {
            return Self::VisibilityWidened;
        }
        body
    }

    /// Whether a consumer that did not change with the entity still works
    /// as it did.
    pub fn consumers_unaffected(self) -> bool {
        matches!(
            self,
            Self::CommentsOrLayout | Self::BehaviorEquivalent | Self::VisibilityWidened
        )
    }

    /// Whether a consumer that did not change with the entity can be left
    /// calling something that is no longer there.
    pub fn breaks_consumers(self) -> bool {
        matches!(self, Self::Signature | Self::VisibilityReduced)
    }

    /// The machine-readable name of this reading.
    pub fn label(self) -> &'static str {
        match self {
            Self::CommentsOrLayout => "comments_or_layout_only",
            Self::BehaviorEquivalent => "behavior_equivalent",
            Self::VisibilityWidened => "visibility_widened",
            Self::Moved => "moved",
            Self::Body => "body_changed",
            Self::VisibilityReduced => "visibility_reduced",
            Self::Signature => "signature_changed",
        }
    }
}

/// Whether the graph's fingerprint says `old` and `new` read the same token
/// for token, comments and whitespace aside.
///
/// The zero hash is the sentinel a record carries when nothing measured its
/// tokens, and it never proves anything.
pub fn tokens_unchanged(old: &Entity, new: &Entity) -> bool {
    let (before, after) = (&old.fingerprint, &new.fingerprint);
    before.algorithm == after.algorithm
        && before.behavior_hash != Hash256::from_bytes([0; 32])
        && before.behavior_hash == after.behavior_hash
        && before.ast_hash == after.ast_hash
        && before.signature_hash == after.signature_hash
}

/// Whether the declared signature changed, rather than only the text it was
/// recorded as.
///
/// Signature text that differs while every token of the entity is the same
/// differs by a comment or layout inside the declaration, and a consumer
/// calls the same thing it did.
pub fn signature_changed(old: &Entity, new: &Entity) -> bool {
    old.signature != new.signature && !tokens_unchanged(old, new)
}

/// Whether the body change from `old` to `new` is behaviour-preserving under
/// the graph's equivalence class, or reads the same token for token.
///
/// The class is attached at ingest for the languages whose adapters define
/// one. True only when both records carry a computed class and the two are
/// equal; the zero hash on either side means the class is unknown, and an
/// unknown class never proves equivalence.
pub fn body_behavior_equivalent(old: &Entity, new: &Entity) -> bool {
    tokens_unchanged(old, new) || behavior_equivalent(old, new)
}

fn behavior_equivalent(old: &Entity, new: &Entity) -> bool {
    let class = old.fingerprint.equivalence_hash;
    class != Hash256::from_bytes([0; 32]) && class == new.fingerprint.equivalence_hash
}

/// Where the checkout holds the entity, as its record names it.
fn place(entity: &Entity) -> Option<&kin_model::FilePathId> {
    entity
        .span
        .as_ref()
        .map(|span| &span.file)
        .or(entity.file_origin.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use kin_model::entity::{
        EntityKind, EntityMetadata, EntityRole, FingerprintAlgorithm, SemanticFingerprint,
        SourceSpan,
    };
    use kin_model::ids::{EntityId, FilePathId, LanguageId};

    fn entity(signature: &str, behavior: u8) -> Entity {
        let file = FilePathId::new("src/encoding.py");
        Entity {
            id: EntityId::new(),
            kind: EntityKind::Function,
            name: "want_bytes".to_string(),
            language: LanguageId::Python,
            fingerprint: SemanticFingerprint {
                algorithm: FingerprintAlgorithm::V1TreeSitter,
                ast_hash: Hash256::from_bytes([3; 32]),
                signature_hash: Hash256::from_bytes([4; 32]),
                behavior_hash: Hash256::from_bytes([behavior; 32]),
                equivalence_hash: Hash256::from_bytes([0; 32]),
                stability_score: 0.8,
            },
            file_origin: Some(file.clone()),
            span: Some(SourceSpan {
                file,
                start_byte: 0,
                end_byte: 10,
                start_line: 1,
                start_col: 0,
                end_line: 3,
                end_col: 0,
            }),
            signature: signature.to_string(),
            visibility: Visibility::Public,
            role: EntityRole::Source,
            doc_summary: None,
            metadata: EntityMetadata::default(),
            lineage_parent: None,
            created_in: None,
            superseded_by: None,
        }
    }

    /// A comment inside the parameter list is cut into the signature text,
    /// and the tokens say the declaration is the same.
    #[test]
    fn a_comment_inside_the_declaration_is_not_a_signature_change() {
        let old = entity("def want_bytes(s, encoding)", 1);
        let new = entity("def want_bytes(# Bytes pass through. s, encoding)", 1);
        assert!(!signature_changed(&old, &new));
        assert_eq!(
            ContractChange::of(&old, &new),
            ContractChange::CommentsOrLayout
        );
        assert!(ContractChange::of(&old, &new).consumers_unaffected());
    }

    /// Falsify by trusting a zero hash: two records nothing measured would
    /// read as the same code, and a real signature change would vanish.
    #[test]
    fn an_unmeasured_fingerprint_proves_nothing() {
        let old = entity("def want_bytes(s)", 0);
        let new = entity("def want_bytes(s, encoding)", 0);
        assert!(signature_changed(&old, &new));
        assert_eq!(ContractChange::of(&old, &new), ContractChange::Signature);
    }

    #[test]
    fn each_reading_is_named_by_what_moved() {
        let old = entity("def want_bytes(s)", 1);

        let mut body = entity("def want_bytes(s)", 2);
        body.id = old.id;
        assert_eq!(ContractChange::of(&old, &body), ContractChange::Body);
        assert!(!ContractChange::of(&old, &body).consumers_unaffected());

        let mut documented = body.clone();
        documented.fingerprint.equivalence_hash = Hash256::from_bytes([5; 32]);
        let mut old_documented = old.clone();
        old_documented.fingerprint.equivalence_hash = Hash256::from_bytes([5; 32]);
        assert_eq!(
            ContractChange::of(&old_documented, &documented),
            ContractChange::BehaviorEquivalent
        );

        let widened = entity("def want_bytes(s, encoding)", 3);
        assert_eq!(
            ContractChange::of(&old, &widened),
            ContractChange::Signature
        );
        assert!(ContractChange::of(&old, &widened).breaks_consumers());

        let mut hidden = old.clone();
        hidden.visibility = Visibility::Private;
        assert_eq!(
            ContractChange::of(&old, &hidden),
            ContractChange::VisibilityReduced
        );
        assert_eq!(
            ContractChange::of(&hidden, &old),
            ContractChange::VisibilityWidened
        );

        let mut moved = old.clone();
        let elsewhere = FilePathId::new("src/bytes.py");
        moved.file_origin = Some(elsewhere.clone());
        moved.span.as_mut().unwrap().file = elsewhere;
        assert_eq!(ContractChange::of(&old, &moved), ContractChange::Moved);
    }
}
