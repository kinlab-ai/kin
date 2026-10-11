//! The named-scope reading a callers walk takes for every method whose callers
//! it read, so a chain can certify the way a reference answer does.
//!
//! `find_references` stands the method gate aside when its focal's named scope
//! is complete: every call site in the store that spells the method's call
//! name is proven or listed (see [`crate::call_sites::named_block`]). A walk
//! that reads callers asks that question of every method it expanded, because
//! a caller it missed for any of them is a step missing from the chain. This
//! module takes the same reading for each one, through the same census, scan
//! and text reader, and publishes it as the walk's `call_sites` block. The
//! verdict reads that block the way it reads a reference answer's, so a chain
//! whose methods are all accounted for certifies, one whose only open sites are
//! a listed residue is `certified_except` that residue, and one with a method
//! nobody could account for keeps `method_call_resolution_incomplete`, naming
//! that method.

use serde_json::{json, Value};

use kin_model::graph::GraphStore;
use kin_model::{Entity, FocalEscape};

use crate::call_sites::{CandidateSite, FocalScan};
use crate::handlers::common::{EntitySourceScope, HeldSourceAuthority};

/// Methods one walk reads, nearest first: the focal, then the expanded chain
/// in step order. Each costs a store-wide scan of the callers that could call
/// it, the same cost one `find_references` pays, so the reading is bounded by
/// this count rather than by the chain, and a method past it keeps the gate.
pub const METHODS_READ_MAX: usize = 12;

/// Named leftovers a method lists before saying how many more it holds.
/// `find_references` on that method, by its id, lists every one.
pub const SITES_LISTED_PER_METHOD: usize = 10;

/// What the block reads: the callers in the store that could call each
/// method whose callers the walk read, by one of its call names.
pub const SCOPE: &str = "each traced method's named callers";

/// Where the block lists each method's residue.
const BY_STEP: &str = "call_sites.by_step";

/// Why a method the walk expanded was not read.
const UNREAD_PAST_CAP: &str = "past_the_methods_one_reading_covers";
const UNREAD_AFTER_OPEN: &str = "after_a_method_that_is_not_accounted_for";
const UNREAD_GRAPH: &str = "its_callers_could_not_be_read_from_the_graph";

/// One method whose callers a walk read: its step (`0` is the focal) and its
/// entity.
pub struct TraceMethod<'e> {
    pub step: usize,
    pub entity: &'e Entity,
}

/// One method's reading, or why it has none.
enum Reading {
    Scanned(MethodSummary),
    Unread(&'static str),
}

/// What one method's scan says, kept once the scan itself is dropped: the
/// rows and counts the block lists for it and whether it accounts for its
/// callers.
///
/// A scan holds every caller it read, which on an escaping method is every
/// caller in its languages, so a walk keeps this summary rather than the scan
/// and shares it with the reach reading (see [`crate::trace_reach::Scans`]),
/// and a method is scanned once however many readers ask about it.
#[derive(Debug, Clone)]
pub struct MethodSummary {
    leftovers: Vec<Value>,
    value_calls: Option<Value>,
    escape: Option<Value>,
    escape_to_check: bool,
    unkeyed: bool,
    accounted: bool,
    open_codes: Vec<String>,
}

impl MethodSummary {
    /// Summarize `scan` the way [`block`] lists it.
    pub fn of(scan: &FocalScan) -> Self {
        let leftovers = scan
            .named
            .iter()
            .chain(scan.promoted.iter())
            .map(site_row)
            .collect();
        // The sites, and the callers whose sites are not known at all when
        // there are any; `find_references` on the method gives the rest of
        // the reading.
        let value_calls = (!scan.value_calls.is_empty()).then(|| {
            if scan.value_calls.callers_unread > 0 {
                json!({
                    "sites": scan.value_calls.sites,
                    "callers_unread": scan.value_calls.callers_unread,
                })
            } else {
                json!(scan.value_calls.sites)
            }
        });
        let escape = escape_summary(&scan.escape);
        let residue = scan.escape.residue();
        let accounted = accounts_for_its_callers(scan);
        Self {
            leftovers,
            escape_to_check: escape.is_some()
                && residue.is_some_and(|residue| !residue.lists_nothing_to_check()),
            unkeyed: escape.is_some()
                && residue.is_some_and(|residue| !residue.unkeyed_lookups.is_empty()),
            value_calls,
            escape,
            accounted,
            open_codes: if accounted {
                Vec::new()
            } else {
                open_codes(scan)
            },
        }
    }

    /// Whether the scan accounts for every call that could reach the method.
    pub fn accounted(&self) -> bool {
        self.accounted
    }
}

struct MethodReading<'e> {
    step: usize,
    entity: &'e Entity,
    reading: Reading,
}

/// Whether a scan accounts for every call that could reach its method: its
/// named scope is complete, and a method a language may run without a call
/// spelling its name has that dispatch named by the census, so it is listed
/// residue rather than an unknown.
///
/// The second half is where this reading is narrower than a bare implicit
/// dispatch flag. `Flask.__call__` runs on a call of an instance, and the
/// census names exactly that as the method's protocol residue; a chain ending
/// there is complete except that named dispatch, which is what
/// `escape_residue_listed` says. A hook the census names no protocol for
/// keeps the gate.
fn accounts_for_its_callers(scan: &FocalScan) -> bool {
    scan.named_complete()
        && (!scan.implicit_dispatch
            || scan
                .escape
                .residue()
                .is_some_and(|residue| residue.protocol.is_some()))
}

/// Read each method's named scope and return the walk's `call_sites` block.
///
/// The census is one batch over the domain for every method read. The scan
/// and its text reader are the ones `find_references` builds for one focal
/// (see [`crate::handlers::entities::reference_caller_arrival`]), so the two
/// surfaces cannot read one method two ways. Reading stops at the first
/// method that is not accounted for: the chain is not complete whatever the
/// rest say, and each further method is another store-wide scan.
pub fn read_methods<G: GraphStore>(
    store: &G,
    held: &HeldSourceAuthority<'_, G>,
    scope: EntitySourceScope,
    methods: &[TraceMethod<'_>],
) -> Value {
    read_methods_with(
        store,
        held,
        scope,
        methods,
        &mut crate::trace_reach::Scans::default(),
    )
}

/// [`read_methods`], taking each method's scan from `scans` when a reader
/// already took it and leaving every scan it takes there, so the reach
/// reading of the same walk does not scan a method twice.
pub fn read_methods_with<G: GraphStore>(
    store: &G,
    held: &HeldSourceAuthority<'_, G>,
    scope: EntitySourceScope,
    methods: &[TraceMethod<'_>],
    scans: &mut crate::trace_reach::Scans,
) -> Value {
    let read = &methods[..methods.len().min(METHODS_READ_MAX)];
    let uncached: Vec<(&Entity, Vec<String>)> = read
        .iter()
        .filter(|method| !scans.contains(&method.entity.id))
        .map(|method| (method.entity, kin_model::focal_call_names(method.entity)))
        .collect();
    let mut census: std::collections::HashMap<kin_model::EntityId, FocalEscape> =
        if uncached.is_empty() {
            std::collections::HashMap::new()
        } else {
            uncached
                .iter()
                .map(|(entity, _)| entity.id)
                .zip(held.escape_evidence_batch(scope, &uncached))
                .collect()
        };
    let mut readings: Vec<MethodReading<'_>> = Vec::with_capacity(methods.len());
    let mut open = false;
    for method in read {
        if open {
            readings.push(MethodReading {
                step: method.step,
                entity: method.entity,
                reading: Reading::Unread(UNREAD_AFTER_OPEN),
            });
            continue;
        }
        if !scans.contains(&method.entity.id) {
            let text = crate::handlers::external_symbols::CalleeText::with_read_limit_at(
                held,
                crate::call_sites::SCAN_TEXT_READS_MAX,
                scope,
            );
            let escape = census
                .remove(&method.entity.id)
                .unwrap_or(crate::call_sites::NO_CENSUS);
            scans.scan(store, method.entity, escape, &text);
        }
        let reading = match scans.method(&method.entity.id) {
            Some(summary) => Reading::Scanned(summary.clone()),
            None => Reading::Unread(UNREAD_GRAPH),
        };
        open = !matches!(&reading, Reading::Scanned(summary) if summary.accounted);
        readings.push(MethodReading {
            step: method.step,
            entity: method.entity,
            reading,
        });
    }
    for method in &methods[read.len()..] {
        readings.push(MethodReading {
            step: method.step,
            entity: method.entity,
            reading: Reading::Unread(UNREAD_PAST_CAP),
        });
    }
    block(&readings)
}

/// A named leftover as the trace lists it: the caller by id and name, the
/// site inside it, and its state. The file is the caller's own projection,
/// which the chain or `get_entity_source` on the id already gives.
fn site_row(site: &CandidateSite) -> Value {
    json!({
        "caller": site.caller.to_string(),
        "caller_name": site.caller_name,
        "line_in_entity": site.line_in_entity,
        "callee": site.callee,
        "state": site.state.wire(),
    })
}

/// The census residue as counts and the protocol that dispatches the method,
/// or `None` when it lists nothing unsettled. Unkeyed lookups are counted
/// beside the rest, and never make the method's residue one to check.
fn escape_summary(escape: &FocalEscape) -> Option<Value> {
    let residue = escape.residue()?;
    if residue.lists_nothing_unsettled() {
        return None;
    }
    let mut summary = serde_json::Map::new();
    for (key, count) in [
        ("values", residue.values.len()),
        ("implicit_calls", residue.implicit_calls.len()),
        ("reflective", residue.reflective.len()),
        ("unkeyed_lookups", residue.unkeyed_lookups.len()),
        ("unrecorded", residue.unrecorded.len()),
    ] {
        if count > 0 {
            summary.insert(key.into(), json!(count));
        }
    }
    if let Some(protocol) = &residue.protocol {
        summary.insert(
            "protocol".into(),
            match &protocol.through {
                Some(through) => json!(through),
                None => json!({"listed_sites": protocol.sites.len()}),
            },
        );
    }
    Some(Value::Object(summary))
}

/// The code a clause opens with.
fn clause_code(clause: &str) -> &str {
    clause.split(':').next().unwrap_or(clause).trim()
}

/// A method's own clauses that name a gap rather than a listed residue: a
/// caller in its named scope owed, stale, unverified or without a resolver.
fn open_codes(scan: &FocalScan) -> Vec<String> {
    let mut codes: Vec<String> = crate::call_sites::named_clauses(scan)
        .iter()
        .map(|clause| clause_code(clause).to_string())
        .filter(|code| !kin_model::call_site_reading::RESIDUE_CODES.contains(&code.as_str()))
        .collect();
    if scan.named_complete() && scan.implicit_dispatch {
        codes.push("implicit_dispatch_unnamed".to_string());
    }
    codes.dedup();
    codes
}

/// The walk's `call_sites` block over `readings`, in step order.
///
/// It lists only the methods that leave something open, each by step and
/// name, then its named leftovers (never more than
/// [`SITES_LISTED_PER_METHOD`], with the rest counted), its value calls
/// counted, its census residue counted with the protocol that dispatches it,
/// and, for a method that is not accounted for, the codes that say why. Its
/// clauses aggregate those across methods, one per code, so the verdict reads
/// the chain the way it reads one reference answer: settled when every method
/// is accounted for and nothing is listed, `certified_except` when what is
/// open is only listed residue, and inconclusive otherwise.
fn block(readings: &[MethodReading<'_>]) -> Value {
    let mut by_step: Vec<Value> = Vec::new();
    let mut named_sites = 0usize;
    let mut named_methods = 0usize;
    let mut value_methods = 0usize;
    let mut escape_methods = 0usize;
    let mut unkeyed_methods = 0usize;
    let mut unaccounted: Vec<String> = Vec::new();
    let mut unread = 0usize;
    let mut accounted = 0usize;
    let mut scanned = 0usize;
    for method in readings {
        let mut entry = serde_json::Map::new();
        match &method.reading {
            Reading::Unread(reason) => {
                unread += 1;
                entry.insert("unread".into(), json!(reason));
            }
            Reading::Scanned(summary) => {
                scanned += 1;
                let leftovers = &summary.leftovers;
                if !leftovers.is_empty() {
                    named_sites += leftovers.len();
                    named_methods += 1;
                    entry.insert(
                        "candidates".into(),
                        Value::Array(
                            leftovers
                                .iter()
                                .take(SITES_LISTED_PER_METHOD)
                                .cloned()
                                .collect(),
                        ),
                    );
                    if leftovers.len() > SITES_LISTED_PER_METHOD {
                        entry.insert(
                            "candidates_withheld".into(),
                            json!(leftovers.len() - SITES_LISTED_PER_METHOD),
                        );
                    }
                }
                if let Some(value) = &summary.value_calls {
                    value_methods += 1;
                    entry.insert("value_calls".into(), value.clone());
                }
                if let Some(escape) = &summary.escape {
                    if summary.escape_to_check {
                        escape_methods += 1;
                    }
                    if summary.unkeyed {
                        unkeyed_methods += 1;
                    }
                    entry.insert("escape".into(), escape.clone());
                }
                if summary.accounted {
                    accounted += 1;
                } else {
                    let codes = &summary.open_codes;
                    unaccounted.push(format!(
                        "{} ({})",
                        method.entity.name,
                        if codes.is_empty() {
                            "its named scope is not complete".to_string()
                        } else {
                            codes.join(", ")
                        }
                    ));
                    entry.insert("open".into(), json!(codes));
                }
            }
        }
        if entry.is_empty() {
            continue;
        }
        let mut row = serde_json::Map::new();
        row.insert("step".into(), json!(method.step));
        row.insert("entity_name".into(), json!(method.entity.name));
        row.extend(entry);
        by_step.push(Value::Object(row));
    }

    let mut clauses: Vec<String> = Vec::new();
    if !unaccounted.is_empty() || unread > 0 {
        let mut parts = Vec::new();
        if !unaccounted.is_empty() {
            parts.push(format!(
                "the callers of {} are not all proven or listed",
                unaccounted.join(", ")
            ));
        }
        if unread > 0 {
            parts.push(format!(
                "{unread} more method(s) the walk expanded were not read, each marked unread \
                 under `{BY_STEP}`"
            ));
        }
        clauses.push(format!(
            "{}: receiver-method calls are linked by bare name and may be unresolved, and {}, \
             so the chain may be missing callers",
            kin_core::reference_coverage::METHOD_ABSENCE_LIMITING_FACTOR,
            parts.join(", and ")
        ));
    }
    if named_sites > 0 {
        clauses.push(format!(
            "{}: {named_sites} unproven call site(s) spell the name of {named_methods} traced \
             method(s), each listed under `{BY_STEP}`, so the chain is complete except those \
             sites",
            kin_model::call_site_reading::NAMED_LEFTOVERS,
        ));
    }
    if value_methods > 0 {
        clauses.push(format!(
            "{}: {value_methods} traced method(s) may be held as a value, so call sites that \
             never spell their names are counted, not listed, under `{BY_STEP}`",
            kin_model::call_site_reading::VALUE_CALLS_UNMEASURED,
        ));
    }
    if escape_methods > 0 {
        clauses.push(format!(
            "{}: {escape_methods} traced method(s) can run without a call that spells their \
             names, each named under `{BY_STEP}`",
            kin_model::call_site_reading::ESCAPE_RESIDUE_LISTED,
        ));
    }
    if unkeyed_methods > 0 {
        clauses.push(format!(
            "{}: {unkeyed_methods} traced method(s) could be reached by lookups by a name \
             computed at run time that spell none of their names, counted under `{BY_STEP}` \
             rather than listed",
            kin_model::call_site_reading::UNKEYED_LOOKUPS_COUNTED,
        ));
    }
    // One separator divides clauses, so none may carry it inside.
    let clauses: Vec<String> = clauses
        .into_iter()
        .map(|clause| clause.replace(crate::verdict::CLAUSE_SEPARATOR, ", "))
        .collect();
    json!({
        "scope": SCOPE,
        "methods": {
            "walked": readings.len(),
            "read": scanned,
            "accounted": accounted,
        },
        "settled": clauses.is_empty(),
        "clauses": clauses,
        "by_step": by_step,
    })
}

/// Whether a trace payload carries this reading, so its own clauses decide
/// the method gate rather than the bare focal-kind rule.
pub fn decides_method_gate(payload: &Value) -> bool {
    payload
        .get(crate::call_sites::CALL_SITES_KEY)
        .and_then(|block| block.get("methods"))
        .is_some_and(Value::is_object)
}

/// What a step is to its parent, named by the edge that reached it.
///
/// A walk reads `Calls`, `References` and `Imports` edges, and `UsesType`
/// edges when asked to, so a step reached inward is a caller only when a call
/// reached it. The rest refer to their parent without calling it: a
/// `referrer` holds or reads it, an `importer` imports it and a `type_user`
/// names it as a type. Outward steps keep `callee`.
pub fn step_role(direction_role: &str, kind: kin_model::RelationKind) -> &'static str {
    if direction_role != "caller" {
        return "callee";
    }
    match kind {
        kin_model::RelationKind::References => "referrer",
        kin_model::RelationKind::Imports => "importer",
        kin_model::RelationKind::UsesType => "type_user",
        _ => "caller",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::call_sites::fixture::{admit, ledger, proof_context, spanned_entity};
    use kin_db::InMemoryGraph;
    use kin_model::graph::EntityStore as _;
    use kin_model::relation::{GraphNodeId, Relation, RelationOrigin};
    use kin_model::{
        CallSiteState, EntityKind, LanguageId, RelationId, RelationKind, UnresolvedReason,
    };
    use std::collections::HashMap;

    const FOCAL_BODY: &str = "def _find_error_handler(self, e):\n    return None\n";
    const HANDLE_BODY: &str =
        "def handle_exception(self, e):\n    return self._find_error_handler(e)\n";
    const WSGI_BODY: &str =
        "def wsgi_app(self, environ):\n    return self.handle_exception(environ)\n";
    const REGISTER_BODY: &str = "def register(self):\n    kept = self.handle_exception\n";
    const CLASS_BODY: &str = "class Flask(App):\n    pass\n";
    const USER_BODY: &str =
        "def handle_user_exception(self, e):\n    return self._find_error_handler(e)\n";

    fn of_kind(mut entity: Entity, kind: EntityKind) -> Entity {
        entity.kind = kind;
        entity
    }

    fn relation(src: &Entity, dst: &Entity, kind: RelationKind) -> Relation {
        Relation {
            id: RelationId::new(),
            kind,
            src: GraphNodeId::Entity(src.id),
            dst: GraphNodeId::Entity(dst.id),
            confidence: 1.0,
            origin: RelationOrigin::Parsed,
            created_in: None,
            import_source: None,
            evidence: Vec::new(),
        }
    }

    /// A small Flask: `App._find_error_handler` called by
    /// `Flask.handle_exception`, which `Flask.wsgi_app` calls and
    /// `Flask.register` holds as a value, and the class `Flask` naming the
    /// focal as a type. Every caller carries a current ledger whose sites are
    /// proven, so every method's named scope is complete. Two unspanned
    /// entities in other files link calls, imports and references across
    /// files, which is what lets an answer over this graph certify at all.
    struct Flask {
        store: InMemoryGraph,
        focal: Entity,
        handle: Entity,
        context: kin_model::ResolutionRecord,
    }

    fn flask() -> Flask {
        let store = InMemoryGraph::new();
        let file = "src/flask/app.py";
        let focal = of_kind(
            spanned_entity(
                "App._find_error_handler",
                file,
                LanguageId::Python,
                0,
                FOCAL_BODY,
            ),
            EntityKind::Method,
        );
        let handle = of_kind(
            spanned_entity(
                "Flask.handle_exception",
                file,
                LanguageId::Python,
                100,
                HANDLE_BODY,
            ),
            EntityKind::Method,
        );
        let wsgi = of_kind(
            spanned_entity("Flask.wsgi_app", file, LanguageId::Python, 300, WSGI_BODY),
            EntityKind::Method,
        );
        let register = of_kind(
            spanned_entity(
                "Flask.register",
                file,
                LanguageId::Python,
                500,
                REGISTER_BODY,
            ),
            EntityKind::Method,
        );
        let class = of_kind(
            spanned_entity("Flask", file, LanguageId::Python, 700, CLASS_BODY),
            EntityKind::Class,
        );
        let mut witness_caller =
            spanned_entity("witness_caller", "src/w1.py", LanguageId::Python, 0, "x");
        witness_caller.span = None;
        let mut witness_callee =
            spanned_entity("witness_callee", "src/w2.py", LanguageId::Python, 0, "y");
        witness_callee.span = None;
        let context = proof_context(LanguageId::Python, "current");
        let id = context.id();
        admit(
            &store,
            &[
                &focal,
                &handle,
                &wsgi,
                &register,
                &class,
                &witness_caller,
                &witness_callee,
            ],
            vec![
                context.clone(),
                ledger(&focal, FOCAL_BODY, id, vec![]),
                ledger(
                    &handle,
                    HANDLE_BODY,
                    id,
                    vec![(
                        "_find_error_handler",
                        CallSiteState::ProvenTarget { target: focal.id },
                    )],
                ),
                ledger(
                    &wsgi,
                    WSGI_BODY,
                    id,
                    vec![(
                        "handle_exception",
                        CallSiteState::ProvenTarget { target: handle.id },
                    )],
                ),
                ledger(&register, REGISTER_BODY, id, vec![]),
                ledger(&class, CLASS_BODY, id, vec![]),
            ],
        );
        for (src, dst, kind) in [
            (&handle, &focal, RelationKind::Calls),
            (&wsgi, &handle, RelationKind::Calls),
            (&register, &handle, RelationKind::References),
            (&class, &focal, RelationKind::UsesType),
            (&witness_caller, &witness_callee, RelationKind::Calls),
            (&witness_caller, &witness_callee, RelationKind::Imports),
            (&witness_caller, &witness_callee, RelationKind::References),
        ] {
            store.upsert_relation(&relation(src, dst, kind)).unwrap();
        }
        Flask {
            store,
            focal,
            handle,
            context,
        }
    }

    /// `Flask.handle_user_exception` calls the focal by name, and no resolver
    /// proved the call, so the graph holds no edge for it and the walk cannot
    /// reach it. Its site is the named leftover.
    fn with_unproven_caller(fixture: &Flask) -> Entity {
        let user = of_kind(
            spanned_entity(
                "Flask.handle_user_exception",
                "src/flask/app.py",
                LanguageId::Python,
                900,
                USER_BODY,
            ),
            EntityKind::Method,
        );
        admit(
            &fixture.store,
            &[&user],
            vec![ledger(
                &user,
                USER_BODY,
                fixture.context.id(),
                vec![(
                    "_find_error_handler",
                    CallSiteState::Unresolved {
                        reason: UnresolvedReason::NoAnswer,
                    },
                )],
            )],
        );
        user
    }

    fn ready_envelope() -> crate::Envelope {
        crate::Envelope::daemon().with_health(&json!({
            "initialized": true,
            "graph_loaded": true,
            "graph_generation": 12,
        }))
    }

    fn traced(store: &InMemoryGraph, focal: &Entity, extra: &[(&str, Value)]) -> Value {
        let mut args = HashMap::from([
            ("focal".to_string(), json!(focal.id.to_string())),
            ("direction".to_string(), json!("callers")),
            ("depth".to_string(), json!(8)),
        ]);
        for (key, value) in extra {
            args.insert((*key).to_string(), value.clone());
        }
        let result = crate::finalize_with_envelope(
            crate::handlers::entities::handle_trace_data_flow(&args, store).unwrap(),
            ready_envelope(),
            "trace_data_flow",
        );
        let crate::types::ContentBlock::Text { text } = &result.content[0];
        serde_json::from_str(text).unwrap()
    }

    fn factor(response: &Value) -> String {
        response["_kin"]["verdict"]["limiting_factor"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    fn rows(response: &Value) -> Vec<(String, String)> {
        response["chain"]
            .as_array()
            .unwrap()
            .iter()
            .map(|step| {
                (
                    step["entity_name"].as_str().unwrap().to_string(),
                    step["role"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    }

    /// Every method whose callers the walk read, the focal and each expanded
    /// method in the chain, has a complete named scope and nothing listed, so
    /// the method gate has nothing left to guard and the chain certifies.
    #[test]
    fn a_complete_callers_chain_certifies() {
        let _servers =
            crate::edge_coverage::test_support::scoped_language_servers(&[LanguageId::Python]);
        let fixture = flask();
        let response = traced(&fixture.store, &fixture.focal, &[]);
        let block = &response[crate::call_sites::CALL_SITES_KEY];
        assert_eq!(
            block["methods"],
            json!({"walked": 4, "read": 4, "accounted": 4}),
            "{block:#}"
        );
        assert_eq!(block["settled"], true, "{block:#}");
        assert_eq!(block["by_step"], json!([]), "{block:#}");
        assert!(
            !factor(&response).contains("method_call_resolution_incomplete"),
            "{:#}",
            response["_kin"]["verdict"]
        );
        assert_eq!(
            response["_kin"]["verdict"]["state"], "certified",
            "{:#}",
            response["_kin"]["verdict"]
        );
    }

    /// A caller that spells the focal's name and whose call no resolver
    /// proved is the one site that could add to the chain. It is listed under
    /// the focal's step with its caller and line, and the chain is complete
    /// except that site, exactly as `find_references` would say of the focal.
    #[test]
    fn a_chain_with_a_named_leftover_is_certified_except_that_site() {
        let _servers =
            crate::edge_coverage::test_support::scoped_language_servers(&[LanguageId::Python]);
        let fixture = flask();
        let user = with_unproven_caller(&fixture);
        let response = traced(&fixture.store, &fixture.focal, &[]);
        let block = &response[crate::call_sites::CALL_SITES_KEY];
        assert_eq!(block["settled"], false, "{block:#}");
        assert_eq!(
            block["methods"],
            json!({"walked": 4, "read": 4, "accounted": 4}),
            "{block:#}"
        );
        let focal_row = &block["by_step"][0];
        assert_eq!(focal_row["step"], 0, "{block:#}");
        assert_eq!(focal_row["entity_name"], "App._find_error_handler");
        // This arm holds no repository authority, so it cannot quote the
        // caller's text: the site is addressed by its caller, and the daemon's
        // arm adds its line and callee.
        assert_eq!(
            focal_row["candidates"],
            json!([{
                "caller": user.id.to_string(),
                "caller_name": "Flask.handle_user_exception",
                "line_in_entity": null,
                "callee": null,
                "state": "unresolved",
            }]),
            "{block:#}"
        );
        assert_eq!(
            response["_kin"]["verdict"]["state"], "certified_except",
            "{:#}",
            response["_kin"]["verdict"]
        );
        assert_eq!(factor(&response), "named_leftovers");
        assert_eq!(
            response["_kin"]["verdict"]["safe_to_conclude_absent"],
            false
        );
    }

    /// A walk its own cap cut short keeps saying so: the reading accounts for
    /// every method it read, and the chain is still not the whole chain. Here
    /// the cap keeps the call from `wsgi_app` and drops the reference from
    /// `register`, and the walk goes on beneath the clipped node, so the
    /// walk's clause is the spine clause that supersedes the plain cap one.
    #[test]
    fn a_truncated_chain_keeps_its_walk_clause() {
        let _servers =
            crate::edge_coverage::test_support::scoped_language_servers(&[LanguageId::Python]);
        let fixture = flask();
        let response = traced(
            &fixture.store,
            &fixture.handle,
            &[("limit_per_step", json!(1))],
        );
        assert_eq!(response["truncated"], true, "{response:#}");
        assert_eq!(response["clipped_steps"][0]["dropped_callers"], 1);
        let verdict = &response["_kin"]["verdict"];
        assert_eq!(verdict["state"], "inconclusive", "{verdict:#}");
        let factor = factor(&response);
        assert!(factor.contains("answer_truncated"), "{factor}");
        assert!(factor.contains("trace_spine_clipped"), "{factor}");
        assert_eq!(
            response[crate::call_sites::CALL_SITES_KEY]["settled"],
            true,
            "{response:#}"
        );
        assert!(
            !factor.contains("method_call_resolution_incomplete"),
            "every method the walk read is accounted for, so the cap is the one reason: {factor}"
        );
    }

    /// A method a caller could call by name whose call sites the graph owes is
    /// not accounted for, and the gate stays, naming that method.
    #[test]
    fn a_method_whose_callers_are_owed_keeps_the_gate_and_is_named() {
        let _servers =
            crate::edge_coverage::test_support::scoped_language_servers(&[LanguageId::Python]);
        let fixture = flask();
        // Its parse-time preview is its whole body, which spells
        // `handle_exception` and not the focal's name, so it is in the named
        // scope of `Flask.handle_exception` alone.
        let body = "def full_dispatch_request(self):\n    return self.handle_exception(None)\n";
        let mut owed = of_kind(
            spanned_entity(
                "Flask.full_dispatch_request",
                "src/flask/app.py",
                LanguageId::Python,
                1100,
                body,
            ),
            EntityKind::Method,
        );
        owed.metadata.extra.insert(
            kin_parser::extract::EMBEDDING_BODY_PREVIEW_KEY.into(),
            json!(body),
        );
        fixture.store.upsert_entity(&owed).unwrap();
        let response = traced(&fixture.store, &fixture.focal, &[]);
        let block = &response[crate::call_sites::CALL_SITES_KEY];
        assert_eq!(block["methods"]["accounted"], 1, "{block:#}");
        let clause = block["clauses"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .find(|clause| clause.starts_with("method_call_resolution_incomplete: "))
            .unwrap_or_else(|| panic!("{block:#}"));
        assert!(clause.contains("Flask.handle_exception"), "{clause}");
        assert!(
            factor(&response).contains("method_call_resolution_incomplete"),
            "{:#}",
            response["_kin"]["verdict"]
        );
        assert_eq!(response["_kin"]["verdict"]["state"], "inconclusive");
    }

    /// A step says what it is to its parent: a caller only when a call reached
    /// it. A reference is a `referrer`, and a type use is withheld from a
    /// callers walk and counted, unless `include_type_edges` asks for it.
    #[test]
    fn a_callers_walk_tells_a_caller_from_a_reference_and_a_type_use() {
        let fixture = flask();
        let response = traced(&fixture.store, &fixture.focal, &[]);
        let mut steps = rows(&response);
        steps.sort();
        assert_eq!(
            steps,
            vec![
                ("Flask.handle_exception".to_string(), "caller".to_string()),
                ("Flask.register".to_string(), "referrer".to_string()),
                ("Flask.wsgi_app".to_string(), "caller".to_string()),
            ]
        );
        assert_eq!(response["type_uses_withheld"], 1, "{response:#}");
        assert!(response.get("terminal_annotation_steps").is_none());

        let opened = traced(
            &fixture.store,
            &fixture.focal,
            &[("include_type_edges", json!(true))],
        );
        assert!(
            rows(&opened).contains(&("Flask".to_string(), "type_user".to_string())),
            "{:#}",
            opened["chain"]
        );
        assert!(opened.get("type_uses_withheld").is_none());
    }

    #[test]
    fn a_step_role_names_the_edge_that_reached_it() {
        for (direction, kind, role) in [
            ("caller", RelationKind::Calls, "caller"),
            ("caller", RelationKind::References, "referrer"),
            ("caller", RelationKind::Imports, "importer"),
            ("caller", RelationKind::UsesType, "type_user"),
            ("callee", RelationKind::Calls, "callee"),
            ("callee", RelationKind::References, "callee"),
        ] {
            assert_eq!(step_role(direction, kind), role, "{direction} {kind:?}");
        }
    }
}
