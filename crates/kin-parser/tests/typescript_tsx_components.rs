// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! A `.tsx` React component parses whole, and everything read from a clean
//! tree comes out of it: its entities, the calls written inside JSX, and the
//! value-origin facts the linker types receivers with.
//!
//! The TypeScript adapter used to parse every file with the non-JSX grammar,
//! which read each JSX element as an error. A component file came back
//! `Incomplete`, and the value-origin reader, which skips any tree holding an
//! error, attached no fact to any of its functions.

use kin_model::{Entity, EntityKind, FilePathId, LanguageId, ParseState, RelationKind};
use kin_parser::value_origins::{
    attach_value_origins, read_value_origins, ProviderLookup, ValueOrigin,
};
use kin_parser::{AdapterRegistry, LanguageAdapter, ParseOutput, TypeScriptAdapter};

/// A DevTools-shaped component: a typed props alias, a context read, a
/// callback closing over the context value, calls written inside JSX, and a
/// generic arrow in the `<T,>` form only TSX accepts.
const COMPONENT: &str = r#"import * as React from 'react';
import {useCallback, useContext} from 'react';
import {StoreContext} from './context';
import type Store from './store';

type BadgeProps = {
  store: Store,
  ownerID: number,
};

export function OwnerBadge({store, ownerID}: BadgeProps): React.ReactNode {
  const isInStore = store.containsElement(ownerID);
  return <span className="badge">{isInStore ? 'in' : 'out'}</span>;
}

export function OwnersList({ownerID}: {ownerID: number}) {
  const store = useContext(StoreContext);
  const select = useCallback(
    (id: number) => {
      if (store.containsElement(id)) {
        store.selectElement(id);
      }
    },
    [store],
  );
  const identity = <T,>(value: T): T => value;
  return (
    <ul>
      <li onClick={() => select(identity(ownerID))}>
        {store.containsElement(ownerID) ? 'in' : 'out'}
      </li>
      <OwnerBadge store={store} ownerID={ownerID} />
    </ul>
  );
}
"#;

struct Parsed {
    output: ParseOutput,
    entities: Vec<Entity>,
    clean: bool,
}

/// Parse `source` the way ingestion does: pick the adapter by extension, parse,
/// extract, and attach the value-origin facts to the entities.
fn parse(path: &str, source: &str) -> Parsed {
    let registry = AdapterRegistry::new();
    let ext = path.rsplit('.').next().unwrap();
    let adapter = registry
        .get_by_extension_and_content(ext, source.as_bytes())
        .expect("an adapter serves the extension");
    assert_eq!(adapter.language_id(), LanguageId::TypeScript);
    let tree = adapter.parse(source.as_bytes()).expect("parses");
    let file_id = FilePathId::new(path);
    let output = adapter
        .extract(&tree, source.as_bytes(), &file_id)
        .expect("extracts");
    let mut entities: Vec<Entity> = output
        .entities
        .iter()
        .cloned()
        .map(|entity| entity.into_entity(LanguageId::TypeScript, &file_id))
        .collect();
    attach_value_origins(
        &tree,
        source.as_bytes(),
        LanguageId::TypeScript,
        path,
        &mut entities,
    );
    Parsed {
        output,
        entities,
        clean: !tree.root_node().has_error(),
    }
}

fn entity<'a>(parsed: &'a Parsed, name: &str) -> &'a Entity {
    parsed
        .entities
        .iter()
        .find(|entity| entity.name == name)
        .unwrap_or_else(|| {
            panic!(
                "no entity {name}; have {:?}",
                parsed
                    .entities
                    .iter()
                    .map(|e| (e.kind, e.name.as_str()))
                    .collect::<Vec<_>>()
            )
        })
}

#[test]
fn a_tsx_component_parses_whole_with_its_entities_and_jsx_calls() {
    let parsed = parse("src/views/OwnersList.tsx", COMPONENT);
    assert!(parsed.clean, "the TSX grammar reads the component whole");
    assert!(
        matches!(parsed.output.parse_state, ParseState::Valid),
        "{:?}",
        parsed.output.parse_state
    );

    for name in ["OwnerBadge", "OwnersList"] {
        assert_eq!(entity(&parsed, name).kind, EntityKind::Function, "{name}");
    }

    let calls_from = |caller: &str| -> Vec<(String, Option<String>)> {
        parsed
            .output
            .relations
            .iter()
            .filter(|relation| relation.kind == RelationKind::Calls && relation.src_name == caller)
            .map(|relation| (relation.dst_name.clone(), relation.receiver.clone()))
            .collect()
    };
    let list_calls = calls_from("OwnersList");
    let through_store = list_calls
        .iter()
        .filter(|(callee, receiver)| {
            callee == "containsElement" && receiver.as_deref() == Some("store")
        })
        .count();
    assert_eq!(
        through_store, 2,
        "the call in the callback and the call inside JSX both belong to the component: \
         {list_calls:?}"
    );
    assert!(
        list_calls.iter().any(|(callee, _)| callee == "useContext"),
        "{list_calls:?}"
    );
    assert!(
        calls_from("OwnerBadge")
            .iter()
            .any(|(callee, receiver)| callee == "containsElement"
                && receiver.as_deref() == Some("store")),
        "{:?}",
        calls_from("OwnerBadge")
    );
}

#[test]
fn a_tsx_component_carries_its_value_origin_facts() {
    let parsed = parse("src/views/OwnersList.tsx", COMPONENT);

    let badge = read_value_origins(entity(&parsed, "OwnerBadge"))
        .expect("the typed-props component carries facts");
    assert_eq!(
        badge.bindings.get("store"),
        Some(&ValueOrigin::Declared {
            type_name: "Store".into()
        }),
        "a prop destructured from a declared object type takes the property's type"
    );

    let list = read_value_origins(entity(&parsed, "OwnersList"))
        .expect("the context-reading component carries facts");
    assert_eq!(
        list.bindings.get("store"),
        Some(&ValueOrigin::Provided {
            lookup: ProviderLookup::Binding {
                kind: "react_context".into()
            },
            name: "StoreContext".into(),
        }),
        "useContext through 'react' reads the context's provider"
    );
}

#[test]
fn a_ts_type_assertion_is_still_read_by_the_typescript_grammar() {
    // `<number>value` is a type assertion in a `.ts` file and would open a
    // JSX element under TSX. The TypeScript grammar reads this file cleanly,
    // so it keeps it, and the assertion stays an assertion.
    let source = "export function widen(value: unknown): number {\n  const n = <number>value;\n  return n + 1;\n}\n";
    let tree = TypeScriptAdapter.parse(source.as_bytes()).unwrap();
    assert!(!tree.root_node().has_error());
    let mut stack = vec![tree.root_node()];
    let mut assertion = false;
    while let Some(node) = stack.pop() {
        assertion |= node.kind() == "type_assertion";
        let mut cursor = node.walk();
        stack.extend(node.children(&mut cursor));
    }
    assert!(assertion, "{}", tree.root_node().to_sexp());
}
