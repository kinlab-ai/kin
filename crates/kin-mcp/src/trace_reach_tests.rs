// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

use super::*;
use crate::call_sites::fixture::{admit, ledger, proof_context, spanned_entity};
use kin_db::InMemoryGraph;
use kin_model::graph::EntityStore as _;
use kin_model::relation::{Relation, RelationEvidence, RelationOrigin};
use kin_model::{
    CallSiteState, DispatchProvenance, DispatchScope, DispatchSet, DispatchSoundness,
    DispatchWorld, EscapeResidue, Hash256, LanguageId, RelationId, ResidueSite, ResolutionRecord,
    SourceSpan, UnresolvedReason,
};

const FILE: &str = "src/app.py";

/// A text reader holding every body the fixture wrote, the way the daemon's
/// reader holds the repository's.
#[derive(Default)]
struct Bodies(HashMap<EntityId, (usize, String)>);

impl SiteText for Bodies {
    fn quote(
        &self,
        caller: &Entity,
        site: &SourceSpan,
    ) -> std::result::Result<String, &'static str> {
        let (start, body) = self.0.get(&caller.id).ok_or("caller_text_not_held")?;
        crate::handlers::external_symbols::quote_site(caller, site, body, *start)
    }
}

impl SiteText for &Bodies {
    fn quote(
        &self,
        caller: &Entity,
        site: &SourceSpan,
    ) -> std::result::Result<String, &'static str> {
        (**self).quote(caller, site)
    }
}

/// A one-file repository: each definition follows the last, one blank line
/// apart, and every edge carries the site of the call it records.
struct Repo {
    store: InMemoryGraph,
    bodies: Bodies,
    entities: Vec<Entity>,
    byte: usize,
    line: u32,
    context: kin_model::ResolutionRecord,
}

impl Repo {
    fn new() -> Self {
        Self {
            store: InMemoryGraph::new(),
            bodies: Bodies::default(),
            entities: Vec::new(),
            byte: 0,
            line: 10,
            context: proof_context(LanguageId::Python, "current"),
        }
    }

    fn define(&mut self, name: &str, kind: EntityKind, body: &str) -> Entity {
        self.define_in_file(name, kind, FILE, body)
    }

    fn define_in_file(&mut self, name: &str, kind: EntityKind, file: &str, body: &str) -> Entity {
        let mut entity = spanned_entity(name, file, LanguageId::Python, self.byte, body);
        entity.kind = kind;
        let span = entity.span.as_mut().unwrap();
        span.start_line = self.line;
        span.end_line = self.line + body.matches('\n').count() as u32;
        self.bodies
            .0
            .insert(entity.id, (self.byte, body.to_string()));
        self.byte += body.len() + 1;
        self.line += body.matches('\n').count() as u32 + 1;
        self.store.upsert_entity(&entity).unwrap();
        self.entities.push(entity.clone());
        entity
    }

    fn function(&mut self, name: &str, body: &str) -> Entity {
        self.define(name, EntityKind::Function, body)
    }

    /// The span of `token`'s first occurrence in `holder`'s body after its
    /// first line.
    fn span_of(&self, holder: &Entity, token: &str) -> SourceSpan {
        let (start, body) = &self.bodies.0[&holder.id];
        let first = body.find('\n').unwrap();
        let offset = first + body[first..].find(token).expect("token in body");
        let line =
            holder.span.as_ref().unwrap().start_line + body[..offset].matches('\n').count() as u32;
        SourceSpan {
            file: kin_model::FilePathId::new(FILE),
            start_byte: start + offset,
            end_byte: start + offset + token.len(),
            start_line: line,
            start_col: 0,
            end_line: line,
            end_col: 0,
        }
    }

    fn relate(&self, from: &Entity, to: &Entity, kind: RelationKind, confidence: f32, token: &str) {
        let relation = Relation {
            id: RelationId::new(),
            kind,
            src: GraphNodeId::Entity(from.id),
            dst: GraphNodeId::Entity(to.id),
            confidence,
            origin: RelationOrigin::Parsed,
            created_in: None,
            import_source: None,
            evidence: vec![RelationEvidence {
                source_span: Some(self.span_of(from, token)),
                ..RelationEvidence::default()
            }],
        };
        self.store.upsert_relation(&relation).unwrap();
    }

    fn calls(&self, from: &Entity, to: &Entity) {
        self.relate(from, to, RelationKind::Calls, 1.0, leaf(&to.name));
    }

    /// Every entity's ledger, each site proven to where its edge goes unless
    /// `sites` says otherwise, admitted with the proof context.
    fn admit_ledgers(
        &self,
        sites: &[(&Entity, Vec<(&str, CallSiteState)>)],
        extra: Vec<ResolutionRecord>,
    ) {
        let id = self.context.id();
        let mut records = vec![self.context.clone()];
        records.extend(extra);
        for entity in &self.entities {
            let body = &self.bodies.0[&entity.id].1;
            let own = sites
                .iter()
                .find(|(holder, _)| holder.id == entity.id)
                .map(|(_, sites)| sites.clone())
                .unwrap_or_default();
            records.push(ledger(entity, body, id, own));
        }
        let entities: Vec<&Entity> = self.entities.iter().collect();
        admit(&self.store, &entities, records);
    }
}

fn leaf(name: &str) -> &str {
    name.rsplit(['.', ':']).next().unwrap_or(name)
}

fn contained(focals: &[(&Entity, Vec<String>)]) -> Vec<FocalEscape> {
    focals.iter().map(|_| FocalEscape::contained(1)).collect()
}

/// A meter that never stops.
struct Unmetered;

impl ReachMeter for Unmetered {
    fn charge_edge(&mut self) -> Option<&'static str> {
        None
    }
    fn stop(&self) -> Option<&'static str> {
        None
    }
}

/// A meter that stops after `left` relations.
struct Edges(usize);

impl ReachMeter for Edges {
    fn charge_edge(&mut self) -> Option<&'static str> {
        self.0 = self.0.saturating_sub(1);
        (self.0 == 0).then_some("edge_budget")
    }
    fn stop(&self) -> Option<&'static str> {
        (self.0 == 0).then_some("edge_budget")
    }
}

fn reach_with(
    repo: &Repo,
    focal: &Entity,
    limits: Limits,
    meter: &mut dyn ReachMeter,
    census: &dyn Fn(&[(&Entity, Vec<String>)]) -> Vec<FocalEscape>,
) -> Value {
    read_with(
        &repo.store,
        census,
        &|| &repo.bodies,
        &repo.bodies,
        focal,
        limits,
        meter,
        &mut Scans::default(),
    )
}

fn reach(repo: &Repo, focal: &Entity) -> Value {
    reach_with(repo, focal, Limits::default(), &mut Unmetered, &contained)
}

fn names(rows: &Value) -> Vec<String> {
    rows.as_array()
        .unwrap()
        .iter()
        .map(|row| row["name"].as_str().unwrap().to_string())
        .collect()
}

fn member<'v>(block: &'v Value, name: &str) -> &'v Value {
    block["members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == name)
        .unwrap_or_else(|| panic!("{name} is not a member: {block:#}"))
}

/// The witness path of `name`: its own name, then each `via` to the focal.
fn witness(block: &Value, focal: &Entity, name: &str) -> Vec<String> {
    let mut path = vec![name.to_string()];
    let mut row = member(block, name);
    loop {
        let via = row["via"].as_str().unwrap();
        if via == focal.id.to_string() {
            path.push(focal.name.clone());
            return path;
        }
        row = block["members"]
            .as_array()
            .unwrap()
            .iter()
            .find(|candidate| candidate["id"] == via)
            .unwrap();
        path.push(row["name"].as_str().unwrap().to_string());
        assert!(path.len() < 20, "a witness path loops: {path:?}");
    }
}

/// A class whose span encloses `inner`, adjacent definitions, with their
/// text as its own.
fn enclosing(repo: &mut Repo, name: &str, inner: &[&Entity]) -> Entity {
    let first = inner.first().unwrap().span.clone().unwrap();
    let last = inner.last().unwrap().span.clone().unwrap();
    let body: Vec<String> = inner
        .iter()
        .map(|entity| repo.bodies.0[&entity.id].1.clone())
        .collect();
    let body = body.join("\n");
    let mut entity = spanned_entity(name, FILE, LanguageId::Python, first.start_byte, &body);
    entity.kind = EntityKind::Class;
    let span = entity.span.as_mut().unwrap();
    span.start_line = first.start_line;
    span.end_line = last.end_line;
    assert_eq!(
        span.end_byte, last.end_byte,
        "the fixture's definitions are adjacent"
    );
    repo.bodies.0.insert(entity.id, (first.start_byte, body));
    repo.store.upsert_entity(&entity).unwrap();
    repo.entities.push(entity.clone());
    entity
}

/// `top` reaches the focal both through `left` and through `right`, and
/// `outer` reaches it only through `top`: a diamond under a tail.
struct Diamond {
    repo: Repo,
    target: Entity,
    left: Entity,
    right: Entity,
    top: Entity,
    outer: Entity,
}

fn diamond(top_to_left_confidence: f32) -> Diamond {
    let mut repo = Repo::new();
    let target = repo.function("target", "def target():\n    return 1\n");
    let left = repo.function("left", "def left():\n    return target()\n");
    let right = repo.function("right", "def right():\n    x = 2\n    return target()\n");
    let top = repo.function("top", "def top():\n    left()\n    return right()\n");
    let outer = repo.function("outer", "def outer():\n    return top()\n");
    repo.calls(&left, &target);
    repo.calls(&right, &target);
    repo.relate(
        &top,
        &left,
        RelationKind::Calls,
        top_to_left_confidence,
        "left",
    );
    repo.calls(&top, &right);
    repo.calls(&outer, &top);
    Diamond {
        repo,
        target,
        left,
        right,
        top,
        outer,
    }
}

fn admit_diamond(d: &Diamond) {
    d.repo.admit_ledgers(
        &[
            (
                &d.left,
                vec![(
                    "target",
                    CallSiteState::ProvenTarget {
                        target: d.target.id,
                    },
                )],
            ),
            (
                &d.right,
                vec![(
                    "target",
                    CallSiteState::ProvenTarget {
                        target: d.target.id,
                    },
                )],
            ),
            (
                &d.top,
                vec![
                    ("left", CallSiteState::ProvenTarget { target: d.left.id }),
                    ("right", CallSiteState::ProvenTarget { target: d.right.id }),
                ],
            ),
            (
                &d.outer,
                vec![("top", CallSiteState::ProvenTarget { target: d.top.id })],
            ),
        ],
        Vec::new(),
    );
}

/// Every entity that reaches the focal is a member exactly once, at its
/// shortest distance, by the strongest of its shortest paths, and each row's
/// line and code are the call it makes to its `via`, so following `via` from
/// any member walks its witness path to the focal.
#[test]
fn a_diamond_names_each_member_once_with_its_shortest_strongest_witness() {
    let d = diamond(0.5);
    admit_diamond(&d);
    let block = reach(&d.repo, &d.target);

    assert_eq!(block["state"], "closed", "{block:#}");
    assert_eq!(
        names(&block["members"]),
        vec!["left", "right", "top", "outer"],
        "{block:#}"
    );
    assert_eq!(block["members_total"], 4);
    for (name, distance) in [("left", 1), ("right", 1), ("top", 2), ("outer", 3)] {
        assert_eq!(member(&block, name)["distance"], distance, "{name}");
    }
    // `top` calls `left` by a bare name match and `right` by a proven edge;
    // both are shortest, so the proven one is its witness.
    assert_eq!(member(&block, "top")["via"], d.right.id.to_string());
    assert_eq!(member(&block, "top")["proof"], "type_resolved");
    assert_eq!(member(&block, "top")["proven_path"], true);
    assert_eq!(
        witness(&block, &d.target, "outer"),
        ["outer", "top", "right", "target"]
    );

    // Each hop's site is the call in the member's own body, by file line.
    let line = |entity: &Entity, offset: u32| entity.span.as_ref().unwrap().start_line + offset + 1;
    for (name, entity, offset, code) in [
        ("left", &d.left, 1, "return target()"),
        ("right", &d.right, 2, "return target()"),
        ("top", &d.top, 2, "return right()"),
        ("outer", &d.outer, 1, "return top()"),
    ] {
        let row = member(&block, name);
        assert_eq!(row["line"], line(entity, offset), "{name}: {row:#}");
        assert_eq!(row["code"], code, "{name}: {row:#}");
    }

    assert_eq!(block["frontier"], json!([]), "{block:#}");
    assert_eq!(block["frontier_total"], 0);
    assert_eq!(block["bounds"], json!([]));
    assert_eq!(block["also_referenced_by"], json!([]));
    let statement = block["statement"].as_str().unwrap();
    assert!(statement.starts_with("Closed:"), "{statement}");
    assert!(statement.contains("these 4 entities"), "{statement}");
}

/// A hop matched by a bare name is followed and says so, and what it
/// reaches is listed apart from the members: the entity that makes the hop,
/// and every entity whose only path runs through it.
#[test]
fn a_name_only_hop_is_followed_and_listed_apart_from_the_members() {
    let mut repo = Repo::new();
    let target = repo.function("target", "def target():\n    return 1\n");
    let guess = repo.function("guess", "def guess():\n    return target()\n");
    let outer = repo.function("outer", "def outer():\n    return guess()\n");
    repo.relate(&guess, &target, RelationKind::Calls, 0.5, "target");
    repo.calls(&outer, &guess);
    repo.admit_ledgers(
        &[
            (
                &guess,
                vec![("target", CallSiteState::ProvenTarget { target: target.id })],
            ),
            (
                &outer,
                vec![("guess", CallSiteState::ProvenTarget { target: guess.id })],
            ),
        ],
        Vec::new(),
    );
    let block = reach(&repo, &target);
    assert_eq!(block["members"], json!([]), "{block:#}");
    assert_eq!(block["members_total"], 0);
    assert_eq!(
        names(&block[NAME_ONLY_KEY]),
        ["guess", "outer"],
        "{block:#}"
    );
    assert_eq!(block[NAME_ONLY_TOTAL_KEY], 2);
    let row = |name: &str| {
        block[NAME_ONLY_KEY]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["name"] == name)
            .unwrap()
    };
    assert_eq!(row("guess")["proof"], "name_only", "{block:#}");
    assert_eq!(row("guess")["proven_path"], false);
    assert_eq!(row("guess")["code"], "return target()");
    assert_eq!(row("guess")["via"], target.id.to_string());
    // The hop `outer` makes is proven; its path runs through the one that
    // is not.
    assert_eq!(row("outer")["proof"], "type_resolved");
    assert_eq!(row("outer")["proven_path"], false);
    assert_eq!(row("outer")["via"], guess.id.to_string());
    let statement = block["statement"].as_str().unwrap();
    assert!(
        statement.contains(
            "2 more entities reach it only through a call matched by name alone, which no \
             resolver tied to its target; see name_only."
        ),
        "{statement}"
    );
    assert!(rows_delivered(block.as_object().unwrap()));
    let mut cut = block.as_object().unwrap().clone();
    cut[NAME_ONLY_KEY].as_array_mut().unwrap().pop();
    assert!(
        !rows_delivered(&cut),
        "a row on a later page is not delivered"
    );

    // An entity with a path free of such a hop is a member by that path.
    let mut repo = Repo::new();
    let target = repo.function("target", "def target():\n    return 1\n");
    let guess = repo.function("guess", "def guess():\n    return target()\n");
    let outer = repo.function("outer", "def outer():\n    guess()\n    return target()\n");
    repo.relate(&guess, &target, RelationKind::Calls, 0.5, "target");
    repo.calls(&outer, &guess);
    repo.calls(&outer, &target);
    repo.admit_ledgers(
        &[
            (
                &guess,
                vec![("target", CallSiteState::ProvenTarget { target: target.id })],
            ),
            (
                &outer,
                vec![
                    ("guess", CallSiteState::ProvenTarget { target: guess.id }),
                    ("target", CallSiteState::ProvenTarget { target: target.id }),
                ],
            ),
        ],
        Vec::new(),
    );
    let block = reach(&repo, &target);
    assert_eq!(names(&block["members"]), ["outer"], "{block:#}");
    assert_eq!(member(&block, "outer")["via"], target.id.to_string());
    assert_eq!(member(&block, "outer")["proven_path"], true);
    assert_eq!(names(&block[NAME_ONLY_KEY]), ["guess"]);
    assert_eq!(block[NAME_ONLY_TOTAL_KEY], 1);
}

/// An entity that names a member without calling it, by a reference or a
/// type, is listed apart with its site and never becomes a member. One that
/// also calls a member is a member and is not listed again.
#[test]
fn references_are_reported_apart_and_never_become_members() {
    let mut d = diamond(1.0);
    let inspector = d.repo.function(
        "inspector",
        "def inspector():\n    for method in (left, right):\n        print(method)\n",
    );
    let annotator = d
        .repo
        .function("annotator", "def annotator(fn: target):\n    return fn\n");
    let both = d
        .repo
        .function("both", "def both():\n    keep = left\n    return right()\n");
    d.repo
        .relate(&inspector, &d.left, RelationKind::References, 1.0, "left");
    // A second edge at the same site is the same reference, not another one.
    d.repo
        .relate(&inspector, &d.left, RelationKind::UsesType, 1.0, "left");
    // A declaration enclosing `left` and `right` restates the sites inside
    // them. It encloses members, so what it names there is theirs.
    let container = enclosing(&mut d.repo, "Container", &[&d.left, &d.right]);
    d.repo
        .relate(&container, &d.target, RelationKind::UsesType, 1.0, "target");
    d.repo
        .relate(&annotator, &d.target, RelationKind::UsesType, 1.0, "fn");
    d.repo
        .relate(&both, &d.left, RelationKind::References, 1.0, "left");
    d.repo.calls(&both, &d.right);
    admit_diamond(&d);
    let block = reach(&d.repo, &d.target);

    assert!(names(&block["members"]).contains(&"both".to_string()));
    for name in ["inspector", "annotator", "Container"] {
        assert!(
            !names(&block["members"]).contains(&name.to_string()),
            "{name} names a member without calling it: {block:#}"
        );
    }
    let referrers = &block["also_referenced_by"];
    let mut listed = names(referrers);
    listed.sort();
    assert_eq!(listed, vec!["annotator", "inspector"], "{block:#}");
    assert_eq!(block["also_referenced_total"], 2);
    let inspector_row = referrers
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == "inspector")
        .unwrap();
    assert_eq!(
        inspector_row["refs"],
        json!([{
            "to": d.left.id.to_string(),
            "to_name": "left",
            "how": "reference",
            "line": inspector.span.as_ref().unwrap().start_line + 2,
            "code": "for method in (left, right):",
        }]),
        "{block:#}"
    );
    let annotator_row = referrers
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == "annotator")
        .unwrap();
    assert_eq!(annotator_row["refs"][0]["how"], "type");
    assert_eq!(annotator_row["refs"][0]["to"], d.target.id.to_string());
    // Still closed: a reference is reported apart, never an open question.
    assert_eq!(block["state"], "closed", "{block:#}");
    assert!(block["statement"]
        .as_str()
        .unwrap()
        .contains("2 more entities refer to them without calling them"));
}

/// A call that spells a member's name and that no resolver proved is the one
/// place that could add a caller. It is listed with its caller, its code and
/// why, and the block is open. The same kind of site inside a member cannot
/// add one, and is only counted. A site the member's own proven edge to the
/// focal carries is that edge's call, and is neither.
#[test]
fn the_frontier_lists_an_unresolved_call_with_its_code_and_why() {
    let mut d = diamond(1.0);
    // A second definition that a call spelling `target` could reach keeps the
    // name from reaching the focal alone.
    let other = d.repo.define(
        "Other.target",
        EntityKind::Method,
        "def target(self):\n    return 2\n",
    );
    let maybe = d
        .repo
        .function("maybe", "def maybe(thing):\n    return thing.target(3)\n");
    // A member through `left` that also calls something named `target` on a
    // value: no edge of its own carries that call.
    let also = d.repo.function(
        "also",
        "def also(thing):\n    left()\n    return thing.target(4)\n",
    );
    d.repo.calls(&also, &d.left);
    let unresolved = CallSiteState::Unresolved {
        reason: UnresolvedReason::NoAnswer,
    };
    d.repo.admit_ledgers(
        &[
            (
                &d.left,
                vec![(
                    "target",
                    CallSiteState::ProvenTarget {
                        target: d.target.id,
                    },
                )],
            ),
            // A member's call to the focal that the ledger leaves open and
            // its proven edge carries: the call is the edge's.
            (&d.right, vec![("target", unresolved.clone())]),
            // A member's unresolved call that no edge carries: `also` is
            // already a member, so the site cannot add one.
            (
                &also,
                vec![
                    ("left", CallSiteState::ProvenTarget { target: d.left.id }),
                    ("target", unresolved.clone()),
                ],
            ),
            (
                &d.top,
                vec![
                    ("left", CallSiteState::ProvenTarget { target: d.left.id }),
                    ("right", CallSiteState::ProvenTarget { target: d.right.id }),
                ],
            ),
            (
                &d.outer,
                vec![("top", CallSiteState::ProvenTarget { target: d.top.id })],
            ),
            (&maybe, vec![("target", unresolved)]),
        ],
        Vec::new(),
    );
    let _ = other;
    let block = reach(&d.repo, &d.target);

    assert_eq!(block["state"], "open", "{block:#}");
    let frontier = block["frontier"].as_array().unwrap();
    let row = frontier
        .iter()
        .find(|row| row["in_name"] == "maybe")
        .unwrap_or_else(|| panic!("{block:#}"));
    assert_eq!(row["kind"], "unresolved_call");
    assert_eq!(row["to"], d.target.id.to_string());
    assert_eq!(row["in"], maybe.id.to_string());
    assert_eq!(row["file"], FILE);
    assert_eq!(row["line"], maybe.span.as_ref().unwrap().start_line + 2);
    assert_eq!(row["code"], "return thing.target(3)");
    let why = row["why"].as_str().unwrap();
    assert!(why.contains("calls `target`"), "{why}");
    assert!(
        why.contains("no resolver proved where the call goes"),
        "{why}"
    );
    assert!(!names(&block["members"]).contains(&"maybe".to_string()));
    // `right`'s open call is its own edge's, and `also`'s is inside a member:
    // whatever that one reaches, `also` is already in the set.
    assert!(
        frontier
            .iter()
            .all(|row| row["in_name"] != "right" && row["in_name"] != "also"),
        "{block:#}"
    );
    assert_eq!(block["settled_by_membership"], 1, "{block:#}");
    assert_eq!(block["frontier_total"], 1, "{block:#}");
    let statement = block["statement"].as_str().unwrap();
    assert!(statement.contains("could still add callers"), "{statement}");
}

/// A call site whose name reaches only the member is followed as a member,
/// marked `name_unique`, and the closure goes on from it through edges.
#[test]
fn a_name_unique_site_is_followed_and_the_closure_goes_on_from_it() {
    let mut repo = Repo::new();
    let target = repo.function("target", "def target():\n    return 1\n");
    let found = repo.function("found", "def found(thing):\n    return thing.target(3)\n");
    let above = repo.function("above", "def above():\n    return found(None)\n");
    repo.calls(&above, &found);
    repo.admit_ledgers(
        &[
            (
                &found,
                vec![(
                    "target",
                    CallSiteState::Unresolved {
                        reason: UnresolvedReason::NoAnswer,
                    },
                )],
            ),
            (
                &above,
                vec![("found", CallSiteState::ProvenTarget { target: found.id })],
            ),
        ],
        Vec::new(),
    );
    let block = reach(&repo, &target);
    assert_eq!(
        names(&block["members"]),
        vec!["found", "above"],
        "{block:#}"
    );
    let row = member(&block, "found");
    assert_eq!(row["proof"], "name_unique");
    assert_eq!(row["proven_path"], false);
    assert_eq!(row["via"], target.id.to_string());
    assert_eq!(row["line"], found.span.as_ref().unwrap().start_line + 2);
    assert_eq!(row["code"], "return thing.target(3)");
    assert_eq!(member(&block, "above")["distance"], 2);
    assert_eq!(block["state"], "closed", "{block:#}");
}

/// A contained, fully resolved alias is a followed call, with no stored
/// value or unmeasured value-call exception. It keeps a complete answer.
#[test]
fn a_contained_alias_with_a_proven_call_keeps_complete_reach() {
    let mut repo = Repo::new();
    let target = repo.function("target", "def target():\n    return 1\n");
    let binding = repo.define("bindings", EntityKind::Module, "alias = target\n");
    let caller = repo.function("caller", "def caller():\n    return alias()\n");
    repo.relate(&caller, &target, RelationKind::Calls, 1.0, "alias");
    repo.admit_ledgers(
        &[(
            &caller,
            vec![("alias", CallSiteState::ProvenTarget { target: target.id })],
        )],
        Vec::new(),
    );
    let target_id = target.id;
    let alias_line = binding.span.as_ref().unwrap().start_line + 1;
    let census = move |focals: &[(&Entity, Vec<String>)]| -> Vec<FocalEscape> {
        focals
            .iter()
            .map(|(entity, _)| {
                if entity.id == target_id {
                    FocalEscape::Contained {
                        entities_checked: 3,
                        residue: EscapeResidue {
                            alias_names: vec!["alias".to_string()],
                            alias_sites: vec![ResidueSite {
                                file: FILE.to_string(),
                                line: alias_line,
                                kind: "alias",
                                name: Some("alias".to_string()),
                                detail: None,
                            }],
                            ..EscapeResidue::default()
                        },
                    }
                } else {
                    FocalEscape::contained(3)
                }
            })
            .collect()
    };
    let block = reach_with(&repo, &target, Limits::default(), &mut Unmetered, &census);
    assert_eq!(names(&block["members"]), vec!["caller"]);
    assert_eq!(block["state"], "closed", "{block:#}");
    assert_eq!(block["frontier_total"], 0);
    assert_eq!(block["blind_spots"].get("calls_through_values"), None);
    assert_eq!(block["blind_spots"].get("uncensused"), None);
    let machine = json!({"direction":"callers", "focal_name":"target", "chain":[],
        "_kin":{"verdict":{"state":"certified", "safe_to_conclude_absent":true}},
        "reach":block});
    let reply = crate::plain_reply::plain_answer(machine.as_object().unwrap().clone(), "trace");
    assert_eq!(
        reply["_kin"]["verdict"],
        crate::plain_reply::COMPLETE,
        "{reply:#}"
    );
}

/// A call a ledger records through a value binding, whose dispatch set names
/// a member, is followed as a callback. The place the member is passed as a
/// value is listed apart, with the use.
#[test]
fn a_callback_a_ledger_records_is_followed_and_the_value_use_is_listed() {
    let mut d = diamond(1.0);
    let holder = d
        .repo
        .function("holder", "def holder():\n    register(left)\n");
    let runner = d
        .repo
        .function("runner", "def runner():\n    return hook()\n");
    let offset = d.repo.bodies.0[&runner.id].1.find("hook()").unwrap() as u32;
    let set = DispatchSet {
        scope: DispatchScope::Site {
            caller: runner.id,
            offset,
            length: "hook".len() as u32,
        },
        members: vec![GraphNodeId::Entity(d.left.id)],
        world: DispatchWorld::Open,
        soundness: DispatchSoundness::Enumerated,
        provenance: DispatchProvenance::WriteReferences,
        context: d.repo.context.id(),
        scope_digest: Hash256::from_bytes([7; 32]),
    };
    let set_record = ResolutionRecord::DispatchSet(set);
    let set_id = set_record.id();
    d.repo.admit_ledgers(
        &[
            (
                &d.left,
                vec![(
                    "target",
                    CallSiteState::ProvenTarget {
                        target: d.target.id,
                    },
                )],
            ),
            (
                &d.right,
                vec![(
                    "target",
                    CallSiteState::ProvenTarget {
                        target: d.target.id,
                    },
                )],
            ),
            (
                &d.top,
                vec![
                    ("left", CallSiteState::ProvenTarget { target: d.left.id }),
                    ("right", CallSiteState::ProvenTarget { target: d.right.id }),
                ],
            ),
            (
                &d.outer,
                vec![("top", CallSiteState::ProvenTarget { target: d.top.id })],
            ),
            (
                &runner,
                vec![(
                    "hook",
                    CallSiteState::Binding {
                        may_call: Some(set_id),
                    },
                )],
            ),
        ],
        vec![set_record],
    );
    let left_id = d.left.id;
    let holder_line = holder.span.as_ref().unwrap().start_line + 2;
    let census = move |focals: &[(&Entity, Vec<String>)]| -> Vec<FocalEscape> {
        focals
            .iter()
            .map(|(entity, _)| {
                if entity.id == left_id {
                    FocalEscape::Escapes {
                        reason: "held as a value",
                        residue: EscapeResidue {
                            values: vec![ResidueSite {
                                file: FILE.to_string(),
                                line: holder_line,
                                kind: "argument",
                                name: Some("left".to_string()),
                                detail: None,
                            }],
                            ..EscapeResidue::default()
                        },
                    }
                } else {
                    FocalEscape::contained(1)
                }
            })
            .collect()
    };
    let block = reach_with(
        &d.repo,
        &d.target,
        Limits::default(),
        &mut Unmetered,
        &census,
    );

    let row = member(&block, "runner");
    assert_eq!(row["proof"], "callback", "{block:#}");
    assert_eq!(row["via"], d.left.id.to_string());
    assert_eq!(row["code"], "return hook()");
    assert_eq!(row["line"], runner.span.as_ref().unwrap().start_line + 2);

    let referrer = block["also_referenced_by"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == "holder")
        .unwrap_or_else(|| panic!("{block:#}"));
    assert_eq!(
        referrer["refs"],
        json!([{
            "to": d.left.id.to_string(),
            "to_name": "left",
            "how": "value",
            "value": "argument",
            "line": holder_line,
            "code": "register(left)",
        }]),
        "{block:#}"
    );
    assert!(!names(&block["members"]).contains(&"holder".to_string()));
}

/// An invocation the census lists with no call expression, such as a
/// decorator, is a frontier row with its code, not a silent gap.
#[test]
fn an_invocation_without_a_call_expression_is_on_the_frontier() {
    let mut d = diamond(1.0);
    let decorated = d.repo.function(
        "decorated",
        "def decorated():\n    @target\n    def inner():\n        pass\n",
    );
    admit_diamond(&d);
    let target_id = d.target.id;
    let line = decorated.span.as_ref().unwrap().start_line + 2;
    let census = move |focals: &[(&Entity, Vec<String>)]| -> Vec<FocalEscape> {
        focals
            .iter()
            .map(|(entity, _)| {
                if entity.id == target_id {
                    FocalEscape::Contained {
                        entities_checked: 1,
                        residue: EscapeResidue {
                            implicit_calls: vec![ResidueSite {
                                file: FILE.to_string(),
                                line,
                                kind: "decorator",
                                name: Some("target".to_string()),
                                detail: None,
                            }],
                            ..EscapeResidue::default()
                        },
                    }
                } else {
                    FocalEscape::contained(1)
                }
            })
            .collect()
    };
    let block = reach_with(
        &d.repo,
        &d.target,
        Limits::default(),
        &mut Unmetered,
        &census,
    );
    assert_eq!(block["state"], "open", "{block:#}");
    assert_eq!(
        block["frontier"],
        json!([{
            "kind": "implicit_call",
            "to": d.target.id.to_string(),
            "to_name": "target",
            "in": decorated.id.to_string(),
            "in_name": "decorated",
            "file": FILE,
            "line": line,
            "code": "@target",
            "why": "`decorator` syntax runs `target` without a call expression, so no call edge \
                    records it",
        }]),
        "{block:#}"
    );
}

/// A limit that stops the closure is reported as that limit, where it
/// stopped, and the block never reads as closed.
#[test]
fn a_bound_is_reported_as_a_bound_and_never_as_closed() {
    let d = diamond(1.0);
    admit_diamond(&d);

    let capped = reach_with(
        &d.repo,
        &d.target,
        Limits {
            members_max: 2,
            ..Limits::default()
        },
        &mut Unmetered,
        &contained,
    );
    assert_eq!(capped["state"], "bounded", "{capped:#}");
    assert_eq!(capped["members_total"], 2);
    let bound = &capped["bounds"][0];
    assert_eq!(bound["which"], "members_max", "{capped:#}");
    assert_eq!(bound["limit"], 2);
    assert!(bound["at"]
        .as_array()
        .unwrap()
        .iter()
        .any(|at| at["name"] == "left" || at["name"] == "right"));
    let statement = capped["statement"].as_str().unwrap();
    assert!(statement.starts_with("Not exhaustive:"), "{statement}");
    assert!(statement.contains("are a floor"), "{statement}");

    let spent = reach_with(
        &d.repo,
        &d.target,
        Limits::default(),
        &mut Edges(3),
        &contained,
    );
    assert_eq!(spent["state"], "bounded", "{spent:#}");
    assert_eq!(spent["bounds"][0]["which"], "edge_budget", "{spent:#}");
    assert!(spent["bounds"][0]["left"].as_u64().unwrap() >= 1);

    let unread = reach_with(
        &d.repo,
        &d.target,
        Limits {
            frontier_reads_max: 1,
            ..Limits::default()
        },
        &mut Unmetered,
        &contained,
    );
    assert_eq!(unread["state"], "bounded", "{unread:#}");
    assert_eq!(unread["members_total"], 4, "every caller is still found");
    let bound = &unread["bounds"][0];
    assert_eq!(bound["which"], "frontier_reads_max");
    assert_eq!(bound["left"], 4);
    assert_eq!(bound["at"][0]["name"], "left");
}

/// A closure is bounded by what it counts, never by how long the machine
/// took: read slowly, the same graph gives the same block. Each scan here
/// outlasts any clock a walk carries, and the block is the one an instant
/// reading gives, with no bound.
#[test]
fn a_slow_reading_of_a_small_reach_gives_the_same_block_as_a_fast_one() {
    let d = diamond(1.0);
    admit_diamond(&d);
    let fast = reach(&d.repo, &d.target);
    assert_eq!(fast["bounds"], json!([]), "{fast:#}");
    assert_eq!(fast["members_total"], 4);

    let slow_census = |focals: &[(&Entity, Vec<String>)]| {
        std::thread::sleep(std::time::Duration::from_millis(30));
        contained(focals)
    };
    let slow = reach_with(
        &d.repo,
        &d.target,
        Limits::default(),
        &mut WorkMeter::shipped(),
        &slow_census,
    );
    assert_eq!(slow, fast);
    // The budget of examined relations is a count, the same on any clock.
    let mut meter = WorkMeter::new(3);
    assert_eq!(meter.charge_edge(), None);
    assert_eq!(meter.charge_edge(), None);
    assert_eq!(meter.stop(), None);
    assert_eq!(meter.charge_edge(), Some("edge_budget"));
    assert_eq!(meter.stop(), Some("edge_budget"));
}

/// The one clock a closure keeps is a safety limit far above what a reading
/// needs. Reaching it bounds the block and says so in one plain sentence:
/// that it is a safety limit, what was left, and that asking again helps.
#[test]
fn the_safety_time_limit_is_far_out_and_says_so_plainly_when_reached() {
    assert!(SAFETY_TIME >= std::time::Duration::from_secs(60));
    assert_eq!(Limits::default().safety_time, SAFETY_TIME);
    let d = diamond(1.0);
    admit_diamond(&d);
    let stopped = reach_with(
        &d.repo,
        &d.target,
        Limits {
            safety_time: std::time::Duration::ZERO,
            ..Limits::default()
        },
        &mut Unmetered,
        &contained,
    );
    assert_eq!(stopped["state"], "bounded", "{stopped:#}");
    let bound = &stopped["bounds"][0];
    assert_eq!(bound["which"], SAFETY_TIME_LIMIT, "{stopped:#}");
    assert_eq!(
        bound["detail"],
        "Kin stopped this reading after 0 seconds, a safety limit far above what it normally \
         takes; asking again usually completes it. It stopped with 1 entity not yet expanded, \
         so callers beyond them are not in members"
    );
}

/// A block cut to fit a bounded reply says it was cut, as a bound.
#[test]
fn a_block_cut_to_fit_a_reply_says_so() {
    let d = diamond(1.0);
    admit_diamond(&d);
    let mut block = reach(&d.repo, &d.target);
    let whole = serde_json::to_vec(&block).unwrap().len();
    assert!(fit(&mut block.clone(), whole));

    let room = whole - 200;
    assert!(fit(&mut block, room));
    assert!(serde_json::to_vec(&block).unwrap().len() <= room);
    assert_eq!(block["state"], "bounded");
    let bound = block["bounds"]
        .as_array()
        .unwrap()
        .iter()
        .find(|bound| bound["which"] == "response_budget")
        .unwrap_or_else(|| panic!("{block:#}"));
    assert!(bound["left"].as_u64().unwrap() >= 1);
    assert!(block["members"].as_array().unwrap().len() < 4);

    let mut tiny = reach(&d.repo, &d.target);
    assert!(!fit(&mut tiny, 20));
}

/// The block every callers walk carries: the chain stops at its depth while
/// the closure goes on to the end, and a callees walk carries none.
#[test]
fn a_callers_trace_carries_the_closure_past_its_chain_depth() {
    let mut repo = Repo::new();
    let target = repo.function("target", "def target():\n    return 1\n");
    let mut previous = target.clone();
    let mut chain = Vec::new();
    for name in ["one", "two", "three", "four", "five"] {
        let callee = leaf(&previous.name).to_string();
        let caller = repo.function(name, &format!("def {name}():\n    return {callee}()\n"));
        repo.calls(&caller, &previous);
        chain.push((caller.clone(), previous.clone(), callee));
        previous = caller;
    }
    let sites: Vec<(&Entity, Vec<(&str, CallSiteState)>)> = chain
        .iter()
        .map(|(caller, callee, token)| {
            (
                caller,
                vec![(
                    token.as_str(),
                    CallSiteState::ProvenTarget { target: callee.id },
                )],
            )
        })
        .collect();
    repo.admit_ledgers(&sites, Vec::new());

    let args = HashMap::from([
        ("focal".to_string(), json!(target.id.to_string())),
        ("direction".to_string(), json!("callers")),
    ]);
    for handler in [
        crate::handlers::entities::handle_trace_data_flow::<InMemoryGraph>,
        crate::handlers::entities::handle_trace_data_flow_unpaged::<InMemoryGraph>,
    ] {
        let result = handler(&args, &repo.store).unwrap();
        let crate::types::ContentBlock::Text { text } = &result.content[0];
        let payload: Value = serde_json::from_str(text).unwrap();
        assert_eq!(payload["depth"], 3);
        assert_eq!(payload["total_steps"], 3, "{payload:#}");
        let block = &payload[REACH_KEY];
        assert_eq!(
            names(&block["members"]),
            vec!["one", "two", "three", "four", "five"],
            "{block:#}"
        );
        assert_eq!(block["state"], "closed", "{block:#}");
        assert_eq!(member(block, "five")["distance"], 5);
    }

    let callees = HashMap::from([
        ("focal".to_string(), json!(previous.id.to_string())),
        ("direction".to_string(), json!("calls")),
    ]);
    let result =
        crate::handlers::entities::handle_trace_data_flow_unpaged(&callees, &repo.store).unwrap();
    let crate::types::ContentBlock::Text { text } = &result.content[0];
    let payload: Value = serde_json::from_str(text).unwrap();
    assert!(payload.get(REACH_KEY).is_none(), "{payload:#}");
}

/// Each chain step quotes the call that proves it: an inward step from its
/// own body, a callee from its parent's.
#[test]
fn each_chain_step_quotes_the_call_that_proves_it() {
    let d = diamond(1.0);
    admit_diamond(&d);
    let chain_of = |focal: &Entity, direction: &str| -> Vec<Value> {
        let args = HashMap::from([
            ("focal".to_string(), json!(focal.id.to_string())),
            ("direction".to_string(), json!(direction)),
        ]);
        let result =
            crate::handlers::entities::handle_trace_data_flow_unpaged(&args, &d.repo.store)
                .unwrap();
        let crate::types::ContentBlock::Text { text } = &result.content[0];
        let payload: Value = serde_json::from_str(text).unwrap();
        let mut chain = payload["chain"].as_array().unwrap().clone();
        assert!(
            chain.iter().all(|step| step.get("code").is_some()),
            "{payload:#}"
        );
        quote_chain(&d.repo.store, focal, &mut chain, &d.repo.bodies);
        chain
    };
    let code_of = |chain: &[Value], name: &str| -> Value {
        chain
            .iter()
            .find(|step| step["entity_name"] == name)
            .map(|step| step["code"].clone())
            .unwrap_or_else(|| panic!("{name} is not in {chain:#?}"))
    };

    let callers = chain_of(&d.target, "callers");
    assert_eq!(code_of(&callers, "left"), "return target()");
    assert_eq!(code_of(&callers, "top"), "left()");
    assert_eq!(code_of(&callers, "outer"), "return top()");

    let callees = chain_of(&d.outer, "calls");
    assert_eq!(code_of(&callees, "top"), "return top()");
    assert_eq!(code_of(&callees, "right"), "return right()");
    assert_eq!(code_of(&callees, "target"), "return target()");
}

/// Each member names the dotted module it is declared in, read from the
/// graph's module records, and never a file path; the block names what its
/// members reach.
#[test]
fn every_member_names_its_module_and_never_a_path() {
    let d = diamond(1.0);
    admit_diamond(&d);
    let mut module = spanned_entity("app", FILE, LanguageId::Python, 0, "");
    module.kind = EntityKind::Module;
    module.id = EntityId::new();
    module.span.as_mut().unwrap().end_byte = d.repo.byte;
    d.repo.store.upsert_entity(&module).unwrap();
    let block = reach(&d.repo, &d.target);
    let expected = crate::semantic_owners::entity_module(&d.repo.store, &d.left)
        .expect("the fixture's module records name the module");
    assert!(!expected.contains('/'), "{expected}");
    for name in ["left", "right", "top", "outer"] {
        let row = member(&block, name);
        assert_eq!(row["module"], expected, "{name}: {row:#}");
    }
    assert_eq!(block["target"]["name"], "target");
    assert_eq!(block["target"]["entity_id"], d.target.id.to_string());
}

/// A place whose caller's text Kin could not read is counted by reason, not
/// listed and not counted as a place that could add a caller; a call that
/// spells the member's name stays listed. Neither makes the walk closed.
#[test]
fn unread_caller_text_is_folded_and_a_spelled_call_stays_listed() {
    let mut d = diamond(1.0);
    // A second definition a call spelling `target` could reach keeps the
    // spelled call from reaching the focal by its name alone.
    let _other = d.repo.define(
        "Other.target",
        EntityKind::Method,
        "def target(self):\n    return 2\n",
    );
    let maybe = d
        .repo
        .function("maybe", "def maybe(thing):\n    return thing.target(3)\n");
    let unread = d
        .repo
        .function("unread", "def unread(thing):\n    return thing.target(4)\n");
    let unresolved = CallSiteState::Unresolved {
        reason: UnresolvedReason::NoAnswer,
    };
    d.repo.admit_ledgers(
        &[
            (
                &d.left,
                vec![(
                    "target",
                    CallSiteState::ProvenTarget {
                        target: d.target.id,
                    },
                )],
            ),
            (
                &d.right,
                vec![(
                    "target",
                    CallSiteState::ProvenTarget {
                        target: d.target.id,
                    },
                )],
            ),
            (
                &d.top,
                vec![
                    ("left", CallSiteState::ProvenTarget { target: d.left.id }),
                    ("right", CallSiteState::ProvenTarget { target: d.right.id }),
                ],
            ),
            (
                &d.outer,
                vec![("top", CallSiteState::ProvenTarget { target: d.top.id })],
            ),
            (&maybe, vec![("target", unresolved.clone())]),
            (&unread, vec![("target", unresolved)]),
        ],
        Vec::new(),
    );
    // The reader holds every body but `unread`'s.
    d.repo.bodies.0.remove(&unread.id);
    let block = reach(&d.repo, &d.target);
    let frontier = block["frontier"].as_array().unwrap();
    assert!(
        frontier.iter().any(|row| row["in_name"] == "maybe"),
        "{block:#}"
    );
    assert!(
        frontier.iter().all(|row| row["in_name"] != "unread"),
        "{block:#}"
    );
    let folded = block[FRONTIER_FOLDED_KEY]
        .as_object()
        .unwrap_or_else(|| panic!("{block:#}"));
    let folded_unread = folded["by_reason"][FOLD_TEXT_NOT_READ].as_u64().unwrap();
    assert!(folded_unread > 0, "{block:#}");
    assert_eq!(folded["count"], folded_unread);
    // The folded places are not places that could add a caller, and the
    // listed one still is: the walk stays open either way.
    assert_eq!(block["frontier_total"], frontier.len(), "{block:#}");
    assert_eq!(block["state"], "open", "a fold never closes the walk");
    assert!(!names(&block["members"]).contains(&"unread".to_string()));
    let why = folded["why"].as_str().unwrap();
    assert!(why.contains("caller_text_not_read"), "{why}");
    assert!(!why.contains('/'), "{why}");
    // No withheld group points at another query.
    for row in block["frontier_withheld"].as_array().unwrap() {
        assert!(row.get("how").is_none(), "{row:#}");
    }
    assert!(!FRONTIER_WITHHELD_NOTE.contains("refs"));
}

#[test]
fn reach_page_view_merges_reading_and_rows_without_inventing_closure() {
    let mut page = json!({"chain":[], "reach":{"members":[{"id":"caller","via":"focal"}]},
        "readings":[{"key":"direction","value":"callers"},{"key":"reach","value":{
            "state":"open","bounds":[],"frontier_total":0,"frontier":[],
            "members_total":1,"also_referenced_total":0,
            "frontier_folded":{"count":4,"by_reason":{"caller_text_not_read":4}},
            "blind_spots":{"calls_through_values":7}}}]});
    let view = answer_view(page.as_object().unwrap()).unwrap();
    assert_eq!(view["members"][0]["via"], "focal");
    assert_eq!(view["frontier_folded"]["count"], 4);
    assert_eq!(view["blind_spots"]["calls_through_values"], 7);
    assert!(rows_delivered(&view));
    page["reach"]["members"] = json!([]);
    assert!(!rows_delivered(
        &answer_view(page.as_object().unwrap()).unwrap()
    ));
    page["reach"]["state"] = json!("closed");
    assert!(
        answer_view(page.as_object().unwrap()).is_none(),
        "conflicting states"
    );
    page["reach"].as_object_mut().unwrap().remove("state");
    page["readings"][1]["value"]
        .as_object_mut()
        .unwrap()
        .remove("bounds");
    assert!(
        answer_view(page.as_object().unwrap()).is_none(),
        "missing bounds"
    );
}

#[test]
fn a_both_direction_trace_cannot_use_reach_to_grade_its_callee_chain() {
    let page = json!({"direction":"both","reach":{"state":"closed","bounds":[],
        "members_total":0,"frontier_total":0}});
    assert!(answer_view(page.as_object().unwrap()).is_none());
}

/// Three unread graph sites are each considered against the focal and its
/// four reached members. Paths exclude the known outside site, but keep the
/// in-scope site and the site that no module record places.
fn folded_scope_fixture(mode: Option<crate::trace_scope::ScopeMode>) -> Value {
    use crate::trace_scope::{ScopeRequest, TraceScope};
    let mut d = diamond(1.0);
    d.repo.define("app", EntityKind::Module, "# app\n");
    d.repo.define_in_file(
        "outside",
        EntityKind::Module,
        "src/outside.py",
        "# outside\n",
    );
    let inside = d
        .repo
        .function("unread_inside", "def unread_inside():\n    target()\n");
    let outside = d.repo.define_in_file(
        "unread_outside",
        EntityKind::Function,
        "src/outside.py",
        "def unread_outside():\n    target()\n",
    );
    let unplaced = d.repo.define_in_file(
        "unread_unplaced",
        EntityKind::Function,
        "scripts/tool.py",
        "def unread_unplaced():\n    target()\n",
    );
    let unresolved = CallSiteState::Unresolved {
        reason: UnresolvedReason::NoAnswer,
    };
    d.repo.admit_ledgers(
        &[
            (
                &d.left,
                vec![(
                    "target",
                    CallSiteState::ProvenTarget {
                        target: d.target.id,
                    },
                )],
            ),
            (
                &d.right,
                vec![(
                    "target",
                    CallSiteState::ProvenTarget {
                        target: d.target.id,
                    },
                )],
            ),
            (
                &d.top,
                vec![
                    ("left", CallSiteState::ProvenTarget { target: d.left.id }),
                    ("right", CallSiteState::ProvenTarget { target: d.right.id }),
                ],
            ),
            (
                &d.outer,
                vec![("top", CallSiteState::ProvenTarget { target: d.top.id })],
            ),
            (&inside, vec![("target", unresolved.clone())]),
            (&outside, vec![("target", unresolved.clone())]),
            (&unplaced, vec![("target", unresolved)]),
        ],
        Vec::new(),
    );
    for entity in [&inside, &outside, &unplaced] {
        d.repo.bodies.0.remove(&entity.id);
    }
    let scope = mode.map(|mode| {
        TraceScope::resolve(&d.repo.store, &ScopeRequest::new("app", mode).unwrap()).unwrap()
    });
    read_with_in_scope(
        &d.repo.store,
        &contained,
        &|| &d.repo.bodies,
        &d.repo.bodies,
        &d.target,
        Limits::default(),
        &mut Unmetered,
        &mut Scans::default(),
        scope.as_ref(),
    )
}

#[test]
fn folded_path_observations_name_the_filter_and_keep_unplaced_callers() {
    let block = folded_scope_fixture(Some(crate::trace_scope::ScopeMode::Paths));
    assert_eq!(block["state"], "open", "{block:#}");
    assert_eq!(block["members_total"], 4, "{block:#}");
    assert_eq!(block["frontier_total"], 0, "{block:#}");
    assert_eq!(block["scope"]["frontier_outside"], 5, "{block:#}");
    let folded = &block[FRONTIER_FOLDED_KEY];
    assert_eq!(
        folded["count"], 10,
        "two graph sites considered under five members: {block:#}"
    );
    assert_eq!(folded["unit"], "observations", "{block:#}");
    assert_eq!(
        folded["scope"],
        json!({"mode":"paths", "module":"src.app",
        "known_outside_callers":"excluded", "unplaced_callers":"retained"}),
        "{block:#}"
    );
}

#[test]
fn folded_rows_observations_are_repository_wide() {
    let block = folded_scope_fixture(Some(crate::trace_scope::ScopeMode::Rows));
    assert_eq!(block["state"], "open", "{block:#}");
    assert_eq!(block["members_total"], 4, "{block:#}");
    let folded = &block[FRONTIER_FOLDED_KEY];
    assert_eq!(
        folded["count"], 15,
        "three graph sites considered under five members: {block:#}"
    );
    assert_eq!(folded["unit"], "observations", "{block:#}");
    assert_eq!(folded["scope"], json!({"mode":"repository"}), "{block:#}");
}

#[test]
fn folded_unscoped_observations_are_repository_wide() {
    let block = folded_scope_fixture(None);
    assert_eq!(block["state"], "open", "{block:#}");
    assert_eq!(block["members_total"], 4, "{block:#}");
    let folded = &block[FRONTIER_FOLDED_KEY];
    assert_eq!(
        folded["count"], 15,
        "three graph sites considered under five members: {block:#}"
    );
    assert_eq!(folded["unit"], "observations", "{block:#}");
    assert_eq!(folded["scope"], json!({"mode":"repository"}), "{block:#}");
}

/// The diamond's own proven sites, for a test that adds its own beside them.
fn diamond_sites(d: &Diamond) -> Vec<(&Entity, Vec<(&'static str, CallSiteState)>)> {
    vec![
        (
            &d.left,
            vec![(
                "target",
                CallSiteState::ProvenTarget {
                    target: d.target.id,
                },
            )],
        ),
        (
            &d.right,
            vec![(
                "target",
                CallSiteState::ProvenTarget {
                    target: d.target.id,
                },
            )],
        ),
        (
            &d.top,
            vec![
                ("left", CallSiteState::ProvenTarget { target: d.left.id }),
                ("right", CallSiteState::ProvenTarget { target: d.right.id }),
            ],
        ),
        (
            &d.outer,
            vec![("top", CallSiteState::ProvenTarget { target: d.top.id })],
        ),
    ]
}

fn unresolved() -> CallSiteState {
    CallSiteState::Unresolved {
        reason: UnresolvedReason::NoAnswer,
    }
}

/// A diamond plus `maybe`, whose one line calls `thing.target()` twice where
/// a second definition keeps the name from reaching the focal alone, with
/// `maybe` stored under `role`.
fn twice_unresolved(role: EntityRole) -> (Diamond, Entity) {
    let mut d = diamond(1.0);
    d.repo.define(
        "Other.target",
        EntityKind::Method,
        "def target(self):\n    return 2\n",
    );
    let mut maybe = d.repo.function(
        "maybe",
        "def maybe(thing):\n    return thing.target() or thing.target()\n",
    );
    maybe.role = role;
    d.repo.store.upsert_entity(&maybe).unwrap();
    let at = d
        .repo
        .entities
        .iter()
        .position(|e| e.id == maybe.id)
        .unwrap();
    d.repo.entities[at] = maybe.clone();
    let mut sites = diamond_sites(&d);
    sites.push((
        &maybe,
        vec![("target", unresolved()), ("target", unresolved())],
    ));
    d.repo.admit_ledgers(&sites, Vec::new());
    (d, maybe)
}

/// The same place recorded twice is one frontier row that says how many
/// sites it stands for, and it counts once in `frontier_total`.
#[test]
fn one_place_recorded_twice_is_one_frontier_row_with_its_site_count() {
    let (d, maybe) = twice_unresolved(EntityRole::Source);
    let block = reach(&d.repo, &d.target);
    let rows: Vec<&Value> = block["frontier"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["in"] == maybe.id.to_string())
        .collect();
    assert_eq!(rows.len(), 1, "{block:#}");
    assert_eq!(rows[0]["sites"], 2, "{block:#}");
    assert_eq!(rows[0]["code"], "return thing.target() or thing.target()");
    assert!(rows[0].get("in_role").is_none(), "{block:#}");
    assert_eq!(block["frontier_total"], 1, "{block:#}");
    assert_eq!(block["state"], "open", "a listed place keeps the walk open");
    assert!(block.get(FRONTIER_IN_TESTS_KEY).is_none(), "{block:#}");
    assert!(crate::trace_reach::rows_delivered(
        block.as_object().unwrap()
    ));
}

/// A place in code whose stored role is Test says so on its row and in the
/// block's count, and stays a place: the walk does not exclude tests.
#[test]
fn a_place_in_test_code_names_its_role_and_stays_listed() {
    let (d, maybe) = twice_unresolved(EntityRole::Test);
    let block = reach(&d.repo, &d.target);
    let row = block["frontier"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["in"] == maybe.id.to_string())
        .unwrap_or_else(|| panic!("{block:#}"));
    assert_eq!(row["in_role"], "test", "{block:#}");
    assert_eq!(block[FRONTIER_IN_TESTS_KEY], 1, "{block:#}");
    assert_eq!(block["frontier_total"], 1, "{block:#}");
    assert_eq!(block["state"], "open", "{block:#}");
    let statement = block["statement"].as_str().unwrap();
    assert!(statement.contains("sits in test code"), "{statement}");
}

/// The diamond, with `left` held as a value at each 1-based file line in
/// `held`, and the census of every other member contained, or unknown for
/// `unknown`.
fn held_left(
    held: Vec<u32>,
    unknown: Option<EntityId>,
) -> impl Fn(&[(&Entity, Vec<String>)]) -> Vec<FocalEscape> {
    move |focals: &[(&Entity, Vec<String>)]| -> Vec<FocalEscape> {
        focals
            .iter()
            .map(|(entity, _)| {
                if Some(entity.id) == unknown {
                    FocalEscape::Unknown {
                        reason: "record unavailable",
                    }
                } else if entity.name == "left" {
                    FocalEscape::Escapes {
                        reason: "held as a value",
                        residue: EscapeResidue {
                            values: held
                                .iter()
                                .map(|line| ResidueSite {
                                    file: FILE.to_string(),
                                    line: *line,
                                    kind: "argument",
                                    name: Some("left".to_string()),
                                    detail: None,
                                })
                                .collect(),
                            ..EscapeResidue::default()
                        },
                    }
                } else {
                    FocalEscape::contained(1)
                }
            })
            .collect()
    }
}

/// Every place the census lists a member held as a value is a served row,
/// and every member has a census: the block says the holders are complete,
/// so they, not a count of calls through values, are the places to check.
#[test]
fn value_holders_are_complete_when_every_held_place_is_served() {
    let mut d = diamond(1.0);
    let holder = d
        .repo
        .function("holder", "def holder():\n    register(left)\n");
    admit_diamond(&d);
    let line = holder.span.as_ref().unwrap().start_line + 2;
    let census = held_left(vec![line], None);
    let block = reach_with(
        &d.repo,
        &d.target,
        Limits::default(),
        &mut Unmetered,
        &census,
    );
    let held = &block["blind_spots"][VALUE_HOLDERS_KEY];
    assert_eq!(held["rows"], 1, "{block:#}");
    assert_eq!(held["complete"], true, "{block:#}");
    assert!(held.get("unlisted").is_none(), "{block:#}");
    assert!(held.get("in_members").is_none(), "{block:#}");
    let row = block["also_referenced_by"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == "holder")
        .unwrap_or_else(|| panic!("{block:#}"));
    assert_eq!(row["refs"][0]["value"], "argument", "{block:#}");
}

/// A member with no census, a held place in no entity, and a place inside a
/// member each keep or name what the holder rows alone cannot.
#[test]
fn value_holders_stay_incomplete_without_a_census_or_a_served_place() {
    let mut d = diamond(1.0);
    let holder = d
        .repo
        .function("holder", "def holder():\n    register(left)\n");
    admit_diamond(&d);
    let line = holder.span.as_ref().unwrap().start_line + 2;

    let census = held_left(vec![line], Some(d.right.id));
    let block = reach_with(
        &d.repo,
        &d.target,
        Limits::default(),
        &mut Unmetered,
        &census,
    );
    let held = &block["blind_spots"][VALUE_HOLDERS_KEY];
    assert_eq!(held["complete"], false, "a member has no census: {block:#}");
    assert!(block["blind_spots"]["uncensused"].is_object(), "{block:#}");

    // A place no entity holds and no edge names is not served by any row.
    let census = held_left(vec![line, 900], None);
    let block = reach_with(
        &d.repo,
        &d.target,
        Limits::default(),
        &mut Unmetered,
        &census,
    );
    let held = &block["blind_spots"][VALUE_HOLDERS_KEY];
    assert_eq!(held["unlisted"], 1, "{block:#}");
    assert_eq!(held["complete"], false, "{block:#}");

    // A member that holds another as a value is itself a place to check.
    let top_line = d.top.span.as_ref().unwrap().start_line + 2;
    let census = held_left(vec![line, top_line], None);
    let block = reach_with(
        &d.repo,
        &d.target,
        Limits::default(),
        &mut Unmetered,
        &census,
    );
    let held = &block["blind_spots"][VALUE_HOLDERS_KEY];
    assert_eq!(held["complete"], true, "{block:#}");
    assert_eq!(
        held["in_members"],
        json!([{"id": d.top.id.to_string(), "name": "top",
            "to": d.left.id.to_string(), "to_name": "left", "line": top_line}]),
        "{block:#}"
    );
}

/// The reach's own call census is measured when every member was read and
/// every caller that could spell a member's names has a current ledger. A
/// caller with none leaves it unmeasured, beside the frontier row that names
/// it.
#[test]
fn the_reach_says_whether_its_call_census_is_measured() {
    let d = diamond(1.0);
    admit_diamond(&d);
    let block = reach(&d.repo, &d.target);
    assert_eq!(block[CALLS_MEASURED_KEY], true, "{block:#}");

    let mut d = diamond(1.0);
    admit_diamond(&d);
    // Defined after the ledgers were admitted, so no ledger describes it.
    let late = d
        .repo
        .function("late", "def late():\n    return target()\n");
    let block = reach(&d.repo, &d.target);
    assert_eq!(block[CALLS_MEASURED_KEY], false, "{block:#}");
    assert!(
        block["frontier"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["in"] == late.id.to_string()),
        "{block:#}"
    );

    // A bound leaves members unread, so the census is not measured.
    let d = diamond(1.0);
    admit_diamond(&d);
    let limits = Limits {
        frontier_reads_max: 1,
        ..Limits::default()
    };
    let block = reach_with(&d.repo, &d.target, limits, &mut Unmetered, &contained);
    assert_eq!(block["state"], "bounded", "{block:#}");
    assert_eq!(block[CALLS_MEASURED_KEY], false, "{block:#}");
}

/// A small repository in three packages, with their module records: `flask`
/// declares the focal `route` and `get`, which calls it; `shop` declares
/// `index`, which calls it; and `tests` declares two tests, one calling it
/// and one calling that test.
struct Packages {
    repo: Repo,
    route: Entity,
}

fn packages(unresolved_in_shop: bool) -> Packages {
    let mut repo = Repo::new();
    for (name, file) in [
        ("flask", "flask/__init__.py"),
        ("app", "flask/app.py"),
        ("shop", "shop/__init__.py"),
        ("views", "shop/views.py"),
        ("test_app", "tests/test_app.py"),
    ] {
        repo.define_in_file(name, EntityKind::Module, file, "# module\n");
    }
    let route = repo.define_in_file(
        "route",
        EntityKind::Function,
        "flask/app.py",
        "def route():\n    return 1\n",
    );
    // A second `route` keeps a call that only spells the name from being
    // promoted to a caller of the focal.
    repo.define_in_file(
        "Other.route",
        EntityKind::Method,
        "flask/app.py",
        "def route(self):\n    return 2\n",
    );
    let get = repo.define_in_file(
        "get",
        EntityKind::Function,
        "flask/app.py",
        "def get():\n    return route()\n",
    );
    let index = repo.define_in_file(
        "index",
        EntityKind::Function,
        "shop/views.py",
        "def index():\n    return route()\n",
    );
    let maybe = repo.define_in_file(
        "maybe",
        EntityKind::Function,
        "shop/views.py",
        "def maybe(thing):\n    return thing.route()\n",
    );
    let mut tests = Vec::new();
    for (name, body) in [
        ("test_route", "def test_route():\n    return route()\n"),
        ("test_more", "def test_more():\n    return test_route()\n"),
    ] {
        let mut test = repo.define_in_file(name, EntityKind::Function, "tests/test_app.py", body);
        test.role = EntityRole::Test;
        repo.store.upsert_entity(&test).unwrap();
        let at = repo.entities.iter().position(|e| e.id == test.id).unwrap();
        repo.entities[at] = test.clone();
        tests.push(test);
    }
    repo.calls(&get, &route);
    repo.calls(&index, &route);
    repo.calls(&tests[0], &route);
    repo.calls(&tests[1], &tests[0]);
    let proven = |target: &Entity| CallSiteState::ProvenTarget { target: target.id };
    let mut sites = vec![
        (&get, vec![("route", proven(&route))]),
        (&index, vec![("route", proven(&route))]),
        (&tests[0], vec![("route", proven(&route))]),
        (&tests[1], vec![("test_route", proven(&tests[0]))]),
    ];
    if unresolved_in_shop {
        sites.push((
            &maybe,
            vec![(
                "route",
                CallSiteState::Unresolved {
                    reason: UnresolvedReason::NoAnswer,
                },
            )],
        ));
    }
    repo.admit_ledgers(&sites, Vec::new());
    Packages { repo, route }
}

/// A reading that leaves tests out never follows, lists or grades test
/// code: a test is no member, the test that calls it is not reached, a call
/// in a test that no resolver settled is no place left to check, and the
/// block counts what it left out. The same graph read with tests keeps them
/// all, so the count is what the reader would otherwise have had to weigh.
#[test]
fn a_reading_without_tests_leaves_them_out_of_members_places_and_the_verdict() {
    let mut p = packages(false);
    // A test that spells the focal's name in a call nothing settled.
    let mut loose = p.repo.define_in_file(
        "test_loose",
        EntityKind::Function,
        "tests/test_app.py",
        "def test_loose(thing):\n    return thing.route()\n",
    );
    loose.role = EntityRole::Test;
    // Its recorded text, as a parse keeps it: what says it spells the name.
    loose.metadata.extra.insert(
        kin_parser::extract::EMBEDDING_BODY_PREVIEW_KEY.into(),
        json!("def test_loose(thing): return thing.route()"),
    );
    p.repo.store.upsert_entity(&loose).unwrap();
    let at = p
        .repo
        .entities
        .iter()
        .position(|entity| entity.id == loose.id)
        .unwrap();
    p.repo.entities[at] = loose.clone();
    p.repo.admit_ledgers(
        &[(
            &loose,
            vec![(
                "route",
                CallSiteState::Unresolved {
                    reason: UnresolvedReason::NoAnswer,
                },
            )],
        )],
        Vec::new(),
    );
    let read = |include_tests: bool| {
        read_with(
            &p.repo.store,
            &contained,
            &|| &p.repo.bodies,
            &p.repo.bodies,
            &p.route,
            Limits::default(),
            &mut Unmetered,
            &mut Scans::reading_tests(include_tests),
        )
    };

    let with_tests = read(true);
    assert_eq!(
        names(&with_tests["members"]),
        ["get", "index", "test_route", "test_more"],
        "{with_tests:#}"
    );
    assert_eq!(with_tests["state"], "open", "{with_tests:#}");
    assert_eq!(with_tests["frontier"][0]["in_name"], "test_loose");
    assert!(with_tests.get(TESTS_EXCLUDED_KEY).is_none());

    let without = read(false);
    assert_eq!(names(&without["members"]), ["get", "index"], "{without:#}");
    assert_eq!(without["state"], "closed", "{without:#}");
    assert_eq!(without["frontier"], json!([]), "{without:#}");
    assert_eq!(without["frontier_total"], 0);
    assert_eq!(without["bounds"], json!([]));
    // `test_route` calls the focal and `test_loose` could by name: two call
    // sites in two tests. `test_more` only calls a test, which was never
    // reached.
    assert_eq!(
        without[TESTS_EXCLUDED_KEY],
        json!({"call_sites": 2, "callers": 2}),
        "{without:#}"
    );
    let statement = without["statement"].as_str().unwrap();
    assert!(
        statement.starts_with("Closed: under this policy these 2 entities are every one"),
        "{statement}"
    );
    assert!(
        statement.ends_with(
            "Test code was left out, as asked: 2 call sites in 2 test entities are not \
             followed or listed."
        ),
        "{statement}"
    );
    // No package of tests is reported for a reading that read none.
    assert!(
        without[PACKAGES_KEY]
            .as_array()
            .is_none_or(|packages| packages.iter().all(|package| package["module"] != "tests")),
        "{without:#}"
    );
}

/// Members in more than one package are listed package by package, the
/// focal's own first and the tests last, and the block says how far each
/// package was read: a question about the callers in one package is answered
/// by that package's rows and its state, whatever the others add up to.
#[test]
fn members_are_listed_by_package_and_each_package_says_how_far_it_was_read() {
    let p = packages(false);
    let block = reach(&p.repo, &p.route);
    assert_eq!(block["state"], "closed", "{block:#}");
    assert_eq!(
        names(&block["members"]),
        ["get", "index", "test_route", "test_more"],
        "{block:#}"
    );
    assert_eq!(
        block[PACKAGES_KEY],
        json!([
            {"module": "flask", "members": 1, "state": "closed"},
            {"module": "shop", "members": 1, "state": "closed"},
            {"module": "tests", "members": 2, "state": "closed", "role": "test"},
        ]),
        "{block:#}"
    );

    // The read bound falls on the tests: code that is not a test is read
    // first, so the packages it sits in are read whole.
    let bounded = reach_with(
        &p.repo,
        &p.route,
        Limits {
            frontier_reads_max: 3,
            ..Limits::default()
        },
        &mut Unmetered,
        &contained,
    );
    assert_eq!(bounded["state"], "bounded", "{bounded:#}");
    assert_eq!(bounded["members_total"], 4, "every caller is still found");
    let bound = &bounded["bounds"][0];
    assert_eq!(bound["which"], "frontier_reads_max");
    assert_eq!(bound["left"], 2);
    assert_eq!(
        names(&bound["at"]),
        ["test_route", "test_more"],
        "{bounded:#}"
    );
    assert_eq!(
        bounded[PACKAGES_KEY],
        json!([
            {"module": "flask", "members": 1, "state": "closed"},
            {"module": "shop", "members": 1, "state": "closed"},
            {"module": "tests", "members": 2, "state": "unread", "unread": 2, "role": "test"},
        ]),
        "{bounded:#}"
    );

    // A bound that cut the members themselves leaves no package closed.
    let capped = reach_with(
        &p.repo,
        &p.route,
        Limits {
            members_max: 3,
            ..Limits::default()
        },
        &mut Unmetered,
        &contained,
    );
    assert!(
        capped[PACKAGES_KEY]
            .as_array()
            .unwrap()
            .iter()
            .all(|package| package["state"] == "bounded"),
        "{capped:#}"
    );
}

/// A place that could add a caller counts against the package it sits in
/// and no other: an unresolved call in `shop` leaves `flask` closed.
#[test]
fn a_place_counts_against_the_package_it_sits_in() {
    let p = packages(true);
    let block = reach(&p.repo, &p.route);
    assert_eq!(block["state"], "open", "{block:#}");
    assert_eq!(block["frontier"][0]["in_name"], "maybe", "{block:#}");
    assert_eq!(
        block[PACKAGES_KEY],
        json!([
            {"module": "flask", "members": 1, "state": "closed"},
            {"module": "shop", "members": 1, "state": "open", "places": 1},
            {"module": "tests", "members": 2, "state": "closed", "role": "test"},
        ]),
        "{block:#}"
    );
    // One package holds every member: there is nothing to tell apart.
    let d = diamond(1.0);
    admit_diamond(&d);
    assert!(reach(&d.repo, &d.target).get(PACKAGES_KEY).is_none());
}
