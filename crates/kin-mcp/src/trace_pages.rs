// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Lossless, byte-bounded pages of a frozen semantic trace.
//!
//! Ordinary hops remain ordinary `chain` rows. Only a semantic record larger
//! than a page is fragmented; its address and absolute hop identity accompany
//! every UTF-8 fragment. Neither source bodies nor safety readings are dropped.
//! A reply over its budget first has readings that repeat another field of the
//! same reply collapsed to their one copy, listed in `_kin.page.compacted`,
//! unless restating that list on every page would leave a page no room for the
//! answer. Every partial page carries the answer's own verdict.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

const MAX_CURSOR_BYTES: usize = 256;
const MAX_CACHE_ENTRIES: usize = 16;
/// A local model can think for many minutes between a reply and its follow-up
/// call; a continuation must outlive one long turn. The cache stays bounded by
/// its entry and byte limits.
const SNAPSHOT_TTL: Duration = Duration::from_secs(3600);

#[derive(Clone, Copy)]
struct SnapshotLimits {
    snapshot_bytes: usize,
    cache_bytes: usize,
}

impl SnapshotLimits {
    fn for_ceiling(ceiling: Option<u64>) -> Self {
        // A continuation is retained semantic evidence, not another graph.
        // Bound aggregate storage against the actual host/container ceiling.
        // One answer can use the allowance; FIFO eviction bounds competing
        // answers, and expired cursors require an explicit restart. Small hosts keep a
        // useful allowance without spending more than 1/32 of their ceiling.
        const BASE: u64 = 64 * 1024 * 1024;
        let bytes = ceiling.filter(|bytes| *bytes > 0).map_or(BASE, |bytes| {
            (bytes / 256).max(BASE).min((bytes / 32).max(1))
        });
        let cache_bytes = usize::try_from(bytes).unwrap_or(usize::MAX);
        Self {
            snapshot_bytes: cache_bytes,
            cache_bytes,
        }
    }
}

fn snapshot_limits() -> SnapshotLimits {
    static LIMITS: OnceLock<SnapshotLimits> = OnceLock::new();
    *LIMITS.get_or_init(|| SnapshotLimits::for_ceiling(kin_core::memory_pressure::ceiling_bytes()))
}

/// Transport-owned presentation scope, attached before freezing a page so a
/// client-folder warning is measured with the answer and cannot change on resume.
pub const CLIENT_ROOT_ARGUMENT: &str = "__kin_client_root";

/// The same frozen-record transport serves traces, reference answers and
/// impact answers. Records keep their original collection address, including
/// nested samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PageKind {
    Trace,
    References,
    Impact,
    Locate,
}

impl PageKind {
    fn primary(self) -> &'static str {
        match self {
            Self::Trace => "chain",
            Self::References => "references",
            Self::Impact => "affected_callers",
            Self::Locate => "entities",
        }
    }

    fn tool(self) -> &'static str {
        match self {
            Self::Trace => "trace_data_flow",
            Self::References => "find_references",
            Self::Impact => "impact_analysis",
            Self::Locate => "semantic_locate",
        }
    }

    pub(crate) fn collections(self) -> &'static [&'static str] {
        match self {
            // A callers walk answers its reach question first: the block's
            // verdict, then its members, frontier and referrers as rows, and
            // then the chain the walk took to reach them.
            Self::Trace => &[
                "reach.members",
                "reach.name_only",
                "reach.frontier",
                "reach.also_referenced_by",
                "chain",
                "candidates",
                "more_candidates",
            ],
            Self::References => &[
                "references",
                "candidates_by_owner",
                "overridden_by.overriders",
                "overridden_by.override_callers",
                "overridden_by.override_caller_candidates",
                "candidates",
                "interface_dispatch.candidates",
                "call_sites.candidates",
                "call_site_candidates.named",
            ],
            // Every blast-radius bucket is a primary answer in its own right,
            // so each pages whole, in the order a reader acts on them: who
            // calls it, who holds it as a value, what overrides it, what may
            // implement it, who calls an override, who may,
            // what shares its name, which is the whole direct answer, then
            // what reaches it further out, then the per-entity counts and the
            // adjacent records.
            Self::Impact => &[
                "affected_callers",
                crate::handlers::review::IMPACT_VALUE_USES_KEY,
                "affected_overrides",
                crate::impact_sections::INTERFACE_IMPLEMENTATIONS_KEY,
                crate::handlers::implementation_family::OVERRIDE_CALLERS_KEY,
                crate::handlers::implementation_family::OVERRIDE_CALLER_CANDIDATES_KEY,
                crate::impact_sections::SAME_NAME_KEY,
                "affected_dependents",
                "affected_contract_consumers",
                "affected_tests",
                "entity_impacts",
                "affected_work_items",
                "affected_annotations",
                "cross_repo_impact",
            ],
            // A ranking's rows, on whichever arm ranked them: `entities` on
            // the fused arm, `results` on the cosine one.
            Self::Locate => &["entities", "results"],
        }
    }

    /// Parents of nested collections whose remaining fields page as one
    /// reading ahead of their rows, so the first page of a long answer states
    /// what the rows after it add up to.
    fn leading_parents(self) -> &'static [&'static str] {
        match self {
            Self::Trace => &["reach"],
            Self::References | Self::Impact | Self::Locate => &[],
        }
    }

    /// Top-level readings that page ahead of `collection` rather than after
    /// every row. An impact answer's `implementation_family`, which says what
    /// a signature change obliges and where each obligation's rows are, leads
    /// the direct rows; its `sections`, which holds each section's verdict
    /// and the call-site evidence of the direct one, follows them and leads
    /// the dependents.
    fn readings_before(self, collection: &str) -> &'static [&'static str] {
        match (self, collection) {
            // What the answer line of a plain reply is written from leads
            // too: how many rows each list holds in all, what was left out
            // as test code, and whether the dependents were asked for. Read
            // off a later page, they left a small answer unable to say it
            // was whole.
            (Self::Impact, "affected_callers") => &[
                crate::handlers::review::IMPACT_ROW_TOTALS_KEY,
                crate::handlers::review::IMPACT_TESTS_EXCLUDED_KEY,
                crate::handlers::review::IMPACT_ROW_SCOPE_KEY,
                crate::handlers::review::IMPACT_DEPENDENTS_ASKED_KEY,
                crate::handlers::review::IMPACT_VALUE_USES_WITHHELD_KEY,
                crate::impact_sections::SAME_NAME_NOT_HELD_KEY,
                crate::handlers::implementation_family::FAMILY_KEY,
                crate::handlers::implementation_family::FAMILY_NOTE_KEY,
            ],
            (Self::Impact, "affected_dependents") => &[crate::impact_sections::SECTIONS_KEY],
            // Where an unscoped walk's steps sit, and the scope that keeps
            // the largest package, leads the first page: a reader deciding
            // whether to narrow the walk needs it before the rows.
            (Self::Trace, "reach.members") => &[crate::trace_scope::MODULES_KEY],
            // What was asked and how many rows the ranking holds lead its
            // rows, so a first page says what it is a page of. The ranking's
            // own `next_cursor` stays with the trailing readings: it pages the
            // ranking, and is followed only once these pages are drained.
            (Self::Locate, "entities") => &["query", "total_ranked", "page"],
            _ => &[],
        }
    }

    /// Recorded reference rows and their owner/override sections precede the
    /// unresolved candidates and coverage readings. Completing this prefix
    /// says only that those rows were delivered, never that the caller set is
    /// closed under unresolved evidence.
    fn is_reference_answer(self, collection: &str) -> bool {
        self == Self::References
            && matches!(
                collection,
                "references"
                    | "candidates_by_owner"
                    | "overridden_by.overriders"
                    | "overridden_by.override_callers"
            )
    }
}

/// Arguments that only size or present a page. A continuation may change them freely, so
/// they never enter a query digest.
pub const TRANSPORT_CONTROLS: [&str; 5] = [
    "cursor",
    "max_chars",
    "max_response_chars",
    "max_output_bytes",
    "details",
];

/// Whether `arguments` carry `key` with content: present, not null, and not an
/// empty string or list.
pub(crate) fn names(arguments: &std::collections::HashMap<String, Value>, key: &str) -> bool {
    match arguments.get(key) {
        None | Some(Value::Null) => false,
        Some(Value::String(text)) => !text.trim().is_empty(),
        Some(Value::Array(items)) => !items.is_empty(),
        Some(_) => true,
    }
}

/// Caller-owned authority identity and the normalized semantic query. The
/// authority stamp must change when the selected graph, its truth revision,
/// repository or authorization/session scope changes.
///
/// A continuation is a handle. The snapshot a cursor names already froze the
/// target set, filters, ordering and graph, so a call that sends the cursor
/// and names no target is bare: it resumes the snapshot under the query it
/// was frozen with, whatever shaping arguments ride along. A call that does
/// name a target must name the snapshot's, as it always had to.
#[derive(Debug, Clone)]
pub struct Context {
    query: String,
    authority: String,
    /// The transport-owned client folder the answer was presented for. A bare
    /// continuation still binds it, because the frozen pages carry its warning.
    presentation: String,
    /// `Some(kind)` when the call names no target and so continues a `kind`
    /// answer as a handle; `None` when it names one.
    bare: Option<PageKind>,
}

impl Context {
    /// Normalize traversal semantics identically on CLI and MCP. Byte budgets
    /// may change while paging; the question itself may not. A call naming no
    /// `focal`, `target` or question continues the frozen trace as it is.
    pub fn from_arguments(
        arguments: &std::collections::HashMap<String, Value>,
        authority: &Value,
    ) -> Self {
        let named = names(arguments, "focal")
            || names(arguments, "target")
            || crate::outside_graph::question_argument(arguments).is_some();
        let integer = |key: &str, default: u64, maximum: u64| {
            arguments
                .get(key)
                .and_then(Value::as_u64)
                .unwrap_or(default)
                .clamp(1, maximum)
        };
        let text = |key: &str| arguments.get(key).and_then(Value::as_str).map(str::trim);
        let raw_direction = text("direction").unwrap_or("both").to_ascii_lowercase();
        let direction = match raw_direction.as_str() {
            "calls" | "callee" | "callees" | "out" | "outgoing" => "calls",
            "callers" | "caller" | "in" | "incoming" => "callers",
            "both" | "all" | "" => "both",
            other => other,
        };
        let mut query = json!({
            "focal": text("focal"), "target": text("target"),
            "question": crate::outside_graph::question_argument(arguments),
            "direction": direction, "depth": integer("depth", 3, 8),
            "limit_per_step": integer("limit_per_step", crate::remediation::TRACE_DEFAULT_LIMIT_PER_STEP as u64, 25),
            "include_body": arguments.get("include_body").and_then(Value::as_bool)
                .unwrap_or_else(|| !arguments.get("compact").and_then(Value::as_bool).unwrap_or(false)),
            "include_type_edges": arguments.get("include_type_edges").and_then(Value::as_bool).unwrap_or(false),
            "client_root": arguments.get(CLIENT_ROOT_ARGUMENT).and_then(Value::as_str),
        });
        // A scope changes the answer, so it is part of the question. Added only
        // when one is named, so an unscoped trace's cursors are unchanged.
        if names(arguments, crate::trace_scope::SCOPE_ARGUMENT) {
            query["scope"] = json!(text(crate::trace_scope::SCOPE_ARGUMENT));
            query["scope_mode"] = json!(text(crate::trace_scope::SCOPE_MODE_ARGUMENT)
                .and_then(|mode| crate::trace_scope::ScopeMode::parse(mode).ok())
                .unwrap_or_default()
                .as_str());
        }
        Self::new(&query, authority).continuing(PageKind::Trace, arguments, named)
    }

    /// A context that binds exactly `query`: every continuation under it must
    /// carry the same query.
    pub fn new(query: &Value, authority: &Value) -> Self {
        Self {
            query: digest(query),
            authority: digest(authority),
            presentation: digest(&Value::Null),
            bare: None,
        }
    }

    /// Bind the presentation `arguments` carry, and mark a call that names no
    /// target as a bare continuation of a `kind` answer.
    pub(crate) fn continuing(
        mut self,
        kind: PageKind,
        arguments: &std::collections::HashMap<String, Value>,
        names_target: bool,
    ) -> Self {
        self.presentation = digest(&json!(arguments
            .get(CLIENT_ROOT_ARGUMENT)
            .and_then(Value::as_str)));
        self.bare = (!names_target).then_some(kind);
        self
    }
}

fn digest(value: &Value) -> String {
    kin_blobs::digest(&serde_json::to_vec(value).expect("JSON digest input")).to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    v: u8,
    snapshot: uuid::Uuid,
    record: usize,
    field: usize,
    byte: usize,
}

impl Cursor {
    fn encode(&self) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(self).expect("cursor JSON"))
    }

    fn decode(token: &str) -> Result<Self, String> {
        if token.len() > MAX_CURSOR_BYTES {
            return Err("trace cursor is too long; restart without cursor".into());
        }
        let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(token)
            .map_err(|_| "invalid trace cursor; restart without cursor")?;
        let cursor: Self = serde_json::from_slice(&decoded)
            .map_err(|_| "invalid trace cursor; restart without cursor")?;
        if cursor.v != 1 {
            return Err("unsupported trace cursor version; restart without cursor".into());
        }
        Ok(cursor)
    }
}

/// Check the bounded wire syntax without accessing a trace snapshot or graph.
/// Acceptance here does not establish that the cursor is held or fresh; resume
/// must still validate its query, authority, and semantic record offsets.
pub fn validate_cursor_syntax(token: &str) -> Result<(), String> {
    Cursor::decode(token).map(|_| ())
}

#[derive(Debug)]
struct Record {
    collection: &'static str,
    index: usize,
    key: Option<String>,
    fields: Vec<FrozenField>,
    object: bool,
    wire_bytes: usize,
}

#[derive(Debug)]
struct FrozenField {
    name: Option<String>,
    encoding: &'static str,
    encoded: Box<str>,
}

impl FrozenField {
    fn new(name: Option<String>, value: Value) -> Self {
        let (encoding, encoded) = match value {
            Value::String(text) => ("utf8", text),
            value => (
                "json_utf8",
                serde_json::to_string(&value).expect("frozen semantic JSON"),
            ),
        };
        Self {
            name,
            encoding,
            encoded: encoded.into_boxed_str(),
        }
    }

    fn value(&self) -> Value {
        if self.encoding == "utf8" {
            json!(self.encoded)
        } else {
            serde_json::from_str(&self.encoded).expect("frozen semantic JSON")
        }
    }
}

impl Record {
    fn new(collection: &'static str, index: usize, key: Option<String>, value: Value) -> Self {
        let wire_bytes = bytes(&value);
        let object = value.is_object();
        let fields = match value {
            Value::Object(object) => object
                .into_iter()
                .map(|(key, value)| FrozenField::new(Some(key), value))
                .collect(),
            value => vec![FrozenField::new(None, value)],
        };
        Self {
            collection,
            index,
            key,
            fields,
            object,
            wire_bytes,
        }
    }

    /// A record whose value pages as one JSON text: whole when it fits, and
    /// otherwise as fragments of that text with no field name, which a reader
    /// joins and parses exactly like any other fragmented value.
    fn stream(collection: &'static str, index: usize, key: Option<String>, value: Value) -> Self {
        let wire_bytes = bytes(&value);
        Self {
            collection,
            index,
            key,
            fields: vec![FrozenField::new(None, value)],
            object: false,
            wire_bytes,
        }
    }

    fn identity(&self) -> Value {
        let mut address = json!({"collection": self.collection, "index": self.index});
        if let Some(key) = &self.key {
            address["key"] = json!(key);
        }
        let get = |key: &str| {
            self.fields
                .iter()
                .find(|field| field.name.as_deref() == Some(key))
        };
        if let Some(id) = get("entity_id").or_else(|| get("id")) {
            address["entity_id"] = id.value();
            if matches!(self.key.as_deref(), Some("focal" | "focal_entity")) {
                address["step"] = json!(0);
                address["parent_step"] = json!(0);
            }
        }
        if let Some(caller) = get("caller").filter(|field| field.encoding == "utf8") {
            address["caller"] = caller.value();
        }
        for key in ["step", "parent_step"] {
            if let Some(field) = get(key) {
                address[key] = field.value();
            }
        }
        address
    }

    fn retained_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            .saturating_add(self.key.as_ref().map_or(0, String::capacity))
            .saturating_add(
                self.fields
                    .capacity()
                    .saturating_mul(std::mem::size_of::<FrozenField>()),
            )
            .saturating_add(
                self.fields
                    .iter()
                    .map(|field| {
                        field
                            .name
                            .as_ref()
                            .map_or(0, String::capacity)
                            .saturating_add(field.encoded.len())
                            .saturating_add(64)
                    })
                    .sum::<usize>(),
            )
    }

    fn value(&self) -> Value {
        if self.object {
            Value::Object(
                self.fields
                    .iter()
                    .map(|field| (field.name.clone().expect("object field"), field.value()))
                    .collect(),
            )
        } else {
            self.fields[0].value()
        }
    }

    fn append_to(&self, page: &mut Value) {
        match self.collection {
            "readings" => {
                if !page["readings"].is_array() {
                    page["readings"] = json!([]);
                }
                page["readings"].as_array_mut().unwrap().push(json!({
                    "key": self.key, "value": self.value(),
                }));
            }
            collection => {
                let mut slot = page;
                for key in collection.split('.') {
                    slot = &mut slot[key];
                }
                if !slot.is_array() {
                    *slot = json!([]);
                }
                slot.as_array_mut().unwrap().push(self.value());
            }
        }
    }
}

struct Snapshot {
    kind: PageKind,
    id: uuid::Uuid,
    context: Context,
    created: Instant,
    bytes: usize,
    records: Vec<Record>,
    total_steps: usize,
    total_candidates: usize,
    reference_answer_records: usize,
    /// For a callers walk that answers with its reach reading, the records
    /// that answer: the leading readings and every `reach` row, which page
    /// before the display chain. Zero for any other answer.
    trace_answer_records: usize,
    /// What every partial page states about the answer it belongs to. See
    /// [`page_verdict`].
    verdict: Value,
    /// What [`compact_readings`] collapsed before this answer was frozen,
    /// stated on every page so a reader reassembling the readings knows.
    compacted: Vec<Value>,
    // Keep the captured answer's identity on every page, including pages
    // carrying only fragments. The complete safety reading is still paged
    // losslessly; a later transport must not borrow a newer graph observation.
    identity: serde_json::Map<String, Value>,
}

#[derive(Default)]
struct Cache {
    snapshots: VecDeque<Snapshot>,
    bytes: usize,
}

impl Cache {
    fn expire(&mut self, now: Instant) {
        while self
            .snapshots
            .front()
            .is_some_and(|snapshot| now.saturating_duration_since(snapshot.created) >= SNAPSHOT_TTL)
        {
            self.remove_oldest();
        }
    }

    fn remove_oldest(&mut self) {
        if let Some(snapshot) = self.snapshots.pop_front() {
            self.bytes = self.bytes.saturating_sub(snapshot.bytes);
        }
    }

    fn insert(&mut self, snapshot: Snapshot) -> Result<(), String> {
        self.insert_with_budget(snapshot, snapshot_limits().cache_bytes)
    }

    fn insert_with_budget(&mut self, snapshot: Snapshot, budget: usize) -> Result<(), String> {
        if snapshot.bytes > budget {
            return Err("continuation exceeds this host's bounded cache allowance".into());
        }
        self.expire(Instant::now());
        while self.snapshots.len() >= MAX_CACHE_ENTRIES
            || self.bytes.saturating_add(snapshot.bytes) > budget
        {
            self.remove_oldest();
        }
        self.bytes += snapshot.bytes;
        self.snapshots.push_back(snapshot);
        Ok(())
    }
}

fn cache() -> &'static Mutex<Cache> {
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(Cache::default()))
}

fn resident_bytes(value: &Value) -> usize {
    let heap = match value {
        Value::String(text) => text.capacity(),
        Value::Array(items) => items
            .capacity()
            .saturating_mul(std::mem::size_of::<Value>())
            .saturating_add(
                items
                    .iter()
                    .map(|item| resident_bytes(item).saturating_sub(std::mem::size_of::<Value>()))
                    .sum::<usize>(),
            ),
        Value::Object(fields) => fields
            .iter()
            .map(|(key, value)| {
                key.capacity()
                    .saturating_add(resident_bytes(value))
                    .saturating_add(128)
            })
            .sum(),
        _ => 0,
    };
    std::mem::size_of::<Value>().saturating_add(heap)
}

/// The compact encoded length of `value`, counted without building the
/// encoding. `budget::compact_len` counts through `serde_json`'s optimized
/// `Display`, so a workspace built without optimization measures a large
/// answer at the optimized speed.
fn bytes(value: &Value) -> usize {
    crate::budget::compact_len(value).unwrap_or(usize::MAX)
}

/// Whether a response already carries the complete page envelope. Later
/// transports must preserve it, rather than append another unbudgeted reading.
pub fn is_page(payload: &Value) -> bool {
    payload
        .pointer("/_kin/page/version")
        .and_then(Value::as_u64)
        == Some(1)
}

/// The refusal a continuation gets when it names a target other than the one
/// its snapshot froze. It says both ways forward.
pub(crate) const QUERY_CHANGED: &str = "trace cursor query changed; the cursor continues a \
    frozen answer, so send the cursor alone for its next page, or restart without cursor to ask \
    a new query";

/// Resume without repeating graph traversal. The concrete dispatcher samples
/// the current authority before calling this, so an old page cannot silently
/// span a graph write or a different selected graph.
///
/// A bare continuation (see [`Context`]) resumes under the snapshot's own
/// query, and only a snapshot of the same tool, presented for the same client
/// folder. Every continuation, bare or not, is refused when its authority
/// moved.
pub fn resume(token: &str, context: &Context, max_bytes: usize) -> Result<Value, String> {
    let cursor = Cursor::decode(token)?;
    let mut cache = cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    cache.expire(Instant::now());
    let snapshot = cache
        .snapshots
        .iter()
        .find(|snapshot| snapshot.id == cursor.snapshot)
        .ok_or("trace continuation expired or was evicted; restart without cursor")?;
    match context.bare {
        Some(kind) if kind != snapshot.kind => {
            return Err(format!(
                "trace cursor continues an answer from {tool}; send it to {tool}, or restart \
                 without cursor",
                tool = snapshot.kind.tool()
            ));
        }
        Some(_) if snapshot.context.presentation != context.presentation => {
            return Err(
                "trace cursor was frozen for another client folder; restart without cursor".into(),
            );
        }
        Some(_) => {}
        None if snapshot.context.query != context.query => {
            return Err(QUERY_CHANGED.into());
        }
        None => {}
    }
    if snapshot.context.authority != context.authority {
        return Err("trace cursor graph or authority scope changed; restart without cursor".into());
    }
    snapshot.page(cursor, max_bytes)
}

/// Attach the complete trust reading before selecting a response page. The
/// unbounded intermediate is never emitted and the old lossy ladder never
/// discards an entity body or hop from it.
pub fn finalize(
    result: crate::ToolCallResult,
    envelope: crate::Envelope,
    context: Context,
    max_bytes: usize,
) -> crate::ToolCallResult {
    finalize_kind(result, envelope, context, max_bytes, PageKind::Trace, false)
}

pub(crate) fn finalize_kind(
    result: crate::ToolCallResult,
    envelope: crate::Envelope,
    context: Context,
    max_bytes: usize,
    kind: PageKind,
    answer_only: bool,
) -> crate::ToolCallResult {
    let full_budget = crate::budget::ResponseBudget {
        max_chars: usize::MAX,
        envelope_reserve: 0,
        compact: false,
        answer_only,
        ..Default::default()
    };
    let full = crate::envelope::finalize_bounded(result, envelope, kind.tool(), &full_budget);
    let is_error = full.is_error;
    let Some(crate::ContentBlock::Text { text }) = full.content.first() else {
        return crate::ToolCallResult::error("trace response contains no semantic payload");
    };
    let outcome = serde_json::from_str::<Value>(text)
        .map_err(|error| error.to_string())
        .and_then(|mut payload| {
            // This intermediate never shipped. Its intentionally unlimited
            // accounting is not an observation about any emitted page.
            if let Some(envelope) = payload.get_mut("_kin").and_then(Value::as_object_mut) {
                envelope.remove("response");
            }
            start_kind(payload, context, max_bytes, kind)
        });
    match outcome {
        Ok(page) => crate::ToolCallResult {
            content: vec![crate::ContentBlock::Text {
                text: serde_json::to_string(&page).expect("trace page JSON"),
            }],
            is_error,
        },
        Err(error) => crate::ToolCallResult::error(error),
    }
}

/// Page the fully annotated result. The full payload is consumed, so the cache
/// stores one copy of each semantic record. Size is checked before admission.
pub fn start(payload: Value, context: Context, max_bytes: usize) -> Result<Value, String> {
    start_kind(payload, context, max_bytes, PageKind::Trace)
}

pub(crate) fn start_kind(
    mut payload: Value,
    context: Context,
    max_bytes: usize,
    kind: PageKind,
) -> Result<Value, String> {
    // Freeze the public record, never an internal coordinate-bearing record
    // whose fragments could bypass a later presentation boundary.
    crate::semantic_surface::project(kind.tool(), &mut payload);
    if kind == PageKind::Impact {
        collapse_section_candidates(&mut payload);
    }
    let total_steps = payload[kind.primary()].as_array().map_or(0, Vec::len);
    // A complete page has no snapshot behind it, so nothing later can cut it
    // and still hand back the rest. A reference answer is served whole only
    // when its plain presentation fits too; otherwise it is frozen, and the
    // presentation can stop at any record and continue from it.
    let mut whole = None;
    if let Some(complete) = complete_page(&payload, kind, total_steps, max_bytes, &[]) {
        if presentation_fits(&complete, kind, max_bytes) {
            return Ok(complete);
        }
        whole = Some(complete);
    }
    // Too large as annotated. Collapse the readings that repeat another field
    // of this reply first: a small answer whose safety readings restate each
    // other then ships whole, as one complete page with its own verdict, and a
    // large one pages fewer bytes.
    let annotated = payload.clone();
    let compacted = compact_readings(&mut payload, kind);
    if !compacted.is_empty() {
        if let Some(complete) = complete_page(&payload, kind, total_steps, max_bytes, &compacted) {
            if presentation_fits(&complete, kind, max_bytes) {
                return Ok(complete);
            }
            whole.get_or_insert(complete);
        }
    }
    // A page budget too small for a frozen page's restated identity keeps the
    // complete page it already had: the machine reply fits, and the client
    // presentation refuses rather than overruns if its own form does not.
    match start_frozen(
        payload,
        annotated,
        context,
        max_bytes,
        kind,
        total_steps,
        compacted,
    ) {
        Err(_) if whole.is_some() => Ok(whole.expect("checked complete page")),
        outcome => outcome,
    }
}

/// Where an impact section's call-site evidence counts the sites it no
/// longer lists: those whose caller's text Kin never read.
pub(crate) const UNREAD_SITES_KEY: &str = "unread_sites";

/// Shape an impact answer's section evidence for paging, after the answer
/// was graded, so its verdict and every clause stand as computed.
///
/// The direct section carries the call-site reading of its focals, as a
/// reference answer does, and that reading keeps every site whose caller's
/// text Kin never read. On React one impact answer held 29,479 such rows, 6.1
/// MB, which an agent paged through as raw fragments; across 103 scored
/// transcripts none of them held an answer. They are counted here by reason,
/// with the call that lists them: `refs` on the changed entity with
/// `details:true` serves the same reading, site by site. Sites whose code
/// spells a name stay listed, and runs of identical ones collapse to one row
/// with an `occurrences` count that expands back in order.
fn collapse_section_candidates(payload: &mut Value) {
    let Some(sections) = payload
        .get_mut(crate::impact_sections::SECTIONS_KEY)
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    for section in sections.values_mut() {
        let Some(block) = section.get_mut(crate::call_sites::CALL_SITES_KEY) else {
            continue;
        };
        count_unread_sites(block);
        collapse_candidate_runs(block);
        if let Some(focals) = block.get_mut("focals").and_then(Value::as_array_mut) {
            for focal in focals {
                count_unread_sites(focal);
                collapse_candidate_runs(focal);
            }
        }
    }
}

/// Whether a retained site is one whose caller's text Kin never read: no
/// callee and no code, or kept for exactly that reason.
fn unread_site(row: &Value) -> bool {
    let blank = |key: &str| row.get(key).is_none_or(Value::is_null);
    row.get("reason").and_then(Value::as_str)
        == Some(kin_model::call_site_reading::REACH_TEXT_UNKNOWN)
        || blank("callee") && blank("expr")
}

/// Move a block's unread sites into one count by reason, with the call that
/// lists them. The block's own counts, clauses and settlement are unchanged.
fn count_unread_sites(block: &mut Value) {
    let entity = block
        .get("entity_id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let Some(rows) = block.get_mut("candidates").and_then(Value::as_array_mut) else {
        return;
    };
    let mut by_reason: BTreeMap<String, u64> = BTreeMap::new();
    rows.retain(|row| {
        if !unread_site(row) {
            return true;
        }
        let reason = row
            .get("expr_unavailable")
            .or_else(|| row.get("reason"))
            .and_then(Value::as_str)
            .unwrap_or("callee_name_unknown")
            .replace(' ', "_");
        let entries = row.get("occurrences").and_then(Value::as_u64).unwrap_or(1);
        *by_reason.entry(reason).or_default() += entries;
        false
    });
    if by_reason.is_empty() {
        return;
    }
    let count: u64 = by_reason.values().sum();
    let target = entity.map_or_else(
        || "the changed entity".to_string(),
        |id| format!("{{\"entity_id\":\"{id}\"}}"),
    );
    block[UNREAD_SITES_KEY] = json!({
        "count": count,
        "by_reason": by_reason,
        "why": format!(
            "Sites whose caller's text Kin never read, so none is an observed call and any \
             could still add a caller; the section's verdict counts them. refs {target} with \
             details:true lists them."
        ),
    });
}

fn collapse_candidate_runs(block: &mut Value) {
    let Some(rows) = block.get_mut("candidates").and_then(Value::as_array_mut) else {
        return;
    };
    let mut runs: Vec<(Value, u64)> = Vec::with_capacity(rows.len());
    for row in rows.drain(..) {
        let countable = row.is_object() && row.get("occurrences").is_none();
        match runs.last_mut() {
            Some((previous, count)) if countable && *previous == row => *count += 1,
            _ => runs.push((row, 1)),
        }
    }
    rows.extend(runs.into_iter().map(|(mut row, count)| {
        if count > 1 {
            row["occurrences"] = json!(count);
        }
        row
    }));
}

/// Bytes a plain reference page leaves for what the connection adds after
/// paging: a note on a separate line, a warning about the client folder.
const PRESENTATION_RESERVE_BYTES: usize = 512;

/// Whether a page's agent presentation also fits `max_bytes`. Reference
/// answers and walks are presented in plain words, which state reasons and
/// limits the machine page does not. Details are measured for a reference
/// answer because they list everything the short form counts, and a walk's
/// plain form before its rows are shortened, so the larger form is measured.
fn presentation_fits(page: &Value, kind: PageKind, max_bytes: usize) -> bool {
    // Machine pages only: a plain impact or locate reply sizes itself from
    // the whole machine answer and is never served one of these pages.
    if matches!(kind, PageKind::Impact | PageKind::Locate) {
        return true;
    }
    let Ok(text) = serde_json::to_string(page) else {
        return true;
    };
    let mut presented = json!({"content": [{"type": "text", "text": text}]});
    crate::plain_reply::present_query(&mut presented, kind.tool(), true);
    let size = presented
        .pointer("/content/0/text")
        .and_then(Value::as_str)
        .map_or(0, str::len);
    size.saturating_add(PRESENTATION_RESERVE_BYTES) <= max_bytes
}

fn start_frozen(
    payload: Value,
    annotated: Value,
    context: Context,
    max_bytes: usize,
    kind: PageKind,
    total_steps: usize,
    compacted: Vec<Value>,
) -> Result<Value, String> {
    let mut snapshot = freeze(payload, context.clone(), kind, total_steps, compacted)?;
    // Every partial page restates the compaction index beside the answer's
    // identity and verdict. At a budget too small for that restatement and the
    // smallest fragment of a record together, the index would stop the answer
    // partway, so the answer pages as it was annotated instead: every reading
    // whole, and nothing to restore.
    if !snapshot.compacted.is_empty() && snapshot.fragment_page_bound(max_bytes) > max_bytes {
        if let Ok(whole) = freeze(annotated, context, kind, total_steps, Vec::new()) {
            snapshot = whole;
        }
    }
    let cursor = Cursor {
        v: 1,
        snapshot: snapshot.id,
        record: 0,
        field: 0,
        byte: 0,
    };
    let page = snapshot.page(cursor, max_bytes)?;
    cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(snapshot)?;
    Ok(page)
}

/// Freeze an answer too large for one page into the records its pages carry.
/// `compacted` is what [`compact_readings`] already collapsed in `payload`.
fn freeze(
    payload: Value,
    context: Context,
    kind: PageKind,
    total_steps: usize,
    compacted: Vec<Value>,
) -> Result<Snapshot, String> {
    freeze_with_limit(
        payload,
        context,
        kind,
        total_steps,
        compacted,
        snapshot_limits().snapshot_bytes,
    )
}

fn freeze_with_limit(
    mut payload: Value,
    context: Context,
    kind: PageKind,
    total_steps: usize,
    compacted: Vec<Value>,
    limit: usize,
) -> Result<Snapshot, String> {
    let size = bytes(&payload);
    if size > limit {
        return Err(format!(
            "{} needs {size} encoded bytes, above this host's {limit}-byte continuation allowance; narrow the query", kind.tool()
        ));
    }
    let verdict = page_verdict(payload.pointer("/_kin/verdict"), kind);
    let mut identity: serde_json::Map<_, _> = [
        "repository",
        "graph_as_of",
        "answered_by",
        "runtime",
        "semantic_surface",
        "binding_coverage",
    ]
    .into_iter()
    .filter_map(|key| {
        payload
            .get("_kin")?
            .get(key)
            .map(|value| (key.to_owned(), value.clone()))
    })
    .collect();
    if kind == PageKind::Locate {
        // Every page names the ranking it is part of, so a later page read
        // alone still says what question its rows answer.
        let question: serde_json::Map<String, Value> = ["query", "granularity", "routing"]
            .into_iter()
            .filter_map(|key| payload.get(key).map(|value| (key.into(), value.clone())))
            .collect();
        identity.insert("locate_question".into(), Value::Object(question));
    }
    if kind == PageKind::Trace {
        let question: serde_json::Map<String, Value> =
            ["direction", "depth", "limit_per_step", "focal_name"]
                .into_iter()
                .filter_map(|key| payload.get(key).map(|value| (key.into(), value.clone())))
                .collect();
        identity.insert("trace_question".into(), Value::Object(question));
    }
    if kind == PageKind::References
        && payload.get("view").and_then(Value::as_str) != Some("overrides")
    {
        for key in ["caller_scope", "relation_kinds"] {
            if let Some(scope) = payload.get(key) {
                identity.insert(key.into(), scope.clone());
            }
        }
        // Candidates precede the full focal reading. Every ordinary page
        // still needs the exact graph-selected target to identify its answer.
        // Retain identity only here; the complete focal record still pages.
        if let Some(focal) = payload
            .get("focal_entity")
            .and_then(reference_focal_identity)
        {
            identity.insert("reference_focal".into(), focal);
        }
        // The complete source report is a later reading. Its canonical
        // binding blocker remains actionable beside each page's verdict.
        if let Some(gap) = payload.get("_kin").and_then(|envelope| {
            envelope
                .get("reference_gap")
                .cloned()
                .or_else(|| crate::plain_reply::reference_binding_gap(envelope))
        }) {
            identity.insert("reference_gap".into(), gap);
        }
    }
    if kind == PageKind::References
        && payload.get("view").and_then(Value::as_str) == Some("overrides")
    {
        // A continuation can contain only override rows or a fragment. Repeat
        // the focused question and its measured scope rather than borrowing
        // caller-delivery semantics. Full original fields still page below.
        let mut scope = serde_json::Map::new();
        for key in [
            "count",
            "proven",
            "coverage",
            "bounded",
            "nested_definitions",
        ] {
            if let Some(value) = payload
                .get("overridden_by")
                .and_then(|block| block.get(key))
            {
                scope.insert(key.to_owned(), value.clone());
            }
        }
        identity.insert(
            "reference_view".into(),
            json!({
                "view": "overrides",
                "focal_entity": payload.get("focal_entity").cloned().unwrap_or(Value::Null),
                "overridden_by": scope,
            }),
        );
    }
    let mut records = Vec::new();
    let mut total_candidates = 0;
    let mut leading_readings = 0usize;
    let mut led: Vec<&str> = Vec::new();
    let fields = payload
        .as_object_mut()
        .ok_or("trace result is not an object")?;
    for &collection in kind.collections() {
        for &key in kind.readings_before(collection) {
            if let Some(value) = fields.remove(key) {
                records.push(Record::new(
                    "readings",
                    leading_readings,
                    Some(key.to_string()),
                    value,
                ));
                leading_readings += 1;
            }
        }
        if let Some((parent, _)) = collection.split_once('.') {
            if kind.leading_parents().contains(&parent) {
                if led.contains(&parent) {
                    continue;
                }
                led.push(parent);
                let Some(Value::Object(mut object)) = fields.remove(parent) else {
                    continue;
                };
                // Every nested collection of this parent, rows kept apart from
                // the remainder, which leads them as one reading.
                let mut nested: Vec<(&'static str, Vec<Value>)> = Vec::new();
                for &inner in kind.collections() {
                    let Some((inner_parent, key)) = inner.split_once('.') else {
                        continue;
                    };
                    if inner_parent != parent {
                        continue;
                    }
                    if object
                        .get(key)
                        .and_then(Value::as_array)
                        .is_some_and(|rows| !rows.is_empty())
                    {
                        if let Some(Value::Array(rows)) = object.remove(key) {
                            nested.push((inner, rows));
                        }
                    }
                }
                records.push(Record::new(
                    "readings",
                    leading_readings,
                    Some(parent.to_string()),
                    Value::Object(object),
                ));
                leading_readings += 1;
                for (inner, rows) in nested {
                    for (index, value) in rows.into_iter().enumerate() {
                        records.push(Record::new(inner, index, None, value));
                    }
                }
                continue;
            }
        }
        let present = if let Some((parent, key)) = collection.split_once('.') {
            fields.get(parent).and_then(|value| value.get(key))
        } else {
            fields.get(collection)
        };
        // Empty and non-array fields remain among the complete readings.
        // Removing them without creating records would lose their identity.
        if present.is_none_or(|value| value.as_array().is_none_or(|rows| rows.is_empty())) {
            continue;
        }
        let value = if let Some((parent, key)) = collection.split_once('.') {
            fields
                .get_mut(parent)
                .and_then(Value::as_object_mut)
                .and_then(|nested| nested.remove(key))
        } else {
            fields.remove(collection)
        };
        if let Some(Value::Array(rows)) = value {
            for (index, value) in rows.into_iter().enumerate() {
                let served_collection = collection;
                if served_collection != kind.primary() {
                    total_candidates += 1;
                }
                records.push(Record::new(served_collection, index, None, value));
            }
        }
    }
    if let Some(target) = fields
        .get_mut("target_ambiguity")
        .and_then(Value::as_object_mut)
        .filter(|target| {
            target
                .get("candidates")
                .and_then(Value::as_array)
                .is_some_and(|rows| !rows.is_empty())
        })
    {
        if let Some(Value::Array(rows)) = target.remove("candidates") {
            for (index, value) in rows.into_iter().enumerate() {
                total_candidates += 1;
                records.push(Record::new("target_candidates", index, None, value));
            }
        }
    }
    for (index, (key, value)) in std::mem::take(fields).into_iter().enumerate() {
        // A reference answer's or walk's readings past its rows are its
        // envelope and evidence blocks. One too large for a page is streamed as one JSON
        // text, so its fragments fill pages instead of spending a page on
        // every field, however small. The focal reading keeps its fields, so
        // each of its fragments names the entity it belongs to.
        let streamed = matches!(kind, PageKind::References | PageKind::Trace)
            && value.is_object()
            && !matches!(key.as_str(), "focal" | "focal_entity");
        let record = if streamed {
            Record::stream("readings", leading_readings + index, Some(key), value)
        } else {
            Record::new("readings", leading_readings + index, Some(key), value)
        };
        records.push(record);
    }
    let retained_size = records
        .iter()
        .map(|record| {
            record
                .retained_bytes()
                .saturating_sub(std::mem::size_of::<Record>())
        })
        .sum::<usize>()
        .saturating_add(
            records
                .capacity()
                .saturating_mul(std::mem::size_of::<Record>()),
        )
        .saturating_add(std::mem::size_of::<Snapshot>() + 512)
        .saturating_add(context.query.capacity())
        .saturating_add(context.authority.capacity())
        .saturating_add(context.presentation.capacity())
        .saturating_add(resident_bytes(&Value::Object(identity.clone())))
        .saturating_add(resident_bytes(&verdict))
        .saturating_add(
            compacted
                .capacity()
                .saturating_mul(std::mem::size_of::<Value>()),
        )
        .saturating_add(
            compacted
                .iter()
                .map(|value| resident_bytes(value).saturating_sub(std::mem::size_of::<Value>()))
                .sum::<usize>(),
        );
    if retained_size > limit {
        return Err(format!("{} needs {retained_size} retained bytes, above this host's {limit}-byte continuation allowance; narrow the query", kind.tool()));
    }
    let reference_answer_records = records
        .iter()
        .take_while(|record| kind.is_reference_answer(record.collection))
        .count();
    let trace_answer_records = if kind == PageKind::Trace
        && records.iter().any(|record| {
            record.collection == "readings"
                && record.key.as_deref() == Some(crate::trace_reach::REACH_KEY)
        }) {
        records
            .iter()
            .take_while(|record| {
                record.collection.starts_with("reach.")
                    || (record.collection == "readings"
                        && matches!(
                            record.key.as_deref(),
                            Some(crate::trace_reach::REACH_KEY | crate::trace_scope::MODULES_KEY)
                        ))
            })
            .count()
    } else {
        0
    };
    Ok(Snapshot {
        kind,
        id: uuid::Uuid::new_v4(),
        context,
        created: Instant::now(),
        bytes: retained_size,
        records,
        total_steps,
        total_candidates,
        reference_answer_records,
        trace_answer_records,
        verdict,
        compacted,
        identity,
    })
}

/// Graph-held coordinates only: no source lookup or name/file inference.
/// Strings remain exact. An identity too large for a page is an honest
/// irreducible-metadata error, never a shortened or guessed target.
fn reference_focal_identity(focal: &Value) -> Option<Value> {
    let mut identity = serde_json::Map::new();
    for (name, keys) in [
        ("entity_id", &["entity_id", "id"][..]),
        ("name", &["name", "entity_name"][..]),
        ("kind", &["kind", "entity_kind"][..]),
        ("qualified_name", &["qualified_name"][..]),
    ] {
        if let Some(value) = keys
            .iter()
            .find_map(|key| focal.get(*key).filter(|value| value.is_string()))
        {
            identity.insert(name.into(), value.clone());
        }
    }
    let file = [
        focal.pointer("/projection/path"),
        focal.get("read_path"),
        focal.get("file_path"),
        focal.get("file"),
        focal.get("entity_file"),
    ]
    .into_iter()
    .flatten()
    .find(|value| value.is_string());
    if let Some(file) = file {
        identity.insert("file".into(), file.clone());
    }
    identity
        .contains_key("entity_id")
        .then_some(Value::Object(identity))
}

/// The whole reply as one complete page, when it fits `max_bytes`.
///
/// `compacted` is what [`compact_readings`] collapsed to get here, stated on
/// the page block so a reader knows which fields to follow to their one copy.
fn complete_page(
    payload: &Value,
    kind: PageKind,
    total_steps: usize,
    max_bytes: usize,
    compacted: &[Value],
) -> Option<Value> {
    // The page block only adds bytes, so a payload over the ceiling on its own
    // cannot fit, and a large one is measured once rather than copied first.
    if bytes(payload) > max_bytes {
        return None;
    }
    let mut complete = payload.clone();
    // A locate answer's `next_cursor` is its ranking's, which a whole page
    // still owes its reader. Every other paged answer's cursor is the pager's.
    if kind != PageKind::Locate {
        complete["next_cursor"] = Value::Null;
    }
    complete["_kin"]["page"] = match kind {
        PageKind::Trace => json!({
            "version": 1, "complete": true, "has_more": false,
            "total_steps": total_steps, "returned_steps": total_steps,
        }),
        PageKind::References => json!({
            "version": 1, "kind": "references", "complete": true, "has_more": false,
            "total_references": total_steps, "returned_references": total_steps,
            "answer_rows_complete": true, "answer_rows_has_more": false,
            "evidence_has_more": false,
        }),
        PageKind::Impact => {
            let rows = impact_rows(payload);
            json!({
                "version": 1, "kind": "impact", "complete": true, "has_more": false,
                "total_rows": rows, "returned_rows": rows,
            })
        }
        PageKind::Locate => {
            let rows = locate_rows(payload);
            json!({
                "version": 1, "kind": "locate", "complete": true, "has_more": false,
                "total_rows": rows, "returned_rows": rows,
            })
        }
    };
    if !compacted.is_empty() {
        complete["_kin"]["page"]["compacted"] = json!(compacted);
    }
    set_wire_size(&mut complete, max_bytes);
    (bytes(&complete) <= max_bytes).then_some(complete)
}

/// Placeholder text a compacted field puts where it restated another field:
/// `{negative.subject}` stands for that field's value in the same reply.
fn field_reference(path: &str) -> String {
    format!("{{{path}}}")
}

/// Collapse reading content this reply states twice, keeping the one
/// canonical copy, and return what was collapsed.
///
/// A fully annotated reply carries the answer and every safety reading about
/// it, and several readings restate each other. Measured on a Python web
/// framework: a `find_references` answer with one reference was 12,239 bytes,
/// of which the answer (the rows, the focal entity and the counts) was about
/// 850. The rest were safety readings, and among them the source-derivation
/// observation appeared twice, byte for byte, and the negative's advice
/// repeated its own subject, the graph identity and every limiting clause its
/// trust reason already states. The reply paged at the default 12,000-byte
/// budget, and every page of it read as a partial answer.
///
/// Three rules, and only these. Each removes text that exists elsewhere in the
/// same reply, so nothing is lost that the reply does not still say:
///
/// 1. A top-level field equal to the envelope's field of the same name is
///    dropped; `_kin` holds the copy.
/// 2. A `negative` field equal to the envelope's field of the same name is
///    dropped; `_kin` holds the copy.
/// 3. `negative.advice` keeps its own sentence and replaces what it restates
///    with a `{path}` reference to the field holding it: a leading copy of
///    `negative.subject`, the rendered `_kin.graph_as_of`, and a trailing copy
///    of `negative.trust_reason`. The limiting clause codes stay in the advice
///    itself, so the sentence a reader acts on still names its cause.
///
/// Every rule compares exact values or exact text, so a reading that differs in
/// any byte is kept. Nothing reads a language, a tool name or a label. A
/// collection the pager serves as rows is never touched, and neither is any
/// field whose meaning is not repeated: a long explanation that exists once in
/// the reply stays.
fn compact_readings(payload: &mut Value, kind: PageKind) -> Vec<Value> {
    let mut compacted = Vec::new();
    let Some(fields) = payload.as_object_mut() else {
        return compacted;
    };
    let Some(envelope) = fields
        .get(crate::envelope::ENVELOPE_KEY)
        .and_then(Value::as_object)
        .cloned()
    else {
        return compacted;
    };
    let rows: Vec<&str> = kind
        .collections()
        .iter()
        .filter_map(|collection| (!collection.contains('.')).then_some(*collection))
        .collect();

    let duplicated: Vec<String> = fields
        .iter()
        .filter(|(key, value)| {
            key.as_str() != crate::envelope::ENVELOPE_KEY
                && !rows.contains(&key.as_str())
                && envelope.get(key.as_str()) == Some(*value)
        })
        .map(|(key, _)| key.clone())
        .collect();
    for key in duplicated {
        let entry = json!({"path": key, "same_as": format!("_kin.{key}")});
        if dropping_saves(&fields[&key], &key, &entry) {
            fields.remove(&key);
            compacted.push(entry);
        }
    }

    if let Some(negative) = fields
        .get_mut(crate::negative::NEGATIVE_KEY)
        .and_then(Value::as_object_mut)
    {
        let duplicated: Vec<String> = negative
            .iter()
            .filter(|(key, value)| envelope.get(key.as_str()) == Some(*value))
            .map(|(key, _)| key.clone())
            .collect();
        for key in duplicated {
            let entry =
                json!({"path": format!("negative.{key}"), "same_as": format!("_kin.{key}")});
            if dropping_saves(&negative[&key], &key, &entry) {
                negative.remove(&key);
                compacted.push(entry);
            }
        }
        if let Some(entry) = compact_advice(negative, &envelope) {
            compacted.push(entry);
        }
    }
    compacted
}

/// Whether dropping `value` under `key` saves more bytes than the entry that
/// records the drop costs. A duplicate shorter than its own disclosure stays.
fn dropping_saves(value: &Value, key: &str, entry: &Value) -> bool {
    bytes(value).saturating_add(key.len() + 4) > bytes(entry)
}

/// Rule 3 of [`compact_readings`]: the advice without what it restates.
fn compact_advice(
    negative: &mut serde_json::Map<String, Value>,
    envelope: &serde_json::Map<String, Value>,
) -> Option<Value> {
    let original = negative.get("advice").and_then(Value::as_str)?;
    let mut advice = original.to_string();
    let mut restates: Vec<&str> = Vec::new();

    if let Some(subject) = negative.get("subject").and_then(Value::as_str) {
        let reference = field_reference("negative.subject");
        if subject.len() > reference.len() && advice.starts_with(subject) {
            advice = format!("{reference}{}", &advice[subject.len()..]);
            restates.push("negative.subject");
        }
    }
    if let Some(as_of) = envelope.get("graph_as_of").filter(|value| !value.is_null()) {
        let rendered = as_of.to_string();
        let reference = field_reference("_kin.graph_as_of");
        if rendered.len() > reference.len() && advice.contains(&rendered) {
            advice = advice.replacen(&rendered, &reference, 1);
            restates.push("_kin.graph_as_of");
        }
    }
    if let Some(reason) = negative
        .get("trust_reason")
        .and_then(Value::as_str)
        .filter(|reason| !reason.is_empty())
    {
        if let Some(head) = advice.strip_suffix(reason) {
            let reference = field_reference("negative.trust_reason");
            let replacement = match clause_codes(reason) {
                Some(codes) => format!("{codes} ({reference})"),
                None => reference,
            };
            if replacement.len() < reason.len() {
                advice = format!("{head}{replacement}");
                restates.push("negative.trust_reason");
            }
        }
    }

    if restates.is_empty() {
        return None;
    }
    negative.insert("advice".to_string(), json!(advice));
    Some(json!({"path": "negative.advice", "restates": restates}))
}

/// The clause codes of a joined trust reason, in order, as the verdict's own
/// limiting factor spells them, or `None` when any clause does not open with a
/// code. A reason that cannot be split cleanly is referenced whole instead of
/// being summarized wrongly.
fn clause_codes(reason: &str) -> Option<String> {
    let codes: Option<Vec<&str>> = reason
        .split(crate::verdict::CLAUSE_SEPARATOR)
        .map(|clause| {
            let (code, _) = clause.split_once(": ")?;
            (!code.is_empty()
                && code
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_'))
            .then_some(code)
        })
        .collect();
    codes.map(|codes| codes.join(crate::verdict::CLAUSE_SEPARATOR))
}

/// The single clause a partial trace page names as its limiting factor when
/// the answer it belongs to carried no verdict of its own.
///
/// Readers split a verdict's limiting factor on the clause separator, so the
/// clause must not contain it, or it would read as a labelled clause plus an
/// unlabelled fragment.
const TRACE_PAGE_PARTIAL: &str = "trace_page_partial: this page is not the complete trace, and every original safety reading remains in the paged readings";

/// The single clause a partial reference page names as its limiting factor
/// when the answer it belongs to carried no verdict of its own.
const REFERENCE_PAGE_PARTIAL: &str = "reference_page_partial: this page is not the complete answer, and every original safety reading remains in the paged readings";

/// The single clause a partial impact page names as its limiting factor.
const IMPACT_PAGE_PARTIAL: &str = "impact_page_partial: this page is not the complete impact answer, and every original safety reading remains in the paged readings";

/// The single clause a partial locate page names as its limiting factor.
const LOCATE_PAGE_PARTIAL: &str = "locate_page_partial: this page is not the complete ranking page, and every original safety reading remains in the paged readings";

/// Rows a locate answer ranks, on whichever arm ranked them.
fn locate_rows(payload: &Value) -> usize {
    PageKind::Locate
        .collections()
        .iter()
        .map(|key| payload[*key].as_array().map_or(0, Vec::len))
        .sum()
}

/// Rows across every blast-radius bucket of an impact answer, which is what an
/// impact page counts in place of a trace's steps.
fn impact_rows(payload: &Value) -> usize {
    PageKind::Impact
        .collections()
        .iter()
        .map(|key| payload[*key].as_array().map_or(0, Vec::len))
        .sum()
}

/// The verdict every partial page of one answer carries.
///
/// It is the answer's own verdict: the state and limiting factor the complete
/// reply was given, not a stamp the transport invents. Stamping every page
/// `inconclusive` made a paged answer read as refused although the answer
/// itself was not, and a reader who took the page's verdict at its word acted
/// on the transport rather than on the graph. That the page carries only part
/// of the answer is a different fact, and the page states it in its own
/// fields: `_kin.page.complete` false, `_kin.completeness` partial with the
/// page's limit, and `next_cursor` while more remains.
///
/// One field is not the answer's. A page never concludes absence, so
/// `safe_to_conclude_absent` is false on every partial page, and an answer
/// whose verdict claimed an authoritative absence has that claim held back
/// (`not_claimed`) until a reader holds every page.
///
/// An answer with no verdict has nothing to carry, and its pages keep the
/// transport's own qualification: inconclusive, limited by the page.
fn page_verdict(original: Option<&Value>, kind: PageKind) -> Value {
    let Some((original, state)) = original.and_then(|verdict| {
        verdict
            .get("state")
            .and_then(Value::as_str)
            .map(|state| (verdict, state))
    }) else {
        return json!({
            "state": "inconclusive", "absence_claim": "not_claimed",
            "safe_to_conclude_absent": false,
            "limiting_factor": match kind {
                PageKind::Trace => TRACE_PAGE_PARTIAL,
                PageKind::References => REFERENCE_PAGE_PARTIAL,
                PageKind::Impact => IMPACT_PAGE_PARTIAL,
                PageKind::Locate => LOCATE_PAGE_PARTIAL,
            },
        });
    };
    let absence_claim = match original.get("absence_claim").and_then(Value::as_str) {
        Some("authoritative") | None => "not_claimed",
        Some(claim) => claim,
    };
    let mut verdict = json!({
        "state": state, "absence_claim": absence_claim,
        "safe_to_conclude_absent": false,
        "limiting_factor": original.get("limiting_factor").cloned().unwrap_or(Value::Null),
    });
    // An impact answer's verdict is its direct section's, and every page says
    // so and carries both sections' states, so a page read alone never takes
    // one section's verdict for the other's.
    if kind == PageKind::Impact {
        for key in [
            crate::impact_sections::SCOPE_KEY,
            crate::impact_sections::SECTIONS_KEY,
        ] {
            if let Some(value) = original.get(key) {
                verdict[key] = value.clone();
            }
        }
    }
    verdict
}

impl Snapshot {
    /// The most bytes a page of this snapshot can need to carry the smallest
    /// piece of any record: one character of the field with the longest name,
    /// in the record with the longest address, at offsets as large as the
    /// frozen answer. A budget this bound fits pages every record to the end;
    /// a smaller one may stop on the first record that neither fits whole nor
    /// leaves room for a fragment.
    fn fragment_page_bound(&self, max_bytes: usize) -> usize {
        let separate = self.fragment_page_bound_with_reference_delivery(max_bytes, true);
        if self.kind == PageKind::References && separate > max_bytes {
            self.fragment_page_bound_with_reference_delivery(max_bytes, false)
        } else {
            separate
        }
    }

    fn fragment_page_bound_with_reference_delivery(
        &self,
        max_bytes: usize,
        separate_reference_delivery: bool,
    ) -> usize {
        let longest_field = |record: &Record| {
            record
                .fields
                .iter()
                .filter_map(|field| field.name.as_ref())
                .max_by_key(|key| key.len())
                .cloned()
        };
        let Some(record) = self.records.iter().max_by_key(|record| {
            bytes(&record.identity())
                .saturating_add(longest_field(record).map_or(0, |field| field.len()))
        }) else {
            return 0;
        };
        let far = Cursor {
            v: 1,
            snapshot: self.id,
            record: self.records.len(),
            field: self
                .records
                .iter()
                .map(|record| record.fields.len())
                .max()
                .unwrap_or(1),
            byte: self.bytes,
        };
        let mut trial = self.shell_with(&far, &far, separate_reference_delivery, true);
        let mut fragment = record.identity();
        fragment["field"] = json!(longest_field(record));
        fragment["encoding"] = json!("json_utf8");
        fragment["byte_offset"] = json!(far.byte);
        fragment["total_bytes"] = json!(far.byte);
        fragment["field_complete"] = json!(false);
        fragment["record_complete"] = json!(false);
        // One character, at the six bytes a JSON escape spells one with.
        fragment["text"] = json!("\u{1}");
        trial["record_fragment"] = fragment;
        // A total past every record keeps the continuation on the trial page.
        update_page(&mut trial, &far, usize::MAX, max_bytes);
        if self.kind == PageKind::References
            && separate_reference_delivery
            && self.reference_answer_records > 0
        {
            // A page still inside the recorded prefix can carry both handles.
            // Measure both at the widest offsets, even though the far cursor
            // used for this bound is beyond that prefix.
            trial["answer_next_cursor"] = json!(far.encode());
            set_wire_size(&mut trial, max_bytes);
        }
        bytes(&trial)
    }

    /// The page every record of this cursor's page is added to. The first
    /// page states the answer's whole identity; a later page restates only
    /// what reading it needs. See [`Self::shell_with`].
    fn shell(&self, cursor: &Cursor, next: &Cursor, separate_reference_delivery: bool) -> Value {
        let first = cursor.record == 0 && cursor.field == 0 && cursor.byte == 0;
        self.shell_with(cursor, next, separate_reference_delivery, first)
    }

    /// Identity a reference continuation page does not restate: the binding
    /// proof and the gap it explains. The first page carries them, and so
    /// does the answer's own envelope, which pages once as a reading. Every
    /// page keeps the verdict and its code, its own partial qualification,
    /// its counts and cursors, and the repository and graph it belongs to.
    const FIRST_PAGE_ONLY: [&'static str; 2] = ["binding_coverage", "reference_gap"];

    fn shell_with(
        &self,
        cursor: &Cursor,
        next: &Cursor,
        separate_reference_delivery: bool,
        full_identity: bool,
    ) -> Value {
        let has_more = next.record < self.records.len();
        let mut page = json!({
            "chain": [],
            "next_cursor": has_more.then(|| next.encode()),
            "_kin": {
                "envelope_version": 2,
                "runtime": "trace-snapshot",
                "verdict": self.verdict,
                "completeness": {
                    "status": "partial", "bound": "at_least",
                    "counted": {"exact": false}, "limits": ["trace_page_partial"],
                },
                "page": {
                    "version": 1, "complete": false, "has_more": has_more,
                    "record_offset": cursor.record, "field_offset": cursor.field, "byte_offset": cursor.byte,
                    "next_record": next.record, "next_field": next.field, "next_byte": next.byte,
                    "total_records": self.records.len(),
                    "total_steps": self.total_steps,
                    "total_candidates": self.total_candidates,
                },
            },
            "negative": {
                "safe_to_conclude_absent": false,
                "interpretation": "qualified_answer", "subject": "trace_page",
            },
        });
        if !self.compacted.is_empty() {
            page["_kin"]["page"]["compacted"] = json!(self.compacted);
        }
        // A callers walk answers with its reach rows, which lead the
        // snapshot; every page says where they end, so a page past them reads
        // as the display chain it is, not as another answer.
        if self.kind == PageKind::Trace && self.trace_answer_records > 0 {
            page["_kin"]["page"]["total_answer_records"] = json!(self.trace_answer_records);
        }
        if self.kind == PageKind::References {
            page.as_object_mut().unwrap().remove("chain");
            page["references"] = json!([]);
            page["_kin"]["runtime"] = json!("reference-snapshot");
            page["_kin"]["completeness"]["limits"] = json!(["reference_page_partial"]);
            page["_kin"]["page"]
                .as_object_mut()
                .unwrap()
                .remove("total_steps");
            page["_kin"]["page"]["kind"] = json!("references");
            page["_kin"]["page"]["total_references"] = json!(self.total_steps);
            if separate_reference_delivery {
                page["_kin"]["page"]["total_answer_records"] = json!(self.reference_answer_records);
            }
            page["negative"]["subject"] = json!("reference_page");
        }
        if self.kind == PageKind::Impact {
            page.as_object_mut().unwrap().remove("chain");
            page[PageKind::Impact.primary()] = json!([]);
            page["_kin"]["runtime"] = json!("impact-snapshot");
            page["_kin"]["completeness"]["limits"] = json!(["impact_page_partial"]);
            let meta = page["_kin"]["page"].as_object_mut().unwrap();
            meta.remove("total_steps");
            meta.remove("total_candidates");
            meta.insert("kind".into(), json!("impact"));
            meta.insert(
                "total_rows".into(),
                json!(self.total_steps + self.total_candidates),
            );
            page["negative"]["subject"] = json!("impact_page");
        }
        if self.kind == PageKind::Locate {
            page.as_object_mut().unwrap().remove("chain");
            page[PageKind::Locate.primary()] = json!([]);
            page["_kin"]["runtime"] = json!("locate-snapshot");
            page["_kin"]["completeness"]["limits"] = json!(["locate_page_partial"]);
            let meta = page["_kin"]["page"].as_object_mut().unwrap();
            meta.remove("total_steps");
            meta.remove("total_candidates");
            meta.insert("kind".into(), json!("locate"));
            meta.insert(
                "total_rows".into(),
                json!(self.total_steps + self.total_candidates),
            );
            page["negative"]["subject"] = json!("locate_page");
        }
        let envelope = page["_kin"].as_object_mut().expect("page envelope");
        envelope.extend(self.identity.clone());
        // An override view states its scope beside its binding coverage on
        // every page, as one identity; only a caller answer trims.
        if matches!(self.kind, PageKind::References | PageKind::Trace)
            && !full_identity
            && !self.identity.contains_key("reference_view")
        {
            for key in Self::FIRST_PAGE_ONLY {
                envelope.remove(key);
            }
        }
        page
    }

    /// The page of the `count` whole records from `cursor`, exactly as the
    /// record-by-record loop in [`Self::page`] builds it.
    fn whole_records(
        &self,
        cursor: &Cursor,
        count: usize,
        separate_reference_delivery: bool,
        max_bytes: usize,
    ) -> Value {
        let mut next = cursor.clone();
        let mut page = self.shell(cursor, &next, separate_reference_delivery);
        for record in self.records.iter().skip(cursor.record).take(count) {
            record.append_to(&mut page);
            next.record += 1;
        }
        update_page(&mut page, &next, self.records.len(), max_bytes);
        page
    }

    /// The longest page of whole records from `cursor`, fewer than `kept`,
    /// whose plain presentation fits; one record when none does, so a walk
    /// always advances and the client presentation refuses that one page
    /// rather than overrunning its budget.
    fn presentable_prefix(
        &self,
        cursor: &Cursor,
        kept: usize,
        separate_reference_delivery: bool,
        max_bytes: usize,
    ) -> Value {
        let (mut low, mut high) = (1, kept - 1);
        let mut best = None;
        while low <= high {
            let count = low + (high - low) / 2;
            let page = self.whole_records(cursor, count, separate_reference_delivery, max_bytes);
            if presentation_fits(&page, self.kind, max_bytes) {
                best = Some(page);
                low = count + 1;
            } else {
                high = count - 1;
            }
        }
        best.unwrap_or_else(|| {
            self.whole_records(cursor, 1, separate_reference_delivery, max_bytes)
        })
    }

    fn page(&self, cursor: Cursor, max_bytes: usize) -> Result<Value, String> {
        if max_bytes < crate::budget::RESPONSE_MIN_MAX_CHARS {
            return Err("trace page budget is below the tool floor".into());
        }
        if cursor.record >= self.records.len() {
            return Err("trace cursor is outside its snapshot; restart without cursor".into());
        }
        // Additive delivery handles must not make an existing lossless cursor
        // refuse an irreducible record at the transport floor. Such a page
        // retains its legacy cursor and the same uncertainty. A larger page
        // again separates recorded rows from evidence.
        let separate_reference_delivery = self.kind != PageKind::References
            || self.fragment_page_bound_with_reference_delivery(max_bytes, true) <= max_bytes;
        let mut next = cursor.clone();
        let mut page = self.shell(&cursor, &next, separate_reference_delivery);
        let mut kept = 0;
        if cursor.field == 0 && cursor.byte == 0 {
            for record in self.records.iter().skip(cursor.record) {
                // The serialized record alone cannot fit. Do not rebuild its
                // object tree just to discard that trial and fragment it.
                if record.wire_bytes > max_bytes {
                    break;
                }
                let mut trial = page.clone();
                record.append_to(&mut trial);
                next.record += 1;
                update_page(&mut trial, &next, self.records.len(), max_bytes);
                if bytes(&trial) > max_bytes {
                    break;
                }
                page = trial;
                kept += 1;
            }
        }
        if kept > 0 {
            // A reference page whose plain presentation would outgrow the
            // budget carries fewer whole records instead; the rest start the
            // next page, which its cursors already name.
            if kept > 1 && !presentation_fits(&page, self.kind, max_bytes) {
                page =
                    self.presentable_prefix(&cursor, kept, separate_reference_delivery, max_bytes);
            }
            return Ok(page);
        }

        let record = &self.records[cursor.record];
        let Some(field) = record.fields.get(cursor.field) else {
            return Err(
                "trace cursor field is outside its semantic record; restart without cursor".into(),
            );
        };
        let encoded = field.encoded.as_ref();
        if cursor.byte > encoded.len()
            || (cursor.byte == encoded.len() && !encoded.is_empty())
            || !encoded.is_char_boundary(cursor.byte)
        {
            return Err(
                "trace fragment cursor is outside its semantic field; restart without cursor"
                    .into(),
            );
        }
        // Search code-point boundaries, measuring JSON escaping, cursor digits
        // and the complete compact envelope on every candidate. Source strings
        // are direct UTF-8 text; structured semantic fields identify their JSON
        // encoding explicitly. No whole row or reply is serialized as a stream.
        // Search byte offsets directly instead of allocating one offset for
        // every code point of a potentially large semantic field.
        let mut low = cursor.byte;
        if !encoded.is_empty() {
            low += 1;
            while low < encoded.len() && !encoded.is_char_boundary(low) {
                low += 1;
            }
        }
        let mut high = encoded.len();
        let mut best = None;
        let mut try_complete = true;
        while low <= high {
            let mut end = if try_complete {
                high
            } else {
                low + (high - low) / 2
            };
            try_complete = false;
            while !encoded.is_char_boundary(end) {
                end -= 1;
            }
            let field_complete = end == encoded.len();
            let record_complete = field_complete && cursor.field + 1 == record.fields.len();
            let mut next = cursor.clone();
            if record_complete {
                next.record += 1;
                next.field = 0;
                next.byte = 0;
            } else if field_complete {
                next.field += 1;
                next.byte = 0;
            } else {
                next.byte = end;
            }
            let mut trial = self.shell(&cursor, &next, separate_reference_delivery);
            let mut fragment = record.identity();
            fragment["field"] = json!(field.name);
            fragment["encoding"] = json!(field.encoding);
            fragment["byte_offset"] = json!(cursor.byte);
            fragment["total_bytes"] = json!(encoded.len());
            fragment["field_complete"] = json!(field_complete);
            fragment["record_complete"] = json!(record_complete);
            fragment["text"] = json!(&encoded[cursor.byte..end]);
            trial["record_fragment"] = fragment;
            update_page(&mut trial, &next, self.records.len(), max_bytes);
            if bytes(&trial) <= max_bytes {
                best = Some(trial);
                low = end + 1;
                while low < encoded.len() && !encoded.is_char_boundary(low) {
                    low += 1;
                }
            } else {
                if end == 0 {
                    break;
                }
                high = end - 1;
                while !encoded.is_char_boundary(high) {
                    high -= 1;
                }
            }
        }
        best.ok_or_else(|| "trace semantic record identity cannot fit the page budget".into())
    }
}

fn update_page(page: &mut Value, next: &Cursor, total: usize, max_bytes: usize) {
    let more = next.record < total;
    page["next_cursor"] = if more {
        json!(next.encode())
    } else {
        Value::Null
    };
    page["_kin"]["page"]["has_more"] = json!(more);
    page["_kin"]["page"]["next_record"] = json!(next.record);
    page["_kin"]["page"]["next_field"] = json!(next.field);
    page["_kin"]["page"]["next_byte"] = json!(next.byte);
    let kind = page
        .pointer("/_kin/page/kind")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if kind.as_deref() == Some("references") {
        page["_kin"]["page"]["returned_references"] =
            json!(page["references"].as_array().map_or(0, Vec::len));
        if let Some(answer_end) = page["_kin"]["page"]["total_answer_records"].as_u64() {
            let answer_end = answer_end as usize;
            let answer_more = next.record < answer_end;
            let evidence_more = next.record.max(answer_end) < total;
            page["_kin"]["page"]["answer_rows_complete"] = json!(!answer_more);
            page["_kin"]["page"]["answer_rows_has_more"] = json!(answer_more);
            page["_kin"]["page"]["evidence_has_more"] = json!(evidence_more);
            page["answer_next_cursor"] = json!(answer_more.then(|| next.encode()));
            let mut evidence = next.clone();
            if evidence.record < answer_end {
                evidence.record = answer_end;
                evidence.field = 0;
                evidence.byte = 0;
            }
            page["evidence_next_cursor"] = json!(evidence_more.then(|| evidence.encode()));
        }
    } else if kind.as_deref() == Some("impact") {
        page["_kin"]["page"]["returned_rows"] = json!(impact_rows(page));
    } else if kind.as_deref() == Some("locate") {
        page["_kin"]["page"]["returned_rows"] = json!(locate_rows(page));
    } else {
        page["total_steps"] = json!(page["chain"].as_array().map_or(0, Vec::len));
        if let Some(answer_end) = page["_kin"]["page"]["total_answer_records"].as_u64() {
            let answer_end = answer_end as usize;
            let answer_more = next.record < answer_end;
            page["_kin"]["page"]["answer_rows_complete"] = json!(!answer_more);
            page["_kin"]["page"]["answer_rows_has_more"] = json!(answer_more);
            page["_kin"]["page"]["evidence_has_more"] = json!(next.record.max(answer_end) < total);
        }
    }
    set_wire_size(page, max_bytes);
}

fn set_wire_size(page: &mut Value, max_bytes: usize) {
    page["_kin"]["response"] = json!({
        "max_chars": max_bytes, "chars_after_budget": 0,
        "bounded": page.pointer("/_kin/page/complete") != Some(&json!(true)),
        "compact": true,
    });
    for _ in 0..8 {
        let size = bytes(page);
        if page["_kin"]["response"]["chars_after_budget"] == size {
            break;
        }
        page["_kin"]["response"]["chars_after_budget"] = json!(size);
    }
}

/// Serializes the tests that hold pages in the process-wide cache against the
/// ones that fill or expire it. A test that fills the cache to its entry limit
/// evicts every other test's pages, and a test resuming one of those then fails
/// for a reason that is not its own.
#[cfg(test)]
pub(crate) fn cache_test_guard() -> std::sync::MutexGuard<'static, ()> {
    static CACHE_TESTS: Mutex<()> = Mutex::new(());
    CACHE_TESTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn ordinary_reference_pages_keep_exact_focal_identity_before_its_full_reading() {
        let _cache_test = cache_test_guard();
        let mut answer = restating_answer(PageKind::References, 0, inconclusive_verdict());
        let focal = json!({"id":"target", "name":"factory.<locals>.wrapper",
            "qualified_name":"module.factory.<locals>.wrapper", "kind":"function",
            "projection":{"path":"src/module.py"}, "signature":"def wrapper(value)",
            "body":"open('config/settings.json')\n".repeat(500)});
        answer["focal_entity"] = focal.clone();
        answer["call_site_candidates"] = json!({"named":(0..55).map(|index| json!({
            "caller":format!("candidate-{index}"), "callee":"wrapper", "state":"binding",
            "expr":format!("wrapper(value_{index})"), "line_in_entity":index + 1,
        })).collect::<Vec<_>>()});
        let first = start_kind(answer, context(7), 4000, PageKind::References).unwrap();
        let expected = json!({"entity_id":"target", "name":"factory.<locals>.wrapper",
            "qualified_name":"module.factory.<locals>.wrapper", "kind":"function"});
        assert_eq!(first["_kin"]["reference_focal"], expected);
        assert!(first.get("focal_entity").is_none());
        let (restored, pages) = drain(first, &context(7), 4000, PageKind::References);
        assert!(pages.len() > 1);
        assert!(pages
            .iter()
            .any(|page| page.get("record_fragment").is_some()));
        let mut semantic_focal = focal.clone();
        semantic_focal.as_object_mut().unwrap().remove("projection");
        assert_eq!(restored["focal_entity"], semantic_focal);
        assert_eq!(restored["focal_entity"]["body"], focal["body"]);
        assert!(!restored.to_string().contains("src/module.py"));
        for page in pages {
            assert_eq!(page["_kin"]["reference_focal"], expected);
            for key in [
                "signature",
                "body",
                "file",
                "projection",
                "line",
                "start_line",
                "end_line",
            ] {
                assert!(page["_kin"]["reference_focal"].get(key).is_none(), "{key}");
            }
        }
    }

    #[test]
    fn reference_identity_is_never_guessed_or_truncated_to_fit() {
        assert!(reference_focal_identity(&json!({"name":"wrapper", "file":"module.py"})).is_none());
        assert_eq!(
            reference_focal_identity(&json!({"entity_id":"target",
            "entity_name":"exact_name", "entity_kind":"function", "projection":{"path":null},
            "entity_file":"known.py"}))
            .unwrap(),
            json!({"entity_id":"target",
            "name":"exact_name","kind":"function","file":"known.py"})
        );
        let exact = json!({"id":"target", "name":"namespace.".repeat(500),
            "projection":{"path":"module.py"}, "kind":"function"});
        assert_eq!(
            reference_focal_identity(&exact).unwrap()["name"],
            exact["name"]
        );
        let mut answer = restating_answer(PageKind::References, 0, inconclusive_verdict());
        answer["focal_entity"] = exact;
        let error = start_kind(answer, context(7), 4000, PageKind::References).unwrap_err();
        assert!(error.contains("cannot fit the page budget"), "{error}");
    }

    #[test]
    fn override_pages_keep_view_identity_and_semantic_binding_coverage_together() {
        let _cache_test = cache_test_guard();
        let mut answer = restating_answer(PageKind::References, 30, inconclusive_verdict());
        answer["view"] = json!("overrides");
        answer["overridden_by"] = json!({
            "count": 1, "proven": 1, "coverage": "complete", "bounded": false,
            "nested_definitions": 0, "overriders": [{"id": "override-1", "name": "Child.method"}],
        });
        let snapshot = freeze(
            answer.clone(),
            context(7),
            PageKind::References,
            30,
            Vec::new(),
        )
        .unwrap();
        assert_eq!(snapshot.identity["reference_view"]["view"], "overrides");
        assert_eq!(
            snapshot.identity["reference_view"]["focal_entity"],
            answer["focal_entity"]
        );
        assert_eq!(
            snapshot.identity["reference_view"]["overridden_by"]["count"],
            1
        );
        let coverage = json!({"checked_scope":"admitted_inventory",
            "local_binding_requirement":"required", "scope":"live_head"});
        assert_eq!(snapshot.identity["binding_coverage"], coverage);
        assert!(!snapshot.identity.contains_key("source_derivation"));
        // Internal inventory counts never become public continuation identity.
        answer["_kin"]["source_derivation"] = json!({"checked_scope":"admitted_inventory",
            "local_binding_requirement":"required", "scope":"live_head",
            "report":{"full_adapter_sources":83,"excluded_artifacts":153}});
        let first = start_kind(answer, context(7), 4000, PageKind::References).unwrap();
        let (_, pages) = drain(first, &context(7), 4000, PageKind::References);
        assert!(pages.len() > 1, "the fixture must exercise continuation");
        for page in pages {
            assert_eq!(
                page["_kin"]["reference_view"]["view"], "overrides",
                "{page}"
            );
            assert_eq!(page["_kin"]["binding_coverage"], coverage, "{page}");
            assert!(page["_kin"].get("source_derivation").is_none(), "{page}");
            assert!(!page.to_string().contains("full_adapter_sources"), "{page}");
            assert!(!page.to_string().contains("excluded_artifacts"), "{page}");
        }
    }

    #[test]
    fn reference_pages_freeze_binding_obligations_before_hiding_source_inventory() {
        let _cache_test = cache_test_guard();
        let mut observation = crate::source_derivation::SourceDerivationObservation::unavailable(
            "binding inspection is incomplete",
        );
        observation.scope = crate::source_derivation::SourceObservationScope::LiveHead;
        observation.checked_scope = "admitted_inventory".into();
        let report = observation.report.as_mut().unwrap();
        report.prior_local_binding =
            kin_review::source_derivation::PriorLocalBindingStatus::Outstanding;
        report.outstanding_local_binding_obligations = Some(7);
        report.full_adapter_sources = Some(83);
        report.excluded_artifacts = Some(153);
        let verdict = json!({"state":"inconclusive", "safe_to_conclude_absent":false,
            "limiting_factor":"local_binding_outstanding"});
        let mut answer = restating_answer(PageKind::References, 30, verdict);
        answer["_kin"]["source_derivation"] = serde_json::to_value(observation).unwrap();
        let first = start_kind(answer, context(7), 4000, PageKind::References).unwrap();
        let (restored, pages) = drain(first, &context(7), 4000, PageKind::References);
        assert!(pages.len() > 1, "the fixture must continue");
        let gap = &restored["_kin"]["reference_gap"];
        assert_eq!(gap["kind"], "local_bindings");
        assert_eq!(gap["count"], 7);
        assert_eq!(
            gap["unit"],
            "outstanding binding obligations (not a caller count)"
        );
        assert_eq!(gap["count_scope"], "checked_inventory");
        // The first page states the gap and the binding scope beside the
        // verdict; later pages keep the verdict and its code and leave the
        // gap to the first page and the envelope, which carries it once.
        for (index, page) in pages.iter().enumerate() {
            if index == 0 {
                assert_eq!(page["_kin"]["reference_gap"], *gap, "{page}");
                assert_eq!(
                    page["_kin"]["binding_coverage"]["checked_scope"],
                    "admitted_inventory"
                );
            } else {
                assert!(page["_kin"].get("reference_gap").is_none(), "{page}");
                assert!(page["_kin"].get("binding_coverage").is_none(), "{page}");
            }
            assert_eq!(page["_kin"]["verdict"]["state"], "inconclusive");
            assert_eq!(
                page["_kin"]["verdict"]["limiting_factor"],
                "local_binding_outstanding"
            );
            assert_eq!(page["negative"]["safe_to_conclude_absent"], false);
            assert!(!page.to_string().contains("full_adapter_sources"));
            assert!(!page.to_string().contains("excluded_artifacts"));
        }
    }

    #[tokio::test]
    #[ignore = "requires a matching prepared repository authority"]
    async fn prepared_reference_snapshot_preserves_census() {
        let store = std::env::var_os("KIN_REFS_PREPARED_STORE").expect("prepared store");
        let repository =
            std::env::var("KIN_REFS_PREPARED_REPOSITORY").expect("repository identity");
        let workspace = kin_model::WorkspaceId(
            std::env::var("KIN_REFS_PREPARED_WORKSPACE")
                .expect("workspace identity")
                .parse()
                .unwrap(),
        );
        let repository = kin_model::RepositoryId::new(repository).unwrap();
        let backend = std::sync::Arc::new(kin_db::LocalFileBackend::new(std::path::PathBuf::from(
            store,
        )));
        let binding = kin_core::repository_authority::LocalRepositoryAuthorityBinding::from_parts(
            repository.clone(),
            workspace,
            backend,
        );
        let manager = std::sync::Arc::new(binding.open_manager().unwrap());
        let selected = manager
            .read_authority()
            .workspace_graph_snapshot(&workspace)
            .unwrap()
            .expect("selected workspace");
        let held = kin_db::InMemoryGraph::from_snapshot_without_text_index(selected).unwrap();
        let opened = crate::handlers::repository_authority::ActiveRepositoryAuthority::from_shared(
            manager, repository, workspace,
        );
        let authority =
            crate::handlers::repository_authority::RequestRepositoryAuthority::already_open(
                binding,
                std::sync::Arc::new(opened),
            );
        let arguments =
            std::collections::HashMap::from([("query".to_owned(), json!("scheduleUpdateOnFiber"))]);
        let reply =
            crate::handlers::entities::handle_find_references(&arguments, &held, Some(&authority))
                .await
                .unwrap();
        let crate::ContentBlock::Text { text } = &reply.content[0];
        let raw: Value = serde_json::from_str(text).unwrap();
        eprintln!(
            "prepared-reference raw wire={} resident={}",
            bytes(&raw),
            resident_bytes(&raw)
        );
        for (field, value) in raw.as_object().unwrap() {
            eprintln!(
                "prepared-reference field={field} wire={} resident={} rows={}",
                bytes(value),
                resident_bytes(value),
                value.as_array().map_or(0, Vec::len)
            );
        }
        if let Some(fields) = raw["call_sites"].as_object() {
            for (field, value) in fields {
                eprintln!(
                    "prepared-reference call-sites={field} wire={} resident={} rows={}",
                    bytes(value),
                    resident_bytes(value),
                    value.as_array().map_or(0, Vec::len)
                );
            }
        }
        let candidates = raw["call_sites"]["candidates"].as_array().unwrap();
        for field in ["reason", "state", "state_reason", "expr_unavailable"] {
            let mut counts = BTreeMap::new();
            for row in candidates {
                *counts.entry(row[field].to_string()).or_insert(0usize) += 1;
            }
            eprintln!("prepared-reference candidate-{field}={counts:?}");
        }
        eprintln!(
            "prepared-reference candidate-callee-known={} body-fields={}",
            candidates
                .iter()
                .filter(|row| row["callee"].is_string())
                .count(),
            candidates
                .iter()
                .filter(|row| row.get("body").is_some())
                .count()
        );
        let expected = raw.clone();
        let frozen = freeze(
            raw,
            context(1),
            PageKind::References,
            expected["references"].as_array().unwrap().len(),
            vec![],
        )
        .unwrap();
        eprintln!(
            "prepared-reference frozen records={} retained={} snapshot-limit={} cache-limit={}",
            frozen.records.len(),
            frozen.bytes,
            snapshot_limits().snapshot_bytes,
            snapshot_limits().cache_bytes
        );
        let cursor = Cursor {
            v: 1,
            snapshot: frozen.id,
            record: 0,
            field: 0,
            byte: 0,
        };
        let page = frozen.page(cursor, 12000).unwrap();
        assert!(bytes(&page) <= 12000);
        assert_eq!(page["_kin"]["verdict"]["safe_to_conclude_absent"], false);
        // The complete drain uses a larger permitted page ceiling, while the
        // same frozen snapshot's ordinary initial page above stays bounded.
        let first = frozen
            .page(
                Cursor {
                    v: 1,
                    snapshot: frozen.id,
                    record: 0,
                    field: 0,
                    byte: 0,
                },
                24576,
            )
            .unwrap();
        cache().lock().unwrap().insert(frozen).unwrap();
        let (remaining, pages) = drain(first, &context(1), 24576, PageKind::References);
        let references = remaining["references"].as_array().unwrap();
        assert_eq!(references, expected["references"].as_array().unwrap());
        assert_eq!(
            remaining["call_sites"]["candidates"],
            expected["call_sites"]["candidates"]
        );
        assert_eq!(
            remaining["call_sites"]["candidate_count"],
            expected["call_sites"]["candidate_count"]
        );
        eprintln!(
            "prepared-reference complete-pages={} references={} candidates={}",
            pages.len(),
            references.len(),
            remaining["call_sites"]["candidates"]
                .as_array()
                .unwrap()
                .len()
        );
    }

    #[test]
    fn impact_sections_count_unread_sites_and_keep_every_spelled_one() {
        let unread = |caller: &str| {
            json!({"callee": null, "caller": caller, "caller_name": "page.spec",
                "expr_unavailable": "caller_source_unavailable", "line_in_entity": null,
                "reason": "caller text unknown", "state": "unresolved", "state_reason": "no_answer"})
        };
        let spelled = json!({"callee": "resolve", "caller": "c", "caller_name": "beginWork",
            "expr": "resolve(props)", "line_in_entity": 4, "state": "unresolved"});
        let mut rows = vec![unread("a"); 400];
        rows.push(spelled.clone());
        rows.push(spelled.clone());
        rows.extend(vec![unread("b"); 250]);
        let mut collapsed_unread = unread("d");
        collapsed_unread["occurrences"] = json!(9);
        rows.push(collapsed_unread);
        let block = |rows: &[Value]| {
            json!({"entity_id": "10315333-a00e-42a6-9390-a8c27627c0c2", "candidates": rows,
                "candidate_count": rows.len(), "settled": false,
                "clauses": ["call_sites_unresolved: 660 sites could not be matched"]})
        };
        let mut payload = json!({"sections": {
            "direct": {"rows": ["affected_callers"], "call_sites": {"candidates": rows.clone(),
                "settled": false, "clauses": ["call_sites_unresolved: 660 sites"],
                "focals": [block(&rows)]}},
            "dependents": {"rows": []},
        }});
        let original = payload.clone();
        let before = payload.to_string().len();
        collapse_section_candidates(&mut payload);
        let direct = &payload["sections"]["direct"]["call_sites"];
        for block in [direct, &direct["focals"][0]] {
            // The spelled sites stay, collapsed to one row that expands back.
            assert_eq!(
                block["candidates"],
                json!([{
                "callee": "resolve", "caller": "c", "caller_name": "beginWork",
                "expr": "resolve(props)", "line_in_entity": 4, "state": "unresolved",
                "occurrences": 2}])
            );
            let unread = &block[UNREAD_SITES_KEY];
            assert_eq!(unread["count"], 659);
            assert_eq!(
                unread["by_reason"],
                json!({"caller_source_unavailable": 659})
            );
            assert!(unread["why"].as_str().unwrap().contains("details:true"));
        }
        assert!(direct["focals"][0][UNREAD_SITES_KEY]["why"]
            .as_str()
            .unwrap()
            .contains("refs {\"entity_id\":\"10315333-a00e-42a6-9390-a8c27627c0c2\"}"));
        // What qualifies the section is untouched.
        for key in ["settled", "clauses"] {
            assert_eq!(
                direct[key],
                original["sections"]["direct"]["call_sites"][key]
            );
        }
        assert_eq!(
            direct["focals"][0]["candidate_count"],
            original["sections"]["direct"]["call_sites"]["focals"][0]["candidate_count"]
        );
        assert!(payload.to_string().len() * 50 < before);
    }

    #[test]
    fn a_whole_impact_page_counts_unread_sites_instead_of_listing_them() {
        let _cache_test = cache_test_guard();
        let unread = json!({"callee": null, "caller": "spec", "caller_name": "page.spec",
            "expr_unavailable": "caller_source_unavailable", "line_in_entity": null,
            "reason": "caller text unknown", "state": "unresolved"});
        let payload = json!({
            "affected_callers": [{"id": "caller-1", "name": "beginWork", "kind": "function"}],
            "sections": {"direct": {"rows": ["affected_callers"],
                "call_sites": {"entity_id": "focal-1", "candidates": vec![unread; 300],
                    "settled": false, "clauses": ["call_sites_unresolved: 300 sites"]}},
                "dependents": {"rows": []}},
            "_kin": {"envelope_version": 2, "semantic_surface": 1,
                "verdict": {"state": "inconclusive", "safe_to_conclude_absent": false,
                    "limiting_factor": "call_sites_unresolved"}},
        });
        let page = start_kind(payload, context(1), 60_000, PageKind::Impact).unwrap();
        assert_eq!(page["_kin"]["page"]["complete"], true, "{page}");
        let block = &page["sections"]["direct"]["call_sites"];
        assert_eq!(block["candidates"], json!([]));
        assert_eq!(block[UNREAD_SITES_KEY]["count"], 300);
        assert_eq!(block["settled"], false);
        assert_eq!(page["_kin"]["verdict"]["state"], "inconclusive");
        assert!(page.to_string().len() < 2_000, "{}", page.to_string().len());
    }

    fn context(revision: u64) -> Context {
        Context::new(
            &json!({"focal":"start", "depth":3}),
            &json!({"repo":"a", "revision":revision}),
        )
    }

    fn payload() -> Value {
        json!({
            "focal_id": "00000000-0000-0000-0000-000000000000", "focal_name":"start",
            "chain": (1..=8).map(|step| json!({
                "step":step, "parent_step":step-1,
                "entity_id":format!("00000000-0000-0000-0000-{step:012}"),
                "entity_name":format!("hop_{step}"), "body":"λ\"\\\n".repeat(2000),
            })).collect::<Vec<_>>(),
            "total_steps":8,
            "_kin":{"envelope_version":2,
                "repository":{"authority_context":"90b5939b2c0fa106b07d37b21b3cd84e29b9aee26c93ec32e11f2b2587906163"},
                "semantic_surface":1,
                "graph_as_of":{"generation":7,"graph_root":"captured-root"},
                "runtime":"repo-daemon",
                "verdict":{"state":"inconclusive", "limiting_factor":"source_binding_unknown"}},
            "negative":{"safe_to_conclude_absent":false},
        })
    }

    #[test]
    fn snapshot_allowance_scales_with_the_actual_host_ceiling() {
        const GIB: u64 = 1024 * 1024 * 1024;
        for ceiling in [128 * 1024 * 1024, 2 * GIB, 8 * GIB, 128 * GIB] {
            let limits = SnapshotLimits::for_ceiling(Some(ceiling));
            assert!(limits.cache_bytes as u64 <= ceiling / 32);
            assert_eq!(limits.snapshot_bytes, limits.cache_bytes);
        }
        assert_eq!(
            SnapshotLimits::for_ceiling(Some(128 * GIB)).snapshot_bytes,
            512 * 1024 * 1024
        );
        assert_eq!(
            SnapshotLimits::for_ceiling(None).cache_bytes,
            64 * 1024 * 1024
        );
        assert_eq!(
            SnapshotLimits::for_ceiling(Some(0)).cache_bytes,
            SnapshotLimits::for_ceiling(None).cache_bytes
        );
    }

    #[test]
    fn an_ordinary_host_admits_react_sized_evidence_and_bounds_competing_answers() {
        let limits = SnapshotLimits::for_ceiling(Some(8 * 1024 * 1024 * 1024));
        assert_eq!(limits.cache_bytes, 64 * 1024 * 1024);
        assert_eq!(limits.snapshot_bytes, limits.cache_bytes);
        let sized = |body_bytes, limit| {
            freeze_with_limit(
                json!({"chain":[{"entity_id":"a", "body":"x".repeat(body_bytes)}]}),
                context(1),
                PageKind::Trace,
                1,
                vec![],
                limit,
            )
        };
        let mut local = Cache::default();
        let prior = freeze_with_limit(
            payload(),
            context(1),
            PageKind::Trace,
            8,
            vec![],
            limits.snapshot_bytes,
        )
        .unwrap();
        let prior_id = prior.id;
        local.insert_with_budget(prior, limits.cache_bytes).unwrap();
        // The prepared React fixture charged 39,369,319 bytes. A real encoded
        // semantic field of that size used to fail the artificial half budget.
        let accepted = sized(39_369_319, limits.snapshot_bytes).unwrap();
        let accepted_id = accepted.id;
        assert!(accepted.bytes > limits.cache_bytes / 2);
        assert!(accepted.bytes <= limits.cache_bytes);
        local
            .insert_with_budget(accepted, limits.cache_bytes)
            .unwrap();
        let retained = local.bytes;
        assert!(local
            .snapshots
            .iter()
            .any(|snapshot| snapshot.id == prior_id));
        assert!(sized(limits.snapshot_bytes + 1, limits.snapshot_bytes).is_err());
        // Defense in depth: even a valid frozen record made under a larger
        // host allowance cannot enter a smaller cache by evicting its evidence.
        let too_large = sized(limits.cache_bytes + 1, limits.cache_bytes * 2).unwrap();
        assert!(too_large.bytes > limits.cache_bytes);
        assert!(local
            .insert_with_budget(too_large, limits.cache_bytes)
            .is_err());
        assert_eq!(local.bytes, retained);
        assert!(local
            .snapshots
            .iter()
            .any(|snapshot| snapshot.id == accepted_id));
        assert!(local
            .snapshots
            .iter()
            .any(|snapshot| snapshot.id == prior_id));
        // Ordinary pressure still evicts FIFO until the unchanged aggregate
        // fits; a second large answer cannot make storage grow past the bound.
        let next = sized(39_369_319, limits.snapshot_bytes).unwrap();
        let next_id = next.id;
        local.insert_with_budget(next, limits.cache_bytes).unwrap();
        assert_eq!(local.snapshots.len(), 1);
        assert_eq!(local.snapshots[0].id, next_id);
        assert!(local.bytes <= limits.cache_bytes);
    }

    #[test]
    fn frozen_fields_keep_one_encoded_copy_and_cache_rejects_oversized_admission() {
        let body = "λ\"\\\n".repeat(100_000);
        let record = Record::new(
            "chain",
            0,
            None,
            json!({"entity_id":"a", "body":body, "proof":{"targets":[1,2,3]}}),
        );
        assert_eq!(
            record
                .fields
                .iter()
                .find(|field| field.name.as_deref() == Some("body"))
                .unwrap()
                .encoded
                .as_ref(),
            body
        );
        assert_eq!(record.value()["body"], body);
        assert!(record.retained_bytes() < record.wire_bytes);
        let mut local = Cache::default();
        let preserved = freeze(payload(), context(1), PageKind::Trace, 8, vec![]).unwrap();
        let identity = preserved.id;
        local.insert(preserved).unwrap();
        let mut too_large = freeze(payload(), context(1), PageKind::Trace, 8, vec![]).unwrap();
        too_large.bytes = snapshot_limits().cache_bytes + 1;
        assert!(local
            .insert(too_large)
            .unwrap_err()
            .contains("cache allowance"));
        assert_eq!(local.snapshots.len(), 1);
        assert_eq!(local.snapshots[0].id, identity);
        assert_eq!(local.bytes, local.snapshots[0].bytes);
    }

    #[test]
    fn fragments_preserve_every_byte_and_pages_fit_the_floor() {
        let _cache_test = cache_test_guard();
        let original = payload();
        let mut page = start(original.clone(), context(1), 2000).unwrap();
        let mut reconstructed = serde_json::Map::new();
        let mut chains = Vec::new();
        let mut pending: BTreeMap<(String, usize, Option<String>), String> = BTreeMap::new();
        let mut partial_records: BTreeMap<(String, usize), serde_json::Map<String, Value>> =
            BTreeMap::new();
        let mut pages = 0;
        loop {
            pages += 1;
            assert!(pages < 1000);
            assert!(bytes(&page) <= 2000, "{}", bytes(&page));
            assert_eq!(page["_kin"]["response"]["chars_after_budget"], bytes(&page));
            assert_eq!(page["negative"]["safe_to_conclude_absent"], false);
            for key in ["repository", "graph_as_of", "runtime"] {
                assert_eq!(page["_kin"][key], original["_kin"][key]);
            }
            chains.extend(page["chain"].as_array().unwrap().iter().cloned());
            for reading in page["readings"].as_array().into_iter().flatten() {
                reconstructed.insert(
                    reading["key"].as_str().unwrap().to_string(),
                    reading["value"].clone(),
                );
            }
            if let Some(fragment) = page.get("record_fragment") {
                let collection = fragment["collection"].as_str().unwrap();
                let record_key = (
                    collection.to_string(),
                    fragment["index"].as_u64().unwrap() as usize,
                );
                let field = fragment["field"].as_str().map(str::to_string);
                let key = (record_key.0.clone(), record_key.1, field.clone());
                let accumulated = pending.entry(key.clone()).or_default();
                assert_eq!(
                    accumulated.len(),
                    fragment["byte_offset"].as_u64().unwrap() as usize
                );
                accumulated.push_str(fragment["text"].as_str().unwrap());
                if fragment["field_complete"] == true {
                    let value = if fragment["encoding"] == "utf8" {
                        json!(accumulated)
                    } else {
                        serde_json::from_str::<Value>(accumulated).unwrap()
                    };
                    pending.remove(&key);
                    let complete_value = if let Some(field) = field {
                        let record = partial_records.entry(record_key.clone()).or_default();
                        record.insert(field, value);
                        (fragment["record_complete"] == true)
                            .then(|| Value::Object(partial_records.remove(&record_key).unwrap()))
                    } else {
                        Some(value)
                    };
                    if let Some(value) = complete_value {
                        if collection == "chain" {
                            chains.push(value);
                        } else {
                            reconstructed
                                .insert(fragment["key"].as_str().unwrap().to_string(), value);
                        }
                    }
                }
            }
            let Some(token) = page["next_cursor"].as_str() else {
                break;
            };
            page = resume(token, &context(1), 2000).unwrap();
        }
        assert!(pending.is_empty());
        reconstructed.insert("chain".into(), json!(chains));
        assert_eq!(Value::Object(reconstructed), original);
    }

    #[test]
    fn complete_and_continued_tool_pages_keep_the_captured_repository_envelope() {
        let _cache_test = cache_test_guard();
        for kind in [PageKind::Trace, PageKind::References, PageKind::Impact] {
            for count in [0, 1, 50] {
                // Exercise the complete-answer path deliberately; the larger
                // fixture must continue under the narrower page budget.
                let ceiling = if count == 50 { 4000 } else { 60_000 };
                let mut envelope = crate::Envelope::daemon().with_repository(
                    &json!({"repo_root":"/example/selected"}),
                    Some(std::path::Path::new("/example/selected/nested")),
                    std::path::Path::to_path_buf,
                );
                let repository =
                    serde_json::to_value(crate::first_contact::semantic_repository_identity(
                        envelope.repository.as_ref().unwrap(),
                    ))
                    .unwrap();
                assert!(
                    repository.get("root").is_none() && repository.get("client_root").is_none()
                );
                assert_ne!(
                    repository["authority_context"],
                    repository["client_context"]
                );
                let graph = json!({"generation":7,"graph_root":"selected-root"});
                envelope.graph_as_of = Some(graph.clone());
                let rows: Vec<_> = (0..count)
                    .map(|index| {
                        json!({
                            "entity_id": format!("entity-{index}"), "step": index,
                            "name": format!("caller_{index}"), "body":"x".repeat(350),
                        })
                    })
                    .collect();
                let mut raw = json!({"total_upstream":count,"total_steps":count});
                raw[kind.primary()] = json!(rows);
                let frozen = context(7);
                // The same answer at a ceiling it fits, for the verdict every
                // partial page of it must carry.
                let whole = finalize_kind(
                    crate::ToolCallResult::text(raw.to_string()),
                    envelope.clone(),
                    context(70),
                    60_000,
                    kind,
                    false,
                );
                let crate::ContentBlock::Text { text } = &whole.content[0];
                let whole: Value = serde_json::from_str(text).unwrap();
                assert_eq!(whole["_kin"]["page"]["complete"], true, "{whole}");
                let answer_verdict = whole["_kin"]["verdict"].clone();
                let result = finalize_kind(
                    crate::ToolCallResult::text(raw.to_string()),
                    envelope,
                    frozen.clone(),
                    ceiling,
                    kind,
                    false,
                );
                let crate::ContentBlock::Text { text } = &result.content[0];
                let mut page: Value = serde_json::from_str(text).unwrap();
                let mut pages = 0;
                loop {
                    pages += 1;
                    assert!(pages < 200);
                    assert_eq!(page["_kin"]["repository"], repository);
                    assert!(page["_kin"]["repository"]["warning"].is_string());
                    assert_eq!(page["_kin"]["graph_as_of"], graph);
                    assert_eq!(page["_kin"]["runtime"], "repo-daemon");
                    assert!(bytes(&page) <= ceiling);
                    if page["_kin"]["page"]["complete"] == false {
                        // The answer's own verdict, not a transport stamp.
                        let verdict = &page["_kin"]["verdict"];
                        assert_eq!(verdict["state"], answer_verdict["state"], "{verdict}");
                        assert_eq!(
                            verdict["limiting_factor"], answer_verdict["limiting_factor"],
                            "{verdict}"
                        );
                        assert_eq!(verdict["safe_to_conclude_absent"], false);
                        // The page says it is partial in its own fields.
                        let code = match kind {
                            PageKind::Trace => "trace_page_partial",
                            PageKind::References => "reference_page_partial",
                            PageKind::Impact => "impact_page_partial",
                            PageKind::Locate => "locate_page_partial",
                        };
                        assert_eq!(page["_kin"]["completeness"]["status"], "partial");
                        assert_eq!(page["_kin"]["completeness"]["limits"], json!([code]));
                        assert!(
                            crate::verdict::disagreements(&page).is_empty(),
                            "{:?}",
                            crate::verdict::disagreements(&page)
                        );
                    }

                    // The stdio finalizer receives another health reading.
                    // It must preserve the already-budgeted frozen answer.
                    let later = crate::Envelope::daemon().with_repository(
                        &json!({"repo_root":"/example/later"}),
                        None,
                        std::path::Path::to_path_buf,
                    );
                    let result = crate::envelope::finalize_bounded(
                        crate::ToolCallResult::text(page.to_string()),
                        later,
                        kind.tool(),
                        &crate::budget::ResponseBudget {
                            max_chars: ceiling,
                            ..Default::default()
                        },
                    );
                    let crate::ContentBlock::Text { text } = &result.content[0];
                    assert_eq!(serde_json::from_str::<Value>(text).unwrap(), page);
                    let Some(cursor) = page["next_cursor"].as_str() else {
                        break;
                    };
                    page = resume(cursor, &frozen, ceiling).unwrap();
                }
                if count == 50 {
                    assert!(pages > 1);
                } else {
                    assert_eq!(pages, 1);
                }
            }
        }
    }

    #[test]
    fn cursor_syntax_is_bounded_and_independent_of_snapshot_authority() {
        let cursor = Cursor {
            v: 1,
            snapshot: uuid::Uuid::nil(),
            record: usize::MAX,
            field: usize::MAX,
            byte: usize::MAX,
        };
        let token = cursor.encode();
        assert!(validate_cursor_syntax(&token).is_ok());
        assert!(resume(&token, &context(1), 2000)
            .unwrap_err()
            .contains("expired or was evicted"));
        for malformed in [
            "not-valid-here".to_string(),
            "x".repeat(MAX_CURSOR_BYTES + 1),
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"{}"),
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
                serde_json::to_vec(&json!({
                    "v": 1, "snapshot": cursor.snapshot, "record": 0,
                    "field": 0, "byte": 0, "extra": true,
                }))
                .unwrap(),
            ),
            Cursor { v: 2, ..cursor }.encode(),
        ] {
            assert!(validate_cursor_syntax(&malformed).is_err(), "{malformed}");
        }
    }

    #[test]
    fn continuation_rejects_changed_authority_query_and_untrusted_offsets() {
        let _cache_test = cache_test_guard();
        let page = start(payload(), context(7), 2000).unwrap();
        let token = page["next_cursor"].as_str().unwrap();
        assert!(resume(token, &context(8), 2000)
            .unwrap_err()
            .contains("authority"));
        let different_query = Context::new(
            &json!({"focal":"elsewhere"}),
            &json!({"repo":"a", "revision":7}),
        );
        assert!(resume(token, &different_query, 2000)
            .unwrap_err()
            .contains("query"));
        assert!(Cursor::decode(&"x".repeat(MAX_CURSOR_BYTES + 1)).is_err());
        let mut bad = Cursor::decode(token).unwrap();
        bad.record = usize::MAX;
        assert!(resume(&bad.encode(), &context(7), 2000).is_err());
    }

    #[test]
    fn continuation_binds_the_effective_outside_graph_question() {
        let _cache_test = cache_test_guard();
        let mut arguments = std::collections::HashMap::from([
            ("focal".into(), json!("entry")),
            ("question".into(), json!("where is the external client")),
        ]);
        let authority = json!({"repo":"a", "revision":1});
        let context = Context::from_arguments(&arguments, &authority);
        let page = start(payload(), context.clone(), 2000).unwrap();
        let token = page["next_cursor"].as_str().unwrap();
        arguments.insert("question".into(), json!("where is another client"));
        assert!(resume(
            token,
            &Context::from_arguments(&arguments, &authority),
            2000
        )
        .unwrap_err()
        .contains("query"));
        arguments.insert("query".into(), json!(" where is the external client "));
        assert!(resume(
            token,
            &Context::from_arguments(&arguments, &authority),
            2000
        )
        .is_ok());
        arguments.insert(
            CLIENT_ROOT_ARGUMENT.into(),
            json!("/example/different-folder"),
        );
        assert!(resume(
            token,
            &Context::from_arguments(&arguments, &authority),
            2000
        )
        .unwrap_err()
        .contains("query"));
    }

    fn arguments(value: Value) -> std::collections::HashMap<String, Value> {
        serde_json::from_value(value).unwrap()
    }

    /// A continuation is a handle: the cursor alone, with any transport budget
    /// and whatever shaping defaults a client layer adds, pages the whole
    /// frozen answer under the query it was frozen with.
    #[test]
    fn a_bare_cursor_pages_the_whole_trace_under_its_frozen_query() {
        let _cache_test = cache_test_guard();
        let authority = json!({"repo": "a", "revision": 11});
        let asked = arguments(json!({
            "focal": "_make_timedelta", "direction": "callers", "depth": 4,
            "include_body": true,
        }));
        let original = restating_answer(PageKind::Trace, 60, inconclusive_verdict());
        let first = start_kind(
            original.clone(),
            Context::from_arguments(&asked, &authority),
            4000,
            PageKind::Trace,
        )
        .unwrap();
        let token = first["next_cursor"].as_str().unwrap().to_string();
        for bare in [
            json!({"cursor": token}),
            // What an agent profile adds after the call is parsed, a budget in
            // each spelling, and empty selectors: none of it names a target.
            json!({"cursor": token, "include_body": false, "max_chars": 4000,
                "max_response_chars": 4000, "max_output_bytes": 4000, "depth": 1,
                "direction": "calls", "focal": "", "query": "  ", "target": null}),
        ] {
            let context = Context::from_arguments(&arguments(bare.clone()), &authority);
            let (answer, pages) = drain(first.clone(), &context, 4000, PageKind::Trace);
            assert!(pages.len() > 1, "the fixture must page: {bare}");
            let compacted = pages[0]["_kin"]["page"]["compacted"].clone();
            assert_eq!(restore(answer, &compacted), original, "{bare}");
        }
    }

    #[test]
    fn a_trace_cursor_that_names_another_target_is_told_to_send_it_alone() {
        let _cache_test = cache_test_guard();
        let authority = json!({"repo": "a", "revision": 12});
        let asked = arguments(json!({"focal": "start", "depth": 3}));
        let page = start(payload(), Context::from_arguments(&asked, &authority), 2000).unwrap();
        let token = page["next_cursor"].as_str().unwrap();
        for named in [
            json!({"cursor": token, "focal": "elsewhere", "depth": 3}),
            json!({"cursor": token, "focal": "start", "depth": 2}),
            json!({"cursor": token, "target": "sink"}),
            json!({"cursor": token, "question": "where is the external client"}),
        ] {
            let error = resume(
                token,
                &Context::from_arguments(&arguments(named.clone()), &authority),
                2000,
            )
            .unwrap_err();
            assert!(error.contains("query changed"), "{named}: {error}");
            assert!(error.contains("send the cursor alone"), "{named}: {error}");
            assert!(error.contains("restart without cursor"), "{named}: {error}");
        }
        // The same target, repeated, still continues as it always did.
        let mut repeated = asked.clone();
        repeated.insert("cursor".into(), json!(token));
        assert!(resume(token, &Context::from_arguments(&repeated, &authority), 2000).is_ok());
    }

    /// Bare is about the query, never the authority: a bare cursor over a
    /// graph or scope that moved, for another client folder, or on another
    /// tool is refused.
    #[test]
    fn a_bare_trace_cursor_keeps_every_authority_and_tool_check() {
        let _cache_test = cache_test_guard();
        let asked = arguments(json!({"focal": "start"}));
        let frozen = json!({"repo": "a", "revision": 13});
        let page = start(payload(), Context::from_arguments(&asked, &frozen), 2000).unwrap();
        let token = page["next_cursor"].as_str().unwrap();
        let bare = arguments(json!({"cursor": token}));
        let moved = resume(
            token,
            &Context::from_arguments(&bare, &json!({"repo": "a", "revision": 14})),
            2000,
        )
        .unwrap_err();
        assert!(moved.contains("authority scope changed"), "{moved}");
        let mut elsewhere = bare.clone();
        elsewhere.insert(CLIENT_ROOT_ARGUMENT.into(), json!("/another/folder"));
        let folder =
            resume(token, &Context::from_arguments(&elsewhere, &frozen), 2000).unwrap_err();
        assert!(folder.contains("another client folder"), "{folder}");
        for (tool, context) in [
            (
                "find_references",
                crate::reference_pages::context(&bare, &frozen),
            ),
            (
                "impact_analysis",
                crate::impact_pages::context(&bare, &frozen),
            ),
        ] {
            let error = resume(token, &context, 2000).unwrap_err();
            assert!(
                error.contains("continues an answer from trace_data_flow"),
                "{tool}: {error}"
            );
        }
        assert!(resume(token, &Context::from_arguments(&bare, &frozen), 2000).is_ok());
    }

    #[test]
    fn hosted_focal_fragments_keep_canonical_entity_and_absolute_step() {
        let record = Record::new(
            "readings",
            0,
            Some("focal_entity".into()),
            json!({"id":"entity-1", "body":"source"}),
        );
        assert_eq!(
            record.identity(),
            json!({"collection":"readings", "index":0,
            "key":"focal_entity", "entity_id":"entity-1", "step":0, "parent_step":0})
        );
    }

    #[test]
    fn normal_step_prefix_is_monotonic_in_the_budget() {
        let _cache_test = cache_test_guard();
        let mut original = payload();
        for row in original["chain"].as_array_mut().unwrap() {
            row["body"] = json!("small".repeat(30));
        }
        let mut before = 0;
        for ceiling in (2000..=16000).step_by(250) {
            let page = start(original.clone(), context(1), ceiling).unwrap();
            let count = page["chain"].as_array().unwrap().len();
            assert!(count >= before, "{ceiling}: {count} after {before}");
            assert!(bytes(&page) <= ceiling);
            before = count;
        }
        assert_eq!(before, 8);
    }
    #[test]
    fn cache_expiry_and_entry_pressure_require_an_explicit_restart() {
        let _cache_test = cache_test_guard();
        let page = start(payload(), context(1), 2000).unwrap();
        let token = page["next_cursor"].as_str().unwrap().to_string();
        for _ in 0..MAX_CACHE_ENTRIES {
            start(payload(), context(1), 2000).unwrap();
        }
        assert!(resume(&token, &context(1), 2000)
            .unwrap_err()
            .contains("evicted"));
        let page = start(payload(), context(1), 2000).unwrap();
        let token = page["next_cursor"].as_str().unwrap().to_string();
        cache()
            .lock()
            .unwrap()
            .expire(Instant::now() + SNAPSHOT_TTL);
        assert!(resume(&token, &context(1), 2000)
            .unwrap_err()
            .contains("expired"));
    }

    #[test]
    fn ambiguity_pages_preserve_every_candidate_and_final_page_remains_qualified() {
        let _cache_test = cache_test_guard();
        let candidates: Vec<_> = (0..150)
            .map(|index| {
                json!({
                    "entity_id":format!("00000000-0000-0000-0000-{index:012}"),
                    "name":format!("Owner{index}.get"), "file":format!("module_{index}.py"),
                })
            })
            .collect();
        let value = json!({"ambiguous_focal":true, "candidate_count":candidates.len(), "candidates":candidates});
        let mut page = start(value, context(1), 2000).unwrap();
        let mut served = Vec::new();
        loop {
            assert!(bytes(&page) <= 2000);
            served.extend(page["candidates"].as_array().into_iter().flatten().cloned());
            assert_eq!(page["negative"]["safe_to_conclude_absent"], false);
            let Some(cursor) = page["next_cursor"].as_str() else {
                break;
            };
            page = resume(cursor, &context(1), 2000).unwrap();
        }
        let expected: Vec<_> = candidates
            .iter()
            .map(|candidate| {
                json!({
                    "entity_id":candidate["entity_id"], "name":candidate["name"],
                })
            })
            .collect();
        assert_eq!(served, expected);
        assert!(served
            .iter()
            .all(|candidate| candidate.get("file").is_none()));
    }

    #[test]
    fn full_envelope_preparation_does_not_drop_requested_bodies() {
        let _cache_test = cache_test_guard();
        let source = payload();
        let result = finalize(
            crate::ToolCallResult::text(source.to_string()),
            crate::Envelope::offline(),
            context(1),
            2000,
        );
        let crate::ContentBlock::Text { text } = &result.content[0];
        let mut page: Value = serde_json::from_str(text).unwrap();
        let mut bodies: BTreeMap<usize, String> = BTreeMap::new();
        loop {
            assert!(bytes(&page) <= 2000);
            if page["record_fragment"]["collection"] == "chain"
                && page["record_fragment"]["field"] == "body"
            {
                let fragment = &page["record_fragment"];
                assert_eq!(fragment["encoding"], "utf8");
                bodies
                    .entry(fragment["index"].as_u64().unwrap() as usize)
                    .or_default()
                    .push_str(fragment["text"].as_str().unwrap());
            }
            let Some(cursor) = page["next_cursor"].as_str() else {
                break;
            };
            page = resume(cursor, &context(1), 2000).unwrap();
        }
        assert_eq!(bodies.len(), 8);
        for (index, body) in bodies {
            assert_eq!(source["chain"][index]["body"], body);
        }
    }

    const SUBJECT: &str = "this answer returned rows, so it asserts no absence; the verdict \
        below says how far those rows can be trusted as the whole set";
    const TRUST_REASON: &str = "binding_unproven: 47 of the 4179 call sites in the store's \
        callers that could call the focal call through a value binding, which proves no \
        target; call_sites_unresolved: 1222 of the 4179 call sites in the store's callers \
        that could call the focal got an answer that proves no target";

    /// The shape measured on a Python web framework, scaled to a fixture: an
    /// answer of `rows` references whose safety readings restate each other.
    /// The semantic binding observation appears twice and the advice repeats
    /// its subject, the graph identity and every clause of its trust reason.
    fn restating_answer(kind: PageKind, rows: usize, verdict: Value) -> Value {
        let graph = json!({"generation": 4, "graph_root": "716c83c0e66a5507504d6f6ec0a7ba0d6601390a5d6a189d150bdb5e88e55aba"});
        let derivation = json!({
            "checked_scope": "admitted_inventory", "local_binding_requirement": "required",
            "scope": "live_head",
        });
        let advice = format!(
            "{SUBJECT}, against graph as-of {graph} with graph coverage for the answer's \
             language (calls present, imports present, references present) and degraded \
             signals [focal_resolution:ambiguous_name]. Treat these rows as a lower bound \
             rather than the whole set, and do not conclude anything from what is missing \
             from them. Limiting factor: {TRUST_REASON}"
        );
        let mut answer = json!({
            "focal_entity": {"id": "focal-1", "name": "_make_timedelta", "kind": "function"},
            "counts": {"referencing_entities": rows},
            "call_sites": {
                "clauses": TRUST_REASON.split(crate::verdict::CLAUSE_SEPARATOR).collect::<Vec<_>>(),
                "candidates": [{"caller": "a", "callee": "endswith"}, {"caller": "b", "callee": "strip"}],
                "scope": "the store's callers that could call the focal",
            },
            "caller_arrival": {"state": "unaccounted", "scope": "this reading counts a call site \
                as arrived when the graph holds any call edge from it; unaccounted call sites \
                can still add callers"},
            "binding_coverage": derivation.clone(),
            "negative": {
                "subject": SUBJECT, "advice": advice, "trust_reason": TRUST_REASON,
                "trust": "inconclusive", "graph_as_of": graph.clone(),
                "safe_to_conclude_absent": false, "interpretation": "qualified_answer",
            },
            "_kin": {
                "envelope_version": 2, "runtime": "repo-daemon",
                "repository": {"authority_context":"90b5939b2c0fa106b07d37b21b3cd84e29b9aee26c93ec32e11f2b2587906163"},
                "semantic_surface":1,
                "graph_as_of": graph, "binding_coverage": derivation, "verdict": verdict,
            },
        });
        answer[kind.primary()] = json!((0..rows)
            .map(|index| json!({
                "entity_id": format!("00000000-0000-4000-8000-{index:012}"),
                "name": format!("Caller{index}"), "step": index + 1, "parent_step": 0,
                "sites": [{"callee": "_make_timedelta", "line_in_entity": 165}],
                "signature": "def caller(self, value: timedelta | int | None) -> None",
            }))
            .collect::<Vec<_>>());
        answer
    }

    fn inconclusive_verdict() -> Value {
        json!({"state": "inconclusive", "absence_claim": "not_applicable",
            "safe_to_conclude_absent": false,
            "limiting_factor": "binding_unproven; call_sites_unresolved",
            "inputs": {"call_sites": "inconclusive", "degradations": "certified"}})
    }

    /// What a reader does with `_kin.page.compacted`: follow each entry to the
    /// one copy the reply kept. Test-only, so the tests can prove the collapse
    /// lost nothing.
    fn restore(mut value: Value, compacted: &Value) -> Value {
        let pointer = |path: &str| format!("/{}", path.replace('.', "/"));
        for entry in compacted.as_array().expect("a compacted list") {
            let path = entry["path"].as_str().unwrap();
            if let Some(same_as) = entry["same_as"].as_str() {
                let copy = value.pointer(&pointer(same_as)).cloned().unwrap();
                let (parent, key) = path.rsplit_once('.').map_or(("", path), |split| split);
                let slot = if parent.is_empty() {
                    &mut value
                } else {
                    value.pointer_mut(&pointer(parent)).unwrap()
                };
                slot[key] = copy;
            }
            for field in entry["restates"].as_array().into_iter().flatten() {
                let field = field.as_str().unwrap();
                let full = match value.pointer(&pointer(field)).unwrap() {
                    Value::String(text) => text.clone(),
                    other => other.to_string(),
                };
                let reference = field_reference(field);
                let mut text = value
                    .pointer(&pointer(path))
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_string();
                text = match clause_codes(&full) {
                    Some(codes) if field == "negative.trust_reason" => {
                        text.replace(&format!("{codes} ({reference})"), &full)
                    }
                    _ => text.replace(&reference, &full),
                };
                *value.pointer_mut(&pointer(path)).unwrap() = json!(text);
            }
        }
        value
    }

    /// Drain every page of a frozen answer the way the public transport
    /// documents: rows by collection, readings by key, fragments concatenated.
    fn drain(
        mut page: Value,
        context: &Context,
        ceiling: usize,
        kind: PageKind,
    ) -> (Value, Vec<Value>) {
        let mut pages = Vec::new();
        let mut readings = serde_json::Map::new();
        let mut rows: BTreeMap<&str, Vec<Value>> = BTreeMap::new();
        let mut pending: BTreeMap<(String, usize, Option<String>), String> = BTreeMap::new();
        let mut partial: BTreeMap<(String, usize), serde_json::Map<String, Value>> =
            BTreeMap::new();
        loop {
            assert!(pages.len() < 1000);
            assert!(bytes(&page) <= ceiling, "{}", bytes(&page));
            for &collection in kind.collections() {
                let found = page.pointer(&format!("/{}", collection.replace('.', "/")));
                for row in found.and_then(Value::as_array).into_iter().flatten() {
                    rows.entry(collection).or_default().push(row.clone());
                }
            }
            for reading in page["readings"].as_array().into_iter().flatten() {
                let key = reading["key"].as_str().unwrap().to_string();
                assert!(readings.insert(key, reading["value"].clone()).is_none());
            }
            if let Some(fragment) = page.get("record_fragment") {
                let collection = fragment["collection"].as_str().unwrap().to_string();
                let index = fragment["index"].as_u64().unwrap() as usize;
                let field = fragment["field"].as_str().map(str::to_string);
                let key = (collection.clone(), index, field.clone());
                let text = pending.entry(key.clone()).or_default();
                assert_eq!(
                    text.len(),
                    fragment["byte_offset"].as_u64().unwrap() as usize
                );
                text.push_str(fragment["text"].as_str().unwrap());
                if fragment["field_complete"] == true {
                    let text = pending.remove(&key).unwrap();
                    let value = if fragment["encoding"] == "utf8" {
                        json!(text)
                    } else {
                        serde_json::from_str(&text).unwrap()
                    };
                    let value = match field {
                        Some(field) => {
                            let record = partial.entry((collection.clone(), index)).or_default();
                            record.insert(field, value);
                            (fragment["record_complete"] == true).then(|| {
                                Value::Object(partial.remove(&(collection.clone(), index)).unwrap())
                            })
                        }
                        None => Some(value),
                    };
                    if let Some(value) = value {
                        assert_eq!(collection, "readings", "fixture rows fit a page whole");
                        readings.insert(fragment["key"].as_str().unwrap().to_string(), value);
                    }
                }
            }
            let cursor = page["next_cursor"].as_str().map(str::to_string);
            pages.push(page);
            let Some(cursor) = cursor else {
                break;
            };
            page = resume(&cursor, context, ceiling).unwrap();
        }
        assert!(pending.is_empty() && partial.is_empty());
        let mut answer = Value::Object(readings);
        for (collection, values) in rows {
            let (parent, key) = collection
                .split_once('.')
                .map_or(("", collection), |split| split);
            if parent.is_empty() {
                answer[key] = json!(values);
            } else {
                answer[parent][key] = json!(values);
            }
        }
        (answer, pages)
    }

    /// A small answer that pages only because its safety readings restate
    /// each other ships as ONE complete page, compacted, with its own verdict.
    ///
    /// The measured reply was 12,239 bytes against the 12,000-byte default,
    /// with an answer of about 850. It paged, and every page read inconclusive
    /// whatever the answer's verdict said.
    #[test]
    fn a_small_answer_whose_readings_restate_each_other_fits_one_compacted_page() {
        let _cache_test = cache_test_guard();
        for kind in [PageKind::References, PageKind::Trace] {
            let original = restating_answer(kind, 1, inconclusive_verdict());
            // The largest ceiling the whole annotated reply does not fit.
            let fits = |ceiling| complete_page(&original, kind, 1, ceiling, &[]).is_some();
            let ceiling = (crate::budget::RESPONSE_MIN_MAX_CHARS..100_000)
                .find(|&ceiling| fits(ceiling))
                .unwrap()
                - 1;
            assert!(
                ceiling >= crate::budget::RESPONSE_MIN_MAX_CHARS,
                "{ceiling}"
            );

            let page = start_kind(original.clone(), context(3), ceiling, kind).unwrap();
            assert!(bytes(&page) <= ceiling);
            assert_eq!(page["_kin"]["page"]["complete"], true, "{page}");
            assert_eq!(page["_kin"]["page"]["has_more"], false);
            assert_eq!(page["next_cursor"], Value::Null);
            assert_eq!(
                page["_kin"]["page"]["compacted"],
                json!([
                    {"path": "binding_coverage", "same_as": "_kin.binding_coverage"},
                    {"path": "negative.graph_as_of", "same_as": "_kin.graph_as_of"},
                    {"path": "negative.advice",
                     "restates": ["negative.subject", "_kin.graph_as_of", "negative.trust_reason"]},
                ])
            );
            // The answer's own verdict, untouched, and no partial-page stamp.
            assert_eq!(page["_kin"]["verdict"], original["_kin"]["verdict"]);
            assert!(crate::verdict::disagreements(&page).is_empty());
            // The answer and every reading that exists once are byte-identical.
            for key in [
                kind.primary(),
                "focal_entity",
                "counts",
                "call_sites",
                "caller_arrival",
            ] {
                assert_eq!(page[key], original[key], "{key}");
            }
            for key in [
                "subject",
                "trust_reason",
                "trust",
                "safe_to_conclude_absent",
            ] {
                assert_eq!(page["negative"][key], original["negative"][key], "{key}");
            }
            assert!(page.get("binding_coverage").is_none());
            assert_eq!(
                page["_kin"]["binding_coverage"],
                original["binding_coverage"]
            );
            assert!(page.get("source_derivation").is_none());
            // The advice keeps its own sentence and still names its cause.
            let advice = page["negative"]["advice"].as_str().unwrap();
            assert!(
                advice.starts_with("{negative.subject}, against graph as-of {_kin.graph_as_of}"),
                "{advice}"
            );
            assert!(
                advice.contains("Treat these rows as a lower bound"),
                "{advice}"
            );
            assert!(
                advice.ends_with(
                    "Limiting factor: binding_unproven; call_sites_unresolved ({negative.trust_reason})"
                ),
                "{advice}"
            );
            // Nothing was lost: following each entry gives back the original.
            let mut restored = restore(page.clone(), &page["_kin"]["page"]["compacted"]);
            let envelope = restored["_kin"].as_object_mut().unwrap();
            envelope.remove("page");
            envelope.remove("response");
            restored.as_object_mut().unwrap().remove("next_cursor");
            assert_eq!(restored, original);

            // Control: where the whole reply already fits, it ships untouched.
            let roomy = start_kind(original.clone(), context(3), ceiling + 1, kind).unwrap();
            assert!(roomy["_kin"]["page"].get("compacted").is_none(), "{roomy}");
            assert_eq!(roomy["binding_coverage"], original["binding_coverage"]);
            assert_eq!(roomy["negative"], original["negative"]);
        }
    }

    /// A mismatch keeps opaque scope identity and its warning on every page.
    /// Semantic runtime identity padding controls the repeated shell size.
    fn with_distant_client(mut answer: Value, padding: usize) -> Value {
        answer["_kin"]["repository"] = json!({
            "authority_context": "a".repeat(64), "client_context": "b".repeat(64),
            "warning": crate::first_contact::SEMANTIC_SCOPE_WARNING,
        });
        answer["_kin"]["runtime"] = json!(format!("repo-daemon:{}", "r".repeat(padding)));
        answer
    }

    /// `answer` frozen for paging, with its restating readings collapsed or
    /// as annotated, and how many were collapsed.
    fn frozen(answer: &Value, kind: PageKind, compact: bool) -> (Snapshot, usize) {
        let mut payload = answer.clone();
        let compacted = if compact {
            compact_readings(&mut payload, kind)
        } else {
            Vec::new()
        };
        let collapsed = compacted.len();
        let rows = payload[kind.primary()].as_array().map_or(0, Vec::len);
        let snapshot = freeze(payload, context(7), kind, rows, compacted).unwrap();
        (snapshot, collapsed)
    }

    /// Page a frozen snapshot from its start, following each continuation,
    /// and say how many pages it took or why a page could not be served.
    fn pages_to_end(snapshot: &Snapshot, max_bytes: usize) -> Result<usize, String> {
        let mut cursor = Cursor {
            v: 1,
            snapshot: snapshot.id,
            record: 0,
            field: 0,
            byte: 0,
        };
        for count in 1..10_000 {
            let page = snapshot.page(cursor, max_bytes)?;
            match page["next_cursor"].as_str() {
                Some(token) => cursor = Cursor::decode(token)?,
                None => return Ok(count),
            }
        }
        Err("the snapshot did not finish".into())
    }

    /// At the tool's floor, an answer whose restated compaction index would
    /// leave a page no room for the smallest fragment of a record still pages
    /// to its end: as annotated, every reading whole and nothing to restore.
    /// With room for both, the same answer keeps its compaction on every page.
    #[test]
    fn an_answer_pages_to_its_end_at_the_floor_when_its_compaction_index_leaves_no_room() {
        let _cache_test = cache_test_guard();
        let floor = crate::budget::RESPONSE_MIN_MAX_CHARS;
        for kind in [PageKind::References, PageKind::Trace] {
            // Find the boundary using valid semantic identity rather than
            // depending on the byte length of a private repository coordinate.
            let original = (0..1000)
                .step_by(8)
                .find_map(|padding| {
                    let answer = with_distant_client(
                        restating_answer(kind, 12, inconclusive_verdict()),
                        padding,
                    );
                    let (compacted, collapsed) = frozen(&answer, kind, true);
                    let (whole, _) = frozen(&answer, kind, false);
                    (collapsed > 0
                        && compacted.fragment_page_bound(floor) > floor
                        && whole.fragment_page_bound(floor) <= floor
                        && pages_to_end(&compacted, floor)
                            .is_err_and(|error| error.contains("cannot fit the page budget")))
                    .then_some(answer)
                })
                .expect("semantic shell has a compaction-index boundary at the floor");
            let (compacted, collapsed) = frozen(&original, kind, true);
            let stopped = pages_to_end(&compacted, floor);
            assert!(
                collapsed > 0
                    && stopped
                        .as_ref()
                        .is_err_and(|error| error.contains("cannot fit the page budget")),
                "compacted, the fixture must stop partway at the floor: {stopped:?}, \
                 {collapsed} collapsed"
            );
            assert!(compacted.fragment_page_bound(floor) > floor);
            let (whole, _) = frozen(&original, kind, false);
            assert!(
                pages_to_end(&whole, floor).is_ok() && whole.fragment_page_bound(floor) <= floor,
                "as annotated, the fixture must page to its end at the floor"
            );

            let first = start_kind(original.clone(), context(7), floor, kind).unwrap();
            let (answer, pages) = drain(first, &context(7), floor, kind);
            assert!(pages.len() > 1, "the fixture must page");
            for page in &pages {
                assert!(page["_kin"]["page"].get("compacted").is_none(), "{page}");
                assert_eq!(page["_kin"]["repository"], original["_kin"]["repository"]);
                assert_eq!(page["_kin"]["graph_as_of"], original["_kin"]["graph_as_of"]);
                assert_eq!(
                    page["_kin"]["verdict"]["limiting_factor"],
                    original["_kin"]["verdict"]["limiting_factor"]
                );
            }
            assert_eq!(answer, original, "every reading arrives whole");

            // Control: where the index and a fragment both fit, compaction stays.
            let roomy = 2 * floor;
            let first = start_kind(original.clone(), context(7), roomy, kind).unwrap();
            let (answer, pages) = drain(first, &context(7), roomy, kind);
            assert!(pages.len() > 1, "the control must page");
            let compacted = pages[0]["_kin"]["page"]["compacted"].clone();
            assert_eq!(compacted.as_array().map(Vec::len), Some(collapsed));
            for page in &pages {
                assert_eq!(page["_kin"]["page"]["compacted"], compacted);
            }
            assert_eq!(restore(answer, &compacted), original);
        }
    }

    /// An answer too large for one page even compacted still pages every row
    /// and reading losslessly, and every page carries the answer's own verdict
    /// and limiting factor, never concludes absence while more remains, and
    /// says in its own fields that it is partial.
    #[test]
    fn a_large_answer_pages_losslessly_and_every_page_keeps_the_answers_verdict() {
        let _cache_test = cache_test_guard();
        let certified = json!({"state": "certified", "absence_claim": "not_applicable",
            "safe_to_conclude_absent": false, "limiting_factor": null,
            "inputs": {"call_sites": "certified"}});
        let authoritative = json!({"state": "certified", "absence_claim": "authoritative",
            "safe_to_conclude_absent": true, "limiting_factor": null, "inputs": {}});
        for kind in [PageKind::References, PageKind::Trace] {
            for verdict in [
                inconclusive_verdict(),
                certified.clone(),
                authoritative.clone(),
            ] {
                let original = restating_answer(kind, 60, verdict.clone());
                let first = start_kind(original.clone(), context(5), 4000, kind).unwrap();
                let (answer, pages) = drain(first, &context(5), 4000, kind);
                assert!(pages.len() > 1, "the fixture must page");
                let compacted = pages[0]["_kin"]["page"]["compacted"].clone();
                assert_eq!(compacted.as_array().map(Vec::len), Some(3), "{compacted}");
                let code = match kind {
                    PageKind::Trace => "trace_page_partial",
                    PageKind::References => "reference_page_partial",
                    PageKind::Impact => "impact_page_partial",
                    PageKind::Locate => "locate_page_partial",
                };
                let expected_claim = match verdict["absence_claim"].as_str() {
                    Some("authoritative") => json!("not_claimed"),
                    _ => verdict["absence_claim"].clone(),
                };
                for page in &pages {
                    let page_verdict = &page["_kin"]["verdict"];
                    assert_eq!(page["_kin"]["page"]["complete"], false);
                    assert_eq!(page_verdict["state"], verdict["state"], "{page_verdict}");
                    assert_eq!(page_verdict["limiting_factor"], verdict["limiting_factor"]);
                    assert_eq!(page_verdict["absence_claim"], expected_claim);
                    assert_eq!(page_verdict["safe_to_conclude_absent"], false);
                    assert_eq!(page["negative"]["safe_to_conclude_absent"], false);
                    assert_eq!(page["_kin"]["completeness"]["status"], "partial");
                    assert_eq!(page["_kin"]["completeness"]["limits"], json!([code]));
                    assert_eq!(page["_kin"]["page"]["compacted"], compacted);
                    assert!(
                        crate::verdict::disagreements(page).is_empty(),
                        "{:?}",
                        crate::verdict::disagreements(page)
                    );
                }
                assert_eq!(restore(answer, &compacted), original);
            }
        }
    }

    /// A reach block too large for one page pages its members, frontier and
    /// referrers as rows, losslessly, and its verdict leads them: the first
    /// page states what the rows after it add up to.
    #[test]
    fn a_large_reach_block_pages_its_rows_behind_its_verdict() {
        let _cache_test = super::cache_test_guard();
        let mut original = payload();
        original["chain"] = json!([]);
        original["total_steps"] = json!(0);
        let row = |index: usize| {
            json!({
                "id": format!("00000000-0000-0000-0000-{index:012}"),
                "name": format!("caller_{index}"),
                "distance": 1 + index / 10,
                "via": "00000000-0000-0000-0000-000000000000",
                "line_in_entity": 10 + index,
                "code": format!("return caller_{index}(value)"),
            })
        };
        original["reach"] = json!({
            "state": "closed",
            "statement": "Closed: under this policy these 120 entities are every one that can \
                          end up calling start.",
            "members_total": 120,
            "members": (0..120).map(row).collect::<Vec<_>>(),
            // Possible callers, matched by name alone, page as rows too.
            "name_only_total": 40,
            "name_only": (200..240).map(row).collect::<Vec<_>>(),
            "frontier": [],
            "also_referenced_by": (0..30).map(row).collect::<Vec<_>>(),
            "bounds": [],
        });
        let first = start(original.clone(), context(9), 4000).unwrap();
        assert_eq!(
            first["readings"][0]["key"], "reach",
            "the block's verdict leads its rows: {first:#}"
        );
        assert_eq!(first["readings"][0]["value"]["state"], "closed");
        assert_eq!(first["readings"][0]["value"]["name_only_total"], 40);
        assert!(
            first["readings"][0]["value"].get("name_only").is_none(),
            "the rows page, the count leads: {first:#}"
        );
        assert!(first["reach"]["members"].is_array(), "{first:#}");
        let (answer, pages) = drain(first, &context(9), 4000, PageKind::Trace);
        assert!(pages.len() > 1, "the fixture must page");
        assert_eq!(answer["reach"], original["reach"]);
    }
}
