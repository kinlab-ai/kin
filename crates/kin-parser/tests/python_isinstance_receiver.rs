// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Guard-derived receiver candidates retain their exact call site and uncertainty.

use kin_model::{FilePathId, RelationKind};
use kin_parser::{
    is_call_extraction_incomplete_marker, LanguageAdapter, ParseOutput, PythonAdapter,
};

fn parse(source: &str) -> ParseOutput {
    let bytes = source.as_bytes();
    let tree = PythonAdapter.parse(bytes).expect("parse");
    PythonAdapter
        .extract(&tree, bytes, &FilePathId::new("aliases.py"))
        .expect("extract")
}

fn guarded(output: &ParseOutput) -> Vec<&kin_parser::ExtractedRelation> {
    output
        .relations
        .iter()
        .filter(|rel| {
            rel.kind == RelationKind::Calls
                && rel.site.as_ref().is_some_and(|site| {
                    serde_json::to_value(site.syntactic_role).unwrap()
                        == serde_json::Value::String("python_guarded_receiver".into())
                })
        })
        .collect()
}

#[test]
fn a_singleton_guard_types_the_loop_receiver_only_at_its_call_site() {
    let source = "class AliasPath:\n    def convert_to_aliases(self):\n        return []\n\nclass AliasChoices:\n    def convert_to_aliases(self):\n        aliases = []\n        for c in self.choices:\n            if isinstance(c, AliasPath):\n                aliases.append(c.convert_to_aliases())\n        return aliases\n";
    let output = parse(source);
    let calls = guarded(&output);
    assert_eq!(
        calls.len(),
        1,
        "the guarded loop call needs a class-grounded candidate"
    );
    let call = calls[0];
    assert_eq!(call.src_name, "AliasChoices.convert_to_aliases");
    assert_eq!(call.dst_name, "AliasPath.convert_to_aliases");
    assert_eq!(call.receiver.as_deref(), Some("c"));
    let site = call.site.as_ref().unwrap();
    assert_eq!(
        &source[site.start_byte..site.end_byte],
        "c.convert_to_aliases()"
    );
    assert_eq!(site.start_line, 9);
    assert!(
        output
            .relations
            .iter()
            .any(is_call_extraction_incomplete_marker),
        "a nominal guard still permits subclass dispatch and cannot certify one body"
    );
}

#[test]
fn a_tuple_guard_keeps_two_candidate_owners_on_one_expression() {
    let source = "from aliases import AliasChoices, AliasPath\n\ndef convert(alias: str | AliasChoices | AliasPath | None):\n    if isinstance(alias, (AliasChoices, AliasPath)):\n        return alias.convert_to_aliases()\n    return alias\n";
    let output = parse(source);
    let calls = guarded(&output);
    let mut names: Vec<_> = calls.iter().map(|call| call.dst_name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "AliasChoices.convert_to_aliases",
            "AliasPath.convert_to_aliases"
        ]
    );
    assert_eq!(
        calls[0].site, calls[1].site,
        "two candidates must share one call occurrence"
    );
    let site = calls[0].site.as_ref().unwrap();
    assert_eq!(
        &source[site.start_byte..site.end_byte],
        "alias.convert_to_aliases()"
    );
    assert_eq!(site.start_line, 4);
    assert!(output
        .relations
        .iter()
        .any(is_call_extraction_incomplete_marker));
}

#[test]
fn narrowing_does_not_leak_to_else_after_guard_or_nested_execution_scopes() {
    let source = "class AliasPath:\n    def convert_to_aliases(self):\n        return []\n\ndef convert(alias):\n    if isinstance(alias, AliasPath):\n        alias.convert_to_aliases()\n        def nested():\n            return alias.convert_to_aliases()\n    else:\n        alias.convert_to_aliases()\n    return alias.convert_to_aliases()\n";
    let output = parse(source);
    let calls = guarded(&output);
    assert_eq!(
        calls.len(),
        1,
        "only the direct guarded execution scope is narrowed"
    );
    assert_eq!(calls[0].site.as_ref().unwrap().start_line, 6);
}

#[test]
fn shadowed_or_negative_guards_do_not_supply_receiver_owners() {
    for source in [
        "class AliasPath: pass\ndef convert(alias, isinstance):\n    if isinstance(alias, AliasPath):\n        return alias.convert_to_aliases()\n",
        "class AliasPath: pass\nisinstance = custom_check\ndef convert(alias):\n    if isinstance(alias, AliasPath):\n        return alias.convert_to_aliases()\n",
        "class AliasPath: pass\ndef convert(alias, AliasPath):\n    if isinstance(alias, AliasPath):\n        return alias.convert_to_aliases()\n",
        "class AliasPath: pass\ndef convert(alias):\n    if not isinstance(alias, AliasPath):\n        return alias.convert_to_aliases()\n",
        "class AliasPath: pass\ndef convert(alias):\n    if isinstance(alias, AliasPath):\n        alias = other\n        return alias.convert_to_aliases()\n",
        "from foreign import *\nclass AliasPath: pass\ndef convert(alias):\n    if isinstance(alias, AliasPath):\n        return alias.convert_to_aliases()\n",
    ] {
        let output = parse(source);
        assert!(guarded(&output).iter().all(|rel| !rel.dst_name.contains('.')),
                "the guard has no admissible owner: {source}");
    }
}

#[test]
fn a_conjunction_holding_the_guard_narrows_and_a_disjunction_does_not() {
    let source = "class AliasPath:\n    def convert_to_aliases(self):\n        return []\n\ndef convert(alias, ready):\n    if ready and isinstance(alias, AliasPath):\n        alias.convert_to_aliases()\n    if ready or isinstance(alias, AliasPath):\n        alias.convert_to_aliases()\n    elif isinstance(alias, AliasPath):\n        alias.convert_to_aliases()\n";
    let output = parse(source);
    let lines: Vec<_> = guarded(&output)
        .iter()
        .map(|call| call.site.as_ref().unwrap().start_line)
        .collect();
    assert_eq!(
        lines,
        [6, 10],
        "the `and` branch and the `elif` branch, never the `or` branch"
    );
}

#[test]
fn the_guard_record_names_the_guard_expression_of_its_call() {
    let source = "class AliasPath:\n    def convert_to_aliases(self):\n        return []\n\ndef convert(alias):\n    if isinstance(alias,\n                  AliasPath):\n        return alias.convert_to_aliases()\n";
    let output = parse(source);
    let calls = guarded(&output);
    assert_eq!(calls.len(), 1);
    let site = calls[0].site.as_ref().unwrap();
    let guards = kin_parser::file_evidence::receiver_guards_of(&output.relations);
    let guard = &guards[&(site.start_byte, site.end_byte)];
    assert_eq!(guard.text, "isinstance(alias, AliasPath)");
    assert_eq!(guard.site.start_line, 5);
    assert_eq!(
        &source[guard.site.start_byte..guard.site.end_byte],
        "isinstance(alias,\n                  AliasPath)"
    );
}

#[test]
fn an_ungrounded_guard_leaves_a_gap_and_no_named_call() {
    for guard in ["str", "(AliasPath, Missing)", "AliasPath | Missing"] {
        let source = format!("class AliasPath:\n    def convert_to_aliases(self):\n        return []\n\ndef convert(alias):\n    if isinstance(alias, {guard}):\n        return alias.convert_to_aliases()\n");
        let output = parse(&source);
        let call_line = 6;
        assert!(
            output
                .relations
                .iter()
                .all(|rel| rel.kind != RelationKind::Calls
                    || rel
                        .site
                        .as_ref()
                        .is_none_or(|site| site.start_line != call_line)),
            "{guard}: no destination is named for an ungrounded guard"
        );
        assert!(
            output
                .relations
                .iter()
                .any(|rel| is_call_extraction_incomplete_marker(rel)
                    && rel
                        .site
                        .as_ref()
                        .is_some_and(|site| site.start_line == call_line)),
            "{guard}: the site stays a recorded gap"
        );
    }
}

#[test]
fn a_decorated_class_still_grounds_the_guard() {
    let source = "import dataclasses\n\n@dataclasses.dataclass(slots=True)\nclass AliasPath:\n    def convert_to_aliases(self):\n        return []\n\ndef convert(alias):\n    if isinstance(alias, AliasPath):\n        return alias.convert_to_aliases()\n";
    let output = parse(source);
    let calls = guarded(&output);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].dst_name, "AliasPath.convert_to_aliases");
}
