// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Receivers typed by the value-origin facts adapters record.
//!
//! A call written `ctx.pop()` reaches the linker as the bare leaf `pop` with
//! the receiver `ctx`. When a language server could not type `ctx`, the only
//! tier left was the bare-name one, which in a repository with several `pop`
//! methods rightly binds nothing. The adapter often knows more: `ctx` is bound
//! once, to what `app.test_request_context()` returns, and `app` is the value
//! a test framework injects by name. This module follows such a chain through
//! the facts in [`kin_parser::value_origins`] to one class, then asks that
//! class for the method, exactly as the declared-receiver tiers do.
//!
//! Every link of a sound chain is something the source writes down: a
//! declared type, a constructor call, a declared return type, the value a
//! plain function's `return`s agree on, an attribute a class declares a type
//! for, or a provider found under the lookup rule its framework documents.
//! Such a chain is
//! stamped [`RECEIVER_ORIGIN_CONFIDENCE`]. One link has no such footing: a
//! method call on a receiver no fact types, settled because exactly one
//! method in the repository carries that name. A dependency, a mock or any
//! class outside the graph can define the same name, so a chain through that
//! link is stamped [`RECEIVER_ORIGIN_NAME_CONFIDENCE`], a `name_only`
//! candidate that no count of proven callers ever includes.
//!
//! A sound chain can also end at a type no file of the repository declares:
//! a dictionary literal, pytest's `recwarn` fixture, a `Map` the caller
//! constructs. Then the call reaches no repository declaration at all, and
//! [`prove_receiver_outside`] says so, provided no class the repository
//! declares could be the value's class and still hold the method: the adapter
//! states how far the value's class may stray from the named type
//! ([`ExternalReach`]), and this module compares owners. A member a language
//! may run without a call spelling it is never settled this way.
//!
//! A receiver may read attributes off the name a fact binds: `app.json` is
//! what `json` holds on the class `app` reaches, by the declaration that
//! class, or the one base that declares it, makes for its instances
//! ([`kin_parser::value_origins::ScopeFacts::members`]).
//!
//! A provider lookup is made for a scope: the caller whose call the chain
//! types. A provider's own requests are looked up for that same scope, from
//! where it sits, since a framework that injects by name resolves every
//! provider of one request from the requester: a nearer provider of a name
//! the farther one requests is the one it receives. Only a request for the
//! provider's own name starts at the provider, and reaches the next one out.
//! A name the requesting scope receives some other way (a parameter a test
//! parametrizes) is answered by no provider at all.
//!
//! A type name the facts spell can be a default import, `import Store from
//! './store'`, which means whatever the module's default export names. The
//! adapter records that name on the module
//! ([`kin_parser::value_origins::ScopeFacts::default_export`]) and the walk
//! follows it, so `createContext<Store>(...)` in one file reaches the
//! `export default class Store` of another.
//!
//! Every link the walk takes is also kept as an [`OriginStep`] naming the
//! entity or file that declares it. A bound edge carries them on a
//! [`RECEIVER_ORIGIN_STEPS_RULE`] record, so a reader can show the proof
//! without rebuilding it. A walk that stops short of a method says which
//! link it could not establish ([`MissingStep`]), and the coverage
//! certificate keeps that for the call's site.
//!
//! Nothing here knows a language or a framework. The adapters name providers,
//! lookups and external types with opaque strings; the linker only matches
//! them.

use std::collections::HashSet;
use std::sync::Arc;

use kin_model::{EntityId, Relation, RelationEvidence};
use kin_parser::value_origins::{
    DeclaredReturn, ExternalReach, ProviderLookup, ProviderModule, ScopeFacts, ValueOrigin,
    ValueProvider,
};
use serde::{Deserialize, Serialize};

use crate::resolution::{RECEIVER_ORIGIN_CONFIDENCE, RECEIVER_ORIGIN_NAME_CONFIDENCE};

/// Evidence rule a receiver-origin edge carries, on a span-free record whose
/// token spells the chain that typed the receiver.
pub const RECEIVER_ORIGIN_RULE: &str = "receiver_origin_chain_v1";

/// Evidence rule of the span-free record beside it whose token holds the
/// same chain as JSON, one [`OriginStep`] per link, each naming the entity or
/// file that declares it.
pub const RECEIVER_ORIGIN_STEPS_RULE: &str = "receiver_origin_steps_v1";

/// Whether an edge was bound by this tier.
pub fn is_receiver_origin_edge(relation: &Relation) -> bool {
    relation
        .evidence
        .iter()
        .any(|record| record.parser_rule.as_deref() == Some(RECEIVER_ORIGIN_RULE))
}

/// The chain a receiver-origin edge recorded, when it carries one.
pub fn receiver_origin_chain(relation: &Relation) -> Option<&str> {
    relation
        .evidence
        .iter()
        .find(|record| record.parser_rule.as_deref() == Some(RECEIVER_ORIGIN_RULE))
        .and_then(|record| record.token.as_deref())
}

/// How deep a chain may nest before the walk gives up. Real chains are two or
/// three links; the cap only stops a cycle the facts could spell.
const CHAIN_DEPTH_CAP: usize = 12;

/// How many classes the subtype check visits before it stops and answers that
/// a repository class may be the value's. Real hierarchies under one method
/// name are a handful of classes.
const HIERARCHY_VISIT_CAP: usize = 256;

/// How many modules a default import is followed through, each handing its
/// default export on from the next.
const DEFAULT_EXPORT_HOPS: usize = 4;

/// How many attributes a receiver may read off the name a fact binds
/// (`app.json` reads one). Each one is a declaration the walk must find.
const MEMBER_HOPS_CAP: usize = 3;

/// How many module-level bindings a chain follows, each naming the next: a
/// name a function reads off its module, an import of it, a re-export.
const MODULE_BINDINGS_CAP: usize = 8;

/// How many plugin modules a provider lookup reads, transitively, before it
/// answers that a plugin it could not read may provide the name.
const PLUGIN_MODULES_CAP: usize = 32;

/// A declaration the repository holds under a member name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Member {
    pub(crate) id: EntityId,
    pub(crate) file: String,
    /// The path of the class or object that owns it, as its name spells it
    /// (`AppContext` for `AppContext.pop`), or `None` for a free function.
    pub(crate) owner: Option<String>,
}

/// What the evaluator asks of the linker it runs inside. The batch and the
/// incremental linker each answer from their own indexes, so both bind the
/// same call the same way.
pub(crate) trait OriginWorld {
    /// The facts an entity carries.
    fn facts(&self, entity: EntityId) -> Option<Arc<ScopeFacts>>;
    /// The file an entity is declared in.
    fn entity_file(&self, entity: EntityId) -> Option<String>;
    /// An entity's own name.
    fn entity_name(&self, entity: EntityId) -> Option<String>;
    /// The class a type name written in `file` names, as (file, class).
    fn locate_class(&self, file: &str, name: &str) -> Option<(String, String)>;
    /// The method `class` in `file` declares or inherits.
    fn resolve_method(&self, file: &str, class: &str, method: &str) -> Option<EntityId>;
    /// The free function a name written in `file` calls: one the file
    /// declares, or the one an import of it reaches.
    fn resolve_callable(&self, file: &str, name: &str) -> Option<EntityId>;
    /// Every provider declared in `file`, with the entity declaring it, or
    /// `None` when this linker cannot read that file's facts. A lookup that
    /// meets an unreadable file stops there: a provider it cannot see may be
    /// the nearest one.
    fn providers_in(&self, file: &str) -> Option<Vec<(EntityId, ValueProvider)>>;
    /// Whether `file` declares an entity named `name`.
    fn file_declares(&self, file: &str, name: &str) -> bool;
    /// Whether the repository holds `path`.
    fn known_file(&self, path: &str) -> bool;
    /// The (file, original name) an import of `local` in `file` binds.
    fn import_binding(&self, file: &str, local: &str) -> Option<(String, String)>;
    /// The one method in the repository named `method`, when exactly one
    /// entity of any kind carries that leaf name.
    fn unique_method(&self, method: &str) -> Option<EntityId>;
    /// Whether a name-based inference may connect these two entities.
    fn compatible(&self, caller: EntityId, target: EntityId) -> bool;
    /// Every declaration the repository holds whose own name is `member`,
    /// free functions and members of any owner alike.
    fn members_named(&self, member: &str) -> Vec<Member>;
    /// The base names class `class` of `file` declares (empty when it
    /// declares none), or `None` when the repository declares no class of
    /// that name there.
    fn class_bases(&self, file: &str, class: &str) -> Option<Vec<String>>;
    /// Every class the repository declares with a base that locates to
    /// `class` of `file`, as (file, class).
    fn subclasses(&self, file: &str, class: &str) -> Vec<(String, String)>;
    /// Whether the repository declares any class-like entity whose own name
    /// is `leaf`.
    fn declares_class_named(&self, leaf: &str) -> bool;
    /// The binding `file`'s default export names, from the facts of the
    /// file's module entity (see
    /// [`kin_parser::value_origins::ScopeFacts::default_export`]).
    fn default_export(&self, file: &str) -> Option<String>;
    /// The entity `file` declares under `name`, for the evidence a step
    /// names.
    fn declaration(&self, file: &str, name: &str) -> Option<EntityId>;
    /// Where an import of the plain `local` in `file` leads, as (file,
    /// original name), when the module it names resolves under no ordinary
    /// source root but under a project nested in the repository: the nearest
    /// directory above the importer that lays out the module's top-level
    /// package. That is where a test beside a nested project's package finds
    /// it, once the project is installed or its directory is on the path.
    fn nested_import(&self, file: &str, local: &str) -> Option<(String, String)>;
    /// The repository file a module name written in `file` names, as an
    /// import there would resolve it.
    fn module_file(&self, file: &str, module: &str) -> Option<String>;
    /// The modules `file`'s module registers providers from (see
    /// [`kin_parser::value_origins::ScopeFacts::provider_modules`]), or
    /// `None` when this linker cannot read that file's facts.
    fn provider_modules(&self, file: &str) -> Option<Vec<ProviderModule>>;
    /// Whether no file of the repository can supply the module that the
    /// dotted name `qualified`, imported in `file`, starts with. `false`
    /// wherever the language's import rule or the tree leaves it open.
    fn outside_module(&self, file: &str, qualified: &str) -> bool;
    /// The module entity of `file` and the facts it carries: what the module
    /// binds each of its own names to (see
    /// [`kin_parser::value_origins::ScopeFacts::bindings`]). `None` when the
    /// file has none or this linker cannot read them.
    fn module_scope(&self, file: &str) -> Option<(EntityId, Arc<ScopeFacts>)>;
    /// Every class the repository declares that names the imported dotted
    /// name `qualified` as a base (see
    /// [`kin_parser::value_origins::ScopeFacts::base_origins`]), as (file,
    /// class), among the classes whose facts this linker can read.
    fn classes_based_on(&self, qualified: &str) -> Vec<(String, String)>;
}

/// What one link of a receiver chain is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OriginStepKind {
    /// A name a scope binds exactly once: the receiver itself, or a local
    /// another binding reads.
    Binding,
    /// A provider a lookup found by name, whose value the binding holds.
    Provider,
    /// What a lookup falls back to when the repository provides the name
    /// nowhere: a framework's own built-in provider.
    Fallback,
    /// A class the value is an instance of, because a type names it, a
    /// constructor makes it, or a declared return says so.
    Type,
    /// A function whose declared return, or the value it supplies, types the
    /// value.
    Call,
    /// A method whose declared return types the value.
    Returns,
    /// A method settled by its name alone, because no fact typed the
    /// receiver it was called on.
    Guess,
    /// A type no file of the repository declares.
    External,
    /// An attribute read off a value, whose declaration in the value's class
    /// or one of its bases types what it holds.
    Member,
    /// The method the call reaches.
    Method,
}

impl OriginStepKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Binding => "binding",
            Self::Provider => "provider",
            Self::Fallback => "fallback",
            Self::Type => "type",
            Self::Call => "call",
            Self::Returns => "returns",
            Self::Guess => "guess",
            Self::External => "external",
            Self::Member => "member",
            Self::Method => "method",
        }
    }
}

/// One link of the chain that typed a receiver, as the graph can address it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OriginStep {
    pub step: OriginStepKind,
    /// The name the link binds, reads or resolves to: `store`,
    /// `StoreContext`, `Store`, `Store.containsElement`.
    pub name: String,
    /// How a type link was reached: `declared`, `constructed`, `returned`
    /// or `supplied`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
    /// The namespace a provider or fallback link answers in, as the adapter
    /// names it (`react_context`, `pytest_fixture`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// The entity that declares the link, when the graph holds one: the
    /// scope that binds a name, the declaration a provider names, the class,
    /// the function or the method.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entity: Option<EntityId>,
    /// The file the link is declared in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
}

impl OriginStep {
    fn new(step: OriginStepKind, name: impl Into<String>) -> Self {
        Self {
            step,
            name: name.into(),
            via: None,
            kind: None,
            entity: None,
            file: None,
        }
    }

    fn via(mut self, via: &str) -> Self {
        self.via = Some(via.to_string());
        self
    }

    fn kind(mut self, kind: &str) -> Self {
        self.kind = Some(kind.to_string());
        self
    }

    fn at(mut self, entity: Option<EntityId>, file: Option<&str>) -> Self {
        self.entity = entity;
        self.file = file.map(str::to_string);
        self
    }
}

/// The link a chain could not establish, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MissingStep {
    /// The kind of link that was needed.
    pub step: OriginStepKind,
    /// The name it had to resolve.
    pub name: String,
    /// Why it did not, as a stable code: `type_not_located`,
    /// `provider_value_unknown`, `method_not_found` and the like.
    pub reason: String,
    /// The namespace a provider link had to answer in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// The file the name was read in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
}

/// A receiver whose facts bind it, but whose chain stops short of a method.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OriginGap {
    /// The links established before the gap, the receiver's binding first.
    pub(crate) steps: Vec<OriginStep>,
    pub(crate) missing: MissingStep,
}

/// A call through a typed receiver, bound.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct OriginBinding {
    pub(crate) target: EntityId,
    pub(crate) confidence: f32,
    pub(crate) chain: String,
    /// The same chain, one addressable link per step: the receiver's
    /// binding first and the method last.
    pub(crate) steps: Vec<OriginStep>,
}

/// A call proved to reach no declaration of the repository, with the chain
/// that typed its receiver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OutsideProof {
    pub(crate) chain: String,
    /// The type outside the repository whose member the call or read gets,
    /// as the chain names it: the value's own type, or the library base a
    /// repository class takes the member from.
    pub(crate) owner: String,
}

/// One link as the evaluator holds it: the text the audit chain spells for
/// it, if any, and the addressable step.
#[derive(Debug, Clone)]
struct Link {
    text: Option<String>,
    step: OriginStep,
}

impl Link {
    fn spelled(text: String, step: OriginStep) -> Self {
        Self {
            text: Some(text),
            step,
        }
    }

    fn silent(step: OriginStep) -> Self {
        Self { text: None, step }
    }
}

/// A class a chain reached, with how it got there.
struct Typed {
    file: String,
    class: String,
    sound: bool,
    links: Vec<Link>,
}

/// A type outside the repository a chain reached.
struct Foreign {
    type_name: String,
    reach: ExternalReach,
    own_members: Vec<String>,
    sound: bool,
    links: Vec<Link>,
}

/// Where a chain ends.
enum Reached {
    Class(Typed),
    External(Foreign),
}

impl Reached {
    fn prefixed(mut self, link: Link) -> Self {
        match &mut self {
            Self::Class(typed) => typed.links.insert(0, link),
            Self::External(foreign) => foreign.links.insert(0, link),
        }
        self
    }
}

/// Where a chain stopped: the links it established, and the one it could
/// not.
struct Stop {
    links: Vec<Link>,
    missing: MissingStep,
}

impl Stop {
    fn at(step: OriginStepKind, name: &str, reason: &str, file: Option<&str>) -> Self {
        Self {
            links: Vec::new(),
            missing: MissingStep {
                step,
                name: name.to_string(),
                reason: reason.to_string(),
                kind: None,
                file: file.map(str::to_string),
            },
        }
    }

    fn after(mut self, links: Vec<Link>) -> Self {
        let mut all = links;
        all.append(&mut self.links);
        self.links = all;
        self
    }

    fn prefixed(mut self, link: Link) -> Self {
        self.links.insert(0, link);
        self
    }
}

type Walk = Result<Reached, Stop>;

fn spell(links: &[Link]) -> Vec<String> {
    links.iter().filter_map(|link| link.text.clone()).collect()
}

fn steps_of(links: Vec<Link>) -> Vec<OriginStep> {
    links.into_iter().map(|link| link.step).collect()
}

/// Where a provider lookup made on a scope's behalf starts: the scope whose
/// request the chain serves. A fixture's own parameters are found for the
/// test that requested the fixture, from where that test sits, because pytest
/// resolves every fixture of one test's request from the test.
#[derive(Debug, Clone)]
struct Anchor {
    entity: EntityId,
    file: String,
    owner: Option<String>,
    owner_path: Option<String>,
}

#[derive(Clone)]
struct Scope {
    entity: EntityId,
    file: String,
    /// The leaf of the owner's name (`TestThing`), which providers scope by.
    owner: Option<String>,
    /// The owner's whole path (`Outer.TestThing`), which the class index
    /// keys by.
    owner_path: Option<String>,
    /// The scope the chain is evaluated for, which a lookup of another name
    /// starts from.
    anchor: Anchor,
    /// The name this scope was found under when it is a provider evaluated
    /// for a lookup. A provider that requests its own name receives the next
    /// one out from itself, so that lookup starts here rather than at the
    /// anchor.
    provides: Option<String>,
}

impl Scope {
    /// The scope of `entity`, evaluated for itself.
    fn of(world: &dyn OriginWorld, entity: EntityId, file: &str) -> Self {
        let name = world.entity_name(entity);
        let owner = name.as_deref().and_then(owner_of);
        let owner_path = name.as_deref().and_then(owner_path_of);
        Self {
            entity,
            file: file.to_string(),
            anchor: Anchor {
                entity,
                file: file.to_string(),
                owner: owner.clone(),
                owner_path: owner_path.clone(),
            },
            owner,
            owner_path,
            provides: None,
        }
    }

    /// The scope of a provider found under `name` for a lookup `requester`
    /// made, which serves the requester's anchor.
    fn provider(
        world: &dyn OriginWorld,
        entity: EntityId,
        file: &str,
        owner: Option<String>,
        name: &str,
        requester: &Scope,
    ) -> Self {
        Self {
            entity,
            file: file.to_string(),
            owner,
            owner_path: world.entity_name(entity).as_deref().and_then(owner_path_of),
            anchor: requester.anchor.clone(),
            provides: Some(name.to_string()),
        }
    }

    /// Where a lookup of `name` made in this scope starts, and the entity it
    /// never answers with.
    fn lookup_origin(&self, name: &str) -> Anchor {
        if self.provides.as_deref() == Some(name) {
            Anchor {
                entity: self.entity,
                file: self.file.clone(),
                owner: self.owner.clone(),
                owner_path: self.owner_path.clone(),
            }
        } else {
            self.anchor.clone()
        }
    }
}

/// One step a receiver takes off the value its key binds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hop<'a> {
    /// `.json`: an attribute read off the value.
    Attribute(&'a str),
    /// `.test_cli_runner(...)`: what calling a method of the value returns.
    Call(&'a str),
}

fn is_name(segment: &str) -> bool {
    let mut chars = segment.chars();
    chars
        .next()
        .is_some_and(|first| first.is_alphabetic() || first == '_' || first == '$')
        && chars.all(|ch| ch.is_alphanumeric() || ch == '_' || ch == '$')
}

/// `text` split at every `.` outside parentheses and string literals, or
/// `None` when it holds a subscript, a literal or brackets outside a call's
/// arguments, or does not balance.
fn split_receiver(text: &str) -> Option<Vec<&str>> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut start = 0;
    for (at, ch) in text.char_indices() {
        if let Some(open) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == open {
                quote = None;
            }
            continue;
        }
        match ch {
            '\'' | '"' if depth > 0 => quote = Some(ch),
            '(' => depth += 1,
            ')' => depth = depth.checked_sub(1)?,
            '.' if depth == 0 => {
                parts.push(&text[start..at]);
                start = at + 1;
            }
            '\'' | '"' | '[' | ']' | '{' | '}' if depth == 0 => return None,
            _ => {}
        }
    }
    if depth != 0 || quote.is_some() {
        return None;
    }
    parts.push(&text[start..]);
    Some(parts)
}

/// A name, or a name called once with arguments that close the segment:
/// `json`, `test_cli_runner()`, `get("/", follow=True)`.
fn receiver_segment(segment: &str) -> Option<(&str, bool)> {
    match segment.find('(') {
        None => is_name(segment).then_some((segment, false)),
        Some(open) => {
            let name = &segment[..open];
            if !is_name(name) || !segment.ends_with(')') {
                return None;
            }
            // The parenthesis that opens the arguments must be the one that
            // closes the segment, not `f()(x)`.
            let mut depth = 0usize;
            let mut quote: Option<char> = None;
            let mut escaped = false;
            for (at, ch) in segment[open..].char_indices() {
                if let Some(opened) = quote {
                    if escaped {
                        escaped = false;
                    } else if ch == '\\' {
                        escaped = true;
                    } else if ch == opened {
                        quote = None;
                    }
                    continue;
                }
                match ch {
                    '\'' | '"' => quote = Some(ch),
                    '(' => depth += 1,
                    ')' => {
                        depth = depth.checked_sub(1)?;
                        if depth == 0 && open + at + 1 != segment.len() {
                            return None;
                        }
                    }
                    _ => {}
                }
            }
            Some((name, true))
        }
    }
}

/// A receiver split into the key a fact may bind (a plain name, or one field
/// of the scope's own instance, `self.<field>`, `this.<field>` or
/// `$this-><field>`) and the steps taken off it, in order, up to
/// [`MEMBER_HOPS_CAP`] of them: attributes read (`app.json`) and methods
/// called (`app.test_cli_runner()`). `None` for anything else: a call of the
/// key itself, a subscript, a literal.
fn receiver_path(receiver: &str) -> Option<(&str, Vec<Hop<'_>>)> {
    if let Some(field) = receiver.strip_prefix("$this->") {
        return is_name(field).then(|| (receiver, Vec::new()));
    }
    let segments = split_receiver(receiver)?
        .into_iter()
        .map(receiver_segment)
        .collect::<Option<Vec<(&str, bool)>>>()?;
    // `self.client` and `this.store` are keys of their own; `self` and
    // `this` alone are not values a fact binds.
    let key_segments = if matches!(segments[0].0, "self" | "this") {
        2
    } else {
        1
    };
    if segments.len() < key_segments
        || segments.len() - key_segments > MEMBER_HOPS_CAP
        || segments[..key_segments].iter().any(|(_, called)| *called)
    {
        return None;
    }
    let key_len = segments[..key_segments]
        .iter()
        .map(|(name, _)| name.len())
        .sum::<usize>()
        + key_segments
        - 1;
    let hops = segments[key_segments..]
        .iter()
        .map(|&(name, called)| {
            if called {
                Hop::Call(name)
            } else {
                Hop::Attribute(name)
            }
        })
        .collect();
    Some((&receiver[..key_len], hops))
}

/// The key a caller's facts must bind for a receiver to be typed: the
/// receiver itself, or the name an attribute path starts at (`app` for
/// `app.json`). `None` for a receiver no fact can type.
pub fn receiver_key(receiver: &str) -> Option<&str> {
    receiver_path(receiver).map(|(key, _)| key)
}

/// The function `scope`'s entity is nested in, by the name the nested one's
/// extends (`test_index.<locals>.index` is nested in `test_index`), when that
/// function carries facts and is not the file's module.
fn enclosing_scope(world: &dyn OriginWorld, scope: &Scope) -> Option<EntityId> {
    let name = world.entity_name(scope.entity)?;
    let (outer, _) = name.rsplit_once(".<locals>.")?;
    let parent = world.declaration(&scope.file, outer)?;
    if world
        .module_scope(&scope.file)
        .is_some_and(|(module, _)| module == parent)
    {
        return None;
    }
    world.facts(parent).map(|_| parent)
}

/// Whether an origin names a binding a module makes rather than a value.
fn reads_module(origin: &ValueOrigin) -> bool {
    matches!(
        origin,
        ValueOrigin::Global { .. } | ValueOrigin::Module { .. } | ValueOrigin::ModuleMember { .. }
    )
}

/// Follow an origin that names a module-level binding to what that binding
/// holds: a name read off the scope's own module, the member of a module an
/// import names, a module whose attribute the next hop reads. Returns the
/// scope the origin it arrives at is to be read in, that origin, and how
/// many of `hops` the module reads consumed. An origin of any other kind
/// comes back as it is.
///
/// `None` when a binding on the way is not one a module records: the name of
/// a class or a function, a name bound twice, a module the repository does
/// not hold. Then no fact types the value, which is not a gap of a chain.
fn through_modules(
    world: &dyn OriginWorld,
    scope: &Scope,
    origin: &ValueOrigin,
    hops: &[Hop<'_>],
) -> Option<(Scope, ValueOrigin, usize)> {
    let mut scope = scope.clone();
    let mut origin = origin.clone();
    let mut used = 0;
    for _ in 0..MODULE_BINDINGS_CAP {
        match origin {
            ValueOrigin::Global { name } => {
                let (module, facts) = world.module_scope(&scope.file)?;
                origin = facts.bindings.get(&name)?.clone();
                scope = Scope::of(world, module, &scope.file);
            }
            ValueOrigin::Module { module } => {
                let Hop::Attribute(attribute) = hops.get(used)? else {
                    return None;
                };
                used += 1;
                // A module inside the module, or a name the module binds.
                let inner = format!("{module}.{attribute}");
                origin = if world.module_file(&scope.file, &inner).is_some() {
                    ValueOrigin::Module { module: inner }
                } else {
                    ValueOrigin::ModuleMember {
                        module,
                        name: attribute.to_string(),
                    }
                };
            }
            ValueOrigin::ModuleMember { module, name } => {
                let file = world.module_file(&scope.file, &module)?;
                let (entity, facts) = world.module_scope(&file)?;
                origin = facts.bindings.get(&name)?.clone();
                scope = Scope::of(world, entity, &file);
            }
            other => return Some((scope, other, used)),
        }
    }
    None
}

/// What the receiver `receiver` of a call in `caller` holds, through the
/// facts of the caller's scope and the declarations of every attribute it
/// reads, or `None` when no fact binds its key there. A name the scope reads
/// off its module, an import of it and a re-export are followed to what the
/// module that binds it once says it is ([`through_modules`]). A receiver
/// that is a module itself is `None`: a call on a module is the module tier's.
fn evaluate_receiver(
    world: &dyn OriginWorld,
    caller: EntityId,
    caller_file: &str,
    receiver: &str,
) -> Option<(Link, Walk)> {
    let (key, hops) = receiver_path(receiver)?;
    let facts = world.facts(caller)?;
    let origin = facts.bindings.get(key)?;
    let scope = Scope::of(world, caller, caller_file);
    let binding = Link::silent(
        OriginStep::new(OriginStepKind::Binding, key).at(Some(caller), Some(caller_file)),
    );
    let (scope, origin, used) = through_modules(world, &scope, origin, &hops)?;
    let mut walk = evaluate(world, &scope, &origin, 0);
    for hop in hops.into_iter().skip(used) {
        walk = match (walk, hop) {
            (Ok(Reached::Class(typed)), Hop::Attribute(attribute)) => {
                member_of(world, typed, attribute)
            }
            (Ok(Reached::Class(typed)), Hop::Call(method)) => {
                match world.resolve_method(&typed.file, &typed.class, method) {
                    Some(member) => returned_by(world, member, typed, method),
                    None => {
                        let stop = Stop::at(
                            OriginStepKind::Method,
                            &format!("{}.{method}", typed.class),
                            "method_not_found",
                            Some(&typed.file),
                        );
                        Err(stop.after(typed.links))
                    }
                }
            }
            (Ok(Reached::External(foreign)), Hop::Attribute(attribute)) => {
                let stop = Stop::at(
                    OriginStepKind::Member,
                    &format!("{}.{attribute}", foreign.type_name),
                    "member_of_external_type",
                    None,
                );
                Err(stop.after(foreign.links))
            }
            // What a method outside the repository returns is not written
            // down anywhere the linker reads.
            (Ok(Reached::External(foreign)), Hop::Call(method)) => {
                let stop = Stop::at(
                    OriginStepKind::Returns,
                    &format!("{}.{method}", foreign.type_name),
                    "method_of_external_type",
                    None,
                );
                Err(stop.after(foreign.links))
            }
            (Err(stop), _) => Err(stop),
        };
    }
    Some((binding, walk))
}

/// What a declaration says an attribute of a class holds.
enum MemberDeclaration {
    /// The class, or the one base that declares it, as (file, class, the
    /// class entity, what it holds when that is one type).
    Declared(String, String, Option<EntityId>, Option<ValueOrigin>),
    /// Neither the class nor any base the repository declares does.
    Undeclared,
}

/// The declaration of `attribute` that a value of `class` of `file` reads:
/// the class's own, else the one its bases the repository declares agree on.
/// Bases that declare it differently read as declared without one type.
fn member_declaration(
    world: &dyn OriginWorld,
    file: &str,
    class: &str,
    attribute: &str,
    visits: &mut usize,
) -> MemberDeclaration {
    *visits += 1;
    if *visits > HIERARCHY_VISIT_CAP {
        return MemberDeclaration::Declared(file.to_string(), class.to_string(), None, None);
    }
    let entity = world.declaration(file, class);
    if let Some(declared) = entity
        .and_then(|entity| world.facts(entity))
        .and_then(|facts| facts.members.get(attribute).cloned())
    {
        return MemberDeclaration::Declared(file.to_string(), class.to_string(), entity, declared);
    }
    let mut found: Option<(String, String, Option<EntityId>, Option<ValueOrigin>)> = None;
    for base in world.class_bases(file, class).unwrap_or_default() {
        let Some((base_file, base_class)) = world.locate_class(file, &base) else {
            continue;
        };
        if let MemberDeclaration::Declared(at_file, at_class, at, declared) =
            member_declaration(world, &base_file, &base_class, attribute, visits)
        {
            found = match found {
                None => Some((at_file, at_class, at, declared)),
                Some(seen) if seen.3 == declared => Some(seen),
                Some((seen_file, seen_class, seen_at, _)) => {
                    Some((seen_file, seen_class, seen_at, None))
                }
            };
        }
    }
    match found {
        Some((at_file, at_class, at, declared)) => {
            MemberDeclaration::Declared(at_file, at_class, at, declared)
        }
        None => MemberDeclaration::Undeclared,
    }
}

/// What reading `attribute` off a value of `typed`'s class holds, from the
/// attribute's declaration, read in the file that declares it.
fn member_of(world: &dyn OriginWorld, typed: Typed, attribute: &str) -> Walk {
    let qualified = format!("{}.{attribute}", typed.class);
    let mut visits = 0;
    let (file, class, entity, declared) =
        match member_declaration(world, &typed.file, &typed.class, attribute, &mut visits) {
            MemberDeclaration::Declared(file, class, entity, declared) => {
                (file, class, entity, declared)
            }
            MemberDeclaration::Undeclared => {
                let stop = Stop::at(
                    OriginStepKind::Member,
                    &qualified,
                    "member_not_declared",
                    Some(&typed.file),
                );
                return Err(stop.after(typed.links));
            }
        };
    let declared_as = format!("{class}.{attribute}");
    let Some(origin) = declared else {
        let stop = Stop::at(
            OriginStepKind::Member,
            &declared_as,
            "member_type_unknown",
            Some(&file),
        );
        return Err(stop.after(typed.links));
    };
    let Some(entity) = entity else {
        let stop = Stop::at(
            OriginStepKind::Member,
            &declared_as,
            "member_type_unknown",
            Some(&file),
        );
        return Err(stop.after(typed.links));
    };
    let link = Link::spelled(
        declared_as.clone(),
        OriginStep::new(OriginStepKind::Member, declared_as).at(Some(entity), Some(&file)),
    );
    let declaring = Scope::of(world, entity, &file);
    let sound = typed.sound;
    let mut links = typed.links;
    links.push(link);
    match evaluate(world, &declaring, &origin, 1) {
        Ok(Reached::Class(mut reached)) => {
            links.append(&mut reached.links);
            reached.links = links;
            reached.sound &= sound;
            Ok(Reached::Class(reached))
        }
        Ok(Reached::External(mut reached)) => {
            links.append(&mut reached.links);
            reached.links = links;
            reached.sound &= sound;
            Ok(Reached::External(reached))
        }
        Err(stop) => Err(stop.after(links)),
    }
}

/// How a receiver's facts settle one call through it.
pub(crate) enum ReceiverWalk {
    /// The chain reaches a class that declares or inherits the method.
    Bound(OriginBinding),
    /// The chain reaches a type outside the repository; whether the call
    /// then reaches no repository declaration is
    /// [`prove_receiver_outside`]'s question.
    External,
    /// The chain stops short of a method.
    Gap(OriginGap),
}

/// Follow `receiver.method(...)` written in `caller` through the facts of the
/// caller's scope. `None` when no fact binds the receiver there, which is not
/// this tier's call to settle.
pub(crate) fn walk_receiver_call(
    world: &dyn OriginWorld,
    caller: EntityId,
    caller_file: &str,
    receiver: &str,
    method: &str,
) -> Option<ReceiverWalk> {
    if method.is_empty() {
        return None;
    }
    let (binding, walk) = evaluate_receiver(world, caller, caller_file, receiver)?;
    let gap = |stop: Stop| {
        let stop = stop.prefixed(binding.clone());
        ReceiverWalk::Gap(OriginGap {
            steps: steps_of(stop.links),
            missing: stop.missing,
        })
    };
    let typed = match walk {
        Ok(Reached::Class(typed)) => typed,
        Ok(Reached::External(_)) => return Some(ReceiverWalk::External),
        Err(stop) => return Some(gap(stop)),
    };
    let qualified = format!("{}.{}", typed.class, method);
    let Some(target) = world.resolve_method(&typed.file, &typed.class, method) else {
        let stop = Stop::at(
            OriginStepKind::Method,
            &qualified,
            "method_not_found",
            Some(&typed.file),
        );
        return Some(gap(stop.after(typed.links)));
    };
    let refusal = if target == caller {
        Some("self_call")
    } else if !world.compatible(caller, target) {
        Some("language_boundary")
    } else if declared_for_typing(world, target) {
        // A method a protocol or a class for the type checker alone
        // declares is a description of a value, never the code a call on
        // it runs: Flask's `ProxyMixin._get_current_object` is werkzeug's
        // `LocalProxy` method at run time.
        Some("declared_for_type_checker")
    } else {
        None
    };
    if let Some(reason) = refusal {
        let stop = Stop::at(
            OriginStepKind::Method,
            &qualified,
            reason,
            Some(&typed.file),
        );
        return Some(gap(stop.after(typed.links)));
    }
    let mut links = typed.links;
    links.push(Link::spelled(
        qualified.clone(),
        OriginStep::new(OriginStepKind::Method, qualified)
            .at(Some(target), world.entity_file(target).as_deref()),
    ));
    let chain = format!("{receiver}: {}", spell(&links).join(" -> "));
    let mut steps = vec![binding.step];
    steps.extend(steps_of(links));
    Some(ReceiverWalk::Bound(OriginBinding {
        target,
        confidence: if typed.sound {
            RECEIVER_ORIGIN_CONFIDENCE
        } else {
            RECEIVER_ORIGIN_NAME_CONFIDENCE
        },
        chain,
        steps,
    }))
}

/// Whether the class that declares `method` is a structural type or one
/// declared for the type checker alone (see
/// [`kin_parser::value_origins::ScopeFacts::typing_only`]).
fn declared_for_typing(world: &dyn OriginWorld, method: EntityId) -> bool {
    let (Some(name), Some(file)) = (world.entity_name(method), world.entity_file(method)) else {
        return false;
    };
    let Some((owner, _)) = name.rsplit_once('.') else {
        return false;
    };
    world
        .declaration(&file, owner)
        .and_then(|class| world.facts(class))
        .is_some_and(|facts| facts.typing_only || facts.structural)
}

/// The class of the repository the facts of `caller`'s scope type `receiver`
/// as, through links the source writes down: (file, class). `None` for a
/// receiver no fact binds, one the chain cannot place, a type outside the
/// repository and a chain through a name-only link.
pub(crate) fn receiver_class(
    world: &dyn OriginWorld,
    caller: EntityId,
    caller_file: &str,
    receiver: &str,
) -> Option<(String, String)> {
    let (_, walk) = evaluate_receiver(world, caller, caller_file, receiver)?;
    match walk {
        Ok(Reached::Class(typed)) if typed.sound => Some((typed.file, typed.class)),
        _ => None,
    }
}

/// Bind `receiver.method(...)` written in `caller` through the facts of the
/// caller's scope, or `None` when no fact types the receiver as a class of the
/// repository. `receiver` is a name the scope binds, or one field of its own
/// instance the adapter recorded a fact for (`self.client`, `this.store`).
pub(crate) fn bind_receiver_call(
    world: &dyn OriginWorld,
    caller: EntityId,
    caller_file: &str,
    receiver: &str,
    method: &str,
) -> Option<OriginBinding> {
    match walk_receiver_call(world, caller, caller_file, receiver, method)? {
        ReceiverWalk::Bound(binding) => Some(binding),
        ReceiverWalk::External | ReceiverWalk::Gap(_) => None,
    }
}

/// Prove that `receiver.method(...)` written in `caller` reaches no
/// declaration of the repository: the facts of the caller's scope type the
/// receiver, through links the source writes down, as a type no file of the
/// repository declares, and no class the repository declares could be the
/// value's class and hold `method`. Or they type it as a class of the
/// repository that holds `method` nowhere in its hierarchy and takes it from
/// a base outside the repository ([`inherited_outside`]).
///
/// `None` whenever any of that is short of proven: an untyped receiver, a
/// chain through a name-only link, a member a language may run without a call
/// spelling it, a member the value holds itself, a repository declaration
/// that patches the external type, or a repository class that may be the
/// value's class (a subclass for a nominal type, any declaration of the name
/// for a structural one).
pub(crate) fn prove_receiver_outside(
    world: &dyn OriginWorld,
    caller: EntityId,
    caller_file: &str,
    receiver: &str,
    method: &str,
) -> Option<OutsideProof> {
    if method.is_empty()
        || kin_model::call_site_reading::member_name_may_dispatch_implicitly(method)
    {
        return None;
    }
    let (_, walk) = evaluate_receiver(world, caller, caller_file, receiver)?;
    let foreign = match walk {
        Ok(Reached::External(foreign)) => foreign,
        Ok(Reached::Class(typed)) => return inherited_outside(world, typed, receiver, method),
        Err(_) => return None,
    };
    if !foreign.sound || foreign.own_members.iter().any(|member| member == method) {
        return None;
    }
    let type_leaf = leaf_of(&foreign.type_name);
    let members: Vec<Member> = world
        .members_named(method)
        .into_iter()
        .filter(|member| member.id != caller && world.compatible(caller, member.id))
        .collect();
    // `Array.prototype.pop = ...` or a class the repository names like the
    // type: the declaration a call may reach is this repository's own.
    if members
        .iter()
        .any(|member| member.owner.as_deref().map(leaf_of) == Some(type_leaf))
    {
        return None;
    }
    match &foreign.reach {
        ExternalReach::Exact => {}
        ExternalReach::Structural => {
            if !members.is_empty() {
                return None;
            }
        }
        ExternalReach::Nominal { roots } => {
            if subtype_may_hold(world, &members, roots) {
                return None;
            }
        }
    }
    let mut steps = spell(&foreign.links);
    steps.push(format!("{type_leaf}.{method} is outside the repository"));
    Some(OutsideProof {
        chain: format!("{receiver}: {}", steps.join(" -> ")),
        owner: foreign.type_name,
    })
}

/// Prove that `method`, called on a value the chain types as a class of the
/// repository, is a member no class of the repository holds, so the call
/// reaches the declaration a library base class makes or none at all:
/// `client.get(...)` on a class derived from a library's test client.
///
/// The value's class is the class the chain reached or any class the
/// repository derives from it. A class declared for the type checker alone
/// (Python's `if TYPE_CHECKING:`) is no value's class: a value declared as
/// one is an instance of what its nominal bases promise, so the classes
/// derived from those count too, and the proof is refused when no base
/// promises anything, as it is for a value declared as a structural type.
/// For each of those classes, and every class of the repository it derives
/// from in turn:
///
/// - the class says every name it binds
///   ([`kin_parser::value_origins::ScopeFacts::bound_names`]), and `method`
///   is not one of them;
/// - every base that is not a class of the repository is one its file's own
///   text decides ([`kin_parser::value_origins::ScopeFacts::base_origins`]):
///   a root that holds no member a call spells, or an import of a module no
///   file of the repository can supply.
///
/// And the class the chain reached must itself derive from such an outside
/// base: a name no class holds, on a class the repository declares whole, is
/// an attribute written from outside the class, which may hold anything.
///
/// As for a bound call, an attribute written onto one instance from outside
/// its class is not read.
fn inherited_outside(
    world: &dyn OriginWorld,
    typed: Typed,
    receiver: &str,
    method: &str,
) -> Option<OutsideProof> {
    // A method the class holds is a call this tier binds, the common case.
    if !typed.sound
        || world
            .resolve_method(&typed.file, &typed.class, method)
            .is_some()
    {
        return None;
    }
    let reached = (typed.file.clone(), typed.class.clone());
    let marks = |at: &(String, String)| {
        world
            .declaration(&at.0, &at.1)
            .and_then(|entity| world.facts(entity))
            .map_or((false, false), |facts| {
                (facts.typing_only, facts.structural)
            })
    };
    // A value declared as a structural type may be of any class of matching
    // shape, so nothing bounds the classes that could hold the member.
    if marks(&reached).1 {
        return None;
    }
    // The reached class's own line first, so the bases met on it are known
    // before a derived class adds its own. That line is the reached class,
    // or, when it is a class for the type checker alone, what its nominal
    // bases promise.
    let mut family: Vec<((String, String), bool)> = Vec::new();
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut stack = vec![(reached, true)];
    while let Some((at, own_line)) = stack.pop() {
        if !seen.insert(at.clone()) {
            continue;
        }
        if seen.len() > HIERARCHY_VISIT_CAP {
            return None;
        }
        // A class declared for the type checker alone is no value's class.
        // The value is an instance of what its nominal bases promise, so
        // each of those and every class derived from it joins the family;
        // a structural base promises nothing, and with no base that does,
        // nothing bounds the value's class.
        let typing_only = marks(&at).0;
        if typing_only {
            let mut promised = false;
            // A recorded base is the class it names: the language reader
            // records `ProxyMixin[Request]` as `ProxyMixin`.
            for base in world.class_bases(&at.0, &at.1)? {
                // A base no declaration places could declare the method.
                let next = world.locate_class(&at.0, &base)?;
                if marks(&next).1 {
                    // A protocol promises no class, but a method it declares
                    // is one the value has from somewhere Kin cannot place:
                    // Flask's proxies take `_get_current_object` from
                    // werkzeug's `LocalProxy`, not from the class they stand
                    // for.
                    let declares = world
                        .declaration(&next.0, &next.1)
                        .and_then(|class| world.facts(class))
                        .is_none_or(|facts| {
                            facts
                                .bound_names
                                .as_ref()
                                .is_none_or(|names| names.contains(method))
                        })
                        || world.resolve_method(&next.0, &next.1, method).is_some();
                    if declares {
                        return None;
                    }
                    continue;
                }
                promised = true;
                stack.push((next, own_line));
            }
            if !promised {
                return None;
            }
        }
        stack.extend(
            world
                .subclasses(&at.0, &at.1)
                .into_iter()
                .map(|derived| (derived, false)),
        );
        // Its own declarations run on no value, and its bases joined above.
        if !typing_only {
            family.push((at, own_line));
        }
    }
    family.sort_by_key(|(_, own_line)| !own_line);
    let mut inherited_from: Option<String> = None;
    let mut checked: HashSet<(String, String)> = HashSet::new();
    for (start, own_line) in family {
        let mut stack = vec![start];
        while let Some((file, class)) = stack.pop() {
            if !checked.insert((file.clone(), class.clone())) {
                continue;
            }
            if checked.len() > HIERARCHY_VISIT_CAP {
                return None;
            }
            let facts = world
                .declaration(&file, &class)
                .and_then(|entity| world.facts(entity))?;
            if facts.bound_names.as_ref()?.contains(method)
                || world.resolve_method(&file, &class, method).is_some()
            {
                return None;
            }
            for base in world.class_bases(&file, &class)? {
                if let Some(next) = world.locate_class(&file, &base) {
                    stack.push(next);
                    continue;
                }
                let Some(qualified) = facts.base_origins.get(&base)? else {
                    continue;
                };
                if !world.outside_module(&file, qualified) {
                    return None;
                }
                if own_line && inherited_from.is_none() {
                    inherited_from = Some(qualified.clone());
                }
            }
        }
    }
    let inherited_from = inherited_from?;
    let mut steps = spell(&typed.links);
    steps.push(format!(
        "{}.{method} is outside the repository, inherited from {}",
        typed.class,
        leaf_of(&inherited_from)
    ));
    Some(OutsideProof {
        chain: format!("{receiver}: {}", steps.join(" -> ")),
        owner: inherited_from,
    })
}

/// Whether a class the repository declares may be the class of a value of a
/// nominal external type and reach one of `members` through its own
/// hierarchy: a member's owner, or a class derived from it, that derives from
/// some base the repository does not declare. A member whose owner the class
/// index cannot place is taken to be reachable.
fn subtype_may_hold(world: &dyn OriginWorld, members: &[Member], roots: &[String]) -> bool {
    let mut seen: HashSet<(String, String)> = HashSet::new();
    for member in members {
        let Some(owner) = member.owner.as_deref() else {
            continue;
        };
        let mut stack = vec![(member.file.clone(), owner.to_string())];
        while let Some((file, class)) = stack.pop() {
            if !seen.insert((file.clone(), class.clone())) {
                continue;
            }
            if seen.len() > HIERARCHY_VISIT_CAP || derives_from_outside(world, &file, &class, roots)
            {
                return true;
            }
            stack.extend(world.subclasses(&file, &class));
        }
    }
    false
}

/// Whether `class` of `file`, or any of its ancestors the repository
/// declares, names a base the repository does not declare and `roots` does
/// not list. A class the index does not hold is taken to.
fn derives_from_outside(
    world: &dyn OriginWorld,
    file: &str,
    class: &str,
    roots: &[String],
) -> bool {
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut stack = vec![(file.to_string(), class.to_string())];
    while let Some((file, class)) = stack.pop() {
        if !seen.insert((file.clone(), class.clone())) {
            continue;
        }
        if seen.len() > HIERARCHY_VISIT_CAP {
            return true;
        }
        let Some(bases) = world.class_bases(&file, &class) else {
            return true;
        };
        for base in bases {
            if roots.iter().any(|root| root == &base) {
                continue;
            }
            match world.locate_class(&file, &base) {
                Some(next) => stack.push(next),
                None => return true,
            }
        }
    }
    false
}

/// The span-free evidence record that names this tier on an edge, carrying
/// the chain for audit.
pub(crate) fn chain_record(chain: &str) -> RelationEvidence {
    RelationEvidence {
        parser_rule: Some(RECEIVER_ORIGIN_RULE.to_string()),
        token: Some(chain.to_string()),
        ..RelationEvidence::default()
    }
}

/// The span-free evidence record that carries a bound chain's steps, one
/// addressable link each, as JSON.
pub(crate) fn steps_record(steps: &[OriginStep]) -> Option<RelationEvidence> {
    let token = serde_json::to_string(steps).ok()?;
    Some(RelationEvidence {
        parser_rule: Some(RECEIVER_ORIGIN_STEPS_RULE.to_string()),
        token: Some(token),
        ..RelationEvidence::default()
    })
}

/// The steps a receiver-origin edge recorded, when it carries a record this
/// build reads.
pub fn receiver_origin_steps(relation: &Relation) -> Option<Vec<OriginStep>> {
    relation
        .evidence
        .iter()
        .find(|record| record.parser_rule.as_deref() == Some(RECEIVER_ORIGIN_STEPS_RULE))
        .and_then(|record| record.token.as_deref())
        .and_then(|token| serde_json::from_str(token).ok())
}

/// The class owning a member entity name (`TestThing.test_it` -> `TestThing`).
fn owner_of(name: &str) -> Option<String> {
    let owner = owner_path_of(name)?;
    let leaf = owner.rsplit(['.', ':']).next()?;
    (!leaf.is_empty()).then(|| leaf.to_string())
}

/// The whole owner path of a member entity name (`a.B.m` -> `a.B`).
fn owner_path_of(name: &str) -> Option<String> {
    let (owner, _) = name.rsplit_once(['.', ':'])?;
    let owner = owner.trim_end_matches(':');
    (!owner.is_empty()).then(|| owner.to_string())
}

fn leaf_of(name: &str) -> &str {
    name.rsplit(['.', ':']).next().unwrap_or(name)
}

/// Where a plain `name` written in `file` leads when an import binds it to a
/// module's default export: the module, and the name its default export
/// names there. `None` for any other name, and for a module whose default
/// export names no binding.
fn default_import(world: &dyn OriginWorld, file: &str, name: &str) -> Option<(String, String)> {
    if name.contains('.') {
        return None;
    }
    let (module, original) = world.import_binding(file, name)?;
    if original != "default" {
        return None;
    }
    let exported = world.default_export(&module)?;
    Some((module, exported))
}

/// The class a type name written in `file` names, as (file, class). A
/// default import means whatever the module's default export names there,
/// and a module can hand its default on from another module, so the walk
/// follows a few such hops before it asks the class index.
fn locate_type(world: &dyn OriginWorld, file: &str, name: &str) -> Option<(String, String)> {
    let mut file = file.to_string();
    let mut name = name.to_string();
    for _ in 0..DEFAULT_EXPORT_HOPS {
        match default_import(world, &file, &name) {
            Some((module, exported)) => {
                file = module;
                name = exported;
            }
            None => {
                return world.locate_class(&file, &name).or_else(|| {
                    let (module, original) = world.nested_import(&file, &name)?;
                    world.locate_class(&module, &original)
                })
            }
        }
    }
    None
}

/// The free function a name written in `file` calls, following default
/// imports as [`locate_type`] does.
fn resolve_function(world: &dyn OriginWorld, file: &str, name: &str) -> Option<EntityId> {
    let mut file = file.to_string();
    let mut name = name.to_string();
    for _ in 0..DEFAULT_EXPORT_HOPS {
        match default_import(world, &file, &name) {
            Some((module, exported)) => {
                file = module;
                name = exported;
            }
            None => {
                return world.resolve_callable(&file, &name).or_else(|| {
                    let (module, original) = world.nested_import(&file, &name)?;
                    world.resolve_callable(&module, &original)
                })
            }
        }
    }
    None
}

/// The step naming the class `(file, class)` a chain reached.
fn type_step(world: &dyn OriginWorld, file: &str, class: &str, via: &str) -> OriginStep {
    OriginStep::new(OriginStepKind::Type, class)
        .via(via)
        .at(world.declaration(file, class), Some(file))
}

/// What calling `callee`, written in `scope`'s file, returns: an instance
/// when the name is a class of the repository, else what the function it
/// names declares or agrees to return.
fn evaluate_call(world: &dyn OriginWorld, scope: &Scope, callee: &str, depth: usize) -> Walk {
    if let Some((file, class)) = locate_type(world, &scope.file, callee) {
        return Ok(Reached::Class(Typed {
            links: vec![Link::spelled(
                format!("{class}(...)"),
                type_step(world, &file, &class, "constructed"),
            )],
            file,
            class,
            sound: true,
        }));
    }
    let not_located = || {
        Stop::at(
            OriginStepKind::Call,
            callee,
            "callee_not_located",
            Some(&scope.file),
        )
    };
    let function = resolve_function(world, &scope.file, callee).ok_or_else(not_located)?;
    let function_file = world.entity_file(function).ok_or_else(not_located)?;
    let call =
        OriginStep::new(OriginStepKind::Call, callee).at(Some(function), Some(&function_file));
    let facts = world.facts(function);
    if let Some(returns) = facts.as_ref().and_then(|facts| facts.returns.clone()) {
        return declared(
            world,
            &function_file,
            returns,
            format!("{callee}()"),
            call,
            "returned",
        );
    }
    // A function that declares nothing returns what every `return`
    // of its body agrees on, read in its own scope.
    let Some(returned) = facts.as_ref().and_then(|facts| facts.returned.clone()) else {
        return Err(Stop::at(
            OriginStepKind::Call,
            callee,
            "callee_declares_no_return",
            Some(&function_file),
        ));
    };
    let callee_scope = Scope::of(world, function, &function_file);
    let link = Link::spelled(format!("{callee}() returns"), call.via("returned"));
    match evaluate(world, &callee_scope, &returned, depth + 1) {
        Ok(reached) => Ok(reached.prefixed(link)),
        Err(stop) => Err(stop.prefixed(link)),
    }
}

/// Whether calling the dotted name `qualified`, imported in `file`, makes an
/// instance of exactly a class outside the repository.
///
/// Two things the source writes down say so. No file of the repository can
/// supply the module the name is imported from. And the name is a class: a
/// class of the repository names it as a base, through an import of the same
/// module that is outside there too. A function of a library could return a
/// value of any class; a class constructs its own.
///
/// What the source cannot show is a library whose constructor hands back an
/// instance of a subclass it was told about while the program runs. For that
/// instance to be of a class the repository declares, the repository must
/// have handed the class over. So every class the repository derives from
/// the name, directly or through its own classes, must be one its file hands
/// to no other code (see
/// [`kin_parser::value_origins::ScopeFacts::handed_over`]); a class whose
/// facts are unreadable is taken to be handed over.
fn constructs_outside_class(world: &dyn OriginWorld, file: &str, qualified: &str) -> bool {
    if !world.outside_module(file, qualified) {
        return false;
    }
    let mut stack: Vec<(String, String)> = world
        .classes_based_on(qualified)
        .into_iter()
        .filter(|(derived, _)| world.outside_module(derived, qualified))
        .collect();
    if stack.is_empty() {
        return false;
    }
    let mut seen: HashSet<(String, String)> = HashSet::new();
    while let Some((file, class)) = stack.pop() {
        if !seen.insert((file.clone(), class.clone())) {
            continue;
        }
        if seen.len() > HIERARCHY_VISIT_CAP {
            return false;
        }
        let kept = world
            .declaration(&file, &class)
            .and_then(|entity| world.facts(entity))
            .is_some_and(|facts| !facts.handed_over);
        if !kept {
            return false;
        }
        stack.extend(world.subclasses(&file, &class));
    }
    true
}

fn evaluate(world: &dyn OriginWorld, scope: &Scope, origin: &ValueOrigin, depth: usize) -> Walk {
    if depth >= CHAIN_DEPTH_CAP {
        return Err(Stop::at(
            OriginStepKind::Binding,
            "",
            "chain_too_deep",
            Some(&scope.file),
        ));
    }
    match origin {
        ValueOrigin::Declared { type_name } => {
            let Some((file, class)) = locate_type(world, &scope.file, type_name) else {
                return Err(Stop::at(
                    OriginStepKind::Type,
                    type_name,
                    "type_not_located",
                    Some(&scope.file),
                ));
            };
            Ok(Reached::Class(Typed {
                links: vec![Link::spelled(
                    format!("declared {class}"),
                    type_step(world, &file, &class, "declared"),
                )],
                file,
                class,
                sound: true,
            }))
        }
        ValueOrigin::Local { name } => {
            // A name the scope reads off its module is not a local of it.
            let bound = world
                .facts(scope.entity)
                .and_then(|facts| facts.bindings.get(name).cloned())
                .filter(|bound| !reads_module(bound));
            let Some(bound) = bound else {
                return Err(Stop::at(
                    OriginStepKind::Binding,
                    name,
                    "local_unbound",
                    Some(&scope.file),
                ));
            };
            let link = Link::silent(
                OriginStep::new(OriginStepKind::Binding, name.as_str())
                    .at(Some(scope.entity), Some(&scope.file)),
            );
            match evaluate(world, scope, &bound, depth + 1) {
                Ok(reached) => Ok(reached.prefixed(link)),
                Err(stop) => Err(stop.prefixed(link)),
            }
        }
        ValueOrigin::Enclosing { name } => {
            let found = enclosing_scope(world, scope).and_then(|parent| {
                let bound = world.facts(parent)?.bindings.get(name).cloned()?;
                Some((parent, bound))
            });
            let Some((parent, bound)) = found else {
                return Err(Stop::at(
                    OriginStepKind::Binding,
                    name,
                    "enclosing_binding_unknown",
                    Some(&scope.file),
                ));
            };
            let link = Link::silent(
                OriginStep::new(OriginStepKind::Binding, name.as_str())
                    .at(Some(parent), Some(&scope.file)),
            );
            let outer = Scope::of(world, parent, &scope.file);
            // The enclosing function may read the name off its module.
            let walked = match through_modules(world, &outer, &bound, &[]) {
                Some((bound_in, bound, _)) => evaluate(world, &bound_in, &bound, depth + 1),
                None => Err(Stop::at(
                    OriginStepKind::Binding,
                    name,
                    "module_binding_unknown",
                    Some(&scope.file),
                )),
            };
            match walked {
                Ok(reached) => Ok(reached.prefixed(link)),
                Err(stop) => Err(stop.prefixed(link)),
            }
        }
        // A binding a module makes is followed where a walk starts
        // ([`through_modules`]) and never from inside a chain.
        ValueOrigin::Global { name }
        | ValueOrigin::ModuleMember { name, .. }
        | ValueOrigin::Module { module: name } => Err(Stop::at(
            OriginStepKind::Binding,
            name,
            "module_binding_unknown",
            Some(&scope.file),
        )),
        ValueOrigin::Call { callee } => evaluate_call(world, scope, callee, depth),
        ValueOrigin::ImportedCall { callee, qualified } => {
            match evaluate_call(world, scope, callee, depth) {
                // The callee is nothing the repository declares. When it is a
                // class of a library, the call makes an instance of exactly
                // that class.
                Err(stop)
                    if stop.links.is_empty()
                        && stop.missing.reason == "callee_not_located"
                        && constructs_outside_class(world, &scope.file, qualified) =>
                {
                    Ok(Reached::External(Foreign {
                        type_name: qualified.clone(),
                        reach: ExternalReach::Exact,
                        own_members: Vec::new(),
                        sound: true,
                        links: vec![Link::spelled(
                            format!("{}(...)", leaf_of(qualified)),
                            OriginStep::new(OriginStepKind::External, qualified.as_str())
                                .via("constructed"),
                        )],
                    }))
                }
                walked => walked,
            }
        }
        ValueOrigin::Supplied { callee } => {
            if let Some((file, class)) = locate_type(world, &scope.file, callee) {
                return Ok(Reached::Class(Typed {
                    links: vec![Link::spelled(
                        format!("{class}(...) supplied"),
                        type_step(world, &file, &class, "supplied"),
                    )],
                    file,
                    class,
                    sound: true,
                }));
            }
            let not_located = || {
                Stop::at(
                    OriginStepKind::Call,
                    callee,
                    "callee_not_located",
                    Some(&scope.file),
                )
            };
            let function = resolve_function(world, &scope.file, callee).ok_or_else(not_located)?;
            let function_file = world.entity_file(function).ok_or_else(not_located)?;
            let call = OriginStep::new(OriginStepKind::Call, callee)
                .via("supplied")
                .at(Some(function), Some(&function_file));
            let Some(facts) = world.facts(function) else {
                return Err(Stop::at(
                    OriginStepKind::Call,
                    callee,
                    "callee_declares_no_return",
                    Some(&function_file),
                ));
            };
            if let Some(supplies) = facts.supplies.as_ref() {
                let supplier = Scope::of(world, function, &function_file);
                let link = Link::spelled(format!("{callee} supplies"), call);
                return match evaluate(world, &supplier, supplies, depth + 1) {
                    Ok(reached) => Ok(reached.prefixed(link)),
                    Err(stop) => Err(stop.prefixed(link)),
                };
            }
            let Some(returns) = facts.returns.clone() else {
                return Err(Stop::at(
                    OriginStepKind::Call,
                    callee,
                    "callee_declares_no_return",
                    Some(&function_file),
                ));
            };
            declared(
                world,
                &function_file,
                returns,
                format!("{callee} supplies"),
                call,
                "supplied",
            )
        }
        ValueOrigin::MethodCall { receiver, method } => {
            match evaluate(world, scope, receiver, depth + 1) {
                Ok(Reached::Class(typed)) => {
                    let Some(member) = world.resolve_method(&typed.file, &typed.class, method)
                    else {
                        let stop = Stop::at(
                            OriginStepKind::Method,
                            &format!("{}.{method}", typed.class),
                            "method_not_found",
                            Some(&typed.file),
                        );
                        return Err(stop.after(typed.links));
                    };
                    returned_by(world, member, typed, method)
                }
                // What a method outside the repository returns is not written
                // down anywhere the linker reads.
                Ok(Reached::External(foreign)) => {
                    let stop = Stop::at(
                        OriginStepKind::Returns,
                        &format!("{}.{method}", foreign.type_name),
                        "method_of_external_type",
                        None,
                    );
                    Err(stop.after(foreign.links))
                }
                Err(stop) => {
                    // The receiver is untyped. Settling the method by its
                    // name alone is a guess, and is graded as one. When no
                    // guess is possible either, the receiver's own gap is the
                    // one to report.
                    let guessed = (|| {
                        let member = world.unique_method(method)?;
                        let member_file = world.entity_file(member)?;
                        let owner = world.entity_name(member).as_deref().and_then(owner_of)?;
                        let (file, class) = world.locate_class(&member_file, &owner)?;
                        Some((
                            member,
                            Typed {
                                links: vec![Link::spelled(
                                    format!("?.{method} named uniquely by {owner}"),
                                    OriginStep::new(
                                        OriginStepKind::Guess,
                                        format!("{owner}.{method}"),
                                    )
                                    .at(Some(member), Some(&member_file)),
                                )],
                                file,
                                class,
                                sound: false,
                            },
                        ))
                    })();
                    match guessed {
                        Some((member, typed)) => returned_by(world, member, typed, method),
                        None => Err(stop),
                    }
                }
            }
        }
        ValueOrigin::Entered { value, method } => {
            let typed = match evaluate(world, scope, value, depth + 1)? {
                Reached::Class(typed) => typed,
                Reached::External(foreign) => {
                    let stop = Stop::at(
                        OriginStepKind::Returns,
                        &format!("{}.{method}", foreign.type_name),
                        "method_of_external_type",
                        None,
                    );
                    return Err(stop.after(foreign.links));
                }
            };
            let Some(member) = world.resolve_method(&typed.file, &typed.class, method) else {
                let stop = Stop::at(
                    OriginStepKind::Method,
                    &format!("{}.{method}", typed.class),
                    "method_not_found",
                    Some(&typed.file),
                );
                return Err(stop.after(typed.links));
            };
            returned_by(world, member, typed, method)
        }
        ValueOrigin::Provided { lookup, name } => {
            let kind = lookup_kind(lookup);
            let found = find_provider(world, scope, lookup, name).map_err(|reason| {
                let mut stop = Stop::at(OriginStepKind::Provider, name, reason, Some(&scope.file));
                stop.missing.kind = Some(kind.to_string());
                stop
            })?;
            match found {
                Found::Provider(provider, file, value) => {
                    evaluate_provider(world, scope, provider, &file, &value, depth)
                }
                Found::Otherwise(kind, value) => {
                    let link = Link::spelled(
                        format!("{kind} {name} has no provider in the repository"),
                        OriginStep::new(OriginStepKind::Fallback, name.as_str()).kind(&kind),
                    );
                    match evaluate(world, scope, &value, depth + 1) {
                        Ok(reached) => Ok(reached.prefixed(link)),
                        Err(stop) => Err(stop.prefixed(link)),
                    }
                }
            }
        }
        ValueOrigin::ImportedProvider { kind, name } => {
            let stop = |reason: &str| {
                let mut stop = Stop::at(OriginStepKind::Provider, name, reason, Some(&scope.file));
                stop.missing.kind = Some(kind.clone());
                stop
            };
            let (module, original) = world
                .import_binding(&scope.file, name)
                .or_else(|| world.nested_import(&scope.file, name))
                .ok_or_else(|| stop("imported_provider_not_located"))?;
            let held = world
                .providers_in(&module)
                .ok_or_else(|| stop("provider_file_unreadable"))?;
            let of_kind = || held.iter().filter(|(_, provider)| &provider.kind == kind);
            // pytest registers the imported function under the name the
            // importing module binds it to, unless the fixture names itself;
            // either way it must answer to the name requested here.
            let matching: Vec<&(EntityId, ValueProvider)> = of_kind()
                .filter(|(entity, provider)| {
                    !provider.every_name
                        && provider.owner.is_none()
                        && &provider.name == name
                        && world.entity_name(*entity).as_deref().map(leaf_of)
                            == Some(original.as_str())
                })
                .collect();
            let found = match matching.as_slice() {
                [one] => (*one).clone(),
                [] if of_kind().any(|(_, provider)| provider.every_name) => {
                    return Err(stop("provider_lookup_open"))
                }
                [] => return Err(stop("imported_provider_not_found")),
                _ => return Err(stop("provider_ambiguous")),
            };
            let value = found
                .1
                .value
                .clone()
                .ok_or_else(|| stop("provider_value_unknown"))?;
            evaluate_provider(world, scope, found, &module, &value, depth)
        }
        ValueOrigin::External {
            type_name,
            reach,
            own_members,
        } => Ok(Reached::External(Foreign {
            type_name: type_name.clone(),
            reach: reach.clone(),
            own_members: own_members.clone(),
            sound: true,
            links: vec![Link::spelled(
                type_name.clone(),
                OriginStep::new(OriginStepKind::External, type_name.as_str()),
            )],
        })),
    }
}

/// What a provider found for `requester` supplies, evaluated in the
/// provider's own scope for the requester's anchor, behind the link that
/// names the provider.
fn evaluate_provider(
    world: &dyn OriginWorld,
    requester: &Scope,
    provider: (EntityId, ValueProvider),
    file: &str,
    value: &ValueOrigin,
    depth: usize,
) -> Walk {
    let provider_scope = Scope::provider(
        world,
        provider.0,
        file,
        provider.1.owner.clone(),
        &provider.1.name,
        requester,
    );
    // The declaration the provider is written as, when the file declares one
    // under its name, else the entity whose facts carry it.
    let declared_at = world
        .declaration(file, &provider.1.name)
        .unwrap_or(provider.0);
    let link = Link::spelled(
        format!("{} {}@{}", provider.1.kind, provider.1.name, file),
        OriginStep::new(OriginStepKind::Provider, provider.1.name.as_str())
            .kind(&provider.1.kind)
            .at(Some(declared_at), Some(file)),
    );
    match evaluate(world, &provider_scope, value, depth + 1) {
        Ok(reached) => Ok(reached.prefixed(link)),
        Err(stop) => Err(stop.prefixed(link)),
    }
}

fn lookup_kind(lookup: &ProviderLookup) -> &str {
    match lookup {
        ProviderLookup::DirectoryChain { kind, .. }
        | ProviderLookup::Binding { kind }
        | ProviderLookup::Owner { kind } => kind,
    }
}

/// Where a declared return leads, read from the callable's own file. `call`
/// is the callable's own step, and `via` how the value the chain reaches is
/// handed over.
fn declared(
    world: &dyn OriginWorld,
    callable_file: &str,
    returns: DeclaredReturn,
    spelled: String,
    call: OriginStep,
    via: &str,
) -> Walk {
    match returns {
        DeclaredReturn::Type { type_name } => {
            let Some((file, class)) = locate_type(world, callable_file, &type_name) else {
                let stop = Stop::at(
                    OriginStepKind::Type,
                    &type_name,
                    "type_not_located",
                    Some(callable_file),
                );
                return Err(stop.after(vec![Link::silent(call)]));
            };
            Ok(Reached::Class(Typed {
                links: vec![
                    Link::spelled(format!("{spelled} -> {class}"), call),
                    Link::silent(type_step(world, &file, &class, via)),
                ],
                file,
                class,
                sound: true,
            }))
        }
        DeclaredReturn::External { type_name, reach } => Ok(Reached::External(Foreign {
            links: vec![
                Link::spelled(format!("{spelled} -> {type_name}"), call),
                Link::silent(OriginStep::new(
                    OriginStepKind::External,
                    type_name.as_str(),
                )),
            ],
            type_name,
            reach,
            own_members: Vec::new(),
            sound: true,
        })),
        DeclaredReturn::Receiver => {
            let name = call.name.clone();
            let stop = Stop::at(
                OriginStepKind::Call,
                &name,
                "callee_returns_receiver",
                Some(callable_file),
            );
            Err(stop.after(vec![Link::silent(call)]))
        }
    }
}

/// What calling `member` on a value of `receiver`'s class returns.
fn returned_by(world: &dyn OriginWorld, member: EntityId, receiver: Typed, method: &str) -> Walk {
    let qualified = format!("{}.{method}", receiver.class);
    let member_file = world.entity_file(member);
    let returns_step = OriginStep::new(OriginStepKind::Returns, qualified.as_str())
        .at(Some(member), member_file.as_deref());
    let Some(returns) = world.facts(member).and_then(|facts| facts.returns.clone()) else {
        let stop = Stop::at(
            OriginStepKind::Returns,
            &qualified,
            "method_declares_no_return",
            member_file.as_deref(),
        );
        return Err(stop.after(receiver.links));
    };
    let mut links = receiver.links;
    match returns {
        DeclaredReturn::Receiver => {
            links.push(Link::spelled(
                format!("{qualified}() -> {}", receiver.class),
                returns_step,
            ));
            Ok(Reached::Class(Typed {
                file: receiver.file,
                class: receiver.class,
                sound: receiver.sound,
                links,
            }))
        }
        DeclaredReturn::Type { type_name } => {
            let located = member_file
                .as_deref()
                .and_then(|member_file| locate_type(world, member_file, &type_name));
            let Some((file, class)) = located else {
                let stop = Stop::at(
                    OriginStepKind::Type,
                    &type_name,
                    "type_not_located",
                    member_file.as_deref(),
                );
                return Err(stop.after(links));
            };
            links.push(Link::spelled(
                format!("{qualified}() -> {class}"),
                returns_step,
            ));
            links.push(Link::silent(type_step(world, &file, &class, "returned")));
            Ok(Reached::Class(Typed {
                file,
                class,
                sound: receiver.sound,
                links,
            }))
        }
        DeclaredReturn::External { type_name, reach } => {
            links.push(Link::spelled(
                format!("{qualified}() -> {type_name}"),
                returns_step,
            ));
            links.push(Link::silent(OriginStep::new(
                OriginStepKind::External,
                type_name.as_str(),
            )));
            Ok(Reached::External(Foreign {
                type_name,
                reach,
                own_members: Vec::new(),
                sound: receiver.sound,
                links,
            }))
        }
    }
}

/// What a lookup found: a provider, with the file it is declared in and the
/// value it supplies, or the fallback a lookup names for when the repository
/// provides nothing, with the provider kind it stands in for.
enum Found {
    Provider((EntityId, ValueProvider), String, ValueOrigin),
    Otherwise(String, ValueOrigin),
}

/// The provider a lookup reaches from `scope`, or the reason code for why it
/// reaches none: none is found and the lookup names no fallback, the nearest
/// one's value is unknown or ambiguous, a file on the walk cannot be read or
/// may provide any name, a blocking hook sits on the walk, or the scope the
/// lookup is made for receives the name some other way.
///
/// A directory-chain lookup starts where the scope it is made for sits (see
/// [`Scope::lookup_origin`]): the scope's owner class first, then the classes
/// that owner derives from, then the scope's file, then each file named
/// `file_name` up the directory chain. Below every file the walk reads come
/// the providers of the modules those files register
/// ([`OriginWorld::provider_modules`]), and below those the lookup's
/// fallback.
fn find_provider(
    world: &dyn OriginWorld,
    scope: &Scope,
    lookup: &ProviderLookup,
    name: &str,
) -> Result<Found, &'static str> {
    match lookup {
        ProviderLookup::DirectoryChain {
            kind,
            file_name,
            blockers,
            otherwise,
        } => {
            let origin = scope.lookup_origin(name);
            let withheld = world
                .facts(scope.anchor.entity)
                .is_some_and(|facts| facts.withheld_names.contains(name));
            if withheld {
                return Err("provider_withheld");
            }
            // A hook a test class declares can parametrize its tests too.
            if let Some(owner_path) = origin.owner_path.as_deref() {
                if blockers.iter().any(|blocker| {
                    world.file_declares(&origin.file, &format!("{owner_path}.{blocker}"))
                }) {
                    return Err("provider_lookup_blocked");
                }
            }
            let mut files = vec![origin.file.clone()];
            let mut dir = parent_dir(&origin.file);
            loop {
                let candidate = if dir.is_empty() {
                    file_name.clone()
                } else {
                    format!("{dir}/{file_name}")
                };
                if candidate != origin.file && world.known_file(&candidate) {
                    files.push(candidate);
                }
                if dir.is_empty() {
                    break;
                }
                dir = parent_dir(dir);
            }
            let mut plugins: Vec<(String, ProviderModule)> = Vec::new();
            for (at, file) in files.iter().enumerate() {
                if blockers
                    .iter()
                    .any(|blocker| world.file_declares(file, blocker))
                {
                    return Err("provider_lookup_blocked");
                }
                let held = world.providers_in(file).ok_or("provider_file_unreadable")?;
                let of_kind: Vec<&(EntityId, ValueProvider)> = held
                    .iter()
                    .filter(|(entity, provider)| &provider.kind == kind && *entity != origin.entity)
                    .collect();
                let named: Vec<&&(EntityId, ValueProvider)> = of_kind
                    .iter()
                    .filter(|(_, provider)| !provider.every_name && provider.name == name)
                    .collect();
                let scoped: Vec<&&&(EntityId, ValueProvider)> = named
                    .iter()
                    .filter(|(_, provider)| {
                        at == 0 && provider.owner.is_some() && provider.owner == origin.owner
                    })
                    .collect();
                let unscoped: Vec<&&&(EntityId, ValueProvider)> = named
                    .iter()
                    .filter(|(_, provider)| provider.owner.is_none())
                    .collect();
                let inherited = if at == 0 && scoped.is_empty() {
                    inherited_provider(world, &origin, kind, name)?
                } else {
                    None
                };
                let found = match (scoped.as_slice(), inherited, unscoped.as_slice()) {
                    ([one], _, _) => Some(((***one).clone(), file.clone())),
                    ([], Some(inherited), _) => Some(inherited),
                    ([], None, [one]) => Some(((***one).clone(), file.clone())),
                    ([], None, []) => None,
                    // Two providers of the name in one scope: which one wins
                    // depends on an order the facts do not record.
                    _ => return Err("provider_ambiguous"),
                };
                if let Some((found, file)) = found {
                    let value = found.1.value.clone().ok_or("provider_value_unknown")?;
                    return Ok(Found::Provider(found, file, value));
                }
                if of_kind.iter().any(|(_, provider)| provider.every_name) {
                    return Err("provider_lookup_open");
                }
                let registered = world
                    .provider_modules(file)
                    .ok_or("provider_file_unreadable")?;
                plugins.extend(
                    registered
                        .into_iter()
                        .filter(|module| &module.kind == kind)
                        .map(|module| (file.clone(), module)),
                );
            }
            if let Some(found) = plugin_provider(world, plugins, kind, name)? {
                let value = found.0 .1.value.clone().ok_or("provider_value_unknown")?;
                return Ok(Found::Provider(found.0, found.1, value));
            }
            let value = otherwise.as_deref().ok_or("provider_not_found")?.clone();
            Ok(Found::Otherwise(kind.clone(), value))
        }
        ProviderLookup::Binding { kind } => {
            let local = world
                .providers_in(&scope.file)
                .ok_or("provider_file_unreadable")?
                .into_iter()
                .find(|(_, provider)| &provider.kind == kind && provider.name == name);
            if let Some(found) = local {
                let value = found.1.value.clone().ok_or("provider_value_unknown")?;
                return Ok(Found::Provider(found, scope.file.clone(), value));
            }
            let (file, original) = world
                .import_binding(&scope.file, name)
                .ok_or("provider_not_found")?;
            let found = world
                .providers_in(&file)
                .ok_or("provider_file_unreadable")?
                .into_iter()
                .find(|(_, provider)| &provider.kind == kind && provider.name == original)
                .ok_or("provider_not_found")?;
            let value = found.1.value.clone().ok_or("provider_value_unknown")?;
            Ok(Found::Provider(found, file, value))
        }
        ProviderLookup::Owner { kind } => {
            let owner = scope.owner.as_deref().ok_or("provider_not_found")?;
            let owner_path = scope.owner_path.as_deref().ok_or("provider_not_found")?;
            // A nested owner's class-index key is not the path its members
            // spell, so it is not checked and not answered.
            if owner_path != owner {
                return Err("provider_owner_open");
            }
            let bases = world
                .class_bases(&scope.file, owner)
                .ok_or("provider_not_found")?;
            if bases.iter().any(|base| {
                world.locate_class(&scope.file, base).is_some()
                    || world.declares_class_named(leaf_of(base))
            }) || !world.subclasses(&scope.file, owner).is_empty()
            {
                return Err("provider_owner_open");
            }
            let held = world
                .providers_in(&scope.file)
                .ok_or("provider_file_unreadable")?;
            let mut matching = held.into_iter().filter(|(entity, provider)| {
                &provider.kind == kind
                    && provider.name == name
                    && provider.owner.as_deref() == Some(owner)
                    && *entity != scope.entity
            });
            let found = matching.next().ok_or("provider_not_found")?;
            if matching.next().is_some() {
                return Err("provider_ambiguous");
            }
            let value = found.1.value.clone().ok_or("provider_value_unknown")?;
            Ok(Found::Provider(found, scope.file.clone(), value))
        }
    }
}

/// The provider of `name` a class `origin`'s owner derives from declares,
/// scoped to that class: pytest collects a test class's fixtures with the
/// ones its bases declare. `Ok(None)` when no base the repository declares
/// provides it, and an error when two of them do.
fn inherited_provider(
    world: &dyn OriginWorld,
    origin: &Anchor,
    kind: &str,
    name: &str,
) -> Result<Option<((EntityId, ValueProvider), String)>, &'static str> {
    let Some(owner) = origin.owner_path.as_deref() else {
        return Ok(None);
    };
    let mut found: Option<((EntityId, ValueProvider), String)> = None;
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut stack: Vec<(String, String)> = world
        .class_bases(&origin.file, owner)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|base| world.locate_class(&origin.file, &base))
        .collect();
    while let Some((file, class)) = stack.pop() {
        if !seen.insert((file.clone(), class.clone())) {
            continue;
        }
        if seen.len() > HIERARCHY_VISIT_CAP {
            return Err("provider_owner_open");
        }
        let held = world
            .providers_in(&file)
            .ok_or("provider_file_unreadable")?;
        let leaf = leaf_of(&class);
        for provider in held.into_iter().filter(|(_, provider)| {
            provider.kind == kind
                && provider.name == name
                && !provider.every_name
                && provider.owner.as_deref() == Some(leaf)
        }) {
            match &found {
                Some((seen_provider, _)) if seen_provider.0 == provider.0 => {}
                Some(_) => return Err("provider_ambiguous"),
                None => found = Some((provider, file.clone())),
            }
        }
        stack.extend(
            world
                .class_bases(&file, &class)
                .unwrap_or_default()
                .into_iter()
                .filter_map(|base| world.locate_class(&file, &base)),
        );
    }
    Ok(found)
}

/// The provider of `name` the registered plugin modules declare, each read
/// with the plugins it registers in turn. `Ok(None)` when none declares it
/// and every module was read; an error when two do, or when a module the
/// repository does not hold, or cannot read, may.
fn plugin_provider(
    world: &dyn OriginWorld,
    registered: Vec<(String, ProviderModule)>,
    kind: &str,
    name: &str,
) -> Result<Option<((EntityId, ValueProvider), String)>, &'static str> {
    let mut found: Option<((EntityId, ValueProvider), String)> = None;
    let mut open = false;
    let mut seen: HashSet<String> = HashSet::new();
    let mut queue: std::collections::VecDeque<(String, ProviderModule)> = registered.into();
    while let Some((from, module)) = queue.pop_front() {
        let Some(file) = world.module_file(&from, &module.module) else {
            open = true;
            continue;
        };
        if !seen.insert(file.clone()) {
            continue;
        }
        if seen.len() > PLUGIN_MODULES_CAP {
            return Err("provider_lookup_open");
        }
        let held = world
            .providers_in(&file)
            .ok_or("provider_file_unreadable")?;
        for provider in held
            .into_iter()
            .filter(|(_, provider)| provider.kind == kind && provider.owner.is_none())
        {
            if provider.1.every_name {
                open = true;
                continue;
            }
            if provider.1.name != name {
                continue;
            }
            match &found {
                Some((seen_provider, _)) if seen_provider.0 == provider.0 => {}
                Some(_) => return Err("provider_ambiguous"),
                None => found = Some((provider, file.clone())),
            }
        }
        let nested = world
            .provider_modules(&file)
            .ok_or("provider_file_unreadable")?;
        queue.extend(
            nested
                .into_iter()
                .filter(|nested| nested.kind == kind)
                .map(|nested| (file.clone(), nested)),
        );
    }
    if open {
        return Err("provider_lookup_open");
    }
    Ok(found)
}

fn parent_dir(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_of_reads_the_segment_before_the_member() {
        assert_eq!(owner_of("TestThing.test_it").as_deref(), Some("TestThing"));
        assert_eq!(owner_of("a.b.C.m").as_deref(), Some("C"));
        assert_eq!(owner_of("Widget::work").as_deref(), Some("Widget"));
        assert_eq!(owner_of("free_function"), None);
    }

    #[test]
    fn a_receiver_is_a_bound_key_and_the_steps_taken_off_it() {
        use Hop::{Attribute, Call};
        for (receiver, key, hops) in [
            ("ctx", "ctx", vec![]),
            ("self.client", "self.client", vec![]),
            ("this.store", "this.store", vec![]),
            ("$scope", "$scope", vec![]),
            ("this.$el", "this.$el", vec![]),
            ("$this->store", "$this->store", vec![]),
            ("app.json", "app", vec![Attribute("json")]),
            ("self.a.b", "self.a", vec![Attribute("b")]),
            ("this.store.cache", "this.store", vec![Attribute("cache")]),
            ("flask.g", "flask", vec![Attribute("g")]),
            (
                "a.b.c.d",
                "a",
                vec![Attribute("b"), Attribute("c"), Attribute("d")],
            ),
            (
                "app.test_cli_runner()",
                "app",
                vec![Call("test_cli_runner")],
            ),
            (
                "app.test_client(use_cookies=False).session",
                "app",
                vec![Call("test_client"), Attribute("session")],
            ),
            (
                "client.get(\"/a.b\", data=(1, 2))",
                "client",
                vec![Call("get")],
            ),
            ("self.app.json", "self.app", vec![Attribute("json")]),
        ] {
            assert_eq!(receiver_path(receiver), Some((key, hops)), "{receiver}");
        }
        for refused in [
            "",
            "self",
            "self.",
            "a b",
            "a.b.c.d.e",
            "make().invoke",
            "self.make()",
            "app.json()()",
            "app.f()(x)",
            "items[0]",
            "items[0].pop",
            "app.f(",
            "\"text\".join",
            "$this->store->cache",
            "$other->store",
        ] {
            assert_eq!(receiver_path(refused), None, "{refused}");
        }
    }

    #[test]
    fn parent_dir_walks_to_the_root() {
        assert_eq!(parent_dir("tests/sub/test_x.py"), "tests/sub");
        assert_eq!(parent_dir("tests/sub"), "tests");
        assert_eq!(parent_dir("tests"), "");
    }
}
