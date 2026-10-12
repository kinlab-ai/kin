// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Test callbacks retain their sites without guessing custom-loader authority.

use std::collections::{BTreeSet, HashMap};

use kin_index::{FileParseData, IncrementalLinker, IndexPipeline};
use kin_model::{ArtifactId, EntityId, EntityKind, FilePathId, Relation, RelationKind};

fn parse(path: &str, source: &str) -> FileParseData {
    let indexed = IndexPipeline::new()
        .index_file_content_with_tests(
            &FilePathId::new(path),
            source.as_bytes(),
            kin_blobs::digest(source.as_bytes()),
        )
        .expect("index fixture")
        .indexed_file;
    FileParseData {
        file_path: path.into(),
        entities: indexed.entities,
        relations: indexed.extracted_relations,
        imports: indexed.imports,
    }
}

fn id(file: &FileParseData, name: &str, kind: EntityKind) -> EntityId {
    file.entities
        .iter()
        .find(|entity| entity.name == name && entity.kind == kind)
        .unwrap_or_else(|| panic!("missing {kind:?} {name} in {}", file.file_path))
        .id
}

fn routes(files: &[FileParseData]) -> Vec<(&'static str, Vec<Relation>)> {
    let artifacts: HashMap<String, ArtifactId> = files
        .iter()
        .map(|file| (file.file_path.clone(), ArtifactId::new()))
        .collect();
    let mut linker = IncrementalLinker::new();
    for file in files {
        linker.add_file(&file.file_path, artifacts[&file.file_path], &file.entities);
    }
    let restored = IncrementalLinker::from_checkpoint_v1(linker.to_checkpoint_v1())
        .expect("restore identities");
    vec![
        (
            "batch",
            kin_index::link_cross_file(files, &artifacts).unwrap(),
        ),
        (
            "incremental",
            kin_index::link_cross_file_incremental(files, &linker).unwrap(),
        ),
        (
            "checkpoint",
            kin_index::link_cross_file_incremental(files, &restored).unwrap(),
        ),
    ]
}

fn source_lines(source: &str, needle: &str) -> BTreeSet<u32> {
    source
        .lines()
        .enumerate()
        .filter_map(|(line, text)| text.contains(needle).then_some(line as u32))
        .collect()
}

fn call_lines(relations: &[Relation], path: &str, target: EntityId) -> BTreeSet<u32> {
    relations
        .iter()
        .filter(|relation| {
            relation.kind == RelationKind::Calls && relation.dst.as_entity() == Some(target)
        })
        .flat_map(|relation| &relation.evidence)
        .filter_map(|evidence| evidence.source_span.as_ref())
        .filter(|span| span.file.0 == path)
        .map(|span| span.start_line)
        .collect()
}

const REDACTOR: &str = "export function redactSensitiveData(value: unknown) { return value; }\n";
const REDACTION_TESTS: &str = r#"import { describe, it, expect } from 'vitest';
import { redactSensitiveData } from './redactSensitiveData';
describe('redactor', () => {
  it('primitives', () => {
    expect(redactSensitiveData(null));
    expect(redactSensitiveData(undefined));
    expect(redactSensitiveData('text'));
    expect(redactSensitiveData(123));
    expect(redactSensitiveData(true));
  });
  it('flat', () => {
    expect(redactSensitiveData({ value: 1 }));
  });
  it('nested', () => {
    expect(redactSensitiveData({ nested: {} }));
  });
  it('arrays', () => {
    expect(redactSensitiveData([{}]));
  });
  it('fields', () => {
    expect(redactSensitiveData({ field: '' }));
  });
  it('circular', () => {
    expect(redactSensitiveData({}));
  });
  it('json', () => {
    expect(redactSensitiveData('{'));
  });
  it('credentials', () => {
    expect(redactSensitiveData({ credential: {} }));
  });
  it('nested credentials', () => {
    expect(redactSensitiveData({ nested: { credential: {} } }));
  });
});
const unrelated = {};
unrelated.redactSensitiveData('untyped object');
"#;

const WRAPPERS: &str = r#"import { redactSensitiveData } from './redactSensitiveData';
function createErrorHandler(args: unknown[]) {
  return (error: unknown) => ({ error, args: redactSensitiveData(args) });
}
function createConsoleSpan(attributes: unknown) {
  const logCompletion = () => redactSensitiveData(attributes);
  return { logCompletion };
}
export function withReporting(args: unknown[]) {
  return createErrorHandler(args);
}
export function withSpan(attributes: unknown) {
  return createConsoleSpan(attributes);
}
"#;

#[test]
fn thirteen_vitest_callback_sites_and_production_wrappers_keep_import_authority() {
    let files = [
        parse("packages/lib/redactSensitiveData.ts", REDACTOR),
        parse("packages/lib/redactSensitiveData.test.ts", REDACTION_TESTS),
        parse("packages/lib/sentryWrapper.ts", WRAPPERS),
        parse("unrelated/redactor.ts", REDACTOR),
    ];
    let target = id(&files[0], "redactSensitiveData", EntityKind::Function);
    let decoy = id(&files[3], "redactSensitiveData", EntityKind::Function);
    let module = id(&files[1], "redactSensitiveData.test", EntityKind::Module);
    let test_sites = source_lines(REDACTION_TESTS, "expect(redactSensitiveData(");
    assert_eq!(test_sites.len(), 13);
    let raw_sites: BTreeSet<_> = files[1]
        .relations
        .iter()
        .filter(|relation| {
            relation.kind == RelationKind::Calls
                && relation.dst_name == "redactSensitiveData"
                && relation.receiver.is_none()
        })
        .map(|relation| {
            assert_eq!(relation.src_name, "redactSensitiveData.test");
            relation
                .site
                .as_ref()
                .expect("callback call site")
                .start_line
        })
        .collect();
    assert_eq!(raw_sites, test_sites);
    for (route, relations) in routes(&files) {
        let receiver_candidates: Vec<_> = relations
            .iter()
            .filter(|relation| relation.kind == RelationKind::Calls)
            .filter(|relation| {
                relation.evidence.iter().any(|evidence| {
                    evidence.source_span.as_ref().is_some_and(|span| {
                        span.file.0 == files[1].file_path && span.start_line == 36
                    })
                })
            })
            .collect();
        assert_eq!(
            call_lines(&relations, &files[1].file_path, target),
            test_sites,
            "{route}: receiver candidates {receiver_candidates:#?}"
        );
        let test_edges: Vec<_> = relations
            .iter()
            .filter(|relation| {
                relation.kind == RelationKind::Calls
                    && relation.src.as_entity() == Some(module)
                    && relation.dst.as_entity() == Some(target)
            })
            .collect();
        assert_eq!(
            test_edges.len(),
            1,
            "distinct sites share one logical edge: {route}"
        );
        assert_eq!(
            call_lines(&relations, &files[2].file_path, target),
            source_lines(WRAPPERS, "=> redactSensitiveData(")
                .union(&source_lines(WRAPPERS, "args: redactSensitiveData("))
                .copied()
                .collect(),
            "{route}: both production sites"
        );
        for (caller, callee) in [
            ("withReporting", "createErrorHandler"),
            ("withSpan", "createConsoleSpan"),
        ] {
            let caller = id(&files[2], caller, EntityKind::Function);
            let callee = id(&files[2], callee, EntityKind::Function);
            assert_eq!(
                relations
                    .iter()
                    .filter(|relation| {
                        relation.kind == RelationKind::Calls
                            && relation.src.as_entity() == Some(caller)
                            && relation.dst.as_entity() == Some(callee)
                    })
                    .count(),
                1,
                "{route}: wrapper call is not swept twice"
            );
        }
        assert!(
            relations
                .iter()
                .all(|relation| relation.kind != RelationKind::Calls
                    || relation.dst.as_entity() != Some(decoy)),
            "{route}: no same-name capture"
        );
    }
}

const TIME_TESTS: &str = r#"const { run_test } = require('./lib/test.cjs');
const util = require('../src/util');
run_test('time input', () => {
  util.check_time_input('24abc');
  util.check_time_input('24.5');
  util.check_time_input('24.5', true);
  util.check_time_input('0.1', true);
  util.check_time_input('0.5', true);
  util.check_time_input('24');
});
"#;

#[test]
fn commonjs_callback_sites_and_nested_test_arrow_bind_with_a_real_import() {
    let files = [
        parse("web/src/util.ts", "export function check_time_input(value: string, floating = false) { return value; }\n"),
        parse("web/tests/util.test.cjs", TIME_TESTS),
        parse("web/src/settings_components.ts", "export function parse_time_limit(value: string) { return value; }\n"),
        parse("web/tests/settings_org.test.cjs", "const settings_components = require('../src/settings_components');\nfunction test_parse_time_limit() {\n  const test_function = (value) => settings_components.parse_time_limit(value);\n  return test_function('24.5');\n}\n"),
    ];
    let target = id(&files[0], "check_time_input", EntityKind::Function);
    let parsed = id(&files[2], "parse_time_limit", EntityKind::Function);
    let expected = source_lines(TIME_TESTS, "util.check_time_input(");
    assert_eq!(expected.len(), 6);
    for (route, relations) in routes(&files) {
        assert_eq!(
            call_lines(&relations, &files[1].file_path, target),
            expected,
            "{route}"
        );
        assert_eq!(
            call_lines(&relations, &files[3].file_path, parsed),
            BTreeSet::from([2]),
            "{route}"
        );
    }
}

#[test]
fn custom_loader_keeps_callback_sites_without_guessing_a_module_target() {
    for argument in ["'util'", "module_name"] {
        let source = TIME_TESTS.replace("const util = require('../src/util');",
            &format!("const {{ zrequire }} = require('./lib/namespace.cjs');\nconst util = zrequire({argument});"));
        let files = [
            parse(
                "web/src/util.ts",
                "export function check_time_input(value: string) { return value; }\n",
            ),
            parse("web/tests/util.test.cjs", &source),
            parse(
                "elsewhere/util.ts",
                "export function check_time_input(value: string) { return value; }\n",
            ),
        ];
        assert!(files[1]
            .imports
            .iter()
            .all(|import| import.module_path != "util"));
        let raw_sites: BTreeSet<_> = files[1]
            .relations
            .iter()
            .filter(|relation| {
                relation.kind == RelationKind::Calls && relation.dst_name == "check_time_input"
            })
            .map(|relation| {
                assert_eq!(relation.receiver.as_deref(), Some("util"));
                relation
                    .site
                    .as_ref()
                    .expect("unresolved call still has its site")
                    .start_line
            })
            .collect();
        assert_eq!(raw_sites, source_lines(&source, "util.check_time_input("));
        assert_eq!(raw_sites.len(), 6);
        for (route, relations) in routes(&files) {
            for file in [&files[0], &files[2]] {
                let candidate = id(file, "check_time_input", EntityKind::Function);
                assert!(
                    call_lines(&relations, &files[1].file_path, candidate).is_empty(),
                    "{route}/{argument}: custom loader is not import authority"
                );
            }
        }
    }
}

#[test]
fn an_object_member_does_not_borrow_a_same_named_function_import() {
    for suffix in ["js", "ts"] {
        let source = "import { read } from './provider';\nimport * as namespace from './alternate';\nexport function inspect(object) {\n  object.read('object');\n  namespace.read('namespace');\n  return read('bare import');\n}\n";
        let files = [
            parse(
                &format!("src/provider.{suffix}"),
                "export function read(value) { return value; }\n",
            ),
            parse(
                &format!("src/alternate.{suffix}"),
                "export function read(value) { return value; }\n",
            ),
            parse(&format!("src/consumer.{suffix}"), source),
        ];
        let object_call = files[2]
            .relations
            .iter()
            .find(|relation| {
                relation.kind == RelationKind::Calls
                    && relation.receiver.as_deref() == Some("object")
            })
            .expect("object member call");
        assert_eq!(
            object_call.import_source, None,
            "{suffix}: member leaf is not the imported binding"
        );
        let provider = id(&files[0], "read", EntityKind::Function);
        let alternate = id(&files[1], "read", EntityKind::Function);
        for (route, relations) in routes(&files) {
            assert_eq!(
                call_lines(&relations, &files[2].file_path, provider),
                BTreeSet::from([5]),
                "{suffix}/{route}"
            );
            assert_eq!(
                call_lines(&relations, &files[2].file_path, alternate),
                BTreeSet::from([4]),
                "{suffix}/{route}"
            );
        }
    }
}
