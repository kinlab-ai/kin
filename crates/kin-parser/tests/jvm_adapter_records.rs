// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The generic answer-completeness records, as the Java and Kotlin adapters
//! fill them on real-shaped source: value-origin facts on the entities they
//! type, the receivers those facts let a call carry, nested definitions, and
//! the declaration census.

use kin_model::{Entity, EntityKind, FilePathId, LanguageId, RelationKind};
use kin_parser::value_origins::{
    attach_value_origins, read_value_origins, DeclaredReturn, ValueOrigin,
};
use kin_parser::{JavaAdapter, KotlinAdapter, LanguageAdapter, ParseOutput};

fn parse(adapter: &dyn LanguageAdapter, path: &str, source: &str) -> (ParseOutput, Vec<Entity>) {
    let bytes = source.as_bytes();
    let file = FilePathId::new(path);
    let tree = adapter.parse(bytes).unwrap();
    assert!(!tree.root_node().has_error(), "fixture must parse: {path}");
    let output = adapter.extract(&tree, bytes, &file).unwrap();
    let mut entities: Vec<Entity> = output
        .entities
        .iter()
        .cloned()
        .map(|entity| entity.into_entity_with_source(adapter.language_id(), &file, Some(bytes)))
        .collect();
    attach_value_origins(&tree, bytes, adapter.language_id(), path, &mut entities);
    (output, entities)
}

fn names(output: &ParseOutput) -> Vec<&str> {
    output
        .entities
        .iter()
        .map(|entity| entity.name.as_str())
        .collect()
}

fn declared(name: &str) -> ValueOrigin {
    ValueOrigin::Declared {
        type_name: name.into(),
    }
}

/// Every call written in `caller`, as (callee, receiver).
fn calls_of<'a>(output: &'a ParseOutput, caller: &str) -> Vec<(&'a str, Option<&'a str>)> {
    output
        .relations
        .iter()
        .filter(|relation| relation.kind == RelationKind::Calls && relation.src_name == caller)
        .map(|relation| (relation.dst_name.as_str(), relation.receiver.as_deref()))
        .collect()
}

const JAVA_SERVICE: &str = r#"package app.orders;

import app.store.Store;
import app.cache.Cache;
import javax.inject.Inject;

public class OrderService implements Service {
    @Inject private Store store;
    private final Cache cache;

    @Inject
    public OrderService(Cache cache) {
        this.cache = cache;
    }

    public Order find(String id) {
        Order cached = cache.get(id);
        var fresh = new OrderMapper();
        Runnable audit = () -> store.log(id);
        Comparator<Order> byDate = new Comparator<Order>() {
            @Override
            public int compare(Order a, Order b) { return a.date().compareTo(b.date()); }
        };
        executor.submit(new Runnable() {
            @Override
            public void run() { store.flush(); }
        });
        class Retry extends Policy {
            void again() { store.load(id); }
        }
        this.store.load(id);
        return fresh.map(store.load(id));
    }

    public Order load(String id) { return null; }
}

interface Service {
    Order find(String id);
    default void warm() {}
}
"#;

#[test]
fn java_interface_members_and_nested_classes_become_entities() {
    let (output, _) = parse(
        &JavaAdapter,
        "src/main/java/app/orders/OrderService.java",
        JAVA_SERVICE,
    );
    let names = names(&output);
    for expected in [
        "OrderService.find",
        "OrderService.load",
        "Service.find",
        "Service.warm",
        "OrderService.find.<locals>.byDate",
        "OrderService.find.<locals>.byDate.compare",
        "OrderService.find.<locals>.Retry",
        "OrderService.find.<locals>.Retry.again",
    ] {
        assert!(names.contains(&expected), "missing {expected} in {names:?}");
    }
    let census = output
        .nested_definitions
        .as_ref()
        .expect("a census is taken");
    let recorded: Vec<_> = census
        .not_admitted
        .iter()
        .map(|definition| {
            (
                definition.name.as_str(),
                definition.kind,
                definition.enclosing.as_deref(),
                definition.bases.clone(),
            )
        })
        .collect();
    assert_eq!(
        recorded,
        vec![
            (
                "OrderService.find.<locals>.audit",
                EntityKind::Function,
                Some("OrderService.find"),
                Vec::new()
            ),
            (
                "OrderService.find.<locals>.<anonymous>",
                EntityKind::Class,
                Some("OrderService.find"),
                vec!["Runnable".to_string()]
            ),
        ],
        "a lambda bound to a local and an anonymous class passed as an argument are recorded"
    );
    assert!(output.relations.iter().any(|relation| {
        relation.kind == RelationKind::Extends
            && relation.src_name == "OrderService.find.<locals>.Retry"
            && relation.dst_name == "Policy"
    }));
    assert!(output.relations.iter().any(|relation| {
        relation.kind == RelationKind::Extends
            && relation.src_name == "OrderService.find.<locals>.byDate"
            && relation.dst_name == "Comparator"
    }));
    assert!(output.relations.iter().any(|relation| {
        relation.kind == RelationKind::Implements
            && relation.src_name == "OrderService"
            && relation.dst_name == "Service"
    }));

    // Calls in an admitted member move to it; the rest stay with `find`.
    let again = calls_of(&output, "OrderService.find.<locals>.Retry.again");
    assert_eq!(again, vec![("load", None)], "{again:?}");
    let find = calls_of(&output, "OrderService.find");
    assert!(!find.iter().any(|(callee, _)| *callee == "compareTo"));
    assert!(find.contains(&("flush", None)), "{find:?}");
}

#[test]
fn java_receivers_typed_by_facts_are_carried_and_injection_is_read_as_declared() {
    let (output, entities) = parse(
        &JavaAdapter,
        "src/main/java/app/orders/OrderService.java",
        JAVA_SERVICE,
    );
    let find = calls_of(&output, "OrderService.find");
    assert!(find.contains(&("get", Some("cache"))), "{find:?}");
    assert!(find.contains(&("map", Some("fresh"))), "{find:?}");
    assert!(find.contains(&("load", Some("store"))), "{find:?}");
    assert!(
        find.contains(&("load", Some("this.store"))),
        "`this.store.load` carries the one field hop the tier follows: {find:?}"
    );
    assert!(
        find.contains(&("log", Some("store"))),
        "a lambda's call reads the field too: {find:?}"
    );

    let find_entity = entities
        .iter()
        .find(|entity| entity.name == "OrderService.find")
        .unwrap();
    let facts = read_value_origins(find_entity).expect("find carries facts");
    assert_eq!(facts.bindings.get("store"), Some(&declared("Store")));
    assert_eq!(facts.bindings.get("cache"), Some(&declared("Cache")));
    assert_eq!(facts.bindings.get("cached"), Some(&declared("Order")));
    assert_eq!(
        facts.bindings.get("fresh"),
        Some(&ValueOrigin::Call {
            callee: "OrderMapper".into()
        })
    );
    assert_eq!(
        facts.returns,
        Some(DeclaredReturn::Type {
            type_name: "Order".into()
        })
    );
    let constructor = entities
        .iter()
        .find(|entity| entity.name == "OrderService.OrderService")
        .unwrap();
    let facts = read_value_origins(constructor).expect("the constructor carries facts");
    assert_eq!(facts.bindings.get("cache"), Some(&declared("Cache")));
    assert_eq!(constructor.language, LanguageId::Java);
}

#[test]
fn java_declaration_census_pairs_admitted_nested_classes() {
    let adapter = JavaAdapter;
    let bytes = JAVA_SERVICE.as_bytes();
    let tree = adapter.parse(bytes).unwrap();
    let output = adapter
        .extract(&tree, bytes, &FilePathId::new("OrderService.java"))
        .unwrap();
    let seen = adapter.declaration_census(&tree, bytes).expect("a census");
    let census = kin_parser::declaration_census::take_census(seen, &output.entities);
    let unadmitted: Vec<_> = census
        .unadmitted
        .iter()
        .map(|declaration| (declaration.name.as_str(), declaration.reason.as_str()))
        .collect();
    assert_eq!(
        unadmitted,
        // The fields `store` and `cache` are field entities and pair.
        vec![
            ("audit", "nested_in_callable"),
            ("run", "nested_in_callable"),
        ],
        "{census:?}"
    );
}

const KOTLIN_SERVICE: &str = r#"package app.orders

import app.store.Store
import app.cache.Cache
import javax.inject.Inject

class OrderService @Inject constructor(
    private val store: Store,
    private val cache: Cache,
) : Service {
    @Inject
    lateinit var audit: AuditLog

    override fun find(id: String): Order {
        val cached = cache.get(id)
        val mapper = OrderMapper()
        val audited = { audit.record(id) }
        items.forEach { item -> mapper.map(item) }
        with(other) {
            store.flush()
        }
        val byDate = object : Comparator<Order> {
            override fun compare(a: Order, b: Order): Int {
                return a.date().compareTo(b.date())
            }
        }
        fun retry() {
            store.load(id)
        }
        executor.submit(object : Runnable {
            override fun run() {
                store.flush()
            }
        })
        return mapper.map(store.load(id))
    }
}

interface Service {
    fun find(id: String): Order
}
"#;

#[test]
fn kotlin_constructor_properties_and_nested_objects_become_entities() {
    let (output, _) = parse(
        &KotlinAdapter,
        "src/main/kotlin/app/orders/OrderService.kt",
        KOTLIN_SERVICE,
    );
    let names = names(&output);
    for expected in [
        "OrderService.store",
        "OrderService.cache",
        "OrderService.audit",
        "OrderService.find",
        "Service.find",
        "OrderService.find.<locals>.byDate",
        "OrderService.find.<locals>.byDate.compare",
    ] {
        assert!(names.contains(&expected), "missing {expected} in {names:?}");
    }
    let census = output
        .nested_definitions
        .as_ref()
        .expect("a census is taken");
    let recorded: Vec<_> = census
        .not_admitted
        .iter()
        .map(|definition| {
            (
                definition.name.as_str(),
                definition.kind,
                definition.bases.clone(),
            )
        })
        .collect();
    assert_eq!(
        recorded,
        vec![
            (
                "OrderService.find.<locals>.audited",
                EntityKind::Function,
                Vec::new()
            ),
            (
                "OrderService.find.<locals>.retry",
                EntityKind::Function,
                Vec::new()
            ),
            (
                "OrderService.find.<locals>.<anonymous>",
                EntityKind::Class,
                vec!["Runnable".to_string()]
            ),
        ]
    );
    assert!(output.relations.iter().any(|relation| {
        relation.kind == RelationKind::Implements
            && relation.src_name == "OrderService.find.<locals>.byDate"
            && relation.dst_name == "Comparator"
    }));
    let compare = calls_of(&output, "OrderService.find.<locals>.byDate.compare");
    assert!(compare.contains(&("compareTo", None)), "{compare:?}");
    assert!(!calls_of(&output, "OrderService.find")
        .iter()
        .any(|(callee, _)| *callee == "compareTo"));
}

#[test]
fn kotlin_receivers_follow_the_implicit_receiver_rule() {
    let (output, entities) = parse(
        &KotlinAdapter,
        "src/main/kotlin/app/orders/OrderService.kt",
        KOTLIN_SERVICE,
    );
    let find = calls_of(&output, "OrderService.find");
    assert!(find.contains(&("get", Some("cache"))), "{find:?}");
    assert!(
        find.contains(&("map", Some("mapper"))),
        "a local keeps typing a receiver inside a lambda: {find:?}"
    );
    assert!(
        find.contains(&("record", None)),
        "a property read inside a lambda may be an implicit receiver's member: {find:?}"
    );
    assert_eq!(
        find.iter().filter(|call| **call == ("flush", None)).count(),
        2,
        "inside `with` and inside an object expression: {find:?}"
    );
    assert!(
        find.contains(&("load", None)),
        "a local function's body reads properties only through locals: {find:?}"
    );
    assert!(find.contains(&("load", Some("store"))), "{find:?}");

    let find_entity = entities
        .iter()
        .find(|entity| entity.name == "OrderService.find")
        .unwrap();
    let facts = read_value_origins(find_entity).expect("find carries facts");
    assert_eq!(facts.bindings.get("store"), Some(&declared("Store")));
    assert_eq!(facts.bindings.get("cache"), Some(&declared("Cache")));
    assert_eq!(facts.bindings.get("audit"), Some(&declared("AuditLog")));
    assert_eq!(
        facts.bindings.get("mapper"),
        Some(&ValueOrigin::Call {
            callee: "OrderMapper".into()
        })
    );
    assert_eq!(
        facts.bindings.get("cached"),
        Some(&ValueOrigin::MethodCall {
            receiver: Box::new(ValueOrigin::Local {
                name: "cache".into()
            }),
            method: "get".into(),
        })
    );
    assert_eq!(
        facts.returns,
        Some(DeclaredReturn::Type {
            type_name: "Order".into()
        })
    );
    assert_eq!(find_entity.language, LanguageId::Kotlin);
}

#[test]
fn a_member_type_of_a_local_class_is_admitted_once_and_never_listed() {
    let (output, _) = parse(
        &JavaAdapter,
        "src/main/java/app/Service.java",
        "class Service {\n    void build() {\n        class Local {\n            class Inner {\n                void run() { go(); }\n            }\n        }\n    }\n}\n",
    );
    let admitted = names(&output);
    for expected in [
        "Service.build.<locals>.Local",
        "Service.build.<locals>.Local.Inner",
        "Service.build.<locals>.Local.Inner.run",
    ] {
        assert!(
            admitted.contains(&expected),
            "missing {expected} in {admitted:?}"
        );
    }
    assert_eq!(
        output.nested_definitions.unwrap().not_admitted,
        Vec::new(),
        "a member type is admitted by its class's walk"
    );

    let (output, _) = parse(
        &KotlinAdapter,
        "src/main/kotlin/app/Service.kt",
        "class Service {\n    fun build() {\n        class Local {\n            inner class Inner {\n                fun run() {\n                    go()\n                }\n            }\n        }\n    }\n}\n",
    );
    let admitted = names(&output);
    for expected in [
        "Service.build.<locals>.Local",
        "Service.build.<locals>.Local.Inner",
        "Service.build.<locals>.Local.Inner.run",
    ] {
        assert!(
            admitted.contains(&expected),
            "missing {expected} in {admitted:?}"
        );
    }
    assert_eq!(output.nested_definitions.unwrap().not_admitted, Vec::new());
}
