// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Call sites, as every read tool serves them.
//!
//! A caller's call-site ledger holds one state for each call expression the
//! parser reads in its body. What a tool serves for a site is read through
//! [`kin_model::read_caller_sites`], the one reading every surface shares, and
//! added up with [`kin_model::CallSiteTally`]. This module is where the read
//! tools meet that reading:
//!
//! - [`GraphSiteFacts`] answers the reading's questions from a [`GraphStore`]:
//!   the ledger under [`ResolutionRecordId::call_sites`], the proof context
//!   validated in the selected graph for each language, and why
//!   no resolver can prove a language's sites, from whether that process
//!   switched enrichment off, and how ([`publish_enrichment_switch`]), and the
//!   language-server readiness it probed
//!   ([`crate::edge_coverage::publish_language_server_readiness`]);
//! - [`focal_block`], [`callers_block`] and [`store_block`] build the one
//!   `call_sites` payload block, which carries the tally, the scope it was
//!   taken over and the verdict clauses it calls for, and for a single focal
//!   one row per site;
//! - [`text_lines`] renders that block for a terminal, so the CLI and the MCP
//!   tools say the same thing about one store.
//!
//! A site is addressed inside its caller: `line_in_entity`, counted from 0 at
//! the caller's first line, and `callee`, the text at the site cut from the
//! caller's own body through the caller-body reader the answer already holds.
//! Never a file line.

use std::collections::{BTreeMap, HashMap, HashSet};

use kin_model::entity::{Entity, SourceSpan};
use kin_model::graph::GraphStore;
use kin_model::{
    read_caller_sites, site_state_reason, CallSite, CallSiteFacts, CallSiteLedger, CallSiteTally,
    CallerSites, EntityId, LanguageId, NoResolver, ResolutionRecordId, SiteStateKind,
};
use serde_json::{json, Value};

use crate::handlers::external_symbols::SiteText;
use crate::site_expression::{self, FocalOwner, ReceiverHint};

/// The key every payload carries the block under.
pub const CALL_SITES_KEY: &str = "call_sites";

/// Site rows one block serves before it says how many it withheld.
pub const CALL_SITE_ROWS_MAX: usize = 50;

/// Residue sites one escape summary serves before it counts the rest.
/// Counts and clauses still describe the full census.
pub const CALL_SITE_CANDIDATE_SAMPLE_MAX: usize = 5;

/// Files with owed callers a store-wide block names before it says how many
/// more there are.
pub const OWED_FILES_MAX: usize = 20;

/// The scope of a block taken over one focal's own sites.
pub const FOCAL_SCOPE: &str = "the focal's own body";

/// The scope of a block taken over several focals' own sites.
pub const FOCALS_SCOPE: &str = "the focals' own bodies";

/// The scope of a block taken over the callers in the files that import the
/// focal's file.
pub const FAMILY_SCOPE: &str = "the modules that import the focal's module";

/// The scope of a block taken over the whole store.
pub const STORE_SCOPE: &str = "the store";

// ── The proof contexts resolvers run under now ────────────────────────────

/// The proof context each language's resolver runs under now.
pub type ProofContexts = HashMap<LanguageId, ResolutionRecordId>;

static PUBLISHED_CONTEXTS: std::sync::RwLock<Option<ProofContexts>> = std::sync::RwLock::new(None);

/// Publish the proof context each language's resolver runs under now.
///
/// Published by the process that starts the resolvers, when one starts,
/// because knowing the context needs the resolver running and a query path
/// must not spawn one. A ledger proven under any other context reads as
/// stale to the scheduler. This compatibility publication is not read authority:
/// readers use the selected graph's durable context-validation records.
pub fn publish_current_proof_contexts(contexts: HashMap<LanguageId, ResolutionRecordId>) {
    if let Ok(mut slot) = PUBLISHED_CONTEXTS.write() {
        *slot = Some(contexts);
    }
}

/// The proof contexts [`publish_current_proof_contexts`] last published, or
/// `None` when nothing has, which is not an empty map: it means nobody looked.
pub fn published_current_proof_contexts() -> Option<HashMap<LanguageId, ResolutionRecordId>> {
    PUBLISHED_CONTEXTS
        .read()
        .ok()
        .and_then(|contexts| contexts.clone())
}

/// Lets a test state which proof context each language's resolver runs
/// under. Thread-local, so it holds for the test that set it whether the
/// suite runs threaded or one process per test.
#[cfg(test)]
pub(crate) mod test_support {
    use super::EnrichmentSwitch;

    thread_local! {
        static SWITCHED_OFF: std::cell::Cell<Option<EnrichmentSwitch>> =
            const { std::cell::Cell::new(None) };
    }

    pub(crate) fn switched_off_override() -> Option<EnrichmentSwitch> {
        SWITCHED_OFF.with(std::cell::Cell::get)
    }

    /// Restores the previous answer on drop, including on unwind.
    pub(crate) struct SwitchedOffGuard(Option<EnrichmentSwitch>);

    impl Drop for SwitchedOffGuard {
        fn drop(&mut self) {
            SWITCHED_OFF.with(|slot| slot.set(self.0));
        }
    }

    /// Declare, for the rest of this scope, whether enrichment is switched
    /// off for the process serving the graph.
    #[must_use = "binding the guard is what keeps the declared answer in force"]
    pub(crate) fn scoped_enrichment_switched_off(switched_off: bool) -> SwitchedOffGuard {
        scoped_enrichment_switch(if switched_off {
            EnrichmentSwitch::Off
        } else {
            EnrichmentSwitch::On
        })
    }

    /// Declare, for the rest of this scope, how enrichment stands for the
    /// process serving the graph.
    #[must_use = "binding the guard is what keeps the declared answer in force"]
    pub(crate) fn scoped_enrichment_switch(switch: EnrichmentSwitch) -> SwitchedOffGuard {
        SwitchedOffGuard(SWITCHED_OFF.with(|slot| slot.replace(Some(switch))))
    }
}

// ── Why no resolver can prove a language's call sites ─────────────────────

/// How language-server enrichment stands for the process serving this graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnrichmentSwitch {
    /// Enrichment runs.
    On,
    /// Switched off for the process: an environment switch, or a store whose
    /// graph is its only write authority.
    Off,
    /// Switched off by the repository's own setting, which
    /// `kin daemon sweep --enable` turns on.
    OffForRepository,
}

static ENRICHMENT_SWITCHED_OFF: std::sync::RwLock<Option<EnrichmentSwitch>> =
    std::sync::RwLock::new(None);

/// Publish how language-server enrichment stands for the process serving this
/// graph.
///
/// Published by the process that decides it, when it decides it, because a
/// switched-off daemon publishes no language-server readiness: it never
/// looks. Until this is called it is unknown, and unknown claims nothing.
pub fn publish_enrichment_switch(switch: EnrichmentSwitch) {
    if let Ok(mut slot) = ENRICHMENT_SWITCHED_OFF.write() {
        *slot = Some(switch);
    }
}

/// How enrichment stands: a test's own answer, when it declared one, and the
/// published one otherwise.
fn enrichment_switched_off() -> Option<EnrichmentSwitch> {
    #[cfg(test)]
    if let Some(switch) = test_support::switched_off_override() {
        return Some(switch);
    }
    ENRICHMENT_SWITCHED_OFF.read().ok().and_then(|slot| *slot)
}

/// Why no resolver can prove `language`'s call sites, from what the process
/// running the resolvers published: enrichment switched off, by the process or
/// by the repository's own setting, then its probe of the language's server.
/// A server the probe found missing, or that could not start, proves nothing
/// until the host changes, and a language no build of Kin enriches never has
/// one, whatever was published. For a language Kin enriches, nothing
/// published, or a probe that did not report on it, is unknown, and reads as
/// owed.
pub fn no_resolver_for(
    switch: Option<EnrichmentSwitch>,
    readiness: Option<&kin_core::reference_coverage::LanguageServerReadinessMap>,
    language: LanguageId,
) -> Option<NoResolver> {
    use kin_core::reference_coverage::{LanguageServerReadiness, ENRICHABLE_LANGUAGES};
    match switch {
        Some(EnrichmentSwitch::Off) => return Some(NoResolver::EnrichmentOff),
        Some(EnrichmentSwitch::OffForRepository) => {
            return Some(NoResolver::EnrichmentOffForRepository)
        }
        Some(EnrichmentSwitch::On) | None => {}
    }
    // A fact of this build, not of the host, so it holds whether or not a
    // probe was published. A daemon serving only such languages starts no
    // server and publishes nothing, and reading that silence as unknown left
    // every caller in a small Ruby project owed enrichment that never comes.
    if !ENRICHABLE_LANGUAGES.contains(&language) {
        return Some(NoResolver::NoLanguageServer);
    }
    match readiness?.get(&language) {
        Some(LanguageServerReadiness::Absent) => Some(NoResolver::NoLanguageServer),
        Some(LanguageServerReadiness::Unusable { reason }) => Some(NoResolver::ServerCannotStart {
            reason: reason.clone(),
        }),
        Some(LanguageServerReadiness::Usable) => None,
        Some(LanguageServerReadiness::Disabled) => Some(NoResolver::EnrichmentOff),
        None => None,
    }
}

/// [`no_resolver_for`] over what the process running the resolvers has
/// published now, captured once, for a reader that reads ledgers through
/// facts of its own: the impact observation reads a caller with no ledger the
/// way [`GraphSiteFacts`] and the store-wide status read it.
pub fn host_no_resolver() -> impl Fn(LanguageId) -> Option<NoResolver> {
    let switched_off = enrichment_switched_off();
    let readiness = crate::edge_coverage::published_language_server_readiness();
    move |language| no_resolver_for(switched_off, readiness.as_ref(), language)
}

// ── The reading's facts, from a graph ─────────────────────────────────────

/// What the call-site reading learns about a caller, answered from a graph.
///
/// The ledger is the graph's record under [`ResolutionRecordId::call_sites`].
/// A record that could not be read counts as no ledger, which reads as owed
/// enrichment: an unreadable ledger never settles a site. Whether a caller's
/// derivation is owed is not something a graph read can prove cheaply, so it
/// answers `false` and the response's source-derivation observation
/// qualifies the answer instead.
pub struct GraphSiteFacts<'s, G: GraphStore + ?Sized> {
    store: &'s G,
    switched_off: Option<EnrichmentSwitch>,
    readiness: Option<kin_core::reference_coverage::LanguageServerReadinessMap>,
}

impl<'s, G: GraphStore + ?Sized> GraphSiteFacts<'s, G> {
    /// Facts about `store`, under its own recorded context validation.
    pub fn new(store: &'s G) -> Self {
        Self {
            store,
            switched_off: enrichment_switched_off(),
            readiness: crate::edge_coverage::published_language_server_readiness(),
        }
    }
}

impl<G: GraphStore + ?Sized> CallSiteFacts for GraphSiteFacts<'_, G> {
    fn ledger(&self, caller: EntityId) -> Option<CallSiteLedger> {
        self.store
            .lookup_resolution_record(&ResolutionRecordId::call_sites(caller))
            .ok()
            .flatten()
            .and_then(|record| record.as_call_sites().cloned())
    }

    fn current_context(&self, language: LanguageId) -> Option<ResolutionRecordId> {
        self.store
            .lookup_resolution_record(&ResolutionRecordId::context_validation(language))
            .ok()
            .flatten()
            .and_then(|record| {
                record
                    .as_context_validation()
                    .and_then(|validation| validation.current_context())
            })
    }

    fn context_unverified_reason(&self, language: LanguageId) -> String {
        match self
            .store
            .lookup_resolution_record(&ResolutionRecordId::context_validation(language))
        {
            Ok(Some(kin_model::ResolutionRecord::ContextValidation(
                kin_model::ContextValidation {
                    state: kin_model::ContextValidationState::Unverified { reason },
                    ..
                },
            ))) => reason,
            Err(_) => "the selected graph's proof-context validation could not be read".into(),
            _ => "the selected graph has no recorded proof-context validation".into(),
        }
    }

    fn no_resolver(&self, language: LanguageId) -> Option<NoResolver> {
        no_resolver_for(self.switched_off, self.readiness.as_ref(), language)
    }
}

// ── The block ─────────────────────────────────────────────────────────────

/// The block for a tally taken over `scope`: the tally's own fields, the
/// scope, and the verdict clauses it calls for, which are empty exactly when
/// it is settled.
///
/// `by_state` names only the states some site in scope reads as, because
/// this block rides every pack under its token budget; a state it does not
/// name holds no site. The store-wide block lists every state in `shares`.
pub fn block_json(tally: &CallSiteTally, scope: &str) -> Value {
    let mut block = tally.to_json();
    if let Some(by_state) = block["by_state"].as_object_mut() {
        by_state.retain(|_, count| count.as_u64() != Some(0));
    }
    block["scope"] = json!(scope);
    block["clauses"] = json!(tally.clauses(scope));
    block
}

/// Where a site sits inside its caller: its line counted from 0 at the
/// caller's first line, and the text at the site, both cut from the caller's
/// own body through `text`.
fn site_address<T: SiteText + ?Sized>(
    caller: &Entity,
    site: &CallSite,
    text: &T,
) -> (Option<u64>, Result<String, &'static str>) {
    let Some(span) = caller.span.as_ref() else {
        return (None, Err("caller_has_no_span"));
    };
    let start = span.start_byte + site.offset as usize;
    let cut = |start_byte: usize, end_byte: usize| SourceSpan {
        file: span.file.clone(),
        start_byte,
        end_byte,
        start_line: 0,
        start_col: 0,
        end_line: 0,
        end_col: 0,
    };
    let callee = text.quote(caller, &cut(start, start + site.length as usize));
    let line = text
        .quote(caller, &cut(span.start_byte, start))
        .ok()
        .map(|before| before.matches('\n').count() as u64);
    (line, callee)
}

/// One site of a caller's ledger, as the block serves it.
fn site_row<T: SiteText + ?Sized>(
    caller: &Entity,
    reading: &CallerSites,
    site: &CallSite,
    text: &T,
) -> Value {
    let (line_in_entity, callee) = site_address(caller, site, text);
    let mut row = json!({
        "line_in_entity": line_in_entity,
        "callee": callee.as_ref().ok(),
        "state": reading.site_kind(site).wire(),
        "reason": site_state_reason(&site.state),
        "target": site.state.proven_node().map(|node| node.to_string()),
    });
    if let Err(reason) = callee {
        row["callee_unavailable"] = json!(reason);
    }
    if matches!(
        reading,
        CallerSites::Stale(_) | CallerSites::Unverified { .. }
    ) {
        row["recorded_state"] = json!(site.state.wire());
    }
    row
}

/// The block for one focal's own call sites, with a row for each site its
/// ledger holds, at most [`CALL_SITE_ROWS_MAX`] of them.
///
/// `reading` says what stands between the reader and the focal's ledger:
/// `current`, `owed_enrichment`, `owed_derivation`, `proof_context_stale`,
/// `proof_context_unverified` or
/// `no_sites` for a focal with no source text. An owed focal has no rows,
/// because how many sites it holds is not known.
pub fn focal_block<G: GraphStore + ?Sized, T: SiteText + ?Sized>(
    store: &G,
    focal: &Entity,
    text: &T,
) -> Value {
    let facts = GraphSiteFacts::new(store);
    let reading = read_caller_sites(&facts, focal);
    let mut tally = CallSiteTally::default();
    tally.add(&reading);
    let mut block = block_json(&tally, FOCAL_SCOPE);
    block["reading"] = json!(reading.wire());
    let sites = reading
        .ledger()
        .map(|ledger| ledger.sites.as_slice())
        .unwrap_or_default();
    let rows: Vec<Value> = sites
        .iter()
        .take(CALL_SITE_ROWS_MAX)
        .map(|site| site_row(focal, &reading, site, text))
        .collect();
    block["rows"] = Value::Array(rows);
    let withheld = sites.len().saturating_sub(CALL_SITE_ROWS_MAX);
    if withheld > 0 {
        block["rows_withheld"] = json!(withheld);
    }
    // The proof context a stale ledger was proven under, which is what a
    // reader asks after when told it is stale. A current one is the context
    // its resolver runs under now, and naming it adds nothing.
    if let CallerSites::Stale(ledger) = &reading {
        block["stale_context"] = json!(ledger.context.to_string());
    }
    if let CallerSites::Unverified { ledger, reason } = &reading {
        block["unverified_context"] = json!(ledger.context.to_string());
        block["validation_reason"] = json!(reason);
    }
    block
}

/// Read every caller in `callers` into one tally.
pub fn tally_callers<'e, G: GraphStore + ?Sized>(
    store: &G,
    callers: impl IntoIterator<Item = &'e Entity>,
) -> CallSiteTally {
    let facts = GraphSiteFacts::new(store);
    let mut tally = CallSiteTally::default();
    for caller in callers {
        tally.add(&read_caller_sites(&facts, caller));
    }
    tally
}

/// The block for several callers, tallied over `scope`, with no rows.
pub fn callers_block<'e, G: GraphStore + ?Sized>(
    store: &G,
    callers: impl IntoIterator<Item = &'e Entity>,
    scope: &str,
) -> Value {
    block_json(&tally_callers(store, callers), scope)
}

/// One file holding callers whose sites the graph has not settled, because
/// no current ledger describes them.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct OwedFile {
    /// The file, as the graph names it.
    pub file: String,
    /// Callers in it with no current ledger.
    pub callers: u64,
}

/// Every file holding a spanned entity no current ledger describes, with how
/// many such callers each holds, in path order.
///
/// A caller reads as owed when its derivation or its enrichment is owed, and
/// as stale when its ledger was proven under a context its resolver no
/// longer runs under; both are callers with no current ledger. A caller no
/// resolver can prove on this host now is not owed, since no sweep will reach
/// it, and is left out.
pub fn owed_files<G: GraphStore + ?Sized>(store: &G, entities: &[Entity]) -> Vec<OwedFile> {
    let facts = GraphSiteFacts::new(store);
    let mut by_file: BTreeMap<String, u64> = BTreeMap::new();
    for entity in entities {
        let Some(span) = entity.span.as_ref() else {
            continue;
        };
        if matches!(
            read_caller_sites(&facts, entity),
            CallerSites::Current(_) | CallerSites::NoSites | CallerSites::NoResolver { .. }
        ) {
            continue;
        }
        *by_file.entry(span.file.0.clone()).or_insert(0) += 1;
    }
    by_file
        .into_iter()
        .map(|(file, callers)| OwedFile { file, callers })
        .collect()
}

/// Owed callers of `language` in files outside `inside`, the way
/// [`owed_files`] reads them, or `None` when the entity index could not be
/// read.
///
/// The family scope is the files that import the focal's file, and a caller
/// can reach the focal without importing that file: a Python method called
/// through a proxy such as `current_app`, or a Go function called from
/// another file of the same package. While such a caller is still owed its
/// sites, the family tally cannot see it, so a settled family is not a
/// settled answer. Measured on Flask right after `kin init`: `kin refs
/// ensure_sync` printed 10 of its 12 callers as settled while the sweep still
/// owed `views.py`, which reaches it through `current_app`.
///
/// Such a caller reaches the focal by name, so only an owed caller whose body
/// could spell the focal's name is counted (see [`could_name_focal`]). One
/// whose span holds no bytes holds no call, and one whose parse-time preview
/// is its whole body and never spells the name cannot call the focal by it.
/// Every other owed caller counts, including one whose preview was cut short
/// or absent, because a name missing from part of a body proves nothing about
/// the rest.
pub fn owed_outside<G: GraphStore + ?Sized>(
    store: &G,
    language: LanguageId,
    inside: &std::collections::HashSet<String>,
    names: &[String],
) -> Option<Vec<OwedFile>> {
    owed_outside_scoped(store, language, inside, names, true)
}

/// [`owed_outside`] restricted to callers whose stored role is not `Test`
/// when `include_tests` is false. Unknown caller state remains owed.
pub fn owed_outside_scoped<G: GraphStore + ?Sized>(
    store: &G,
    language: LanguageId,
    inside: &std::collections::HashSet<String>,
    names: &[String],
    include_tests: bool,
) -> Option<Vec<OwedFile>> {
    let filter = kin_model::graph::EntityFilter {
        languages: Some(vec![language]),
        ..Default::default()
    };
    let entities = store.query_entities(&filter).ok()?;
    let outside: Vec<Entity> = entities
        .into_iter()
        .filter(|entity| caller_in_scope(entity, include_tests))
        .filter(|entity| {
            entity.span.as_ref().is_some_and(|span| {
                !inside.contains(&span.file.0) && span.start_byte < span.end_byte
            }) && could_name_focal(entity, names)
        })
        .collect();
    Some(owed_files(store, &outside))
}

/// Whether a current graph entity contributes to the requested caller scope.
/// Role is persisted entity metadata, never inferred from its name or path.
pub(crate) fn caller_in_scope(entity: &Entity, include_tests: bool) -> bool {
    include_tests || entity.role != kin_model::EntityRole::Test
}

/// The longest preview the parser keeps whole; a longer body is summarised
/// with gaps, so its preview no longer proves what the body leaves out.
const WHOLE_BODY_PREVIEW_CHARS: usize = 8000;

pub use kin_model::{calling_languages, focal_call_names, FocalEscape};

/// The body preview the parser keeps on `entity`, when it is the whole body:
/// every identifier the body holds, with its whitespace collapsed. `None` for
/// an entity with no preview, or one cut short because the body is longer
/// than [`WHOLE_BODY_PREVIEW_CHARS`].
pub fn whole_body_preview(entity: &Entity) -> Option<&str> {
    entity
        .metadata
        .extra
        .get(kin_parser::extract::EMBEDDING_BODY_PREVIEW_KEY)
        .and_then(Value::as_str)
        .filter(|preview| preview.chars().count() <= WHOLE_BODY_PREVIEW_CHARS)
}

/// Whether the text `entity` records of its body spells one of `names`:
/// true only for a name the parse-time preview holds, whole or cut short.
/// Where [`could_name_focal`] asks what cannot be ruled out, this asks what
/// was seen.
fn spells_focal_name(entity: &Entity, names: &[String]) -> bool {
    entity
        .metadata
        .extra
        .get(kin_parser::extract::EMBEDDING_BODY_PREVIEW_KEY)
        .and_then(Value::as_str)
        .is_some_and(|preview| {
            names
                .iter()
                .filter(|name| !name.is_empty())
                .any(|name| kin_model::spells_identifier(preview, name))
        })
}

/// Whether `entity`'s body could spell one of `names`, the names a call to
/// the focal is spelled with (see [`focal_call_names`]), read off its
/// parse-time preview: false only when that preview is the whole body and
/// spells none of them. A focal with no call name rules nothing out.
pub fn could_name_focal(entity: &Entity, names: &[String]) -> bool {
    let names: Vec<&str> = names
        .iter()
        .map(String::as_str)
        .filter(|name| !name.is_empty())
        .collect();
    if names.is_empty() {
        return true;
    }
    let preview = entity
        .metadata
        .extra
        .get(kin_parser::extract::EMBEDDING_BODY_PREVIEW_KEY)
        .and_then(Value::as_str);
    match preview {
        None => true,
        Some(preview) if preview.chars().count() > WHOLE_BODY_PREVIEW_CHARS => true,
        Some(preview) => names
            .iter()
            .any(|name| kin_model::spells_identifier(preview, name)),
    }
}

/// The clause a family block carries while callers outside its scope are
/// owed, or while whether they are could not be read.
pub fn owed_outside_clause(owed: Option<&[OwedFile]>) -> Option<String> {
    match owed {
        None => Some(owed_outside_unreadable_clause()),
        Some([]) => None,
        Some(files) => Some(owed_outside_counts_clause(
            files.len() as u64,
            files.iter().map(|file| file.callers).sum(),
        )),
    }
}

/// [`owed_outside_clause`] for `callers` owed callers in `files` files, so a
/// reader of the published counts states the same sentence.
pub fn owed_outside_counts_clause(files: u64, callers: u64) -> String {
    format!(
        "{}: {callers} caller(s) in {files} module(s) outside {FAMILY_SCOPE} have call sites the \
         graph has not settled yet because their derivation or enrichment is owed, and a caller \
         can reach the focal without importing its module, so one may be missing from this \
         answer until `kin daemon sweep` settles them",
        kin_model::call_site_reading::CALL_SITES_OWED
    )
}

/// [`owed_outside_clause`] when the entity index could not be read.
pub fn owed_outside_unreadable_clause() -> String {
    format!(
        "{}: the entity index for the focal's language could not be read, so callers outside \
         {FAMILY_SCOPE} were not checked, and one that reaches the focal without importing its \
         module may be missing from this answer",
        kin_model::call_site_reading::CALL_SITES_OWED
    )
}

/// The block for the callers in the files that import the focal's file,
/// qualified by the owed callers outside them.
///
/// Unsettled with [`owed_outside_clause`] whenever that clause applies, even
/// when every site inside the family is settled, and it names the outside
/// files it counted, at most [`OWED_FILES_MAX`] of them.
pub fn family_block(tally: &CallSiteTally, owed_outside: Option<&[OwedFile]>) -> Value {
    let mut block = block_json(tally, FAMILY_SCOPE);
    let Some(clause) = owed_outside_clause(owed_outside) else {
        return block;
    };
    block["settled"] = json!(false);
    if let Some(clauses) = block["clauses"].as_array_mut() {
        clauses.push(json!(clause));
    } else {
        block["clauses"] = json!([clause]);
    }
    match owed_outside {
        Some(files) => {
            block["owed_outside_scope"] = json!({
                "file_count": files.len(),
                "callers": files.iter().map(|file| file.callers).sum::<u64>(),
                "files": files.iter().take(OWED_FILES_MAX).collect::<Vec<_>>(),
            });
            if files.len() > OWED_FILES_MAX {
                block["owed_outside_scope"]["files_withheld"] = json!(files.len() - OWED_FILES_MAX);
            }
        }
        None => block["owed_outside_scope"] = json!({ "unreadable": true }),
    }
    block
}

/// The store-wide block: the tally over every entity the store holds, each
/// state's share of the census, how many callers are owed, and the files
/// holding them, at most [`OWED_FILES_MAX`] of them.
///
/// The census is the sites the read ledgers hold, so the shares add up to it.
/// An owed caller contributes no site, since how many it holds is not known,
/// which is why a store with callers owed is not settled whatever its shares
/// say.
pub fn store_block<G: GraphStore + ?Sized>(store: &G) -> crate::error::Result<Value> {
    let entities = store
        .list_all_entities()
        .map_err(crate::error::McpError::graph)?;
    Ok(store_block_over(store, &entities))
}

/// [`store_block`] over a listing of every entity the caller already holds,
/// so a surface that listed the store once does not list it again.
pub fn store_block_over<G: GraphStore + ?Sized>(store: &G, entities: &[Entity]) -> Value {
    let tally = tally_callers(store, entities);
    let mut block = block_json(&tally, STORE_SCOPE);
    block["census"] = json!(tally.sites);
    block["callers_owed"] = json!(tally.callers_owed());
    let shares: serde_json::Map<String, Value> = SiteStateKind::ALL
        .iter()
        .filter(|kind| !kind.is_uncounted())
        .map(|kind| {
            (
                kind.wire().to_string(),
                json!({
                    "sites": tally.count(*kind),
                    "share": tally.share(*kind),
                }),
            )
        })
        .collect();
    block["shares"] = Value::Object(shares);
    let owed = owed_files(store, entities);
    block["owed_file_count"] = json!(owed.len());
    block["owed_files"] = json!(owed.iter().take(OWED_FILES_MAX).collect::<Vec<_>>());
    if owed.len() > OWED_FILES_MAX {
        block["owed_files_withheld"] = json!(owed.len() - OWED_FILES_MAX);
    }
    block
}

// ── Which unsettled sites anywhere in the store could be calls to one focal ──

/// The scope of a block taken over every caller in the store whose body
/// could call the focal by name, or through a value when the focal escapes.
pub const NAMED_SCOPE: &str = "the store's callers that could call the focal";

/// A site reader that holds no text, for a surface with no graph-held body to
/// read. Every site it is asked about reads as unknown, so every rule that
/// needs text keeps the site.
pub struct NoSiteText;

impl SiteText for NoSiteText {
    fn quote(&self, _caller: &Entity, _site: &SourceSpan) -> Result<String, &'static str> {
        Err("caller_text_not_held")
    }
}

/// `entity`'s exact text from its first byte, read through `text`, or `None`
/// when the reader cannot give it.
fn exact_text<T: SiteText + ?Sized>(text: &T, entity: &Entity) -> Option<String> {
    let span = entity.span.as_ref()?;
    text.quote(
        entity,
        &SourceSpan {
            file: span.file.clone(),
            start_byte: span.start_byte,
            end_byte: span.end_byte,
            start_line: 0,
            start_col: 0,
            end_line: 0,
            end_col: 0,
        },
    )
    .ok()
}

/// A recorded site's own bytes, without requiring the caller's whole body.
/// Invalid or empty coordinates cannot establish which callee was spelled.
fn exact_site_text<T: SiteText + ?Sized>(
    text: &T,
    entity: &Entity,
    site: &CallSite,
) -> Option<String> {
    let span = entity.span.as_ref()?;
    let start_byte = span.start_byte.checked_add(site.offset as usize)?;
    let end_byte = start_byte.checked_add(site.length as usize)?;
    if start_byte >= end_byte || end_byte > span.end_byte {
        return None;
    }
    text.quote(
        entity,
        &SourceSpan {
            file: span.file.clone(),
            start_byte,
            end_byte,
            start_line: 0,
            start_col: 0,
            end_line: 0,
            end_col: 0,
        },
    )
    .ok()
    .filter(|text| !text.is_empty())
}

/// Whether `text` is one identifier, the shape of a placeable callee token.
fn is_identifier(text: &str) -> bool {
    let mut chars = text.chars();
    chars
        .next()
        .is_some_and(|first| first.is_alphabetic() || first == '_' || first == '$')
        && chars
            .all(|character| character.is_alphanumeric() || character == '_' || character == '$')
}

/// A call site anywhere in the store that could be a call to a focal but is
/// not settled.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CandidateSite {
    /// The caller holding the site.
    pub caller: EntityId,
    /// The caller's name, as the graph names it.
    pub caller_name: String,
    /// The caller's file, as the graph names it. Served as
    /// `projection.path`: a projection of the caller, never its address,
    /// which is `caller`.
    #[serde(rename = "projection", serialize_with = "serialize_projection")]
    pub caller_file: Option<String>,
    /// The module that holds the caller, which is how a reader places the
    /// site: the projection above is withheld at the agent surface.
    #[serde(rename = "module", skip_serializing_if = "Option::is_none")]
    pub caller_module: Option<String>,
    /// The site's line counted from 0 at the caller's first line, when the
    /// caller's text was read.
    pub line_in_entity: Option<u64>,
    /// The site's callee token, when the site has one and the caller's text
    /// was read. `None` for a site with no placeable callee.
    pub callee: Option<String>,
    /// What a reader serves for the site.
    pub state: SiteStateKind,
    /// The site's own reason: the unresolved reason, the server failure or
    /// the not-in-build reason.
    pub state_reason: Option<String>,
    /// Why it can reach the focal (see [`kin_model::site_could_call_at`]).
    pub reason: &'static str,
    /// The call as written, cut from the caller's own body: its receiver and
    /// callee through its argument list, on one line (see
    /// [`site_expression::read_expression`]). Read for every named leftover,
    /// never for a value call, which is only counted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expr: Option<String>,
    /// What the callee is reached through, when the site names one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub receiver: Option<String>,
    /// What the receiver says about reaching the focal's owner (see
    /// [`ReceiverHint`]), left out when it says nothing either way.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub receiver_hint: Option<&'static str>,
    /// Why a named leftover carries no `expr`: its caller's body could not be
    /// read, or the site is not inside it. Never a guess in its place.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expr_unavailable: Option<&'static str>,
    /// For a named leftover made through a receiver, the link of the chain
    /// typing that receiver the graph could not establish (see
    /// [`crate::receiver_binding::chain_gap`]): the step, the name it had to
    /// resolve and why it did not. Absent when the graph cannot tell.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub receiver_binding_missing: Option<Value>,
    /// For a named leftover made on a name its file imports, the module the
    /// import statement names and whether no file of the repository can
    /// supply it (see [`site_expression::RECEIVER_IMPORT_KEY`]). Absent when
    /// the file's record does not bind the receiver by one import statement.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub receiver_import: Option<Value>,
    /// Whether the call is written on no value, in a language whose adapter
    /// says a bare name never reads a member through an implicit instance
    /// ([`kin_parser::site_syntax::SiteSyntax::bare_calls_reach_no_member`]):
    /// its callee token follows no member operator, so it calls a name its
    /// scope binds and reads no member off an object (see
    /// [`site_expression::written_bare`]). Left out for a call made on one,
    /// for a site whose text was not read and for every other language.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub bare_call: bool,
}

/// Fill `row`'s expression fields from its caller's exact text `body` at the
/// site keyed `(offset, length)`: the call as written, its receiver, and, for
/// a site whose callee spells a call name, what that receiver says about
/// reaching the focal's owner.
fn address_expression(
    row: &mut CandidateSite,
    caller: &Entity,
    body: &str,
    (offset, length): (u32, u32),
    owner: &FocalOwner,
) {
    let syntax = site_expression::syntax_of(caller);
    let start = offset as usize;
    let Some(expression) =
        site_expression::read_expression(body, start, start + length as usize, &syntax)
    else {
        row.expr_unavailable = Some("site_outside_caller");
        return;
    };
    if row.reason == kin_model::call_site_reading::REACH_CALLEE_SPELLS {
        row.receiver_hint = expression
            .receiver
            .as_deref()
            .map_or(ReceiverHint::Unknown, |receiver| {
                site_expression::receiver_hint(receiver, caller, owner, &syntax)
            })
            .wire();
    }
    // Only where the caller's adapter says a bare name never reads a member
    // through an implicit instance.
    row.bare_call = syntax.bare_calls_reach_no_member
        && expression.receiver.is_none()
        && site_expression::written_bare(body, start);
    row.expr = Some(expression.expr);
    row.receiver = expression.receiver;
    row.expr_unavailable = None;
}

/// A candidate's file as the labelled projection every reference row serves:
/// `{"path": ...}`.
fn serialize_projection<S: serde::Serializer>(
    path: &Option<String>,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    use serde::ser::SerializeMap;
    let mut projection = serializer.serialize_map(Some(1))?;
    projection.serialize_entry("path", path)?;
    projection.end()
}

/// A caller a store-wide scan read, with its reading narrowed to its settled
/// sites and the unsettled ones that could call the focal.
#[derive(Debug, Clone)]
pub struct ScannedCaller {
    pub entity: Entity,
    pub reading: CallerSites,
    /// Whether the caller is in the named scope: its body could spell a call
    /// name of the focal, or a proven site calls the focal from it. A caller
    /// outside that scope was read only because the focal may be held as a
    /// value, and whatever it holds is counted as a value call.
    pub named: bool,
}

/// The unsettled sites a reading kept only because the focal may be held as a
/// value or reached through a name computed at run time. None of them spells a
/// call name of the focal, so they are counted once, beside the census reason
/// that kept them, rather than listed.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct ValueCalls {
    /// Sites kept this way.
    pub sites: u64,
    /// Callers holding them.
    pub callers: u64,
    /// Callers that never spell a call name of the focal whose sites are not
    /// known at all, because their derivation or enrichment is owed or no
    /// resolver can prove them. Read only because the focal may escape.
    pub callers_unread: u64,
    /// Those sites by why each is kept.
    pub by_reason: BTreeMap<&'static str, u64>,
}

impl ValueCalls {
    /// Whether nothing was kept this way.
    pub fn is_empty(&self) -> bool {
        self.sites == 0 && self.callers_unread == 0
    }
}

/// Whether a reading promoted named sites to `name_unique` reference rows, and
/// why not when it did not.
///
/// A promoted site is never proven. It is a site whose callee spells the
/// focal's one call name while the store holds exactly one definition that
/// name can reach, the focal itself, and nothing the resolver settled says the
/// name goes anywhere else. It is served as a reference row labelled
/// `name_unique` and counted apart from proven rows, and it never licenses an
/// absence or a dead-code claim.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum NameUnique {
    /// The rule held and every unresolved or server-failed site whose callee
    /// spells `name` was promoted.
    Promoted { name: String },
    /// The rule did not hold, for `reason`.
    NotApplied { reason: &'static str },
}

/// The focal is not a function or a method with one call name.
const UNIQUE_NOT_ONE_CALL_NAME: &str = "the focal is not a function or method with one call name";
/// Another definition in the calling languages carries the call name.
const UNIQUE_SHARED_DEFINITION: &str =
    "another definition in the calling languages carries the call name";
/// A symbol outside the repository carries the call name.
const UNIQUE_EXTERNAL_SYMBOL: &str = "a symbol outside the repository carries the call name";
/// A settled site that spells the call name was proven to call something else.
const UNIQUE_CONTESTED: &str =
    "a settled call site that spells the call name was proven to call something else";
/// A caller holding settled sites that could spell the call name was not read.
const UNIQUE_CONTEST_UNREAD: &str =
    "a caller whose settled sites could spell the call name could not be read";
/// No unresolved or server-failed site spells the call name.
const UNIQUE_NOTHING_TO_PROMOTE: &str =
    "no unresolved or server-failed call site spells the call name";
/// The graph could not be read to establish the rule.
const UNIQUE_UNREADABLE: &str = "the entity index could not be read to establish the rule";

/// Entity kinds that can never be reached by a call spelling their name, so
/// sharing it does not contest a promotion: type-level declarations and file
/// structure.
const NEVER_CALLED_KINDS: [kin_model::EntityKind; 9] = [
    kin_model::EntityKind::Interface,
    kin_model::EntityKind::TraitDef,
    kin_model::EntityKind::TypeAlias,
    kin_model::EntityKind::Module,
    kin_model::EntityKind::Package,
    kin_model::EntityKind::File,
    kin_model::EntityKind::DocumentNode,
    kin_model::EntityKind::Schema,
    kin_model::EntityKind::EnumDef,
];

/// The last segment of a graph name, the member a call spells.
fn leaf_name(name: &str) -> &str {
    name.rsplit(['.', ':']).next().unwrap_or(name)
}

/// The one call name a named site could be promoted under, or why promotion
/// does not apply: the focal must be a function or a non-constructor method
/// with one call name, the store's calling languages must hold no other
/// definition that name could reach (a callable, a class or a value that may
/// hold one, and never a body-less declaration such as an interface member or
/// an overload signature), and no symbol outside the repository may carry it.
/// Whether a settled site contests the name is read during the scan.
fn unique_call_name<G: GraphStore + ?Sized>(
    store: &G,
    focal: &Entity,
    names: &[String],
) -> Result<String, &'static str> {
    let [name] = names else {
        return Err(UNIQUE_NOT_ONE_CALL_NAME);
    };
    if !matches!(
        focal.kind,
        kin_model::EntityKind::Function | kin_model::EntityKind::Method
    ) || name.is_empty()
    {
        return Err(UNIQUE_NOT_ONE_CALL_NAME);
    }
    let languages = calling_languages(focal.language);
    let mut others = HashSet::new();
    for pattern in [name.clone(), format!("*.{name}"), format!("*::{name}")] {
        let found = store
            .query_entities(&kin_model::graph::EntityFilter {
                name_pattern: Some(pattern),
                languages: Some(languages.clone()),
                ..Default::default()
            })
            .map_err(|_| UNIQUE_UNREADABLE)?;
        for entity in found {
            if entity.id == focal.id
                || leaf_name(&entity.name) != name
                || NEVER_CALLED_KINDS.contains(&entity.kind)
                || !kin_core::disambiguation::carries_body(&entity)
            {
                continue;
            }
            others.insert(entity.id);
        }
    }
    if !others.is_empty() {
        return Err(UNIQUE_SHARED_DEFINITION);
    }
    let externals = store.external_references().map_err(|_| UNIQUE_UNREADABLE)?;
    if externals.into_iter().any(|reference| {
        let node = crate::handlers::external_symbols::ExternalSymbolNode {
            id: reference.id,
            reference,
        };
        // A SCIP descriptor ends its member in `#name().` or `/name().`, and
        // a display name in `Type.name`: the leaf is what follows the last
        // separator of either form.
        let display = node.display_name();
        display
            .trim_end_matches(['(', ')', '.', '#'])
            .rsplit(['.', ':', '#', '/', ' '])
            .next()
            == Some(name.as_str())
    }) {
        return Err(UNIQUE_EXTERNAL_SYMBOL);
    }
    Ok(name.clone())
}

/// The ledger keys of the sites of `caller` that sit at `spans`, the call
/// expressions (or callee tokens) its proven edges to a focal carry, read
/// against `body`, the caller's exact text. A span outside the caller's own
/// text, or one whose callee is itself an expression, names no site.
fn edge_site_keys(caller: &Entity, body: &str, spans: &[SourceSpan]) -> HashSet<(u32, u32)> {
    let Some(own) = caller.span.as_ref() else {
        return HashSet::new();
    };
    let syntax = site_expression::syntax_of(caller);
    spans
        .iter()
        .filter(|span| span.file == own.file)
        .filter_map(|span| {
            let start = span.start_byte.checked_sub(own.start_byte)?;
            let end = span.end_byte.checked_sub(own.start_byte)?;
            let (from, to) = site_expression::callee_token(body, start, end, &syntax)?;
            Some((u32::try_from(from).ok()?, u32::try_from(to - from).ok()?))
        })
        .collect()
}

/// Whether a settled state proves the call reaches `focal` itself.
fn targets_focal(state: &kin_model::CallSiteState, focal: EntityId) -> bool {
    match state {
        kin_model::CallSiteState::ProvenTarget { target } => *target == focal,
        kin_model::CallSiteState::ProvenDeclaration { declaration, .. } => *declaration == focal,
        _ => false,
    }
}

/// Whether a settled site in `body` spells `name` and was proven to go
/// somewhere other than `focal`.
fn contests_name(ledger: &CallSiteLedger, body: &str, name: &str, focal: EntityId) -> bool {
    ledger.sites.iter().any(|site| {
        SiteStateKind::of(&site.state).is_settled()
            && !targets_focal(&site.state, focal)
            && kin_model::site_text(site, body) == Some(name)
    })
}

/// What a store-wide reading of one focal's possible callers found.
#[derive(Debug, Clone)]
pub struct FocalScan {
    /// Whether callers with the current stored `Test` role were included.
    pub include_tests: bool,
    /// The focal the reading was taken for.
    pub focal: EntityId,
    /// The names a call to the focal is spelled with.
    pub names: Vec<String>,
    /// Whether the focal may be held as a value.
    pub escape: FocalEscape,
    /// Every unsettled site that could be a call to the focal, the ones tied
    /// to it by name first, then in caller file, caller name and site order.
    pub candidates: Vec<CandidateSite>,
    /// The named leftovers: every site of `candidates` whose reason ties it to
    /// the focal by name ([`kin_model::reach_is_named`]) and that was not
    /// promoted. Never sampled.
    pub named: Vec<CandidateSite>,
    /// Named sites served as `name_unique` reference rows (see [`NameUnique`]).
    pub promoted: Vec<CandidateSite>,
    /// Whether named sites were promoted, and why not when they were not.
    pub name_unique: NameUnique,
    /// The sites kept only because the focal may be held as a value, counted.
    pub value_calls: ValueCalls,
    /// Every caller whose body could call the focal by name, or through a
    /// value when it escapes, or that a proven site calls it from, each read
    /// through the one site-state reading and narrowed.
    pub callers: Vec<ScannedCaller>,
    /// Callers some proven site calls the focal from.
    pub proven_callers: HashSet<EntityId>,
    /// Test-role callers a reading that excludes tests left out: each one
    /// that calls the focal by a proven site, or whose body could spell one
    /// of its call names. Their sites were not read, so this counts callers,
    /// never sites, and it is empty for a reading that includes tests.
    pub excluded_test_callers: Vec<EntityId>,
    /// Settled sites a resolver proved call the focal itself.
    pub settled_to_focal: u64,
    /// Sites the ledger leaves open that a proven `Calls` edge from their
    /// caller to the focal carries: the call is that edge's, so for this
    /// focal it is answered and is no leftover. The ledger settles a site
    /// only where one target is proven and every other is ruled out, which a
    /// tier that names one of an open set never does: a call of a name
    /// imported from a module fork stub reaches the declaration each variant
    /// supplies, and each of those edges carries the site.
    pub proven_by_edge: u64,
    /// Whether a language may run the focal without a call spelling its name
    /// (see [`kin_model::member_may_dispatch_without_its_name`]), so no
    /// reading of the sites that spell it is the whole set of its callers.
    pub implicit_dispatch: bool,
    /// When [`Self::implicit_dispatch`] holds only because the method is
    /// declared under decorators, those decorators, less Python's
    /// name-preserving ones (see [`kin_model::member_dispatch_decorators`]):
    /// the method can then run without a call that spells its name only
    /// through what one of them does with it, so a reference answer lists them
    /// as its residue rather than leaving its callers unaccounted for. `None`
    /// while any other dispatch could run it. The focal's display name rides
    /// along so the listing can say what each decorator is on.
    pub dispatch_decorators: Option<(String, Vec<String>)>,
    /// Whether [`Self::implicit_dispatch`] holds only because the focal is a
    /// constructor (see [`kin_model::member_dispatches_by_construction`]), so
    /// a reference answer can name the constructions no call name shows
    /// rather than the bare method gate.
    pub construction_dispatch: bool,
    /// The line each listed residue site sits on, as written, keyed by file
    /// and 1-based line (see [`site_expression::residue_lines`]).
    pub residue_lines: BTreeMap<(String, u32), site_expression::ResidueLine>,
    kept: HashSet<(EntityId, u32, u32)>,
    named_kept: HashSet<(EntityId, u32, u32)>,
    judged: HashSet<EntityId>,
}

impl FocalScan {
    /// Confine the reading to the callers `keep` admits, for a question
    /// about one part of the repository, and return how many callers it
    /// left out.
    ///
    /// A caller left out holds no site in what remains: its named
    /// leftovers, the rows the name-unique rule would promote from it and
    /// its own reading are gone, so the clauses the reading calls for speak
    /// of the callers kept. An unresolved site in a caller left out could
    /// add that caller alone, and it is not one the question asked about.
    /// What the reading counts without a caller, the value calls and the
    /// census of where the focal is held as a value, stays as it was, so
    /// the confined reading is never stronger than the evidence for it.
    pub fn confine(&mut self, keep: impl Fn(&Entity) -> bool) -> usize {
        let outside: HashSet<EntityId> = self
            .callers
            .iter()
            .filter(|caller| !keep(&caller.entity))
            .map(|caller| caller.entity.id)
            .collect();
        if outside.is_empty() {
            return 0;
        }
        self.callers
            .retain(|caller| !outside.contains(&caller.entity.id));
        for sites in [&mut self.candidates, &mut self.named, &mut self.promoted] {
            sites.retain(|site| !outside.contains(&site.caller));
        }
        self.proven_callers
            .retain(|caller| !outside.contains(caller));
        self.kept.retain(|(caller, _, _)| !outside.contains(caller));
        self.named_kept
            .retain(|(caller, _, _)| !outside.contains(caller));
        outside.len()
    }

    /// Whether `site` of `caller` could be a call to the focal. A caller the
    /// scan ruled out by its whole body holds no such site.
    pub fn keeps(&self, caller: EntityId, site: &CallSite) -> bool {
        self.judged.contains(&caller) && self.kept.contains(&(caller, site.offset, site.length))
    }

    /// Whether the scan read `caller`.
    pub fn read(&self, caller: EntityId) -> bool {
        self.judged.contains(&caller)
    }

    /// The tally over the named scope: every caller that could spell a call
    /// name of the focal, narrowed to the settled sites that call the focal
    /// and the unsettled sites tied to it by name. Settled sites that go
    /// elsewhere are not the focal's call sites and are not counted, and
    /// neither are the value calls, which [`Self::value_calls`] counts.
    pub fn tally(&self) -> CallSiteTally {
        let mut tally = CallSiteTally::default();
        for caller in self.callers.iter().filter(|caller| caller.named) {
            let id = caller.entity.id;
            tally.add_sites(&caller.reading, |site| {
                self.named_kept.contains(&(id, site.offset, site.length))
                    || targets_focal(&site.state, self.focal)
            });
        }
        tally
    }

    /// Whether every site that could call the focal by name is either proven
    /// or listed: no caller in the named scope is owed its sites, stale,
    /// unverified or without a resolver.
    pub fn named_complete(&self) -> bool {
        let tally = self.tally();
        tally.callers_owed() == 0
            && tally.callers_unproven_no_resolver == 0
            && tally.callers_stale == 0
            && tally.callers_unverified == 0
    }

    /// Callers read that no current ledger describes.
    pub fn callers_without_current_ledger(&self) -> usize {
        self.callers
            .iter()
            .filter(|caller| {
                !matches!(
                    caller.reading,
                    CallerSites::Current(_) | CallerSites::NoSites
                )
            })
            .count()
    }

    /// Each caller holding a promoted site, with its promoted sites, in the
    /// order the sites sort.
    pub fn promoted_callers(&self) -> Vec<(&Entity, Vec<&CandidateSite>)> {
        let entities: HashMap<EntityId, &Entity> = self
            .callers
            .iter()
            .map(|caller| (caller.entity.id, &caller.entity))
            .collect();
        let mut grouped: Vec<(&Entity, Vec<&CandidateSite>)> = Vec::new();
        for site in &self.promoted {
            let Some(entity) = entities.get(&site.caller) else {
                continue;
            };
            match grouped
                .iter_mut()
                .find(|(caller, _)| caller.id == site.caller)
            {
                Some((_, sites)) => sites.push(site),
                None => grouped.push((entity, vec![site])),
            }
        }
        grouped
    }
}

/// Caller bodies a store-wide reading reads to judge sites while the focal
/// is contained, past which a site keeps whatever text would have ruled out.
pub const SCAN_TEXT_READS_MAX: usize = 500;

/// The census answer of a reading that took no census: unknown, which every
/// rule reads as escaping.
pub const NO_CENSUS: FocalEscape = FocalEscape::Unknown {
    reason: "no escape census was taken for this reading",
};

/// Read every caller in the store that could be a call to `focal`, through
/// the one site-state reading, and keep each unsettled site the rule of
/// [`kin_model::site_could_call_at`] keeps under `escape`, the census answer
/// for the focal (see
/// [`crate::handlers::common::HeldSourceAuthority::escape_evidence_batch`]).
///
/// The kept sites split in two. A site whose callee or caller body spells a
/// call name of the focal, or whose text was not read in a caller that could
/// spell one, is a named leftover, and every one is listed. A site kept only
/// because the focal may be held as a value, or reached through a name built
/// at run time, is a value call, counted once beside the census reason. When
/// the focal's one call name reaches exactly one definition in the store, the
/// focal, a named unresolved site is promoted to a `name_unique` row instead
/// (see [`NameUnique`]).
///
/// While the focal may escape, every unsettled site of every caller in the
/// calling languages is kept and no text is needed to decide; the text of a
/// caller that could spell a call name is still read, so its named sites are
/// told apart from its value calls. While it is contained, a caller whose
/// whole preview spells none of its call names holds no site that could call
/// it (the census saw no dynamic access in the domain), and the rest are
/// judged by their exact text, read through `text`. A reader that holds no
/// text keeps every site text would have ruled out.
pub fn scan_focal<G: GraphStore + ?Sized, T: SiteText + ?Sized>(
    store: &G,
    focal: &Entity,
    text: &T,
    escape: FocalEscape,
) -> crate::error::Result<FocalScan> {
    scan_focal_scoped(store, focal, text, escape, true)
}

/// [`scan_focal`] over the requested caller roles. Definition lookup and the
/// escape census retain their full binding domain; only caller contributions
/// are excluded, before sites, debt, candidates and tallies are collected.
pub fn scan_focal_scoped<G: GraphStore + ?Sized, T: SiteText + ?Sized>(
    store: &G,
    focal: &Entity,
    text: &T,
    escape: FocalEscape,
    include_tests: bool,
) -> crate::error::Result<FocalScan> {
    // The census's alias names are call names too, and what it lists is
    // served beside the rows, so only an escape it could not list widens the
    // scan to every unsettled site.
    let names = escape.call_names(focal);
    let escapes = escape.may_escape_unlisted(&names);
    let calls_in: Vec<kin_model::Relation> = store
        .get_all_relations_for_entity(&focal.id)
        .map_err(crate::error::McpError::graph)?
        .into_iter()
        .filter(|relation| {
            relation.kind == kin_model::RelationKind::Calls
                && relation.dst.as_entity() == Some(focal.id)
        })
        .collect();
    let mut proven_callers: HashSet<EntityId> = calls_in
        .iter()
        .filter_map(|relation| relation.src.as_entity())
        .collect();
    // Where each caller's proven edges to the focal say the call is written.
    let mut proven_calls: HashMap<EntityId, Vec<SourceSpan>> = HashMap::new();
    for relation in &calls_in {
        if let Some(caller) = relation.src.as_entity() {
            let (sites, _) = kin_index::occurrence::proven_sites(relation);
            if !sites.is_empty() {
                proven_calls.entry(caller).or_default().extend(sites);
            }
        }
    }
    let entities = store
        .query_entities(&kin_model::graph::EntityFilter {
            languages: Some(calling_languages(focal.language)),
            ..Default::default()
        })
        .map_err(crate::error::McpError::graph)?;
    let mut excluded_test_callers: Vec<EntityId> = Vec::new();
    if !include_tests {
        proven_callers.retain(|id| {
            let excluded = matches!(
                store.get_entity(id),
                Ok(Some(entity)) if !caller_in_scope(&entity, false)
            );
            if excluded {
                excluded_test_callers.push(*id);
            }
            !excluded
        });
    }
    let proven_in_tests: HashSet<EntityId> = excluded_test_callers.iter().copied().collect();
    let facts = GraphSiteFacts::new(store);
    let mut scan = FocalScan {
        include_tests,
        focal: focal.id,
        names,
        escape,
        candidates: Vec::new(),
        named: Vec::new(),
        promoted: Vec::new(),
        name_unique: NameUnique::NotApplied {
            reason: UNIQUE_NOT_ONE_CALL_NAME,
        },
        value_calls: ValueCalls::default(),
        callers: Vec::new(),
        proven_callers,
        excluded_test_callers,
        settled_to_focal: 0,
        proven_by_edge: 0,
        implicit_dispatch: focal.kind == kin_model::EntityKind::Method
            && kin_model::member_may_dispatch_without_its_name(focal),
        dispatch_decorators: (focal.kind == kin_model::EntityKind::Method
            && kin_model::member_may_dispatch_without_its_name(focal))
        .then(|| kin_model::member_dispatch_decorators(focal))
        .flatten()
        .map(|decorators| (focal.name.clone(), decorators)),
        construction_dispatch: kin_model::member_dispatches_by_construction(focal),
        residue_lines: BTreeMap::new(),
        kept: HashSet::new(),
        named_kept: HashSet::new(),
        judged: HashSet::new(),
    };
    if matches!(scan.escape, FocalEscape::NonCallable { .. }) {
        // Proven reference edges remain in the answer. This proof only rules
        // unproven call sites out of the focal's call domain.
        return Ok(scan);
    }
    // The names a receiver of the focal's owner is spelled with, for reading
    // what each named leftover's receiver says.
    let owner = FocalOwner::of(focal);
    // The name a named site could be promoted under, decided before the walk
    // so the walk reads the settled sites that could contest it only when the
    // rest of the rule already holds.
    let mut unique = unique_call_name(store, focal, &scan.names);
    // Callers whose settled sites could contest the name and whose text the
    // walk did not otherwise need, read after it so the named leftovers are
    // addressed first.
    let mut contest_pending: Vec<usize> = Vec::new();
    // Excluded callers contribute no sites or counts, but a settled binding
    // can still refute the global name-uniqueness rule. Read that evidence
    // after in-scope candidates so it cannot consume their text budget first.
    let mut excluded_contests: Vec<(Entity, CallSiteLedger)> = Vec::new();
    // Each kept site with its caller's index in `scan.callers` and its key,
    // so rows sort by site and the served ones can be addressed after.
    let mut kept_sites: Vec<(CandidateSite, usize, (u32, u32))> = Vec::new();
    for entity in entities {
        let Some(span) = entity
            .span
            .as_ref()
            .filter(|span| span.start_byte < span.end_byte)
        else {
            continue;
        };
        let could_name = could_name_focal(&entity, &scan.names);
        if !caller_in_scope(&entity, include_tests) {
            // Counted only where the test's recorded text spells a call
            // name. One with no text on record, or text cut short, could
            // spell it too, and is not counted for something nobody read.
            if could_name
                && !proven_in_tests.contains(&entity.id)
                && spells_focal_name(&entity, &scan.names)
            {
                scan.excluded_test_callers.push(entity.id);
            }
            if unique.is_ok() && could_name {
                if let CallerSites::Current(ledger) = read_caller_sites(&facts, &entity) {
                    if ledger.sites.iter().any(|site| {
                        SiteStateKind::of(&site.state).is_settled()
                            && !targets_focal(&site.state, scan.focal)
                    }) {
                        excluded_contests.push((entity, ledger));
                    }
                }
            }
            continue;
        }
        let file = span.file.0.clone();
        let named_caller = could_name || scan.proven_callers.contains(&entity.id);
        if !escapes && !named_caller {
            continue;
        }
        // Read once per caller, and only when one of its sites is kept.
        let caller_module = std::cell::OnceCell::new();
        let reading = read_caller_sites(&facts, &entity);
        scan.judged.insert(entity.id);
        let Some(ledger) = reading.ledger() else {
            if !named_caller && !matches!(reading, CallerSites::NoSites) {
                scan.value_calls.callers_unread += 1;
            }
            scan.callers.push(ScannedCaller {
                entity,
                reading,
                named: named_caller,
            });
            continue;
        };
        let recorded_only = !matches!(reading, CallerSites::Current(_));
        if !recorded_only {
            scan.settled_to_focal += ledger
                .sites
                .iter()
                .filter(|site| targets_focal(&site.state, scan.focal))
                .count() as u64;
        }
        // Whether this caller's settled sites could say the call name goes
        // somewhere other than the focal, which only matters while the rest
        // of the promotion rule holds.
        let could_contest = unique.is_ok()
            && could_name
            && !recorded_only
            && ledger.sites.iter().any(|site| {
                SiteStateKind::of(&site.state).is_settled()
                    && !targets_focal(&site.state, scan.focal)
            });
        // While the focal is contained, text decides which sites could call
        // it. While it may escape, every unsettled site stays whatever the
        // text says, and the text of a caller that could spell a call name is
        // still read so a site whose callee spells it is told apart from one
        // kept only because the focal may be held as a value. That is what
        // splits the named leftovers from the value calls.
        let body = if (!escapes || could_name)
            && ledger
                .sites
                .iter()
                .any(|site| recorded_only || !SiteStateKind::of(&site.state).is_settled())
        {
            exact_text(text, &entity)
        } else {
            None
        };
        if could_contest {
            match (body.as_deref(), &unique) {
                (Some(body), Ok(name)) => {
                    if contests_name(ledger, body, name, scan.focal) {
                        unique = Err(UNIQUE_CONTESTED);
                    }
                }
                _ => contest_pending.push(scan.callers.len()),
            }
        }
        let spelling = body.as_deref().or_else(|| whole_body_preview(&entity));
        // The sites of this caller a proven edge to the focal carries.
        let proven_here = match (body.as_deref(), proven_calls.get(&entity.id)) {
            (Some(body), Some(spans)) => edge_site_keys(&entity, body, spans),
            _ => HashSet::new(),
        };
        let mut keys = Vec::new();
        let mut value_sites_here = 0u64;
        for site in &ledger.sites {
            // A ledger under another context or none recorded its states
            // under a proof that may not hold, so a site it recorded as
            // settled is judged as a site with no answer, by its text.
            let judged = if recorded_only && SiteStateKind::of(&site.state).is_settled() {
                CallSite {
                    state: kin_model::CallSiteState::Unresolved {
                        reason: kin_model::UnresolvedReason::NoAnswer,
                    },
                    ..site.clone()
                }
            } else {
                site.clone()
            };
            // A structural caller may have readable site evidence without
            // a body. That evidence can distinguish its callee but cannot
            // stand in for complete caller text or receiver/escape analysis.
            let site_text = if body.is_none()
                && (!escapes || could_name)
                && !SiteStateKind::of(&judged.state).is_settled()
            {
                exact_site_text(text, &entity, site)
            } else {
                None
            };
            let at = body
                .as_deref()
                .and_then(|body| kin_model::site_text(site, body))
                .or(site_text.as_deref());
            // An open site a proven edge to the focal carries is that edge's
            // call: answered for this focal, whatever else it may reach.
            if !SiteStateKind::of(&judged.state).is_settled() && proven_here.contains(&site.key()) {
                scan.proven_by_edge += 1;
                continue;
            }
            let Some(mut why) =
                kin_model::site_could_call_at(&judged, at, &scan.names, spelling, escapes)
            else {
                continue;
            };
            // A call through a lookup by a computed name is the census's to
            // judge: it read every such lookup in the domain against the focal
            // and lists each one that could fetch it, so the call is not kept
            // a second time as dynamic access. A site whose text was not read
            // could be a literal lookup that spells a name, and stays.
            if why == kin_model::call_site_reading::REACH_DYNAMIC_ACCESS
                && at.is_some()
                && scan.escape.judged_dynamic_access()
            {
                continue;
            }
            // A caller whose body could spell a call name but whose site was
            // not read (past the read bound) may hold a named site; saying it
            // is kept only because the focal escapes would be a guess.
            if escapes
                && could_name
                && at.is_none()
                && why == kin_model::call_site_reading::REACH_FOCAL_ESCAPES
            {
                why = kin_model::call_site_reading::REACH_TEXT_UNKNOWN;
            }
            keys.push(site.key());
            if kin_model::reach_is_named(why) {
                scan.named_kept
                    .insert((entity.id, site.offset, site.length));
            } else {
                value_sites_here += 1;
                scan.value_calls.sites += 1;
                *scan.value_calls.by_reason.entry(why).or_default() += 1;
            }
            let mut row = CandidateSite {
                caller: entity.id,
                caller_name: entity.name.clone(),
                caller_file: Some(file.clone()),
                caller_module: caller_module
                    .get_or_init(|| crate::semantic_owners::entity_module(store, &entity))
                    .clone(),
                line_in_entity: body.as_deref().and_then(|body| line_in(body, site)),
                callee: at.filter(|text| is_identifier(text)).map(str::to_string),
                state: reading.site_kind(site),
                state_reason: site_state_reason(&site.state),
                reason: why,
                expr: None,
                receiver: None,
                receiver_hint: None,
                expr_unavailable: None,
                receiver_binding_missing: None,
                receiver_import: None,
                bare_call: false,
            };
            if kin_model::reach_is_named(why) {
                if let Some(body) = body.as_deref() {
                    address_expression(&mut row, &entity, body, site.key(), &owner);
                }
            }
            kept_sites.push((row, scan.callers.len(), site.key()));
        }
        if value_sites_here > 0 {
            scan.value_calls.callers += 1;
        }
        let kept: HashSet<(u32, u32)> = keys.iter().copied().collect();
        for (offset, length) in keys {
            scan.kept.insert((entity.id, offset, length));
        }
        let narrowed = reading.retain_sites(|site| {
            (!recorded_only && SiteStateKind::of(&site.state).is_settled())
                || kept.contains(&site.key())
        });
        scan.callers.push(ScannedCaller {
            entity,
            reading: narrowed,
            named: named_caller,
        });
    }
    kept_sites.sort_by(|(left, _, left_key), (right, _, right_key)| {
        (
            reach_rank(left.reason),
            &left.caller_file,
            &left.caller_name,
            left.caller,
            left_key,
        )
            .cmp(&(
                reach_rank(right.reason),
                &right.caller_file,
                &right.caller_name,
                right.caller,
                right_key,
            ))
    });
    // Every named row is served, so every one is addressed inside its caller,
    // with the call as written, even when no text was needed to keep it. Each
    // caller's body is read once however many of its sites are listed; value
    // calls are only counted.
    let mut bodies: HashMap<usize, Result<String, &'static str>> = HashMap::new();
    for (row, caller, (offset, length)) in kept_sites
        .iter_mut()
        .filter(|(row, _, _)| kin_model::reach_is_named(row.reason))
    {
        if row.expr.is_some() || row.expr_unavailable.is_some() {
            continue;
        }
        let entity = &scan.callers[*caller].entity;
        let body = bodies
            .entry(*caller)
            .or_insert_with(|| site_expression::whole_text(text, entity));
        match body {
            Ok(body) => {
                let site = CallSite {
                    offset: *offset,
                    length: *length,
                    state: kin_model::CallSiteState::ProvenOutside,
                };
                if row.line_in_entity.is_none() {
                    row.line_in_entity = line_in(body, &site);
                    row.callee = kin_model::site_text(&site, body)
                        .filter(|text| is_identifier(text))
                        .map(str::to_string);
                }
                address_expression(row, entity, body, (*offset, *length), &owner);
            }
            Err(reason) => row.expr_unavailable = Some(*reason),
        }
    }
    // A named leftover made through a receiver says which link of the chain
    // typing it is missing, so a reader knows what the graph could not prove
    // before reading the caller. One made on a name its file imports says
    // which module the import names, and whether the repository holds it.
    let census = kin_db::call_site_escape::StoreCensus { store };
    let mut imports = kin_db::call_site_escape::ReceiverImports::new(&census);
    for (row, caller, _) in kept_sites
        .iter_mut()
        .filter(|(row, _, _)| kin_model::reach_is_named(row.reason))
    {
        let (Some(receiver), Some(method)) = (row.receiver.as_deref(), row.callee.as_deref())
        else {
            continue;
        };
        let entity = &scan.callers[*caller].entity;
        if let Some((_, missing)) =
            crate::receiver_binding::chain_gap(store, entity, receiver, method)
        {
            row.receiver_binding_missing = Some(crate::receiver_binding::missing_brief(&missing));
        }
        let line = entity
            .span
            .as_ref()
            .zip(row.line_in_entity)
            .and_then(|(span, line)| u32::try_from(line).ok()?.checked_add(span.start_line));
        if let Some(import) = entity
            .file_origin
            .as_ref()
            .zip(line)
            .and_then(|(path, line)| imports.of(path, receiver, line))
        {
            row.receiver_import = Some(json!({
                "module": import.module,
                "outside_repository": import.outside_repository,
            }));
        }
    }
    scan.candidates = kept_sites.into_iter().map(|(row, _, _)| row).collect();
    // Promotion: the rest of the rule held during the walk, so the settled
    // sites of callers whose text it did not need are read now, and any that
    // spells the name and goes elsewhere, or that cannot be read, stops it.
    // A call made on a name its file imports from outside the repository
    // runs that module's own member, so it is never read as a call to a
    // module-level focal that merely shares the name. A method keeps it: an
    // outside module can hand out a value of a class declared here.
    let on_outside_import = |site: &CandidateSite| {
        !owner.is_member()
            && site
                .receiver_import
                .as_ref()
                .and_then(|import| import.get("outside_repository"))
                .and_then(Value::as_bool)
                == Some(true)
    };
    let promotable = |site: &CandidateSite| {
        site.reason == kin_model::call_site_reading::REACH_CALLEE_SPELLS
            && matches!(
                site.state,
                SiteStateKind::Unresolved | SiteStateKind::ServerFailed
            )
            && !on_outside_import(site)
    };
    if unique.is_ok() && !scan.candidates.iter().any(promotable) {
        unique = Err(UNIQUE_NOTHING_TO_PROMOTE);
    }
    if unique.is_ok() {
        for caller in contest_pending {
            let Ok(name) = &unique else {
                break;
            };
            let entity = &scan.callers[caller].entity;
            let Some(ledger) = scan.callers[caller].reading.ledger() else {
                continue;
            };
            match exact_text(text, entity) {
                Some(body) if contests_name(ledger, &body, name, scan.focal) => {
                    unique = Err(UNIQUE_CONTESTED);
                }
                Some(_) => {}
                None => unique = Err(UNIQUE_CONTEST_UNREAD),
            }
        }
    }
    if unique.is_ok() {
        for (entity, ledger) in excluded_contests {
            let Ok(name) = &unique else {
                break;
            };
            match exact_text(text, &entity) {
                Some(body) if contests_name(&ledger, &body, name, scan.focal) => {
                    unique = Err(UNIQUE_CONTESTED);
                }
                Some(_) => {}
                None => unique = Err(UNIQUE_CONTEST_UNREAD),
            }
        }
    }
    scan.name_unique = match unique {
        Ok(name) => NameUnique::Promoted { name },
        Err(reason) => NameUnique::NotApplied { reason },
    };
    let promote = matches!(scan.name_unique, NameUnique::Promoted { .. });
    for candidate in &scan.candidates {
        if !kin_model::reach_is_named(candidate.reason) {
            continue;
        }
        if promote && promotable(candidate) {
            scan.promoted.push(candidate.clone());
        } else {
            scan.named.push(candidate.clone());
        }
    }
    // Sites whose receiver can hold the focal's owner first, then the ones
    // nothing says about, then literals and builtins, each group in the order
    // above. None is dropped.
    scan.named
        .sort_by_key(|row| ReceiverHint::rank_of(row.receiver_hint));
    scan.residue_lines =
        site_expression::residue_lines(store, text, &scan.escape, CALL_SITE_CANDIDATE_SAMPLE_MAX);
    Ok(scan)
}

/// How strongly a kept site's reason ties it to the focal, strongest first:
/// its callee spells a call name, then its caller's body does, then its text
/// was not read in a caller that could spell one, then it could reach the
/// focal only through access the text cannot name, and last it is kept only
/// because the focal may be held as a value. Rows sort by this before file,
/// so the named leftovers come before the value calls.
pub fn reach_rank(reason: &str) -> u8 {
    match reason {
        kin_model::call_site_reading::REACH_CALLEE_SPELLS => 0,
        kin_model::call_site_reading::REACH_BODY_SPELLS => 1,
        kin_model::call_site_reading::REACH_FOCAL_ESCAPES => 4,
        reason if kin_model::reach_is_named(reason) => 2,
        _ => 3,
    }
}

/// The line of `site` inside a caller whose exact text is `body`, counted
/// from 0 at its first line.
fn line_in(body: &str, site: &CallSite) -> Option<u64> {
    body.get(..site.offset as usize)
        .map(|before| before.matches('\n').count() as u64)
}

/// Every unsettled site in the store that could be a call to `focal`, read
/// from ledgers alone, with no text reader and no census: the focal reads as
/// escaping, so every unsettled site of the calling languages is kept.
/// [`unsettled_sites_naming_with`] is the same reading with the graph-held
/// text and the census an answer holds.
pub fn unsettled_sites_naming<G: GraphStore + ?Sized>(
    store: &G,
    focal: &Entity,
) -> crate::error::Result<Vec<CandidateSite>> {
    unsettled_sites_naming_with(store, focal, &NoSiteText, NO_CENSUS)
}

/// [`unsettled_sites_naming`] with `text`, the reader of callers' graph-held
/// text the answer already holds, under `escape`, its census for the focal.
pub fn unsettled_sites_naming_with<G: GraphStore + ?Sized, T: SiteText + ?Sized>(
    store: &G,
    focal: &Entity,
    text: &T,
    escape: FocalEscape,
) -> crate::error::Result<Vec<CandidateSite>> {
    Ok(scan_focal(store, focal, text, escape)?.candidates)
}

/// Why the store's ledgers alone do not settle who calls a focal.
pub const UNSETTLED_CANDIDATE_SITES: &str = "unsettled_candidate_sites";
/// A ledger in scope was not proven under the selected graph's validated
/// context for its language.
pub const LEDGER_NOT_UNDER_CURRENT_CONTEXT: &str = "ledger_not_under_current_context";
/// A proven site in scope names an entity the graph no longer holds.
pub const PROVEN_TARGET_NOT_LIVE: &str = "proven_target_not_live";
/// A file in scope holds call expressions no ledger attributes to a caller.
pub const UNATTRIBUTED_EXPRESSIONS: &str = "unattributed_expressions";
/// The selected graph holds no validated proof context for the focal's
/// language, or for a caller's.
pub const NO_VALIDATED_CONTEXT: &str = "no_validated_context";
/// A caller whose body could call the focal has no ledger at all: its
/// derivation or enrichment is owed, or no resolver can prove its sites.
pub const CALLERS_WITHOUT_LEDGER: &str = "callers_without_ledger";

/// Whether the store's call-site ledgers alone settle who calls a focal.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CallsEvidence {
    /// True only when every condition below holds.
    pub settled: bool,
    /// Store-wide unsettled sites that could be calls to the focal.
    pub candidates: Vec<CandidateSite>,
    /// Why it is not settled, each once, in this order:
    /// [`UNSETTLED_CANDIDATE_SITES`], [`ESCAPE_RESIDUE_LISTED`], [`UNKEYED_LOOKUPS_COUNTED`],
    /// [`CALLERS_WITHOUT_LEDGER`],
    /// [`NO_VALIDATED_CONTEXT`], [`LEDGER_NOT_UNDER_CURRENT_CONTEXT`],
    /// [`PROVEN_TARGET_NOT_LIVE`], [`UNATTRIBUTED_EXPRESSIONS`].
    pub unsettled_because: Vec<&'static str>,
    /// Callers whose ledgers were read.
    pub callers_read: usize,
    /// Whether the focal may be held as a value, which widens which sites
    /// could call it.
    pub escape: FocalEscape,
}

/// Whether the store's call-site ledgers alone settle who calls `focal`,
/// with no text reader (see [`unsettled_sites_naming`]).
pub fn calls_evidence_for<G: GraphStore + ?Sized>(
    store: &G,
    focal: &Entity,
) -> crate::error::Result<CallsEvidence> {
    calls_evidence_for_scoped(store, focal, true)
}

/// [`calls_evidence_for`] over the requested caller roles, preserving unknown
/// and unowned file inventory as a gap even when test callers are excluded.
pub fn calls_evidence_for_scoped<G: GraphStore + ?Sized>(
    store: &G,
    focal: &Entity,
    include_tests: bool,
) -> crate::error::Result<CallsEvidence> {
    let scan = scan_focal_scoped(store, focal, &NoSiteText, NO_CENSUS, include_tests)?;
    Ok(calls_evidence_from(store, focal, &scan))
}

/// [`calls_evidence_for`] with `text`, the reader of callers' graph-held
/// text the answer already holds, under `escape`, its census for the focal.
///
/// Candidate rows are narrowed by which sites could call the focal. The
/// replacement proof separately audits every entity and file in the potential
/// calling-language domain, including empty ledgers and files with no candidate
/// rows. Never the import family, and never an empty tally read as settled. It
/// is settled only when:
///
/// 1. no unsettled site anywhere could be a call to the focal;
/// 2. no caller that could call it lacks a ledger;
/// 3. every ledger in scope was proven under the selected graph's validated
///    context for its language, the focal's own language included;
/// 4. every proven target in those ledgers names an entity the graph holds;
/// 5. every graph-owned file in the potential calling-language domain
///    attributes each parsed call expression to a caller: its ledgers'
///    censuses add up to the parse-side count stamped on the file. An
///    unavailable inventory or a file with no parse census is not complete.
pub fn calls_evidence_for_with<G: GraphStore + ?Sized, T: SiteText + ?Sized>(
    store: &G,
    focal: &Entity,
    text: &T,
    escape: FocalEscape,
) -> crate::error::Result<CallsEvidence> {
    let scan = scan_focal(store, focal, text, escape)?;
    Ok(calls_evidence_from(store, focal, &scan))
}

/// [`calls_evidence_for_with`] over a scan the answer already took.
pub fn calls_evidence_from<G: GraphStore + ?Sized>(
    store: &G,
    focal: &Entity,
    scan: &FocalScan,
) -> CallsEvidence {
    let facts = GraphSiteFacts::new(store);
    let mut because: Vec<&'static str> = Vec::new();
    let mut add = |reason: &'static str| {
        if !because.contains(&reason) {
            because.push(reason);
        }
    };
    if !scan.candidates.is_empty() {
        add(UNSETTLED_CANDIDATE_SITES);
    }
    if escape_residue_clause(&scan.escape).is_some() {
        add(ESCAPE_RESIDUE_LISTED);
    }
    if unkeyed_lookups_clause(&scan.escape).is_some() {
        add(UNKEYED_LOOKUPS_COUNTED);
    }
    // Candidate rows are a result, not a completeness census. An empty stale
    // ledger or an unattributed expression produces no row but still prevents
    // replacing the store's binding proof. Audit the full calling-language
    // domain, including entities the name filter correctly omitted from rows.
    let in_scope: Vec<ScannedCaller> = match store.query_entities(&kin_model::graph::EntityFilter {
        languages: Some(calling_languages(focal.language)),
        ..Default::default()
    }) {
        Ok(entities) => entities
            .into_iter()
            .filter(|entity| caller_in_scope(entity, scan.include_tests))
            .map(|entity| {
                let reading = read_caller_sites(&facts, &entity);
                ScannedCaller {
                    entity,
                    reading,
                    named: true,
                }
            })
            .collect(),
        Err(_) => {
            add(UNATTRIBUTED_EXPRESSIONS);
            Vec::new()
        }
    };
    if in_scope.iter().any(|caller| {
        matches!(
            caller.reading,
            CallerSites::OwedDerivation
                | CallerSites::OwedEnrichment
                | CallerSites::NoResolver { .. }
        )
    }) {
        add(CALLERS_WITHOUT_LEDGER);
    }
    if facts.current_context(focal.language).is_none() {
        add(NO_VALIDATED_CONTEXT);
    }
    let mut files: BTreeMap<String, ()> = BTreeMap::new();
    // The resolved tree also holds admitted files with no entity rows. Their
    // missing parse-side census is a gap, not evidence of zero expressions.
    match store.resolved_tree_snapshot() {
        Ok(Some(tree)) => {
            let languages = calling_languages(focal.language);
            for artifact in tree.artifacts() {
                if !matches!(artifact.entry, kin_model::TreeEntry::Blob { .. }) {
                    continue;
                }
                // Extensions are ASCII even when the filename is not UTF-8.
                // Classify that suffix first: an unrelated raw-byte asset is
                // outside this proof, but an unreadable source path is a gap.
                let bytes = artifact.path.as_bytes();
                let language = bytes
                    .iter()
                    .rposition(|byte| *byte == b'.')
                    .and_then(|at| std::str::from_utf8(&bytes[at..]).ok())
                    .and_then(kin_model::language_of_path);
                if !language.is_some_and(|language| languages.contains(&language)) {
                    continue;
                }
                let Some(path) = artifact.path.as_utf8() else {
                    add(UNATTRIBUTED_EXPRESSIONS);
                    continue;
                };
                files.insert(path.to_owned(), ());
            }
        }
        _ => add(UNATTRIBUTED_EXPRESSIONS),
    }
    let mut callers_read = 0usize;
    for caller in &in_scope {
        // NoSites is valid only with its file's zero-call census. Keep that
        // file in the audit even though it has no ledger and no candidate.
        if let Some(file) = caller
            .entity
            .file_origin
            .as_ref()
            .or_else(|| caller.entity.span.as_ref().map(|span| &span.file))
        {
            files.insert(file.0.clone(), ());
        }
        let Some(ledger) = caller.reading.ledger() else {
            continue;
        };
        callers_read += 1;
        match &caller.reading {
            CallerSites::Current(_) => {}
            CallerSites::Unverified { .. } => add(NO_VALIDATED_CONTEXT),
            _ => add(LEDGER_NOT_UNDER_CURRENT_CONTEXT),
        }
        // The full reading retains every proven target, including sites
        // unrelated to the focal whose liveness still supports this proof.
        if ledger.sites.iter().any(|site| match site.state {
            kin_model::CallSiteState::ProvenTarget { target }
            | kin_model::CallSiteState::ProvenDeclaration {
                declaration: target,
                ..
            } => !matches!(store.get_entity(&target), Ok(Some(_))),
            _ => false,
        }) {
            add(PROVEN_TARGET_NOT_LIVE);
        }
    }
    for file in files.keys() {
        if !file_attributes_every_call(store, &facts, file) {
            add(UNATTRIBUTED_EXPRESSIONS);
            break;
        }
    }
    CallsEvidence {
        settled: because.is_empty(),
        candidates: scan.candidates.clone(),
        unsettled_because: because,
        callers_read,
        escape: scan.escape.clone(),
    }
}

/// Whether every call expression the parser read in `file` belongs to a
/// ledger: the censuses of its entities' current ledgers add up to at least
/// the parse-side count stamped on the file. A file with no parse-side count,
/// or with an entity whose ledger is not current, cannot show it.
pub(crate) fn file_attributes_every_call<G: GraphStore + ?Sized>(
    store: &G,
    facts: &GraphSiteFacts<'_, G>,
    file: &str,
) -> bool {
    let Ok(entities) = store.query_entities(&kin_model::graph::EntityFilter {
        file_path: Some(kin_model::FilePathId::new(file)),
        ..Default::default()
    }) else {
        return false;
    };
    let Some(parsed) = entities.iter().find_map(|entity| {
        entity
            .metadata
            .extra
            .get(kin_model::call_site_reading::FILE_PARSED_CALL_SITES_KEY)
            .and_then(Value::as_u64)
    }) else {
        return false;
    };
    let mut census = 0u64;
    for entity in &entities {
        match read_caller_sites(facts, entity) {
            CallerSites::Current(ledger) => census += u64::from(ledger.census),
            CallerSites::NoSites => {}
            _ => return false,
        }
    }
    census >= parsed
}

/// Where an answer-only reply lists the named leftovers a `call_sites` block
/// carries under `candidates`.
pub const ANSWER_ONLY_NAMED_PATH: &str = "call_site_candidates.named";

/// The text before a clause's first colon: its code.
fn clause_code(clause: &str) -> &str {
    clause.split(':').next().unwrap_or(clause).trim()
}

/// The clause the named leftovers call for: how many sites that spell a call
/// name are not proven, and where each one is listed.
fn named_leftovers_clause(scan: &FocalScan) -> String {
    let listed = scan.named.len();
    let promoted = scan.promoted.len();
    let names = scan.names.join(", ");
    let promoted_part = if promoted > 0 {
        format!(
            ", and {promoted} are served as `name_unique` reference rows because the store \
             holds one definition that name can reach"
        )
    } else {
        String::new()
    };
    format!(
        "{}: {} call site(s) whose callee or caller body spells {names} are not proven: \
         {listed} are listed by caller and line under `call_sites.candidates` \
         (`{ANSWER_ONLY_NAMED_PATH}` in an answer-only reply){promoted_part}, so the answer is \
         complete except those sites",
        kin_model::call_site_reading::NAMED_LEFTOVERS,
        listed + promoted,
    )
}

/// The census reason as a clause fragment: why a value call could reach the
/// focal at all.
fn census_words(escape: &FocalEscape) -> String {
    let reason = match escape {
        FocalEscape::Unknown { reason } => {
            format!("the escape census could not rule that out ({reason})")
        }
        FocalEscape::Escapes { reason, .. } => {
            format!("the escape census found it may be ({reason})")
        }
        FocalEscape::Contained { .. } | FocalEscape::NonCallable { .. } => {
            "the escape census found it is not, so only a name computed at run time could \
             reach it"
                .to_string()
        }
    };
    reason.replace(crate::verdict::CLAUSE_SEPARATOR, ", ")
}

/// The clause the value calls call for: how many sites, and how many callers
/// whose sites are not known, could reach the focal only through a value or a
/// computed name, and why the census kept them.
fn value_calls_clause(scan: &FocalScan) -> String {
    let value = &scan.value_calls;
    let mut parts = Vec::new();
    if value.sites > 0 {
        parts.push(format!(
            "{} unsettled call site(s) in {} caller(s) never spell a call name of the focal",
            value.sites, value.callers
        ));
    }
    if value.callers_unread > 0 {
        parts.push(format!(
            "{} caller(s) that never spell one have call sites the graph has not settled",
            value.callers_unread
        ));
    }
    format!(
        "{}: {}, and they could reach it only if it is held as a value or looked up by a \
         computed name, and {}, so they are counted under `call_sites.value_calls` rather \
         than listed",
        kin_model::call_site_reading::VALUE_CALLS_UNMEASURED,
        parts.join(", and "),
        census_words(&scan.escape),
    )
    .replace(crate::verdict::CLAUSE_SEPARATOR, ", ")
}

/// The clauses a named block calls for: the named scope's own tally clauses,
/// with every per-site clause the listing answers replaced by one
/// [`kin_model::call_site_reading::NAMED_LEFTOVERS`] clause, and one
/// [`kin_model::call_site_reading::VALUE_CALLS_UNMEASURED`] clause for the value
/// calls. Caller-level clauses stay: a caller whose sites are owed, stale,
/// unverified or unprovable holds sites nobody listed.
pub fn named_clauses(scan: &FocalScan) -> Vec<String> {
    let mut clauses: Vec<String> = scan
        .tally()
        .clauses(NAMED_SCOPE)
        .into_iter()
        .filter(|clause| {
            !kin_model::call_site_reading::LISTED_SITE_CODES.contains(&clause_code(clause))
        })
        .collect();
    if !scan.named.is_empty() || !scan.promoted.is_empty() {
        clauses.push(named_leftovers_clause(scan));
    }
    if !scan.value_calls.is_empty() {
        clauses.push(value_calls_clause(scan));
    }
    if let Some(clause) = escape_residue_clause(&scan.escape) {
        clauses.push(clause);
    }
    if let Some(clause) = unkeyed_lookups_clause(&scan.escape) {
        clauses.push(clause);
    }
    if let Some(clause) = decorator_dispatch_clause(scan) {
        clauses.push(clause);
    }
    clauses.sort();
    clauses
}

/// Where a `call_sites` block says how a method it reads may run without a
/// call that spells its name, when that is known.
pub const IMPLICIT_DISPATCH_THROUGH_KEY: &str = "implicit_dispatch_through";

/// [`IMPLICIT_DISPATCH_THROUGH_KEY`] for a constructor: constructions that
/// spell no name the block follows.
pub const CONSTRUCTION_DISPATCH: &str = "construction";

/// Where a `call_sites` block lists the decorators a method may run through
/// without a call that spells its name, when they are the only way it can
/// (see [`FocalScan::dispatch_decorators`]). An empty list says the method is
/// declared under Python's name-preserving decorators alone.
pub const DISPATCH_DECORATORS_KEY: &str = "dispatch_decorators";

/// The decorator sites a named block lists, or `None` when the method may run
/// without a call that spells its name through something other than its
/// decorators, or not at all. Each site names the decorator as written ahead
/// of the declaration and the method it sits on.
fn dispatch_decorator_sites(scan: &FocalScan) -> Option<Value> {
    if !scan.implicit_dispatch {
        return None;
    }
    let (focal_name, decorators) = scan.dispatch_decorators.as_ref()?;
    Some(json!(decorators
        .iter()
        .map(|name| json!({
            "kind": "decorator",
            "name": name,
            "expr": format!("@{name}"),
            "inside": focal_name,
            "inside_id": scan.focal.to_string(),
        }))
        .collect::<Vec<_>>()))
}

/// The clause a method run through its decorators calls for: which decorators
/// it is declared under, so the answer is complete except what they do with
/// it. `None` when the method's callers need no such clause or when something
/// other than its decorators could run it, which keeps the method gate.
fn decorator_dispatch_clause(scan: &FocalScan) -> Option<String> {
    if !scan.implicit_dispatch {
        return None;
    }
    let (focal_name, decorators) = scan.dispatch_decorators.as_ref()?;
    if decorators.is_empty() {
        return None;
    }
    let listed: Vec<String> = decorators.iter().map(|name| format!("@{name}")).collect();
    Some(
        format!(
            "{}: {focal_name} is declared under {} ({}), and a call that spells its name runs \
             what that decorator returned, so it could run without such a call only through \
             what the decorator does with it, each listed in `call_sites.{DISPATCH_DECORATORS_KEY}`",
            kin_model::call_site_reading::DECORATOR_DISPATCH_LISTED,
            if decorators.len() == 1 {
                "1 decorator".to_string()
            } else {
                format!("{} decorators", decorators.len())
            },
            listed.join(", "),
        )
        .replace(crate::verdict::CLAUSE_SEPARATOR, ", "),
    )
}

/// The block `find_references` serves for a focal: the tally over the named
/// scope (every caller that could spell a call name of the focal, narrowed to
/// the settled sites that call the focal and the unsettled sites tied to it by
/// name), one row for every named leftover, never a sample, the promotion
/// reading, and the value calls counted once with the census reason. The
/// block is settled exactly while its clauses name nothing; while it names
/// only the listed residue, the verdict reads it as complete except that
/// residue.
pub fn named_block(scan: &FocalScan) -> Value {
    let tally = scan.tally();
    let mut block = block_json(&tally, NAMED_SCOPE);
    let clauses = named_clauses(scan);
    block["settled"] = json!(clauses.is_empty());
    block["clauses"] = json!(clauses);
    block["named_complete"] = json!(scan.named_complete());
    if scan.implicit_dispatch {
        block["implicit_dispatch"] = json!(true);
    }
    if let Some(decorators) = dispatch_decorator_sites(scan) {
        block[DISPATCH_DECORATORS_KEY] = decorators;
    }
    if scan.implicit_dispatch && scan.construction_dispatch {
        block[IMPLICIT_DISPATCH_THROUGH_KEY] = json!(CONSTRUCTION_DISPATCH);
    }
    block["call_names"] = json!(scan.names);
    let mut focal_escape = scan.escape.summary_json(CALL_SITE_CANDIDATE_SAMPLE_MAX);
    site_expression::annotate_residue(&mut focal_escape, &scan.residue_lines);
    block["focal_escape"] = focal_escape.clone();
    block["settled_to_focal"] = json!(scan.settled_to_focal);
    if scan.proven_by_edge > 0 {
        block["proven_by_edge"] = json!(scan.proven_by_edge);
    }
    block["candidate_count"] = json!(scan.named.len());
    block["candidates"] = json!(scan.named);
    if let Some(rows) = block["candidates"].as_array_mut() {
        let cut = site_expression::cap_expressions(
            rows.iter_mut(),
            site_expression::NAMED_EXPR_BYTES_MAX,
        );
        if let Some(disclosure) = cut.disclosure(
            site_expression::NAMED_EXPR_BYTES_MAX,
            "rows, listed most likely to reach the focal first,",
        ) {
            block[NAMED_EXPR_ELIDED_KEY] = disclosure;
        }
    }
    block["name_unique"] = json!({
        "reading": scan.name_unique,
        "promoted": scan.promoted.len(),
    });
    if !scan.value_calls.is_empty() {
        block["value_calls"] = json!({
            "sites": scan.value_calls.sites,
            "callers": scan.value_calls.callers,
            "callers_unread": scan.value_calls.callers_unread,
            "by_reason": scan.value_calls.by_reason,
            "census": focal_escape,
        });
    }
    block
}

/// The scope a [`mention_block`] reads: the focal's non-call reads.
pub const MENTION_SCOPE: &str = "the focal's non-call reads";

/// The residue lists a [`mention_block`] keeps: the census's reading of the
/// focal's non-call reads, the lookups by a computed name that bound them,
/// and the regions no record covers.
const MENTION_RESIDUE_KEYS: [&str; 3] = [
    kin_model::call_site_reading::MENTIONS_KEY,
    "unkeyed_lookups",
    "unrecorded",
];

/// The block a reference answer serves when it asks for imports or references
/// and not calls: the escape census's reading of every non-call read of the
/// focal, matched against the rows the graph stores into it, which is what
/// decides whether those kinds are complete (`reference_completion` in the
/// verdict). Each unmatched read is addressed by the declaration it sits in
/// and its line, as a named block addresses its residue.
///
/// It carries no tally and no `settled`, because it read no call site: a call
/// census settles calls alone, and this block makes no claim about them. The
/// census's call residue (values passed on, reflective lookups, protocol
/// dispatch) is left out for the same reason; a value use is a non-call read
/// and already sits in the mention reading.
pub fn mention_block<G: GraphStore + ?Sized, T: SiteText + ?Sized>(
    store: &G,
    text: &T,
    escape: &FocalEscape,
) -> Value {
    let lines = site_expression::residue_lines(store, text, escape, CALL_SITE_CANDIDATE_SAMPLE_MAX);
    let mut focal_escape = escape.summary_json(CALL_SITE_CANDIDATE_SAMPLE_MAX);
    if let Some(residue) = focal_escape
        .get_mut("residue")
        .and_then(Value::as_object_mut)
    {
        residue.retain(|key, _| MENTION_RESIDUE_KEYS.contains(&key.as_str()));
    }
    site_expression::annotate_residue(&mut focal_escape, &lines);
    json!({
        "scope": MENTION_SCOPE,
        "focal_escape": focal_escape,
    })
}

/// Where a named block says how many of its `candidates` its expression
/// budget left without `expr`, beside the list.
pub const NAMED_EXPR_ELIDED_KEY: &str = "candidates_expr_elided";

/// A focal the escape census lists sites for is reachable without a call
/// that spells its call names.
pub const ESCAPE_RESIDUE_LISTED: &str = "escape_residue_listed";

/// Lookups by a name no record follows could reach the focal, counted rather
/// than listed (see [`kin_model::EscapeResidue::unkeyed_lookups`]).
pub const UNKEYED_LOOKUPS_COUNTED: &str = kin_model::call_site_reading::UNKEYED_LOOKUPS_COUNTED;

/// The clause naming what the census listed as able to reach the focal
/// without a call that spells a call name, or `None` when it listed nothing a
/// reader can check. Unkeyed lookups are the clause of their own
/// ([`unkeyed_lookups_clause`]).
pub fn escape_residue_clause(escape: &FocalEscape) -> Option<String> {
    let residue = escape.residue()?;
    if residue.lists_nothing_to_check() {
        return None;
    }
    let mut parts = Vec::new();
    for (count, what) in [
        (residue.values.len(), "value uses"),
        (residue.implicit_calls.len(), "implicit calls"),
        (residue.reflective.len(), "reflective lookups"),
        (residue.unrecorded.len(), "unrecorded regions"),
    ] {
        if count > 0 {
            parts.push(format!("{count} {what}"));
        }
    }
    if let Some(protocol) = &residue.protocol {
        parts.push(match &protocol.through {
            Some(through) => format!("implicit dispatch through {through}"),
            None => format!("{} protocol dispatch sites", protocol.sites.len()),
        });
    }
    Some(format!(
        "escape_residue_listed: {} sites could reach the focal without a call that spells \
         its call names ({}), each listed in focal_escape.residue",
        residue.unsettled_count(),
        parts.join(", ")
    ))
}

/// The clause counting lookups by a name computed at run time that no record
/// follows and that spell no call name, or `None` when there are none. They
/// could reach the focal as they could reach any declaration their receiver
/// holds, which every reader that does not run the program shares, so the
/// answer counts them and names no site to check. The clause is residue: it
/// keeps the verdict `certified_except`, so it never licenses an absence.
pub fn unkeyed_lookups_clause(escape: &FocalEscape) -> Option<String> {
    let count = escape.residue()?.unkeyed_lookups.len();
    (count > 0).then(|| {
        format!(
            "{UNKEYED_LOOKUPS_COUNTED}: {count} lookups by a name computed at run time could \
             reach the focal as they could reach anything their receiver holds, and none \
             spells a call name, so they are counted in focal_escape.residue.unkeyed_lookups \
             rather than listed as sites to check"
        )
    })
}

// ── Text ──────────────────────────────────────────────────────────────────

/// Candidate rows a terminal answer lists before saying how many more.
pub const CANDIDATE_LINES_MAX: usize = 20;

/// The unsettled call sites a store-wide reading kept, in plain words for a
/// person at a terminal: the named leftovers, each at its caller and its line
/// in that caller, the sites promoted to `name_unique` rows, and the value
/// calls counted once by why they are kept. Empty when the reading kept none.
pub fn candidate_lines(scan: &FocalScan, focal_name: &str) -> Vec<String> {
    if scan.candidates.is_empty() {
        return Vec::new();
    }
    let mut lines = vec![format!(
        "Unproven call sites that could call {focal_name}: {}.",
        scan.candidates.len()
    )];
    let site_line = |candidate: &CandidateSite| {
        let at = candidate
            .line_in_entity
            .map(|line| format!(", line {line} in it"))
            .unwrap_or_default();
        let callee = match (&candidate.expr, &candidate.callee) {
            (Some(expr), _) => format!(": {expr}"),
            (None, Some(callee)) => format!(" calls {callee}"),
            (None, None) => String::new(),
        };
        let why = candidate
            .state_reason
            .as_deref()
            .map(|reason| format!(" ({})", reason.replace('_', " ")))
            .unwrap_or_default();
        format!(
            "    {}{at}{callee}, {}{why}",
            candidate.caller_name,
            candidate.state.wire().replace('_', " ")
        )
    };
    if !scan.promoted.is_empty() {
        lines.push(format!(
            "  {} name {focal_name}, and the store holds no other definition that name can \
             reach, so each is a name_unique reference:",
            scan.promoted.len()
        ));
        lines.extend(scan.promoted.iter().map(site_line));
    }
    if !scan.named.is_empty() {
        lines.push(format!(
            "  {} name {focal_name}, so check them first:",
            scan.named.len()
        ));
        lines.extend(scan.named.iter().take(CANDIDATE_LINES_MAX).map(site_line));
        if scan.named.len() > CANDIDATE_LINES_MAX {
            lines.push(format!(
                "    and {} more that name it, every one in the call_sites block.",
                scan.named.len() - CANDIDATE_LINES_MAX
            ));
        }
    }
    if !scan.value_calls.is_empty() {
        let unread = if scan.value_calls.callers_unread > 0 {
            format!(
                ", and {} caller(s) that never spell it are owed their sites",
                scan.value_calls.callers_unread
            )
        } else {
            String::new()
        };
        lines.push(format!(
            "  {} more could reach it only if {focal_name} is held as a value or looked up by a \
             computed name, and {}{unread}.",
            scan.value_calls.sites,
            census_words(&scan.escape)
        ));
    }
    lines
}

/// A share as a whole percentage, the way a terminal line states it.
fn percent(share: Option<f64>) -> String {
    match share {
        Some(share) => format!("{:.0}%", share * 100.0),
        None => "no share".to_string(),
    }
}

/// The block as terminal lines, so the CLI says what the MCP payload says.
///
/// A focal's block lists one line per row, `+N` lines below the caller's
/// first line with the text at the site, the state, its reason and the target
/// id when a resolver proved one. A store-wide block lists each state's share
/// of the census. Every block ends with the clauses it calls for, one per
/// line, or with the sentence that it is settled.
pub fn text_lines(block: &Value) -> Vec<String> {
    let mut lines = Vec::new();
    let scope = block["scope"].as_str().unwrap_or("this answer");
    let sites = block["sites"].as_u64().unwrap_or(0);
    let callers = block["callers"].as_u64().unwrap_or(0);
    let owed = block["callers_owed_derivation"].as_u64().unwrap_or(0)
        + block["callers_owed_enrichment"].as_u64().unwrap_or(0);
    let unproven = block["callers_unproven_no_resolver"].as_u64().unwrap_or(0);
    let mut header = format!("Call sites in {scope}: {sites} across {callers} caller(s)");
    if owed > 0 {
        header.push_str(&format!(", {owed} caller(s) owed"));
    }
    if unproven > 0 {
        header.push_str(&format!(", {unproven} caller(s) no resolver can prove"));
    }
    if let Some(reading) = block["reading"].as_str() {
        header.push_str(&format!(" ({reading})"));
    }
    lines.push(header);
    if let Some(cannot_name) = block["owed_callers_cannot_name_focal"]
        .as_u64()
        .filter(|count| *count > 0)
    {
        lines.push(format!(
            "  {cannot_name} more owed caller(s) there never spell the focal's name, so they \
             cannot call it by name and are not counted"
        ));
    }
    if let Some(shares) = block["shares"].as_object() {
        let parts: Vec<String> = shares
            .iter()
            .filter(|(_, share)| share["sites"].as_u64().unwrap_or(0) > 0)
            .map(|(state, share)| {
                format!(
                    "{state} {} ({})",
                    share["sites"].as_u64().unwrap_or(0),
                    percent(share["share"].as_f64())
                )
            })
            .collect();
        if !parts.is_empty() {
            lines.push(format!("  by state: {}", parts.join(", ")));
        }
    }
    if let Some(files) = block["owed_files"].as_array() {
        for file in files {
            lines.push(format!(
                "  owed enrichment: {} ({} caller(s))",
                file["file"].as_str().unwrap_or("?"),
                file["callers"].as_u64().unwrap_or(0)
            ));
        }
        if let Some(withheld) = block["owed_files_withheld"].as_u64() {
            lines.push(format!(
                "  ({withheld} more files with owed callers are not listed)"
            ));
        }
    }
    if let Some(files) = block["owed_outside_scope"]["files"].as_array() {
        for file in files.iter().take(5) {
            lines.push(format!(
                "  owed outside {scope}: {} ({} caller(s))",
                file["file"].as_str().unwrap_or("?"),
                file["callers"].as_u64().unwrap_or(0)
            ));
        }
        let more = block["owed_outside_scope"]["file_count"]
            .as_u64()
            .unwrap_or(0)
            .saturating_sub(5);
        if more > 0 {
            lines.push(format!(
                "  ({more} more files outside {scope} with owed callers are not listed)"
            ));
        }
    }
    if let Some(rows) = block["rows"].as_array() {
        for row in rows {
            let at = row["line_in_entity"]
                .as_u64()
                .map(|line| format!("+{line}"))
                .unwrap_or_else(|| "+?".to_string());
            let callee = row["callee"]
                .as_str()
                .map(|callee| format!("`{callee}`"))
                .unwrap_or_else(|| {
                    format!(
                        "(text unavailable: {})",
                        row["callee_unavailable"].as_str().unwrap_or("unknown")
                    )
                });
            let mut line = format!("  {at} {callee} {}", row["state"].as_str().unwrap_or("?"));
            if let Some(reason) = row["reason"].as_str() {
                line.push_str(&format!(" ({reason})"));
            }
            if let Some(target) = row["target"].as_str() {
                line.push_str(&format!(" -> {target}"));
            }
            lines.push(line);
        }
        if let Some(withheld) = block["rows_withheld"].as_u64() {
            lines.push(format!(
                "  ({withheld} more sites past the {CALL_SITE_ROWS_MAX}-row cap are not listed)"
            ));
        }
    }
    let clauses: Vec<&str> = block["clauses"]
        .as_array()
        .map(|clauses| clauses.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if clauses.is_empty() {
        lines.push("  every site in scope is settled".to_string());
    } else {
        for clause in clauses {
            lines.push(format!("  not settled: {clause}"));
        }
    }
    lines
}

/// Graphs holding call-site ledgers, for the tests of every surface that
/// serves them.
#[cfg(test)]
pub(crate) mod fixture {
    use kin_db::InMemoryGraph;
    use kin_model::entity::{Entity, EntityMetadata, SourceSpan};
    use kin_model::graph::EntityStore as _;
    use kin_model::{
        CallSite, CallSiteLedger, CallSiteState, EntityId, EntityKind, EntityRole, FilePathId,
        FingerprintAlgorithm, Hash256, LanguageId, ProofContext, ResolutionRecord,
        ResolutionRecordDelta, ResolutionRecordId, SemanticFingerprint, TransactionDelta,
        Visibility,
    };

    /// A function whose own text is `body`, starting at byte `start` of
    /// `file`.
    pub(crate) fn spanned_entity(
        name: &str,
        file: &str,
        language: LanguageId,
        start: usize,
        body: &str,
    ) -> Entity {
        Entity {
            id: EntityId::from_content(file, name, "Function", start as u32),
            kind: EntityKind::Function,
            name: name.to_string(),
            language,
            fingerprint: SemanticFingerprint {
                algorithm: FingerprintAlgorithm::V1TreeSitter,
                ast_hash: Hash256::from_bytes([0; 32]),
                signature_hash: Hash256::from_bytes([0; 32]),
                behavior_hash: Hash256::from_bytes([9; 32]),
                equivalence_hash: Hash256::from_bytes([0; 32]),
                stability_score: 1.0,
            },
            file_origin: Some(FilePathId::new(file)),
            span: Some(SourceSpan {
                file: FilePathId::new(file),
                start_byte: start,
                end_byte: start + body.len(),
                start_line: 10,
                start_col: 0,
                end_line: 10 + body.matches('\n').count() as u32,
                end_col: 0,
            }),
            signature: format!("def {name}()"),
            visibility: Visibility::Public,
            role: EntityRole::Source,
            doc_summary: None,
            metadata: EntityMetadata::default(),
            lineage_parent: None,
            created_in: None,
            superseded_by: None,
        }
    }

    /// The proof context `language`'s resolver ran under, told apart from
    /// another by `version`.
    pub(crate) fn proof_context(language: LanguageId, version: &str) -> ResolutionRecord {
        ResolutionRecord::ProofContext(ProofContext {
            language,
            resolver: "lsp:pyright".to_string(),
            resolver_version: version.to_string(),
            configuration_hash: Hash256::from_bytes([0x41; 32]),
            environment_hash: Hash256::from_bytes([0x42; 32]),
            environment_summary: "python 3.12".to_string(),
        })
    }

    /// `caller`'s ledger under `context`, one site per callee token, each
    /// token found in `body` at or after the previous one.
    pub(crate) fn ledger(
        caller: &Entity,
        body: &str,
        context: ResolutionRecordId,
        sites: Vec<(&str, CallSiteState)>,
    ) -> ResolutionRecord {
        let mut from = 0usize;
        let sites: Vec<CallSite> = sites
            .into_iter()
            .map(|(token, state)| {
                let offset = from + body[from..].find(token).expect("the token is in the body");
                from = offset + token.len();
                CallSite {
                    offset: offset as u32,
                    length: token.len() as u32,
                    state,
                }
            })
            .collect();
        ResolutionRecord::CallSites(CallSiteLedger {
            caller: caller.id,
            behavior_hash: caller.fingerprint.behavior_hash,
            body_hash: Hash256::from_bytes([0x43; 32]),
            context,
            census: sites.len() as u32,
            sites,
        })
    }

    /// Put `entities` in the graph, then `records` in one transaction, each
    /// proof context a ledger names before the ledger.
    pub(crate) fn admit(
        store: &InMemoryGraph,
        entities: &[&Entity],
        records: Vec<ResolutionRecord>,
    ) {
        // This fixture admits a resolver's completed evidence, including its
        // validation. Tests for legacy/missing validation remove that record.
        let validations: Vec<_> = records
            .iter()
            .filter_map(|record| record.as_proof_context())
            .map(|context| {
                ResolutionRecord::ContextValidation(kin_model::ContextValidation {
                    language: context.language,
                    state: kin_model::ContextValidationState::Validated {
                        context: context.clone(),
                    },
                })
            })
            .collect();
        let mut records = records;
        records.extend(validations);
        records.sort_by_key(|record| !matches!(record, ResolutionRecord::ProofContext(_)));
        let records: Vec<ResolutionRecord> = records
            .into_iter()
            .filter(|record| {
                store
                    .lookup_resolution_record(&record.id())
                    .ok()
                    .flatten()
                    .is_none()
            })
            .collect();
        for entity in entities {
            if store.get_entity(&entity.id).ok().flatten().is_none() {
                store
                    .upsert_entity(entity)
                    .expect("the fixture's entity is admitted");
            }
        }
        store
            .apply_transaction_delta(&TransactionDelta {
                resolution_record_deltas: records
                    .into_iter()
                    .map(|new| ResolutionRecordDelta::Added { new })
                    .collect(),
                ..TransactionDelta::default()
            })
            .expect("the fixture's records are admitted");
    }

    /// The id a record goes by.
    pub(crate) fn id_of(record: &ResolutionRecord) -> ResolutionRecordId {
        record.id()
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::{admit, id_of, ledger, proof_context, spanned_entity};
    use super::*;
    use crate::handlers::external_symbols::quote_site;
    use kin_db::InMemoryGraph;
    use kin_model::graph::EntityStore as _;
    use kin_model::{
        CallSiteState, ExternalReference, ExternalReferenceDelta, TransactionDelta,
        UnresolvedReason,
    };

    fn census_entity(
        name: &str,
        file: &str,
        language: LanguageId,
        body: &str,
        parsed: u64,
    ) -> Entity {
        let mut entity = spanned_entity(name, file, language, 0, body);
        entity.metadata.extra.insert(
            kin_model::call_site_reading::FILE_PARSED_CALL_SITES_KEY.into(),
            json!(parsed),
        );
        entity
    }

    #[test]
    fn calls_evidence_audits_empty_stale_and_unverified_domain_ledgers() {
        use kin_model::{ResolutionRecord, ResolutionRecordDelta};
        for unverified in [false, true] {
            let store = InMemoryGraph::new();
            let focal = census_entity(
                "target",
                "target.ts",
                LanguageId::TypeScript,
                "function target() {}",
                0,
            );
            // JavaScript can call TypeScript. Its absent validation must not be
            // hidden by the focal language's independently valid context.
            let caller_language = if unverified {
                LanguageId::JavaScript
            } else {
                LanguageId::TypeScript
            };
            let caller = census_entity(
                "unrelated",
                "caller.js",
                caller_language,
                "function unrelated() {}",
                0,
            );
            let focal_context = proof_context(LanguageId::TypeScript, "current");
            let caller_context = proof_context(caller_language, "current");
            let caller_record = ledger(&caller, "", caller_context.id(), vec![]);
            admit(
                &store,
                &[&focal, &caller],
                vec![
                    focal_context.clone(),
                    ledger(&focal, "", focal_context.id(), vec![]),
                ],
            );
            admit(
                &store,
                &[],
                vec![caller_context.clone(), caller_record.clone()],
            );
            assert!(calls_evidence_for(&store, &focal).unwrap().settled);
            let changes = if unverified {
                let old = store
                    .lookup_resolution_record(&ResolutionRecordId::context_validation(
                        caller_language,
                    ))
                    .unwrap()
                    .unwrap();
                vec![ResolutionRecordDelta::Removed { old }]
            } else {
                let old_context = proof_context(caller_language, "old");
                let mut old_ledger = caller_record.clone();
                let ResolutionRecord::CallSites(record) = &mut old_ledger else {
                    unreachable!()
                };
                record.context = old_context.id();
                vec![
                    ResolutionRecordDelta::Added { new: old_context },
                    ResolutionRecordDelta::Modified {
                        old: caller_record,
                        new: old_ledger,
                    },
                ]
            };
            store
                .apply_transaction_delta(&TransactionDelta {
                    resolution_record_deltas: changes,
                    ..Default::default()
                })
                .unwrap();
            let evidence = calls_evidence_for(&store, &focal).unwrap();
            assert!(evidence.candidates.is_empty());
            assert!(!evidence.settled);
            assert!(
                evidence.unsettled_because.contains(&if unverified {
                    NO_VALIDATED_CONTEXT
                } else {
                    LEDGER_NOT_UNDER_CURRENT_CONTEXT
                }),
                "{evidence:?}"
            );
        }
    }

    #[test]
    fn calls_evidence_audits_unattributed_files_omitted_from_candidate_rows() {
        for proven_outside in [false, true] {
            let store = InMemoryGraph::new();
            let focal = census_entity(
                "target",
                "target.ts",
                LanguageId::TypeScript,
                "function target() {}",
                0,
            );
            let body = "function unrelated() { external(); hidden(); }";
            let mut caller =
                census_entity("unrelated", "caller.ts", LanguageId::TypeScript, body, 2);
            caller.metadata.extra.insert(
                kin_parser::extract::EMBEDDING_BODY_PREVIEW_KEY.into(),
                json!(body),
            );
            let context = proof_context(LanguageId::TypeScript, "current");
            admit(
                &store,
                &[&focal, &caller],
                vec![
                    context.clone(),
                    ledger(&focal, "", context.id(), vec![]),
                    ledger(
                        &caller,
                        body,
                        context.id(),
                        if proven_outside {
                            vec![("external", CallSiteState::ProvenOutside)]
                        } else {
                            vec![]
                        },
                    ),
                ],
            );
            let scan = scan_focal(&store, &focal, &NoSiteText, FocalEscape::contained(2)).unwrap();
            assert!(scan.candidates.is_empty());
            assert!(
                !scan
                    .callers
                    .iter()
                    .any(|scanned| scanned.entity.id == caller.id),
                "the unrelated preview is omitted from focal candidate rows"
            );
            let evidence = calls_evidence_from(&store, &focal, &scan);
            assert!(!evidence.settled);
            assert!(
                evidence
                    .unsettled_because
                    .contains(&UNATTRIBUTED_EXPRESSIONS),
                "{evidence:?}"
            );
        }
    }

    /// `_json.load(fp)` in a file that says `import json as _json`: the named
    /// site says which module its receiver is imported from, read from the
    /// record of the caller's file, and whether the tree holds a file that
    /// could be that module. `src/flask/json/` sits inside the package
    /// `flask`, so it is not what `import json` finds; a `json.py` at the
    /// repository root is.
    #[test]
    fn a_named_site_says_what_its_receiver_is_imported_from() {
        const READS: &str = "def read_config(fp):\n    return _json.load(fp)\n";
        let py = LanguageId::Python;
        let blob = |byte: u8| kin_model::Hash256::from_bytes([byte; 32]);
        let scan_with = |extra: Option<&str>| {
            let store = InMemoryGraph::new();
            let focal = spanned_entity(
                "load",
                "src/flask/json/__init__.py",
                py,
                0,
                "def load(fp):\n    pass\n",
            );
            let mut reader = spanned_entity("read_config", "tests/test_config.py", py, 200, READS);
            // The reader starts on line 10 of its file, so the call is on 11.
            let mut record = kin_model::FileEscapeRecord::new(kin_model::ModuleScope::Open);
            record.values.insert(
                "_json".into(),
                vec![kin_model::ValueOccurrence {
                    line: 11,
                    context: kin_model::OccurrenceContext::Mention,
                    binding: kin_model::OccurrenceBinding::Import,
                    receiver: None,
                    receiver_kind: None,
                    aliases: Vec::new(),
                    module: Some("json".into()),
                    import: Some((0, 19)),
                    imported: Some("json".into()),
                    target_line: None,
                    form: Some(kin_model::MentionForm::Receiver),
                }],
            );
            reader
                .metadata
                .extra
                .insert("blob_hash".into(), json!(blob(0x71).to_string()));
            reader.metadata.extra.insert(
                kin_model::VALUE_ESCAPE_RECORD_KEY.into(),
                record.to_metadata(),
            );
            let context = proof_context(py, "1.1.400");
            let context_id = id_of(&context);
            admit(
                &store,
                &[&focal, &reader],
                vec![
                    context,
                    ledger(
                        &reader,
                        READS,
                        context_id,
                        vec![(
                            "load",
                            CallSiteState::Unresolved {
                                reason: UnresolvedReason::NoAnswer,
                            },
                        )],
                    ),
                ],
            );
            let files = [
                ("src/flask/__init__.py", 0x70),
                ("tests/test_config.py", 0x71),
                ("src/flask/json/__init__.py", 0x72),
            ];
            let tree_deltas = files
                .into_iter()
                .chain(extra.map(|path| (path, 0x73)))
                .map(|(path, byte)| kin_model::TreeDelta::Added {
                    artifact_id: kin_model::ArtifactId::new(),
                    new: kin_model::LocatedEntry::new(
                        kin_model::RepoPath::from_utf8(path).unwrap(),
                        kin_model::TreeEntry::blob(blob(byte), false),
                    ),
                })
                .collect();
            store
                .apply_transaction_delta(&TransactionDelta {
                    tree_deltas,
                    ..Default::default()
                })
                .unwrap();
            scan_focal(&store, &focal, &Body(READS), NO_CENSUS).unwrap()
        };
        let import_of = |rows: &[CandidateSite]| {
            let [row] = rows else {
                panic!("{rows:?}");
            };
            assert_eq!(row.caller_name, "read_config");
            assert_eq!(row.receiver.as_deref(), Some("_json"));
            assert_eq!(row.expr.as_deref(), Some("_json.load(fp)"));
            row.receiver_import.clone()
        };
        // `load` is the one definition of its name, and still a call on the
        // outside module is never promoted to a row of it.
        let outside = scan_with(None);
        assert!(outside.promoted.is_empty(), "{:?}", outside.promoted);
        assert_eq!(
            import_of(&outside.named),
            Some(json!({"module": "json", "outside_repository": true}))
        );
        // The row serializes the field where a reply's named rows read it.
        let served = serde_json::to_value(&outside.named[0]).unwrap();
        assert_eq!(
            served[site_expression::RECEIVER_IMPORT_KEY]["module"],
            "json"
        );
        assert!(site_expression::EXPR_FIELDS.contains(&site_expression::RECEIVER_IMPORT_KEY));
        // With a `json.py` the repository could supply, the site is read as
        // before: the name is unique, so it is promoted, and it says what
        // its receiver imports.
        let inside = scan_with(Some("json.py"));
        assert!(inside.named.is_empty(), "{:?}", inside.named);
        assert_eq!(
            import_of(&inside.promoted),
            Some(json!({"module": "json", "outside_repository": false}))
        );
    }

    #[test]
    fn calls_evidence_includes_calling_domain_files_without_entities() {
        let store = InMemoryGraph::new();
        let focal = census_entity(
            "target",
            "target.ts",
            LanguageId::TypeScript,
            "function target() {}",
            0,
        );
        let context = proof_context(LanguageId::TypeScript, "current");
        admit(
            &store,
            &[&focal],
            vec![context.clone(), ledger(&focal, "", context.id(), vec![])],
        );
        assert!(calls_evidence_for(&store, &focal).unwrap().settled);
        store
            .apply_transaction_delta(&TransactionDelta {
                tree_deltas: vec![kin_model::TreeDelta::Added {
                    artifact_id: kin_model::ArtifactId::new(),
                    new: kin_model::LocatedEntry::new(
                        kin_model::RepoPath::from_utf8("unattributed.js").unwrap(),
                        kin_model::TreeEntry::blob(
                            kin_model::Hash256::from_bytes([0x61; 32]),
                            false,
                        ),
                    ),
                }],
                ..Default::default()
            })
            .unwrap();
        let evidence = calls_evidence_for(&store, &focal).unwrap();
        assert!(evidence.candidates.is_empty());
        assert!(!evidence.settled);
        assert!(
            evidence
                .unsettled_because
                .contains(&UNATTRIBUTED_EXPRESSIONS),
            "{evidence:?}"
        );
    }

    #[test]
    fn calls_evidence_raw_byte_paths_only_bound_calling_domain_blobs() {
        let cases: &[(&[u8], bool, bool)] = &[
            (b"assets/\xff.png", false, true),
            (b"other/\xff.py", false, true),
            (b"src/\xff.ts", false, false),
            (b"src/\xff.js", false, false),
            (b"src/\xff.ts", true, true),
            (b"src/linked.ts", true, true),
        ];
        for &(path, symlink, settled) in cases {
            let store = InMemoryGraph::new();
            let focal = census_entity(
                "target",
                "target.ts",
                LanguageId::TypeScript,
                "function target() {}",
                0,
            );
            let context = proof_context(LanguageId::TypeScript, "current");
            admit(
                &store,
                &[&focal],
                vec![context.clone(), ledger(&focal, "", context.id(), vec![])],
            );
            let hash = kin_model::Hash256::from_bytes([0x62; 32]);
            store
                .apply_transaction_delta(&TransactionDelta {
                    tree_deltas: vec![kin_model::TreeDelta::Added {
                        artifact_id: kin_model::ArtifactId::new(),
                        new: kin_model::LocatedEntry::new(
                            kin_model::RepoPath::from_bytes(path).unwrap(),
                            if symlink {
                                kin_model::TreeEntry::symlink(hash)
                            } else {
                                kin_model::TreeEntry::blob(hash, false)
                            },
                        ),
                    }],
                    ..Default::default()
                })
                .unwrap();
            let evidence = calls_evidence_for(&store, &focal).unwrap();
            assert_eq!(
                evidence.settled, settled,
                "{path:?}, symlink={symlink}: {evidence:?}"
            );
            assert!(evidence.candidates.is_empty());
            assert_eq!(
                evidence
                    .unsettled_because
                    .contains(&UNATTRIBUTED_EXPRESSIONS),
                !settled
            );
        }
    }

    #[test]
    fn calls_evidence_keeps_true_no_sites_and_current_empty_ledgers_settled() {
        for empty_ledger in [false, true] {
            let store = InMemoryGraph::new();
            let focal = census_entity(
                "target",
                "target.ts",
                LanguageId::TypeScript,
                "function target() {}",
                0,
            );
            let caller = census_entity(
                "unrelated",
                "caller.ts",
                LanguageId::TypeScript,
                "function unrelated() {}",
                0,
            );
            let context = proof_context(LanguageId::TypeScript, "current");
            let mut records = vec![context.clone()];
            if empty_ledger {
                records.extend([
                    ledger(&focal, "", context.id(), vec![]),
                    ledger(&caller, "", context.id(), vec![]),
                ]);
            }
            admit(&store, &[&focal, &caller], records);
            let facts = GraphSiteFacts::new(&store);
            if !empty_ledger {
                assert!(matches!(
                    read_caller_sites(&facts, &caller),
                    CallerSites::NoSites
                ));
            }
            let evidence = calls_evidence_for(&store, &focal).unwrap();
            assert!(evidence.settled, "{evidence:?}");
            assert!(evidence.candidates.is_empty());
            assert_eq!(evidence.callers_read, if empty_ledger { 2 } else { 0 });
        }
    }

    /// Why no resolver can prove a language's sites, from what was published:
    /// a switched-off daemon first, then the probe's word per language, and
    /// a language no build of Kin enriches never has a resolver. Nothing
    /// published, or a language the probe did not report on, claims nothing.
    #[test]
    fn no_resolver_is_read_from_what_the_resolver_process_published() {
        use kin_core::reference_coverage::{LanguageServerReadiness, LanguageServerReadinessMap};
        let probed: LanguageServerReadinessMap = [
            (LanguageId::Python, LanguageServerReadiness::Usable),
            (LanguageId::Go, LanguageServerReadiness::Absent),
            (
                LanguageId::Rust,
                LanguageServerReadiness::Unusable {
                    reason: "no Cargo".to_string(),
                },
            ),
        ]
        .into_iter()
        .collect();
        let read = |switch, readiness, language| no_resolver_for(switch, readiness, language);
        let on = Some(EnrichmentSwitch::On);
        assert_eq!(read(None, None, LanguageId::Python), None);
        assert_eq!(read(on, None, LanguageId::Python), None);
        assert_eq!(
            read(
                Some(EnrichmentSwitch::Off),
                Some(&probed),
                LanguageId::Python
            ),
            Some(NoResolver::EnrichmentOff)
        );
        // The repository's own setting is named as itself, ahead of any probe,
        // so a reading says how to turn it on instead of calling it owed.
        assert_eq!(
            read(
                Some(EnrichmentSwitch::OffForRepository),
                Some(&probed),
                LanguageId::Go
            ),
            Some(NoResolver::EnrichmentOffForRepository)
        );
        assert_eq!(read(on, Some(&probed), LanguageId::Python), None);
        assert_eq!(
            read(on, Some(&probed), LanguageId::Go),
            Some(NoResolver::NoLanguageServer)
        );
        assert_eq!(
            read(on, Some(&probed), LanguageId::Rust),
            Some(NoResolver::ServerCannotStart {
                reason: "no Cargo".to_string()
            })
        );
        // Enrichable, and the probe said nothing about it: unknown.
        assert_eq!(read(on, Some(&probed), LanguageId::TypeScript), None);
        // A language no build of Kin enriches, whether or not anything was
        // published.
        assert_eq!(
            read(on, Some(&probed), LanguageId::Java),
            Some(NoResolver::NoLanguageServer)
        );
        assert_eq!(
            read(None, None, LanguageId::Ruby),
            Some(NoResolver::NoLanguageServer)
        );
        assert_eq!(
            read(on, None, LanguageId::Java),
            Some(NoResolver::NoLanguageServer)
        );
    }

    const BODY: &str = "def run(data):\n    helper(data)\n    json.dumps(data)\n    handler(data)\n    mystery(data)\n";

    /// The caller's own body, as the answer's body reader would hand it over.
    struct Body<'a>(&'a str);

    impl SiteText for Body<'_> {
        fn quote(&self, caller: &Entity, site: &SourceSpan) -> Result<String, &'static str> {
            let span = caller.span.as_ref().ok_or("caller_has_no_span")?;
            quote_site(caller, site, self.0, span.start_byte)
        }
    }

    struct Fixture {
        store: InMemoryGraph,
        run: Entity,
        helper: Entity,
        external: ExternalReference,
        context: ResolutionRecordId,
    }

    /// `run` calls `helper` in the repository, `dumps` outside it, a value
    /// bound to `handler`, and `mystery`, which the resolver answered and could
    /// not place.
    fn fixture(with_ledger: bool) -> Fixture {
        let store = InMemoryGraph::new();
        let run = spanned_entity("run", "app.py", LanguageId::Python, 200, BODY);
        let helper = spanned_entity(
            "helper",
            "app.py",
            LanguageId::Python,
            20,
            "def helper(data):\n    pass\n",
        );
        let external =
            ExternalReference::new_resolved("kin-scip-v1", "pip python 3.12", "json/dumps().")
                .expect("a resolved external symbol");
        store
            .apply_transaction_delta(&TransactionDelta {
                external_reference_deltas: vec![ExternalReferenceDelta::Added {
                    new: external.clone(),
                }],
                ..TransactionDelta::default()
            })
            .expect("the external symbol is admitted");
        let context = proof_context(LanguageId::Python, "1.1.400");
        let context_id = id_of(&context);
        let mut records = vec![context];
        if with_ledger {
            // Every spanned entity a sweep finishes gets a ledger, a caller
            // with no call an empty one.
            records.push(ledger(&helper, "", context_id, Vec::new()));
            records.push(ledger(
                &run,
                BODY,
                context_id,
                vec![
                    ("helper", CallSiteState::ProvenTarget { target: helper.id }),
                    (
                        "dumps",
                        CallSiteState::ProvenExternal {
                            target: external.id,
                        },
                    ),
                    ("handler", CallSiteState::Binding { may_call: None }),
                    (
                        "mystery",
                        CallSiteState::Unresolved {
                            reason: UnresolvedReason::NoAnswer,
                        },
                    ),
                ],
            ));
        }
        admit(&store, &[&run, &helper], records);
        Fixture {
            store,
            run,
            helper,
            external,
            context: context_id,
        }
    }

    #[test]
    fn non_callable_binding_removes_uncertain_calls_without_weakening_the_strict_audit() {
        let fixture = fixture(false);
        let before = scan_focal(&fixture.store, &fixture.helper, &Body(BODY), NO_CENSUS).unwrap();
        assert!(!before.callers.is_empty());
        let proof = FocalEscape::NonCallable {
            reason: "the selected parser census proves a scalar binding",
            entities_checked: 2,
        };
        let scan = scan_focal(&fixture.store, &fixture.helper, &Body(BODY), proof).unwrap();
        assert!(scan.candidates.is_empty());
        assert!(scan.callers.is_empty());
        assert_eq!(scan.proven_callers, before.proven_callers);
        let block = named_block(&scan);
        assert_eq!(block["settled"], true);
        assert_eq!(block["focal_escape"]["escape"], "non_callable");
        let strict = calls_evidence_from(&fixture.store, &fixture.helper, &scan);
        assert!(!strict.settled);
        assert!(strict.unsettled_because.contains(&CALLERS_WITHOUT_LEDGER));
    }

    #[test]
    fn a_focal_with_a_ledger_serves_one_row_per_site_addressed_inside_the_focal() {
        let fixture = fixture(true);
        let block = focal_block(&fixture.store, &fixture.run, &Body(BODY));
        assert_eq!(block["reading"], "current", "{block}");
        assert_eq!(block["scope"], FOCAL_SCOPE);
        assert_eq!(block["sites"], 4);
        assert_eq!(block["settled"], false);
        assert_eq!(
            block["rows"],
            json!([
                {
                    "line_in_entity": 1,
                    "callee": "helper",
                    "state": "proven_target",
                    "reason": null,
                    "target": format!("entity:{}", fixture.helper.id),
                },
                {
                    "line_in_entity": 2,
                    "callee": "dumps",
                    "state": "proven_external",
                    "reason": null,
                    "target": format!("external_reference:{}", fixture.external.id),
                },
                {
                    "line_in_entity": 3,
                    "callee": "handler",
                    "state": "binding",
                    "reason": null,
                    "target": null,
                },
                {
                    "line_in_entity": 4,
                    "callee": "mystery",
                    "state": "unresolved",
                    "reason": "no_answer",
                    "target": null,
                },
            ]),
            "{block}"
        );
        let text = serde_json::to_string(&block).unwrap();
        assert!(
            !text.contains("start_line") && !text.contains("app.py"),
            "a site is never addressed by file or file line: {block}"
        );
        assert_eq!(
            block["clauses"],
            json!([
                "binding_unproven: 1 of the 4 call sites in the focal's own body call through a \
                 value binding, which proves no target",
                "call_sites_unresolved: 1 of the 4 call sites in the focal's own body got an \
                 answer that proves no target",
            ])
        );
    }

    /// A second definition of `mystery` beside the focal, with an empty
    /// ledger so it adds no owed caller: the name no longer reaches one
    /// definition, so no named site is promoted.
    fn admit_rival(store: &InMemoryGraph, context: ResolutionRecordId) -> Entity {
        let body = "def mystery(other):\n    pass\n";
        let rival = spanned_entity("Other.mystery", "other.py", LanguageId::Python, 0, body);
        admit(
            store,
            &[&rival],
            vec![ledger(&rival, body, context, Vec::new())],
        );
        rival
    }

    fn mystery_focal() -> Entity {
        spanned_entity(
            "mystery",
            "lib.py",
            LanguageId::Python,
            0,
            "def mystery(data):\n    pass\n",
        )
    }

    fn unknown_census() -> FocalEscape {
        FocalEscape::Unknown {
            reason: "dynamic reflective access in the domain",
        }
    }

    /// Holds only exact recorded callee quotations, never an aggregate body.
    struct SiteOnlyText<'a> {
        body: &'a str,
        readable: bool,
    }

    impl SiteText for SiteOnlyText<'_> {
        fn quote(&self, caller: &Entity, site: &SourceSpan) -> Result<String, &'static str> {
            let span = caller.span.as_ref().ok_or("caller_has_no_span")?;
            if !self.readable
                || (site.start_byte == span.start_byte && site.end_byte == span.end_byte)
            {
                return Err("caller_source_unavailable");
            }
            let token = quote_site(caller, site, self.body, span.start_byte)?;
            if token == "unread" || !is_identifier(&token) {
                return Err("caller_source_unavailable");
            }
            Ok(token)
        }
    }

    fn site_only_scan(escape: FocalEscape, readable: bool) -> FocalScan {
        let body = "mystery(); different(); unread();";
        let store = InMemoryGraph::new();
        let mut caller = spanned_entity("app", "app.py", LanguageId::Python, 0, body);
        caller.kind = kin_model::EntityKind::Module;
        caller.signature = "module app.py".into();
        let context = proof_context(LanguageId::Python, "current");
        admit(
            &store,
            &[&caller],
            vec![
                context.clone(),
                ledger(
                    &caller,
                    body,
                    context.id(),
                    vec![
                        (
                            "mystery",
                            CallSiteState::Unresolved {
                                reason: UnresolvedReason::NoAnswer,
                            },
                        ),
                        (
                            "different",
                            CallSiteState::Unresolved {
                                reason: UnresolvedReason::NoAnswer,
                            },
                        ),
                        (
                            "unread",
                            CallSiteState::Unresolved {
                                reason: UnresolvedReason::NoAnswer,
                            },
                        ),
                    ],
                ),
            ],
        );
        admit_rival(&store, context.id());
        scan_focal(
            &store,
            &mystery_focal(),
            &SiteOnlyText { body, readable },
            escape,
        )
        .unwrap()
    }

    #[test]
    fn site_only_text_narrows_contained_callers_without_a_whole_body() {
        use kin_model::call_site_reading::{REACH_CALLEE_SPELLS, REACH_TEXT_UNKNOWN};
        let scan = site_only_scan(FocalEscape::contained(1), true);
        assert_eq!(
            scan.candidates.len(),
            2,
            "known different token is not a contained call; unread remains: {:?}",
            scan.candidates
        );
        assert!(scan
            .candidates
            .iter()
            .any(|site| site.callee.as_deref() == Some("mystery")
                && site.reason == REACH_CALLEE_SPELLS));
        assert!(scan
            .candidates
            .iter()
            .any(|site| site.callee.is_none() && site.reason == REACH_TEXT_UNKNOWN));
        assert_eq!(scan.value_calls.sites, 0);
        assert!(
            scan.promoted.is_empty(),
            "a shared name never becomes a proof"
        );
    }

    #[test]
    fn site_only_text_preserves_escaping_values_and_unread_sites() {
        use kin_model::call_site_reading::{
            REACH_CALLEE_SPELLS, REACH_FOCAL_ESCAPES, REACH_TEXT_UNKNOWN,
        };
        let scan = site_only_scan(unknown_census(), true);
        assert_eq!(
            scan.candidates.len(),
            3,
            "escaping-value possibilities must not be deleted"
        );
        assert!(scan
            .candidates
            .iter()
            .any(|site| site.callee.as_deref() == Some("mystery")
                && site.reason == REACH_CALLEE_SPELLS));
        assert!(scan
            .candidates
            .iter()
            .any(|site| site.callee.as_deref() == Some("different")
                && site.reason == REACH_FOCAL_ESCAPES));
        assert!(scan
            .candidates
            .iter()
            .any(|site| site.callee.is_none() && site.reason == REACH_TEXT_UNKNOWN));
        assert_eq!(scan.value_calls.sites, 1);
        assert_eq!(scan.named.len(), 2);
        assert!(scan.promoted.is_empty());
    }

    #[test]
    fn unread_site_text_never_certifies_absence() {
        use kin_model::call_site_reading::REACH_TEXT_UNKNOWN;
        let scan = site_only_scan(FocalEscape::contained(1), false);
        assert_eq!(scan.candidates.len(), 3);
        assert!(scan
            .candidates
            .iter()
            .all(|site| site.callee.is_none() && site.reason == REACH_TEXT_UNKNOWN));
        assert!(!named_block(&scan)["settled"].as_bool().unwrap());
    }

    #[test]
    fn a_site_whose_callee_spells_the_focal_is_listed_before_escape_only_sites() {
        use kin_model::call_site_reading::{REACH_CALLEE_SPELLS, REACH_FOCAL_ESCAPES};
        let fixture = fixture(true);
        admit_rival(&fixture.store, fixture.context);
        let focal = mystery_focal();
        // A census that cannot rule escape out keeps every unsettled site, and
        // the one whose callee spells the focal's name still comes first.
        let scan = scan_focal(&fixture.store, &focal, &Body(BODY), unknown_census()).unwrap();
        let rows: Vec<(Option<&str>, &str)> = scan
            .candidates
            .iter()
            .map(|candidate| (candidate.callee.as_deref(), candidate.reason))
            .collect();
        assert_eq!(
            rows,
            vec![
                (Some("mystery"), REACH_CALLEE_SPELLS),
                (Some("handler"), REACH_FOCAL_ESCAPES),
            ]
        );
        // The named site is listed, and the value call is counted, not listed.
        let block = named_block(&scan);
        assert_eq!(block["candidate_count"], 1, "{block}");
        assert_eq!(block["candidates"][0]["callee"], "mystery", "{block}");
        assert_eq!(block["value_calls"]["sites"], 1, "{block}");
        assert_eq!(
            block["value_calls"]["by_reason"][REACH_FOCAL_ESCAPES], 1,
            "{block}"
        );
        assert_eq!(
            block["value_calls"]["census"]["escape"], "unknown",
            "{block}"
        );
    }

    /// Confined to the callers a question is about, a reading holds no site
    /// of a caller left out: its unresolved call is in no list, and the
    /// clauses the reading calls for speak of the callers kept. With no
    /// caller kept that holds such a site, nothing is left to list.
    #[test]
    fn a_confined_reading_holds_no_site_of_a_caller_left_out() {
        let store = InMemoryGraph::new();
        let body = "def run():\n    mystery()\n";
        let inside = spanned_entity("run", "pkg/app.py", LanguageId::Python, 0, body);
        let outside = spanned_entity("run_elsewhere", "other/lib.py", LanguageId::Python, 0, body);
        let focal = mystery_focal();
        let context = proof_context(LanguageId::Python, "current");
        let unresolved = || {
            vec![(
                "mystery",
                CallSiteState::Unresolved {
                    reason: UnresolvedReason::NoAnswer,
                },
            )]
        };
        let records = vec![
            context.clone(),
            ledger(&inside, body, context.id(), unresolved()),
            ledger(&outside, body, context.id(), unresolved()),
        ];
        admit(&store, &[&inside, &outside], records);
        admit_rival(&store, context.id());
        let whole = scan_focal(&store, &focal, &Body(body), unknown_census()).unwrap();
        assert_eq!(whole.named.len(), 2);
        let leftovers = |scan: &FocalScan| {
            named_clauses(scan)
                .iter()
                .any(|clause| clause_code(clause) == kin_model::call_site_reading::NAMED_LEFTOVERS)
        };
        assert!(leftovers(&whole));

        // Every caller kept: the reading is as it was.
        let mut kept = whole.clone();
        assert_eq!(kept.confine(|_| true), 0);
        assert_eq!(json!(kept.named), json!(whole.named));

        // One caller kept: its site alone is listed.
        let mut confined = whole.clone();
        assert_eq!(
            confined.confine(|caller| caller.id == inside.id),
            whole.callers.len() - 1
        );
        for sites in [&confined.named, &confined.candidates] {
            assert_eq!(sites.len(), 1);
            assert_eq!(sites[0].caller, inside.id);
        }
        assert_eq!(confined.callers.len(), 1);
        let block = named_block(&confined);
        assert_eq!(block["candidate_count"], 1, "{block}");
        assert!(leftovers(&confined));
        assert!(whole
            .callers
            .iter()
            .any(|scanned| scanned.entity.id == outside.id));
        assert!(confined
            .callers
            .iter()
            .all(|scanned| scanned.entity.id != outside.id));

        // No caller kept: nothing is listed, and no clause says something is.
        let mut none = whole.clone();
        assert_eq!(none.confine(|_| false), whole.callers.len());
        assert!(none.named.is_empty() && none.candidates.is_empty());
        assert!(!leftovers(&none));
        assert_eq!(named_block(&none)["candidate_count"], 0);
    }

    #[test]
    fn named_leftovers_are_listed_in_full_at_every_size() {
        for site_count in [0, 3, 5, 613] {
            let store = InMemoryGraph::new();
            let body = format!("def run():\n{}", "    mystery()\n".repeat(site_count));
            let caller = spanned_entity("run", "app.py", LanguageId::Python, 0, &body);
            let focal = mystery_focal();
            let context = proof_context(LanguageId::Python, "current");
            let records = vec![
                context.clone(),
                ledger(
                    &caller,
                    &body,
                    context.id(),
                    (0..site_count)
                        .map(|_| {
                            (
                                "mystery",
                                CallSiteState::Unresolved {
                                    reason: UnresolvedReason::NoAnswer,
                                },
                            )
                        })
                        .collect(),
                ),
            ];
            admit(&store, &[&caller], records);
            admit_rival(&store, context.id());
            let scan = scan_focal(&store, &focal, &Body(&body), unknown_census()).unwrap();
            assert_eq!(scan.candidates.len(), site_count);
            assert_eq!(scan.named.len(), site_count);
            let block = named_block(&scan);
            // Every named leftover is served, never a sample. Its call as
            // written rides within the list's byte budget, and past it the
            // row keeps its address and the list counts what it dropped.
            let per_row = "expr".len() + 4 + "\"mystery()\"".len();
            let served = site_count.min(site_expression::NAMED_EXPR_BYTES_MAX / per_row);
            let mut expected = json!(&scan.named);
            for row in expected.as_array_mut().unwrap().iter_mut().skip(served) {
                row.as_object_mut().unwrap().remove("expr");
            }
            assert_eq!(block["candidates"], expected, "{site_count}");
            assert_eq!(
                block["candidates"][0]["expr"],
                json!(scan.named.first().map(|_| "mystery()"))
            );
            if site_count > served {
                assert_eq!(
                    block[NAMED_EXPR_ELIDED_KEY]["served"], served,
                    "{site_count}"
                );
                assert_eq!(block[NAMED_EXPR_ELIDED_KEY]["elided"], site_count - served);
                let added: usize = block["candidates"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|row| row.get("expr"))
                    .map(|expr| "expr".len() + 4 + expr.to_string().len())
                    .sum();
                assert!(added <= site_expression::NAMED_EXPR_BYTES_MAX, "{added}");
            } else {
                assert!(block.get(NAMED_EXPR_ELIDED_KEY).is_none(), "{site_count}");
            }
            assert_eq!(block["candidate_count"], site_count);
            assert!(block.get("candidates_withheld").is_none(), "{site_count}");
            assert!(block.get("value_calls").is_none(), "{site_count}");
            let clauses: Vec<&str> = block["clauses"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(Value::as_str)
                .collect();
            if site_count > 0 {
                assert_eq!(block["settled"], false);
                assert_eq!(block["sites"], site_count);
                // One residue clause, never the per-state code it replaces.
                assert_eq!(clauses.len(), 1, "{clauses:?}");
                assert!(clauses[0].starts_with("named_leftovers: "), "{clauses:?}");
                assert!(
                    clauses[0].contains(&format!("{site_count} call site(s)")),
                    "{clauses:?}"
                );
            } else {
                assert_eq!(block["settled"], true, "{block}");
                assert!(clauses.is_empty(), "{clauses:?}");
            }
            assert_eq!(block["named_complete"], true, "{block}");
        }
    }

    #[test]
    fn settled_sites_count_only_when_they_call_the_focal() {
        let fixture = fixture(true);
        // `run` calls `helper`, `dumps`, a binding and `mystery`. Read for
        // `helper` under a census that proves it contained, only the proven
        // call to `helper` is in scope: the call to `dumps` is settled
        // elsewhere, and neither unsettled site can be a call to `helper`.
        let scan = scan_focal(
            &fixture.store,
            &fixture.helper,
            &Body(BODY),
            FocalEscape::contained(2),
        )
        .unwrap();
        assert_eq!(scan.settled_to_focal, 1);
        let block = named_block(&scan);
        assert_eq!(block["sites"], 1, "{block}");
        assert_eq!(block["settled_to_focal"], 1, "{block}");
        assert_eq!(block["settled"], true, "{block}");
        assert_eq!(block["named_complete"], true, "{block}");
        assert_eq!(block["clauses"], json!([]), "{block}");
        assert!(block.get("value_calls").is_none(), "{block}");
    }

    #[test]
    fn a_name_one_definition_carries_is_promoted_and_never_listed_twice() {
        let fixture = fixture(true);
        let focal = mystery_focal();
        let scan = scan_focal(&fixture.store, &focal, &Body(BODY), unknown_census()).unwrap();
        assert_eq!(
            scan.name_unique,
            NameUnique::Promoted {
                name: "mystery".to_string()
            }
        );
        assert_eq!(scan.promoted.len(), 1);
        assert!(scan.named.is_empty());
        let callers = scan.promoted_callers();
        assert_eq!(callers.len(), 1);
        assert_eq!(callers[0].0.id, fixture.run.id);
        assert_eq!(callers[0].1[0].line_in_entity, Some(4));
        let block = named_block(&scan);
        assert_eq!(block["candidate_count"], 0, "{block}");
        assert_eq!(block["name_unique"]["promoted"], 1, "{block}");
        assert_eq!(
            block["name_unique"]["reading"]["state"], "promoted",
            "{block}"
        );
        // A promoted site is not proven, so it is still residue.
        assert!(
            block["clauses"]
                .as_array()
                .unwrap()
                .iter()
                .any(|clause| clause
                    .as_str()
                    .is_some_and(|text| text.starts_with("named_leftovers: 1 call site(s)")
                        && text.contains("1 are served as `name_unique`"))),
            "{block}"
        );
    }

    #[test]
    fn a_shared_name_is_never_promoted() {
        let fixture = fixture(true);
        admit_rival(&fixture.store, fixture.context);
        let scan = scan_focal(
            &fixture.store,
            &mystery_focal(),
            &Body(BODY),
            unknown_census(),
        )
        .unwrap();
        assert_eq!(
            scan.name_unique,
            NameUnique::NotApplied {
                reason: UNIQUE_SHARED_DEFINITION
            }
        );
        assert!(scan.promoted.is_empty());
        assert_eq!(scan.named.len(), 1);
    }

    #[test]
    fn a_name_a_settled_site_proves_elsewhere_is_never_promoted() {
        let store = InMemoryGraph::new();
        let body = "def run():\n    a.mystery()\n    b.mystery()\n";
        let caller = spanned_entity("run", "app.py", LanguageId::Python, 0, body);
        let context = proof_context(LanguageId::Python, "current");
        let records = vec![
            context.clone(),
            ledger(
                &caller,
                body,
                context.id(),
                vec![
                    ("mystery", CallSiteState::ProvenOutside),
                    (
                        "mystery",
                        CallSiteState::Unresolved {
                            reason: UnresolvedReason::NoAnswer,
                        },
                    ),
                ],
            ),
        ];
        admit(&store, &[&caller], records);
        let scan = scan_focal(&store, &mystery_focal(), &Body(body), unknown_census()).unwrap();
        assert_eq!(
            scan.name_unique,
            NameUnique::NotApplied {
                reason: UNIQUE_CONTESTED
            }
        );
        assert_eq!(scan.named.len(), 1);
        assert!(scan.promoted.is_empty());
    }

    #[test]
    fn a_binding_site_is_never_promoted() {
        let store = InMemoryGraph::new();
        let body = "def run():\n    mystery()\n";
        let caller = spanned_entity("run", "app.py", LanguageId::Python, 0, body);
        let context = proof_context(LanguageId::Python, "current");
        let records = vec![
            context.clone(),
            ledger(
                &caller,
                body,
                context.id(),
                vec![("mystery", CallSiteState::Binding { may_call: None })],
            ),
        ];
        admit(&store, &[&caller], records);
        let scan = scan_focal(&store, &mystery_focal(), &Body(body), unknown_census()).unwrap();
        assert_eq!(
            scan.name_unique,
            NameUnique::NotApplied {
                reason: UNIQUE_NOTHING_TO_PROMOTE
            }
        );
        assert_eq!(scan.named.len(), 1);
        assert_eq!(scan.named[0].state, SiteStateKind::Binding);
    }

    #[test]
    fn a_symbol_outside_the_repository_that_carries_the_name_stops_promotion() {
        let fixture = fixture(true);
        let external = ExternalReference::new_resolved(
            "kin-scip-v1",
            "pip python 3.12",
            "lib/Thing#mystery().",
        )
        .expect("a resolved external symbol");
        fixture
            .store
            .apply_transaction_delta(&TransactionDelta {
                external_reference_deltas: vec![ExternalReferenceDelta::Added { new: external }],
                ..TransactionDelta::default()
            })
            .expect("the external symbol is admitted");
        let scan = scan_focal(
            &fixture.store,
            &mystery_focal(),
            &Body(BODY),
            unknown_census(),
        )
        .unwrap();
        assert_eq!(
            scan.name_unique,
            NameUnique::NotApplied {
                reason: UNIQUE_EXTERNAL_SYMBOL
            }
        );
        assert!(scan.promoted.is_empty());
    }

    /// `flask.app._make_timedelta`: `class App` in `sansio/app.py` spells
    /// the name (its own twin) and calls `ConfigAttribute[timedelta](...)`,
    /// whose callee is a subscript with a computed key. The census judged
    /// every lookup by a computed name in the domain and listed none that
    /// could fetch the focal, so the call is no value call of it.
    #[test]
    fn a_contained_focal_leaves_calls_through_computed_lookups_to_the_census() {
        const CALL: &str = "ConfigAttribute[timedelta](\"X\", get_converter=_make_timedelta)";
        let app = format!("class App:\n    permanent = {CALL}\n");
        let store = InMemoryGraph::new();
        let caller = spanned_entity("App", "sansio/app.py", LanguageId::Python, 0, &app);
        let focal = spanned_entity(
            "_make_timedelta",
            "app.py",
            LanguageId::Python,
            500,
            "def _make_timedelta(value):\n    return value\n",
        );
        let context = proof_context(LanguageId::Python, "1.1.400");
        let context_id = id_of(&context);
        admit(
            &store,
            &[&caller, &focal],
            vec![
                context,
                ledger(&focal, "", context_id, Vec::new()),
                ledger(
                    &caller,
                    &app,
                    context_id,
                    vec![(
                        CALL,
                        CallSiteState::Unresolved {
                            reason: UnresolvedReason::CalleeNotPlaceable,
                        },
                    )],
                ),
            ],
        );
        let contained = scan_focal(&store, &focal, &Body(&app), FocalEscape::contained(2)).unwrap();
        assert!(
            contained.value_calls.is_empty(),
            "{:?}",
            contained.value_calls
        );
        assert!(contained.named.is_empty());
        let block = named_block(&contained);
        assert!(block.get("value_calls").is_none(), "{block}");
        assert_eq!(block["settled"], true, "{block}");
        // With no census the call may hold the focal as a value.
        let unknown = scan_focal(&store, &focal, &Body(&app), NO_CENSUS).unwrap();
        assert_eq!(unknown.value_calls.sites, 1);
        // Without the caller's text the call may be a literal lookup that
        // spells a name, so it stays.
        let untexted = scan_focal(&store, &focal, &NoSiteText, FocalEscape::contained(2)).unwrap();
        assert_eq!(untexted.value_calls.sites, 1);
    }

    /// A dead-code reading: nothing calls the focal. Lookups by a name no
    /// record follows are counted in one clause of their own, and a lookup
    /// whose key names the focal (here `getattr(module, "_make_timedelta")`,
    /// which an adapter keeps as the key's literal) is listed residue a reader
    /// must check. Neither settles the block, so neither licenses an absence.
    #[test]
    fn a_dead_code_reading_counts_unkeyed_lookups_and_lists_a_keyed_one() {
        use kin_model::{EscapeResidue, ResidueSite};
        const CALL: &str = "ConfigAttribute[timedelta](\"X\", get_converter=_make_timedelta)";
        let app = format!("class App:\n    permanent = {CALL}\n");
        let store = InMemoryGraph::new();
        let caller = spanned_entity("App", "sansio/app.py", LanguageId::Python, 0, &app);
        let focal = spanned_entity(
            "_make_timedelta",
            "app.py",
            LanguageId::Python,
            500,
            "def _make_timedelta(value):\n    return value\n",
        );
        let context = proof_context(LanguageId::Python, "1.1.400");
        let context_id = id_of(&context);
        admit(
            &store,
            &[&caller, &focal],
            vec![
                context,
                ledger(&focal, "", context_id, Vec::new()),
                ledger(
                    &caller,
                    &app,
                    context_id,
                    vec![(
                        CALL,
                        CallSiteState::Unresolved {
                            reason: UnresolvedReason::CalleeNotPlaceable,
                        },
                    )],
                ),
            ],
        );
        let site = |line: u32, excerpt: &str| ResidueSite {
            file: "cli.py".into(),
            line,
            kind: "computed_member",
            name: None,
            detail: Some(excerpt.into()),
        };
        let unkeyed = || vec![site(49, "app = getattr(module, attr_name, None)")];

        // Only unkeyed lookups: one counted clause, the block unsettled.
        let only_unkeyed = FocalEscape::Contained {
            entities_checked: 2,
            residue: EscapeResidue {
                unkeyed_lookups: unkeyed(),
                ..EscapeResidue::default()
            },
        };
        let scan = scan_focal(&store, &focal, &Body(&app), only_unkeyed).unwrap();
        assert!(scan.value_calls.is_empty(), "{:?}", scan.value_calls);
        let clauses = named_clauses(&scan);
        assert_eq!(clauses.len(), 1, "{clauses:?}");
        assert!(
            clauses[0].starts_with("unkeyed_lookups_counted: 1 lookups"),
            "{clauses:?}"
        );
        assert!(escape_residue_clause(&scan.escape).is_none());
        let block = named_block(&scan);
        assert_eq!(block["settled"], false, "{block}");
        assert_eq!(
            block["focal_escape"]["residue"]["unkeyed_lookups"]["count"], 1,
            "{block}"
        );
        let evidence = calls_evidence_from(&store, &focal, &scan);
        assert!(!evidence.settled);
        assert!(evidence
            .unsettled_because
            .contains(&UNKEYED_LOOKUPS_COUNTED));
        assert!(!evidence.unsettled_because.contains(&ESCAPE_RESIDUE_LISTED));

        // A lookup whose key names the focal stays a listed site.
        let keyed = FocalEscape::Contained {
            entities_checked: 2,
            residue: EscapeResidue {
                reflective: vec![site(80, "getattr(module, \"_make_timedelta\")")],
                unkeyed_lookups: unkeyed(),
                ..EscapeResidue::default()
            },
        };
        let scan = scan_focal(&store, &focal, &Body(&app), keyed).unwrap();
        let clauses = named_clauses(&scan);
        assert_eq!(clauses.len(), 2, "{clauses:?}");
        assert!(
            clauses[0].starts_with("escape_residue_listed: 1 sites"),
            "{clauses:?}"
        );
        assert!(
            clauses[1].starts_with("unkeyed_lookups_counted: 1 lookups"),
            "{clauses:?}"
        );
        let block = named_block(&scan);
        assert_eq!(block["settled"], false, "{block}");
        assert_eq!(
            block["focal_escape"]["residue"]["reflective"]["sites"][0]["line"], 80,
            "{block}"
        );
    }

    #[test]
    fn value_calls_are_counted_once_with_the_census_reason() {
        let fixture = fixture(true);
        admit_rival(&fixture.store, fixture.context);
        let scan = scan_focal(
            &fixture.store,
            &mystery_focal(),
            &Body(BODY),
            unknown_census(),
        )
        .unwrap();
        assert_eq!(scan.value_calls.sites, 1);
        assert_eq!(scan.value_calls.callers, 1);
        let clauses = named_clauses(&scan);
        assert_eq!(clauses.len(), 2, "{clauses:?}");
        assert!(
            clauses[0].starts_with("named_leftovers: 1 call site(s)"),
            "{clauses:?}"
        );
        assert!(
            clauses[1]
                .starts_with("value_calls_unmeasured: 1 unsettled call site(s) in 1 caller(s)")
                && clauses[1].contains("dynamic reflective access in the domain"),
            "{clauses:?}"
        );
        for clause in &clauses {
            assert!(
                !clause.contains(crate::verdict::CLAUSE_SEPARATOR),
                "{clause}"
            );
        }
        // The binding that is only a value call no longer reads as its own code.
        assert!(
            !clauses
                .iter()
                .any(|clause| clause.starts_with("binding_unproven")),
            "{clauses:?}"
        );
    }

    #[test]
    fn a_named_caller_still_owed_its_sites_keeps_the_scope_open() {
        let fixture = fixture(false);
        let scan = scan_focal(
            &fixture.store,
            &mystery_focal(),
            &Body(BODY),
            unknown_census(),
        )
        .unwrap();
        assert!(!scan.named_complete());
        let block = named_block(&scan);
        assert_eq!(block["named_complete"], false, "{block}");
        assert!(
            block["clauses"]
                .as_array()
                .unwrap()
                .iter()
                .any(|clause| clause
                    .as_str()
                    .is_some_and(|text| text.starts_with("call_sites_owed"))),
            "an owed caller's sites were never listed, so it stays a gap: {block}"
        );
    }

    #[test]
    fn a_terminal_lists_the_named_candidates_and_counts_the_rest() {
        let fixture = fixture(true);
        let scan = scan_focal(
            &fixture.store,
            &mystery_focal(),
            &Body(BODY),
            unknown_census(),
        )
        .unwrap();
        assert_eq!(
            candidate_lines(&scan, "mystery"),
            vec![
                "Unproven call sites that could call mystery: 2.".to_string(),
                "  1 name mystery, and the store holds no other definition that name can reach, \
                 so each is a name_unique reference:"
                    .to_string(),
                "    run, line 4 in it: mystery(data), unresolved (no answer)".to_string(),
                "  1 more could reach it only if mystery is held as a value or looked up by a \
                 computed name, and the escape census could not rule that out (dynamic \
                 reflective access in the domain)."
                    .to_string(),
            ]
        );
        admit_rival(&fixture.store, fixture.context);
        let scan = scan_focal(
            &fixture.store,
            &mystery_focal(),
            &Body(BODY),
            unknown_census(),
        )
        .unwrap();
        assert_eq!(
            candidate_lines(&scan, "mystery")[1..3],
            [
                "  1 name mystery, so check them first:".to_string(),
                "    run, line 4 in it: mystery(data), unresolved (no answer)".to_string(),
            ]
        );
    }

    /// A candidate row is addressed by its caller's id and its line in that
    /// caller. Its file rides only as the labelled projection every reference
    /// row serves, never as a bare path.
    #[test]
    fn a_candidate_row_serves_its_file_as_a_projection() {
        let fixture = fixture(true);
        admit_rival(&fixture.store, fixture.context);
        // The module the callers' unit declares, which is how a row places them.
        let mut module = spanned_entity("app", "app.py", LanguageId::Python, 0, BODY);
        module.kind = kin_model::EntityKind::Module;
        fixture.store.upsert_entity(&module).unwrap();
        let focal = spanned_entity(
            "mystery",
            "lib.py",
            LanguageId::Python,
            0,
            "def mystery(data):\n    pass\n",
        );
        let escape = FocalEscape::Unknown {
            reason: "dynamic reflective access in the domain",
        };
        let scan = scan_focal(&fixture.store, &focal, &Body(BODY), escape).unwrap();
        let block = named_block(&scan);
        let rows = block["candidates"].as_array().expect("candidate rows");
        assert!(!rows.is_empty(), "{block}");
        let mut placed = 0;
        for row in rows {
            assert!(row.get("caller_file").is_none(), "{row}");
            assert!(row.get("file_path").is_none(), "{row}");
            assert!(
                row["projection"]["path"].is_string(),
                "the caller's file is a labelled projection: {row}"
            );
            if row["projection"]["path"] == "app.py" {
                assert_eq!(row["module"], "app", "a row is placed by its module: {row}");
                placed += 1;
            }
            assert!(row["caller"].is_string(), "{row}");
        }
        assert!(
            placed > 0,
            "no candidate sat in the module the test placed: {block}"
        );
    }

    #[test]
    fn a_spanned_focal_with_no_ledger_reads_as_owed_enrichment() {
        let fixture = fixture(false);
        let block = focal_block(&fixture.store, &fixture.run, &Body(BODY));
        assert_eq!(block["reading"], "owed_enrichment", "{block}");
        assert_eq!(block["callers_owed_enrichment"], 1);
        assert_eq!(block["sites"], 0);
        assert_eq!(block["rows"], json!([]));
        assert_eq!(block["settled"], false);
        let clauses = block["clauses"].as_array().expect("clauses");
        assert_eq!(clauses.len(), 1, "{block}");
        assert!(
            clauses[0]
                .as_str()
                .is_some_and(|clause| clause.starts_with("call_sites_owed: ")),
            "{block}"
        );

        let mut spanless = fixture.run.clone();
        spanless.span = None;
        let block = focal_block(&fixture.store, &spanless, &Body(BODY));
        assert_eq!(block["reading"], "no_sites", "{block}");
        assert_eq!(
            block["settled"], true,
            "a focal with no text holds no site: {block}"
        );
    }

    fn set_validation(store: &InMemoryGraph, state: Option<kin_model::ContextValidationState>) {
        use kin_model::{EntityStore, ResolutionRecord, ResolutionRecordDelta};
        let id = ResolutionRecordId::context_validation(LanguageId::Python);
        let old = store.lookup_resolution_record(&id).unwrap();
        let new = state.map(|state| {
            ResolutionRecord::ContextValidation(kin_model::ContextValidation {
                language: LanguageId::Python,
                state,
            })
        });
        let delta = match (old, new) {
            (Some(old), Some(new)) => ResolutionRecordDelta::Modified { old, new },
            (None, Some(new)) => ResolutionRecordDelta::Added { new },
            (Some(old), None) => ResolutionRecordDelta::Removed { old },
            (None, None) => return,
        };
        store
            .apply_transaction_delta(&kin_model::TransactionDelta {
                resolution_record_deltas: vec![delta],
                ..Default::default()
            })
            .unwrap();
    }

    #[test]
    fn context_validation_selected_graph_controls_stale_and_current_readings() {
        let fixture = fixture(true);
        let newer = proof_context(LanguageId::Python, "1.1.401");
        set_validation(
            &fixture.store,
            Some(kin_model::ContextValidationState::Validated {
                context: newer.as_proof_context().unwrap().clone(),
            }),
        );
        let block = focal_block(&fixture.store, &fixture.run, &Body(BODY));
        assert_eq!(block["reading"], "proof_context_stale", "{block}");
        assert_eq!(block["callers_stale"], 1);
        assert_eq!(block["by_state"]["proof_context_stale"], 4);
        assert_eq!(block["rows"][0]["recorded_state"], "proven_target");
        assert_eq!(block["stale_context"], fixture.context.to_string());

        let historical = self::fixture(true);
        // Publishing another resolver's context cannot change this selected graph.
        publish_current_proof_contexts(HashMap::from([(LanguageId::Python, newer.id())]));
        let block = focal_block(&historical.store, &historical.run, &Body(BODY));
        publish_current_proof_contexts(HashMap::new());
        assert_eq!(block["reading"], "current", "{block}");
    }

    #[test]
    fn context_validation_missing_or_unverified_survives_reopen_and_preserves_proof() {
        for state in [
            None,
            Some(kin_model::ContextValidationState::Unverified {
                reason: "server could not start; validation not completed".into(),
            }),
        ] {
            let fixture = fixture(true);
            set_validation(&fixture.store, state.clone());
            let reopened = InMemoryGraph::from_snapshot(fixture.store.to_snapshot()).unwrap();
            let block = focal_block(&reopened, &fixture.run, &Body(BODY));
            assert_eq!(block["reading"], "proof_context_unverified", "{block}");
            assert_eq!(block["settled"], false);
            assert_eq!(block["callers_unverified"], 1);
            assert_eq!(block["by_state"]["proof_context_unverified"], 4);
            assert_eq!(block["rows"][0]["recorded_state"], "proven_target");
            assert!(!block["rows"][0]["target"].is_null());
            assert_eq!(block["unverified_context"], fixture.context.to_string());
            assert!(block["clauses"][0]
                .as_str()
                .unwrap()
                .starts_with("proof_context_unverified:"));
            assert!(!block["clauses"][0].as_str().unwrap().contains("; "));
            if state.is_some() {
                assert_eq!(
                    block["validation_reason"],
                    "server could not start; validation not completed"
                );
            } else {
                assert!(block["validation_reason"]
                    .as_str()
                    .unwrap()
                    .contains("no recorded"));
            }
            assert!(!owed_files(&reopened, std::slice::from_ref(&fixture.run)).is_empty());
            let good = self::fixture(true);
            let good_reopened = InMemoryGraph::from_snapshot(good.store.to_snapshot()).unwrap();
            assert_eq!(
                focal_block(&good_reopened, &good.run, &Body(BODY))["reading"],
                "current"
            );
        }
    }

    #[test]
    fn the_store_block_s_shares_add_up_to_its_census_and_name_the_owed_files() {
        let fixture = fixture(true);
        let owed = spanned_entity(
            "later",
            "lib/tools.py",
            LanguageId::Python,
            0,
            "def later():\n    pass\n",
        );
        admit(&fixture.store, &[&owed], Vec::new());
        let block = store_block(&fixture.store).expect("the store reads");
        assert_eq!(block["scope"], STORE_SCOPE);
        assert_eq!(block["census"], 4, "{block}");
        assert_eq!(block["callers_owed"], 1, "{block}");
        let shares = block["shares"].as_object().expect("shares");
        let total: u64 = shares
            .values()
            .map(|share| share["sites"].as_u64().unwrap_or(0))
            .sum();
        assert_eq!(total, 4, "the shares add up to the census: {block}");
        assert_eq!(shares["proven_target"]["share"], 0.25);
        assert_eq!(shares["unresolved"]["sites"], 1);
        assert_eq!(
            block["owed_files"],
            json!([{"file": "lib/tools.py", "callers": 1}]),
            "{block}"
        );
        assert_eq!(block["settled"], false);

        let lines = text_lines(&block);
        assert!(
            lines
                .iter()
                .any(|line| line.contains("proven_target 1 (25%)")),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("owed enrichment: lib/tools.py (1 caller(s))")),
            "{lines:?}"
        );
    }

    #[test]
    fn a_focal_block_renders_as_lines_addressed_inside_the_focal() {
        let fixture = fixture(true);
        let lines = text_lines(&focal_block(&fixture.store, &fixture.run, &Body(BODY)));
        assert!(
            lines.contains(&format!(
                "  +1 `helper` proven_target -> entity:{}",
                fixture.helper.id
            )),
            "{lines:?}"
        );
        assert!(
            lines.contains(&"  +4 `mystery` unresolved (no_answer)".to_string()),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with("  not settled: call_sites_unresolved: ")),
            "{lines:?}"
        );
    }

    /// Each caller's own body, keyed by the caller, the way the answer's body
    /// reader hands them over, counting how often each caller is read. A
    /// caller it holds no body for reads as unavailable.
    struct Bodies {
        bodies: HashMap<EntityId, String>,
        reads: std::cell::RefCell<HashMap<EntityId, usize>>,
    }

    impl SiteText for Bodies {
        fn quote(&self, caller: &Entity, site: &SourceSpan) -> Result<String, &'static str> {
            let span = caller.span.as_ref().ok_or("caller_has_no_span")?;
            let body = self
                .bodies
                .get(&caller.id)
                .ok_or("caller_source_unavailable")?;
            *self.reads.borrow_mut().entry(caller.id).or_default() += 1;
            quote_site(caller, site, body, span.start_byte)
        }
    }

    /// A caller whose body starts at graph line `start_line` of `file`.
    fn caller_at(
        name: &str,
        file: &str,
        language: LanguageId,
        start_line: u32,
        body: &str,
    ) -> Entity {
        let mut entity = spanned_entity(name, file, language, 0, body);
        let span = entity.span.as_mut().unwrap();
        span.start_line = start_line;
        span.end_line = start_line + body.matches('\n').count() as u32;
        entity
    }

    /// Record, the way an adapter does, what `binding` holds in `caller`.
    fn bind(caller: &mut Entity, binding: &str, origin: Value) {
        caller.metadata.extra.insert(
            kin_parser::value_origins::VALUE_ORIGINS_KEY.into(),
            json!({
                "version": kin_parser::value_origins::VALUE_ORIGINS_VERSION,
                "bindings": { binding: origin },
            }),
        );
    }

    /// One caller of a fixture: the entity, its body, the tokens its ledger
    /// holds an unresolved site for in body order, and whether the body
    /// reader holds its body.
    struct Caller {
        entity: Entity,
        body: &'static str,
        tokens: Vec<&'static str>,
        readable: bool,
    }

    fn caller(entity: Entity, body: &'static str, tokens: Vec<&'static str>) -> Caller {
        Caller {
            entity,
            body,
            tokens,
            readable: true,
        }
    }

    /// Admit `callers` beside a rival definition of the focal's call name (so
    /// no site is promoted), each with a current ledger, and a body reader
    /// holding every readable caller's body.
    fn admit_callers(
        language: LanguageId,
        rival: (&str, &str),
        callers: &[Caller],
    ) -> (InMemoryGraph, Bodies) {
        let store = InMemoryGraph::new();
        let rival_body = "function rival() {}\n";
        let rival = spanned_entity(rival.0, rival.1, language, 5_000, rival_body);
        let context = proof_context(language, "current");
        let mut records = vec![
            context.clone(),
            ledger(&rival, rival_body, context.id(), Vec::new()),
        ];
        for caller in callers {
            records.push(ledger(
                &caller.entity,
                caller.body,
                context.id(),
                caller
                    .tokens
                    .iter()
                    .map(|token| {
                        (
                            *token,
                            CallSiteState::Unresolved {
                                reason: UnresolvedReason::NoAnswer,
                            },
                        )
                    })
                    .collect(),
            ));
        }
        let mut entities: Vec<&Entity> = callers.iter().map(|caller| &caller.entity).collect();
        entities.push(&rival);
        admit(&store, &entities, records);
        let text = Bodies {
            bodies: callers
                .iter()
                .filter(|caller| caller.readable)
                .map(|caller| (caller.entity.id, caller.body.to_string()))
                .collect(),
            reads: std::cell::RefCell::new(HashMap::new()),
        };
        (store, text)
    }

    /// Flask's `AppContext.pop` and callers shaped like the ones its tests
    /// hold: calls through untyped values, through a value the caller
    /// constructs as the owner and writes over several lines, through a
    /// literal and a builtin, and one whose body the reader cannot read. A
    /// census lists a value use, a reflective lookup and a binding.
    fn flask_pop() -> (InMemoryGraph, Entity, Bodies, FocalEscape) {
        use kin_model::value_escape::{EscapeResidue, ResidueSite};
        let py = LanguageId::Python;
        let focal = spanned_entity(
            "AppContext.pop",
            "src/flask/ctx.py",
            py,
            0,
            "def pop(self, exc=None):\n    pass\n",
        );
        let manual_body =
            "def test_manual_pop(app):\n    ctx = AppContext(app)\n    ctx.push()\n    \
                           ctx.pop(\n        None,  # no error (yet)\n    )\n";
        let mut manual = caller_at(
            "test_manual_pop",
            "tests/test_appctx.py",
            py,
            40,
            manual_body,
        );
        bind(
            &mut manual,
            "ctx",
            json!({"call": {"callee": "AppContext"}}),
        );
        let cookie_body = "def test_max_cookie_size(app, client, recwarn):\n    \
                           app.config[\"SESSION_COOKIE_DOMAIN\"] = \".localhost\"\n    \
                           w = recwarn.pop()\n    assert \"too large\" in str(w.message)\n";
        let lang_body = "def pull_lang_code(endpoint, values):\n    \
                         flask.g.lang_code = values.pop(\"lang_code\", None)\n";
        let teardown_body =
            "def app_teardown1(e=None):\n    assert flask.g.seen.pop() == \"app_1\"\n";
        let literal_body = "def test_literals():\n    [1, 2].pop()\n    dict(a=1).pop(\"a\")\n    \
                            \"abc\".strip()\n";
        let unreadable_body = "def test_unreadable(stack):\n    stack.pop()\n";
        let session_body = "def test_session_special_types(app, client):\n    \
                            expect_exception(flask.session.pop, \"foo\")\n";
        let check_body = "def check_method(self, method):\n        \
                          base_method = getattr(Flask, method.__name__)  # the base\n";
        let mut unreadable = caller(
            caller_at(
                "test_unreadable",
                "tests/test_unreadable.py",
                py,
                7,
                unreadable_body,
            ),
            unreadable_body,
            vec!["pop"],
        );
        unreadable.readable = false;
        let callers = vec![
            caller(manual, manual_body, vec!["push", "pop"]),
            caller(
                caller_at(
                    "test_max_cookie_size",
                    "tests/test_basic.py",
                    py,
                    1930,
                    cookie_body,
                ),
                cookie_body,
                vec!["pop"],
            ),
            caller(
                caller_at(
                    "test_url_processors.<locals>.pull_lang_code",
                    "tests/test_basic.py",
                    py,
                    1653,
                    lang_body,
                ),
                lang_body,
                vec!["pop"],
            ),
            caller(
                caller_at(
                    "test_nested_callback_order.<locals>.app_teardown1",
                    "tests/test_blueprints.py",
                    py,
                    919,
                    teardown_body,
                ),
                teardown_body,
                vec!["pop"],
            ),
            caller(
                caller_at(
                    "test_literals",
                    "tests/test_literals.py",
                    py,
                    3,
                    literal_body,
                ),
                literal_body,
                vec!["pop", "pop", "strip"],
            ),
            unreadable,
            // The census's residue sits in these two.
            caller(
                caller_at(
                    "test_session_special_types",
                    "tests/test_basic.py",
                    py,
                    391,
                    session_body,
                ),
                session_body,
                vec![],
            ),
            caller(
                caller_at(
                    "Flask.check_method",
                    "src/flask/app.py",
                    py,
                    273,
                    check_body,
                ),
                check_body,
                vec![],
            ),
        ];
        let (store, text) = admit_callers(py, ("RequestContext.pop", "src/flask/ctx.py"), &callers);
        let residue_site =
            |file: &str, line: u32, kind, name: Option<&str>, detail: &str| ResidueSite {
                file: file.to_string(),
                line,
                kind,
                name: name.map(str::to_string),
                detail: Some(detail.to_string()),
            };
        let escape = FocalEscape::Escapes {
            reason: "a use of its name holds it as a value: see residue.values",
            residue: EscapeResidue {
                values: vec![residue_site(
                    "tests/test_basic.py",
                    393,
                    "argument",
                    Some("pop"),
                    "flask.session",
                )],
                bindings: vec![residue_site(
                    "src/flask/sessions.py",
                    96,
                    "binding",
                    Some("pop"),
                    "",
                )],
                reflective: vec![residue_site(
                    "src/flask/app.py",
                    275,
                    "computed_member",
                    None,
                    "getattr(Flask, method.__name__)",
                )],
                ..EscapeResidue::default()
            },
        };
        (store, focal, text, escape)
    }

    /// A named leftover made through a receiver says which link of the chain
    /// typing it the graph lacks: a receiver no fact binds lacks its binding,
    /// a field chain is no binding at all, and a bound receiver whose file
    /// carries no certificate says nothing rather than guess.
    #[test]
    fn named_leftovers_name_the_missing_link_of_their_receiver_chain() {
        let (store, focal, text, escape) = flask_pop();
        let scan = scan_focal(&store, &focal, &text, escape).unwrap();
        let missing = |caller: &str| -> Vec<Option<Value>> {
            scan.named
                .iter()
                .filter(|row| row.caller_name == caller)
                .map(|row| row.receiver_binding_missing.clone())
                .collect()
        };
        assert_eq!(
            missing("test_max_cookie_size"),
            vec![Some(json!({
                "step": "binding",
                "name": "recwarn",
                "reason": "no_value_origin_fact",
            }))]
        );
        assert_eq!(
            missing("test_nested_callback_order.<locals>.app_teardown1"),
            vec![Some(json!({
                "step": "binding",
                "name": "flask.g.seen",
                "reason": "receiver_not_a_name",
            }))]
        );
        assert_eq!(missing("test_manual_pop"), vec![None]);
    }

    /// The pilot's `AppContext.pop` leftovers each carry the call as written
    /// and its receiver, so a reader judges `values.pop("lang_code", None)`
    /// the way a grep hit shows it, without reading the caller. The sites
    /// whose receiver can hold the focal's owner come first and the literal
    /// and builtin ones last, none dropped; the caller whose body could not
    /// be read says so and guesses nothing; and every caller is read once
    /// however many of its sites are listed.
    #[test]
    fn named_leftovers_carry_their_call_as_written_most_likely_first() {
        let (store, focal, text, escape) = flask_pop();
        let scan = scan_focal(&store, &focal, &text, escape).unwrap();
        let rows: Vec<Value> = scan
            .named
            .iter()
            .map(|row| {
                json!({
                    "caller_name": row.caller_name,
                    "line_in_entity": row.line_in_entity,
                    "expr": row.expr,
                    "receiver": row.receiver,
                    "receiver_hint": row.receiver_hint,
                    "expr_unavailable": row.expr_unavailable,
                })
            })
            .collect();
        let row = |caller: &str, line: Option<u64>, expr: Option<&str>, receiver: Option<&str>| {
            json!({
                "caller_name": caller,
                "line_in_entity": line,
                "expr": expr,
                "receiver": receiver,
                "receiver_hint": null,
                "expr_unavailable": null,
            })
        };
        let hinted = |mut row: Value, hint: &str| {
            row["receiver_hint"] = json!(hint);
            row
        };
        let mut unreadable = row("test_unreadable", None, None, None);
        unreadable["expr_unavailable"] = json!("caller_source_unavailable");
        assert_eq!(
            rows,
            vec![
                hinted(
                    row(
                        "test_manual_pop",
                        Some(3),
                        Some("ctx.pop(None,)"),
                        Some("ctx")
                    ),
                    "typed_as_owner",
                ),
                row(
                    "test_max_cookie_size",
                    Some(2),
                    Some("recwarn.pop()"),
                    Some("recwarn"),
                ),
                row(
                    "test_url_processors.<locals>.pull_lang_code",
                    Some(1),
                    Some("values.pop(\"lang_code\", None)"),
                    Some("values"),
                ),
                row(
                    "test_nested_callback_order.<locals>.app_teardown1",
                    Some(1),
                    Some("flask.g.seen.pop()"),
                    Some("flask.g.seen"),
                ),
                unreadable,
                hinted(
                    row(
                        "test_literals",
                        Some(1),
                        Some("[1, 2].pop()"),
                        Some("[1, 2]")
                    ),
                    "literal",
                ),
                hinted(
                    row(
                        "test_literals",
                        Some(2),
                        Some("dict(a=1).pop(\"a\")"),
                        Some("dict(a=1)"),
                    ),
                    "builtin",
                ),
            ],
        );
        // One read per caller, however many of its sites are listed.
        let reads = text.reads.borrow();
        assert!(reads.values().all(|reads| *reads == 1), "{reads:?}");

        // The served block and the answer-only reply keep all of it, and the
        // listed residue carries the line each site sits on.
        let block = named_block(&scan);
        assert!(block.get(NAMED_EXPR_ELIDED_KEY).is_none(), "{block}");
        let mut reply = json!({
            "focal_entity": { "name": "AppContext.pop" },
            "references": [],
            "total_upstream": 0,
            "unconfirmed_candidates": 0,
            CALL_SITES_KEY: block,
        });
        crate::handlers::entities::project_answer_only(&mut reply);
        let candidates = &reply["call_site_candidates"];
        assert_eq!(
            candidates["named"][2]["expr"],
            "values.pop(\"lang_code\", None)"
        );
        assert_eq!(candidates["named"][2]["receiver"], "values");
        assert_eq!(candidates["named"][0]["receiver_hint"], "typed_as_owner");
        assert_eq!(
            candidates["named"][4]["expr_unavailable"],
            "caller_source_unavailable"
        );
        assert!(candidates["named"][4].get("expr").is_none());
        let residue = &candidates["value_calls"]["census"]["residue"];
        assert_eq!(
            residue["values"]["sites"][0]["expr"], "expect_exception(flask.session.pop, \"foo\")",
            "{residue}"
        );
        assert_eq!(
            residue["reflective"]["sites"][0]["expr"],
            "base_method = getattr(Flask, method.__name__)",
            "{residue}"
        );
        assert_eq!(
            residue["bindings"]["sites"][0]["expr_unavailable"], "no_entity_holds_line",
            "a site no entity holds says so rather than reading a file: {residue}"
        );
    }

    /// JavaScript and TypeScript callers read the same way: an optional call,
    /// a template literal holding a bracket, a chain broken over lines with a
    /// comment in its arguments, a constructor call, and the language's own
    /// literals and globals.
    /// React's reconciler calls a host-config function by the name it imports
    /// from a fork stub. The linker binds the call to the declaration each
    /// fork variant supplies, an open set, so the ledger leaves the site
    /// open. Asked about one of those declarations, the site is the proven
    /// edge's own call and no leftover. Asked about a declaration no edge
    /// reaches, the noop renderer's member, it stays listed.
    #[test]
    fn an_open_site_a_proven_edge_carries_is_answered_for_that_focal_only() {
        let js = LanguageId::JavaScript;
        let dom = spanned_entity(
            "measureInstance",
            "packages/react-dom-bindings/src/client/ReactFiberConfigDOM.js",
            js,
            0,
            "function measureInstance(instance) {\n  return null;\n}\n",
        );
        let mut noop = spanned_entity(
            "createReactNoop.<locals>.mutationHostConfig.measureInstance",
            "packages/react-noop-renderer/src/createReactNoop.js",
            js,
            0,
            "measureInstance(instance) {\n  return null;\n}\n",
        );
        noop.kind = kin_model::EntityKind::Method;
        let body = "function measureViewTransitionHostInstancesRecursive(instance) {\n  \
                    const nextMeasurement = measureInstance(instance);\n  \
                    return nextMeasurement;\n}\n";
        let reconciler = "packages/react-reconciler/src/ReactFiberCommitViewTransitions.js";
        let measure = caller_at(
            "measureViewTransitionHostInstancesRecursive",
            reconciler,
            js,
            900,
            body,
        );
        let callers = vec![caller(measure.clone(), body, vec!["measureInstance"])];
        let (store, text) = admit_callers(
            js,
            (
                "measureInstance",
                "packages/react-art/src/ReactFiberConfigART.js",
            ),
            &callers,
        );
        // The declarations themselves hold no call, and their ledgers say so.
        let context = proof_context(js, "current");
        let empty = |entity: &Entity| ledger(entity, "", context.id(), Vec::new());
        admit(
            &store,
            &[&dom, &noop],
            vec![context.clone(), empty(&dom), empty(&noop)],
        );
        // The module fork tier's edge to the DOM variant, carrying the call.
        let call = "measureInstance(instance)";
        let own = measure.span.as_ref().unwrap();
        let start = own.start_byte + body.find(call).unwrap();
        store
            .upsert_relation(&kin_model::Relation {
                id: kin_model::RelationId::new(),
                kind: kin_model::RelationKind::Calls,
                src: kin_model::GraphNodeId::Entity(measure.id),
                dst: kin_model::GraphNodeId::Entity(dom.id),
                confidence: kin_index::resolution::MODULE_FORK_CONFIDENCE,
                origin: kin_model::RelationOrigin::Inferred,
                created_in: None,
                import_source: None,
                evidence: vec![
                    kin_model::RelationEvidence {
                        source_span: Some(SourceSpan {
                            file: own.file.clone(),
                            start_byte: start,
                            end_byte: start + call.len(),
                            start_line: own.start_line + 1,
                            start_col: 26,
                            end_line: own.start_line + 1,
                            end_col: 26 + call.len() as u32,
                        }),
                        ..Default::default()
                    },
                    kin_model::RelationEvidence {
                        parser_rule: Some(kin_index::MODULE_FORK_EVIDENCE_RULE.to_string()),
                        source_path: Some(
                            "packages/react-reconciler/src/ReactFiberConfig.js".to_string(),
                        ),
                        resolved_path: Some(
                            "packages/react-reconciler/src/forks/ReactFiberConfig.dom.js"
                                .to_string(),
                        ),
                        ..Default::default()
                    },
                ],
            })
            .unwrap();
        let contained = || FocalEscape::Contained {
            entities_checked: 4,
            residue: kin_model::value_escape::EscapeResidue::default(),
        };

        let asked_dom = scan_focal(&store, &dom, &text, contained()).unwrap();
        assert_eq!(asked_dom.proven_by_edge, 1);
        assert!(asked_dom.named.is_empty(), "{:?}", asked_dom.named);
        assert!(asked_dom.candidates.is_empty());
        let block = named_block(&asked_dom);
        assert_eq!(block["proven_by_edge"], 1, "{block}");
        assert_eq!(block["candidate_count"], 0, "{block}");
        assert_eq!(block["clauses"], json!([]), "{block}");
        assert_eq!(block["settled"], true, "{block}");

        // No edge reaches the noop renderer's member, so for it the same
        // site is what it was: a call that spells the name, left to check.
        let asked_noop = scan_focal(&store, &noop, &text, contained()).unwrap();
        assert_eq!(asked_noop.proven_by_edge, 0);
        assert_eq!(asked_noop.named.len(), 1, "{:?}", asked_noop.named);
        assert_eq!(asked_noop.named[0].caller, measure.id);
        assert_eq!(asked_noop.named[0].expr.as_deref(), Some(call));
        // It is written on no object, and the row says so.
        assert!(asked_noop.named[0].bare_call);
        assert_eq!(
            serde_json::to_value(&asked_noop.named[0]).unwrap()["bare_call"],
            true
        );
        let block = named_block(&asked_noop);
        assert!(block.get("proven_by_edge").is_none(), "{block}");
        let clauses = named_clauses(&asked_noop);
        assert_eq!(clauses.len(), 1, "{clauses:?}");
        assert!(clauses[0].starts_with("named_leftovers: 1 call site(s)"));
    }

    #[test]
    fn js_and_ts_leftovers_carry_their_call_as_written() {
        let ts = LanguageId::TypeScript;
        let js = LanguageId::JavaScript;
        let focal = spanned_entity(
            "Stack.pop",
            "src/stack.ts",
            ts,
            0,
            "pop(): number {\n    return 0;\n  }\n",
        );
        let drain_body = "function drain(items: Stack): void {\n  items\n    .pop(\n      \
                          1, // first (of many)\n    );\n}\n";
        let mut drain = caller_at("drain", "src/drain.ts", ts, 10, drain_body);
        bind(
            &mut drain,
            "items",
            json!({"declared": {"type_name": "Stack"}}),
        );
        let make_body = "function make(): number {\n  return new Stack().pop();\n}\n";
        let queue_body = "function useQueue(queue) {\n  return queue?.pop(`a)`, b);\n}\n";
        let literal_body =
            "function literal(text) {\n  const last = [3, 4].pop();\n  JSON.parse(text).pop();\n}\n";
        let callers = vec![
            caller(drain, drain_body, vec!["pop"]),
            caller(
                caller_at("make", "src/make.ts", ts, 3, make_body),
                make_body,
                vec!["pop"],
            ),
            caller(
                caller_at("useQueue", "src/queue.js", js, 1, queue_body),
                queue_body,
                vec!["pop"],
            ),
            caller(
                caller_at("literal", "src/literal.js", js, 1, literal_body),
                literal_body,
                vec!["pop", "pop"],
            ),
        ];
        let (store, text) = admit_callers(ts, ("Other.pop", "src/other.ts"), &callers);
        let scan = scan_focal(&store, &focal, &text, unknown_census()).unwrap();
        // Every one of these is made on a value.
        assert!(scan.named.iter().all(|row| !row.bare_call));
        type Row<'s> = (&'s str, Option<&'s str>, Option<&'s str>, Option<&'s str>);
        let rows: Vec<Row> = scan
            .named
            .iter()
            .map(|row| {
                (
                    row.caller_name.as_str(),
                    row.expr.as_deref(),
                    row.receiver.as_deref(),
                    row.receiver_hint,
                )
            })
            .collect();
        assert_eq!(
            rows,
            vec![
                (
                    "drain",
                    Some("items.pop(1,)"),
                    Some("items"),
                    Some("typed_as_owner")
                ),
                (
                    "make",
                    Some("new Stack().pop()"),
                    Some("new Stack()"),
                    Some("names_owner"),
                ),
                ("useQueue", Some("queue?.pop(`a)`, b)"), Some("queue"), None),
                (
                    "literal",
                    Some("[3, 4].pop()"),
                    Some("[3, 4]"),
                    Some("literal")
                ),
                (
                    "literal",
                    Some("JSON.parse(text).pop()"),
                    Some("JSON.parse(text)"),
                    Some("builtin"),
                ),
            ],
        );
    }
}

#[cfg(test)]
mod owed_outside_tests {
    use serde_json::json;

    use super::{family_block, text_lines, OwedFile, FAMILY_SCOPE};
    use crate::caller_arrival::{impact_arrival_gaps, owed_outside_gap, CALLER_ARRIVAL_KEY};

    fn owed(file: &str, callers: u64) -> OwedFile {
        OwedFile {
            file: file.to_string(),
            callers,
        }
    }

    #[test]
    fn a_settled_family_with_nothing_owed_outside_stays_settled() {
        let tally = kin_model::CallSiteTally::default();
        let block = family_block(&tally, Some(&[]));
        assert_eq!(block["settled"], true, "{block}");
        assert_eq!(block["scope"], FAMILY_SCOPE, "{block}");
        assert!(block.get("owed_outside_scope").is_none(), "{block}");
        assert!(text_lines(&block)
            .iter()
            .any(|line| line == "  every site in scope is settled"));
    }

    #[test]
    fn an_owed_caller_outside_the_family_unsettles_it_and_names_the_file() {
        let tally = kin_model::CallSiteTally::default();
        let block = family_block(
            &tally,
            Some(&[owed("pkg/views.py", 2), owed("pkg/ctx.py", 1)]),
        );
        assert_eq!(block["settled"], false, "{block}");
        assert_eq!(block["owed_outside_scope"]["file_count"], 2, "{block}");
        assert_eq!(block["owed_outside_scope"]["callers"], 3, "{block}");
        let text = text_lines(&block).join("\n");
        assert!(!text.contains("every site in scope is settled"), "{text}");
        assert!(
            text.contains("not settled: call_sites_owed: 3 caller(s) in 2 module(s) outside"),
            "{text}"
        );
        assert!(text.contains("pkg/views.py (2 caller(s))"), "{text}");
        assert!(text.contains("kin daemon sweep"), "{text}");
    }

    #[test]
    fn an_unreadable_index_refuses_rather_than_certifying() {
        let tally = kin_model::CallSiteTally::default();
        let block = family_block(&tally, None);
        assert_eq!(block["settled"], false, "{block}");
        assert_eq!(block["owed_outside_scope"]["unreadable"], true, "{block}");
        let payload = json!({ CALLER_ARRIVAL_KEY: { "state": "accounted",
            "owed_outside_scope": { "unreadable": true } } });
        let gap = owed_outside_gap(&payload).expect("an unreadable index is a gap");
        assert!(gap.starts_with("call_sites_owed: "), "{gap}");
    }

    /// A caller outside the family reaches the focal by name, so an owed one
    /// counts only while its body could spell that name: a whole preview that
    /// never does rules it out, and a cut or absent preview never does.
    #[test]
    fn only_a_body_that_could_spell_the_focal_counts_outside_the_family() {
        use kin_model::graph::EntityStore as _;
        let graph = kin_db::InMemoryGraph::new();
        let body = |name: &str, preview: Option<String>| {
            let mut entity = super::fixture::spanned_entity(
                name,
                &format!("pkg/{name}.py"),
                kin_model::LanguageId::Python,
                20,
                "def f():\n    g()\n",
            );
            if let Some(preview) = preview {
                entity.metadata.extra.insert(
                    kin_parser::extract::EMBEDDING_BODY_PREVIEW_KEY.to_string(),
                    json!(preview),
                );
            }
            graph.upsert_entity(&entity).unwrap();
        };
        body(
            "names_it",
            Some("def names_it(): current_app.ensure_sync(f)".into()),
        );
        body("never_does", Some("def never_does(): print(1)".into()));
        body(
            "cut_short",
            Some(format!("def cut_short(): {}", "x".repeat(9000))),
        );
        body("no_preview", None);
        // The graph names the focal by its owner, but an ordinary method
        // call spells the member, so derive its call names from the entity.
        let mut focal = super::fixture::spanned_entity(
            "App.ensure_sync",
            "pkg/app.py",
            kin_model::LanguageId::Python,
            0,
            "def ensure_sync(self, f): return f",
        );
        focal.kind = kin_model::EntityKind::Method;
        let names = super::focal_call_names(&focal);
        let owed = super::owed_outside(
            &graph,
            kin_model::LanguageId::Python,
            &std::collections::HashSet::new(),
            &names,
        )
        .expect("the index reads");
        let files: Vec<&str> = owed.iter().map(|file| file.file.as_str()).collect();
        assert_eq!(
            files,
            ["pkg/cut_short.py", "pkg/names_it.py", "pkg/no_preview.py"],
            "{owed:?}"
        );
    }

    /// An ordinary method call spells its member, while a constructor may
    /// spell the owner instead. Qualified owners alone must not widen a method.
    #[test]
    fn a_focal_is_named_by_every_segment_a_call_may_spell() {
        use kin_model::EntityKind;
        fn names(name: &str, kind: EntityKind) -> Vec<String> {
            let mut focal = super::fixture::spanned_entity(
                name,
                "pkg/focal.py",
                kin_model::LanguageId::Python,
                0,
                "def f(): pass",
            );
            focal.kind = kind;
            super::focal_call_names(&focal)
        }
        assert_eq!(
            names("App.ensure_sync", EntityKind::Method),
            ["ensure_sync"]
        );
        assert_eq!(names("Store::open", EntityKind::Method), ["open"]);
        assert_eq!(names("parse_note", EntityKind::Function), ["parse_note"]);
        assert_eq!(names("Trailing.", EntityKind::Class), ["Trailing"]);
        assert_eq!(
            names("HTTPAdapter.__init__", EntityKind::Method),
            ["HTTPAdapter", "__init__"]
        );

        let entity = |preview: &str| {
            let mut entity = super::fixture::spanned_entity(
                "caller",
                "pkg/caller.py",
                kin_model::LanguageId::Python,
                0,
                preview,
            );
            entity.metadata.extra.insert(
                kin_parser::extract::EMBEDDING_BODY_PREVIEW_KEY.to_string(),
                json!(preview),
            );
            entity
        };
        let constructs = entity("def make(): return HTTPAdapter()");
        assert!(super::could_name_focal(
            &constructs,
            &names("HTTPAdapter.__init__", EntityKind::Method)
        ));
        let sends = entity("def go(self): return self.send(req)");
        assert!(super::could_name_focal(
            &sends,
            &names("Session.send", EntityKind::Method)
        ));
        let unrelated = entity("def go(): return print(1)");
        assert!(!super::could_name_focal(
            &unrelated,
            &names("Session.send", EntityKind::Method)
        ));
        let owner_only = entity("def go(): return Session()");
        assert!(!super::could_name_focal(
            &owner_only,
            &names("Session.send", EntityKind::Method)
        ));
    }

    /// Impact's zero consumer counts are not whole while a caller outside a
    /// family is owed, even when every family is accounted.
    #[test]
    fn impact_names_the_owed_callers_outside_an_accounted_reading() {
        let payload = json!({ CALLER_ARRIVAL_KEY: {
            "state": "accounted",
            "entities_examined": 1,
            "entities": [],
            "owed_outside_scope": { "file_count": 1, "callers": 2,
                "files": [{ "file": "pkg/views.py", "callers": 2 }] },
        } });
        let gaps = impact_arrival_gaps(&payload);
        assert_eq!(gaps.len(), 1, "{gaps:?}");
        assert!(
            gaps[0].starts_with("call_sites_owed: 2 caller(s) in 1 module(s) outside"),
            "{gaps:?}"
        );
        let settled = json!({ CALLER_ARRIVAL_KEY: {
            "state": "accounted", "entities_examined": 1, "entities": [] } });
        assert!(impact_arrival_gaps(&settled).is_empty());
    }
}
