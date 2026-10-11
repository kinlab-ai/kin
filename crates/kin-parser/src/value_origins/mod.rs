// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Where the value a name holds comes from, as adapters read it from syntax.
//!
//! A language server answers a call through a receiver only when it can type
//! the receiver. It cannot type a parameter a test framework injects by name,
//! a local bound to what a method it cannot type returns, or a value read out
//! of a framework context object, so a call through such a receiver came back
//! with no answer and the linker's bare-name tier had nothing to settle it
//! with. The declarations that settle it are all in the source: a fixture
//! function returns a constructed class, a method declares its return type, a
//! context object is created with a type argument.
//!
//! Adapters record those declarations here as generic facts on the entity
//! whose scope they belong to, and the linker evaluates them in any language
//! without knowing which framework produced them. Four kinds of fact:
//!
//! - **Bindings**: for a name bound exactly once in a callable's scope, what it
//!   holds. A declared type, a constructor or function call, a method call on
//!   another binding, the value a context-manager protocol enters, a value a
//!   provider supplies by name, or a value of a type no file of the
//!   repository declares ([`ValueOrigin::External`]). A binding may also name
//!   a member of the scope's receiver (`self.client`, `this.store`) when the
//!   adapter can prove what every read of it holds.
//! - **Returns**: the type a callable declares it returns.
//! - **Provides**: a value this entity supplies under a name, for the lookup
//!   rule a [`ValueOrigin::Provided`] binding names.
//! - **Supplies**: what a callable hands a framework that calls it for a
//!   value, for a [`ValueOrigin::Supplied`] binding.
//!
//! A binding that reaches a class the repository declares lets the linker
//! bind a call through it. One that reaches an external type proves the
//! opposite: the call reaches no declaration in the repository, so it is
//! settled as going elsewhere rather than left for a reader to check. The
//! adapter says which types are external and how far a value may stray from
//! the one named ([`ExternalReach`]); the linker only compares owners.
//!
//! Every fact is something the source writes down. An adapter emits nothing
//! for a name bound twice, a parameter with a default, or an annotation that
//! names more than one type, so a scope the adapter cannot read contributes no
//! fact and the linker keeps its earlier behaviour for it.
//!
//! The facts ride on the entity's metadata under [`VALUE_ORIGINS_KEY`], the
//! same channel [`crate::binding_callability`] uses, so the parser-to-linker
//! seam keeps its published shape and a store written before this module read
//! as holding no fact.

use std::collections::{BTreeMap, BTreeSet};

use kin_model::{Entity, LanguageId};
use serde::{Deserialize, Serialize};
use tree_sitter::Tree;

pub(crate) mod cpp;
pub(crate) mod go;
pub(crate) mod java;
mod javascript;
pub(crate) mod kotlin;
pub(crate) mod php;
mod python;
mod rust;
pub(crate) mod swift;

/// Entity metadata key the facts are stored under.
pub const VALUE_ORIGINS_KEY: &str = "value_origins_v1";

/// The version written into every fact record. A reader refuses any other.
pub const VALUE_ORIGINS_VERSION: u32 = 1;

/// Where the value one binding holds comes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ValueOrigin {
    /// A type the source declares for the value, spelled as the declaring
    /// file spells it (`Flask`, `flask.Flask`). Resolved from that file.
    Declared { type_name: String },
    /// The value another binding of the same scope holds.
    Local { name: String },
    /// What calling a plain or dotted name returns: an instance when the name
    /// is a class, the declared return type when it is a function.
    Call { callee: String },
    /// What calling `method` on the value `receiver` describes returns.
    MethodCall {
        receiver: Box<ValueOrigin>,
        method: String,
    },
    /// What a context-manager protocol enters: `method` (`__enter__`) called
    /// on the value `value` describes.
    Entered {
        value: Box<ValueOrigin>,
        method: String,
    },
    /// A value some provider supplies by name, found under `lookup`.
    Provided {
        lookup: ProviderLookup,
        name: String,
    },
    /// The value of the provider of `kind` that an import of `name` in the
    /// providing file reaches: a pytest fixture a conftest imports from
    /// another module (`from .fixtures import app`). The linker follows the
    /// import to the module and takes the one provider there whose
    /// declaration is named like the import's original and that answers to
    /// `name` itself, since that is the name the framework registers it under.
    ImportedProvider { kind: String, name: String },
    /// A value whose class no file of the repository declares: a language's
    /// built-in type (`dict`, `Array`) or a class a framework documents for a
    /// value it makes (pytest's `recwarn` fixture). The linker never resolves
    /// `type_name`. It labels the chain an answer reports, and its last
    /// segment finds a repository declaration that patches the type
    /// (`Array.prototype.pop = ...` declares `Array.pop`).
    External {
        type_name: String,
        reach: ExternalReach,
        /// Members the value itself holds, found before any its type
        /// declares: the keys an object literal writes.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        own_members: Vec<String>,
    },
    /// What a framework hands over when it calls `callee` for a value: an
    /// instance when the name is a class, else what the callable's facts say
    /// it supplies ([`ScopeFacts::supplies`]), else its declared return.
    Supplied { callee: String },
    /// What calling `callee` returns, where the callee is written through a
    /// name its file binds by one import and by nothing else, so the callee
    /// is the dotted name `qualified` (`click.testing.CliRunner` under `from
    /// click.testing import CliRunner`). The linker reads it as any
    /// [`ValueOrigin::Call`] first. When that names nothing the repository
    /// declares, no file of the repository can supply the module, and the
    /// repository proves the name is a class by deriving from it
    /// ([`ScopeFacts::base_origins`]), the value is an instance of exactly
    /// that outside class.
    ImportedCall { callee: String, qualified: String },
    /// What the scope's file binds `name` to at module level, read in a
    /// scope that does not bind the name itself. The linker reads the origin
    /// the file's module entity records for the name
    /// ([`ScopeFacts::bindings`]).
    Global { name: String },
    /// The module `module` names, as an import in the scope's file resolves
    /// it: `flask` under `import flask`. An attribute read off it reads a
    /// module-level name of that module, or a module inside it.
    Module { module: String },
    /// What the module `module` binds `name` to at module level: `session`
    /// under `from .globals import session`.
    ModuleMember { module: String, name: String },
    /// What the function the scope is nested in binds `name` to, read in a
    /// nested function that does not bind the name itself: `app` in a view
    /// a test declares under `@app.route("/")`, whose decorator and body
    /// read the test's `app`. The linker reads the binding of the enclosing
    /// function, whose entity's name the nested one's extends
    /// (`test_index.<locals>.index`).
    Enclosing { name: String },
}

/// Which classes a value of an [`ValueOrigin::External`] type may be an
/// instance of, which decides whether a class the repository declares could
/// receive a call made through it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ExternalReach {
    /// The source fixes the value's own class, and no file of the repository
    /// declares it: a literal, a call to the built-in type itself, a value a
    /// framework constructs.
    Exact,
    /// The value is an instance of the type or of a class derived from it by
    /// naming it as a base. A repository class derives from it only through a
    /// base the repository does not declare, so a class whose every base is
    /// declared here, or spelled as one of `roots` (a language's universal
    /// root, which makes a class a subtype of nothing), cannot be it.
    Nominal {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        roots: Vec<String>,
    },
    /// The value is anything whose shape fits the type, so any member the
    /// repository declares under the called name could be the one reached.
    Structural,
}

/// How the linker finds the provider a [`ValueOrigin::Provided`] names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ProviderLookup {
    /// The nearest provider of `kind` with the name: first one scoped to the
    /// requesting scope's owner in its own file, then an unscoped one in that
    /// file, then an unscoped one in a file called `file_name` in the file's
    /// directory, and so on up to the repository root. The first found wins
    /// and shadows every farther one.
    ///
    /// The lookup fails, rather than answering, when any file it walks
    /// declares an entity named in `blockers`: a hook that can replace the
    /// value the provider supplies. Two providers of the name in one file
    /// fail it too, since which one wins depends on order.
    ///
    /// `otherwise` is the value when the walk reads every file on it, meets
    /// no blocker and finds no provider of the name at all: what a
    /// framework's own built-in provider supplies. A file that can bring in
    /// providers the walk cannot name records one with
    /// [`ValueProvider::every_name`], which stops the walk before it.
    DirectoryChain {
        kind: String,
        file_name: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        blockers: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        otherwise: Option<Box<ValueOrigin>>,
    },
    /// The provider of `kind` bound to the name in the requesting file: one
    /// the file itself declares, or the one an import of the name reaches.
    Binding { kind: String },
    /// The one provider of `kind` scoped to the requesting scope's owner, in
    /// the requesting file: a value the owner's own setup method binds for
    /// its other members. It answers only when no class of the repository
    /// derives from the owner and no base of the owner is a class the
    /// repository declares, since either could bind the value differently.
    Owner { kind: String },
}

/// A value an entity supplies under a name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValueProvider {
    /// The namespace the provider answers in, matched as an opaque string
    /// against a lookup's `kind`.
    pub kind: String,
    /// The name it answers to.
    pub name: String,
    /// The class that scopes it, when it is visible only to that class's
    /// members.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// What it supplies, evaluated in the providing entity's own scope.
    /// `None` when the adapter cannot read it: the provider still shadows
    /// every farther one of its name, so it is recorded all the same.
    #[serde(default)]
    pub value: Option<ValueOrigin>,
    /// Whether it answers to every name, because the adapter cannot read
    /// which names it supplies (a wildcard import, a plugin list). A lookup
    /// meeting one in a file that provides the name under no other record
    /// fails there, and never reaches a farther provider or a fallback.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub every_name: bool,
}

/// The type a callable declares it returns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum DeclaredReturn {
    /// A named type, resolved from the callable's file.
    Type { type_name: String },
    /// The type of the receiver the method was called on.
    Receiver,
    /// A type no file of the repository declares, as
    /// [`ValueOrigin::External`] reads it.
    External {
        type_name: String,
        reach: ExternalReach,
    },
}

/// Every fact one entity carries.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScopeFacts {
    pub version: u32,
    /// Bindings of the entity's own scope, by the name they bind.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub bindings: BTreeMap<String, ValueOrigin>,
    /// The entity's declared return, when it is a callable that declares one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub returns: Option<DeclaredReturn>,
    /// Values the entity supplies by name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provides: Vec<ValueProvider>,
    /// What the callable hands a framework that calls it for a value, when
    /// that is not simply its declared return: what a generator yields, or
    /// the one value every `return` agrees on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supplies: Option<ValueOrigin>,
    /// The declaration a module's default export names, by the name the
    /// module binds it under: `Store` for `export default class Store {}`,
    /// `export default Store` or `export {Store as default}`. A default import
    /// of the module means what this name means in the module's own file.
    /// Only a module entity carries it, and only when the module has exactly
    /// one default export that names a binding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_export: Option<String>,
    /// Where each binding is written, as its line counted from 0 at the
    /// entity's first line, for the bindings whose adapter records one. It is
    /// evidence a reader quotes; nothing resolves through it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub binding_lines: BTreeMap<String, u32>,
    /// What calling the callable returns when it declares no return type:
    /// the one value every `return` of its own body agrees on, evaluated in
    /// its own scope. Only a plain function carries it: never a generator or
    /// a coroutine, whose call returns something else, and never one a
    /// decorator may replace. A call that falls off the end returns nothing a
    /// method can be called on, so it cannot reach a method the value's class
    /// does not hold.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub returned: Option<ValueOrigin>,
    /// What each attribute of a class's instances is declared to hold, by the
    /// attribute's name: an annotation in the class body (`json:
    /// JSONProvider`), one on an assignment to the instance's own attribute
    /// in a method (`self.json: JSONProvider = ...`), or the declared return
    /// of a property. `None` for an attribute the class declares without one
    /// type the reader can name, or that two declarations disagree about: it
    /// still shadows whatever a base declares under the name. Only a class
    /// entity carries it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub members: BTreeMap<String, Option<ValueOrigin>>,
    /// Every name a class binds for itself or its instances: each name its
    /// body binds directly (a method, a nested class, an assignment or an
    /// annotated target, an import) and each attribute written anywhere in
    /// it (`self.name = ...`, `cls.name = ...`). `None` when the class may
    /// hold a name its text does not spell (it writes or reads attributes by
    /// a computed name, a decorator or a metaclass may add to it, its body
    /// holds a wildcard import) and for a record that predates the fact. A
    /// reader that finds a name in none of a hierarchy's sets knows no class
    /// of that hierarchy declares it. Only a class entity carries it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bound_names: Option<BTreeSet<String>>,
    /// What each base a class names is, by the base as the class writes it,
    /// where the file's own text decides it. `Some` dotted name for a base
    /// the file binds by one import and by nothing else
    /// (`werkzeug.test.Client`) or that the language itself binds
    /// (`builtins.dict`); `None` for a root that gives a class no member a
    /// call spells by name (Python's `object` and `Generic[...]`). A base
    /// missing here is one the text does not decide. Only a class entity
    /// carries it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub base_origins: BTreeMap<String, Option<String>>,
    /// Whether the class's own file hands the class itself to other code: a
    /// decorator on it, a keyword in its base list, or its name passed as an
    /// argument of a call. Code a class is handed to can keep it and make
    /// instances of it where the source names another class. Only a class
    /// entity carries it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub handed_over: bool,
    /// Whether the class is declared only for a type checker and never when
    /// the program runs (Python's `if TYPE_CHECKING:`), so no value is an
    /// instance of it: a value declared as one is an instance of what its
    /// bases promise. Only a class entity carries it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub typing_only: bool,
    /// Whether the class is a structural type, which any class of matching
    /// shape is a subtype of whatever it derives from (a Python `Protocol`),
    /// so it promises nothing about the class of a value declared as it.
    /// Only a class entity carries it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub structural: bool,
    /// Modules whose providers this file registers below every provider a
    /// file on a lookup's walk declares, for every lookup of their kind that
    /// walks this file: pytest's `pytest_plugins = ["pkg.fixtures"]`. Only a
    /// module entity carries it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provider_modules: Vec<ProviderModule>,
    /// Names a lookup made on this scope's behalf may not answer from a
    /// provider, because the framework hands the scope's whole request a
    /// value for each of them some other way: a name a pytest test
    /// parametrizes directly is the parameter for every fixture its request
    /// reaches, too.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub withheld_names: BTreeSet<String>,
}

/// A module a file registers providers from, as the file spells its name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderModule {
    /// The namespace its providers answer in.
    pub kind: String,
    /// The module, as an import in the registering file would name it.
    pub module: String,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl ScopeFacts {
    fn is_empty(&self) -> bool {
        self.bindings.is_empty()
            && self.returns.is_none()
            && self.provides.is_empty()
            && self.supplies.is_none()
            && self.default_export.is_none()
            && self.returned.is_none()
            && self.members.is_empty()
            && self.bound_names.is_none()
            && self.base_origins.is_empty()
            && !self.handed_over
            && !self.typing_only
            && !self.structural
            && self.provider_modules.is_empty()
            && self.withheld_names.is_empty()
    }
}

/// The facts an entity carries, or `None` when it carries none or a record
/// this build does not read.
pub fn read_value_origins(entity: &Entity) -> Option<ScopeFacts> {
    let value = entity.metadata.extra.get(VALUE_ORIGINS_KEY)?;
    let facts: ScopeFacts = serde_json::from_value(value.clone()).ok()?;
    (facts.version == VALUE_ORIGINS_VERSION).then_some(facts)
}

/// Facts for one declaring node: the byte ranges the entity's span may cover
/// (see [`FileFacts::by_node`]), the leaf name the entity must carry, and the
/// facts themselves.
pub(crate) type NodeFacts = (Vec<(usize, usize)>, String, ScopeFacts);

/// The facts one adapter read, keyed by the byte range of the syntax node
/// that declares the entity they belong to.
#[derive(Debug, Default)]
pub(crate) struct FileFacts {
    /// Facts by the byte ranges the entity's span may cover (the declaring
    /// node, and any wrapper an adapter spans instead, such as a decorated
    /// definition or an export statement), with the leaf name the entity must
    /// carry.
    pub(crate) by_node: Vec<NodeFacts>,
    /// Facts about the file's module scope, carried by its module entity.
    pub(crate) module: ScopeFacts,
    /// The name each default export of the module names, in source order.
    /// An adapter settles [`ScopeFacts::default_export`] from these: one
    /// name, or none when the module writes several.
    pub(crate) default_exports: Vec<String>,
}

/// Attach every fact an adapter reads in a completely parsed file to the
/// entity it belongs to. A file whose tree holds a parse error gets none: a
/// recovered tree can drop the very binding that makes a name single.
pub fn attach_value_origins(
    tree: &Tree,
    source: &[u8],
    language: LanguageId,
    file_path: &str,
    entities: &mut [Entity],
) {
    for entity in entities.iter_mut() {
        entity.metadata.extra.remove(VALUE_ORIGINS_KEY);
    }
    if tree.root_node().has_error() {
        return;
    }
    let facts = match language {
        LanguageId::Python => python::read_file(tree, source, file_path),
        LanguageId::JavaScript | LanguageId::TypeScript => {
            javascript::read_file(tree, source, file_path)
        }
        LanguageId::Go => go::read_file(tree, source, file_path),
        LanguageId::Java => java::read_file(tree, source, file_path),
        LanguageId::Kotlin => kotlin::read_file(tree, source, file_path),
        LanguageId::Rust => rust::read_file(tree, source, file_path),
        LanguageId::Cpp => cpp::read_file(tree, source, file_path),
        LanguageId::Swift => swift::read_file(tree, source, file_path),
        LanguageId::Php => php::read_file(tree, source, file_path),
        _ => return,
    };
    for (ranges, leaf, mut scope) in facts.by_node {
        if scope.is_empty() {
            continue;
        }
        // A class carries what it says about itself and nothing else: what
        // its instances' attributes are declared to hold, the names it binds,
        // what its bases are, whether its file hands it over and whether it
        // is a class for the type checker alone or a structural type. No
        // class scope binds a value a call reads.
        let of_class = !scope.members.is_empty()
            || scope.bound_names.is_some()
            || !scope.base_origins.is_empty()
            || scope.handed_over
            || scope.typing_only
            || scope.structural;
        if of_class {
            scope = ScopeFacts {
                members: scope.members,
                bound_names: scope.bound_names,
                base_origins: scope.base_origins,
                handed_over: scope.handed_over,
                typing_only: scope.typing_only,
                structural: scope.structural,
                ..ScopeFacts::default()
            };
        }
        let Some(entity) = entities.iter_mut().find(|entity| {
            entity
                .span
                .as_ref()
                .is_some_and(|span| ranges.contains(&(span.start_byte, span.end_byte)))
                && entity.name.rsplit(['.', ':']).next() == Some(leaf.as_str())
                && if of_class {
                    is_class_kind(entity)
                } else {
                    is_scope_kind(entity)
                }
        }) else {
            continue;
        };
        // An adapter records a binding's line in the file; the entity keeps
        // it counted from its own first line, the way a site is addressed, so
        // an edit above the entity leaves its facts as they were.
        let (first, last) = entity
            .span
            .as_ref()
            .map_or((0, 0), |span| (span.start_line, span.end_line));
        scope.binding_lines = std::mem::take(&mut scope.binding_lines)
            .into_iter()
            .filter(|(name, line)| {
                scope.bindings.contains_key(name) && (first..=last).contains(line)
            })
            .map(|(name, line)| (name, line - first))
            .collect();
        scope.version = VALUE_ORIGINS_VERSION;
        if let Ok(value) = serde_json::to_value(&scope) {
            entity
                .metadata
                .extra
                .insert(VALUE_ORIGINS_KEY.into(), value);
        }
    }
    let mut module = facts.module;
    // A module scope's bindings are not recorded, so neither are their lines.
    module.binding_lines.clear();
    if !module.is_empty() {
        // The file's own module entity, which spans the most of the file. A
        // namespace or inner module the adapter also mints as a module spans
        // only its own body, so it is never picked over the file's.
        let file_module = entities
            .iter_mut()
            .filter(|entity| entity.kind == kin_model::EntityKind::Module)
            .max_by_key(|entity| {
                entity
                    .span
                    .as_ref()
                    .map_or(0, |span| span.end_byte.saturating_sub(span.start_byte))
            });
        if let Some(entity) = file_module {
            module.version = VALUE_ORIGINS_VERSION;
            if let Ok(value) = serde_json::to_value(&module) {
                entity
                    .metadata
                    .extra
                    .insert(VALUE_ORIGINS_KEY.into(), value);
            }
        }
    }
}

fn is_scope_kind(entity: &Entity) -> bool {
    matches!(
        entity.kind,
        kin_model::EntityKind::Function | kin_model::EntityKind::Method
    )
}

fn is_class_kind(entity: &Entity) -> bool {
    matches!(entity.kind, kin_model::EntityKind::Class)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fact_record_round_trips_through_metadata() {
        let facts = ScopeFacts {
            version: VALUE_ORIGINS_VERSION,
            bindings: BTreeMap::from([(
                "ctx".to_string(),
                ValueOrigin::MethodCall {
                    receiver: Box::new(ValueOrigin::Provided {
                        lookup: ProviderLookup::DirectoryChain {
                            kind: "pytest_fixture".into(),
                            file_name: "conftest.py".into(),
                            blockers: vec!["pytest_generate_tests".into()],
                            otherwise: Some(Box::new(ValueOrigin::External {
                                type_name: "WarningsRecorder".into(),
                                reach: ExternalReach::Exact,
                                own_members: Vec::new(),
                            })),
                        },
                        name: "app".into(),
                    }),
                    method: "test_request_context".into(),
                },
            )]),
            returns: Some(DeclaredReturn::Receiver),
            provides: vec![ValueProvider {
                kind: "pytest_fixture".into(),
                name: "app".into(),
                owner: None,
                value: Some(ValueOrigin::Call {
                    callee: "Flask".into(),
                }),
                every_name: false,
            }],
            supplies: Some(ValueOrigin::External {
                type_name: "dict".into(),
                reach: ExternalReach::Nominal {
                    roots: vec!["object".into()],
                },
                own_members: vec!["pop".into()],
            }),
            default_export: Some("Store".into()),
            binding_lines: BTreeMap::from([("ctx".to_string(), 3)]),
            returned: Some(ValueOrigin::Local { name: "app".into() }),
            members: BTreeMap::from([
                (
                    "json".to_string(),
                    Some(ValueOrigin::Declared {
                        type_name: "JSONProvider".into(),
                    }),
                ),
                ("config".to_string(), None),
            ]),
            bound_names: Some(BTreeSet::from(["json".to_string(), "open".to_string()])),
            base_origins: BTreeMap::from([
                (
                    "Client".to_string(),
                    Some("werkzeug.test.Client".to_string()),
                ),
                ("object".to_string(), None),
            ]),
            handed_over: true,
            typing_only: true,
            structural: true,
            provider_modules: vec![ProviderModule {
                kind: "pytest_fixture".into(),
                module: "tests.fixtures".into(),
            }],
            withheld_names: BTreeSet::from(["runner".to_string()]),
        };
        let value = serde_json::to_value(&facts).unwrap();
        let back: ScopeFacts = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(back, facts);
        // A record written before a class said what it binds reads as one
        // that does not say.
        let mut older = value;
        older.as_object_mut().unwrap().remove("bound_names");
        let older: ScopeFacts = serde_json::from_value(older).unwrap();
        assert_eq!(older.bound_names, None);
        let imported = ValueOrigin::ImportedProvider {
            kind: "pytest_fixture".into(),
            name: "app".into(),
        };
        let value = serde_json::to_value(&imported).unwrap();
        assert_eq!(
            serde_json::from_value::<ValueOrigin>(value).unwrap(),
            imported
        );
    }
}
