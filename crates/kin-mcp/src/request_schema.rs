// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Typed request definitions, the input schemas served from them, and the
//! server-side check that holds a call to the same rules.
//!
//! A request type is declared once with [`typed_request!`]: a Rust struct whose
//! fields are the arguments a tool takes, each with the bounds, default and
//! short description an agent is served. From that one declaration come three
//! things that used to be written separately and drifted apart:
//!
//! * the `inputSchema` a profile serves ([`TypedRequest::schema`]), with the
//!   type of every field taken from its Rust type through [`ArgType`];
//! * the check every call is held to before it runs ([`check`]), which reads the
//!   same field list and refuses a call the served schema would refuse, with a
//!   sentence that names the fix;
//! * the arguments a handler reads, which are the checked request serialized
//!   back ([`check`] returns them), so a handler never sees an alias, a synonym
//!   or a value the schema does not describe.
//!
//! What a schema cannot say without a top-level combinator is a [`Rule`]. Model
//! provider tool APIs refuse a schema whose top level is `oneOf`, `anyOf` or
//! `allOf`, and a client that loads tools for one of them drops such a tool
//! without a word (see [`crate::input_contract`]), so "exactly one of these
//! selectors, or a continuation" is checked here and said in the served
//! descriptions rather than encoded as a combinator.

use std::collections::HashMap;

use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{json, Map, Value};

/// Which listing a schema is rendered for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// The agent profiles: short descriptions, only the fields an agent needs.
    Agent,
    /// The `full` profile: every field a caller may name, with the long
    /// registered descriptions where a registered tool carries one.
    Full,
}

/// Where a field is advertised. Every field is accepted everywhere; this only
/// decides which listings describe it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Visibility {
    /// Served on every profile.
    Agent,
    /// Served on the `full` profile only.
    Full,
    /// Accepted and checked, never advertised: a deprecated input or one a
    /// runtime fills in.
    Hidden,
}

impl Visibility {
    fn served_on(self, profile: Profile) -> bool {
        matches!(
            (self, profile),
            (Visibility::Agent, _) | (Visibility::Full, Profile::Full)
        )
    }
}

/// A string enumeration: the values served, and the spellings accepted for
/// them without being served.
#[derive(Debug)]
pub struct EnumSpec {
    pub values: &'static [&'static str],
    /// Values served on the `full` profile only, and accepted everywhere.
    pub full_only: &'static [&'static str],
    /// `(accepted, canonical)`: a spelling a handler has always taken, read as
    /// the served value it means.
    pub synonyms: &'static [(&'static str, &'static str)],
    /// Whether the handler has always read this value case-insensitively and
    /// trimmed, so `Calls` and ` calls ` are `calls`.
    pub fold_case: bool,
}

impl EnumSpec {
    /// The served value `raw` means, or `None` when it names none.
    pub fn canonical(&self, raw: &str) -> Option<&'static str> {
        let folded;
        let raw = if self.fold_case {
            folded = raw.trim().to_ascii_lowercase();
            folded.as_str()
        } else {
            raw
        };
        self.values
            .iter()
            .chain(self.full_only)
            .find(|value| **value == raw)
            .or_else(|| {
                self.synonyms
                    .iter()
                    .find(|(accepted, _)| *accepted == raw)
                    .map(|(_, canonical)| canonical)
            })
            .copied()
    }
}

/// An object whose schema and check live with the type that owns it, such as
/// a mutation operation, whose forms are the backend's own.
pub struct NestedSpec {
    /// The schema served for it on each profile.
    pub render: fn(Profile) -> Value,
    /// Why one value is not a valid instance, in words that name the fix.
    pub check: fn(&Value) -> Result<(), String>,
}

impl std::fmt::Debug for NestedSpec {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("NestedSpec")
    }
}

/// The JSON type of one field, derived from its Rust type by [`ArgType`].
#[derive(Debug, Clone)]
pub enum Ty {
    String,
    Integer,
    Boolean,
    /// An object whose keys the tool does not fix.
    Object,
    /// An object of string values, such as an environment.
    StringMap,
    Array(Box<Ty>),
    Enum(&'static EnumSpec),
    Nested(&'static NestedSpec),
    /// Any JSON value: a reply row whose shape its own producer owns.
    Any,
}

/// How a Rust field type is served and checked.
pub trait ArgType {
    fn ty() -> Ty;
}

impl ArgType for String {
    fn ty() -> Ty {
        Ty::String
    }
}

impl ArgType for u64 {
    fn ty() -> Ty {
        Ty::Integer
    }
}

impl ArgType for bool {
    fn ty() -> Ty {
        Ty::Boolean
    }
}

impl ArgType for Map<String, Value> {
    fn ty() -> Ty {
        Ty::Object
    }
}

impl ArgType for std::collections::BTreeMap<String, String> {
    fn ty() -> Ty {
        Ty::StringMap
    }
}

impl ArgType for Value {
    fn ty() -> Ty {
        Ty::Any
    }
}

impl<T: ArgType> ArgType for Vec<T> {
    fn ty() -> Ty {
        Ty::Array(Box::new(T::ty()))
    }
}

/// One argument a tool takes: its wire name, type, bounds, default, served
/// description and where it is advertised.
#[derive(Debug, Clone)]
pub struct Field {
    pub name: &'static str,
    pub ty: Ty,
    /// The short description a field is served with; empty serves none,
    /// because the name, type, bounds and default already say it.
    pub doc: &'static str,
    /// What the agent profiles serve in place of [`Field::doc`], when that
    /// differs: `Some("")` serves none there, and the `full` profile keeps
    /// `doc`. Every agent profile re-sends its schemas on every model turn, so
    /// a clause the name, type or default already says is paid for each turn.
    pub agent_doc: Option<&'static str>,
    pub default: Option<Value>,
    pub minimum: Option<i64>,
    pub maximum: Option<i64>,
    pub min_length: Option<u64>,
    pub max_length: Option<u64>,
    pub min_items: Option<u64>,
    pub max_items: Option<u64>,
    pub required: bool,
    /// Whether the caller sends back a value a reply handed it, such as a
    /// cursor or a returned UUID, rather than one it chooses. See
    /// [`Field::from_reply`].
    pub from_reply: bool,
    /// Whether the value may be null, as a reply field often is.
    pub nullable: bool,
    /// Other spellings accepted for this field and never served.
    pub aliases: Vec<&'static str>,
    pub visibility: Visibility,
}

impl Field {
    pub fn new(name: &'static str, ty: Ty) -> Self {
        Self {
            name,
            ty,
            doc: "",
            agent_doc: None,
            default: None,
            minimum: None,
            maximum: None,
            min_length: None,
            max_length: None,
            min_items: None,
            max_items: None,
            required: false,
            from_reply: false,
            nullable: false,
            aliases: Vec::new(),
            visibility: Visibility::Agent,
        }
    }

    pub fn doc(mut self, doc: &'static str) -> Self {
        self.doc = doc;
        self
    }

    /// A shorter description for the agent profiles; the `full` profile keeps
    /// [`Field::doc`].
    pub fn agent_doc(mut self, doc: &'static str) -> Self {
        self.agent_doc = Some(doc);
        self
    }

    /// No description on the agent profiles, because the field's name, type
    /// and served default already say what [`Field::doc`] says; the `full`
    /// profile keeps it.
    pub fn no_agent_doc(self) -> Self {
        self.agent_doc("")
    }

    /// The description `profile` serves for this field, if any.
    pub fn served_doc(&self, profile: Profile) -> Option<&'static str> {
        let doc = match profile {
            Profile::Agent => self.agent_doc.unwrap_or(self.doc),
            Profile::Full => self.doc,
        };
        (!doc.is_empty()).then_some(doc)
    }

    pub fn default(mut self, value: impl Into<Value>) -> Self {
        self.default = Some(value.into());
        self
    }

    pub fn range(mut self, minimum: i64, maximum: i64) -> Self {
        self.minimum = Some(minimum);
        self.maximum = Some(maximum);
        self
    }

    pub fn min(mut self, minimum: i64) -> Self {
        self.minimum = Some(minimum);
        self
    }

    pub fn non_empty(mut self) -> Self {
        self.min_length = Some(1);
        self
    }

    pub fn max_len(mut self, maximum: u64) -> Self {
        self.max_length = Some(maximum);
        self
    }

    pub fn items(mut self, minimum: u64, maximum: u64) -> Self {
        self.min_items = Some(minimum);
        self.max_items = Some(maximum);
        self
    }

    pub fn min_items(mut self, minimum: u64) -> Self {
        self.min_items = Some(minimum);
        self
    }

    pub fn max_items(mut self, maximum: u64) -> Self {
        self.max_items = Some(maximum);
        self
    }

    pub fn required(mut self) -> Self {
        self.required = true;
        self
    }

    pub fn nullable(mut self) -> Self {
        self.nullable = true;
        self
    }

    /// A value the caller copies from a reply and sends back unchanged: a
    /// continuation cursor, or a UUID a reply returned. Every call is still
    /// held to its length bound, and the `full` profile still serves it. The
    /// agent profiles do not, because no choice a caller makes can break it,
    /// so the bound tells a model nothing about how to call, and an agent
    /// profile re-sends it on every turn.
    pub fn from_reply(mut self) -> Self {
        self.from_reply = true;
        self
    }

    pub fn alias(mut self, name: &'static str) -> Self {
        self.aliases.push(name);
        self
    }

    /// The reply budget every read verb takes, under its one served name. See
    /// [`OUTPUT_BUDGET`].
    pub fn output_budget(self) -> Self {
        self.range(
            crate::budget::RESPONSE_MIN_MAX_CHARS as i64,
            crate::budget::RESPONSE_MAX_MAX_CHARS as i64,
        )
        .default(crate::budget::RESPONSE_DEFAULT_MAX_CHARS as u64)
        .alias("max_chars")
        .alias("max_response_chars")
    }

    /// Advertised on the `full` profile only.
    pub fn full(mut self) -> Self {
        self.visibility = Visibility::Full;
        self
    }

    /// Accepted, never advertised.
    pub fn hidden(mut self) -> Self {
        self.visibility = Visibility::Hidden;
        self
    }
}

/// The one served name of a read verb's reply budget: the most UTF-8 bytes of
/// serialized JSON the reply is cut toward, the `_kin` envelope included.
///
/// Two spellings were served for the same number, `max_chars` on most tools
/// and `max_response_chars` on the trace walk, and neither said its unit. Both
/// are accepted as hidden aliases for one version, and a call that names two
/// spellings with different numbers is refused.
pub const OUTPUT_BUDGET: &str = "max_output_bytes";

/// A rule a flat served schema cannot carry.
#[derive(Debug, Clone)]
pub enum Rule {
    /// Target selectors: exactly one set must be supplied in full (at least
    /// one when `several` is true), unless the call continues a page with one
    /// of `continuation`. A continuation may still name the selector it
    /// continues, which is how callers sent one before a handle alone was
    /// enough.
    Select {
        sets: &'static [&'static [&'static str]],
        continuation: &'static [&'static str],
        several: bool,
    },
    /// `field` means nothing without `needs`.
    Needs {
        field: &'static str,
        needs: &'static str,
    },
    /// `field` cannot be combined with `with`, and `why` says so.
    Excludes {
        field: &'static str,
        with: &'static str,
        why: &'static str,
    },
}

/// A request type declared with [`typed_request!`].
pub trait TypedRequest: Sized + Serialize + DeserializeOwned {
    /// The tool name the request is served under.
    const NAME: &'static str;
    /// Every argument, in the order the declaration lists them.
    fn fields() -> Vec<Field>;
    /// The cross-field rules.
    fn rules() -> Vec<Rule> {
        Vec::new()
    }
    /// One call that works, for a refusal to end with.
    fn example() -> Value;

    /// The served input schema on `profile`. `describe` supplies the long
    /// description a registered tool carries for a field on the `full`
    /// profile; the short one is used where it has none.
    fn schema(profile: Profile, describe: &dyn Fn(&str) -> Option<String>) -> Value {
        render(&Self::fields(), profile, describe)
    }
}

/// Render a field list as a served object schema.
pub fn render(
    fields: &[Field],
    profile: Profile,
    describe: &dyn Fn(&str) -> Option<String>,
) -> Value {
    let mut properties = Map::new();
    let mut required = Vec::new();
    for field in fields {
        if !field.visibility.served_on(profile) {
            continue;
        }
        let mut schema = render_ty(&field.ty, profile);
        let object = schema.as_object_mut().expect("a field schema is an object");
        if field.nullable {
            if let Some(kind) = object.get("type").cloned() {
                object.insert("type".into(), json!([kind, "null"]));
            }
        }
        let description = match profile {
            Profile::Full => describe(field.name),
            Profile::Agent => None,
        }
        .or_else(|| field.served_doc(profile).map(str::to_string));
        if let Some(description) = description {
            object.insert("description".into(), json!(description));
        }
        // On the profiles that pay for every byte, a default is served only
        // when it is a boolean that is on unless turned off, the one default a
        // caller cannot read from the field's absence. A false boolean is what
        // absence means, and a number or a choice is the handler's to apply
        // whether or not it is advertised. The defaults a belt injects on its
        // agents' behalf are advertised by the belt itself
        // ([`crate::agent_belt::apply_belt_schema_defaults`]), so a number the
        // profile sends is still the number it serves.
        let unstated = profile == Profile::Agent && field.default != Some(Value::Bool(true));
        if let Some(default) = field.default.as_ref().filter(|_| !unstated) {
            object.insert("default".into(), default.clone());
        }
        // A bound is served wherever a caller's own choice could break it. A
        // value copied back from a reply has no such choice, so its length
        // bound is the server's alone on the profiles that pay for every byte.
        let copied = profile == Profile::Agent && field.from_reply;
        let bounds = [
            ("minimum", field.minimum.map(Value::from)),
            ("maximum", field.maximum.map(Value::from)),
            ("minLength", field.min_length.map(Value::from)),
            (
                "maxLength",
                field.max_length.filter(|_| !copied).map(Value::from),
            ),
            ("minItems", field.min_items.map(Value::from)),
            ("maxItems", field.max_items.map(Value::from)),
        ];
        for (key, value) in bounds {
            if let Some(value) = value {
                object.insert(key.into(), value);
            }
        }
        if field.required {
            required.push(json!(field.name));
        }
        properties.insert(field.name.into(), schema);
    }
    let mut schema = json!({"type": "object", "properties": properties});
    if !required.is_empty() {
        schema["required"] = Value::Array(required);
    }
    schema
}

fn render_ty(ty: &Ty, profile: Profile) -> Value {
    match ty {
        Ty::String => json!({"type": "string"}),
        Ty::Integer => json!({"type": "integer"}),
        Ty::Boolean => json!({"type": "boolean"}),
        Ty::Object => json!({"type": "object"}),
        Ty::StringMap => json!({"type": "object", "additionalProperties": {"type": "string"}}),
        Ty::Array(items) => json!({"type": "array", "items": render_ty(items, profile)}),
        Ty::Enum(spec) => {
            let mut values: Vec<&str> = spec.values.to_vec();
            if profile == Profile::Full {
                values.extend(spec.full_only);
            }
            json!({"type": "string", "enum": values})
        }
        Ty::Nested(spec) => (spec.render)(profile),
        Ty::Any => json!({}),
    }
}

/// The served schema of a request type used as a nested object, such as a
/// session's capabilities. For a [`NestedSpec`] built from a request type.
pub fn render_nested<R: TypedRequest>(profile: Profile) -> Value {
    R::schema(profile, &|_| None)
}

/// The check of a request type used as a nested object. For a [`NestedSpec`]
/// built from a request type.
pub fn check_nested<R: TypedRequest>(value: &Value) -> Result<(), String> {
    let arguments: HashMap<String, Value> = serde_json::from_value(value.clone())
        .map_err(|_| format!("must be an object, and it was {}.", describe_value(value)))?;
    check_fields::<R>(&arguments).map(|_| ())
}

/// Hold one call to `R`'s declaration, and return the request and the
/// arguments its handler reads.
///
/// Every alias is renamed to its field, every accepted synonym to the value it
/// means, and the request is serialized back, so the arguments a handler reads
/// are exactly what the type holds. A key the type does not declare is carried
/// through unread by the check, as it always was: no schema here is closed yet.
pub fn check<R: TypedRequest>(
    arguments: &HashMap<String, Value>,
) -> Result<(R, HashMap<String, Value>), String> {
    check_fields::<R>(arguments).map_err(|problem| refusal(R::NAME, &problem, &R::example()))
}

/// [`check`] without the refusal around it: the problem alone, for a nested
/// object whose refusal is its parent's.
pub fn check_fields<R: TypedRequest>(
    arguments: &HashMap<String, Value>,
) -> Result<(R, HashMap<String, Value>), String> {
    let fields = R::fields();
    let mut arguments = arguments.clone();

    // Aliases first, so every later step reads one spelling.
    for field in &fields {
        let spelled: Vec<(&str, Value)> = std::iter::once(field.name)
            .chain(field.aliases.iter().copied())
            .filter_map(|name| {
                arguments
                    .get(name)
                    .filter(|value| !value.is_null())
                    .map(|value| (name, value.clone()))
            })
            .collect();
        if let Some((first, value)) = spelled.first().cloned() {
            if let Some((other, _)) = spelled.iter().find(|(_, other)| *other != value) {
                return Err(format!(
                    "{first} and {other} are two names for {canonical}, and they disagree. \
                     Send only {canonical}.",
                    canonical = field.name
                ));
            }
            for (name, _) in &spelled {
                arguments.remove(*name);
            }
            arguments.insert(field.name.into(), value);
        }
    }

    for field in &fields {
        let Some(value) = arguments.get_mut(field.name) else {
            continue;
        };
        if value.is_null() {
            continue;
        }
        check_value(field.name, &field.ty, value).and_then(|()| check_bounds(field, value))?;
    }

    let supplied = |name: &str| arguments.get(name).is_some_and(|value| !value.is_null());
    let missing: Vec<&str> = fields
        .iter()
        .filter(|field| field.required && !supplied(field.name))
        .map(|field| field.name)
        .collect();
    if !missing.is_empty() {
        return Err(format!("needs {}.", missing.join(" and ")));
    }
    let served = |name: &str| {
        fields
            .iter()
            .find(|field| field.name == name)
            .is_none_or(|field| field.visibility != Visibility::Hidden)
    };
    for rule in R::rules() {
        check_rule(&rule, &supplied, &served)?;
    }

    let declared: Map<String, Value> = fields
        .iter()
        .filter_map(|field| {
            arguments
                .get(field.name)
                .map(|value| (field.name.to_string(), value.clone()))
        })
        .collect();
    let request: R = serde_json::from_value(Value::Object(declared)).map_err(|error| {
        format!(
            "the arguments do not fit the {} request ({error}).",
            R::NAME
        )
    })?;
    let Value::Object(canonical) = serde_json::to_value(&request)
        .map_err(|error| format!("the request could not be serialized ({error})."))?
    else {
        return Err("the request did not serialize as an object.".into());
    };
    // A null is an absent field. It is handed on as the caller sent it, since
    // a handler has always read it as absent, and the request drops it.
    let names: Vec<&str> = fields.iter().map(|field| field.name).collect();
    let mut handled: HashMap<String, Value> = arguments
        .into_iter()
        .filter(|(key, value)| !names.contains(&key.as_str()) || value.is_null())
        .collect();
    handled.extend(canonical);
    Ok((request, handled))
}

/// The refusal a malformed call gets: what was wrong, and one call that works.
/// The same shape [`crate::verbs`] refuses a malformed option with.
pub fn refusal(tool: &str, problem: &str, example: &Value) -> String {
    format!("{tool}: {problem} Valid form: {tool} {example}")
}

fn describe_value(value: &Value) -> String {
    match value {
        Value::String(text) => format!("the string {}", Value::String(text.clone())),
        Value::Number(number) => format!("the number {number}"),
        Value::Bool(flag) => format!("{flag}"),
        Value::Array(_) => "an array".into(),
        Value::Object(_) => "an object".into(),
        Value::Null => "null".into(),
    }
}

fn type_word(ty: &Ty) -> &'static str {
    match ty {
        Ty::String | Ty::Enum(_) => "a string",
        Ty::Integer => "a whole number",
        Ty::Boolean => "true or false",
        Ty::Object | Ty::StringMap | Ty::Nested(_) => "an object",
        Ty::Array(_) => "an array",
        Ty::Any => "any value",
    }
}

/// Check one value against its type, rewriting an accepted synonym to the
/// served value it means.
fn check_value(name: &str, ty: &Ty, value: &mut Value) -> Result<(), String> {
    let wrong = |value: &Value| {
        format!(
            "{name} must be {}, and it was {}.",
            type_word(ty),
            describe_value(value)
        )
    };
    match ty {
        Ty::String => value.is_string().then_some(()).ok_or_else(|| wrong(value)),
        Ty::Integer => (value.is_u64() || value.is_i64())
            .then_some(())
            .ok_or_else(|| wrong(value)),
        Ty::Boolean => value.is_boolean().then_some(()).ok_or_else(|| wrong(value)),
        Ty::Object => value.is_object().then_some(()).ok_or_else(|| wrong(value)),
        Ty::StringMap => {
            let Some(entries) = value.as_object() else {
                return Err(wrong(value));
            };
            match entries.iter().find(|(_, entry)| !entry.is_string()) {
                Some((key, entry)) => Err(format!(
                    "{name}.{key} must be a string, and it was {}.",
                    describe_value(entry)
                )),
                None => Ok(()),
            }
        }
        Ty::Enum(spec) => {
            let one_of = || {
                spec.values
                    .iter()
                    .chain(spec.full_only)
                    .copied()
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let Some(raw) = value.as_str() else {
                return Err(format!(
                    "{name} must be one of {}, and it was {}.",
                    one_of(),
                    describe_value(value)
                ));
            };
            match spec.canonical(raw) {
                Some(canonical) => {
                    *value = json!(canonical);
                    Ok(())
                }
                None => Err(format!(
                    "{name} must be one of {}, and it was {}.",
                    one_of(),
                    describe_value(value)
                )),
            }
        }
        Ty::Array(items) => {
            let Some(elements) = value.as_array_mut() else {
                return Err(wrong(value));
            };
            for (index, element) in elements.iter_mut().enumerate() {
                check_value(&format!("{name}[{index}]"), items, element)?;
            }
            Ok(())
        }
        Ty::Any => Ok(()),
        Ty::Nested(spec) => {
            if !value.is_object() && !value.is_array() {
                return Err(wrong(value));
            }
            (spec.check)(value).map_err(|problem| format!("{name}: {problem}"))
        }
    }
}

fn check_bounds(field: &Field, value: &Value) -> Result<(), String> {
    let name = field.name;
    if let Some(number) = value.as_i64().or_else(|| value.as_u64().map(|n| n as i64)) {
        let (minimum, maximum) = (field.minimum, field.maximum);
        let out = minimum.is_some_and(|minimum| number < minimum)
            || maximum.is_some_and(|maximum| number > maximum);
        if out {
            return Err(match (minimum, maximum) {
                (Some(minimum), Some(maximum)) => {
                    format!("{name} must be from {minimum} to {maximum}, and it was {number}.")
                }
                (Some(minimum), None) => {
                    format!("{name} must be at least {minimum}, and it was {number}.")
                }
                (None, Some(maximum)) => {
                    format!("{name} must be at most {maximum}, and it was {number}.")
                }
                (None, None) => unreachable!("out of range with no bound"),
            });
        }
    }
    if let Some(text) = value.as_str() {
        let length = text.chars().count() as u64;
        let blank = field.min_length.is_some() && text.trim().is_empty();
        if blank || field.min_length.is_some_and(|minimum| length < minimum) {
            return Err(format!("{name} must not be empty."));
        }
        if let Some(maximum) = field.max_length {
            if text.len() as u64 > maximum {
                return Err(format!(
                    "{name} is {} bytes, over its {maximum}-byte limit.",
                    text.len()
                ));
            }
        }
    }
    if let Some(items) = value.as_array() {
        let count = items.len() as u64;
        if let Some(minimum) = field.min_items {
            if count < minimum {
                return Err(if count == 0 {
                    format!("{name} is empty; it needs at least {minimum}.")
                } else {
                    format!("{name} has {count}; it needs at least {minimum}.")
                });
            }
        }
        if let Some(maximum) = field.max_items {
            if count > maximum {
                return Err(format!("{name} has {count}; it takes at most {maximum}."));
            }
        }
    }
    Ok(())
}

fn spell_sets(sets: &[&[&str]]) -> String {
    let spelled: Vec<String> = sets.iter().map(|set| set.join(" and ")).collect();
    match spelled.as_slice() {
        [] => String::new(),
        [one] => one.clone(),
        [init @ .., last] => format!("{}, or {last}", init.join(", ")),
    }
}

/// Hold a call to one rule. `served` says whether a field is advertised; a
/// refusal names only the ways to call a tool its schema describes, so a
/// hidden alternative, still accepted, is never offered.
fn check_rule(
    rule: &Rule,
    supplied: &dyn Fn(&str) -> bool,
    served: &dyn Fn(&str) -> bool,
) -> Result<(), String> {
    match rule {
        Rule::Select {
            sets,
            continuation,
            several,
        } => {
            let offered: Vec<&[&str]> = sets
                .iter()
                .copied()
                .filter(|set| set.iter().all(|field| served(field)))
                .collect();
            // A set named in part is its own mistake, whatever else is there.
            for set in sets.iter() {
                let named: Vec<&str> = set.iter().copied().filter(|f| supplied(f)).collect();
                if !named.is_empty() && named.len() < set.len() {
                    let missing: Vec<&str> = set.iter().copied().filter(|f| !supplied(f)).collect();
                    return Err(format!(
                        "{} needs {} as well.",
                        named.join(" and "),
                        missing.join(" and ")
                    ));
                }
            }
            let chosen: Vec<&&[&str]> = sets
                .iter()
                .filter(|set| set.iter().all(|field| supplied(field)))
                .collect();
            let continues = continuation.iter().any(|field| supplied(field));
            if chosen.len() > 1 && !several {
                let named: Vec<String> = chosen.iter().map(|set| set.join(" and ")).collect();
                return Err(format!(
                    "takes one target, and this call names {}. Send only one of {}.",
                    named.join(" and "),
                    spell_sets(&offered)
                ));
            }
            if chosen.is_empty() && !continues {
                let continuation = if continuation.is_empty() {
                    String::new()
                } else {
                    format!(", or {} to continue a page", continuation.join(" or "))
                };
                return Err(format!("needs {}{continuation}.", spell_sets(&offered)));
            }
            Ok(())
        }
        Rule::Needs { field, needs } => {
            if supplied(field) && !supplied(needs) {
                return Err(format!("{field} needs {needs} as well."));
            }
            Ok(())
        }
        Rule::Excludes { field, with, why } => {
            if supplied(field) && supplied(with) {
                return Err(format!("{field} and {with} cannot be combined: {why}"));
            }
            Ok(())
        }
    }
}

/// Declare a request type: a struct of optional fields, and its [`Field`]
/// list, from one declaration.
///
/// Each field is written `name: Type { modifier(args), ... }`, where a modifier
/// is a [`Field`] builder method, so the Rust type and the served schema cannot
/// name different fields.
#[macro_export]
macro_rules! typed_request {
    (
        $(#[$meta:meta])*
        $vis:vis struct $name:ident for $tool:expr, example $example:expr, rules $rules:expr, {
            $(
                $field:ident : $ty:ty { $( $modifier:ident ( $($arg:expr),* ) ),* $(,)? }
            ),* $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
        $vis struct $name {
            $(
                #[serde(default, skip_serializing_if = "Option::is_none")]
                pub $field: Option<$ty>,
            )*
        }

        impl $crate::request_schema::TypedRequest for $name {
            const NAME: &'static str = $tool;

            fn fields() -> Vec<$crate::request_schema::Field> {
                vec![
                    $(
                        $crate::request_schema::Field::new(
                            stringify!($field),
                            <$ty as $crate::request_schema::ArgType>::ty(),
                        )
                        $( .$modifier( $($arg),* ) )*
                    ),*
                ]
            }

            fn rules() -> Vec<$crate::request_schema::Rule> {
                $rules
            }

            fn example() -> serde_json::Value {
                $example
            }
        }
    };
}

/// Declare a string enumeration a field takes: its served values, the
/// synonyms a handler has always accepted for them, and whether it reads them
/// case-insensitively.
#[macro_export]
macro_rules! arg_enum {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident {
            $( $variant:ident = $value:literal ),* $(,)?
        }
        full_only [ $( $full_variant:ident = $full_value:literal ),* $(,)? ]
        synonyms [ $( $synonym:literal => $canonical:literal ),* $(,)? ]
        fold_case $fold:literal
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
        $vis enum $name {
            $( #[serde(rename = $value)] $variant, )*
            $( #[serde(rename = $full_value)] $full_variant, )*
        }

        impl $name {
            pub const SPEC: $crate::request_schema::EnumSpec = $crate::request_schema::EnumSpec {
                values: &[ $( $value ),* ],
                full_only: &[ $( $full_value ),* ],
                synonyms: &[ $( ($synonym, $canonical) ),* ],
                fold_case: $fold,
            };

            pub fn as_str(self) -> &'static str {
                match self {
                    $( Self::$variant => $value, )*
                    $( Self::$full_variant => $full_value, )*
                }
            }
        }

        impl $crate::request_schema::ArgType for $name {
            fn ty() -> $crate::request_schema::Ty {
                static SPEC: $crate::request_schema::EnumSpec = $name::SPEC;
                $crate::request_schema::Ty::Enum(&SPEC)
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::arg_enum! {
        enum Side { Left = "left", Right = "right" }
        full_only []
        synonyms ["l" => "left"]
        fold_case true
    }

    crate::typed_request! {
        struct Probe for "probe", example json!({"id": "x"}), rules vec![Rule::Select {
            sets: &[&["id"], &["name"]],
            continuation: &["cursor"],
            several: false,
        }], {
            id: String { doc("The id."), non_empty() },
            name: String {},
            cursor: String { max_len(8) },
            side: Side { default("left") },
            depth: u64 { range(1, 8), default(2) },
            max_output_bytes: u64 { output_budget() },
            tags: Vec<String> { items(1, 3) },
            legacy: bool { hidden() },
            wide: bool { full() },
        }
    }

    fn args(value: Value) -> HashMap<String, Value> {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn the_schema_is_the_declaration() {
        let agent = Probe::schema(Profile::Agent, &|_| None);
        let properties = agent["properties"].as_object().unwrap();
        assert!(!properties.contains_key("legacy"));
        assert!(!properties.contains_key("wide"));
        assert_eq!(properties["side"]["enum"], json!(["left", "right"]));
        assert_eq!(properties["depth"]["maximum"], 8);
        assert_eq!(properties["id"]["description"], "The id.");
        assert_eq!(properties["tags"]["items"]["type"], "string");
        let full = Probe::schema(Profile::Full, &|name| {
            (name == "wide").then(|| "Long words.".to_string())
        });
        assert_eq!(full["properties"]["wide"]["description"], "Long words.");
        assert!(full["properties"].get("legacy").is_none());
    }

    #[test]
    fn a_checked_call_is_the_request_serialized_back() {
        let (request, handled) = check::<Probe>(&args(json!({
            "id": "a", "side": " L ", "max_chars": 3000, "unknown": 1
        })))
        .unwrap();
        assert_eq!(request.side.map(Side::as_str), Some("left"));
        assert_eq!(handled["side"], "left");
        assert_eq!(handled["max_output_bytes"], 3000);
        assert!(!handled.contains_key("max_chars"));
        assert_eq!(
            handled["unknown"], 1,
            "an undeclared key is carried through"
        );
    }

    #[test]
    fn every_violation_is_refused_with_the_fix() {
        for (call, expected) in [
            (json!({"id": "a", "depth": 9}), "depth must be from 1 to 8"),
            (
                json!({"id": "a", "depth": "2"}),
                "depth must be a whole number",
            ),
            (
                json!({"id": "a", "side": "up"}),
                "side must be one of left, right",
            ),
            (json!({"id": ""}), "id must not be empty"),
            (json!({"id": "a", "tags": []}), "tags is empty"),
            (
                json!({"id": "a", "cursor": "123456789"}),
                "over its 8-byte limit",
            ),
            (
                json!({"id": "a", "max_chars": 3000, "max_output_bytes": 4000}),
                "Send only max_output_bytes",
            ),
        ] {
            let error = check::<Probe>(&args(call.clone())).unwrap_err();
            assert!(error.contains(expected), "{call}: {error}");
            assert!(error.contains("Valid form: probe"), "{call}: {error}");
        }
    }

    #[test]
    fn selectors_take_exactly_one_target_or_a_continuation() {
        let supplied = |call: Value| {
            let call = args(call);
            move |name: &str| call.get(name).is_some_and(|value| !value.is_null())
        };
        let rule = &Probe::rules()[0];
        let served = |_: &str| true;
        assert!(check_rule(rule, &supplied(json!({"id": "a"})), &served).is_ok());
        assert!(check_rule(rule, &supplied(json!({"cursor": "c"})), &served).is_ok());
        assert!(check_rule(rule, &supplied(json!({"cursor": "c", "id": "a"})), &served).is_ok());
        let both =
            check_rule(rule, &supplied(json!({"id": "a", "name": "b"})), &served).unwrap_err();
        assert!(both.contains("Send only one of id, or name"), "{both}");
        let none = check_rule(rule, &supplied(json!({})), &served).unwrap_err();
        assert!(none.contains("needs id, or name, or cursor"), "{none}");
    }
}
