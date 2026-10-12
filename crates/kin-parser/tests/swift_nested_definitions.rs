// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Definitions nested in Swift function bodies: nested functions and local
//! types become entities named `enclosing.<locals>.name`, the calls in their
//! bodies move onto them, and what the adapter declines is recorded.

use kin_model::{EntityKind, FilePathId, LanguageId, RelationKind};
use kin_parser::{LanguageAdapter, ParseOutput, SwiftAdapter};

const SYNC_TESTS: &str = r#"import XCTest

final class SyncServiceTests: XCTestCase {
    func testRetriesAfterATransportError() throws {
        final class FailingTransport: Transport {
            var attempts = 0
            func send(_ frame: Frame) throws {
                attempts += 1
                recordFailure(frame)
            }
        }
        func makeService(transport: Transport) -> SyncService {
            let service = SyncService(transport: transport)
            service.configure()
            return service
        }
        let onDone = { (code: Int) in report(code) }
        let transport = FailingTransport()
        let service = makeService(transport: transport)
        try service.sync()
        onDone(0)
        run(makeService)
    }

    func testAnotherCase() {
        func makeService(transport: Transport) -> SyncService {
            return SyncService(transport: transport)
        }
        _ = makeService(transport: StubTransport())
    }
}
"#;

fn extract(source: &str) -> ParseOutput {
    let adapter = SwiftAdapter;
    let tree = adapter.parse(source.as_bytes()).unwrap();
    assert!(
        !tree.root_node().has_error(),
        "{}",
        tree.root_node().to_sexp()
    );
    adapter
        .extract(
            &tree,
            source.as_bytes(),
            &FilePathId::new("Tests/SyncTests/SyncServiceTests.swift"),
        )
        .unwrap()
}

fn calls_from(output: &ParseOutput, caller: &str) -> Vec<String> {
    let mut calls: Vec<String> = output
        .relations
        .iter()
        .filter(|relation| relation.kind == RelationKind::Calls && relation.src_name == caller)
        .map(|relation| relation.dst_name.clone())
        .collect();
    calls.sort();
    calls
}

const TEST: &str = "SyncServiceTests.testRetriesAfterATransportError";

#[test]
fn nested_functions_and_local_types_are_entities_that_own_their_calls() {
    let output = extract(SYNC_TESTS);
    let transport = format!("{TEST}.<locals>.FailingTransport");
    let factory = format!("{TEST}.<locals>.makeService");
    let kinds: Vec<(&str, EntityKind)> = output
        .entities
        .iter()
        .map(|entity| (entity.name.as_str(), entity.kind))
        .collect();
    for (name, kind) in [
        (transport.clone(), EntityKind::Class),
        (format!("{transport}.send"), EntityKind::Method),
        (factory.clone(), EntityKind::Function),
        (
            "SyncServiceTests.testAnotherCase.<locals>.makeService".to_string(),
            EntityKind::Function,
        ),
    ] {
        assert!(
            kinds.contains(&(name.as_str(), kind)),
            "{name} in {kinds:?}"
        );
    }
    assert!(output.relations.iter().any(|relation| {
        relation.kind == RelationKind::Implements
            && relation.src_name == transport
            && relation.dst_name == "Transport"
    }));
    assert_eq!(
        calls_from(&output, &format!("{transport}.send")),
        vec!["recordFailure"]
    );
    let factory_calls = calls_from(&output, &factory);
    assert!(
        factory_calls.contains(&"SyncService".to_string()),
        "{factory_calls:?}"
    );
    assert!(
        factory_calls.contains(&"configure".to_string()),
        "{factory_calls:?}"
    );

    let test_calls = calls_from(&output, TEST);
    for expected in [factory.as_str(), transport.as_str(), "sync"] {
        assert!(
            test_calls.iter().any(|call| call == expected),
            "{expected} in {test_calls:?}"
        );
    }
    for moved in ["recordFailure", "configure", "SyncService"] {
        assert!(
            !test_calls.iter().any(|call| call == moved),
            "{moved}: {test_calls:?}"
        );
    }
    // The closure bound to a local stays with the test and is declined.
    assert!(test_calls.contains(&"report".to_string()), "{test_calls:?}");
    assert!(calls_from(&output, "SyncServiceTests.testAnotherCase")
        .contains(&"SyncServiceTests.testAnotherCase.<locals>.makeService".to_string()));
}

#[test]
fn a_closure_bound_to_a_local_is_recorded_as_declined() {
    let output = extract(SYNC_TESTS);
    let census = output.nested_definitions.as_ref().expect("a census");
    let declined: Vec<(&str, EntityKind)> = census
        .not_admitted
        .iter()
        .map(|definition| (definition.name.as_str(), definition.kind))
        .collect();
    assert_eq!(
        declined,
        vec![(
            "SyncServiceTests.testRetriesAfterATransportError.<locals>.onDone",
            EntityKind::Function
        )]
    );
}

#[test]
fn passing_a_nested_function_as_a_value_is_a_use_of_it() {
    let source = SYNC_TESTS.as_bytes();
    let tree = SwiftAdapter.parse(source).unwrap();
    let census = kin_parser::value_escape::census_file(&tree, source, LanguageId::Swift).unwrap();
    let uses = census.record.values.get("makeService").expect("a use");
    assert!(uses.iter().any(|occurrence| occurrence.binding
        == kin_model::OccurrenceBinding::Local
        && occurrence.target_line == Some(11)
        && occurrence.context == kin_model::OccurrenceContext::Argument));
}
