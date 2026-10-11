// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Parse-time records of how a file uses names as values, and the listed
//! residue a focal's escape census reports from them.
//!
//! A call that never spells a focal's name can still reach it: the focal is
//! held as a value (`handler = target`, then `handler()`), fetched by a name
//! computed at run time (`getattr(obj, name)`, `obj[key]`), or invoked by the
//! language itself (`with ctx:` calls `__enter__`, `<Row />` calls `Row`).
//! Whether a focal can be reached that way is decided per focal from three
//! records every adapter can fill for a file, at parse time, from its syntax
//! tree:
//!
//! - [`ValueOccurrence`]: a name read as a value rather than called or
//!   declared, with the lexical binding it reads and where the value goes;
//! - [`ReflectiveSite`]: a lookup by a name computed at run time, an
//!   evaluation of code, or an enumeration of an object's members;
//! - [`ProtocolSite`]: syntax the language turns into a call that spells no
//!   call name, or spells one without a call expression.
//!
//! The adapter writes the record, [`FileEscapeRecord`], once per file on one
//! entity of the file under [`VALUE_ESCAPE_RECORD_KEY`]. A member the language
//! invokes through syntax carries [`ProtocolMember`] under
//! [`PROTOCOL_MEMBER_KEY`], and a member no own-property enumeration yields
//! carries [`NON_ENUMERABLE_MEMBER_KEY`]. The census reads them from the graph
//! and never the file's bytes.
//!
//! Python and JavaScript/TypeScript fill these records. Go, Java, Kotlin,
//! Swift, C#, PHP, Ruby, Rust and C/C++ can fill them next: every field is a
//! syntactic fact their grammars expose (reflection APIs such as Go's
//! `reflect`, Java's `Method.invoke`, PHP's variable functions and Ruby's
//! `send` are reflective sites; operator overloads and `Drop`/`Deref` are
//! protocol members).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The entity metadata key under which an adapter keeps a file's record.
pub const VALUE_ESCAPE_RECORD_KEY: &str = "value_escape_record";

/// The record version this build writes.
///
/// Version 2 adds [`OccurrenceContext::Mention`] and
/// [`FileEscapeRecord::lists_mentions`]: a record that sets it lists every
/// non-call read of a name it can bind, not only the reads whose value flows
/// on, so a census can match each one against the reference rows the graph
/// stores. Everything a version 1 record says, a version 2 record says the
/// same way.
///
/// Version 3 states three marks on every class it lists
/// ([`ClassBases::typing_only`], [`ClassBases::structural`],
/// [`ClassBases::dynamic`]). An older record lists its classes without them,
/// so a reader that needs to know a class answers no attribute read by its
/// name, or runs, cannot learn it there.
pub const VALUE_ESCAPE_RECORD_VERSION: u32 = 3;

/// The oldest record version whose classes carry their marks (see
/// [`VALUE_ESCAPE_RECORD_VERSION`]).
pub const CLASS_MARKS_RECORD_VERSION: u32 = 3;

/// The oldest record version this build reads. A version 1 record reads
/// exactly as it did: it lists no mentions, so a census over it leaves
/// non-call completion unmeasured and answers everything else unchanged.
pub const VALUE_ESCAPE_RECORD_VERSION_MIN: u32 = 1;

/// Whether this build reads a record that carries `version`.
pub fn readable_record_version(version: u64) -> bool {
    (u64::from(VALUE_ESCAPE_RECORD_VERSION_MIN)..=u64::from(VALUE_ESCAPE_RECORD_VERSION))
        .contains(&version)
}

/// The entity metadata key under which an adapter marks a member the language
/// invokes through syntax.
pub const PROTOCOL_MEMBER_KEY: &str = "protocol_member";

/// The entity metadata key under which an adapter marks a member no
/// enumeration of an object's own enumerable properties yields: a method or
/// accessor declared in a JavaScript or TypeScript class body lives on the
/// prototype and is not enumerable. The value is `true`. A reflective site
/// with [`ReflectiveSite::enumerable_only`] never reaches such a member.
pub const NON_ENUMERABLE_MEMBER_KEY: &str = "non_enumerable_member";

/// Where the value a [`ValueOccurrence`] reads goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OccurrenceContext {
    /// Passed to a call: a positional or keyword argument, a JSX attribute, a
    /// spread into an argument list.
    Argument,
    /// Bound to a function-local name, a destructured local or a parameter
    /// default: the value can travel on through a name no record follows.
    Assignment,
    /// Returned, yielded, or the body of an expression-bodied function.
    Return,
    /// Held by a container: a list, tuple, set, dict or object literal
    /// element, a spread into one, or a subscript store (`d[k] = f`).
    Collection,
    /// Bound under the names in [`ValueOccurrence::aliases`] at module or
    /// class scope, or stored into a named attribute (`obj.cb = f`): calls
    /// then spell an alias.
    Alias,
    /// Exported from its module under [`ValueOccurrence::aliases`], or, with
    /// no alias, as the module's value itself (`module.exports = f`).
    Export,
    /// Imported under another local name (`import {f as g}`,
    /// `from m import f as g`). [`ValueOccurrence::module`] names the module
    /// and [`ValueOccurrence::import`] the statement.
    Import,
    /// Not a use of a value: the name is the target of a module-scope,
    /// class-scope or member assignment that is not a declaration
    /// (`pop = _fail` in a class body). Listed, never an escape.
    Binding,
    /// Any other position a value flows on from.
    Other,
    /// A read of the name whose value flows nowhere a later call could reach
    /// it through: an import that binds it under its own name, a type
    /// annotation, a condition, a comparison, the receiver of a member read,
    /// a decorator, an except clause, a subscript, a class base, an argument
    /// a builtin only inspects, or a store into a member of that name.
    /// [`ValueOccurrence::form`] says which. Never an escape, and never an
    /// alias: a census lists it only to match it against the reference rows
    /// the graph stores for the focal it could read.
    Mention,
}

impl OccurrenceContext {
    /// The wire word for the context.
    pub fn wire(self) -> &'static str {
        match self {
            Self::Argument => "argument",
            Self::Assignment => "assignment",
            Self::Return => "return",
            Self::Collection => "collection",
            Self::Alias => "alias",
            Self::Export => "export",
            Self::Import => "import",
            Self::Binding => "binding",
            Self::Other => "other",
            Self::Mention => "mention",
        }
    }
}

/// Where an [`OccurrenceContext::Mention`] reads its name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MentionForm {
    /// An import that binds the name under its own name (`from m import f`,
    /// `import { f } from "m"`).
    Import,
    /// A type annotation or any other type position, a quoted forward
    /// reference included.
    Annotation,
    /// A condition: `if`, `while`, `assert`, a conditional expression's test,
    /// a comprehension filter, a `not` or a match guard.
    Condition,
    /// An operand of a comparison: `==`, `is`, `in`, `instanceof`.
    Comparison,
    /// The object of a member read or a call through a member: `name.attr`,
    /// `name.method()`.
    Receiver,
    /// A decorator that is not itself a call expression.
    Decorator,
    /// An exception type an except clause matches.
    Except,
    /// The value or an index of a subscript or a slice.
    Subscript,
    /// A base or a metaclass a class declaration names.
    Base,
    /// An argument to a builtin that only inspects it (`isinstance`, `len`).
    Argument,
    /// A class or value pattern of a match statement.
    Pattern,
    /// A store into a member of the name (`obj.name = value`,
    /// `obj.name += 1`): it reads no value, and it changes the one a later
    /// read of the member gets.
    Store,
    /// Any other read that is not a call, a declaration or a value use: an
    /// iterable, an operand, an interpolation, a `with` or `raise` operand, a
    /// bare expression statement.
    Other,
}

impl MentionForm {
    /// The wire word for the form.
    pub fn wire(self) -> &'static str {
        match self {
            Self::Import => "import",
            Self::Annotation => "annotation",
            Self::Condition => "condition",
            Self::Comparison => "comparison",
            Self::Receiver => "receiver",
            Self::Decorator => "decorator",
            Self::Except => "except",
            Self::Subscript => "subscript",
            Self::Base => "base",
            Self::Argument => "inert_argument",
            Self::Pattern => "pattern",
            Self::Store => "store",
            Self::Other => "read",
        }
    }
}

/// The lexical binding a name occurrence reads, as far as its file says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OccurrenceBinding {
    /// A member access, `receiver.name`: [`ValueOccurrence::receiver`] holds
    /// the receiver.
    Member,
    /// A declaration nested in the enclosing function.
    Local,
    /// A name bound directly in the enclosing class body.
    Class,
    /// A name bound at the file's module scope by a declaration or an
    /// assignment.
    Module,
    /// A name an import of this file binds.
    Import,
    /// No binding in the file: a builtin, a global or a star import.
    Free,
}

/// What a member access's receiver is, as far as its file says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReceiverKind {
    /// The enclosing instance or class: `self`, `cls`, `this`, `super()`. It
    /// holds members, never a module-level declaration.
    Instance,
    /// A module object: an imported module, a namespace import, a required
    /// module, `module.exports` or `exports`.
    Namespace,
    /// Anything else.
    Other,
}

/// One non-call use of a name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValueOccurrence {
    /// The 0-based line of the name token.
    #[serde(rename = "l")]
    pub line: u32,
    #[serde(rename = "c")]
    pub context: OccurrenceContext,
    #[serde(rename = "b")]
    pub binding: OccurrenceBinding,
    /// The receiver's text for a member occurrence, whitespace collapsed and
    /// cut to [`RECEIVER_TEXT_MAX`] characters.
    #[serde(rename = "r", default, skip_serializing_if = "Option::is_none")]
    pub receiver: Option<String>,
    #[serde(rename = "rk", default, skip_serializing_if = "Option::is_none")]
    pub receiver_kind: Option<ReceiverKind>,
    /// The names the value is bound or exported under, for
    /// [`OccurrenceContext::Alias`], [`OccurrenceContext::Export`] and
    /// [`OccurrenceContext::Import`].
    #[serde(rename = "a", default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    /// The module an import binding names, as written.
    #[serde(rename = "m", default, skip_serializing_if = "Option::is_none")]
    pub module: Option<String>,
    /// The byte range of the import statement that binds the name, for an
    /// import binding or an [`OccurrenceContext::Import`] occurrence. The
    /// census matches it against the import edges that land on a focal.
    #[serde(rename = "i", default, skip_serializing_if = "Option::is_none")]
    pub import: Option<(u32, u32)>,
    /// The name the import statement imports, when the local name differs.
    #[serde(rename = "o", default, skip_serializing_if = "Option::is_none")]
    pub imported: Option<String>,
    /// The 0-based line of the one declaration a module, class or local
    /// binding reads, when exactly one binds the name there.
    #[serde(rename = "t", default, skip_serializing_if = "Option::is_none")]
    pub target_line: Option<u32>,
    /// Where an [`OccurrenceContext::Mention`] reads the name. Absent for
    /// every other context.
    #[serde(rename = "p", default, skip_serializing_if = "Option::is_none")]
    pub form: Option<MentionForm>,
}

impl ValueOccurrence {
    /// The wire word a residue lists the occurrence under: the mention's
    /// form for a mention, the context otherwise.
    pub fn residue_kind(&self) -> &'static str {
        match (self.context, self.form) {
            (OccurrenceContext::Mention, Some(form)) => form.wire(),
            (context, _) => context.wire(),
        }
    }
}

/// The longest receiver text a record keeps.
pub const RECEIVER_TEXT_MAX: usize = 60;

/// The longest excerpt a reflective site keeps.
pub const EXCERPT_MAX: usize = 80;

/// How a reflective site reaches a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReflectiveForm {
    /// A member fetched by a computed name: `getattr(o, k)`, `o[k]`,
    /// `Reflect.get(o, k)`, a computed destructuring key, `o.__dict__`.
    ComputedMember,
    /// A module's own name table: `globals()`, module-level `locals()` or
    /// `vars()`.
    ScopeTable,
    /// Code evaluated at run time: `eval`, `exec`, `compile`, `Function`.
    Evaluate,
    /// A module loaded by a computed specifier: `import(k)`, `require(k)`,
    /// `__import__(k)`, `import_module(k)`. It yields a module object; any
    /// member read off it is its own occurrence or reflective site, so it
    /// reaches no declaration by itself.
    DynamicImport,
    /// Every member value of an object at once: `Object.values(o)`,
    /// `Object.entries(o)`, `inspect.getmembers(o)`.
    Enumerate,
}

impl ReflectiveForm {
    pub fn wire(self) -> &'static str {
        match self {
            Self::ComputedMember => "computed_member",
            Self::ScopeTable => "scope_table",
            Self::Evaluate => "evaluate",
            Self::DynamicImport => "dynamic_import",
            Self::Enumerate => "enumerate",
        }
    }
}

/// What a reflective site can reach, as its syntax says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReflectiveReach {
    /// Members of the enclosing instance or class (`getattr(self, k)`,
    /// `this[k]`): methods, never a module-level declaration.
    Members,
    /// The declarations at its own file's module scope.
    OwnModule,
    /// Anything: methods, and module-level declarations through a module
    /// object.
    Any,
    /// Nothing by itself (see [`ReflectiveForm::DynamicImport`]).
    Nothing,
}

/// What the receiver of a computed member lookup provably is, when its file
/// says. A lookup reads members of its receiver and nothing else, so a
/// receiver that is not a module object never reaches a module-level
/// declaration, and one that is a module object never reaches a class member.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "k", rename_all = "snake_case")]
pub enum LookupReceiver {
    /// The class or function declared under `name` in this file at the
    /// 0-based `line`: the lookup reads that declaration's own members and
    /// its bases' (`getattr(Flask, k)`, `Store[k]`), never a module-level
    /// declaration.
    Declared {
        #[serde(rename = "n")]
        name: String,
        #[serde(rename = "l")]
        line: u32,
    },
    /// A module object: a whole-module import (`import a.b`, `import * as
    /// ns`). The lookup reads that module's module-level bindings, never a
    /// class member.
    Module,
}

/// One reflective site.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReflectiveSite {
    #[serde(rename = "l")]
    pub line: u32,
    #[serde(rename = "f")]
    pub form: ReflectiveForm,
    #[serde(rename = "r")]
    pub reach: ReflectiveReach,
    /// What the receiver of a computed member lookup or an enumeration
    /// provably is. Absent, the receiver may be anything [`Self::reach`]
    /// allows.
    #[serde(rename = "o", default, skip_serializing_if = "Option::is_none")]
    pub receiver: Option<LookupReceiver>,
    /// Whether the site yields only the receiver's own enumerable
    /// properties: `Object.values(o)`, `Object.entries(o)`, or `o[k]` with
    /// `k` from `for (k in o)` or `Object.keys(o)`. Such a site never reaches
    /// a member the language defines as non-enumerable (see
    /// [`NON_ENUMERABLE_MEMBER_KEY`]).
    #[serde(rename = "e", default, skip_serializing_if = "std::ops::Not::not")]
    pub enumerable_only: bool,
    /// A literal the computed name provably starts with (`f"visit_{x}"`).
    #[serde(rename = "p", default, skip_serializing_if = "Option::is_none")]
    pub key_prefix: Option<String>,
    /// A literal the computed name provably ends with (`x + "Capture"`).
    #[serde(rename = "s", default, skip_serializing_if = "Option::is_none")]
    pub key_suffix: Option<String>,
    /// Whether the site's value is called on the spot (`getattr(o, k)()`).
    /// Such a site is also a call site the call-site ledger keeps as dynamic
    /// access, so the census does not list it a second time.
    #[serde(rename = "k", default, skip_serializing_if = "std::ops::Not::not")]
    pub called: bool,
    /// The site's text, whitespace collapsed and cut to [`EXCERPT_MAX`]
    /// characters.
    #[serde(rename = "x")]
    pub excerpt: String,
    /// Literal runs the computed name provably holds between its prefix and
    /// its suffix, in order.
    #[serde(rename = "m", default, skip_serializing_if = "Vec::is_empty")]
    pub key_parts: Vec<String>,
}

impl ReflectiveSite {
    /// Whether the record says anything about the name the site looks up: a
    /// literal it starts with, ends with or holds. An adapter that resolves a
    /// constant key records the constant's text here too. A site with none
    /// computes its name from something no record follows, so it could fetch
    /// any member its receiver holds and carries no evidence about any one of
    /// them.
    pub fn key_has_literals(&self) -> bool {
        self.key_prefix
            .as_deref()
            .is_some_and(|text| !text.is_empty())
            || self
                .key_suffix
                .as_deref()
                .is_some_and(|text| !text.is_empty())
            || self.key_parts.iter().any(|part| !part.is_empty())
    }

    /// Whether the site is evidence about `name` in particular: its key's
    /// literals admit it ([`Self::key_admits`]), or its text spells it as a
    /// whole word, as `eval("ctx.pop()")` or a conditional key does. A site
    /// that is neither is a lookup by a name nothing records, which every
    /// reader that does not run the program shares as a blind spot.
    pub fn names_key(&self, name: &str) -> bool {
        (self.key_has_literals() && self.key_admits(name)) || spells_word(&self.excerpt, name)
    }

    /// Whether the computed name could be `name`, given the literals it
    /// provably starts and ends with.
    pub fn key_admits(&self, name: &str) -> bool {
        let Some(rest) = name.strip_prefix(self.key_prefix.as_deref().unwrap_or("")) else {
            return false;
        };
        let Some(mut body) = rest.strip_suffix(self.key_suffix.as_deref().unwrap_or("")) else {
            return false;
        };
        for part in &self.key_parts {
            let Some(at) = body.find(part.as_str()) else {
                return false;
            };
            body = &body[at + part.len()..];
        }
        true
    }
}

/// Whether `text` holds `word` with no identifier character on either side.
/// `$` counts as one, as it does in JavaScript and PHP names.
pub fn spells_word(text: &str, word: &str) -> bool {
    if word.is_empty() {
        return false;
    }
    let is_ident = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
    text.match_indices(word).any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let after = text[at + word.len()..].chars().next();
        !before.is_some_and(is_ident) && !after.is_some_and(is_ident)
    })
}

/// Syntax the language turns into an invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolForm {
    /// A `with` item: calls `__enter__` and `__exit__`.
    With,
    /// An `async with` item: calls `__aenter__` and `__aexit__`.
    AsyncWith,
    /// A JSX element: calls the component its name spells.
    Jsx,
    /// A decorator that is not itself a call expression the parser records.
    Decorator,
    /// `raise Name`: instantiates the class it spells.
    Raise,
    /// `f.call(...)`, `f.apply(...)`, `f.__call__(...)`: calls `f`.
    Invoke,
}

impl ProtocolForm {
    pub fn wire(self) -> &'static str {
        match self {
            Self::With => "with",
            Self::AsyncWith => "async_with",
            Self::Jsx => "jsx",
            Self::Decorator => "decorator",
            Self::Raise => "raise",
            Self::Invoke => "invoke",
        }
    }
}

/// One protocol-dispatch site.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolSite {
    #[serde(rename = "l")]
    pub line: u32,
    #[serde(rename = "f")]
    pub form: ProtocolForm,
    /// The name the site spells, when it spells one: the component of a JSX
    /// element, the decorator, the raised class, the invoked function.
    #[serde(rename = "n", default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// A region of a file its grammar could not parse, with every name token in
/// it: a use of a name the region does not spell cannot hide there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnparsedRegion {
    /// 0-based first line.
    #[serde(rename = "f")]
    pub first: u32,
    /// 0-based last line.
    #[serde(rename = "l")]
    pub last: u32,
    /// Every identifier, property name and identifier-shaped string the
    /// region holds, deduplicated.
    #[serde(rename = "n", default)]
    pub names: Vec<String>,
}

/// A class a file declares, with the bases it names, so a census can tell
/// which classes' members a lookup on the class reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClassBases {
    #[serde(rename = "n")]
    pub name: String,
    /// The 0-based line of the declaration, as a [`LookupReceiver::Declared`]
    /// names it.
    #[serde(rename = "l")]
    pub line: u32,
    /// Each base and metaclass by the name its class is declared under: a
    /// base read through an import is named by the name its module exports,
    /// and one read off a module object by its member name.
    #[serde(rename = "b", default, skip_serializing_if = "Vec::is_empty")]
    pub bases: Vec<String>,
    /// Whether some base is an expression no declared name settles (a call,
    /// a name bound by assignment, a default import), or a decorator may
    /// replace the class, so it may inherit from anything.
    #[serde(rename = "u", default, skip_serializing_if = "std::ops::Not::not")]
    pub open: bool,
    /// Whether the class is declared only for a type checker and never when
    /// the program runs (Python's `if TYPE_CHECKING:`), so no value is an
    /// instance of it. A value declared as one is an instance of what its
    /// bases promise: Flask declares `session` as a `SessionMixinProxy` that
    /// exists only for type checking, and the object is whatever session is
    /// current.
    #[serde(rename = "t", default, skip_serializing_if = "std::ops::Not::not")]
    pub typing_only: bool,
    /// Whether the class is a structural type: any class of matching shape
    /// is a subtype of it, whatever it derives from (a Python `Protocol`).
    /// It promises nothing about the class of a value declared as it.
    #[serde(rename = "s", default, skip_serializing_if = "std::ops::Not::not")]
    pub structural: bool,
    /// Whether the class answers an attribute read by its name, whatever its
    /// hierarchy declares (Python's `__getattr__` or `__getattribute__`), so
    /// a read off one of its instances may get anything.
    #[serde(rename = "d", default, skip_serializing_if = "std::ops::Not::not")]
    pub dynamic: bool,
}

/// Whether every module-level declaration of a file is a member of its
/// module object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModuleScope {
    /// Every module-level name is an attribute of the module (Python).
    Open,
    /// Only exported names are reachable from outside the file
    /// (ECMAScript modules and CommonJS): [`FileEscapeRecord::exports`] lists
    /// the names exported by something other than their own declaration.
    Closed,
}

/// A file's record: every non-call use of a name, every reflective site and
/// every protocol-dispatch site its syntax tree holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEscapeRecord {
    #[serde(rename = "v")]
    pub version: u32,
    pub scope: ModuleScope,
    /// Names the file exports through an export list, an `exports.x = ...`
    /// assignment or `module.exports = {...}`, beside declarations that export
    /// themselves.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exports: Vec<String>,
    /// Regions the grammar could not parse: uses inside them may be missing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unparsed: Vec<UnparsedRegion>,
    /// Non-call uses, by the name used.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub values: BTreeMap<String, Vec<ValueOccurrence>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reflective: Vec<ReflectiveSite>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub protocol: Vec<ProtocolSite>,
    /// Every class the file declares under a name, with its bases.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub classes: Vec<ClassBases>,
    /// Whether `values` also lists every non-call read of a name the file
    /// can bind as an [`OccurrenceContext::Mention`], so a census can tell a
    /// read the graph stores a reference row for from one it does not. A
    /// record that leaves it unset lists only the reads whose value flows on,
    /// and a census over it leaves non-call completion unmeasured. Python and
    /// JavaScript/TypeScript set it.
    #[serde(rename = "lm", default, skip_serializing_if = "std::ops::Not::not")]
    pub lists_mentions: bool,
}

impl FileEscapeRecord {
    /// An empty record of `scope`.
    pub fn new(scope: ModuleScope) -> Self {
        Self {
            version: VALUE_ESCAPE_RECORD_VERSION,
            scope,
            exports: Vec::new(),
            unparsed: Vec::new(),
            values: BTreeMap::new(),
            reflective: Vec::new(),
            protocol: Vec::new(),
            classes: Vec::new(),
            lists_mentions: false,
        }
    }

    /// Whether this record lists its mentions in a version that carries
    /// them.
    pub fn records_mentions(&self) -> bool {
        self.version >= 2 && self.lists_mentions
    }

    /// Whether the file holds nothing a census would read.
    pub fn is_empty(&self) -> bool {
        self.unparsed.is_empty()
            && self.values.is_empty()
            && self.reflective.is_empty()
            && self.protocol.is_empty()
            && self.exports.is_empty()
            && self.classes.is_empty()
    }

    /// The record as entity metadata.
    pub fn to_metadata(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("a file escape record serializes")
    }

    /// The record an entity carries, when it carries one of a version this
    /// build reads ([`readable_record_version`]).
    pub fn from_entity(entity: &crate::Entity) -> Option<Self> {
        let value = entity.metadata.extra.get(VALUE_ESCAPE_RECORD_KEY)?;
        if !value
            .get("v")
            .and_then(serde_json::Value::as_u64)
            .is_some_and(readable_record_version)
        {
            return None;
        }
        serde_json::from_value(value.clone()).ok()
    }
}

/// Whether a record held as JSON lists its mentions in a version that
/// carries them ([`FileEscapeRecord::records_mentions`]).
pub fn record_value_lists_mentions(record: &serde_json::Value) -> bool {
    record
        .get("v")
        .and_then(serde_json::Value::as_u64)
        .is_some_and(|version| version >= 2 && readable_record_version(version))
        && record.get("lm").and_then(serde_json::Value::as_bool) == Some(true)
}

/// How the language reaches a protocol member without a call that spells it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolMember {
    /// Only through [`ProtocolSite`]s of these forms, which the census lists.
    Listed(Vec<ProtocolForm>),
    /// Through syntax too common to list (operators, attribute access,
    /// iteration, conversion, calling an instance): the words name it.
    Implicit(String),
}

impl ProtocolMember {
    pub fn to_metadata(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("a protocol member serializes")
    }

    pub fn from_entity(entity: &crate::Entity) -> Option<Self> {
        serde_json::from_value(entity.metadata.extra.get(PROTOCOL_MEMBER_KEY)?.clone()).ok()
    }
}

/// One listed site of an escape residue.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct ResidueSite {
    pub file: String,
    /// 1-based.
    pub line: u32,
    /// What the site is: an [`OccurrenceContext`], [`ReflectiveForm`] or
    /// [`ProtocolForm`] wire word, or `unparsed` / `unrecorded`.
    pub kind: &'static str,
    /// The name the site spells, when it spells one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The receiver of a member occurrence, the excerpt of a reflective site,
    /// or why a file is unrecorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Reads of a followed name on one receiver that the reading files import
/// from one module no file of the repository can supply, counted together.
/// Such a read gets that module's own member, never a declaration of this
/// repository, so it neither holds the focal as a value nor is a site to
/// check.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct OutsideReceiver {
    /// The receiver as written (`json`).
    pub receiver: String,
    /// The module its one import statement names, as written (`json`).
    pub module: String,
    /// The name read on it (`load`).
    pub name: String,
    /// How many reads the domain holds.
    pub reads: usize,
}

/// Reads of a followed name, not calls, on one receiver whose value the
/// facts of the reading function prove to be of a type outside the
/// repository, counted together: `partial(runner.invoke, cli)` where `runner`
/// is a library's own class. Such a read gets that type's member, never a
/// declaration of this repository, so it neither holds the focal as a value
/// nor is a site to check.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct OutsideValueRead {
    /// The receiver as written (`runner`).
    pub receiver: String,
    /// The type outside the repository whose member the read gets
    /// (`click.testing.CliRunner`).
    pub owner: String,
    /// The name read on it (`invoke`).
    pub name: String,
    /// How many reads the domain holds.
    pub reads: usize,
}

/// Reads of a followed member name on a receiver whose class shares no
/// instance with the focal's owner, counted together: `getattr(self, "get")`
/// inside a class that is neither the owner, one of its bases nor derived
/// from it. Such a read gets a member of the receiver's own hierarchy. The
/// reading is relative to the focal: it says the read is not of this member,
/// and nothing about where the member it does read is declared.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct UnrelatedReceiver {
    /// The receiver as written (`self`).
    pub receiver: String,
    /// The class the receiver's value is an instance of (`MethodView`).
    pub class: String,
    /// The name read on it (`get`).
    pub name: String,
    /// How many reads the domain holds.
    pub reads: usize,
}

/// How the language dispatches to a protocol member, in an escape residue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProtocolResidue {
    /// `listed` or `implicit`.
    pub dispatch: &'static str,
    /// For an implicit dispatch, the syntax that invokes the member.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub through: Option<String>,
    /// For a listed dispatch, every site of its forms in the domain.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sites: Vec<ResidueSite>,
}

/// What a per-focal escape census lists beside its answer: every site that
/// could reach the focal without a call expression that spells one of its
/// call names, each named, so an answer can say complete except these.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct EscapeResidue {
    /// Possible names beyond the focal's own that its callers may spell:
    /// import renames, module- or class-scope rebindings, named attribute
    /// stores and export names. This conservative closure finds candidates;
    /// each caller's proof determines whether its call reaches the focal.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub alias_names: Vec<String>,
    /// Alias bindings whose declaration or incoming import proves the focal's
    /// identity. The conservative name closure may follow more possible aliases
    /// than can be asserted here.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub alias_sites: Vec<ResidueSite>,
    /// Uses that hold the focal as a value it can be called through later.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<ResidueSite>,
    /// Same-named bindings made by assignment rather than declaration: a call
    /// spelling the name may reach them instead. Never an escape.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub bindings: Vec<ResidueSite>,
    /// Invocations that spell a call name without a call expression: JSX
    /// elements, `raise Name`, `f.call(...)`, decorators.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub implicit_calls: Vec<ResidueSite>,
    /// Reflective sites that can reach the focal without spelling it and whose
    /// record is evidence about the focal's name: a key literal that admits
    /// it, or text that spells it ([`ReflectiveSite::names_key`]). A reader
    /// can check each one.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub reflective: Vec<ResidueSite>,
    /// Reflective sites that can reach the focal only the way they can reach
    /// any declaration their receiver holds: a lookup by a name no record
    /// follows, an evaluation of code, a scope table or an enumeration, none
    /// of which spells a call name. Every reader that does not run the program
    /// shares them as a blind spot, so an answer counts them rather than
    /// listing them as sites to check, and never licenses an absence while
    /// any is left.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unkeyed_lookups: Vec<ResidueSite>,
    /// How the language dispatches to the focal when it is a protocol member.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol: Option<ProtocolResidue>,
    /// Regions of the domain no record covers: unparsed regions, and files
    /// with no parse on record.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unrecorded: Vec<ResidueSite>,
    /// Reads of a followed name on a receiver imported from outside the
    /// repository (see [`OutsideReceiver`]). They read another module's
    /// member, so nothing below counts them: not [`Self::is_empty`], not
    /// [`Self::lists_nothing_to_check`] and not [`Self::unsettled_count`].
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub outside_receivers: Vec<OutsideReceiver>,
    /// Reads of a followed name on a receiver whose value is proved to be of
    /// a type outside the repository (see [`OutsideValueRead`]). They read
    /// that type's member, so nothing below counts them, exactly as for
    /// [`Self::outside_receivers`].
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub outside_values: Vec<OutsideValueRead>,
    /// Reads of a followed member name on a receiver whose class shares no
    /// instance with the focal's owner (see [`UnrelatedReceiver`]). They read
    /// another hierarchy's member, so nothing below counts them, exactly as
    /// for [`Self::outside_receivers`].
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unrelated_receivers: Vec<UnrelatedReceiver>,
    /// The focal's non-call reads matched against the reference rows the
    /// graph stores for it. They never reach the focal through a call, so
    /// nothing above depends on them: none of [`Self::is_empty`],
    /// [`Self::lists_nothing_unsettled`], [`Self::lists_nothing_to_check`]
    /// or [`Self::unsettled_count`] reads them.
    #[serde(skip_serializing_if = "MentionResidue::is_untaken")]
    pub mentions: MentionResidue,
}

/// The census of a focal's non-call reads: every recorded non-call use of a
/// name it followed that could read the focal by its binding (a mention, a
/// value use, an alias or import site), each matched against the `Calls`,
/// `Imports` and `References` rows the graph stores into the focal.
///
/// A use a stored row covers is already in a reference answer that asks for
/// that row's kind. One no stored row covers is a non-call reference the
/// answer's rows miss, listed by file and line in [`Self::unmatched`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct MentionResidue {
    /// Whether the census took this reading at all. A census that never
    /// reached the records (an unknown or non-callable answer) leaves it
    /// false and the residue renders nothing for it.
    #[serde(skip)]
    pub taken: bool,
    /// Whether every non-call read the domain holds was measured: every
    /// file's record lists its mentions
    /// ([`FileEscapeRecord::records_mentions`]), no region is unrecorded,
    /// and the stored rows into the focal could be read.
    pub measured: bool,
    /// Why not, when not measured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
    /// Uses a stored row covers, counted by the kind of the row each is
    /// credited to (`calls`, `imports` or `references`): the row its form
    /// names when one covers it (`imports` for an import, `calls` for a
    /// decorator, `references` otherwise), else the first that does in that
    /// order.
    pub matched: BTreeMap<&'static str, usize>,
    /// Uses no stored row covers, each by file, line and what it is.
    pub unmatched: Vec<ResidueSite>,
    /// Why the reading, measured or not, cannot speak for `references`: a
    /// focal that holds members is referenced by a use of one of them through
    /// a value of its type, which spells no name this reading follows. An
    /// import of it still spells its name, so `imports` stays measured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub references_unmeasured: Option<&'static str>,
}

impl MentionResidue {
    /// A reading the census could not measure, for `reason`.
    pub fn unmeasured(reason: &'static str) -> Self {
        Self {
            taken: true,
            measured: false,
            reason: Some(reason),
            matched: BTreeMap::new(),
            unmatched: Vec::new(),
            references_unmeasured: None,
        }
    }

    /// Whether the census took no reading, so nothing renders.
    pub fn is_untaken(&self) -> bool {
        !self.taken
    }

    /// Every use the census matched or listed.
    pub fn count(&self) -> usize {
        self.matched.values().sum::<usize>() + self.unmatched.len()
    }
}

impl EscapeResidue {
    /// Whether nothing could reach the focal without a call that spells a
    /// call name: nothing is listed beyond alias names, which only widen the
    /// names a call spells, and no unkeyed lookup is counted.
    pub fn lists_nothing_unsettled(&self) -> bool {
        self.lists_nothing_to_check() && self.unkeyed_lookups.is_empty()
    }

    /// Whether nothing is left for a reader to check: no value use, implicit
    /// call, reflective site that names the focal's key, protocol dispatch or
    /// unrecorded region. Unkeyed lookups may remain; they are counted, not
    /// checked.
    pub fn lists_nothing_to_check(&self) -> bool {
        self.values.is_empty()
            && self.implicit_calls.is_empty()
            && self.reflective.is_empty()
            && self.protocol.is_none()
            && self.unrecorded.is_empty()
    }

    /// Whether the residue holds nothing at all.
    pub fn is_empty(&self) -> bool {
        self.alias_names.is_empty() && self.bindings.is_empty() && self.lists_nothing_unsettled()
    }

    /// Sites a reader must still check, counted: values, implicit calls,
    /// reflective sites that name the focal's key, listed protocol sites and
    /// unrecorded regions. An implicit protocol counts as one. Unkeyed lookups
    /// are not among them.
    pub fn unsettled_count(&self) -> usize {
        self.values.len()
            + self.implicit_calls.len()
            + self.reflective.len()
            + self.unrecorded.len()
            + self.protocol.as_ref().map_or(0, |protocol| {
                if protocol.sites.is_empty() {
                    1
                } else {
                    protocol.sites.len()
                }
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_round_trips_through_entity_metadata() {
        let mut record = FileEscapeRecord::new(ModuleScope::Open);
        record.values.insert(
            "pop".into(),
            vec![ValueOccurrence {
                line: 392,
                context: OccurrenceContext::Argument,
                binding: OccurrenceBinding::Member,
                receiver: Some("flask.session".into()),
                receiver_kind: Some(ReceiverKind::Other),
                aliases: Vec::new(),
                module: None,
                import: None,
                imported: None,
                target_line: None,
                form: None,
            }],
        );
        record.reflective.push(ReflectiveSite {
            line: 3,
            form: ReflectiveForm::ComputedMember,
            reach: ReflectiveReach::Any,
            key_prefix: Some("visit_".into()),
            key_suffix: None,
            called: false,
            excerpt: "getattr(self, f\"visit_{kind}\")".into(),
            key_parts: Vec::new(),
            receiver: None,
            enumerable_only: false,
        });
        let mut entity = crate::Entity {
            id: crate::EntityId::new(),
            kind: crate::EntityKind::Module,
            name: "m".into(),
            language: crate::LanguageId::Python,
            fingerprint: crate::SemanticFingerprint {
                algorithm: crate::FingerprintAlgorithm::V1TreeSitter,
                ast_hash: crate::Hash256::from_bytes([0; 32]),
                signature_hash: crate::Hash256::from_bytes([0; 32]),
                behavior_hash: crate::Hash256::from_bytes([0; 32]),
                equivalence_hash: crate::Hash256::from_bytes([0; 32]),
                stability_score: 1.0,
            },
            file_origin: None,
            span: None,
            signature: String::new(),
            visibility: crate::Visibility::Public,
            role: crate::EntityRole::Source,
            doc_summary: None,
            metadata: crate::EntityMetadata::default(),
            lineage_parent: None,
            created_in: None,
            superseded_by: None,
        };
        entity
            .metadata
            .extra
            .insert(VALUE_ESCAPE_RECORD_KEY.into(), record.to_metadata());
        assert_eq!(FileEscapeRecord::from_entity(&entity), Some(record));
        let text = serde_json::to_string(&entity.metadata.extra[VALUE_ESCAPE_RECORD_KEY]).unwrap();
        assert!(text.contains("\"r\":\"flask.session\""), "{text}");
        assert!(
            !text.contains("\"m\":"),
            "absent fields stay off the wire: {text}"
        );
    }

    #[test]
    fn a_version_1_record_still_reads_and_lists_no_mentions() {
        assert!(readable_record_version(1));
        assert!(readable_record_version(u64::from(
            VALUE_ESCAPE_RECORD_VERSION
        )));
        assert!(!readable_record_version(0));
        assert!(!readable_record_version(
            u64::from(VALUE_ESCAPE_RECORD_VERSION) + 1
        ));
        let v1 = serde_json::json!({"v": 1, "scope": "open", "lm": true});
        assert!(
            !record_value_lists_mentions(&v1),
            "a version 1 record cannot carry mentions"
        );
        let mut record = FileEscapeRecord::new(ModuleScope::Open);
        assert!(!record.records_mentions());
        assert!(!record_value_lists_mentions(&record.to_metadata()));
        record.lists_mentions = true;
        record.values.insert(
            "Flask".into(),
            vec![ValueOccurrence {
                line: 4,
                context: OccurrenceContext::Mention,
                binding: OccurrenceBinding::Import,
                receiver: None,
                receiver_kind: None,
                aliases: Vec::new(),
                module: Some(".app".into()),
                import: Some((0, 24)),
                imported: None,
                target_line: None,
                form: Some(MentionForm::Annotation),
            }],
        );
        let metadata = record.to_metadata();
        assert!(record.records_mentions());
        assert!(record_value_lists_mentions(&metadata));
        let text = serde_json::to_string(&metadata).unwrap();
        assert!(text.contains("\"c\":\"mention\""), "{text}");
        assert!(text.contains("\"p\":\"annotation\""), "{text}");
        assert!(text.contains("\"lm\":true"), "{text}");
        let back: FileEscapeRecord = serde_json::from_value(metadata).unwrap();
        assert_eq!(back, record);
        assert_eq!(back.values["Flask"][0].residue_kind(), "annotation");
    }

    #[test]
    fn a_key_with_literal_ends_admits_only_names_that_share_them() {
        let site = ReflectiveSite {
            line: 0,
            form: ReflectiveForm::ComputedMember,
            reach: ReflectiveReach::Any,
            key_prefix: Some("on".into()),
            key_suffix: Some("Capture".into()),
            called: false,
            excerpt: String::new(),
            key_parts: vec!["Click".into()],
            receiver: None,
            enumerable_only: false,
        };
        assert!(site.key_admits("onClickCapture"));
        assert!(site.key_admits("onDoubleClickCapture"));
        assert!(!site.key_admits("onClick"));
        assert!(!site.key_admits("onCapture"));
        assert!(!site.key_admits("containsElement"));
    }

    fn lookup(excerpt: &str, prefix: Option<&str>, suffix: Option<&str>) -> ReflectiveSite {
        ReflectiveSite {
            line: 0,
            form: ReflectiveForm::ComputedMember,
            reach: ReflectiveReach::Any,
            key_prefix: prefix.map(str::to_string),
            key_suffix: suffix.map(str::to_string),
            called: false,
            excerpt: excerpt.into(),
            key_parts: Vec::new(),
            receiver: None,
            enumerable_only: false,
        }
    }

    #[test]
    fn a_lookup_names_its_key_only_on_evidence_about_the_name() {
        // No literal and no spelling: a blind spot shared by every reader
        // that does not run the program, and evidence about no name.
        let unknown = lookup("app = getattr(module, attr_name, None)", None, None);
        assert!(!unknown.key_has_literals());
        assert!(!unknown.names_key("pop"));
        // A key built from a literal that admits the name.
        let built = lookup("getattr(self, \"po\" + rest)", Some("po"), None);
        assert!(built.key_has_literals());
        assert!(built.names_key("pop"));
        // A literal the name cannot carry is not evidence about it.
        assert!(!lookup("getattr(self, f\"visit_{kind}\")", Some("visit_"), None).names_key("pop"));
        // A key an adapter resolved from a constant is recorded as its text.
        assert!(lookup("store[METHOD]", Some("containsElement"), None).names_key("containsElement"));
        // Code or a conditional key that spells the name as a whole word.
        assert!(lookup("eval(\"ctx.pop()\")", None, None).names_key("pop"));
        assert!(lookup("getattr(o, \"pop\" if a else \"push\")", None, None).names_key("pop"));
        // A longer identifier that contains the name does not spell it.
        assert!(!lookup("eval(pop_expression)", None, None).names_key("pop"));
        assert!(!lookup("getattr(o, popname)", None, None).names_key("pop"));
        // An empty literal says nothing.
        assert!(!lookup("o[k]", Some(""), Some("")).key_has_literals());
    }

    #[test]
    fn unkeyed_lookups_are_counted_but_not_left_to_check() {
        let site = ResidueSite {
            file: "cli.py".into(),
            line: 49,
            kind: "computed_member",
            name: None,
            detail: Some("getattr(module, attr_name, None)".into()),
        };
        let residue = EscapeResidue {
            unkeyed_lookups: vec![site.clone()],
            ..EscapeResidue::default()
        };
        assert!(residue.lists_nothing_to_check());
        assert!(!residue.lists_nothing_unsettled());
        assert!(!residue.is_empty());
        assert_eq!(residue.unsettled_count(), 0);
        let keyed = EscapeResidue {
            reflective: vec![site],
            ..residue
        };
        assert!(!keyed.lists_nothing_to_check());
        assert_eq!(keyed.unsettled_count(), 1);
    }

    #[test]
    fn a_protocol_member_round_trips() {
        for member in [
            ProtocolMember::Listed(vec![ProtocolForm::With]),
            ProtocolMember::Implicit("operators".into()),
        ] {
            let value = member.to_metadata();
            assert_eq!(
                serde_json::from_value::<ProtocolMember>(value).unwrap(),
                member
            );
        }
    }
}
