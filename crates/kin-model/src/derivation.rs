// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Definition evidence is independent of an entity's project/file role.

use serde::{Deserialize, Serialize};

use crate::{Entity, SourceSpan};

pub const ENTITY_DERIVATION_KEY: &str = "kin_entity_derivation";
pub const MEMBER_COVERAGE_KEY: &str = "kin_computed_member_coverage";
pub const DERIVED_MEMBER_CANDIDATE_RULE: &str = "derived_member_candidate_v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DerivationSchema {
    #[serde(rename = "kin.entity.derivation.v1")]
    V1,
}

/// A candidate member's real generating syntax, never its independent body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntityDerivation {
    pub schema: DerivationSchema,
    pub generator: SourceSpan,
    pub assignment: SourceSpan,
    pub source_blob_hash: String,
    pub owner: String,
    pub member_key: String,
    pub rule: String,
    pub conditions: Vec<String>,
}

impl EntityDerivation {
    pub fn validate(&self) -> Result<(), String> {
        if self.source_blob_hash.len() != 64
            || !self
                .source_blob_hash
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err("derived member has no valid source-blob basis".into());
        }
        if self.generator.start_byte >= self.generator.end_byte
            || self.assignment.start_byte >= self.assignment.end_byte
            || self.assignment.file != self.generator.file
            || self.assignment.start_byte < self.generator.start_byte
            || self.assignment.end_byte > self.generator.end_byte
            || self.owner.is_empty()
            || self.member_key.is_empty()
            || self.conditions.is_empty()
        {
            return Err("derived member has invalid generator evidence".into());
        }
        Ok(())
    }
}

/// The only decoder for entity derivation authority. Errors remain untrusted.
pub fn entity_derivation(entity: &Entity) -> Result<Option<EntityDerivation>, String> {
    let Some(value) = entity.metadata.extra.get(ENTITY_DERIVATION_KEY) else {
        if entity.doc_summary.as_deref().is_some_and(|note| {
            note.starts_with("Derived from a loop over `") && note.contains("no literal `")
        }) {
            return Err("legacy derived member needs re-admission; its stored span is not an independent implementation".into());
        }
        return Ok(None);
    };
    let derivation: EntityDerivation = serde_json::from_value(value.clone())
        .map_err(|error| format!("unrecognized entity derivation: {error}"))?;
    derivation.validate()?;
    if entity.span.is_some() || entity.file_origin.as_ref() != Some(&derivation.generator.file) {
        return Err(
            "derived member must be spanless and associated with its generator file".into(),
        );
    }
    Ok(Some(derivation))
}

/// Validate the complete generator edge against the admitted artifact and bytes.
pub fn generator_relation_matches(
    entity: &Entity,
    relation: &crate::Relation,
    artifact: crate::ArtifactId,
    source_hash: &str,
) -> bool {
    let Ok(Some(derivation)) = entity_derivation(entity) else {
        return false;
    };
    relation.kind == crate::RelationKind::DerivedFrom
        && relation.src == crate::GraphNodeId::Entity(entity.id)
        && relation.dst == crate::GraphNodeId::Artifact(artifact)
        && derivation.source_blob_hash == source_hash
        && relation.evidence.iter().any(|e| {
            e.parser_rule.as_deref() == Some("derived_member_generator_v1")
                && e.source_span.as_ref() == Some(&derivation.generator)
                && e.token.as_deref() == Some(source_hash)
        })
}

/// Unknown/malformed/legacy derivations cannot regain declaration authority.
pub fn is_derived_member(entity: &Entity) -> bool {
    !matches!(entity_derivation(entity), Ok(None))
}

/// A compatibility/import node for a whole source file, not a declaration.
///
/// Import adapters retain these nodes to own module/import relationships. Their
/// exact signature includes the provenance path, unlike real `mod`, namespace,
/// or configuration module declarations. Use the persisted adapter contract so
/// older admitted graphs gain the same protection without rebuilding them.
pub fn is_file_module_surface(entity: &Entity) -> bool {
    if entity.kind == crate::EntityKind::File {
        return true;
    }
    if !matches!(
        entity.kind,
        crate::EntityKind::Module | crate::EntityKind::Package
    ) {
        return false;
    }
    let Some(origin) = entity.file_origin.as_ref() else {
        return false;
    };
    ["module ", "package ", "namespace "].iter().any(|prefix| {
        entity
            .signature
            .strip_prefix(prefix)
            .is_some_and(|tail| tail == origin.0)
    })
}

/// Why a whole file's module surface has no source of its own, after its
/// name: what it is, that Kin serves and changes entities rather than whole
/// files, and the calls that read the code in it instead. `None` for any
/// other entity. Every surface that refuses one words it this way, so a
/// reader meets one sentence it can act on.
pub fn whole_file_refusal_tail(entity: &Entity) -> Option<String> {
    if !is_file_module_surface(entity) {
        return None;
    }
    let what = match entity.kind {
        crate::EntityKind::Package => "package",
        crate::EntityKind::File => "file",
        _ => "module",
    };
    let at = entity
        .file_origin
        .as_ref()
        .map(|path| format!(", {}", path.0))
        .unwrap_or_default();
    if serves_module_code(entity) {
        return Some(format!(
            "is a whole {what}{at}, and Kin does not return or edit whole files. Its own code, \
             the imports and top-level statements outside every function and class, is read \
             with source on this {what} and changed with a patch against that read. To read a \
             function or class in it, find it with locate (by name, or exact text with mode \
             literal) and call source with that id; to see what calls something, call refs on \
             it."
        ));
    }
    Some(format!(
        "is a whole {what}{at}, and Kin does not return or edit whole files. To read code in it, \
         find a function or class with locate (by name, or exact text with mode literal) and \
         call source with that id; to see what calls something, call refs on it."
    ))
}

/// How a language's module-level code is written around the entities a module
/// holds: the comment that opens the one-line marker standing for an entity,
/// and the text that closes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModuleCodeSyntax {
    pub marker_open: &'static str,
    pub marker_close: &'static str,
}

/// The module-code syntax of `language`, or `None` for a language whose
/// modules are served as a member inventory only.
///
/// A language joins by naming, in its [`crate::LanguageProfile`], a comment
/// its grammar accepts wherever a top-level declaration may stand. Nothing
/// else about the language is read: the code a module owns is cut around the
/// spans its entities already record.
pub fn module_code_syntax(language: crate::LanguageId) -> Option<ModuleCodeSyntax> {
    language.profile().module_code
}

/// Whether `entity` is a whole file's module surface whose own code Kin
/// serves: its imports and the top-level statements outside every entity it
/// holds. Never the file: each held function or class stays behind a marker
/// and is read and changed by its own id.
pub fn serves_module_code(entity: &Entity) -> bool {
    matches!(
        entity.kind,
        crate::EntityKind::Module | crate::EntityKind::Package
    ) && is_file_module_surface(entity)
        && module_code_syntax(entity.language).is_some()
}

/// A kind that is one top-level statement binding a value. A small one stays
/// in its module's own code as the statement it is; every other kind holds a
/// body or members and is always a marker there.
pub fn is_module_value_kind(kind: crate::EntityKind) -> bool {
    matches!(
        kind,
        crate::EntityKind::Constant | crate::EntityKind::StaticVar
    )
}

pub fn require_independent_source(entity: &Entity) -> Result<(), String> {
    if let Some(tail) = whole_file_refusal_tail(entity) {
        return Err(format!("{} {tail}", entity.name));
    }
    match entity_derivation(entity) {
        Ok(None) => Ok(()),
        Ok(Some(_)) => Err(format!(
            "derived_member_body_unavailable: {} ({}) is a derived member candidate without an independently readable or editable body; inspect its shared generator with source using this entity id. Changes to the generator may affect sibling members",
            entity.name, entity.id
        )),
        Err(reason) => Err(format!("{} has no independent source authority: {reason}", entity.name)),
    }
}

/// Module containers are traversed through their members, never read or changed
/// as aggregate source. This policy belongs to semantic agent surfaces, while
/// import, reconciliation and materialization may still validate module spans.
pub fn is_module_container(entity: &Entity) -> bool {
    matches!(
        entity.kind,
        crate::EntityKind::Module | crate::EntityKind::Package | crate::EntityKind::File
    )
}

pub fn require_targeted_source(entity: &Entity) -> Result<(), String> {
    if serves_module_code(entity) {
        return Err(format!(
            "module_body_unavailable: {} ({}) is a whole module, never read or replaced as one \
             body. Read its own code, the imports and top-level statements, with source on it, \
             and change that code with a patch (EntitySourcePatch) carrying the source_base \
             that read returns; read and change a function or class in it by that entity's id",
            entity.name, entity.id
        ));
    }
    if is_module_container(entity) {
        return Err(format!(
            "module_body_unavailable: {} ({}) is a module container, not an independently \
             readable or editable body; inspect its recorded members and target the entity \
             or relationship you need. Missing member coverage is a semantic gap",
            entity.name, entity.id
        ));
    }
    require_independent_source(entity)
}

/// Whether a guarded write may name `entity`'s source: an entity with a body
/// of its own, or a module whose own code Kin serves. A module's base guards a
/// patch to its own code only, never a replacement of the whole module.
pub fn require_guarded_source(entity: &Entity) -> Result<(), String> {
    if serves_module_code(entity) {
        return Ok(());
    }
    require_targeted_source(entity)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_whole_file_is_refused_with_the_calls_that_read_its_code() {
        let mut module = crate::Entity {
            id: crate::EntityId::new(),
            kind: crate::EntityKind::Module,
            name: "ctx".into(),
            language: crate::LanguageId::Python,
            fingerprint: crate::SemanticFingerprint {
                algorithm: crate::FingerprintAlgorithm::V1TreeSitter,
                ast_hash: crate::Hash256::from_bytes([0; 32]),
                signature_hash: crate::Hash256::from_bytes([0; 32]),
                behavior_hash: crate::Hash256::from_bytes([0; 32]),
                equivalence_hash: crate::Hash256::from_bytes([0; 32]),
                stability_score: 1.0,
            },
            file_origin: Some(crate::FilePathId::new("src/flask/ctx.py")),
            span: None,
            signature: "module src/flask/ctx.py".into(),
            visibility: crate::Visibility::Public,
            role: crate::EntityRole::Source,
            doc_summary: None,
            metadata: crate::EntityMetadata::default(),
            lineage_parent: None,
            created_in: None,
            superseded_by: None,
        };
        let reason = require_independent_source(&module).unwrap_err();
        assert_eq!(
            reason,
            "ctx is a whole module, src/flask/ctx.py, and Kin does not return or edit whole \
             files. Its own code, the imports and top-level statements outside every function \
             and class, is read with source on this module and changed with a patch against \
             that read. To read a function or class in it, find it with locate (by name, or \
             exact text with mode literal) and call source with that id; to see what calls \
             something, call refs on it."
        );
        // A language whose module code is not served keeps the entity-only refusal.
        module.language = crate::LanguageId::Rust;
        module.signature = "module src/ctx.rs".into();
        module.file_origin = Some(crate::FilePathId::new("src/ctx.rs"));
        assert_eq!(
            require_independent_source(&module).unwrap_err(),
            "ctx is a whole module, src/ctx.rs, and Kin does not return or edit whole files. To \
             read code in it, find a function or class with locate (by name, or exact text with \
             mode literal) and call source with that id; to see what calls something, call refs \
             on it."
        );
        // A real module declaration is not a whole file's surface.
        module.signature = "mod ctx".into();
        assert!(whole_file_refusal_tail(&module).is_none());
        assert!(require_independent_source(&module).is_ok());
    }

    /// A module whose own code is served is guardable for a patch to that
    /// code, and still refuses every read or write of it as one body.
    #[test]
    fn a_served_module_is_guardable_and_never_a_whole_body() {
        let mut module = crate::Entity {
            id: crate::EntityId::new(),
            kind: crate::EntityKind::Module,
            name: "helpers".into(),
            language: crate::LanguageId::Python,
            fingerprint: crate::SemanticFingerprint {
                algorithm: crate::FingerprintAlgorithm::V1TreeSitter,
                ast_hash: crate::Hash256::from_bytes([0; 32]),
                signature_hash: crate::Hash256::from_bytes([0; 32]),
                behavior_hash: crate::Hash256::from_bytes([0; 32]),
                equivalence_hash: crate::Hash256::from_bytes([0; 32]),
                stability_score: 1.0,
            },
            file_origin: Some(crate::FilePathId::new("src/flask/helpers.py")),
            span: None,
            signature: "module src/flask/helpers.py".into(),
            visibility: crate::Visibility::Public,
            role: crate::EntityRole::Source,
            doc_summary: None,
            metadata: crate::EntityMetadata::default(),
            lineage_parent: None,
            created_in: None,
            superseded_by: None,
        };
        assert!(serves_module_code(&module));
        assert!(require_guarded_source(&module).is_ok());
        let whole = require_targeted_source(&module).unwrap_err();
        assert!(
            whole.starts_with("module_body_unavailable: helpers"),
            "{whole}"
        );
        assert!(whole.contains("change that code with a patch"), "{whole}");
        // A package surface is served the same way.
        module.signature = "package src/flask/__init__.py".into();
        module.file_origin = Some(crate::FilePathId::new("src/flask/__init__.py"));
        assert!(serves_module_code(&module));
        // A module declared inside a file, and a language with no module-code
        // syntax, stay inventory-only and unguardable.
        module.signature = "mod helpers".into();
        assert!(!serves_module_code(&module));
        assert!(require_guarded_source(&module)
            .unwrap_err()
            .contains("module container"));
        module.signature = "module src/helpers.go".into();
        module.file_origin = Some(crate::FilePathId::new("src/helpers.go"));
        module.language = crate::LanguageId::Go;
        assert!(!serves_module_code(&module));
        assert!(require_guarded_source(&module).is_err());
        assert!(is_module_value_kind(crate::EntityKind::Constant));
        assert!(!is_module_value_kind(crate::EntityKind::Function));
    }

    #[test]
    fn derived_member_schema_refuses_unknown_versions_and_invalid_spans() {
        let mut value = serde_json::json!({
            "schema":"kin.entity.derivation.v2", "generator":{}, "assignment":{},
            "source_blob_hash":"", "owner":"app", "member_key":"get", "rule":"literal", "conditions":[]
        });
        assert!(serde_json::from_value::<EntityDerivation>(value.clone()).is_err());
        value["schema"] = serde_json::json!("kin.entity.derivation.v1");
        assert!(serde_json::from_value::<EntityDerivation>(value).is_err());
    }
}
