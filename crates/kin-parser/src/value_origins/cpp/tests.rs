// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

use kin_model::{Entity, EntityKind, FilePathId, RelationKind};

use super::*;
use crate::value_origins::{attach_value_origins, read_value_origins};
use crate::{CppAdapter, LanguageAdapter};

/// A small service in the header-plus-implementation shape real C++ uses.
const SERVICE: &str = r#"#include "store.h"
#include "registry.h"
#include <memory>
#include <vector>

namespace app {

class Store;

Store make_store(int shards);
std::unique_ptr<Store> open_store(const char* path);

class Service {
 public:
  explicit Service(Registry& registry) : registry_(registry) {}

  void flush(Store& primary, Cache* cache) {
    primary.save();
    cache->save();
    store_->save();
    cache_.save();
  }

  int rebuild() {
    auto fresh = make_store(4);
    fresh.save();
    auto opened = open_store("db");
    opened->save();
    auto built = std::make_unique<Store>();
    built->save();
    auto raw = new Store();
    raw->save();
    Store braced{};
    braced.save();
    auto listed = Store{};
    listed.save();
    auto found = registry_.lookup("main");
    found.save();
    auto alias = found;
    alias.save();
    auto remote = connect_remote();
    remote.save();
    this->reset();
    std::vector<Store> all;
    all.push_back(braced);
    return 0;
  }

  Service& reset();
  Store current() const;

 private:
  Registry& registry_;
  std::unique_ptr<Store> store_;
  Cache cache_;
};

Store Service::current() const { return make_store(1); }

}  // namespace app
"#;

fn attached(path: &str, source: &str) -> (Vec<Entity>, crate::extract::ParseOutput) {
    let adapter = CppAdapter;
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
        .map(|entity| entity.into_entity(kin_model::LanguageId::Cpp, &file_id))
        .collect();
    attach_value_origins(
        &tree,
        bytes,
        kin_model::LanguageId::Cpp,
        path,
        &mut entities,
    );
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
fn parameters_fields_and_explicit_constructions_are_declared() {
    let (entities, _) = attached("src/service.cpp", SERVICE);
    let flush = facts_on(&entities, "Service::flush");
    assert_eq!(flush.bindings.get("primary"), Some(&declared("Store")));
    assert_eq!(flush.bindings.get("cache"), Some(&declared("Cache")));
    // A field the method reads and never declares: a smart pointer names
    // its pointee.
    assert_eq!(flush.bindings.get("store_"), Some(&declared("Store")));
    assert_eq!(flush.bindings.get("cache_"), Some(&declared("Cache")));

    let rebuild = facts_on(&entities, "Service::rebuild");
    assert_eq!(
        rebuild.bindings.get("fresh"),
        Some(&ValueOrigin::Call {
            callee: "make_store".into()
        })
    );
    assert_eq!(
        rebuild.bindings.get("opened"),
        Some(&ValueOrigin::Call {
            callee: "open_store".into()
        })
    );
    assert_eq!(rebuild.bindings.get("built"), Some(&declared("Store")));
    assert_eq!(rebuild.bindings.get("raw"), Some(&declared("Store")));
    assert_eq!(rebuild.bindings.get("braced"), Some(&declared("Store")));
    assert_eq!(rebuild.bindings.get("listed"), Some(&declared("Store")));
    assert_eq!(
        rebuild.bindings.get("found"),
        Some(&ValueOrigin::MethodCall {
            receiver: Box::new(ValueOrigin::Local {
                name: "registry_".into()
            }),
            method: "lookup".into(),
        })
    );
    assert_eq!(
        rebuild.bindings.get("alias"),
        Some(&ValueOrigin::Local {
            name: "found".into()
        })
    );
    assert_eq!(
        rebuild.bindings.get("registry_"),
        Some(&declared("Registry"))
    );
    // A standard container is no evidence for a repository class.
    assert_eq!(rebuild.bindings.get("all"), None);
}

#[test]
fn declared_returns_read_through_pointers_references_and_trailing_types() {
    let (entities, _) = attached("src/service.cpp", SERVICE);
    assert_eq!(
        facts_on(&entities, "make_store").returns,
        Some(DeclaredReturn::Type {
            type_name: "Store".into()
        })
    );
    assert_eq!(
        facts_on(&entities, "open_store").returns,
        Some(DeclaredReturn::Type {
            type_name: "Store".into()
        }),
        "a std::unique_ptr return names its pointee"
    );
    let current: Vec<&Entity> = entities
        .iter()
        .filter(|entity| entity.name == "Service::current")
        .collect();
    assert!(current.iter().all(|entity| read_value_origins(entity)
        .and_then(|facts| facts.returns)
        == Some(DeclaredReturn::Type {
            type_name: "Store".into()
        })));

    let source = "auto build() -> Widget { return Widget{}; }\n\
                  template <class T> T make() { return T(); }\n\
                  int count() { return 0; }\n\
                  void (*handler())(int) { return nullptr; }\n\
                  Widget& Widget::chain() { return *this; }\n";
    let (entities, _) = attached("src/build.cpp", source);
    assert_eq!(
        facts_on(&entities, "build").returns,
        Some(DeclaredReturn::Type {
            type_name: "Widget".into()
        })
    );
    assert_eq!(
        facts_on(&entities, "make").returns,
        None,
        "a template parameter"
    );
    assert_eq!(facts_on(&entities, "count").returns, None, "a primitive");
    assert_eq!(
        facts_on(&entities, "handler").returns,
        None,
        "a function pointer"
    );
    assert_eq!(
        facts_on(&entities, "Widget::chain").returns,
        Some(DeclaredReturn::Type {
            type_name: "Widget".into()
        })
    );
}

#[test]
fn the_adapter_qualifies_what_the_file_settles_and_keeps_the_rest_bare() {
    let (_, output) = attached("src/service.cpp", SERVICE);
    let flush = calls_from(&output, "Service::flush");
    for expected in ["Store::save", "Cache::save"] {
        assert!(
            flush
                .iter()
                .any(|(dst, receiver)| dst == expected && receiver.is_none()),
            "{expected} in {flush:?}"
        );
    }
    assert_eq!(
        flush.iter().filter(|(dst, _)| dst == "Store::save").count(),
        2,
        "the parameter and the unique_ptr field both name Store: {flush:?}"
    );

    let rebuild = calls_from(&output, "Service::rebuild");
    // The file declares make_store and open_store, so it settles both.
    assert_eq!(
        rebuild
            .iter()
            .filter(|(dst, receiver)| dst == "Store::save" && receiver.is_none())
            .count(),
        6,
        "fresh, opened, built, raw, braced and listed: {rebuild:?}"
    );
    // Registry::lookup is declared in another file: the call keeps its bare
    // shape and the name candidates the linker offers for it.
    assert_eq!(
        rebuild
            .iter()
            .filter(|(dst, receiver)| dst == "save" && receiver.is_none())
            .count(),
        3,
        "found, alias and remote: {rebuild:?}"
    );
    assert!(rebuild.iter().any(|(dst, _)| dst == "Service::reset"));
    assert!(
        rebuild
            .iter()
            .any(|(dst, receiver)| dst == "vector::push_back" && receiver.is_none()),
        "a standard container keeps the owner-qualified spelling it always had: {rebuild:?}"
    );
}

#[test]
fn a_name_declared_twice_with_different_types_or_read_outside_its_block_is_left_out() {
    let source = r#"
void shadowed(bool fast) {
  if (fast) {
    Store handle;
    handle.save();
  } else {
    Cache handle;
    handle.save();
  }
}

void same_type_twice(bool fast) {
  if (fast) {
    Store handle;
    handle.save();
  } else {
    Store handle;
    handle.save();
  }
}

class Owner {
  Store handle;
  void outside(bool fast) {
    if (fast) {
      Cache handle;
      handle.save();
    }
    handle.save();
  }
};

void by_lambda(Store handle) {
  auto run = [](Cache handle) { handle.save(); };
  handle.save();
}

void after_use() {
  handle.save();
  Store handle;
  handle.save();
}
"#;
    let (entities, output) = attached("src/shadow.cpp", source);
    assert_eq!(facts_on(&entities, "shadowed").bindings.get("handle"), None);
    assert_eq!(
        facts_on(&entities, "same_type_twice")
            .bindings
            .get("handle"),
        Some(&declared("Store"))
    );
    assert_eq!(
        facts_on(&entities, "Owner::outside").bindings.get("handle"),
        None
    );
    assert_eq!(
        facts_on(&entities, "by_lambda").bindings.get("handle"),
        None
    );
    assert_eq!(
        facts_on(&entities, "after_use").bindings.get("handle"),
        None
    );

    let shadowed = calls_from(&output, "shadowed");
    assert!(
        shadowed
            .iter()
            .all(|(dst, receiver)| dst == "save" && receiver.is_none()),
        "no declared type is sound for either read: {shadowed:?}"
    );
    let outside = calls_from(&output, "Owner::outside");
    assert!(
        outside.iter().all(|(dst, _)| dst == "save"),
        "the field and the local disagree: {outside:?}"
    );
    assert!(calls_from(&output, "same_type_twice")
        .iter()
        .all(|(dst, _)| dst == "Store::save"));
}

#[test]
fn macros_that_can_declare_names_silence_the_names_they_touch() {
    let source = r#"
#define WITH_CACHE(name) Cache name
void guarded(Store item) {
  WITH_CACHE(item);
  item.save();
}

void looped(Store item) {
  FOREACH(item, caches) { item.save(); }
}

void clean(Store item) {
  item.save();
}
"#;
    let (entities, _) = attached("src/macros.cpp", source);
    assert_eq!(facts_on(&entities, "guarded").bindings.get("item"), None);
    assert_eq!(facts_on(&entities, "looped").bindings, BTreeMap::new());
    assert_eq!(
        facts_on(&entities, "clean").bindings.get("item"),
        Some(&declared("Store"))
    );
}

#[test]
fn a_file_the_grammar_cannot_parse_carries_no_fact() {
    let source = "void broken(Store s) { s.save( }\nvoid fine(Store s) { s.save(); }\n";
    let adapter = CppAdapter;
    let tree = adapter.parse(source.as_bytes()).unwrap();
    assert!(tree.root_node().has_error());
    let file_id = FilePathId::new("src/broken.cpp");
    let output = adapter.extract(&tree, source.as_bytes(), &file_id).unwrap();
    let mut entities: Vec<Entity> = output
        .entities
        .into_iter()
        .map(|entity| entity.into_entity(kin_model::LanguageId::Cpp, &file_id))
        .collect();
    attach_value_origins(
        &tree,
        source.as_bytes(),
        kin_model::LanguageId::Cpp,
        "src/broken.cpp",
        &mut entities,
    );
    assert!(entities
        .iter()
        .filter(|entity| entity.kind == EntityKind::Function)
        .all(|entity| read_value_origins(entity).is_none()));
}
