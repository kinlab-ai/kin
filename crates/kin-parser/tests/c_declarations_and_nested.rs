// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The C declaration census and nested-definition census.

use kin_model::{EntityKind, FilePathId};
use kin_parser::declaration_census::{
    take_census, REASON_CLASS_MEMBER, REASON_MODULE_BINDING, REASON_NESTED_IN_CALLABLE,
};
use kin_parser::{CAdapter, LanguageAdapter};

const STORE_C: &str = r#"#include <stdlib.h>
#include "store.h"

#define STORE_MAGIC 0x5354

typedef struct store_ops {
    int (*save)(struct store *s);
    void (*free)(struct store *s);
} store_ops_t;

enum store_state { STORE_OPEN, STORE_CLOSED };

static int open_count;
const int store_version = 3;

int store_save(struct store *s);

static void on_timeout(void *timer) {
    struct retry { int attempts; } retry = {0};
    int helper(int x) { return x + retry.attempts; }
    FOREACH_STORE(s) { store_save(s); }
    (void)helper(1);
}

int store_save(struct store *s) {
    return s->ops->save(s);
}
"#;

fn parse() -> (tree_sitter::Tree, kin_parser::ParseOutput) {
    let adapter = CAdapter;
    let tree = adapter.parse(STORE_C.as_bytes()).unwrap();
    let output = adapter
        .extract(&tree, STORE_C.as_bytes(), &FilePathId::new("src/store.c"))
        .unwrap();
    (tree, output)
}

#[test]
fn admitted_c_declarations_pair_and_the_rest_are_listed() {
    let (tree, output) = parse();
    let census = take_census(
        CAdapter
            .declaration_census(&tree, STORE_C.as_bytes())
            .expect("the C adapter takes a census"),
        &output.entities,
    );
    let listed: Vec<(&str, &str)> = census
        .unadmitted
        .iter()
        .map(|declaration| (declaration.name.as_str(), declaration.reason.as_str()))
        .collect();
    for admitted in [
        "STORE_MAGIC",
        "store_ops",
        "store_ops_t",
        "store_state",
        "store_version",
        "store_save",
        "on_timeout",
    ] {
        assert!(
            !listed.iter().any(|(name, _)| *name == admitted),
            "{admitted} is admitted: {listed:?}"
        );
    }
    for expected in [
        ("open_count", REASON_MODULE_BINDING),
        ("save", REASON_CLASS_MEMBER),
        ("free", REASON_CLASS_MEMBER),
        ("STORE_OPEN", REASON_CLASS_MEMBER),
        ("retry", REASON_NESTED_IN_CALLABLE),
        ("helper", REASON_NESTED_IN_CALLABLE),
    ] {
        assert!(listed.contains(&expected), "{expected:?} in {listed:?}");
    }
    assert!(
        !listed.iter().any(|(name, _)| *name == "FOREACH_STORE"),
        "a macro-opened block declares nothing: {listed:?}"
    );
    assert_eq!(census.seen, census.admitted + census.unadmitted.len());
}

#[test]
fn a_gnu_nested_function_and_a_local_struct_are_recorded_not_admitted() {
    let (_, output) = parse();
    let census = output.nested_definitions.expect("a census");
    let declined: Vec<(&str, EntityKind, Option<&str>)> = census
        .not_admitted
        .iter()
        .map(|definition| {
            (
                definition.name.as_str(),
                definition.kind,
                definition.enclosing.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        declined,
        vec![
            (
                "on_timeout.<locals>.retry",
                EntityKind::Class,
                Some("on_timeout")
            ),
            (
                "on_timeout.<locals>.helper",
                EntityKind::Function,
                Some("on_timeout")
            ),
        ]
    );
}
