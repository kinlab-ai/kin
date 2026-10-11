// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Definitions nested in C++ function bodies: local classes and lambdas bound
//! to `auto` locals become entities named `enclosing.<locals>.name`, the calls
//! in their bodies move onto them, and what the adapter declines is recorded.

use kin_model::{EntityKind, FilePathId, RelationKind};
use kin_parser::{CppAdapter, LanguageAdapter, ParseOutput};

const SESSION_TEST: &str = r#"#include "session.hpp"
#include <algorithm>
#include <functional>

void session_retries_after_a_transport_error(Session& session, Frame frame) {
  class FailingTransport : public Transport {
   public:
    void send(const Frame& frame) override {
      record(frame);
      fail_next();
    }
    int attempts() const { return count_; }

   private:
    int count_ = 0;
  };
  auto on_done = [&](int code) {
    report(code);
    session.finish();
  };
  auto retry = [](Session& s) { s.reconnect(); };
  std::function<void()> fallback = [] { log_fallback(); };
  FailingTransport transport;
  transport.send(frame);
  session.attach(transport);
  on_done(0);
  retry(session);
  fallback();
  std::for_each(frames.begin(), frames.end(), [](Frame& f) { f.flush(); });
}

void another_test() {
  auto on_done = [] { log_done(); };
  on_done();
  if (flaky()) {
    struct Probe { void hit() { mark(); } };
    Probe().hit();
  } else {
    struct Probe { void hit() { unmark(); } };
    Probe().hit();
  }
}
"#;

fn extract(path: &str, source: &str) -> ParseOutput {
    let adapter = CppAdapter;
    let tree = adapter.parse(source.as_bytes()).unwrap();
    assert!(
        !tree.root_node().has_error(),
        "{}",
        tree.root_node().to_sexp()
    );
    adapter
        .extract(&tree, source.as_bytes(), &FilePathId::new(path))
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

const OUTER: &str = "session_retries_after_a_transport_error";

#[test]
fn a_local_class_and_its_methods_are_entities_under_the_enclosing_function() {
    let output = extract("tests/session_test.cpp", SESSION_TEST);
    let class = format!("{OUTER}.<locals>.FailingTransport");
    let kinds: Vec<(&str, EntityKind)> = output
        .entities
        .iter()
        .map(|entity| (entity.name.as_str(), entity.kind))
        .collect();
    for (name, kind) in [
        (class.clone(), EntityKind::Class),
        (format!("{class}::send"), EntityKind::Method),
        (format!("{class}::attempts"), EntityKind::Method),
    ] {
        assert!(
            kinds.contains(&(name.as_str(), kind)),
            "{name} in {kinds:?}"
        );
    }
    assert!(output.relations.iter().any(|relation| {
        relation.kind == RelationKind::Extends
            && relation.src_name == class
            && relation.dst_name == "Transport"
    }));
    assert!(output.relations.iter().any(|relation| {
        relation.kind == RelationKind::Contains
            && relation.src_name == class
            && relation.dst_name == format!("{class}::send")
    }));
    assert_eq!(
        calls_from(&output, &format!("{class}::send")),
        vec!["fail_next", "record"],
        "the method's calls are its own"
    );
}

#[test]
fn a_lambda_bound_to_an_auto_local_owns_its_calls_and_is_called_by_name() {
    let output = extract("tests/session_test.cpp", SESSION_TEST);
    let on_done = format!("{OUTER}.<locals>.on_done");
    let retry = format!("{OUTER}.<locals>.retry");
    for name in [&on_done, &retry] {
        let entity = output
            .entities
            .iter()
            .find(|entity| &entity.name == name)
            .unwrap_or_else(|| panic!("no {name}"));
        assert_eq!(entity.kind, EntityKind::Function);
    }
    assert_eq!(
        calls_from(&output, &on_done),
        vec!["Session::finish", "report"],
        "the lambda's calls, typed by the enclosing function's parameters"
    );
    assert_eq!(calls_from(&output, &retry), vec!["Session::reconnect"]);
    let outer = calls_from(&output, OUTER);
    let transport_send = format!("{OUTER}.<locals>.FailingTransport::send");
    for expected in [
        on_done.as_str(),
        retry.as_str(),
        transport_send.as_str(),
        "Session::attach",
        "fallback",
        "Frame::flush",
    ] {
        assert!(
            outer.iter().any(|call| call == expected),
            "{expected} in {outer:?}"
        );
    }
    for moved in [
        "report",
        "Session::finish",
        "Session::reconnect",
        "record",
        "fail_next",
    ] {
        assert!(
            !outer.iter().any(|call| call == moved),
            "{moved} moved off: {outer:?}"
        );
    }
    // A lambda passed inline stays with the function that holds it, and a
    // std::function local is no admitted definition: its call keeps the bare
    // name, and the lambda's call stays with the enclosing function.
    assert!(outer.iter().any(|call| call == "log_fallback"));

    let other = "another_test.<locals>.on_done";
    assert_eq!(calls_from(&output, other), vec!["log_done"]);
    assert!(calls_from(&output, "another_test")
        .iter()
        .any(|call| call == other));
}

#[test]
fn what_the_adapter_declines_is_recorded() {
    let output = extract("tests/session_test.cpp", SESSION_TEST);
    let census = output.nested_definitions.as_ref().expect("a census");
    let declined: Vec<(&str, EntityKind)> = census
        .not_admitted
        .iter()
        .map(|definition| (definition.name.as_str(), definition.kind))
        .collect();
    assert!(
        declined.contains(&(
            "session_retries_after_a_transport_error.<locals>.fallback",
            EntityKind::Function
        )),
        "{declined:?}"
    );
    assert!(
        declined.contains(&("another_test.<locals>.Probe", EntityKind::Class)),
        "the second class of one local-scope name is declined: {declined:?}"
    );
    let probes: Vec<&str> = output
        .entities
        .iter()
        .filter(|entity| entity.name.starts_with("another_test.<locals>.Probe"))
        .map(|entity| entity.name.as_str())
        .collect();
    assert_eq!(
        probes,
        vec![
            "another_test.<locals>.Probe",
            "another_test.<locals>.Probe::hit"
        ]
    );
    assert_eq!(
        calls_from(&output, "another_test.<locals>.Probe::hit"),
        vec!["mark"]
    );
    let another = calls_from(&output, "another_test");
    assert_eq!(
        another
            .iter()
            .filter(|call| call.as_str() == "another_test.<locals>.Probe")
            .count(),
        1,
        "only the admitted class's construction reaches its entity: {another:?}"
    );
}
