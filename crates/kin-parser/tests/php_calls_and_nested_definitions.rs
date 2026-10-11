// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! PHP call shapes and nested definitions: `$this`, `self`, `static`,
//! `parent` and named-class calls are owner-qualified, a call through a
//! typed receiver carries its receiver for the linker's value-origin tier,
//! and a closure bound once to a local is its own entity.

use kin_model::{EntityKind, FilePathId, LanguageId, RelationKind};
use kin_parser::value_origins::{attach_value_origins, read_value_origins, ValueOrigin};
use kin_parser::{LanguageAdapter, ParseOutput, PhpAdapter};

const CHECKOUT: &str = r#"<?php
namespace App\Services;

use App\Models\Store;
use App\Models\Registry as Reg;

class Checkout extends BaseService
{
    private Store $store;

    public function __construct(private readonly Cache $cache, Store $store)
    {
        $this->store = $store;
    }

    public function place(Store $primary, ?Reg $registry, $untyped): void
    {
        $primary->save();
        $this->store->save();
        $this->cache->save();
        $this->audit();
        self::log();
        static::log();
        parent::boot();
        Store::create();
        $found = $registry->lookup('main');
        $found->save();
        $fresh = new Store();
        $fresh->save();
        $untyped->save();
        $onDone = function (int $code) use ($primary) {
            report($code);
            $primary->save();
        };
        $format = fn(Store $s) => $s->label();
        $onDone(0);
        $format($primary);
        $anon = new class {
            public function run() {}
        };
    }

    public function audit(): void {}
    public static function log(): void {}
}

function helpers_boot()
{
    function late_helper() { return make(); }
    return late_helper();
}
"#;

fn extract(source: &str) -> ParseOutput {
    let adapter = PhpAdapter;
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
            &FilePathId::new("app/Services/Checkout.php"),
        )
        .unwrap()
}

fn calls_from(output: &ParseOutput, caller: &str) -> Vec<(String, Option<String>)> {
    let mut calls: Vec<(String, Option<String>)> = output
        .relations
        .iter()
        .filter(|relation| relation.kind == RelationKind::Calls && relation.src_name == caller)
        .map(|relation| (relation.dst_name.clone(), relation.receiver.clone()))
        .collect();
    calls.sort();
    calls
}

fn call(dst: &str, receiver: Option<&str>) -> (String, Option<String>) {
    (dst.to_string(), receiver.map(str::to_string))
}

#[test]
fn own_instance_and_named_class_calls_are_owner_qualified() {
    let output = extract(CHECKOUT);
    let place = calls_from(&output, "Checkout.place");
    for expected in [
        call("Checkout.audit", None),
        call("Checkout.log", None),
        call("BaseService.boot", None),
        call("Store.create", None),
    ] {
        assert!(place.contains(&expected), "{expected:?} in {place:?}");
    }
    assert_eq!(
        place
            .iter()
            .filter(|(dst, _)| dst == "Checkout.log")
            .count(),
        2,
        "self:: and static::"
    );
    assert!(
        output
            .relations
            .iter()
            .any(|relation| relation.kind == RelationKind::Extends
                && relation.src_name == "Checkout"
                && relation.dst_name == "BaseService"),
        "the extends clause is a child, not a field"
    );
}

#[test]
fn typed_receivers_carry_their_receiver_for_the_linker() {
    let output = extract(CHECKOUT);
    let place = calls_from(&output, "Checkout.place");
    for receiver in [
        "$primary",
        "$this->store",
        "$this->cache",
        "$found",
        "$fresh",
    ] {
        assert!(
            place.contains(&call("save", Some(receiver))),
            "{receiver} in {place:?}"
        );
    }
    assert!(
        place.contains(&call("save", None)),
        "an untyped parameter keeps the bare call: {place:?}"
    );
    assert!(
        place.contains(&call("lookup", Some("$registry"))),
        "{place:?}"
    );
}

#[test]
fn closures_bound_to_locals_are_entities_that_own_their_calls() {
    let output = extract(CHECKOUT);
    let on_done = "Checkout.place.<locals>.onDone";
    let format = "Checkout.place.<locals>.format";
    for name in [on_done, format] {
        let entity = output
            .entities
            .iter()
            .find(|entity| entity.name == name)
            .unwrap_or_else(|| panic!("no {name}"));
        assert_eq!(entity.kind, EntityKind::Function);
    }
    let done_calls = calls_from(&output, on_done);
    assert!(done_calls.contains(&call("report", None)), "{done_calls:?}");
    assert!(
        done_calls.contains(&call("save", Some("$primary"))),
        "a captured typed variable keeps its type: {done_calls:?}"
    );
    assert_eq!(calls_from(&output, format), vec![call("label", Some("$s"))]);
    let place = calls_from(&output, "Checkout.place");
    assert!(place.contains(&call(on_done, None)), "{place:?}");
    assert!(place.contains(&call(format, None)), "{place:?}");
    assert!(!place.contains(&call("report", None)), "{place:?}");

    let census = output.nested_definitions.as_ref().expect("a census");
    assert!(
        census
            .not_admitted
            .iter()
            .any(|definition| definition.kind == EntityKind::Class
                && definition
                    .name
                    .starts_with("Checkout.place.<locals>.anonymous@")),
        "{:?}",
        census.not_admitted
    );
}

#[test]
fn a_function_declared_in_a_body_is_global() {
    let output = extract(CHECKOUT);
    assert!(output
        .entities
        .iter()
        .any(|entity| entity.name == "late_helper" && entity.kind == EntityKind::Function));
    assert_eq!(calls_from(&output, "late_helper"), vec![call("make", None)]);
    assert_eq!(
        calls_from(&output, "helpers_boot"),
        vec![call("late_helper", None)]
    );
}

#[test]
fn a_bound_closure_carries_its_own_value_origin_facts() {
    let adapter = PhpAdapter;
    let bytes = CHECKOUT.as_bytes();
    let tree = adapter.parse(bytes).unwrap();
    let file_id = FilePathId::new("app/Services/Checkout.php");
    let output = adapter.extract(&tree, bytes, &file_id).unwrap();
    let mut entities: Vec<kin_model::Entity> = output
        .entities
        .into_iter()
        .map(|entity| entity.into_entity(LanguageId::Php, &file_id))
        .collect();
    attach_value_origins(
        &tree,
        bytes,
        LanguageId::Php,
        "app/Services/Checkout.php",
        &mut entities,
    );
    let format = entities
        .iter()
        .find(|entity| entity.name == "Checkout.place.<locals>.format")
        .unwrap();
    assert_eq!(
        read_value_origins(format).unwrap().bindings.get("$s"),
        Some(&ValueOrigin::Declared {
            type_name: "Store".into()
        })
    );
    let place = entities
        .iter()
        .find(|entity| entity.name == "Checkout.place")
        .unwrap();
    let facts = read_value_origins(place).unwrap();
    assert_eq!(
        facts.bindings.get("$this->store"),
        Some(&ValueOrigin::Declared {
            type_name: "Store".into()
        })
    );
    assert_eq!(
        facts.bindings.get("$registry"),
        Some(&ValueOrigin::Declared {
            type_name: "Registry".into()
        })
    );
}
