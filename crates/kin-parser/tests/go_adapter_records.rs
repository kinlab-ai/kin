// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The generic records the Go adapter fills, on real-shaped Go: value-origin
//! facts and the typed receivers that carry them to the linker, the
//! value-escape record, the declaration census, and definitions nested in
//! function bodies.

use kin_model::{
    Entity, EntityKind, FileEscapeRecord, FilePathId, LanguageId, OccurrenceBinding,
    OccurrenceContext, ProtocolMember, RelationKind, PROTOCOL_MEMBER_KEY,
};
use kin_parser::value_origins::{attach_value_origins, read_value_origins, ValueOrigin};
use kin_parser::{ExtractedRelation, GoAdapter, LanguageAdapter, ParseOutput};

fn parse(path: &str, source: &str) -> (tree_sitter::Tree, ParseOutput) {
    let tree = GoAdapter.parse(source.as_bytes()).expect("parse");
    let output = GoAdapter
        .extract(&tree, source.as_bytes(), &FilePathId::new(path))
        .expect("extract");
    (tree, output)
}

fn entities(path: &str, source: &str) -> Vec<Entity> {
    let (tree, output) = parse(path, source);
    let file_id = FilePathId::new(path);
    let mut entities: Vec<Entity> = output
        .entities
        .into_iter()
        .map(|entity| {
            entity.into_entity_with_source(LanguageId::Go, &file_id, Some(source.as_bytes()))
        })
        .collect();
    attach_value_origins(
        &tree,
        source.as_bytes(),
        LanguageId::Go,
        path,
        &mut entities,
    );
    let census = kin_parser::value_escape::census_file(&tree, source.as_bytes(), LanguageId::Go);
    kin_parser::value_escape::attach(census.as_ref(), &mut entities);
    entities
}

fn entity<'a>(entities: &'a [Entity], name: &str) -> &'a Entity {
    entities
        .iter()
        .find(|entity| entity.name == name)
        .unwrap_or_else(|| panic!("no entity {name}"))
}

fn calls<'a>(output: &'a ParseOutput, src: &str) -> Vec<&'a ExtractedRelation> {
    output
        .relations
        .iter()
        .filter(|relation| relation.kind == RelationKind::Calls && relation.src_name == src)
        .collect()
}

fn call<'a>(output: &'a ParseOutput, src: &str, dst: &str) -> &'a ExtractedRelation {
    calls(output, src)
        .into_iter()
        .find(|relation| relation.dst_name == dst)
        .unwrap_or_else(|| panic!("no call {src} -> {dst}: {:?}", output.relations))
}

/// A store package the way Go code is usually laid out: a struct, a
/// constructor, an interface it satisfies, and a server holding it in a
/// field, with a same-named method on a second type.
const STORE: &str = r#"package store

import (
	"context"
	"errors"
)

// Loader loads a value by key.
type Loader interface {
	Load(ctx context.Context, key string) (string, error)
}

// Store keeps values in memory.
type Store struct {
	values map[string]string
}

// Cache is a second type with a method of the same name.
type Cache struct{}

func (c *Cache) Load(ctx context.Context, key string) (string, error) {
	return "", nil
}

// NewStore builds an empty store.
func NewStore() *Store {
	return &Store{values: map[string]string{}}
}

func (s *Store) Load(ctx context.Context, key string) (string, error) {
	value, ok := s.values[key]
	if !ok {
		return "", errors.New("missing")
	}
	return value, nil
}

// Server serves values from its store and its loader.
type Server struct {
	store  *Store
	loader Loader
	cache  Cache
}

func (srv *Server) Get(ctx context.Context, key string) string {
	value, _ := srv.store.Load(ctx, key)
	fallback, _ := srv.loader.Load(ctx, key)
	srv.cache.Load(ctx, key)
	return value + fallback
}

func warm(ctx context.Context, keys []string) {
	s := NewStore()
	var fallback Loader
	for _, key := range keys {
		s.Load(ctx, key)
		fallback.Load(ctx, key)
	}
	for _, c := range []*Cache{} {
		c.Load(ctx, "")
	}
}
"#;

#[test]
fn a_typed_local_carries_its_receiver_and_facts_type_it() {
    let (_, output) = parse("store/store.go", STORE);
    let warm = calls(&output, "warm");
    let loads: Vec<(Option<&str>, &str)> = warm
        .iter()
        .filter(|relation| relation.dst_name == "Load")
        .map(|relation| (relation.receiver.as_deref(), relation.dst_name.as_str()))
        .collect();
    assert_eq!(
        loads,
        vec![
            (Some("s"), "Load"),
            (Some("fallback"), "Load"),
            (None, "Load")
        ],
        "the range variable `c` has no fact, so its call keeps the shape it always had"
    );

    let entities = entities("store/store.go", STORE);
    let facts = read_value_origins(entity(&entities, "warm")).expect("facts on warm");
    assert_eq!(
        facts.bindings.get("s"),
        Some(&ValueOrigin::Call {
            callee: "NewStore".into()
        })
    );
    assert_eq!(
        facts.bindings.get("fallback"),
        Some(&ValueOrigin::Declared {
            type_name: "Loader".into()
        }),
        "an interface-typed local is typed as the interface"
    );
    assert!(
        !facts.bindings.contains_key("ctx"),
        "context.Context is standard"
    );
    let returns = read_value_origins(entity(&entities, "NewStore")).expect("facts on NewStore");
    assert_eq!(
        returns.returns,
        Some(kin_parser::value_origins::DeclaredReturn::Type {
            type_name: "Store".into()
        })
    );
    let method = read_value_origins(entity(&entities, "Server.Get")).expect("facts on Get");
    assert_eq!(
        method.bindings.get("srv"),
        Some(&ValueOrigin::Declared {
            type_name: "Server".into()
        }),
        "a method's receiver is typed by its declaration"
    );
}

#[test]
fn a_field_chain_written_in_the_file_names_the_field_type() {
    let (_, output) = parse("store/store.go", STORE);
    let store = call(&output, "Server.Get", "Store.Load");
    assert_eq!(store.receiver.as_deref(), Some("srv.store"));
    let loader = call(&output, "Server.Get", "Loader.Load");
    assert_eq!(
        loader.receiver.as_deref(),
        Some("srv.loader"),
        "an interface-typed field binds the interface method"
    );
    let cache = call(&output, "Server.Get", "Cache.Load");
    assert_eq!(cache.receiver.as_deref(), Some("srv.cache"));
    assert!(
        calls(&output, "Server.Get")
            .iter()
            .all(|relation| relation.dst_name != "Load"),
        "no call through a typed field is left bare: {:?}",
        calls(&output, "Server.Get")
    );
}

#[test]
fn a_chain_through_a_type_or_method_written_elsewhere_keeps_its_old_shape() {
    let source = r#"package api

import "example.com/app/store"

type Handler struct {
	remote *store.Store
	local  *Local
	Embedded
}

type Local struct{}

type Embedded struct {
	inner *Local
}

func (h *Handler) Serve() {
	h.remote.Load()
	h.local.Missing()
	h.inner.Load()
}
"#;
    let (_, output) = parse("api/handler.go", source);
    let serve = calls(&output, "Handler.Serve");
    let shapes: Vec<(Option<&str>, &str)> = serve
        .iter()
        .map(|relation| (relation.receiver.as_deref(), relation.dst_name.as_str()))
        .collect();
    assert_eq!(
        shapes,
        vec![(None, "Load"), (None, "Missing"), (None, "Load")],
        "a qualified field type, a method this file does not declare, and a promoted field are left alone"
    );
}

#[test]
fn methods_and_entry_points_carry_protocol_marks_and_the_record_lists_value_uses() {
    let source = r#"package store

import "net/http"

type Store struct{}

func (s *Store) ServeHTTP(w http.ResponseWriter, r *http.Request) {}

func (s *Store) handle(w http.ResponseWriter, r *http.Request) {}

func routes(mux *http.ServeMux, s *Store) {
	mux.HandleFunc("/load", s.handle)
	mux.HandleFunc("/health", health)
	register(defaults)
}

func health(w http.ResponseWriter, r *http.Request) {}

func init() {}
"#;
    let entities = entities("store/server.go", source);
    let marked = |name: &str| {
        ProtocolMember::from_entity(entity(&entities, name)).map(|member| match member {
            ProtocolMember::Implicit(through) => through,
            ProtocolMember::Listed(_) => panic!("go marks are implicit"),
        })
    };
    assert!(marked("Store.ServeHTTP").is_some_and(|through| through.contains("interface")));
    assert!(marked("Store.handle").is_some());
    assert_eq!(marked("init").as_deref(), Some("package initialization"));
    assert!(!entity(&entities, "health")
        .metadata
        .extra
        .contains_key(PROTOCOL_MEMBER_KEY));

    let record = entities
        .iter()
        .find_map(FileEscapeRecord::from_entity)
        .expect("the file carries a record");
    assert!(
        entities
            .iter()
            .filter(|entity| entity.kind == EntityKind::Module)
            .any(|entity| FileEscapeRecord::from_entity(entity).is_some()),
        "the file's module surface carries the record"
    );
    let handle = &record.values["handle"];
    assert_eq!(handle[0].context, OccurrenceContext::Argument);
    assert_eq!(handle[0].binding, OccurrenceBinding::Member);
    let health = &record.values["health"];
    assert_eq!(health[0].binding, OccurrenceBinding::Module);
    assert_eq!(
        record.values["defaults"][0].binding,
        OccurrenceBinding::Free
    );
    assert_eq!(record.exports, vec!["health", "routes"]);
}

#[test]
fn local_types_become_entities_and_named_literals_are_recorded() {
    let source = r#"package store

func TestLoad(t *testing.T) {
	type testCase struct {
		name string
		want string
	}
	type closer interface {
		Close() error
	}
	check := func(tc testCase) {
		type result struct{ got string }
		_ = result{}
	}
	var walk func(n int)
	walk = func(n int) {
		if n > 0 {
			walk(n - 1)
		}
	}
	run(func() {
		type testCase struct{}
	})
	_ = check
}

var hook = func() {
	type local struct{}
}
"#;
    let (_, output) = parse("store/store_test.go", source);
    let kind = |name: &str| {
        output
            .entities
            .iter()
            .find(|entity| entity.name == name)
            .map(|entity| entity.kind)
    };
    assert_eq!(kind("TestLoad.<locals>.testCase"), Some(EntityKind::Class));
    assert_eq!(
        kind("TestLoad.<locals>.testCase.name"),
        Some(EntityKind::Field)
    );
    assert_eq!(
        kind("TestLoad.<locals>.closer"),
        Some(EntityKind::Interface)
    );
    assert_eq!(
        kind("TestLoad.<locals>.closer.Close"),
        Some(EntityKind::Method)
    );
    assert_eq!(
        kind("TestLoad.<locals>.check.<locals>.result"),
        Some(EntityKind::Class),
        "a named literal is a scope in the qualified name"
    );
    assert!(output.relations.iter().any(|relation| {
        relation.kind == RelationKind::Contains
            && relation.src_name == "TestLoad"
            && relation.dst_name == "TestLoad.<locals>.testCase"
    }));
    assert!(
        output
            .entities
            .iter()
            .filter(|entity| entity.name.contains("<locals>"))
            .all(|entity| entity.visibility == kin_model::Visibility::Private),
        "nothing outside the function names a local type"
    );
    let census = output
        .nested_definitions
        .clone()
        .expect("go takes a census");
    let recorded: Vec<(&str, EntityKind, Option<&str>)> = census
        .not_admitted
        .iter()
        .map(|d| (d.name.as_str(), d.kind, d.enclosing.as_deref()))
        .collect();
    assert_eq!(
        recorded,
        vec![
            ("TestLoad.<locals>.check", EntityKind::Function, Some("TestLoad")),
            ("TestLoad.<locals>.walk", EntityKind::Function, Some("TestLoad")),
            (
                "TestLoad.<locals>.testCase",
                EntityKind::Class,
                Some("TestLoad")
            ),
            ("hook.<locals>.local", EntityKind::Class, Some("hook")),
        ],
        "an anonymous literal's second testCase collides and a variable's literal is no function body"
    );
    // The calls in a named literal stay with the function around it.
    assert!(calls(&output, "TestLoad")
        .iter()
        .any(|relation| relation.dst_name == "walk"));
}

#[test]
fn the_declaration_census_pairs_admitted_nested_types() {
    let source = "package p\n\nfunc Run() {\n\ttype row struct{ id int }\n\thelper := func() {}\n\t_ = helper\n}\n";
    let tree = GoAdapter.parse(source.as_bytes()).unwrap();
    let mut output = GoAdapter
        .extract(&tree, source.as_bytes(), &FilePathId::new("p/run.go"))
        .unwrap();
    kin_parser::file_evidence::attach_file_evidence(
        &GoAdapter,
        &tree,
        source.as_bytes(),
        &mut output,
    );
    let census = kin_parser::file_evidence::declaration_census_of(&output.relations)
        .expect("go takes a declaration census");
    assert_eq!(census.seen, 4, "{census:?}");
    assert_eq!(census.admitted, 3, "Run, row and row.id: {census:?}");
    assert_eq!(census.unadmitted.len(), 1);
    assert_eq!(census.unadmitted[0].name, "helper");
}

#[test]
fn a_chain_another_file_of_the_package_can_finish_carries_its_receiver() {
    let source = r#"package store

type IP []byte

func (ip IP) To4() IP { return ip }

type Pool struct{}

func (p *Pool) Get() {}

func use(ip IP, pool *Pool, other *Remote) {
	s := NewStoreFromSiblingFile()
	s.Load()
	ip.To4()
	pool.Put()
	pool.Get()
	other.Load()
}
"#;
    let (_, output) = parse("store/use.go", source);
    let shapes: Vec<(&str, Option<&str>)> = calls(&output, "use")
        .iter()
        .filter(|relation| relation.dst_name != "NewStoreFromSiblingFile")
        .map(|relation| (relation.dst_name.as_str(), relation.receiver.as_deref()))
        .collect();
    assert_eq!(
        shapes,
        vec![
            ("Load", Some("s")),
            ("To4", Some("ip")),
            ("Put", Some("pool")),
            ("Get", Some("pool")),
            ("Load", Some("other")),
        ],
        "the linker finds a constructor, a method and a type in any file of the \
         package, and a non-struct named type carries methods, so every typed \
         local carries its receiver"
    );
}
