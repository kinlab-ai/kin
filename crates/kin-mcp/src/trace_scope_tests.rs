// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

use super::*;
use crate::call_sites::fixture::{admit, ledger, proof_context, spanned_entity};
use crate::types::ContentBlock;
use kin_db::InMemoryGraph;
use kin_model::graph::EntityStore as _;
use kin_model::relation::{Relation, RelationEvidence, RelationOrigin};
use kin_model::{
    CallSiteState, GraphNodeId, LanguageId, RelationId, RelationKind, ResolutionRecord, SourceSpan,
};

/// A Python repository spread over several modules, every call proven by a
/// ledger so a closure over it can close.
struct Repo {
    store: InMemoryGraph,
    bodies: HashMap<EntityId, (usize, String)>,
    entities: Vec<Entity>,
    /// Each caller's call tokens and targets, in body order.
    sites: HashMap<EntityId, Vec<(String, EntityId)>>,
    byte: usize,
}

impl Repo {
    fn new() -> Self {
        Self {
            store: InMemoryGraph::new(),
            bodies: HashMap::new(),
            entities: Vec::new(),
            sites: HashMap::new(),
            byte: 0,
        }
    }

    fn define(&mut self, name: &str, kind: EntityKind, file: &str, body: &str) -> Entity {
        let mut entity = spanned_entity(name, file, LanguageId::Python, self.byte, body);
        entity.kind = kind;
        self.bodies.insert(entity.id, (self.byte, body.to_string()));
        self.byte += body.len() + 1;
        self.store.upsert_entity(&entity).unwrap();
        self.entities.push(entity.clone());
        entity
    }

    /// A file's module surface, wider than anything defined in the file.
    fn module(&mut self, name: &str, file: &str) -> Entity {
        self.define(
            name,
            EntityKind::Module,
            file,
            &format!("# {name}\n{}", " ".repeat(64)),
        )
    }

    /// A function in `file` whose body calls each of `calls`, in order.
    fn function(&mut self, name: &str, file: &str, calls: &[&str]) -> Entity {
        let mut body = format!("def {name}():\n");
        for call in calls {
            body.push_str(&format!("    {call}()\n"));
        }
        if calls.is_empty() {
            body.push_str("    return 1\n");
        }
        self.define(name, EntityKind::Function, file, &body)
    }

    fn test(&mut self, name: &str, file: &str, calls: &[&str]) -> Entity {
        let mut entity = self.function(name, file, calls);
        entity.role = EntityRole::Test;
        self.store.upsert_entity(&entity).unwrap();
        let at = self
            .entities
            .iter()
            .position(|e| e.id == entity.id)
            .unwrap();
        self.entities[at] = entity.clone();
        entity
    }

    fn relate(&mut self, from: &Entity, to: &Entity, kind: RelationKind) {
        let (start, body) = &self.bodies[&from.id];
        let first = body.find('\n').unwrap_or(0);
        let offset = body[first..]
            .find(&format!("{}(", to.name))
            .map(|at| first + at)
            .unwrap_or(0);
        let line = 10 + body[..offset].matches('\n').count() as u32;
        let file = from.file_origin.clone().unwrap();
        let relation = Relation {
            id: RelationId::new(),
            kind,
            src: GraphNodeId::Entity(from.id),
            dst: GraphNodeId::Entity(to.id),
            confidence: 1.0,
            origin: RelationOrigin::Parsed,
            created_in: None,
            import_source: None,
            evidence: vec![RelationEvidence {
                source_span: Some(SourceSpan {
                    file,
                    start_byte: start + offset,
                    end_byte: start + offset + to.name.len(),
                    start_line: line,
                    start_col: 0,
                    end_line: line,
                    end_col: 0,
                }),
                ..RelationEvidence::default()
            }],
        };
        self.store.upsert_relation(&relation).unwrap();
        if kind == RelationKind::Calls {
            self.sites
                .entry(from.id)
                .or_default()
                .push((to.name.clone(), to.id));
        }
    }

    fn calls(&mut self, from: &Entity, to: &Entity) {
        self.relate(from, to, RelationKind::Calls);
    }

    /// Admit every entity's ledger, each recorded call proven to its target.
    fn admit(&self) {
        let context = proof_context(LanguageId::Python, "current");
        let id = context.id();
        let mut records: Vec<ResolutionRecord> = vec![context];
        for entity in &self.entities {
            let body = &self.bodies[&entity.id].1;
            let sites = self
                .sites
                .get(&entity.id)
                .map(|sites| {
                    sites
                        .iter()
                        .map(|(token, target)| {
                            (
                                token.as_str(),
                                CallSiteState::ProvenTarget { target: *target },
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            records.push(ledger(entity, body, id, sites));
        }
        let entities: Vec<&Entity> = self.entities.iter().collect();
        admit(&self.store, &entities, records);
    }
}

/// `pkg` holds `core`, `ui` and the subpackage `sub`; `other` holds `bridge`;
/// `tests/test_core.py` sits in no package. `target` is called:
///
/// - inside `pkg`: by `direct`, by `top` through `direct`, and by `deep` (in
///   `pkg.sub`) through `top`;
/// - from `pkg` by a path that leaves it: `leaves` calls `bridge` in
///   `other.bridge`, which calls `target`;
/// - from outside: by `outsider`, and by the tests `test_target` and
///   `test_helper`;
/// - around a cycle inside `pkg`: `cyc_a` calls `target` and `cyc_b`, and
///   `cyc_b` calls `cyc_a`.
struct Layout {
    repo: Repo,
    target: Entity,
    direct: Entity,
    top: Entity,
    deep: Entity,
    leaves: Entity,
    bridge: Entity,
}

fn layout() -> Layout {
    let mut repo = Repo::new();
    repo.module("pkg", "src/pkg/__init__.py");
    repo.module("core", "src/pkg/core.py");
    repo.module("ui", "src/pkg/ui.py");
    repo.module("sub", "src/pkg/sub/__init__.py");
    repo.module("leaf", "src/pkg/sub/leaf.py");
    repo.module("other", "src/other/__init__.py");
    repo.module("bridge_mod", "src/other/bridge_mod.py");
    repo.module("test_core", "tests/test_core.py");

    let target = repo.function("target", "src/pkg/core.py", &[]);
    let direct = repo.function("direct", "src/pkg/core.py", &["target"]);
    let cyc_a = repo.function("cyc_a", "src/pkg/core.py", &["target", "cyc_b"]);
    let top = repo.function("top", "src/pkg/ui.py", &["direct"]);
    let cyc_b = repo.function("cyc_b", "src/pkg/ui.py", &["cyc_a"]);
    let leaves = repo.function("leaves", "src/pkg/ui.py", &["bridge"]);
    let deep = repo.function("deep", "src/pkg/sub/leaf.py", &["top"]);
    let bridge = repo.function("bridge", "src/other/bridge_mod.py", &["target"]);
    let outsider = repo.function("outsider", "src/other/bridge_mod.py", &["target"]);
    let test_target = repo.test("test_target", "tests/test_core.py", &["target"]);
    let test_helper = repo.test("test_helper", "tests/test_core.py", &["test_target"]);

    repo.calls(&direct, &target);
    repo.calls(&cyc_a, &target);
    repo.calls(&cyc_a, &cyc_b);
    repo.calls(&top, &direct);
    repo.calls(&cyc_b, &cyc_a);
    repo.calls(&leaves, &bridge);
    repo.calls(&deep, &top);
    repo.calls(&bridge, &target);
    repo.calls(&outsider, &target);
    repo.calls(&test_target, &target);
    repo.calls(&test_helper, &test_target);
    repo.admit();
    Layout {
        repo,
        target,
        direct,
        top,
        deep,
        leaves,
        bridge,
    }
}

fn call(repo: &Repo, focal: &Entity, arguments: &[(&str, Value)]) -> crate::types::ToolCallResult {
    let mut args = HashMap::from([("focal".to_string(), json!(focal.id.to_string()))]);
    for (key, value) in arguments {
        args.insert((*key).to_string(), value.clone());
    }
    crate::handlers::entities::handle_trace_data_flow_unpaged(&args, &repo.store)
        .expect("the walk answers")
}

fn trace(repo: &Repo, focal: &Entity, arguments: &[(&str, Value)]) -> Value {
    let result = call(repo, focal, arguments);
    let ContentBlock::Text { text } = &result.content[0];
    assert_ne!(result.is_error, Some(true), "{text}");
    serde_json::from_str(text).unwrap()
}

fn refusal(repo: &Repo, focal: &Entity, arguments: &[(&str, Value)]) -> String {
    let result = call(repo, focal, arguments);
    let ContentBlock::Text { text } = &result.content[0];
    assert_eq!(result.is_error, Some(true), "{text}");
    text.clone()
}

fn names(rows: &Value) -> Vec<String> {
    let mut names: Vec<String> = rows
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["name"].as_str().unwrap().to_string())
        .collect();
    names.sort();
    names
}

fn chain_names(payload: &Value) -> Vec<String> {
    let mut names: Vec<String> = payload["chain"]
        .as_array()
        .unwrap()
        .iter()
        .map(|step| step["entity_name"].as_str().unwrap().to_string())
        .collect();
    names.sort();
    names
}

fn callers(scope: &str) -> Vec<(&'static str, Value)> {
    vec![
        ("direction", json!("callers")),
        ("scope", json!(scope.to_string())),
    ]
}

/// The reported shape: `leaves` is in the package and reaches `target` only
/// through `bridge`, outside it. A question that confines paths to the package
/// must not count it, and filtering the rows of a whole-graph walk would.
#[test]
fn a_path_that_leaves_the_scope_and_comes_back_does_not_count() {
    let l = layout();
    let payload = trace(&l.repo, &l.target, &callers("pkg"));
    let reach = &payload["reach"];
    assert_eq!(
        names(&reach["members"]),
        vec!["cyc_a", "cyc_b", "deep", "direct", "top"],
        "{reach:#}"
    );
    assert_eq!(reach["state"], "closed", "{reach:#}");
    let statement = reach["statement"].as_str().unwrap();
    assert!(
        statement.contains(
            "every one in pkg that can end up calling target on a path that stays in pkg"
        ),
        "{statement}"
    );
    assert_eq!(
        reach["question"],
        "which entities in pkg can end up calling the focal on a path that stays in pkg"
    );
    // Each member's witness hop: its `via` and the call that proves it.
    let deep = reach["members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == "deep")
        .unwrap();
    assert_eq!(deep["via"], l.top.id.to_string());
    assert_eq!(deep["distance"], 3);
    assert!(deep["line"].as_u64().is_some(), "{deep:#}");

    // The out-of-scope callers met and not followed, counted and sampled.
    assert_eq!(reach["scope"]["mode"], "paths");
    assert_eq!(
        reach["scope"]["outside"], 3,
        "bridge, outsider, test_target"
    );
    let scope = &payload["scope"];
    assert_eq!(scope["module"], "pkg");
    assert_eq!(scope["mode"], "paths");
    assert_eq!(scope["answer"], "reach");
    assert_eq!(scope["state"], "complete", "{scope:#}");
    assert_eq!(scope["focal_in_scope"], true);
    assert!(scope["outside"].as_u64().unwrap() >= 3, "{scope:#}");
    assert!(scope["outside_sample"]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["name"] == "bridge" && row["module"] == "other.bridge_mod"));

    // The chain is held to the scope during the walk too: `leaves` is never a
    // step, though it is two calls from the focal.
    let chain = chain_names(&payload);
    assert!(!chain.contains(&"leaves".to_string()), "{chain:?}");
    assert!(!chain.contains(&"bridge".to_string()), "{chain:?}");
    assert!(chain.contains(&"direct".to_string()), "{chain:?}");
}

/// Under `rows` the walk is whole: `leaves` counts, and the out-of-scope
/// entity its witness path runs through is listed apart so its `via` can be
/// followed to the focal.
#[test]
fn rows_mode_lists_in_scope_rows_and_keeps_their_witnesses() {
    let l = layout();
    let mut arguments = callers("pkg");
    arguments.push(("scope_mode", json!("rows")));
    let payload = trace(&l.repo, &l.target, &arguments);
    let reach = &payload["reach"];
    assert_eq!(
        names(&reach["members"]),
        vec!["cyc_a", "cyc_b", "deep", "direct", "leaves", "top"],
        "{reach:#}"
    );
    let leaves = reach["members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == "leaves")
        .unwrap();
    assert_eq!(leaves["via"], l.bridge.id.to_string());
    let witnesses = reach["scope"]["via_outside"].as_array().unwrap();
    assert_eq!(witnesses.len(), 1, "{reach:#}");
    assert_eq!(witnesses[0]["name"], "bridge");
    assert_eq!(witnesses[0]["module"], "other.bridge_mod");
    assert_eq!(witnesses[0]["via"], l.target.id.to_string());
    assert_eq!(reach["scope"]["mode"], "rows");
    assert_eq!(payload["scope"]["mode"], "rows");
    assert_eq!(payload["scope"]["state"], "complete");
    // The chain stays the walk's evidence and names its out-of-scope steps.
    let outside_steps = payload["scope"]["outside_steps"].as_array().unwrap();
    assert!(!outside_steps.is_empty(), "{:#}", payload["scope"]);
}

/// A cycle inside the scope closes: each member is listed once.
#[test]
fn a_cycle_inside_the_scope_closes_with_each_member_once() {
    let l = layout();
    let payload = trace(&l.repo, &l.target, &callers("pkg"));
    let members = names(&payload["reach"]["members"]);
    assert_eq!(
        members.iter().filter(|name| *name == "cyc_a").count(),
        1,
        "{members:?}"
    );
    assert_eq!(
        members.iter().filter(|name| *name == "cyc_b").count(),
        1,
        "{members:?}"
    );
    assert_eq!(payload["reach"]["state"], "closed");
}

/// Test-role and source-role code sit in different modules, and a scope over
/// either answers for it alone, with each row saying its role.
#[test]
fn a_test_scope_and_a_source_scope_answer_apart() {
    let l = layout();
    let tests = trace(&l.repo, &l.target, &callers("tests"));
    let reach = &tests["reach"];
    assert_eq!(
        names(&reach["members"]),
        vec!["test_helper", "test_target"],
        "{reach:#}"
    );
    assert!(reach["members"]
        .as_array()
        .unwrap()
        .iter()
        .all(|row| row["role"] == "test"));
    assert_eq!(tests["scope"]["module"], "tests");
    assert_eq!(tests["scope"]["focal_in_scope"], false);
    assert_eq!(tests["scope"]["state"], "complete");

    let source = trace(&l.repo, &l.target, &callers("pkg"));
    assert!(source["reach"]["members"]
        .as_array()
        .unwrap()
        .iter()
        .all(|row| row.get("role").is_none()));
    assert!(!names(&source["reach"]["members"]).contains(&"test_target".to_string()));
}

/// A scope that holds no caller reachable inside it is a closed, empty answer
/// for that scope; one that names no module at all, or nothing, is refused,
/// so a typo never reads as an absence.
#[test]
fn an_empty_scope_is_an_answer_and_an_unknown_one_is_refused() {
    let l = layout();
    // `sub` is not the start of any module path; it sits inside exactly one.
    let payload = trace(&l.repo, &l.target, &callers("sub"));
    let scope = &payload["scope"];
    assert_eq!(scope["module"], "pkg.sub", "{scope:#}");
    assert_eq!(scope["asked"], "sub");
    // `deep` is in `pkg.sub` but reaches `target` only through `pkg.ui`.
    let reach = &payload["reach"];
    assert_eq!(reach["members_total"], 0, "{reach:#}");
    assert_eq!(reach["state"], "closed");
    assert!(reach["statement"]
        .as_str()
        .unwrap()
        .starts_with("Closed: under this policy nothing in pkg.sub calls target"));
    assert_eq!(scope["state"], "complete");
    assert_eq!(payload["chain"].as_array().unwrap().len(), 0);

    let unknown = refusal(&l.repo, &l.target, &callers("nosuch"));
    assert!(unknown.contains("names no module path"), "{unknown}");
    assert!(
        unknown.contains("pkg"),
        "the refusal names real scopes: {unknown}"
    );

    // A path is not a scope; the refusal names the module path it holds.
    let path = refusal(&l.repo, &l.target, &callers("src/pkg/sub"));
    assert!(path.contains("not a module path"), "{path}");
    assert!(path.contains("pkg.sub"), "{path}");

    let mut args = HashMap::from([
        ("focal".to_string(), json!(l.target.id.to_string())),
        ("scope".to_string(), json!("  ")),
    ]);
    assert!(
        crate::handlers::entities::handle_trace_data_flow_unpaged(&args, &l.repo.store).is_err()
    );
    args.remove("scope");
    args.insert("scope_mode".to_string(), json!("rows"));
    assert!(
        crate::handlers::entities::handle_trace_data_flow_unpaged(&args, &l.repo.store).is_err()
    );
}

/// A scope that sits inside several module paths names none of them.
#[test]
fn a_scope_inside_several_module_paths_is_refused_with_each() {
    let mut l = layout();
    l.repo.module("util", "src/pkg/util.py");
    l.repo.module("util", "src/other/util.py");
    let refused = refusal(&l.repo, &l.target, &callers("util"));
    assert!(refused.contains("pkg.util"), "{refused}");
    assert!(refused.contains("other.util"), "{refused}");
    // Named whole, either one resolves.
    let payload = trace(&l.repo, &l.target, &callers("pkg.util"));
    assert_eq!(payload["scope"]["module"], "pkg.util");
}

/// A caller whose file has no module record cannot be placed. It is never
/// dropped in silence: it is a frontier place, and the answer is open.
#[test]
fn a_caller_no_module_places_is_frontier_not_a_silent_drop() {
    let mut l = layout();
    let tool = l.repo.function("tool", "scripts/tool.py", &["target"]);
    l.repo.calls(&tool, &l.target);
    l.repo.admit();
    let payload = trace(&l.repo, &l.target, &callers("pkg"));
    let reach = &payload["reach"];
    assert_eq!(reach["state"], "open", "{reach:#}");
    let row = reach["frontier"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["kind"] == "unplaced")
        .unwrap_or_else(|| panic!("{reach:#}"));
    assert_eq!(row["in_name"], "tool");
    assert!(row["why"].as_str().unwrap().contains("no module record"));
    assert_eq!(payload["scope"]["state"], "open");
}

/// A re-export hands a name on without calling it. The caller in another
/// package reaches the definition through the edge the linker resolved, and
/// the re-exporting module is neither a member nor a bridge.
#[test]
fn a_cross_package_re_export_neither_bridges_nor_hides_a_caller() {
    let mut repo = Repo::new();
    repo.module("lib", "src/lib/__init__.py");
    repo.module("core", "src/lib/core.py");
    repo.module("app", "src/app/__init__.py");
    repo.module("main", "src/app/main.py");
    let parse = repo.function("parse", "src/lib/core.py", &[]);
    let internal = repo.function("internal", "src/lib/core.py", &["parse"]);
    let run = repo.function("run", "src/app/main.py", &["parse"]);
    let reexport = repo.entities[0].clone();
    assert_eq!(reexport.name, "lib");
    repo.relate(&reexport, &parse, RelationKind::Imports);
    repo.calls(&internal, &parse);
    repo.calls(&run, &parse);
    repo.admit();

    let app = trace(&repo, &parse, &callers("app"));
    assert_eq!(
        names(&app["reach"]["members"]),
        vec!["run"],
        "{:#}",
        app["reach"]
    );
    assert_eq!(
        app["reach"]["also_imported_by"], 0,
        "the re-export is outside app"
    );
    assert_eq!(app["reach"]["state"], "closed");
    assert!(!chain_names(&app).contains(&"lib".to_string()));

    let lib = trace(&repo, &parse, &callers("lib"));
    assert_eq!(names(&lib["reach"]["members"]), vec!["internal"]);
    assert_eq!(lib["reach"]["also_imported_by"], 1, "{:#}", lib["reach"]);
    assert_eq!(lib["scope"]["focal_in_scope"], true);
}

/// Eleven functions in one module, each calling the next, in a graph that
/// links calls, imports and references across modules, so a walk that reads
/// a node with no edge can call it a leaf rather than a coverage gap.
fn long_chain() -> (Repo, Vec<Entity>) {
    let mut repo = Repo::new();
    repo.module("pkg", "src/pkg/__init__.py");
    repo.module("deep", "src/pkg/deep.py");
    let wa_module = repo.module("wa", "src/pkg/wa.py");
    repo.module("wb", "src/pkg/wb.py");
    let wb = repo.function("wb", "src/pkg/wb.py", &[]);
    let wa = repo.function("wa", "src/pkg/wa.py", &["wb"]);
    repo.calls(&wa, &wb);
    repo.relate(&wa_module, &wb, RelationKind::Imports);
    repo.relate(&wa, &wb, RelationKind::References);
    let names: Vec<String> = (0..=10).map(|at| format!("h{at}")).collect();
    let mut hops = Vec::new();
    for (at, name) in names.iter().enumerate() {
        let next: Vec<&str> = names.get(at + 1).map(String::as_str).into_iter().collect();
        hops.push(repo.function(name, "src/pkg/deep.py", &next));
    }
    for pair in hops.windows(2) {
        repo.calls(&pair[0], &pair[1]);
    }
    repo.admit();
    (repo, hops)
}

/// A chain longer than the deepest walk: the calls walk is cut at depth 8, and
/// that cut is a frontier row, never completeness. The callers closure is not
/// cut at depth, so from the other end it reaches all ten and closes.
#[test]
fn a_chain_past_the_depth_bound_is_frontier_not_complete() {
    let (repo, hops) = long_chain();
    let calls = trace(
        &repo,
        &hops[0],
        &[
            ("direction", json!("calls")),
            ("depth", json!(8)),
            ("scope", json!("pkg")),
        ],
    );
    assert_eq!(calls["chain"].as_array().unwrap().len(), 8);
    let scope = &calls["scope"];
    assert_eq!(scope["answer"], "chain");
    assert_eq!(scope["state"], "open", "{scope:#}");
    assert_eq!(scope["frontier_total"], 1, "{scope:#}");
    let row = &scope["frontier"][0];
    assert_eq!(row["kind"], "depth");
    assert_eq!(row["name"], "h8");
    assert_eq!(row["step"], 8);
    let why = scope["frontier_kinds"]["depth"].as_str().unwrap();
    assert!(why.contains("depth 8, the deepest it goes"), "{why}");
    assert!(scope["statement"].as_str().unwrap().starts_with("Open:"));

    // Walked from a node whose chain ends inside the bound, nothing is cut.
    let short = trace(
        &repo,
        &hops[7],
        &[
            ("direction", json!("calls")),
            ("depth", json!(8)),
            ("scope", json!("pkg")),
        ],
    );
    assert_eq!(short["scope"]["state"], "complete", "{:#}", short["scope"]);
    assert_eq!(short["scope"]["frontier_total"], 0);

    let callers = trace(
        &repo,
        &hops[10],
        &[
            ("direction", json!("callers")),
            ("depth", json!(8)),
            ("scope", json!("pkg")),
        ],
    );
    assert_eq!(
        callers["reach"]["members_total"], 10,
        "{:#}",
        callers["reach"]
    );
    assert_eq!(callers["reach"]["state"], "closed");
    assert_eq!(callers["scope"]["state"], "complete");
    assert!(callers["scope"].get("frontier").is_none());
}

/// No scope, no new key anywhere: an unscoped trace is the walk it was.
#[test]
fn an_unscoped_trace_carries_no_scope() {
    let l = layout();
    for direction in ["callers", "calls", "both"] {
        let payload = trace(&l.repo, &l.target, &[("direction", json!(direction))]);
        assert!(payload.get(SCOPE_KEY).is_none(), "{payload:#}");
        if let Some(reach) = payload.get("reach") {
            assert!(reach.get("scope").is_none());
            assert_eq!(
                reach["question"],
                "which entities can end up calling the focal"
            );
        }
    }
    let whole = trace(&l.repo, &l.target, &[("direction", json!("callers"))]);
    assert!(names(&whole["reach"]["members"]).contains(&"leaves".to_string()));
    let _ = (&l.direct, &l.deep, &l.leaves);
}

/// A scope is part of the question a cursor freezes, and an unscoped query's
/// identity is the one it always had.
#[test]
fn a_scope_is_part_of_the_frozen_question() {
    let authority = json!({"store": "s"});
    let unscoped = HashMap::from([("focal".to_string(), json!("target"))]);
    let mut scoped = unscoped.clone();
    scoped.insert("scope".to_string(), json!("pkg"));
    let mut rows = scoped.clone();
    rows.insert("scope_mode".to_string(), json!("rows"));
    let identity = |arguments: &HashMap<String, Value>| {
        format!(
            "{:?}",
            crate::trace_pages::Context::from_arguments(arguments, &authority)
        )
    };
    assert_ne!(identity(&unscoped), identity(&scoped));
    assert_ne!(identity(&scoped), identity(&rows));
    let before = crate::trace_pages::Context::new(
        &json!({
            "focal": "target", "target": null, "question": null, "direction": "both",
            "depth": 3,
            "limit_per_step": crate::remediation::TRACE_DEFAULT_LIMIT_PER_STEP as u64,
            "include_body": true, "include_type_edges": false, "client_root": null,
        }),
        &authority,
    )
    .continuing(crate::trace_pages::PageKind::Trace, &unscoped, true);
    assert_eq!(identity(&unscoped), format!("{before:?}"));
}

/// The typed request takes the scope and holds it to its rules.
#[test]
fn the_typed_trace_request_takes_a_scope_and_its_rules() {
    let mut ok = HashMap::from([
        ("focal".to_string(), json!("target")),
        ("scope".to_string(), json!("pkg")),
        ("scope_mode".to_string(), json!("output")),
    ]);
    crate::requests::check_call(crate::verbs::TRACE, &mut ok).expect("a scoped walk is valid");
    assert_eq!(
        ok["scope_mode"], "rows",
        "a synonym reads as its served value"
    );

    // No scope named, none supplied: an unscoped call reaches the handler as
    // it always did, with no default mode filled in.
    let mut bare = HashMap::from([("focal".to_string(), json!("target"))]);
    crate::requests::check_call(crate::verbs::TRACE, &mut bare).unwrap();
    assert!(
        !bare.contains_key("scope") && !bare.contains_key("scope_mode"),
        "{bare:?}"
    );

    let mut lone_mode = HashMap::from([
        ("focal".to_string(), json!("target")),
        ("scope_mode".to_string(), json!("rows")),
    ]);
    let refused = crate::requests::check_call(crate::verbs::TRACE, &mut lone_mode).unwrap_err();
    assert!(refused.contains("scope"), "{refused}");

    let mut route = HashMap::from([
        ("focal".to_string(), json!("target")),
        ("to".to_string(), json!("main")),
        ("scope".to_string(), json!("pkg")),
    ]);
    let refused = crate::requests::check_call(crate::verbs::TRACE, &mut route).unwrap_err();
    assert!(refused.contains("route"), "{refused}");
}

/// A bounded reply that could not carry the reach answer whole says the
/// scoped answer is bounded, never complete.
#[test]
fn a_reach_answer_the_reply_cut_is_bounded() {
    let mut payload = json!({
        "scope": {"module": "pkg", "answer": "reach", "state": "complete", "statement": "x"},
    });
    reconcile(&mut payload);
    assert_eq!(payload["scope"]["state"], "bounded");
    let mut kept = json!({
        "scope": {"module": "pkg", "answer": "reach", "state": "complete", "statement": "x"},
        "reach": {"state": "closed"},
    });
    reconcile(&mut kept);
    assert_eq!(kept["scope"]["state"], "complete");
    let mut chain = json!({"scope": {"answer": "chain", "state": "open"}});
    reconcile(&mut chain);
    assert_eq!(chain["scope"]["state"], "open");
}
