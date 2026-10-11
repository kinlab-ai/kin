// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

use kin_model::{Entity, FilePathId, LanguageId, RelationKind};

use super::*;
use crate::value_origins::{attach_value_origins, read_value_origins};
use crate::{LanguageAdapter, SwiftAdapter};

const SYNC: &str = r#"import Foundation

protocol Persisting {
    func save()
}

final class SyncService {
    let store: Store
    private var cache = Cache()
    static let shared = SyncService(store: Store())

    init(store: Store) {
        self.store = store
    }

    func flush(primary: Store, backup: Cache?, all: [Store]) {
        primary.save()
        backup?.save()
        store.save()
        self.store.save()
        cache.save()
        self.reset()
        all.forEach { item in item.save() }
    }

    func rebuild(registry: Registry) throws -> Store {
        let fresh = Store()
        fresh.save()
        let found = try registry.lookup(name: "main")
        found.save()
        let copy = found
        copy.save()
        let made = makeStore(shards: 4)
        made.save()
        let remote = connectRemote()
        remote.save()
        if let child = found.child() {
            child.save()
        }
        guard let parent = found.parent() else { return fresh }
        parent.save()
        return made
    }

    func reset() {}
    func chain() -> Self { return self }
    func current() -> Store? { return nil }
}

func makeStore(shards: Int) -> Store {
    return Store()
}
"#;

fn attached(path: &str, source: &str) -> (Vec<Entity>, crate::extract::ParseOutput) {
    let adapter = SwiftAdapter;
    let bytes = source.as_bytes();
    let tree = adapter.parse(bytes).unwrap();
    assert!(
        !tree.root_node().has_error(),
        "the fixture must parse cleanly: {}",
        tree.root_node().to_sexp()
    );
    let file_id = FilePathId(path.to_string());
    let output = adapter.extract(&tree, bytes, &file_id).unwrap();
    let mut entities: Vec<Entity> = output
        .entities
        .iter()
        .cloned()
        .map(|entity| entity.into_entity(LanguageId::Swift, &file_id))
        .collect();
    attach_value_origins(&tree, bytes, LanguageId::Swift, path, &mut entities);
    (entities, output)
}

fn facts_on(entities: &[Entity], name: &str) -> ScopeFacts {
    let entity = entities
        .iter()
        .find(|entity| entity.name == name)
        .unwrap_or_else(|| {
            panic!(
                "no entity {name} in {:?}",
                entities.iter().map(|e| &e.name).collect::<Vec<_>>()
            )
        });
    read_value_origins(entity).unwrap_or_default()
}

fn declared(type_name: &str) -> ValueOrigin {
    ValueOrigin::Declared {
        type_name: type_name.into(),
    }
}

fn calls_from(output: &crate::extract::ParseOutput, caller: &str) -> Vec<(String, Option<String>)> {
    output
        .relations
        .iter()
        .filter(|relation| relation.kind == RelationKind::Calls && relation.src_name == caller)
        .map(|relation| (relation.dst_name.clone(), relation.receiver.clone()))
        .collect()
}

#[test]
fn parameters_properties_and_initializers_are_recorded() {
    let (entities, _) = attached("Sources/Sync/SyncService.swift", SYNC);
    let flush = facts_on(&entities, "SyncService.flush");
    assert_eq!(flush.bindings.get("primary"), Some(&declared("Store")));
    assert_eq!(flush.bindings.get("backup"), Some(&declared("Cache")));
    assert_eq!(
        flush.bindings.get("all"),
        None,
        "an array names no one class"
    );
    assert_eq!(flush.bindings.get("store"), Some(&declared("Store")));
    assert_eq!(
        flush.bindings.get("cache"),
        Some(&ValueOrigin::Call {
            callee: "Cache".into()
        })
    );
    assert_eq!(
        flush.bindings.get("shared"),
        None,
        "a static property is no member of self"
    );

    let rebuild = facts_on(&entities, "SyncService.rebuild");
    assert_eq!(
        rebuild.bindings.get("registry"),
        Some(&declared("Registry"))
    );
    assert_eq!(
        rebuild.bindings.get("fresh"),
        Some(&ValueOrigin::Call {
            callee: "Store".into()
        })
    );
    assert_eq!(
        rebuild.bindings.get("found"),
        Some(&ValueOrigin::MethodCall {
            receiver: Box::new(ValueOrigin::Local {
                name: "registry".into()
            }),
            method: "lookup".into(),
        })
    );
    assert_eq!(
        rebuild.bindings.get("copy"),
        Some(&ValueOrigin::Local {
            name: "found".into()
        })
    );
    assert_eq!(
        rebuild.bindings.get("child"),
        Some(&ValueOrigin::MethodCall {
            receiver: Box::new(ValueOrigin::Local {
                name: "found".into()
            }),
            method: "child".into(),
        })
    );
    assert_eq!(
        rebuild.bindings.get("parent"),
        Some(&ValueOrigin::MethodCall {
            receiver: Box::new(ValueOrigin::Local {
                name: "found".into()
            }),
            method: "parent".into(),
        })
    );

    let init = facts_on(&entities, "SyncService.init");
    assert_eq!(init.bindings.get("store"), Some(&declared("Store")));
}

#[test]
fn declared_returns_read_types_optionals_and_self() {
    let (entities, _) = attached("Sources/Sync/SyncService.swift", SYNC);
    assert_eq!(
        facts_on(&entities, "SyncService.rebuild").returns,
        Some(DeclaredReturn::Type {
            type_name: "Store".into()
        })
    );
    assert_eq!(
        facts_on(&entities, "SyncService.current").returns,
        Some(DeclaredReturn::Type {
            type_name: "Store".into()
        })
    );
    assert_eq!(
        facts_on(&entities, "SyncService.chain").returns,
        Some(DeclaredReturn::Receiver)
    );
    assert_eq!(
        facts_on(&entities, "makeStore").returns,
        Some(DeclaredReturn::Type {
            type_name: "Store".into()
        })
    );
    assert_eq!(facts_on(&entities, "SyncService.reset").returns, None);
}

#[test]
fn a_call_carries_its_receiver_only_when_the_linker_can_follow_it() {
    let (_, output) = attached("Sources/Sync/SyncService.swift", SYNC);
    let flush = calls_from(&output, "SyncService.flush");
    for receiver in ["primary", "backup", "store", "cache"] {
        assert!(
            flush
                .iter()
                .any(|(dst, held)| dst == "save" && held.as_deref() == Some(receiver)),
            "{receiver} in {flush:?}"
        );
    }
    assert_eq!(
        flush
            .iter()
            .filter(|(dst, held)| dst == "save" && held.as_deref() == Some("store"))
            .count(),
        2,
        "`self.store` reads the same property: {flush:?}"
    );
    assert!(flush.contains(&("SyncService.reset".into(), None)));
    assert!(
        flush.contains(&("save".into(), None)),
        "the closure parameter has no declared type: {flush:?}"
    );

    let rebuild = calls_from(&output, "SyncService.rebuild");
    for receiver in ["fresh", "found", "copy", "made", "child", "parent"] {
        assert!(
            rebuild
                .iter()
                .any(|(dst, held)| dst == "save" && held.as_deref() == Some(receiver)),
            "{receiver} in {rebuild:?}"
        );
    }
    // `connectRemote` is a function in another file: the linker cannot
    // follow it, so the call keeps its bare shape and name candidates.
    assert!(rebuild.contains(&("save".into(), None)), "{rebuild:?}");
}

#[test]
fn a_name_two_scopes_bind_differently_or_read_outside_its_block_is_left_out() {
    let source = r#"
func shadowed(fast: Bool) {
    if fast {
        let handle = Store()
        handle.save()
    } else {
        let handle = Cache()
        handle.save()
    }
}

func closureParameter(handle: Store) {
    run { (handle: Cache) in handle.save() }
    handle.save()
}

func sameTypeTwice(fast: Bool) {
    if fast {
        let handle = Store()
        handle.save()
    } else {
        let handle = Store()
        handle.save()
    }
}

func unwrapped(handle: Store?) {
    if let handle = handle {
        handle.save()
    }
}
"#;
    let (entities, output) = attached("Sources/Shadow.swift", source);
    assert_eq!(facts_on(&entities, "shadowed").bindings.get("handle"), None);
    assert_eq!(
        facts_on(&entities, "closureParameter")
            .bindings
            .get("handle"),
        None
    );
    assert_eq!(
        facts_on(&entities, "sameTypeTwice").bindings.get("handle"),
        Some(&ValueOrigin::Call {
            callee: "Store".into()
        })
    );
    assert_eq!(
        facts_on(&entities, "unwrapped").bindings.get("handle"),
        Some(&declared("Store")),
        "`if let x = x` unwraps the same value"
    );
    assert!(calls_from(&output, "shadowed")
        .iter()
        .all(|(_, receiver)| receiver.is_none()));
}
