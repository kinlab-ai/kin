// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

use kin_model::{Entity, FilePathId, LanguageId, PROTOCOL_MEMBER_KEY};

use super::*;
use crate::{CAdapter, CppAdapter, LanguageAdapter};

fn c_census(source: &str) -> FileEscapeCensus {
    let tree = CAdapter.parse(source.as_bytes()).unwrap();
    crate::value_escape::census_file(&tree, source.as_bytes(), LanguageId::C).unwrap()
}

fn cpp_census(source: &str) -> FileEscapeCensus {
    let tree = CppAdapter.parse(source.as_bytes()).unwrap();
    crate::value_escape::census_file(&tree, source.as_bytes(), LanguageId::Cpp).unwrap()
}

/// (1-based line, context, binding, aliases, receiver, target line) of every
/// use of `name`.
#[allow(clippy::type_complexity)]
fn uses(
    census: &FileEscapeCensus,
    name: &str,
) -> Vec<(
    u32,
    OccurrenceContext,
    OccurrenceBinding,
    Vec<String>,
    Option<String>,
    Option<u32>,
)> {
    census
        .record
        .values
        .get(name)
        .map(|occurrences| {
            occurrences
                .iter()
                .map(|occurrence| {
                    (
                        occurrence.line + 1,
                        occurrence.context,
                        occurrence.binding,
                        occurrence.aliases.clone(),
                        occurrence.receiver.clone(),
                        occurrence.target_line.map(|line| line + 1),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

const LOOP_C: &str = r#"#include <uv.h>
#include "store.h"

#define DEFAULT_HANDLER on_timeout
#define LOG_CALL(x) log_write(x, LOG_LEVEL)

static void on_timeout(uv_timer_t *timer) { (void)timer; }
static void on_close(uv_handle_t *handle) { free(handle); }
void store_save(struct store *s);

static const struct store_ops default_ops = {
    .save = store_save,
    .load = &store_load,
    store_free,
};

int start(uv_loop_t *loop, struct store *s, const char *name) {
  uv_timer_t *timer = malloc(sizeof(*timer));
  uv_timer_start(timer, on_timeout, 10, 0);
  s->ops->save(s);
  close_cb f = on_close;
  s->hook = helper;
  if (on_timeout == NULL) { return -1; }
  void *sym = dlsym(loop->lib, "store_save");
  void *dyn = dlsym(loop->lib, name);
  (void)f; (void)sym; (void)dyn;
  return (int)sizeof(on_timeout);
}
"#;

#[test]
fn a_c_callback_table_and_callbacks_are_recorded_with_their_bindings() {
    let census = c_census(LOOP_C);
    assert_eq!(census.record.scope, ModuleScope::Closed);
    // A static function the file defines is read as that definition.
    assert_eq!(
        uses(&census, "on_timeout"),
        vec![(
            19,
            OccurrenceContext::Argument,
            OccurrenceBinding::Module,
            vec![],
            None,
            Some(7)
        )],
        "the comparison and sizeof are inert"
    );
    assert_eq!(
        uses(&census, "on_close"),
        vec![(
            21,
            OccurrenceContext::Assignment,
            OccurrenceBinding::Module,
            vec![],
            None,
            Some(8)
        )]
    );
    // A designated initializer aliases the function under the member name,
    // so a call through `ops->save` is followed; a public function may be
    // defined in another file, so its binding is free.
    let saves = uses(&census, "store_save");
    assert!(saves.contains(&(
        12,
        OccurrenceContext::Alias,
        OccurrenceBinding::Free,
        vec!["save".into()],
        None,
        None
    )));
    assert!(
        saves.contains(&(
            24,
            OccurrenceContext::Argument,
            OccurrenceBinding::Free,
            vec![],
            None,
            None
        )),
        "a literal dlsym names the symbol: {saves:?}"
    );
    assert_eq!(
        uses(&census, "store_load"),
        vec![(
            13,
            OccurrenceContext::Alias,
            OccurrenceBinding::Free,
            vec!["load".into()],
            None,
            None
        )]
    );
    assert_eq!(
        uses(&census, "store_free")[0].1,
        OccurrenceContext::Collection
    );
    assert_eq!(
        uses(&census, "helper"),
        vec![(
            22,
            OccurrenceContext::Alias,
            OccurrenceBinding::Free,
            vec!["hook".into()],
            None,
            None
        )]
    );
    // Locals and parameters are never recorded, and a call is no value use.
    for local in ["timer", "s", "loop", "f", "name", "sym", "dyn"] {
        assert!(uses(&census, local).is_empty(), "{local}");
    }
    assert!(
        census.record.values.get("save").is_none_or(|uses| uses
            .iter()
            .all(|use_| use_.context != OccurrenceContext::Argument)),
        "`s->ops->save(s)` is a call"
    );
    assert_eq!(census.record.reflective.len(), 1);
    assert_eq!(
        census.record.reflective[0].form,
        ReflectiveForm::ComputedMember
    );
    assert_eq!(census.record.reflective[0].reach, ReflectiveReach::Any);
    assert_eq!(census.record.reflective[0].line + 1, 25);
    // Macro bodies are text the grammar never parses: every name they spell
    // is listed, parameters aside.
    let regions: Vec<(u32, Vec<String>)> = census
        .record
        .unparsed
        .iter()
        .map(|region| (region.first + 1, region.names.clone()))
        .collect();
    assert_eq!(
        regions,
        vec![
            (4, vec!["on_timeout".to_string()]),
            (5, vec!["LOG_LEVEL".to_string(), "log_write".to_string()]),
        ]
    );
}

const SESSION_CPP: &str = r#"#include "session.hpp"
#include <functional>
#include <thread>

namespace net {

class Session : public Connection {
 public:
  Session();
  ~Session();

  void start() {
    timer_.async_wait(std::bind(&Session::on_timer, this));
    worker_ = std::thread(run_worker, 3);
    callbacks_.push_back(on_event);
    auto done = [this](int code) { finish(code); };
    submit(done);
    handler_ = &Session::on_timer;
    std::function<void()> fallback = helper;
    events_.emplace(this->on_close);
    fallback();
  }

  void on_timer() override {}
  virtual void on_close() {}
  static void on_event(int code) {}
  void finish(int code);

 private:
  std::function<void()> handler_;
  Timer timer_;
};

void Session::finish(int code) { notify(on_done); }

namespace {
void hidden_cb() {}
}  // namespace

void wire(Session& session) {
  register_cb(hidden_cb);
  register_cb(net::helper);
}

}  // namespace net
"#;

#[test]
fn a_cpp_member_function_pointer_bind_and_thread_are_recorded() {
    let census = cpp_census(SESSION_CPP);
    let timers = uses(&census, "on_timer");
    assert!(timers.contains(&(
        13,
        OccurrenceContext::Argument,
        OccurrenceBinding::Member,
        vec![],
        Some("Session".into()),
        None
    )));
    assert!(
        timers.contains(&(
            18,
            OccurrenceContext::Alias,
            OccurrenceBinding::Member,
            vec!["handler_".into()],
            Some("Session".into()),
            None
        )),
        "{timers:?}"
    );
    assert_eq!(
        uses(&census, "run_worker"),
        vec![(
            14,
            OccurrenceContext::Argument,
            OccurrenceBinding::Member,
            vec![],
            Some("Session".into()),
            None
        )],
        "inside a member function an undeclared name may be a base member or a namespace name"
    );
    assert_eq!(
        uses(&census, "on_event"),
        vec![(
            15,
            OccurrenceContext::Argument,
            OccurrenceBinding::Class,
            vec![],
            None,
            Some(26)
        )],
        "the class body declares on_event once"
    );
    assert_eq!(
        uses(&census, "done"),
        vec![(
            17,
            OccurrenceContext::Argument,
            OccurrenceBinding::Local,
            vec![],
            None,
            Some(16)
        )],
        "a lambda bound once to a local is a nested definition"
    );
    assert_eq!(uses(&census, "helper")[0].1, OccurrenceContext::Assignment);
    let closes = uses(&census, "on_close");
    assert_eq!(closes.len(), 1);
    assert_eq!(closes[0].2, OccurrenceBinding::Member);
    assert_eq!(
        census.record.values["on_close"][0].receiver_kind,
        Some(ReceiverKind::Instance)
    );
    assert_eq!(
        uses(&census, "on_done"),
        vec![(
            34,
            OccurrenceContext::Argument,
            OccurrenceBinding::Member,
            vec![],
            Some("Session".into()),
            None
        )],
        "an out-of-line member reads its class's names too"
    );
    assert_eq!(
        uses(&census, "hidden_cb"),
        vec![(
            41,
            OccurrenceContext::Argument,
            OccurrenceBinding::Module,
            vec![],
            None,
            Some(37)
        )],
        "an anonymous namespace gives internal linkage"
    );
    assert!(uses(&census, "helper").iter().any(|use_| use_.0 == 42
        && use_.2 == OccurrenceBinding::Member
        && use_.4.as_deref() == Some("net")));
    for local in ["code", "session", "fallback"] {
        assert!(uses(&census, local).is_empty(), "{local}");
    }
    assert!(
        uses(&census, "bind").is_empty() && uses(&census, "thread").is_empty(),
        "standard library names are callees or types, never a repository value"
    );
}

#[test]
fn constructors_destructors_and_virtual_members_are_stamped_on_their_entities() {
    let source = SESSION_CPP.as_bytes();
    let adapter = CppAdapter;
    let tree = adapter.parse(source).unwrap();
    let file_id = FilePathId::new("src/session.cpp");
    let output = adapter.extract(&tree, source, &file_id).unwrap();
    let mut entities: Vec<Entity> = output
        .entities
        .into_iter()
        .map(|entity| entity.into_entity(LanguageId::Cpp, &file_id))
        .collect();
    let census = crate::value_escape::census_file(&tree, source, LanguageId::Cpp);
    crate::value_escape::attach(census.as_ref(), &mut entities);
    let stamped: Vec<(String, String)> = entities
        .iter()
        .filter_map(|entity| {
            let member = kin_model::ProtocolMember::from_entity(entity)?;
            let kin_model::ProtocolMember::Implicit(words) = member else {
                return None;
            };
            Some((entity.name.clone(), words))
        })
        .collect();
    let names: Vec<&str> = stamped.iter().map(|(name, _)| name.as_str()).collect();
    for expected in [
        "Session::Session",
        "Session::~Session",
        "Session::on_timer",
        "Session::on_close",
    ] {
        assert!(names.contains(&expected), "{expected} in {stamped:?}");
    }
    assert!(
        !names.contains(&"Session::start") && !names.contains(&"Session::on_event"),
        "{stamped:?}"
    );
    assert!(entities.iter().any(|entity| entity
        .metadata
        .extra
        .contains_key(kin_model::VALUE_ESCAPE_RECORD_KEY)));
    assert!(!entities
        .iter()
        .filter(|entity| entity.name == "Session::start")
        .any(|entity| entity.metadata.extra.contains_key(PROTOCOL_MEMBER_KEY)));
}

#[test]
fn a_label_handed_to_a_macro_is_a_local() {
    let census =
        c_census("int f(void) {\n  CHECK(ready(), fail);\n  return 0;\nfail:\n  return -1;\n}\n");
    assert!(
        uses(&census, "fail").is_empty(),
        "{:?}",
        census.record.values
    );
}

#[test]
fn declared_names_are_not_reads() {
    let census = cpp_census(
        "class Session {\n public:\n  ~Session();\n  enum Mode { kFast };\n};\nSession::~Session() {}\n",
    );
    for name in ["Session", "kFast"] {
        assert!(
            uses(&census, name).is_empty(),
            "{name}: {:?}",
            census.record.values
        );
    }
}

#[test]
fn a_file_holding_only_a_function_table_still_carries_its_record() {
    let source = "#include \"ops.h\"\nstruct ops table = { .save = store_save };\n";
    let tree = CAdapter.parse(source.as_bytes()).unwrap();
    let file_id = FilePathId::new("src/table.c");
    let output = CAdapter
        .extract(&tree, source.as_bytes(), &file_id)
        .unwrap();
    assert!(output.entities.is_empty(), "{:?}", output.entities);
    let census = crate::value_escape::census_file(&tree, source.as_bytes(), LanguageId::C).unwrap();
    assert!(!census.record.is_empty());
    let surface =
        crate::value_escape::module_surface(LanguageId::C, &file_id, &tree, source.as_bytes())
            .expect("a carrier for the record");
    let entity = surface.into_entity(LanguageId::C, &file_id);
    assert!(kin_model::is_file_module_surface(&entity));
}
