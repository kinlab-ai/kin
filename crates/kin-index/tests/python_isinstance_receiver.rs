// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Positive isinstance guards bound the possible receiver owners at one call
//! site. Those possibilities are candidates, never two proven destinations.

use std::collections::HashSet;

use kin_index::resolution::RECEIVER_ORIGIN_NAME_CONFIDENCE;
use kin_index::{
    link_cross_file, link_cross_file_incremental, FileParseData, IncrementalLinker,
    RelationResolution,
};
use kin_model::{ArtifactId, Entity, EntityId, EntityKind, FilePathId, Relation, RelationKind};
use kin_parser::{LanguageAdapter, PythonAdapter};

const GUARDED_RULE: &str = "python_guarded_receiver_v1";
const MODELS: &str = r#"
class AliasPath:
    def convert_to_aliases(self):
        return ["path"]

class AliasChoices:
    def convert_to_aliases(self):
        return [["choice"]]

class Unrelated:
    def convert_to_aliases(self):
        return ["decoy"]
"#;

fn parse_py(file: &str, source: &str) -> FileParseData {
    let adapter = PythonAdapter;
    let file_id = FilePathId::new(file);
    let bytes = source.as_bytes();
    let tree = adapter.parse(bytes).expect("parse fixture");
    assert!(
        !tree.root_node().has_error(),
        "valid Python fixture: {file}"
    );
    let output = adapter
        .extract(&tree, bytes, &file_id)
        .expect("extract fixture");
    let mut entities: Vec<Entity> = output
        .entities
        .into_iter()
        .map(|entity| entity.into_entity_with_source(adapter.language_id(), &file_id, Some(bytes)))
        .collect();
    for entity in &mut entities {
        entity.metadata.extra.insert(
            "blob_hash".into(),
            serde_json::Value::String(kin_blobs::digest(bytes).to_string()),
        );
    }
    FileParseData {
        file_path: file.to_string(),
        entities,
        relations: output.relations,
        imports: output.imports,
    }
}

fn link_both(files: &[FileParseData]) -> Vec<(&'static str, Vec<Relation>)> {
    let artifacts = files
        .iter()
        .map(|file| (file.file_path.clone(), ArtifactId::new()))
        .collect();
    let batch = link_cross_file(files, &artifacts).expect("fixture artifact identities");
    let mut linker = IncrementalLinker::new();
    for file in files {
        linker.add_file(&file.file_path, ArtifactId::new(), &file.entities);
    }
    let incremental =
        link_cross_file_incremental(files, &linker).expect("fixture incremental index");
    vec![("batch", batch), ("incremental", incremental)]
}

fn entity_id(files: &[FileParseData], file: &str, name: &str, kind: EntityKind) -> EntityId {
    let matches: Vec<_> = files
        .iter()
        .flat_map(|parsed| &parsed.entities)
        .filter(|entity| {
            entity.name == name
                && entity.kind == kind
                && entity.file_origin.as_ref().map(|origin| origin.0.as_str()) == Some(file)
        })
        .collect();
    assert_eq!(matches.len(), 1, "one {kind:?} {file}:{name}");
    matches[0].id
}

fn call_ranges(source: &str, call: &str) -> Vec<(usize, usize)> {
    source
        .match_indices(call)
        .map(|(start, _)| (start, start + call.len()))
        .collect()
}

fn calls_at(relations: &[Relation], caller: EntityId, range: (usize, usize)) -> Vec<&Relation> {
    relations
        .iter()
        .filter(|relation| {
            relation.kind == RelationKind::Calls
                && relation.src.as_entity() == Some(caller)
                && relation.evidence.iter().any(|evidence| {
                    evidence
                        .source_span
                        .as_ref()
                        .is_some_and(|span| (span.start_byte, span.end_byte) == range)
                })
        })
        .collect()
}

fn assert_guard_candidates(
    relations: &[Relation],
    caller: EntityId,
    file: &str,
    source: &str,
    range: (usize, usize),
    expected: &[EntityId],
    path: &str,
) {
    let calls = calls_at(relations, caller, range);
    let actual: HashSet<_> = calls
        .iter()
        .filter_map(|relation| relation.dst.as_entity())
        .collect();
    let expected: HashSet<_> = expected.iter().copied().collect();
    assert_eq!(
        actual, expected,
        "{path}: only the guarded receiver owners may supply candidates"
    );
    assert_eq!(
        calls.len(),
        expected.len(),
        "{path}: no extra external or duplicate call"
    );
    for relation in calls {
        assert_eq!(
            relation.confidence.to_bits(),
            RECEIVER_ORIGIN_NAME_CONFIDENCE.to_bits(),
            "{path}"
        );
        let resolution = RelationResolution::of(relation);
        assert_eq!(resolution, RelationResolution::NameOnly, "{path}");
        assert!(
            !resolution.is_proven(),
            "{path}: a guard is not a proven runtime destination"
        );
        assert!(
            kin_index::occurrence::static_proof_sites(relation).is_empty(),
            "{path}: no static proof for a candidate"
        );
        let (proven, withheld) = kin_index::occurrence::proven_sites(relation);
        assert!(
            proven.is_empty(),
            "{path}: candidate sites must stay out of proven references"
        );
        assert!(
            withheld,
            "{path}: the consumer must disclose the unproven occurrence"
        );
        let evidence: Vec<_> = relation
            .evidence
            .iter()
            .filter(|evidence| {
                evidence
                    .source_span
                    .as_ref()
                    .is_some_and(|span| (span.start_byte, span.end_byte) == range)
            })
            .collect();
        assert!(
            !evidence.is_empty(),
            "{path}: retain this call's exact site"
        );
        assert!(
            evidence
                .iter()
                .all(|evidence| evidence.parser_rule.as_deref() == Some(GUARDED_RULE)),
            "{path}"
        );
        for evidence in evidence {
            let span = evidence.source_span.as_ref().expect("call source span");
            assert_eq!(span.file.0, file, "{path}");
            assert_eq!(
                span.start_line as usize,
                source[..range.0]
                    .bytes()
                    .filter(|byte| *byte == b'\n')
                    .count(),
                "{path}"
            );
            assert_eq!(
                &source[span.start_byte..span.end_byte],
                &source[range.0..range.1],
                "{path}"
            );
        }
    }
}

fn assert_unguarded_unproven(
    relations: &[Relation],
    caller: EntityId,
    range: (usize, usize),
    case: &str,
) {
    for relation in calls_at(relations, caller, range) {
        assert!(
            !RelationResolution::of(relation).is_proven(),
            "{case}: no invented receiver proof"
        );
        assert!(
            relation
                .evidence
                .iter()
                .filter(|evidence| {
                    evidence
                        .source_span
                        .as_ref()
                        .is_some_and(|span| (span.start_byte, span.end_byte) == range)
                })
                .all(|evidence| evidence.parser_rule.as_deref() != Some(GUARDED_RULE)),
            "{case}: narrowing must not escape its positive guard"
        );
    }
}

#[test]
fn a_same_file_alias_choice_loop_keeps_only_the_guarded_path_candidate() {
    let source = r#"
class AliasPath:
    def convert_to_aliases(self):
        return ["path"]

class AliasChoices:
    choices: list[str | AliasPath]
    def convert_to_aliases(self):
        aliases = []
        for c in self.choices:
            if isinstance(c, AliasPath):
                aliases.append(c.convert_to_aliases())
            else:
                aliases.append([c])
        return aliases

class Unrelated:
    def convert_to_aliases(self):
        return ["decoy"]
"#;
    let files = vec![parse_py("aliases.py", source)];
    let caller = entity_id(
        &files,
        "aliases.py",
        "AliasChoices.convert_to_aliases",
        EntityKind::Method,
    );
    let target = entity_id(
        &files,
        "aliases.py",
        "AliasPath.convert_to_aliases",
        EntityKind::Method,
    );
    let range = call_ranges(source, "c.convert_to_aliases()")[0];
    for (path, relations) in link_both(&files) {
        assert_guard_candidates(
            &relations,
            caller,
            "aliases.py",
            source,
            range,
            &[target],
            path,
        );
    }
}

#[test]
fn an_imported_tuple_guard_keeps_both_alias_candidates_and_excludes_decoys() {
    let source = r#"
from models import AliasChoices, AliasPath

class LocalDecoy:
    def convert_to_aliases(self):
        return ["local decoy"]

def convert(alias: str | AliasChoices | AliasPath | None):
    if isinstance(alias, (AliasChoices, AliasPath)):
        return alias.convert_to_aliases()
    return alias
"#;
    let files = vec![
        parse_py("models.py", MODELS),
        parse_py("consumer.py", source),
    ];
    let caller = entity_id(&files, "consumer.py", "convert", EntityKind::Function);
    let expected = [
        entity_id(
            &files,
            "models.py",
            "AliasChoices.convert_to_aliases",
            EntityKind::Method,
        ),
        entity_id(
            &files,
            "models.py",
            "AliasPath.convert_to_aliases",
            EntityKind::Method,
        ),
    ];
    let range = call_ranges(source, "alias.convert_to_aliases()")[0];
    for (path, relations) in link_both(&files) {
        assert_guard_candidates(
            &relations,
            caller,
            "consumer.py",
            source,
            range,
            &expected,
            path,
        );
    }
}

#[test]
fn an_import_alias_guard_finds_the_method_declared_by_the_base_class() {
    let models = r#"
class BaseAliasPath:
    def convert_to_aliases(self):
        return ["base path"]

class AliasPath(BaseAliasPath):
    pass

class Unrelated:
    def convert_to_aliases(self):
        return ["decoy"]
"#;
    let source = r#"
from models import AliasPath as PathAlias

def convert(value):
    if isinstance(value, PathAlias):
        return value.convert_to_aliases()
"#;
    let files = vec![
        parse_py("models.py", models),
        parse_py("consumer.py", source),
    ];
    let caller = entity_id(&files, "consumer.py", "convert", EntityKind::Function);
    let target = entity_id(
        &files,
        "models.py",
        "BaseAliasPath.convert_to_aliases",
        EntityKind::Method,
    );
    let range = call_ranges(source, "value.convert_to_aliases()")[0];
    for (path, relations) in link_both(&files) {
        assert_guard_candidates(
            &relations,
            caller,
            "consumer.py",
            source,
            range,
            &[target],
            path,
        );
    }
}

#[test]
fn the_else_arm_and_the_call_after_the_guard_keep_their_original_uncertainty() {
    let source = r#"
from models import AliasPath

def convert(value):
    if isinstance(value, AliasPath):
        value.convert_to_aliases()
    else:
        value.convert_to_aliases()
    return value.convert_to_aliases()
"#;
    let files = vec![
        parse_py("models.py", MODELS),
        parse_py("consumer.py", source),
    ];
    let caller = entity_id(&files, "consumer.py", "convert", EntityKind::Function);
    let target = entity_id(
        &files,
        "models.py",
        "AliasPath.convert_to_aliases",
        EntityKind::Method,
    );
    let ranges = call_ranges(source, "value.convert_to_aliases()");
    assert_eq!(ranges.len(), 3);
    for (path, relations) in link_both(&files) {
        assert_guard_candidates(
            &relations,
            caller,
            "consumer.py",
            source,
            ranges[0],
            &[target],
            path,
        );
        for range in &ranges[1..] {
            assert_unguarded_unproven(&relations, caller, *range, path);
        }
    }
}

#[test]
fn shadowed_or_rebound_isinstance_and_negated_guards_never_supply_guard_candidates() {
    let cases = [
        ("parameter", "from models import AliasPath\ndef convert(value, isinstance):\n    if isinstance(value, AliasPath):\n        return value.convert_to_aliases()\n"),
        ("module definition", "from models import AliasPath\ndef isinstance(*args):\n    return True\ndef convert(value):\n    if isinstance(value, AliasPath):\n        return value.convert_to_aliases()\n"),
        ("module rebound", "from models import AliasPath\nisinstance = predicate\ndef convert(value):\n    if isinstance(value, AliasPath):\n        return value.convert_to_aliases()\n"),
        ("local import", "from models import AliasPath\ndef convert(value):\n    from predicates import isinstance\n    if isinstance(value, AliasPath):\n        return value.convert_to_aliases()\n"),
        ("local rebound", "from models import AliasPath\ndef convert(value):\n    isinstance = predicate\n    if isinstance(value, AliasPath):\n        return value.convert_to_aliases()\n"),
        ("wildcard import", "from models import *\ndef convert(value):\n    if isinstance(value, AliasPath):\n        return value.convert_to_aliases()\n"),
        ("negated guard", "from models import AliasPath\ndef convert(value):\n    if not isinstance(value, AliasPath):\n        return value.convert_to_aliases()\n"),
    ];
    for (case, source) in cases {
        let files = vec![
            parse_py("models.py", MODELS),
            parse_py("consumer.py", source),
        ];
        let caller = entity_id(&files, "consumer.py", "convert", EntityKind::Function);
        let range = call_ranges(source, "value.convert_to_aliases()")[0];
        for (path, relations) in link_both(&files) {
            assert_unguarded_unproven(&relations, caller, range, &format!("{path}: {case}"));
        }
    }
}

#[test]
fn receiver_reassignment_deletion_and_rebinding_loops_end_the_guard() {
    let cases = [
        (
            "assignment",
            "value = replacement\n        return value.convert_to_aliases()",
        ),
        (
            "walrus",
            "if (value := replacement):\n            return value.convert_to_aliases()",
        ),
        (
            "loop binding",
            "for value in replacements:\n            value.convert_to_aliases()",
        ),
        (
            "deleted binding",
            "del value\n        return value.convert_to_aliases()",
        ),
    ];
    for (case, statements) in cases {
        let source = format!("from models import AliasPath\ndef convert(value):\n    if isinstance(value, AliasPath):\n        {statements}\n");
        let files = vec![
            parse_py("models.py", MODELS),
            parse_py("consumer.py", &source),
        ];
        let caller = entity_id(&files, "consumer.py", "convert", EntityKind::Function);
        let range = call_ranges(&source, "value.convert_to_aliases()")[0];
        for (path, relations) in link_both(&files) {
            assert_unguarded_unproven(&relations, caller, range, &format!("{path}: {case}"));
        }
    }
}

#[test]
fn shadowed_or_rebound_guard_classes_cannot_reach_the_imported_class() {
    let cases = [
        ("class parameter", "from models import AliasPath\ndef convert(value, AliasPath):\n    if isinstance(value, AliasPath):\n        return value.convert_to_aliases()\n"),
        ("class rebound locally", "from models import AliasPath\ndef convert(value):\n    AliasPath = replacement\n    if isinstance(value, AliasPath):\n        return value.convert_to_aliases()\n"),
        ("class rebound in module", "from models import AliasPath\nAliasPath = replacement\ndef convert(value):\n    if isinstance(value, AliasPath):\n        return value.convert_to_aliases()\n"),
    ];
    for (case, source) in cases {
        let files = vec![
            parse_py("models.py", MODELS),
            parse_py("consumer.py", source),
        ];
        let caller = entity_id(&files, "consumer.py", "convert", EntityKind::Function);
        let range = call_ranges(source, "value.convert_to_aliases()")[0];
        for (path, relations) in link_both(&files) {
            assert!(
                calls_at(&relations, caller, range).is_empty(),
                "{path}: {case} must not fall back to same-name methods"
            );
        }
    }
}

#[test]
fn an_unknown_or_mixed_guard_owner_set_is_a_gap_instead_of_broad_method_fanout() {
    let guards = [
        "Missing",
        "(AliasPath, Missing)",
        "(AliasPath, factory())",
        "(AliasPath, ['invalid'])",
    ];
    for guard in guards {
        let source = format!("from models import AliasPath\ndef convert(value):\n    if isinstance(value, {guard}):\n        return value.convert_to_aliases()\n");
        let files = vec![
            parse_py("models.py", MODELS),
            parse_py("consumer.py", &source),
        ];
        let caller = entity_id(&files, "consumer.py", "convert", EntityKind::Function);
        let range = call_ranges(&source, "value.convert_to_aliases()")[0];
        for (path, relations) in link_both(&files) {
            assert!(
                calls_at(&relations, caller, range).is_empty(),
                "{path}: inadmissible guard {guard} must not regain broad method fanout"
            );
        }
    }
}

#[test]
fn an_unguarded_union_does_not_become_a_guarded_or_proven_destination() {
    let source = r#"
from models import AliasChoices, AliasPath

def convert(alias: str | AliasChoices | AliasPath | None):
    return alias.convert_to_aliases()
"#;
    let files = vec![
        parse_py("models.py", MODELS),
        parse_py("consumer.py", source),
    ];
    let caller = entity_id(&files, "consumer.py", "convert", EntityKind::Function);
    let range = call_ranges(source, "alias.convert_to_aliases()")[0];
    for (path, relations) in link_both(&files) {
        assert_unguarded_unproven(&relations, caller, range, path);
    }
}

#[test]
fn a_proven_direct_call_does_not_promote_the_guarded_occurrence_on_the_same_edge() {
    let source = r#"
from models import AliasPath

def convert(value):
    AliasPath.convert_to_aliases(value)
    if isinstance(value, AliasPath):
        return value.convert_to_aliases()
"#;
    let files = vec![
        parse_py("models.py", MODELS),
        parse_py("consumer.py", source),
    ];
    let caller = entity_id(&files, "consumer.py", "convert", EntityKind::Function);
    let target = entity_id(
        &files,
        "models.py",
        "AliasPath.convert_to_aliases",
        EntityKind::Method,
    );
    let direct = call_ranges(source, "AliasPath.convert_to_aliases(value)")[0];
    let guarded = call_ranges(source, "value.convert_to_aliases()")[0];
    for (path, relations) in link_both(&files) {
        let edges: Vec<_> = relations
            .iter()
            .filter(|relation| {
                relation.kind == RelationKind::Calls
                    && relation.src.as_entity() == Some(caller)
                    && relation.dst.as_entity() == Some(target)
            })
            .collect();
        assert_eq!(
            edges.len(),
            1,
            "{path}: aggregate occurrences on one logical edge"
        );
        let edge = edges[0];
        assert!(
            edge.evidence.iter().any(|evidence| {
                evidence.parser_rule.as_deref() == Some(GUARDED_RULE)
                    && evidence
                        .source_span
                        .as_ref()
                        .is_some_and(|span| (span.start_byte, span.end_byte) == guarded)
            }),
            "{path}: retain guarded occurrence evidence after merging"
        );
        let static_sites = kin_index::occurrence::static_proof_sites(edge);
        assert!(
            static_sites
                .iter()
                .any(|span| (span.start_byte, span.end_byte) == direct),
            "{path}: retain the direct call's proof"
        );
        assert!(
            !static_sites
                .iter()
                .any(|span| (span.start_byte, span.end_byte) == guarded),
            "{path}: aggregate confidence is not a proof of the guarded site"
        );
        let (proven, withheld) = kin_index::occurrence::proven_sites(edge);
        assert!(
            proven
                .iter()
                .any(|span| (span.start_byte, span.end_byte) == direct),
            "{path}"
        );
        assert!(
            !proven
                .iter()
                .any(|span| (span.start_byte, span.end_byte) == guarded),
            "{path}"
        );
        assert!(
            withheld,
            "{path}: the guard occurrence remains withheld from proven reference sites"
        );
    }
}

#[test]
fn a_guarded_edge_names_the_guard_that_admitted_its_class() {
    let source = r#"
from models import AliasPath

def convert(value):
    if isinstance(value, AliasPath):
        return value.convert_to_aliases()
"#;
    let files = vec![
        parse_py("models.py", MODELS),
        parse_py("consumer.py", source),
    ];
    let caller = entity_id(&files, "consumer.py", "convert", EntityKind::Function);
    let range = call_ranges(source, "value.convert_to_aliases()")[0];
    let guard_start = source.find("isinstance(").unwrap();
    for (path, relations) in link_both(&files) {
        let calls = calls_at(&relations, caller, range);
        assert_eq!(calls.len(), 1, "{path}");
        let record = calls[0]
            .evidence
            .iter()
            .find(|record| record.parser_rule.as_deref() == Some(GUARDED_RULE))
            .expect("guarded occurrence");
        let (guard, span) = kin_index::linker::guard_of_occurrence(record).expect("guard token");
        assert_eq!(guard, "isinstance(value, AliasPath)", "{path}");
        assert_eq!(span.start_byte, guard_start, "{path}");
        assert_eq!(span.file.0, "consumer.py", "{path}");
    }
}

/// The two sites the Pydantic audit found, in their own package layout:
/// dataclass-decorated alias classes, the loop guard inside
/// `AliasChoices.convert_to_aliases`, and a helper in a subpackage that
/// imports both classes relatively and guards with a tuple.
#[test]
fn the_pydantic_alias_sites_reach_their_guarded_candidates_across_a_package() {
    let aliases = r#"
from __future__ import annotations

import dataclasses


@dataclasses.dataclass(slots=True)
class AliasPath:
    path: list[int | str]

    def convert_to_aliases(self) -> list[str | int]:
        return self.path


@dataclasses.dataclass(slots=True)
class AliasChoices:
    choices: list[str | AliasPath]

    def convert_to_aliases(self) -> list[list[str | int]]:
        aliases: list[list[str | int]] = []
        for c in self.choices:
            if isinstance(c, AliasPath):
                aliases.append(c.convert_to_aliases())
            else:
                aliases.append([c])
        return aliases
"#;
    let schema = r#"
from ..aliases import AliasChoices, AliasPath


def _convert_to_aliases(
    alias: str | AliasChoices | AliasPath | None,
) -> str | list[str | int] | list[list[str | int]] | None:
    if isinstance(alias, (AliasChoices, AliasPath)):
        return alias.convert_to_aliases()
    else:
        return alias
"#;
    let files = vec![
        parse_py("pydantic/__init__.py", ""),
        parse_py("pydantic/aliases.py", aliases),
        parse_py("pydantic/_internal/__init__.py", ""),
        parse_py("pydantic/_internal/_generate_schema.py", schema),
    ];
    let path_method = entity_id(
        &files,
        "pydantic/aliases.py",
        "AliasPath.convert_to_aliases",
        EntityKind::Method,
    );
    let choices_method = entity_id(
        &files,
        "pydantic/aliases.py",
        "AliasChoices.convert_to_aliases",
        EntityKind::Method,
    );
    let helper = entity_id(
        &files,
        "pydantic/_internal/_generate_schema.py",
        "_convert_to_aliases",
        EntityKind::Function,
    );
    let loop_site = call_ranges(aliases, "c.convert_to_aliases()")[0];
    let helper_site = call_ranges(schema, "alias.convert_to_aliases()")[0];
    for (path, relations) in link_both(&files) {
        assert_guard_candidates(
            &relations,
            choices_method,
            "pydantic/aliases.py",
            aliases,
            loop_site,
            &[path_method],
            path,
        );
        assert_guard_candidates(
            &relations,
            helper,
            "pydantic/_internal/_generate_schema.py",
            schema,
            helper_site,
            &[path_method, choices_method],
            path,
        );
    }
}
