// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! One verdict per response (FIR-2463).
//!
//! A retrieval response used to carry up to three verdict-shaped claims that
//! were computed independently and could disagree with each other. The
//! `negative` object certified or refused an absence, `edge_coverage` reported
//! whether the graph could hold the edges the answer was read off, and
//! `_kin.completeness` reported `status`, `bound` and `counted.exact`. Which one
//! an agent acted on depended on which key it happened to read.
//!
//! The shape is not hypothetical. A shipped v0.5.42 `find_references` response
//! carried `completeness.status: "complete"`, `bound: "exact"`,
//! `counted.exact: true` and a note reading "the counts here are the whole set"
//! directly above `classes: {"imports": "absent", "references": "absent"}` and
//! `negative.safe_to_conclude_absent: false`. In the same session on the same
//! repository, `graph_neighborhood` walked the same two inbound edges and framed
//! them as `"complete"` with no `negative` object at all, because a non-empty
//! answer synthesizes none.
//!
//! ## The contract
//!
//! [`Verdict::compute`] is the one authority. Every block that publishes a
//! verdict-shaped claim is an INPUT to it, the most pessimistic input wins, and
//! certification requires every input that spoke to agree. The blocks keep
//! publishing their raw observations, because those are the evidence a reader
//! needs to act, but the fields a reader acts ON are projections of this one
//! verdict:
//!
//! - `negative.safe_to_conclude_absent` / `negative.trust` / `negative.advice`
//!   are built from the gap list this module contributes to, so they are correct
//!   by construction rather than patched afterwards.
//! - `_kin.completeness.bound` and `counted.exact` are capped by
//!   [`Verdict::project_onto_completeness`].
//! - `_kin.completeness.status`, `classes`, `decided_by` and `limits` stay raw
//!   observation. `status` answers "was the substrate whole", which is a
//!   different question from "can this answer be acted on", and collapsing the
//!   two would throw away the evidence rather than reconcile it.
//!
//! [`disagreements`] is the invariant that holds this together. It scans a fully
//! built response for any block whose verdict language contradicts
//! `_kin.verdict`, and it runs under `debug_assert!` on every annotated response
//! so a future block cannot reintroduce a second verdict silently.

use serde_json::{json, Map, Value};

use crate::envelope::Envelope;

/// Reserved key under `_kin` holding the single verdict for the response.
pub const VERDICT_KEY: &str = "verdict";

/// Wire words for a verdict state and for one input's reading.
const CERTIFIED: &str = "certified";
const INCONCLUSIVE: &str = "inconclusive";
const NOT_APPLICABLE: &str = "not_applicable";
/// The state of an answer that is complete except a residue it lists or
/// counts in full: every clause that refused is one of
/// [`kin_model::call_site_reading::RESIDUE_CODES`]. Its rows are proven, and
/// the only sites that could add to them are the ones the answer names, so a
/// reader checks those rather than distrusting the whole answer. It never
/// licenses an absence: `safe_to_conclude_absent` stays false.
pub const CERTIFIED_EXCEPT: &str = "certified_except";

/// Machine-stable label recorded in `_kin.completeness.limits` when the single
/// verdict, rather than this answer's own substrate reading, is what stopped the
/// counts being called whole.
const VERDICT_LIMIT: &str = "verdict_inconclusive";

/// The label recorded in `_kin.completeness.limits` when the counts are not
/// whole only because of a residue the answer lists: a floor with a known,
/// named remainder rather than an unknown one.
const VERDICT_EXCEPT_LIMIT: &str = "verdict_certified_except";

/// What separates two clauses when the factor is rendered as one sentence.
///
/// The rendering only. Nothing parses it back: clauses are carried as a list
/// from the reading that produced them to the single join at serialization,
/// because this string is also ordinary punctuation inside a clause's prose and
/// a boundary inferred from it is a boundary a human never chose.
pub(crate) const CLAUSE_SEPARATOR: &str = "; ";

/// One code the verdict can carry, with what it means.
///
/// `meaning` is the one sentence Kin writes for the code, once, and
/// `docs/mcp-tools.md` carries the same table: a test holds the two equal, so
/// the docs cannot drift from the list the verdict reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClauseCode {
    /// The code as it appears in `_kin.verdict.limiting_factor`.
    pub code: &'static str,
    /// What it means, in one sentence.
    pub meaning: &'static str,
}

/// Every code `_kin.verdict.limiting_factor` and `negative.trust_reason` can
/// carry, in code order.
///
/// Closed: a clause whose label is not here is sent as [`UNLISTED_CLAUSE_CODE`]
/// rather than minted, and `every_clause_label_a_producer_writes_is_listed`
/// scans this crate's sources so a producer that writes a new label fails CI
/// until the label is listed here and in the docs.
pub const CLAUSE_CODES: &[ClauseCode] = &[
    ClauseCode {
        code: "absence_coverage_unmeasured",
        meaning: "No coverage class was measured for the answer's language, so an empty result cannot be separated from a declaration the extractor never admitted.",
    },
    ClauseCode {
        code: "absence_coverage_unreported",
        meaning: "The answer did not report which languages its absence claim spans or whether this build resolves them.",
    },
    ClauseCode {
        code: "absence_scope_empty",
        meaning: "The graph holds no entity at all under the filter the query applied, so an empty result describes the index rather than the code.",
    },
    ClauseCode {
        code: "answer_coverage_unmeasured",
        meaning: "No coverage class was measured for the answer's language, so its rows are a floor.",
    },
    ClauseCode {
        code: "answer_coverage_unreported",
        meaning: "The answer did not report which languages its rows span or whether this build resolves them, so its rows are a floor.",
    },
    ClauseCode {
        code: "answer_truncated",
        meaning: "The answer stopped early and returned part of what it found, so its counts are a floor.",
    },
    ClauseCode {
        code: "binding_unproven",
        meaning: "A call site in the answer's scope calls through a value binding, which proves no target, so where that call goes is not known.",
    },
    ClauseCode {
        code: "body_unverified",
        meaning: "This entity records no source digest, so the served span could not be checked against the bytes it was cut from.",
    },
    ClauseCode {
        code: "call_sites_not_in_build",
        meaning: "A call site in the answer's scope sits in a source unit no build of the repository compiles, so no resolver proved where that call goes.",
    },
    ClauseCode {
        code: "call_sites_owed",
        meaning: "A caller in the answer's scope has call sites the graph has not settled yet, because its derivation or its enrichment is still owed, so a call there is not accounted for.",
    },
    ClauseCode {
        code: "call_sites_server_failed",
        meaning: "The resolver timed out, crashed or broke protocol at a call site in the answer's scope, so where that call goes is not known.",
    },
    ClauseCode {
        code: "call_sites_unproven_no_resolver",
        meaning: "A caller in the answer's scope has call sites no resolver can prove on this host now, because language-server enrichment is switched off, no language server serves its language, or the one that does cannot start (its analysis environment is missing, say), so waiting for enrichment will not settle them; the clause names which.",
    },
    ClauseCode {
        code: "call_sites_unresolved",
        meaning: "The resolver answered at a call site in the answer's scope and its answer proves no target, so where that call goes is not known.",
    },
    ClauseCode {
        code: "caller_arrival_state_unknown",
        meaning: "The answer reported a caller-arrival state this build does not recognise, so an empty reference list cannot be read as whole.",
    },
    ClauseCode {
        code: "caller_arrival_unmeasured",
        meaning: "The set of modules that can reach the focal could not be established, because the language links no imports between modules in this graph.",
    },
    ClauseCode {
        code: "caller_arrival_unresolved",
        meaning: "Some modules that can reach the focal have callers the graph did not resolve, so the reference list is a floor.",
    },
    ClauseCode {
        code: "constructor_dispatch_unlisted",
        meaning: "Every call that spells the constructor's call names is proven or listed, and a constructor also runs when code constructs a subclass that defines no constructor of its own, or constructs through `cls`, `type(self)` or `self.__class__`, none of which spells those names, so such constructions are not in the answer.",
    },
    ClauseCode {
        code: "counts_are_a_floor",
        meaning: "The answer's own accounting reports its numbers as a lower bound.",
    },
    ClauseCode {
        code: "coverage_absent",
        meaning: "No entity in the store carries an embedding, so an empty semantic result means nothing was ranked.",
    },
    ClauseCode {
        code: "coverage_graph_body_gap",
        meaning: "Some graph-owned source paths carry no body, so their entities rank on text fallback.",
    },
    ClauseCode {
        code: "coverage_partial",
        meaning: "The semantic index is incomplete, so an empty result may mean not indexed rather than not present.",
    },
    ClauseCode {
        code: "coverage_role_filter_withheld",
        meaning: "Test-role source paths were withheld from ranking; pass include_tests to rank them.",
    },
    ClauseCode {
        code: "coverage_unknown",
        meaning: "Embedding coverage was not reported, so an empty result may mean not indexed rather than not present.",
    },
    ClauseCode {
        code: "cross_file_edges_absent",
        meaning: "The graph was not observed to hold edges of a requested class between modules for the language, so a use reaching the target through that class could not have been found; the gap is in extraction or enrichment rather than in the code, and the classes that are present do not stand in for it.",
    },
    ClauseCode {
        code: "cross_file_edges_unproduced",
        meaning: "The build produced no entity-level edge of a requested class, either because no build mints that class for the language or because the linker resolved its sites without emitting one; the gap is in extraction, not in the code.",
    },
    ClauseCode {
        code: "cross_repo_authority_incomplete",
        meaning: "The cross-repo spine's topology or the requested relation subtype is incomplete at its revision.",
    },
    ClauseCode {
        code: "cross_repo_authority_missing",
        meaning: "The answer did not report cross-repo authority.",
    },
    ClauseCode {
        code: "cross_repo_authority_unknown",
        meaning: "The answer reported a cross-repo authority status this build does not recognise.",
    },
    ClauseCode {
        code: "cross_repo_not_applicable",
        meaning: "The focal is a symbol outside every repository, so no cross-repo authority applies and the answer lists the callers this repository holds.",
    },
    ClauseCode {
        code: "cross_repo_not_configured",
        meaning: "No cross-repo spine is configured, so the answer is scoped to this repository.",
    },
    ClauseCode {
        code: "cross_repo_unavailable",
        meaning: "A configured cross-repo spine could not answer and named no condition.",
    },
    ClauseCode {
        code: "declaration_census_unmeasured",
        meaning: "Some source unit the name search covers carries no current declaration census, so a declaration the extractor did not admit there could match the query and is not listed.",
    },
    ClauseCode {
        code: "declarations_unadmitted_listed",
        meaning: "Declarations the grammar shows that extraction did not admit could match the name search, and each is listed, so the rows are complete except those.",
    },
    ClauseCode {
        code: "declarations_unparsed_files_listed",
        meaning: "Source units the name search covers are held as bytes and were never parsed, and each is listed, so the rows are complete except what those units declare.",
    },
    ClauseCode {
        code: "decorator_dispatch_listed",
        meaning: "The method could run without a call that spells its name only through the decorators it is declared under, each of which receives it and returns what a call spelling its name runs; the answer lists every one in `call_sites.dispatch_decorators`, so it is complete except what those decorators do with it.",
    },
    ClauseCode {
        code: "degraded",
        meaning: "The daemon reported a degraded signal; `_kin.degraded` names which.",
    },
    ClauseCode {
        code: "dependency_outside_graph",
        meaning: "The question names a dependency this repository imports and this graph holds no definitions for, so the answer may live outside what was searched.",
    },
    ClauseCode {
        code: "dependency_scan_incomplete",
        meaning: "The scan of unadmitted imports hit its budget before it checked all of them.",
    },
    ClauseCode {
        code: "dependency_scan_unavailable",
        meaning: "The store could not say which of its imports are unadmitted.",
    },
    ClauseCode {
        code: "depth_zero",
        meaning: "The walk expanded no edges, so an empty neighbourhood is not evidence of isolation.",
    },
    ClauseCode { code: "derived_source_stale", meaning: "Derived source evidence differs from admitted bytes; useful last-good rows do not establish current completeness." },
    ClauseCode { code: "derived_source_unproven", meaning: "Bounded graph evidence did not establish source binding in the selected scope." },
    ClauseCode {
        code: "edge_coverage_budget_exhausted",
        meaning: "The coverage scan for the language stopped before it could establish what the graph holds.",
    },
    ClauseCode {
        code: "edge_coverage_unknown",
        meaning: "Whether the graph holds edges of a requested class between modules for the language could not be established.",
    },
    ClauseCode {
        code: "edge_coverage_unreported",
        meaning: "The answer did not report whether the graph holds the edges between modules it depends on.",
    },
    ClauseCode {
        code: "enrichment_incomplete",
        meaning: "Persisted call-site evidence in the selected repository graph is unsettled, so impact counts are a lower bound and review risk may change; the listed entities are not proven dependencies of the change.",
    },
    ClauseCode {
        code: "enrichment_metadata_unavailable",
        meaning: "Selected graph enrichment detail exceeded its bounded metadata scan. Aggregate counters remain observations, but the unavailable source inventory cannot attest dependency completion or absence.",
    },
    ClauseCode {
        code: "entity_index_unresolved",
        meaning: "Nothing resolves the program behind the parsed declarations, so an empty name or kind filter cannot separate a missing declaration from one the extractor did not admit.",
    },
    ClauseCode {
        code: "enumeration_shifted",
        meaning: "The source unit gained or lost entities between pages, so the pages do not assemble into one state; walk again from the start.",
    },
    ClauseCode {
        code: "escape_residue_listed",
        meaning: "The focal can be reached without a call that spells its call names through sites the escape census lists: uses that hold it as a value, implicit calls, lookups whose recorded key or text could spell its name, protocol dispatch or unparsed regions, so the answer is complete except those listed sites.",
    },
    ClauseCode {
        code: "file_bytes_unadmitted",
        meaning: "The working copy holds content for this source unit that graph truth does not carry, so the spans describe earlier bytes; `kin admit` takes the working tree.",
    },
    ClauseCode {
        code: "file_bytes_unchecked",
        meaning: "This repository has a working copy the graph is supposed to be level with and nothing is comparing them, so whether the answer describes the source unit as it is now is not known.",
    },
    ClauseCode {
        code: "file_coverage_unreported",
        meaning: "The answer did not report whether a language adapter parsed the source unit.",
    },
    ClauseCode {
        code: "file_not_parsed",
        meaning: "No language adapter produced a layout for the source unit, so an empty enumeration is a fact about extraction coverage, not about the code.",
    },
    ClauseCode {
        code: "file_parse_failed",
        meaning: "The adapter could not parse the source unit, so the entities the graph still carries for it describe an earlier state.",
    },
    ClauseCode {
        code: "file_parse_state_unknown",
        meaning: "The answer reported a parse state this build does not recognise.",
    },
    ClauseCode {
        code: "file_parse_unrecorded",
        meaning: "The graph holds entities for the source unit and no record of how completely they were extracted, so the rows are real and their being the whole set is unestablished.",
    },
    ClauseCode {
        code: "file_parsed_partially",
        meaning: "The adapter hit parse errors in the source unit, so its entities are a floor.",
    },
    ClauseCode {
        code: "file_spans_stale",
        meaning: "Some entity spans in the source unit were derived from bytes the repository tree no longer holds for it.",
    },
    ClauseCode {
        code: "focal_not_in_graph",
        meaning: "The focal entity was not found, so an empty neighbourhood is not evidence that it is isolated.",
    },
    ClauseCode {
        code: "focal_resolution_ambiguous",
        meaning: "The focal name resolved to several entities and only one was answered for.",
    },
    ClauseCode {
        code: "focal_resolution_unreported",
        meaning: "The answer did not report how many entities the focal could have resolved to, so it may describe a same-named sibling.",
    },
    ClauseCode {
        code: "generator_source_unavailable",
        meaning: "This derived member's generator source could not be read, so what it was generated from is not shown.",
    },
    ClauseCode {
        code: "graph_admission_unrecorded",
        meaning: "The daemon reports no complete admission of the repository into graph truth, so how far the graph is behind is unmeasured.",
    },
    ClauseCode {
        code: "graph_behind_working_tree",
        meaning: "Host paths on disk have never been admitted, or admitted paths are still owed their parse; `_kin.behind` counts each separately.",
    },
    ClauseCode {
        code: "graph_empty",
        meaning: "The graph that answered holds no entities, so it cannot speak for the repository yet; ask again once its graph is loaded.",
    },
    ClauseCode {
        code: "graph_not_loaded",
        meaning: "The daemon reports no graph loaded, so an empty structural result is not authoritative.",
    },
    ClauseCode {
        code: "graph_uninitialized",
        meaning: "The daemon has not confirmed its first reconciliation or snapshot load.",
    },
    ClauseCode {
        code: "history_partial",
        meaning: "The store admitted only its checked-out commit, so an answer that reads its history is missing every earlier revision; `_kin.partial_history` says how far the history derivation got.",
    },
    ClauseCode {
        code: "history_predates_upgrade",
        meaning: "The answer reads the store's history, and history recorded before its last `kin upgrade` keeps the replay version that authored it and carries no checked binding history.",
    },
    ClauseCode {
        code: "impact_page_partial",
        meaning: "This page contains only part of an impact answer; reconstruct every page and retain the original safety readings before assessing the complete result.",
    },
    ClauseCode {
        code: "lexical_fallback_matched_nothing",
        meaning: "A phrase query matched no name, and the per-token fallback ranks by word overlap rather than meaning.",
    },
    ClauseCode {
        code: "lexical_lookup_not_structural",
        meaning: "This is lexical evidence over stored graph fields, not a resolved call or reference edge; a hit or a miss here is not proof the identifier is or is not used.",
    },
    ClauseCode {
        code: "local_binding_outstanding",
        meaning: "Previously local source bindings remain unresolved; current source and parse evidence do not establish complete dependency knowledge.",
    },
    ClauseCode {
        code: "local_binding_unproven",
        meaning: "Prior-local binding evidence could not be validated for the checked scope; complete dependency knowledge is unproven.",
    },
    ClauseCode {
        code: "local_binding_unrelated",
        meaning: "Outstanding local-name bindings in the answer's scope sit outside every entity it lists and spell none of their names; they are counted beside the answer, which is complete except them and never an absence.",
    },
    ClauseCode {
        code: "locate_page_partial",
        meaning: "This page contains only part of a ranking page; reconstruct every page and retain the original safety readings before assessing the complete result, then follow the ranking's own next_cursor for the rows after it.",
    },
    ClauseCode {
        code: MATCHED_BY_NAME,
        meaning: "These rows name the entity in their text, and Kin has not resolved what they use it for; a name built while code runs, or a pattern, is not followed.",
    },
    ClauseCode {
        code: "method_call_resolution_incomplete",
        meaning: "Receiver-method calls are linked by bare name and may be unresolved, so an empty result is not authoritative for a method.",
    },
    ClauseCode {
        code: "model_filtered",
        meaning: "A model stage withheld graph candidates; its scores cannot certify repository absence.",
    },
    ClauseCode {
        code: "model_observation_invalid",
        meaning: "The answer supplied invalid model-stage evidence, so repository absence is not certified.",
    },
    ClauseCode {
        code: "name_filter_narrowed_to_zero",
        meaning: "The name pattern selects declarations and the query's other filters removed every one of them.",
    },
    ClauseCode {
        code: "named_leftovers",
        meaning: "Call sites whose callee or caller body spells the focal's call name are not proven, and the answer lists every one by caller and line beside its rows, so it is complete except those sites.",
    },
    ClauseCode {
        code: "none_of_these",
        meaning: "The candidate scorer rated no candidate at or above its calibrated threshold and the query named none of them, so the rows are leads rather than an answer.",
    },
    ClauseCode {
        code: "offline_fallback",
        meaning: "The in-process fallback graph answered, not the daemon's graph truth.",
    },
    ClauseCode {
        code: "page_bounded",
        meaning: "The response holds one page of the enumeration; follow `next_cursor` to the end before reading the set as whole.",
    },
    ClauseCode {
        code: "proof_context_stale",
        meaning: "A call site in the answer's scope was proven under a proof context its resolver no longer runs under, so the proof may not hold for the code as it builds now.",
    },
    ClauseCode {
        code: "proof_context_unverified",
        meaning: "The selected graph has not validated the proof context of recorded call-site evidence, so that evidence does not establish current validity.",
    },
    ClauseCode {
        code: "ranking_is_bounded",
        meaning: "A ranking is a bounded candidate set, so a name absent from it may belong to an entity the query never ranked.",
    },
    ClauseCode {
        code: "reference_completion_unmeasured",
        meaning: "Completion of requested non-call relations is unmeasured: the escape census took no reading of the focal's non-call uses, or took it over records that predate it or regions no record covers, so the Call census cannot certify this set.",
    },
    ClauseCode {
        code: "reference_enrichment_no_language_server",
        meaning: "An adapter is wired for the language but no language server for it is installed on this host.",
    },
    ClauseCode {
        code: "reference_enrichment_unknown",
        meaning: "No completed language-server readiness observation is recorded for the selected language, so reference-enrichment availability is unestablished.",
    },
    ClauseCode {
        code: "reference_enrichment_unsupported",
        meaning: "This build cannot link references between modules for the language, so an unused symbol cannot be told from an unlinked one.",
    },
    ClauseCode {
        code: "reference_enrichment_unusable",
        meaning: "A language server for the selected language could not start, so reference-enrichment capability is unavailable. The clause names why, as does `edge_coverage.reference_enrichment_reason`.",
    },
    ClauseCode {
        code: "reference_mentions_unmatched",
        meaning: "Non-call reads of the focal's name that could read it have no stored row of a requested kind, and the answer lists every one in `focal_escape.residue.mentions.unmatched`, so it is complete except those reads.",
    },
    ClauseCode {
        code: "reference_page_partial",
        meaning: "This page contains only part of a reference answer; reconstruct every page and retain the original safety readings before assessing the complete result.",
    },
    ClauseCode {
        code: "reference_sites_undecided",
        meaning: "Reference sites a language server left undecided on repeated passes over unchanged code are served as references that nothing proves, and the answer counts them in `undecided_reference_sites` without listing every one, so usages there may be missing.",
    },
    ClauseCode {
        code: "reference_sites_undecided_listed",
        meaning: "Reference sites a language server left undecided on repeated passes over unchanged code are served as references that nothing proves, and the answer lists every one with its caller and line in `undecided_reference_sites.sites`, so it is complete except those sites.",
    },
    ClauseCode {
        code: "relevance_floor_unmeasured",
        meaning: "Every returned row was a fallback neighbour and no calibrated threshold says any of them answers the concept.",
    },
    ClauseCode {
        code: "response_bounded",
        meaning: "The response budget withheld part of the answer; `_kin.response` names what was cut.",
    },
    ClauseCode {
        code: "retrieval_degraded",
        meaning: "The query reported degradations; the payload's `degradations` names them.",
    },
    ClauseCode {
        code: "selected_graph_sample_stale",
        meaning: "The selected graph could not be sampled live, so the counters replay an earlier observation; `_kin.freshness` has its age.",
    },
    ClauseCode {
        code: "semantic_authoritative",
        meaning: "Certifying: daemon-owned truth with complete embedding coverage. Appears only when trust is authoritative.",
    },
    ClauseCode { code: "semantic_readmission_failed", meaning: "Semantic readmission failed for admitted source; current semantic completeness is not established." },
    ClauseCode {
        code: "spine_candidate_representation_gap",
        meaning: "The daemon's cross-repo spine refused this repository's graph because it holds an inferred member whose candidate authority the spine format cannot carry, so no cross-repo authority stands behind the answer for as long as the graph holds it.",
    },
    ClauseCode {
        code: "spine_initialization_deferred",
        meaning: "The daemon's cross-repo spine was not built yet, because its initialization stepped aside while a writer held graph authority, so no cross-repo authority stands behind the answer; a read after the writer finishes builds it.",
    },
    ClauseCode {
        code: "spine_root_stale",
        meaning: "The cross-repo spine's recorded root for this repository is stale.",
    },
    ClauseCode {
        code: "store_semantics_ahead",
        meaning: "A newer Kin build recorded this store's replay semantics; upgrade Kin rather than re-deriving the store with this older build.",
    },
    ClauseCode {
        code: "store_semantics_behind",
        meaning: "This store serves state an older Kin build derived; run `kin upgrade` in this repository (`npx -y @kinlab/kin@<version> upgrade` through npm) to re-derive it under this build, keeping every native commit, branch and review.",
    },
    ClauseCode {
        code: "store_semantics_unknown",
        meaning: "This store's replay-semantics record is missing or unreadable; upgrade Kin to the newest build, then run `kin upgrade` in this repository.",
    },
    ClauseCode {
        code: "structural_authoritative",
        meaning: "Certifying: the daemon's graph is initialized and loaded. Appears only when trust is authoritative.",
    },
    ClauseCode {
        code: "substrate_partial",
        meaning: "A coverage class the answer depended on was observed short of whole; `_kin.completeness.classes` names it and says whether it was `partial`, `absent` or `unproduced`.",
    },
    ClauseCode {
        code: "substrate_unknown",
        meaning: "The coverage classes the answer depended on were not all observed present; `_kin.completeness.classes` names them.",
    },
    ClauseCode {
        code: "trace_page_partial",
        meaning: "This page contains only part of a trace; reconstruct every page and retain the original safety readings before assessing the complete result.",
    },
    ClauseCode {
        code: "trace_spine_clipped",
        meaning: "The per-step cap cut the walk's fan-out, so the chain is one route among those the cap kept and a missing hop was not looked for.",
    },
    ClauseCode {
        code: "trace_walk_degraded",
        meaning: "The walk reported degradations, so it did not complete under its own work bounds.",
    },
    ClauseCode {
        code: "trace_walk_truncated",
        meaning: "The walk hit a per-step or total cap before examining everything an empty chain would have to rule out.",
    },
    ClauseCode {
        code: "tracked_changes_unadmitted",
        meaning: "Tracked source changed or removed while no daemon was watching is not admitted yet, so the answer may describe bytes the working copy no longer holds; the daemon's catch-up takes it on its own, and `kin admit` takes it now.",
    },
    ClauseCode {
        code: "tracked_changes_unchecked",
        meaning: "The daemon could not check whether tracked source changed while no daemon was watching, so the answer may describe bytes the working copy no longer holds; `kin admit` settles it.",
    },
    ClauseCode {
        code: "unkeyed_lookups_counted",
        meaning: "Lookups by a name computed at run time could reach the focal only as they could reach anything their receiver holds: no record says anything about the name they look up and none spells a call name, which every reader that does not run the program shares as a blind spot, so the answer counts them in `focal_escape.residue.unkeyed_lookups` rather than listing them as sites to check. It never licenses an absence.",
    },
    ClauseCode {
        code: "unlisted_clause",
        meaning: "A reason this build carries no code for. The blocks the verdict's `inputs` name hold its facts. Seeing it is a Kin defect.",
    },
    ClauseCode {
        code: "value_calls_unmeasured",
        meaning: "Unsettled call sites that never spell the focal's call name could reach it only if it is held as a value or looked up by a computed name, and the escape census could not rule that out, so the answer counts them once with the census reason.",
    },
    ClauseCode {
        code: "walk_bounded",
        meaning: "The walk stopped at a work bound before its frontier emptied, so a route may exist beyond what was explored.",
    },
    ClauseCode {
        code: "walk_depth_bounded",
        meaning: "The walk stopped at max_depth before its frontier emptied; raise max_depth.",
    },
    ClauseCode {
        code: "watcher_events_lost",
        meaning: "The working-copy watcher lost change events no admission has covered; a running daemon retries a full admission, and `kin admit` runs it now.",
    },
    ClauseCode {
        code: "watcher_loss_unreadable",
        meaning: "The durable watcher-loss record could not be read, so recovery is unknown; `kin admit` rewrites it after a complete admission.",
    },
    ClauseCode {
        code: "withheld_candidates",
        meaning: "Same-name candidates are held out of the counts and carried in `candidates`.",
    },
    ClauseCode {
        code: "working_copy_unmeasured",
        meaning: "Nothing has measured the working copy, so whether graph truth is level with it is unknown.",
    },
];

/// The listed code a clause with an unlisted label is sent as.
pub const UNLISTED_CLAUSE_CODE: &str = "unlisted_clause";

/// The code a response the size budget bounded carries in the verdict.
const RESPONSE_BOUNDED_CODE: &str = "response_bounded";

/// The limiting factor a response bounded by the size budget carries.
const RESPONSE_BOUNDED_FACTOR: &str = "response_bounded: the response budget withheld part of \
                                       this answer, so its counts are a lower bound and its \
                                       absence claims are not authoritative";

/// The factor a reader acts on: the code of every reason the inputs gave.
///
/// The most pessimistic input decides the STATE; it does not make the other
/// inputs vanish. When the coverage reading and the run's own degradations both
/// refused, a factor that kept only the first sent a reader to fix the edge gap
/// and never told them the embedding worker had died, which is a second thing
/// wrong with the same answer (FIR-2672). So the factor carries one code per
/// reason, in the readings' order: the absence gate's own composition first,
/// then the coverage observation, withheld rows, the run's degradations and the
/// completeness signal, joined by [`CLAUSE_SEPARATOR`]. A code appears once,
/// because the absence gate already composes the class gap and the degradations
/// that the later readings repeat as named inputs.
///
/// The code is the label each clause was written under, and it is all that is
/// sent. Every fact the clause's sentence stated is a field of the block its
/// input names, and what the code means is written once, in
/// `docs/mcp-tools.md`, from [`CLAUSE_CODES`].
fn compose_limiting_factor(readings: &[(&str, Reading)]) -> Option<String> {
    let mut codes: Vec<&'static str> = Vec::new();
    for clause in compose_clauses(readings) {
        let code = listed_code(&clause_label(&clause));
        if !codes.contains(&code) {
            codes.push(code);
        }
    }
    (!codes.is_empty()).then(|| codes.join(CLAUSE_SEPARATOR))
}

/// Codes for gaps an answer counts beside itself rather than lists: kinds of
/// place that never held an answer unit, so they are not part of the answer's
/// uncertainty. Like a listed residue they leave the answer complete except
/// them and never license an absence.
pub(crate) const FOLDED_CODES: [&str; 1] = [LOCAL_BINDING_UNRELATED];

/// The code [`crate::source_derivation`] writes when every outstanding
/// binding obligation in scope is shown unrelated to the answer.
pub(crate) const LOCAL_BINDING_UNRELATED: &str = "local_binding_unrelated";

/// Whether `code` names a residue the answer lists or a gap it counts beside
/// itself: either leaves the answer complete except that residue.
pub(crate) fn is_residue_code(code: &str) -> bool {
    kin_model::call_site_reading::RESIDUE_CODES.contains(&code)
        || crate::declaration_coverage::RESIDUE_CODES.contains(&code)
        || FOLDED_CODES.contains(&code)
}

/// Whether every code a composed factor carries names a listed residue, so the
/// answer is complete except that residue ([`CERTIFIED_EXCEPT`]).
fn factor_is_residue_only(factor: &str) -> bool {
    let mut codes = factor
        .split(CLAUSE_SEPARATOR)
        .map(str::trim)
        .filter(|code| !code.is_empty())
        .peekable();
    codes.peek().is_some() && codes.all(is_residue_code)
}

/// The closed-list code for a clause label, or [`UNLISTED_CLAUSE_CODE`].
///
/// A label the list does not carry is a producer that wrote a code nobody
/// documented. It still refuses the answer, because the reading that carried it
/// refused, but it is sent as the one listed code that says so rather than as a
/// code no reader can look up.
fn listed_code(label: &str) -> &'static str {
    match CLAUSE_CODES.iter().find(|entry| entry.code == label) {
        Some(entry) => entry.code,
        None => {
            tracing::warn!(
                label,
                "a verdict clause carries a label the closed code list does not; it is sent as \
                 unlisted_clause"
            );
            UNLISTED_CLAUSE_CODE
        }
    }
}

/// `code` first, then every code `existing` already carries, each once.
///
/// The factor is codes joined by [`CLAUSE_SEPARATOR`] and no code holds the
/// separator, so splitting it recovers exactly the codes that were joined.
fn lead_with_code(existing: Option<&str>, code: &'static str) -> String {
    let mut codes = vec![code];
    for existing in existing.unwrap_or_default().split(CLAUSE_SEPARATOR) {
        let existing = existing.trim();
        if !existing.is_empty() && !codes.contains(&existing) {
            codes.push(existing);
        }
    }
    codes.join(CLAUSE_SEPARATOR)
}

/// The factor's clauses, deduplicated by label, in the readings' order.
///
/// Each refusing reading hands over the clauses it built. Nothing is split back
/// out of a joined string here, which is the whole change: the separator is also
/// ordinary punctuation inside a clause's prose, so a boundary parsed from it is
/// a boundary no author chose. `cross_file_edges_absent` and
/// `name_filter_narrowed_to_zero` both carry one, and each used to arrive as a
/// labelled clause plus a bare fragment that reached the reader with no label at
/// all.
fn compose_clauses(readings: &[(&str, Reading)]) -> Vec<String> {
    let mut labels: Vec<String> = Vec::new();
    let mut clauses: Vec<String> = Vec::new();
    for (_, reading) in readings {
        let Reading::Inconclusive(reasons) = reading else {
            continue;
        };
        for clause in reasons.iter().map(|clause| clause.trim()) {
            if clause.is_empty() {
                continue;
            }
            let label = clause_label(clause);
            if labels.contains(&label) {
                continue;
            }
            labels.push(label);
            clauses.push(clause.to_string());
        }
    }
    clauses
}

/// The gap a clause names, which is the text before its first colon.
///
/// Two readings that notice the same gap say it once (FIR-2672), and this is
/// how they are recognised as the same. It is still inferred rather than stated,
/// which is a known limit recorded on FIR-2723: nothing enforces that two
/// readings sharing a prefix describe the same gap. What it no longer does is
/// invent clause boundaries, so a label is always a label an author wrote.
fn clause_label(clause: &str) -> String {
    clause
        .split(':')
        .next()
        .unwrap_or(clause)
        .trim()
        .to_string()
}

/// What this response says about absence, which is not what it says about trust.
///
/// One bool answered two questions and a reader could not branch on it
/// (FIR-2673 finding 1). `safe_to_conclude_absent: false` meant either "this
/// absence is not trustworthy" or "absence is not a concept for this call", and
/// the second is the common case: a qualifier rides every retrieval answer, so
/// the field is false on every answer that returned rows. A stranger read one
/// `list_file_entities` response carrying `state: certified`, a note saying an
/// absence in it is authoritative, and `safe_to_conclude_absent: false`, all in
/// one object, and reported that the boolean gives the opposite of the verdict.
///
/// Three states, because there were always three cases.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum AbsenceClaim {
    /// This answer returned rows and asserts no absence, so there is nothing to
    /// conclude absent and nothing to distrust. The case that used to be
    /// indistinguishable from a refusal.
    NotApplicable,
    /// An absence is claimed and every input that could qualify it agreed.
    Authoritative,
    /// An absence is claimed and at least one input refuses, so it may not be
    /// acted on.
    NotAuthoritative,
}

impl AbsenceClaim {
    fn as_str(self) -> &'static str {
        match self {
            AbsenceClaim::NotApplicable => "not_applicable",
            AbsenceClaim::Authoritative => "authoritative",
            AbsenceClaim::NotAuthoritative => "not_authoritative",
        }
    }

    /// The legacy boolean, defined FROM the tri-state so the two can never
    /// disagree.
    ///
    /// Kept because ten shipped tool descriptions name the field, `kin-bench`
    /// branches on its negative-side twin, and two kinlab launch-copy strings
    /// and a published blog post describe it. It is bit-identical to what
    /// `certified && makes_absence_claim` produced.
    fn legacy_bool(self) -> bool {
        matches!(self, AbsenceClaim::Authoritative)
    }
}

/// One input's reading of the same answer.
enum Reading {
    /// The input observed nothing that stops this answer being acted on.
    Certified,
    /// The input refuses, and says why in the clauses it carries.
    ///
    /// A list rather than one string, so the factor is never parsed back out of
    /// a joined sentence. Most readings build exactly one clause; the absence
    /// gate composes several, and it is the one whose prose contains the same
    /// separator the renderer uses.
    Inconclusive(Vec<String>),
    /// The input has nothing to say about this response, which is different from
    /// saying it is fine. A silent input never certifies anything.
    Silent,
}

impl Reading {
    fn state(&self) -> &'static str {
        match self {
            Reading::Certified => CERTIFIED,
            Reading::Inconclusive(_) => INCONCLUSIVE,
            Reading::Silent => NOT_APPLICABLE,
        }
    }
}

/// The single verdict for one response, plus the inputs it was computed from.
pub struct Verdict {
    /// The candidate scorer's report, lifted from the payload's top-level
    /// `scorer` block unchanged, when a retrieval arm ran one.
    scorer: Option<Value>,
    certified: bool,
    /// Not certified, and every clause that refused names a listed residue
    /// ([`CERTIFIED_EXCEPT`]).
    except_residue: bool,
    absence_claim: AbsenceClaim,
    limiting_factor: Option<String>,
    inputs: Map<String, Value>,
}

impl Verdict {
    /// Compute the response's one verdict, or `None` for a response no input
    /// spoke about (a mutation, a session call, anything that is not retrieval).
    ///
    /// `negative` is the already-built absence object when one was synthesized.
    /// It is an input rather than the authority: it exists only for an answer
    /// that came back empty, and the contradiction this module exists to end was
    /// found on a NON-empty answer whose only verdict-shaped claim was
    /// `completeness`.
    ///
    /// Certification requires every input that spoke to agree, and an input that
    /// stayed silent never contributes agreement. A response where no input
    /// spoke returns `None` rather than a certified verdict on no evidence.
    pub fn compute(
        tool: &str,
        payload: &Value,
        envelope: &Envelope,
        negative: Option<&Value>,
    ) -> Option<Self> {
        // A qualifier now rides every retrieval answer, so its presence no longer
        // means an absence is being claimed. The object says which it is, and
        // reading that rather than its existence is what keeps
        // `safe_to_conclude_absent` false on a populated answer.
        let makes_absence_claim = negative.is_some_and(|negative| {
            negative.get("interpretation").and_then(Value::as_str) != Some("qualified_answer")
        });
        let readings = [
            // First, because it is the only input here that says the SUBSTRATE
            // was fed incompletely. Every other reading below is about what this
            // answer could see in graph truth; this one says graph truth itself
            // is missing an unknown set of paths, which is the fact that
            // qualifies all of them.
            ("watcher_loss", watcher_loss_reading(envelope)),
            // Beside the watcher loss, for the same reason: it says graph truth
            // itself holds bytes the working copy no longer does, which
            // qualifies every reading below it, a populated answer included.
            ("tracked_changes", tracked_changes_reading(envelope)),
            (
                "absence_gate",
                absence_gate_reading(tool, payload, negative),
            ),
            // Right after the absence gate, whose composition already carries
            // this reading's clauses under the same labels, so the factor names
            // each gap once and the input still says it was read.
            (
                "caller_arrival",
                caller_arrival_reading(tool, payload, makes_absence_claim),
            ),
            // The sites in the answer's own scope, on every shape of answer: a
            // site whose call no resolver settled bounds the rows an answer did
            // return as much as the absence it did not.
            ("call_sites", call_sites_reading(payload)),
            (
                "reference_completion",
                if tool == "find_references" {
                    reference_completion_reading(payload)
                } else {
                    Reading::Silent
                },
            ),
            (
                "undecided_reference_sites",
                match undecided_reference_sites_gap(payload) {
                    Some(gap) => Reading::Inconclusive(vec![gap]),
                    None => Reading::Silent,
                },
            ),
            (
                "impact_consumer_absence",
                match impact_consumer_absence_gaps(payload) {
                    gaps if gaps.is_empty() => Reading::Silent,
                    gaps => Reading::Inconclusive(gaps),
                },
            ),
            ("enrichment", enrichment_reading(payload)),
            ("edge_coverage", edge_coverage_reading(tool, payload)),
            ("withheld_candidates", withheld_candidates_reading(payload)),
            ("degradations", degradations_reading(payload)),
            ("scorer", scorer_reading(payload)),
            ("cross_repo", cross_repo_reading(tool, payload)),
            ("outside_graph", outside_graph_reading(payload)),
            ("source_body", source_body_reading(tool, payload)),
            ("named_in", named_in_reading(tool, payload)),
            ("completeness", completeness_reading(envelope)),
            ("graph_freshness", graph_freshness_reading(envelope)),
            (
                "source_derivation",
                match envelope.source_limiting_factor(if tool == "entity_history" {
                    crate::envelope::AbsenceSubstrate::History
                } else {
                    crate::envelope::AbsenceSubstrate::Relations
                }) {
                    Some(reason) => Reading::Inconclusive(vec![reason.into()]),
                    None => Reading::Silent,
                },
            ),
            // Last, so where the absence gate already named an empty graph its clause
            // leads and this one is folded into it by label. It is the only input
            // that speaks on an answer with no absence gate at all, which is the
            // status answer, where a zero otherwise went out with no verdict.
            ("graph_empty", graph_empty_reading(envelope)),
        ];

        if readings
            .iter()
            .all(|(_, reading)| matches!(reading, Reading::Silent))
        {
            return None;
        }

        let limiting_factor = compose_limiting_factor(&readings);
        let certified = limiting_factor.is_none();
        let except_residue = limiting_factor
            .as_deref()
            .is_some_and(factor_is_residue_only);
        let inputs = readings
            .iter()
            // The scorer input exists only on a retrieval arm that runs one, so
            // every other answer's verdict lists exactly the inputs it did before.
            .filter(|(name, reading)| {
                !matches!(
                    *name,
                    "scorer"
                        | "reference_completion"
                        | "undecided_reference_sites"
                        | "impact_consumer_absence"
                        | "named_in"
                ) || !matches!(reading, Reading::Silent)
            })
            .map(|(name, reading)| ((*name).to_string(), json!(reading.state())))
            .collect();

        Some(Verdict {
            scorer: scorer_block(payload),
            certified,
            except_residue,
            absence_claim: match (makes_absence_claim, certified) {
                (false, _) => AbsenceClaim::NotApplicable,
                (true, true) => AbsenceClaim::Authoritative,
                (true, false) => AbsenceClaim::NotAuthoritative,
            },
            limiting_factor,
            inputs,
        })
    }

    /// Gaps this response carries that [`crate::negative::negative_for`] cannot
    /// observe on its own, in the order they should lead the composed reason.
    ///
    /// These are threaded INTO the absence object rather than patched onto it
    /// afterwards, so its `trust`, `trust_reason` and `advice` are one
    /// consistent sentence. Patching a built advice string is how a verdict and
    /// the reason a reader acts on come apart again.
    ///
    /// Only gaps the absence gates structurally cannot reach are listed here.
    /// Withheld same-name candidates are already disclosed as a payload
    /// degradation by every tool that withholds them today, and the absence gate
    /// consumes `degradations`, so repeating them would put the same fact in one
    /// reason twice.
    pub fn pre_negative_gaps(payload: &Value) -> Vec<String> {
        let mut gaps = Vec::new();
        // An unsettled call-site block's clauses are pushed below; a reference
        // gap under one of their labels (the unkeyed lookups both count) is
        // the same gap, said once.
        let call_site_labels: Vec<String> = payload
            .get(crate::call_sites::CALL_SITES_KEY)
            .filter(|block| block.get("settled").and_then(Value::as_bool) == Some(false))
            .map(call_sites_clauses)
            .unwrap_or_default()
            .iter()
            .map(|clause| clause_label(clause))
            .collect();
        if let Some(gap) = reference_completion_gap(payload)
            .filter(|gap| !call_site_labels.contains(&clause_label(gap)))
        {
            gaps.push(gap);
        }
        gaps.extend(impact_consumer_absence_gaps(payload));
        if payload.get("truncated").and_then(Value::as_bool) == Some(true) {
            gaps.push(
                "answer_truncated: this answer stopped early and returned part of what it \
                 found, so its counts are a floor and an absence in it is a fact about where \
                 the walk stopped"
                    .to_string(),
            );
        }
        // The answer's own call sites, so the absence object and the verdict
        // read the same gap: a verdict refused on an unsettled site under an
        // absence object that stayed authoritative would be two verdicts.
        if let Some(block) = payload.get(crate::call_sites::CALL_SITES_KEY) {
            if block.get("settled").and_then(Value::as_bool) == Some(false) {
                gaps.extend(call_sites_clauses(block));
            }
        }
        if withheld_candidate_count(payload).is_some_and(|withheld| withheld > 0)
            && !discloses_withheld_candidates(payload)
        {
            gaps.push(
                "withheld_candidates: same-name candidates were held out of the counts here, \
                 so the headline is a floor and the withheld rows may belong in it"
                    .to_string(),
            );
        }
        gaps
    }

    /// Cap the completeness signal's verdict-shaped fields at this verdict.
    ///
    /// Only ever downgrades. `bound` and `counted.exact` are what a reader acts
    /// on, so they follow the one verdict; `status`, `classes`,
    /// `decided_by` and `limits` are the observation the verdict was computed
    /// from and stay exactly as measured.
    /// The qualifiers that stop the `edge_coverage` block licensing a
    /// certification on its own, named by the input that limits it.
    ///
    /// The block reports what a scan observed. It does not, and must not, report
    /// whether the answer around it can be trusted as whole, and a reader with
    /// only the block in hand cannot tell those apart: "every requested class is
    /// present" and "this answer's completeness is unknown" are both true at
    /// once, and the block renders only the first. A suite reading it alone
    /// therefore graded it as certifying beside a completeness that refused, and
    /// that is one response with two verdicts.
    ///
    /// This never re-derives another block's state. It reads the states this
    /// verdict already computed, which is why it lives here: `compute` is the
    /// one place holding every input, so the block's self-presentation and the
    /// verdict cannot drift. An empty list means the block licenses on its own.
    ///
    /// The class states themselves are left alone. A class that is present IS
    /// present, and reporting it otherwise to settle a disagreement would make a
    /// true fact read false, which is the failure this envelope exists to stop.
    /// The list is DERIVED from `self.inputs` rather than written out, because
    /// "every input except `edge_coverage`" spelled as four names is only
    /// correct until a fifth arrives. A name this function has never heard of
    /// yields a shorter list, not an error, so the block would go on presenting
    /// itself as licensing on its own while a reading nobody enumerated here
    /// refused. That is the edge-class trap in its other form: the hazard is not
    /// a consumer that counts a class, it is a producer that enumerates the
    /// classes it knows.
    ///
    /// Sorted so the stamp is byte-stable. `Map` is a `BTreeMap` unless
    /// serde_json's `preserve_order` is enabled, and a wire field's order should
    /// not depend on a feature flag in a transitive dependency.
    pub fn edge_coverage_limits(&self) -> Vec<String> {
        if self.inputs.get("edge_coverage").and_then(Value::as_str) != Some(CERTIFIED) {
            // The block already qualifies itself; nothing to add.
            return Vec::new();
        }
        let mut limits: Vec<String> = self
            .inputs
            .iter()
            .filter(|(name, state)| {
                name.as_str() != "edge_coverage" && state.as_str() == Some(INCONCLUSIVE)
            })
            .map(|(name, _)| format!("{name}:inconclusive"))
            .collect();
        limits.sort();
        limits
    }

    pub fn project_onto_completeness(
        &self,
        completeness: &mut Option<crate::envelope::Completeness>,
    ) {
        if self.certified {
            return;
        }
        let Some(completeness) = completeness else {
            return;
        };
        // `status` follows the verdict too. It reads as a claim that the answer
        // is whole, whatever it was derived from, and the stranger's report named
        // it first: `status: "complete"` sat directly above a `classes` map
        // marking two of three entries absent, and above a `negative` refusing
        // the same zero. The evidence it was computed from is not lost, because
        // `classes`, `decided_by` and `limits` are published unchanged beside it;
        // what changes is that the one-word summary can no longer say "whole"
        // while the response's verdict says otherwise.
        //
        // `partial` when something was actually observed short of whole,
        // `unknown` otherwise, because a verdict refused for a reason no class
        // captures is not evidence that a class was missing. `partial` is on
        // this list because the embedding class publishes it: an index reading
        // 18123 of 18124 was observed, and reporting that observation as
        // `unknown` would say nobody looked.
        completeness.status = if completeness.classes.values().any(|state| {
            matches!(
                state.as_str(),
                Some("absent") | Some("unproduced") | Some("partial")
            )
        }) {
            "partial".to_string()
        } else {
            "unknown".to_string()
        };
        completeness.bound = "at_least".to_string();
        if let Some(counted) = completeness.counted.as_mut().and_then(Value::as_object_mut) {
            counted.insert("exact".to_string(), json!(false));
        }
        let limit = if self.except_residue {
            VERDICT_EXCEPT_LIMIT
        } else {
            VERDICT_LIMIT
        };
        if !completeness.limits.iter().any(|existing| existing == limit) {
            completeness.limits.push(limit.to_string());
        }
    }

    /// The state this verdict serializes as.
    fn state(&self) -> &'static str {
        if self.certified {
            CERTIFIED
        } else if self.except_residue {
            CERTIFIED_EXCEPT
        } else {
            INCONCLUSIVE
        }
    }

    /// One verdict for an answer made of sections, each a whole answer about
    /// its own focal: a name several entities carry, answered for every one of
    /// them under `candidates_by_owner`.
    ///
    /// The top level of such an answer carries no rows, no coverage and no
    /// ledger of its own, so it is never graded as a single-focal answer: that
    /// would read an absent `edge_coverage` block as a gap in a reference list
    /// it never had. What it does report about itself is its `degradations`,
    /// which name a candidate the section cap left unanswered, and that is the
    /// one reading taken from it. Every section's verdict is composed with it
    /// the way readings are composed within one answer: certified only when
    /// every one of them is, the limiting factor carrying each code once in the
    /// order they were named, and each input at the most pessimistic state any
    /// of them read. The whole makes no absence claim of its own, because each
    /// section claims or declines its own absence under its own verdict, which
    /// is where a reader looking at one candidate reads it.
    pub fn compose_sections(payload: &Value, sections: Vec<Self>) -> Option<Self> {
        if sections.is_empty() {
            return None;
        }
        let rank = |state: Option<&str>| match state {
            Some(INCONCLUSIVE) => 2,
            Some(CERTIFIED) => 1,
            _ => 0,
        };
        let own = [("degradations", degradations_reading(payload))];
        let mut certified = !matches!(own[0].1, Reading::Inconclusive(_));
        let mut codes: Vec<String> = compose_limiting_factor(&own)
            .map(|factor| factor.split(CLAUSE_SEPARATOR).map(str::to_string).collect())
            .unwrap_or_default();
        let mut inputs: Map<String, Value> = Map::new();
        inputs.insert("degradations".to_string(), json!(own[0].1.state()));
        for verdict in &sections {
            certified &= verdict.certified;
            for code in verdict
                .limiting_factor
                .iter()
                .flat_map(|factor| factor.split(CLAUSE_SEPARATOR))
                .map(str::trim)
                .filter(|code| !code.is_empty())
            {
                if !codes.iter().any(|known| known == code) {
                    codes.push(code.to_string());
                }
            }
            for (name, state) in &verdict.inputs {
                if !inputs.contains_key(name)
                    || rank(state.as_str()) > rank(inputs.get(name).and_then(Value::as_str))
                {
                    inputs.insert(name.clone(), state.clone());
                }
            }
        }
        let limiting_factor = (!codes.is_empty()).then(|| codes.join(CLAUSE_SEPARATOR));
        Some(Verdict {
            scorer: None,
            certified: certified && codes.is_empty(),
            except_residue: limiting_factor
                .as_deref()
                .is_some_and(factor_is_residue_only),
            absence_claim: AbsenceClaim::NotApplicable,
            limiting_factor,
            inputs,
        })
    }

    /// Serialize for embedding under `_kin.verdict`.
    pub fn to_value(&self) -> Value {
        let mut value = json!({
            "state": self.state(),
            "absence_claim": self.absence_claim.as_str(),
            "safe_to_conclude_absent": self.absence_claim.legacy_bool(),
            "limiting_factor": self.limiting_factor.clone().map(Value::String).unwrap_or(Value::Null),
            "inputs": Value::Object(self.inputs.clone()),
        });
        if let Some(remedy) = remedy_for(self.limiting_factor.as_deref()) {
            value["remedy"] = Value::String(remedy);
        }
        if let Some(scorer) = &self.scorer {
            value["scorer"] = scorer.clone();
        }
        value
    }
}

/// The one step that lifts a store-wide limit, when a limiting factor names
/// one, as a sentence an agent can pass to the user.
///
/// Only the store-level codes carry one. Every other clause is about this
/// answer, and the codes themselves already say what it rests on.
fn remedy_for(limiting_factor: Option<&str>) -> Option<String> {
    let codes: Vec<&str> = limiting_factor?.split(CLAUSE_SEPARATOR).collect();
    if codes.contains(&"store_semantics_behind") {
        return Some(format!(
            "Run `kin upgrade` in this repository (`{}` through npm): an older version of Kin \
             built this repository's records, and no answer from them can be complete until \
             they are rebuilt.",
            kin_core::hydration_semantics::npm_upgrade_command()
        ));
    }
    if codes.contains(&"store_semantics_unknown") {
        return Some(format!(
            "Upgrade Kin to the newest version, then run `kin upgrade` in this repository (`{}` \
             through npm): the note of how this repository's records were built is missing or \
             unreadable.",
            kin_core::hydration_semantics::NPM_LATEST_UPGRADE_COMMAND
        ));
    }
    None
}

/// The absence gate's reading: the composed verdict
/// [`crate::negative::negative_for`] already reached, when it built one.
///
/// When it did not, the gate is run here against the same payload rather than
/// skipped. That is the whole point of computing this once: a non-empty answer
/// and a tool with no absence spec are exactly the routes that used to reach a
/// reader with no gate applied at all, and the missing dependency that stops an
/// absence being certified is the same missing dependency that makes a
/// non-empty answer a floor.
fn absence_gate_reading(tool: &str, payload: &Value, negative: Option<&Value>) -> Reading {
    if let Some(negative) = negative {
        // `trust`, not `safe_to_conclude_absent`. The two answer different
        // questions and only the first is this response's verdict: a populated
        // answer sets `safe_to_conclude_absent` false because it claims no
        // absence, and reading that as the gate refusing made every answer with
        // rows inconclusive on its own success.
        return match negative.get("trust").and_then(Value::as_str) {
            Some("authoritative") => Reading::Certified,
            // `trust_reason` arrives as one string off the wire, and it is a
            // JOIN of clauses: `push_gap` builds it by repeated
            // `format!("{trust_reason}; {gap}")` across seven call sites. So it
            // is split back into clauses here, and that is NOT the boundary
            // inference this change removed elsewhere.
            //
            // The difference is which side owns the string. Splitting a factor
            // Kin had just joined itself was inventing a boundary; splitting a
            // wire string that IS a join recovers one, and it is safe now
            // because no clause carries the separator, which
            // `no_clause_carries_the_separator_that_divides_clauses` asserts on
            // the producer.
            //
            // Taking it as one clause instead was a real regression, caught by
            // the `two_reasons` acceptance check on a live payload rather than
            // by any unit test: `trust_reason` carries a `retrieval_degraded`
            // clause of its own, and as one opaque blob it stopped deduplicating
            // against the `degradations` reading's, so one answer named that gap
            // twice. The dedupe was doing real work on this path.
            Some(_) => Reading::Inconclusive(
                negative
                    .get("trust_reason")
                    .and_then(Value::as_str)
                    .unwrap_or("the absence gate refused and reported no reason")
                    .split(CLAUSE_SEPARATOR)
                    .map(str::trim)
                    .filter(|clause| !clause.is_empty())
                    .map(str::to_string)
                    .collect(),
            ),
            None => Reading::Silent,
        };
    }
    // The clauses, never the joined rendering. This is the path the probe on
    // FIR-2723 caught: `cross_file_edges_absent`'s text carries a semicolon, so
    // joining here and splitting in the composer cut it into a labelled clause
    // and a bare fragment that reached the reader with no label at all.
    let clauses = crate::negative::absence_coverage_clauses(tool, payload);
    if !clauses.is_empty() {
        return Reading::Inconclusive(clauses);
    }
    if crate::negative::declares_absence_dependency(tool, payload) {
        return Reading::Certified;
    }
    Reading::Silent
}

/// The caller-arrival reading, named as its own input.
///
/// Its gaps reach the verdict through the absence gate too, which folds every
/// gap `negative` pushed into one reading, so a certified verdict used to cite
/// `absence_gate` and never the arrival reading it certified over. Named here,
/// `inputs.caller_arrival` says the answer's `caller_arrival` block was read and
/// what it said, and the block's `scope` says what that reading can and cannot
/// see.
///
/// It reads the block with the functions the absence gate's own gaps come from,
/// and only where that gate reads it: an answer from `find_references`,
/// `get_context_pack` or `impact_analysis` that claims an absence. A populated
/// reference list claims none, and a shortfall in the arrival paths says
/// nothing about whether the rows it did return are real. An impact answer
/// that reported no entity without consumers had nothing for the reading to
/// qualify, and says so with `not_applicable`.
fn caller_arrival_reading(tool: &str, payload: &Value, makes_absence_claim: bool) -> Reading {
    if !makes_absence_claim {
        return Reading::Silent;
    }
    let Some(state) = payload
        .get(crate::caller_arrival::CALLER_ARRIVAL_KEY)
        .and_then(|block| block.get("state"))
        .and_then(Value::as_str)
    else {
        return Reading::Silent;
    };
    let gaps: Vec<String> = match tool {
        "find_references" | "get_context_pack" => crate::caller_arrival::arrival_gap(payload)
            .into_iter()
            .chain(crate::caller_arrival::owed_outside_gap(payload))
            .collect(),
        "impact_analysis" if state == crate::caller_arrival::IMPACT_ARRIVAL_NOT_APPLICABLE => {
            return Reading::Silent
        }
        "impact_analysis" => crate::caller_arrival::impact_arrival_gaps(payload),
        _ => return Reading::Silent,
    };
    if gaps.is_empty() {
        Reading::Certified
    } else {
        Reading::Inconclusive(gaps)
    }
}

/// The `call_sites` block's reading: inconclusive with the block's own clauses
/// while a site in its scope is not settled, certified when every one is, and
/// silent on an answer that carries no block.
///
/// The block is [`crate::call_sites`]'s, taken over the answer's scope through
/// the one site-state reading every surface shares, and it states its clauses
/// itself, so this reading and the block cannot disagree about what is
/// unsettled. A block that says it is unsettled and names no clause is one no
/// producer here writes, and it still refuses rather than certifying on no
/// evidence.
/// The report observes the selected graph's persisted ledgers. A settled
/// call-site observation is deliberately silent about all other relation types.
///
/// Every clause the observation names, joined the way `trust_reason` joins
/// gaps, or `None` while it records no call-site debt.
pub(crate) fn enrichment_gap(payload: &Value) -> Option<String> {
    let clauses = enrichment_gaps(payload);
    (!clauses.is_empty()).then(|| clauses.join(CLAUSE_SEPARATOR))
}

/// The clauses a bounded enrichment observation names, one per code, each
/// under the code its pending entities call for. Only owed work is
/// `enrichment_incomplete`: an observation that counts no owed entity never
/// reads as unfinished, because waiting would not change it.
fn enrichment_gaps(payload: &Value) -> Vec<String> {
    let Some(observation) = payload
        .get("enrichment")
        .or_else(|| payload.pointer("/impact/enrichment"))
    else {
        return Vec::new();
    };
    if observation.get("status").and_then(Value::as_str) != Some("bounded") {
        return Vec::new();
    }
    let clauses: Vec<String> = observation
        .get("clauses")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|clause| !clause.is_empty())
        .map(str::to_string)
        .collect();
    if !clauses.is_empty() {
        return clauses;
    }
    if let Some(limitation) = observation
        .get("limitation")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|limitation| !limitation.is_empty())
    {
        return limitation
            .split(CLAUSE_SEPARATOR)
            .map(str::trim)
            .filter(|clause| !clause.is_empty())
            .map(str::to_string)
            .collect();
    }
    let owed = observation
        .get("owed_entities")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if owed > 0 {
        return vec![format!(
            "enrichment_incomplete: {owed} entities have call-site evidence the graph has not \
             settled yet because their derivation or enrichment is still owed, so impact counts \
             are a lower bound and review risk may change"
        )];
    }
    let codes: Vec<String> = observation
        .get("pending_by_code")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .map(|(code, count)| {
            format!(
                "{code}: {} entities hold call-site evidence no resolver settled, so impact \
                 counts are a lower bound and review risk may change",
                count.as_u64().unwrap_or(0)
            )
        })
        .collect();
    if !codes.is_empty() {
        return codes;
    }
    // Bounded, and nothing says by what. Not owed work, which it never
    // claimed, and still never a certification.
    vec![format!(
        "{UNLISTED_CLAUSE_CODE}: the enrichment observation bounds this answer and names no \
         clause"
    )]
}

fn enrichment_reading(payload: &Value) -> Reading {
    let clauses = enrichment_gaps(payload);
    if clauses.is_empty() {
        Reading::Silent
    } else {
        Reading::Inconclusive(clauses)
    }
}

fn call_sites_reading(payload: &Value) -> Reading {
    let Some(block) = payload.get(crate::call_sites::CALL_SITES_KEY) else {
        return Reading::Silent;
    };
    match block.get("settled").and_then(Value::as_bool) {
        Some(true) => Reading::Certified,
        Some(false) => Reading::Inconclusive(call_sites_clauses(block)),
        None => Reading::Silent,
    }
}

/// The non-call relation kinds a reference answer asked for, or `None` when
/// it is no reference answer or asked for calls alone.
fn requested_non_call_kinds(payload: &Value) -> Option<Vec<&str>> {
    payload.get("references")?.as_array()?;
    let kinds: Vec<&str> = payload
        .get("relation_kinds")?
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .collect();
    kinds
        .iter()
        .any(|kind| !kind.eq_ignore_ascii_case("calls"))
        .then_some(kinds)
}

/// Undecided sites a `reference_sites_undecided_listed` clause names before it
/// points at the block for the rest.
const UNDECIDED_SITES_NAMED: usize = 5;

/// The gap the answer's `undecided_reference_sites` block names, or `None`
/// when it carries none (see [`crate::undecided_sites`]).
///
/// A site no pass could decide over unchanged code is served as a reference
/// that nothing proves, so an answer that holds one is never complete because
/// of it. Listed whole, the clause is a residue: the answer is complete except
/// those sites. Counted beyond its list, or unreadable, it is partial.
fn undecided_reference_sites_gap(payload: &Value) -> Option<String> {
    use kin_model::call_site_reading::{
        REFERENCE_SITES_UNDECIDED, REFERENCE_SITES_UNDECIDED_LISTED, UNDECIDED_REFERENCE_SITES_KEY,
    };
    let block = payload.get(UNDECIDED_REFERENCE_SITES_KEY)?;
    let sites = block
        .get("sites")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let Some(count) = block.get("count").and_then(Value::as_u64) else {
        return Some(format!(
            "{REFERENCE_SITES_UNDECIDED}: the reference sites no pass could decide could not \
             be read, so usages there may be missing"
        ));
    };
    if count == 0 {
        return None;
    }
    if sites.len() as u64 >= count {
        let named: Vec<String> = sites
            .iter()
            .take(UNDECIDED_SITES_NAMED)
            .map(|site| {
                let caller = site
                    .get("caller_name")
                    .and_then(Value::as_str)
                    .filter(|name| !name.is_empty())
                    .or_else(|| site.get("caller").and_then(Value::as_str))
                    .unwrap_or("an unnamed caller");
                match site.get("line_in_entity").and_then(Value::as_u64) {
                    Some(line) => format!("{caller} +{line}"),
                    None => caller.to_string(),
                }
            })
            .collect();
        let rest = count.saturating_sub(named.len() as u64);
        let mut at = named.join(", ");
        if rest > 0 {
            at.push_str(&format!(" and {rest} more"));
        }
        return Some(
            format!(
                "{REFERENCE_SITES_UNDECIDED_LISTED}: {count} reference sites stayed undecided \
                 after repeated sweeps of unchanged code, at {at}, each listed in \
                 {UNDECIDED_REFERENCE_SITES_KEY}"
            )
            .replace(CLAUSE_SEPARATOR, ", "),
        );
    }
    Some(format!(
        "{REFERENCE_SITES_UNDECIDED}: {count} reference sites stayed undecided after repeated \
         sweeps of unchanged code and {} are listed, so usages there may be missing",
        sites.len()
    ))
}

/// The `reference_completion` input: silent unless the answer asked for a
/// non-call relation, inconclusive while [`reference_completion_gap`] names
/// a gap, and certified when the census measured every non-call read that
/// could read the focal and a stored row of a requested kind carries each.
fn reference_completion_reading(payload: &Value) -> Reading {
    if requested_non_call_kinds(payload).is_none() {
        return Reading::Silent;
    }
    match reference_completion_gap(payload) {
        Some(gap) => Reading::Inconclusive(vec![gap]),
        None => Reading::Certified,
    }
}

/// A zero consumer count covers calls, imports and references. Certifying the
/// direct call-site section alone cannot certify that broader absence. Read
/// each zero against its own mention census; another changed entity's complete
/// census never licenses it. Positive consumer rows do not claim absence.
fn impact_consumer_absence_gaps(payload: &Value) -> Vec<String> {
    let Some(impacts) = payload.get("entity_impacts").and_then(Value::as_array) else {
        return Vec::new();
    };
    let call_sites = payload.get(crate::call_sites::CALL_SITES_KEY);
    let mut gaps = Vec::new();
    for row in impacts
        .iter()
        .filter(|row| row.get("consumer_count").and_then(Value::as_u64) == Some(0))
    {
        let target = row.get("entity_id").and_then(Value::as_str);
        let matches_target = |block: &&Value| {
            target.is_some() && block.get("entity_id").and_then(Value::as_str) == target
        };
        let census = call_sites.filter(matches_target).or_else(|| {
            call_sites?
                .get("focals")?
                .as_array()?
                .iter()
                .find(matches_target)
        });
        let reference_view = json!({
            "references": [],
            "relation_kinds": ["calls", "imports", "references"],
            "call_sites": census,
        });
        if let Some(gap) = reference_completion_gap(&reference_view) {
            let target = target.unwrap_or("an unidentified changed entity");
            gaps.push(format!("{gap} for the zero consumer count of {target}"));
        }
    }
    gaps
}

/// Sites a `reference_mentions_unmatched` clause names before it points at
/// the residue for the rest.
const UNMATCHED_SITES_NAMED: usize = 5;

/// Whether the requested non-call relations are complete, read from the
/// escape census's mention reading (`call_sites.focal_escape.residue.mentions`,
/// see [`kin_model::MentionResidue`]), or the gap that stops them being.
///
/// A Call census settles calls alone, and an edge class being present says
/// nothing about any one read. Completion of `imports` and `references` holds
/// only when the census matched every recorded non-call read that could read
/// the focal (a mention, a value use, an alias or import site) against the
/// stored rows:
///
/// - no mention reading was taken, or it was taken over records that predate
///   it or regions no record covers: `reference_completion_unmeasured`, with
///   the census's own reason;
/// - a lookup by a name computed at run time could reach the focal: the same
///   residue the call census counts, `unkeyed_lookups_counted`, bounds the
///   non-call reads too;
/// - a read no stored row of a requested kind carries: a
///   `reference_mentions_unmatched` clause naming each by file and line, all
///   of them listed in `residue.mentions.unmatched`.
///
/// Reads that hold the focal as a value, keyed reflective sites, implicit
/// calls and protocol dispatch are listed by the call census and refuse
/// through its own clause.
fn reference_completion_gap(payload: &Value) -> Option<String> {
    use kin_model::call_site_reading::{
        REFERENCE_COMPLETION_UNMEASURED, REFERENCE_MENTIONS_UNMATCHED, UNKEYED_LOOKUPS_COUNTED,
    };
    let requested = requested_non_call_kinds(payload)?;
    // Word for word what every reference answer carried before the census
    // took a mention reading, so a store whose records predate it answers
    // exactly as it did. Why it was not measured is the census's own
    // `residue.mentions.reason`.
    let unmeasured = || {
        Some(format!(
            "{REFERENCE_COMPLETION_UNMEASURED}: non-call completion is unmeasured"
        ))
    };
    let Some(census) = payload
        .get(crate::call_sites::CALL_SITES_KEY)
        .and_then(|block| block.get("focal_escape"))
    else {
        return unmeasured();
    };
    let residue = census.get("residue");
    let Some(mentions) =
        residue.and_then(|residue| residue.get(kin_model::call_site_reading::MENTIONS_KEY))
    else {
        return unmeasured();
    };
    if mentions.get("measured").and_then(Value::as_bool) != Some(true) {
        return unmeasured();
    }
    // A focal that holds members is referenced through them without its name
    // being spelled, which the reading cannot see; an import of it is still
    // measured.
    if mentions.get("references_unmeasured").is_some()
        && requested
            .iter()
            .any(|kind| kind.eq_ignore_ascii_case("references"))
    {
        return unmeasured();
    }
    let unkeyed = residue
        .and_then(|residue| residue.pointer("/unkeyed_lookups/count"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if unkeyed > 0 {
        return Some(format!(
            "{UNKEYED_LOOKUPS_COUNTED}: {unkeyed} lookups by a name computed at run time could \
             reach the focal as they could reach anything their receiver holds, so a non-call \
             read through one is counted in focal_escape.residue.unkeyed_lookups rather than \
             matched against a stored row"
        ));
    }
    let unmatched = mentions.get("unmatched");
    let listed = unmatched
        .and_then(|list| list.get("count"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let requested_kind = |kind: &str| {
        requested
            .iter()
            .any(|asked| asked.eq_ignore_ascii_case(kind))
    };
    let mut unrequested: Vec<(String, u64)> = mentions
        .get("matched")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter(|(kind, _)| !requested_kind(kind))
        .filter_map(|(kind, count)| Some((kind.clone(), count.as_u64()?)))
        .filter(|(_, count)| *count > 0)
        .collect();
    unrequested.sort();
    let carried_elsewhere: u64 = unrequested.iter().map(|(_, count)| count).sum();
    if listed == 0 && carried_elsewhere == 0 {
        return None;
    }
    let mut parts = Vec::new();
    if listed > 0 {
        let named: Vec<String> = unmatched
            .and_then(|list| list.get("sites"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .take(UNMATCHED_SITES_NAMED)
            .filter_map(|site| {
                Some(format!(
                    "{}:{} ({})",
                    site.get("file")?.as_str()?,
                    site.get("line")?.as_u64()?,
                    site.get("kind").and_then(Value::as_str).unwrap_or("read"),
                ))
            })
            .collect();
        let rest = listed.saturating_sub(named.len() as u64);
        let mut at = named.join(", ");
        if rest > 0 {
            at.push_str(&format!(" and {rest} more"));
        }
        parts.push(format!(
            "{listed} non-call reads that could read the focal have no stored row, at {at}"
        ));
    }
    if carried_elsewhere > 0 {
        let kinds: Vec<&str> = unrequested.iter().map(|(kind, _)| kind.as_str()).collect();
        parts.push(format!(
            "{carried_elsewhere} are carried only by {} rows this query did not request",
            kinds.join(" and ")
        ));
    }
    Some(
        format!(
            "{REFERENCE_MENTIONS_UNMATCHED}: {}, each listed in focal_escape.residue.mentions",
            parts.join(", and ")
        )
        .replace(CLAUSE_SEPARATOR, ", "),
    )
}

/// The clauses an unsettled `call_sites` block names, never none.
fn call_sites_clauses(block: &Value) -> Vec<String> {
    let clauses: Vec<String> = block
        .get("clauses")
        .and_then(Value::as_array)
        .map(|clauses| {
            clauses
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if clauses.is_empty() {
        vec![format!(
            "{UNLISTED_CLAUSE_CODE}: the call_sites block reports its scope unsettled and names \
             no clause"
        )]
    } else {
        clauses
    }
}

/// The raw `edge_coverage` observation's own reading, independent of whichever
/// route consumed it.
///
/// It repeats what the absence gate reads on the tools that declare edge
/// classes, and that repetition is deliberate: recording both as named inputs is
/// what makes the verdict auditable, and the two come apart exactly where the
/// contradiction lived. A tool that declares no edge class still publishes this
/// observation, and `reference_enrichment: "unsupported"` is a fact about what
/// the build can resolve that no coverage number can override.
fn edge_coverage_reading(tool: &str, payload: &Value) -> Reading {
    let Some(coverage) = payload
        .get(crate::edge_coverage::EDGE_COVERAGE_KEY)
        .and_then(Value::as_object)
    else {
        return Reading::Silent;
    };
    let language = coverage
        .get("language")
        .and_then(Value::as_str)
        .filter(|language| !language.trim().is_empty())
        .unwrap_or("an unreported language");

    let requested = crate::negative::absence_cross_unit_classes(tool, payload);
    // An entity-index answer carrying a declaration census is decided by it:
    // the census measured what the extractor admitted in every file the answer
    // covers, which is what the language-server gates below stand in for.
    let declarations = requested
        .is_empty()
        .then(|| crate::declaration_coverage::clauses(coverage))
        .flatten();
    let census_measured = declarations.is_some()
        && crate::declaration_coverage::class_state(coverage)
            != Some(crate::declaration_coverage::STATE_UNMEASURED);

    // A reference answer whose own censuses measured every kind it asked for
    // has nothing a pending readiness observation could stand in for (see
    // `crate::negative::reference_census_measured`).
    if !census_measured && !crate::negative::reference_census_measured(tool, payload) {
        if let Some(gap) = crate::negative::reference_readiness_gap(coverage) {
            return Reading::Inconclusive(vec![gap]);
        }
    }

    // The same reading [`crate::negative::absence_coverage_gap`] takes, on
    // purpose. `available` is the only enrichment state that licenses a
    // certification; every other state means nothing established that this host
    // can produce reference edges for the language, and two inputs that read one
    // observation differently are the disagreement this module exists to end.
    match coverage
        .get("reference_enrichment")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
    {
        _ if census_measured => {}
        "available" => {}
        "unsupported" => {
            return Reading::Inconclusive(vec![format!(
                "reference_enrichment_unsupported: this build wires no language-server adapter \
                 for {language}, so reference and override edges between its modules cannot exist \
                 for it at all"
            )])
        }
        "no_language_server" => {
            return Reading::Inconclusive(vec![format!(
                "reference_enrichment_no_language_server: an adapter is wired for {language} but \
                 no language server for it is installed on this host"
            )])
        }
        _ => {}
    }
    if coverage.get("scope_entities").and_then(Value::as_u64) == Some(0) {
        return Reading::Inconclusive(vec![format!(
            "absence_scope_empty: the graph holds no entity at all under the filter this query \
             applied for {language}"
        )]);
    }
    if coverage.get("budget_exhausted").and_then(Value::as_bool) == Some(true) {
        return Reading::Inconclusive(vec![format!(
            "edge_coverage_budget_exhausted: the coverage scan for {language} stopped before it \
             could establish what the graph holds"
        )]);
    }

    let states = coverage.get("classes").and_then(Value::as_object);
    if let Some(clauses) = declarations {
        return if clauses.is_empty() {
            Reading::Certified
        } else {
            Reading::Inconclusive(clauses)
        };
    }
    if requested.is_empty() {
        // FIR-2496. This input used to answer `certified` for any observation
        // that named no class to check, which is every observation a tool
        // traversing no edge publishes. That is agreement inferred from silence:
        // the shipped verdict block read `"inputs": {"edge_coverage":
        // "certified"}` over `"classes": {}` on the two searches that were wrong
        // and on the one that was right, with nothing separating them. An
        // observation that measured nothing licenses nothing, and the absence
        // gate leads with the sharper wording when both refuse.
        //
        // On a real language only, matching the gate: an answer that resolved
        // none has no language's coverage to be missing, and it carries its own
        // sharper reason. Read from the gate's own function so the input's
        // reading and the refusal can never disagree about one observation.
        if crate::negative::coverage_classes_unmeasured(coverage, &requested) {
            let code = if crate::negative::answer_claims_absence(tool, payload) {
                "absence_coverage_unmeasured"
            } else {
                "answer_coverage_unmeasured"
            };
            return Reading::Inconclusive(vec![format!(
                "{code}: this answer measured no coverage class for \
                 {language}, so nothing established what the extractor admitted for it"
            )]);
        }
        return Reading::Certified;
    }
    // Every requested class, and the most specific reason first (FIR-2672). A
    // class this answer could not have read is a class its counts cannot be
    // whole over, whatever kept it from being read, so the reading refuses on
    // any state but `present` and says which state it was.
    let deciding = crate::negative::load_bearing_classes(&requested);
    let state_of = |class: &String| {
        states
            .and_then(|states| states.get(class.as_str()))
            .and_then(Value::as_str)
            .unwrap_or("unknown")
    };
    let in_state = |wanted: &str| -> Vec<&str> {
        deciding
            .iter()
            .filter(|class| state_of(class) == wanted)
            .map(String::as_str)
            .collect()
    };
    let unproduced = in_state("unproduced");
    if !unproduced.is_empty() {
        // `unproduced` carries two reasons and only one of them is a statement
        // about the source, so they take different sentences here exactly as
        // they do in the absence gate. A class this build mints for no language
        // at all has no resolved site to blame, and the older sentence would
        // send a reader hunting a linker bug that is not there.
        let build_gap = |class: &str| -> bool {
            coverage
                .get("unproduced_evidence")
                .and_then(Value::as_object)
                .and_then(|evidence| evidence.get(class))
                .and_then(|evidence| evidence.get("this_build_mints_no_entity_level_edge_for"))
                .is_some()
        };
        let (unminted, unproduced): (Vec<&str>, Vec<&str>) =
            unproduced.into_iter().partition(|class| build_gap(class));
        let mut clauses = Vec::new();
        if !unminted.is_empty() {
            let missing = unminted.join(", ");
            clauses.push(format!(
                "cross_file_edges_unproduced: this build mints no entity-level {missing} edge \
                 for {language} at all, so no graph it builds holds one and a use that reaches \
                 the target through {missing} could not have been found whatever the source \
                 contains, and the gap is in this build, not in the code"
            ));
        }
        if !unproduced.is_empty() {
            let missing = unproduced.join(", ");
            clauses.push(format!(
                "cross_file_edges_unproduced: this build produced no entity-level {missing} edge \
                 for {language} although the source carries {missing} sites the linker resolved, \
                 so a use that reaches the target through {missing} could not have been found, \
                 and the gap is in the linker, not in the code"
            ));
        }
        return Reading::Inconclusive(clauses);
    }
    let absent = in_state("absent");
    if !absent.is_empty() {
        let missing = absent.join(", ");
        return Reading::Inconclusive(vec![format!(
            "cross_file_edges_absent: the graph was not observed to hold {missing} edges \
             between modules for {language}, so a use that reaches the target through {missing} \
             could not have been found"
        )]);
    }
    let unhealthy: Vec<&str> = deciding
        .iter()
        .filter(|class| state_of(class) != "present")
        .map(String::as_str)
        .collect();
    if unhealthy.is_empty() {
        Reading::Certified
    } else {
        let missing = unhealthy.join(", ");
        Reading::Inconclusive(vec![format!(
            "edge_coverage_unknown: whether the graph holds {missing} edges between modules \
             for {language} could not be established, so a use that reaches the target through \
             {missing} may not have been found"
        )])
    }
}

/// Same-name candidates this answer held out of its headline.
///
/// A withheld row is evidence the answer already has and did not count, which is
/// the one shape where the count and the payload beside it disagree about the
/// same repository. `find_references(HTTPAdapter.send)` on psf/requests answered
/// `total_upstream: 0` while carrying `Session.send` at `sessions.py:784` in
/// `candidates`, and a reader of the headline deletes code a reader of the array
/// keeps.
fn withheld_candidates_reading(payload: &Value) -> Reading {
    match withheld_candidate_count(payload) {
        // A payload carrying no candidate accounting at all has nothing to say
        // here, which is not the same as saying nothing was withheld. Answering
        // `certified` for a concept the payload does not carry is how a mutation
        // response, whose every other input is silent, came to ship a certified
        // verdict about nothing.
        None => Reading::Silent,
        Some(0) => Reading::Certified,
        Some(withheld) => Reading::Inconclusive(vec![format!(
            "withheld_candidates: {withheld} same-name candidate(s) are carried in `candidates` \
             and are not in the counts here, so the headline is a floor and each withheld row \
             may belong in it"
        )]),
    }
}

/// Whether the configured cross-repo authority could answer for this response.
///
/// The absence gate already weighs this, and weighs it well, but only on an
/// answer that CLAIMS an absence. That scoping is deliberate and it is right for
/// what it was written for: a populated answer claims nothing is missing, so
/// refusing there on a store with no spine would put a floor under every answer
/// forever.
///
/// What it leaves uncovered is the case this input exists for. A populated
/// answer is presented as the reference set, and when a configured spine went
/// stale or answered incompletely, every cross-repo row is missing from a set
/// nothing marks as partial. That is `find_references` certifying an answer as
/// the whole set while recording that a class is absent, one level up: the class
/// here is every other repository. The gap belongs in the verdict on both
/// shapes of answer, and only the absence claim differs.
///
/// The rule itself is not restated here. [`crate::negative::cross_repo_qualifier`]
/// owns it, so this reading and the absence gate can never disagree about what
/// counts as a gap, and the clause text is identical, which is what makes the
/// composer's label dedupe collapse them to one on an answer where both speak.
///
/// Silent, never refusing, in the states that are facts about an install rather
/// than limits on an answer: no spine configured, and a repository the spine has
/// not registered. Both already travel as notes on the absence path, and both
/// are the ordinary state of a healthy single-repo store, so refusing on either
/// is the "mark everything uncertain" regression arriving by a side door.
///
/// Silent on a third, which the absence path does treat as a gap and this one
/// deliberately does not. An `unavailable` carrying no `code` is a caller that
/// never bound to a repository at all, not a spine that failed:
/// the absence gate's own unavailable-qualifier states the principle
/// itself, that every code "describes a spine that IS configured and did not
/// answer", and an uncoded reason reaches the gap branch only through the
/// catch-all label. `kin refs` is such a caller on every invocation, since it
/// resolves its binding from `KIN_REPO_ID` and that is an override rather than
/// something an install sets, so weighing it here would mark every CLI reference
/// answer on every machine a lower bound. The absence path keeps its stricter
/// reading, because an answer asserting that nothing references a symbol while
/// reporting no cross-repo authority is exactly what that gate exists to refuse,
/// and a populated answer asserts no such thing.
fn cross_repo_reading(tool: &str, payload: &Value) -> Reading {
    use crate::negative::CrossRepoQualifier;
    let unbound_caller = payload.get("cross_repo").is_some_and(|cross_repo| {
        cross_repo.get("status").and_then(Value::as_str) == Some("unavailable")
            && cross_repo.get("code").and_then(Value::as_str).is_none()
    });
    if unbound_caller {
        return Reading::Silent;
    }
    match crate::negative::cross_repo_qualifier(tool, payload) {
        None => Reading::Silent,
        Some(CrossRepoQualifier::Complete) => Reading::Certified,
        Some(CrossRepoQualifier::Note(_)) => Reading::Silent,
        Some(CrossRepoQualifier::Gap(reason)) => Reading::Inconclusive(vec![reason]),
    }
}

/// Whether the question names a dependency this graph holds no definitions for,
/// or could not be shown not to.
///
/// Silent on every response that does not carry the block, which is nearly all
/// of them: the block is published only when there is something to report, so
/// its presence is already the finding. There is no certified state here on
/// purpose. A response with no block did not establish that the question stays
/// inside the graph, it establishes that nobody looked, and an input that
/// certifies on no evidence is how a verdict comes to say yes about something it
/// never measured.
///
/// Three shapes refuse, not one. A matched symbol is the finding this exists
/// for. A scan that hit its budget, and a store that could not answer the query
/// at all, are the same finding at the boundary: an empty answer from an
/// observation that never completed is not evidence of absence, and dropping it
/// would rebuild the false certification one level down.
///
/// The clause is [`crate::outside_graph::limiting_clause`]'s, so the block and
/// the sentence a reader acts on cannot drift apart.
fn outside_graph_reading(payload: &Value) -> Reading {
    let Some(block) = payload.get(crate::outside_graph::OUTSIDE_GRAPH_KEY) else {
        return Reading::Silent;
    };
    match crate::outside_graph::limiting_clause(block) {
        Some(clause) => Reading::Inconclusive(vec![clause]),
        None => Reading::Silent,
    }
}

/// Whether a source read served the entity's recorded source.
///
/// A body read is an answer like any other, and without this every other
/// input stays silent on it, so it went out with no verdict at all while the
/// tool beside it carried one. It certifies when the span was checked against
/// the bytes it was cut from, by the entity's recorded digest or by reading
/// span and bytes from one committed state, and when a container was served as
/// its shape from those same bytes. A span nothing could check is served and
/// says so. A reply that served no source (a list of name candidates) leaves
/// the verdict to the inputs that read it.
fn source_body_reading(tool: &str, payload: &Value) -> Reading {
    if !matches!(tool, "get_entity_source" | "get_entity_body") {
        return Reading::Silent;
    }
    // An entity Kin holds without parsing it is served as its text, or, held
    // as bytes, as its description, read from its exact content.
    let served = payload.get("body").is_some_and(Value::is_string)
        || payload.get("members").is_some_and(Value::is_array)
        || payload.get("generator_source").is_some()
        || payload
            .as_object()
            .is_some_and(crate::artifact_entity::is_artifact_reply);
    if !served {
        return Reading::Silent;
    }
    if payload.get("generator_source_unavailable").is_some() {
        return Reading::Inconclusive(vec![
            "generator_source_unavailable: this derived member's generator source could not be read, so what it was generated from is not shown"
                .to_string(),
        ]);
    }
    match payload.get("span_coherence").and_then(Value::as_str) {
        Some("unverified") => Reading::Inconclusive(vec![
            "body_unverified: this entity records no source digest, so the served span could not be checked against the bytes it was cut from"
                .to_string(),
        ]),
        _ => Reading::Certified,
    }
}

/// The code a `refs` answer about an unparsed entity carries: its rows are
/// the places whose text names the entity, matched by name.
pub(crate) const MATCHED_BY_NAME: &str = "matched_by_name";

/// A `refs` answer about an unparsed entity lists the places whose text names
/// it. A match by name is not a resolved use: Kin has not proved what any of
/// those places uses it for, and a name built while code runs, or a pattern,
/// is not followed. So the answer is never complete, whether it lists places
/// or none.
fn named_in_reading(tool: &str, payload: &Value) -> Reading {
    if tool != "find_references" {
        return Reading::Silent;
    }
    match payload.as_object() {
        Some(reply) if crate::artifact_entity::mentions::is_named_in_reply(reply) => {
            Reading::Inconclusive(vec![
                "matched_by_name: these rows name the entity in their text, and Kin has not resolved what they use it for; a name built while code runs, or a pattern, is not followed"
                    .to_string(),
            ])
        }
        _ => Reading::Silent,
    }
}

/// A module inventory whose graph recorded no member coverage cannot establish
/// an empty module. Read the same gap in single, batch and context projections.
fn module_members_reading(payload: &Value) -> Reading {
    fn has_gap(value: &Value) -> bool {
        match value {
            Value::Object(object) => {
                object.get("semantic_gap").and_then(Value::as_str)
                    == Some(crate::container_shape::MODULE_MEMBER_GAP)
                    || object.values().any(has_gap)
            }
            Value::Array(values) => values.iter().any(has_gap),
            _ => false,
        }
    }
    if has_gap(payload) {
        Reading::Inconclusive(vec![format!(
            "{}: the graph could not establish member coverage for a module in this answer; \
             this inventory does not establish that its source has no declarations",
            crate::container_shape::MODULE_MEMBER_GAP
        )])
    } else {
        Reading::Silent
    }
}

/// The payload's candidate-scorer block, when a retrieval arm ran a scorer.
fn scorer_block(payload: &Value) -> Option<Value> {
    payload
        .get("scorer")
        .filter(|block| block.is_object())
        .cloned()
}

/// What the candidate scorer said about this answer's rows.
///
/// An abstention refuses: the scorer read the leading rows and rated none of
/// them the answer, so an agent must not act on rank one as if it were. A
/// scorer that rated a row over its threshold certifies nothing more than that.
/// A scorer that could not run is silent here, because the ledger already
/// carries it under `degradations`, and an answer with no scorer is silent too.
fn scorer_reading(payload: &Value) -> Reading {
    let Some(scorer) = payload.get("scorer").and_then(Value::as_object) else {
        return Reading::Silent;
    };
    if scorer
        .get("unavailable")
        .is_some_and(|value| !value.is_null())
    {
        return Reading::Silent;
    }
    if scorer.get("abstained").and_then(Value::as_bool) != Some(true) {
        return Reading::Certified;
    }
    let name = scorer
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("candidate");
    let threshold = scorer
        .get("threshold")
        .and_then(Value::as_f64)
        .unwrap_or_default();
    let best = match scorer.get("top_probability").and_then(Value::as_f64) {
        Some(best) => format!("the best row scored {best:.2}"),
        None => "no row was scored".to_string(),
    };
    Reading::Inconclusive(vec![format!(
        "none_of_these: the {name} scorer rated no candidate at or above its calibrated threshold \
         {threshold:.2} ({best}) and the query named none of them, so these rows are leads rather \
         than an answer"
    )])
}

/// This query's own reported degradations.
///
/// Refuses on every entry that reports the run lost a capability or cut its
/// answer. An entry marked `severity: "note"` records a policy the run applied
/// by design, such as a filter the caller chose, and is read past rather than
/// refused on: it stays in `degradations[]` for the caller, and a ledger holding
/// only notes certifies. The split is the producer's, read off each entry by
/// [`crate::negative::payload_degradation_labels`], so no label list lives here.
fn degradations_reading(payload: &Value) -> Reading {
    if let reading @ Reading::Inconclusive(_) = module_members_reading(payload) {
        return reading;
    }
    if payload.get("degradations").is_none() {
        return Reading::Silent;
    }
    let labels = crate::negative::payload_degradation_labels(payload);
    if labels.is_empty() {
        Reading::Certified
    } else {
        Reading::Inconclusive(vec![format!(
            "retrieval_degraded: this query reported degradations [{}], so it did not run at \
             full capability",
            labels.join(", ")
        )])
    }
}

/// The completeness signal's own reading of the substrate and the numbers.
/// Graph freshness as a verdict input. The selected graph's own stale-sample
/// disclosure refuses; the admission clock still weighs nothing.
///
/// Two readings share one field and only one of them is decidable here, which is
/// why this is a match rather than a single verdict over
/// [`crate::envelope::GraphFreshness`].
///
/// **Why the replayed sample refuses.** `Stale` is a positive observation the
/// status producer made: the selected graph could not be sampled live after a
/// counted number of attempts, the failure mode is named, and the age of the
/// observation whose counters this response replays is measured in
/// milliseconds. Nothing is inferred from an absence, so the objection this
/// module raises against the clock below does not reach it. It has to refuse,
/// because completeness is a reading of the SUBSTRATE and says nothing about
/// whether the sample was live: an answer replaying counters from an earlier
/// observation could reach `_kin.verdict` as certified with every coverage
/// class complete, which is the false all-clear this whole module exists to
/// stop.
///
/// **Why not refuse when no admission is recorded.** The wire's clock is the
/// daemon's in-memory record, set only by a completed exact-tree admission pass,
/// so a freshly initialized store whose daemon has not yet run one carries
/// nothing. Absence is therefore the ordinary state of a healthy new store, not
/// evidence of staleness, and refusing on it puts a floor under every answer on
/// every such store. That is the regression `crate::negative`'s own gate comment
/// warns about, and the acceptance suite's anti-vacuity control caught this
/// module doing it.
///
/// **Why not certify when one IS recorded.** A present clock proves an admission
/// completed at some time. It does not prove the store is current, and the
/// verdict's note asserts agreement of every input, so certifying here would
/// state exactly the false all-clear for the case this cannot see: a months-stale
/// store under a daemon that has been up the whole time and admitted once.
///
/// Both admission-clock halves need the same missing fact, a reading the daemon
/// does not publish: the durable last-admission marker, which survives a restart
/// and carries `tracked_artifacts` beside its timestamp. With it, absence and
/// staleness separate and the clock becomes a real input too. Until then,
/// silence is the only honest reading of it, and a silent input never
/// contributes agreement, so nothing there can license an answer either.
/// A watcher-reported loss of events as a verdict input.
///
/// The one reading here that rests on a positive observation of missing input
/// rather than on an inference from an absence, which is why it refuses where
/// the admission clock beside it stays silent. The watcher's backend said it
/// dropped events and named no path, so the graph is missing an unknown set of
/// them and no absence over this store can be authoritative until a complete
/// exact-tree admission runs.
///
/// The consequence is deliberate and worth stating plainly: every answer over
/// an affected store reads `inconclusive` until someone runs `kin admit`. That
/// is the founder's contract for this class, fail loud plus an explicit
/// admission, and the alternative is the silent healthy read this whole module
/// exists to stop. It is bounded in exactly the way a permanent floor would not
/// be: the record clears, so the verdict returns.
///
/// Silent when nothing stands. The producer omits the block entirely on a store
/// that owes no recovery, and a silent input never contributes agreement, so
/// this can never license an answer either.
fn watcher_loss_reading(envelope: &Envelope) -> Reading {
    match envelope.watcher_loss.as_ref() {
        // Taken from the observation rather than written again here, so the
        // sentence the daemon publishes and the one the verdict refuses with
        // cannot drift apart.
        Some(loss) => Reading::Inconclusive(vec![loss.limiting_factor()]),
        None => Reading::Silent,
    }
}

/// Tracked files the working copy edited or removed while no daemon watched,
/// that no admission has taken yet.
///
/// Every answer is inconclusive while one stands, a populated one included,
/// because the graph still answers from those files' old bytes: it names a
/// renamed function under its old name and ranks a deleted file as a hit. The
/// daemon plans these paths before it publishes its endpoint and admits them on
/// its own, so the reading clears without anyone acting, and it is bounded the
/// way a permanent floor would not be.
///
/// Silent when nothing is owed; the producer omits the count on a daemon that
/// took everything, and a silent input never licenses an answer.
fn tracked_changes_reading(envelope: &Envelope) -> Reading {
    match envelope
        .behind
        .as_ref()
        .and_then(crate::envelope::GraphBehind::tracked_changes_limiting_factor)
    {
        Some(reason) => Reading::Inconclusive(vec![reason]),
        None => Reading::Silent,
    }
}

fn graph_freshness_reading(envelope: &Envelope) -> Reading {
    match envelope.freshness.as_ref() {
        // The clause is taken from the type rather than written again here, so
        // the sentence `_kin.freshness` publishes and the one the verdict
        // refuses with cannot drift apart.
        Some(stale @ crate::envelope::GraphFreshness::Stale { .. }) => {
            match stale.limiting_factor() {
                Some(clause) => Reading::Inconclusive(vec![clause]),
                None => Reading::Silent,
            }
        }
        Some(_) | None => Reading::Silent,
    }
}

/// A graph that holds no entities is a graph gap, never a zero.
///
/// Keyed on the entity count and nothing else. A daemon that began serving a
/// moment ago, a repository no admission has reached, and a store that holds
/// nothing all read this way, and in none of them can the graph speak for the
/// repository. It deliberately does not consult the admission clock, whose
/// absence is the ordinary state of a healthy fresh store and must never refuse
/// on its own. The clause carries the label the absence gate uses for the same
/// fact, so the two never disagree about an empty graph.
fn graph_empty_reading(envelope: &Envelope) -> Reading {
    match envelope.graph_state.entity_count {
        Some(0) => Reading::Inconclusive(vec![
            "graph_empty: the graph that answered holds no entities at all, so it cannot speak \
             for this repository; a daemon that has just begun serving, or a repository no \
             admission has reached yet, reads this way until its graph is loaded, so ask again \
             rather than act on it"
                .to_string(),
        ]),
        _ => Reading::Silent,
    }
}

/// The completeness signal's own reading.
///
/// ## Why a nearly whole substrate still refuses
///
/// A store verified at 18124/18124 indexed, read a moment later at 18123 with 2
/// pending, refuses here. 2 in 18,124 is 0.011 percent, and the obvious reply is
/// that a shortfall that small should be disclosed rather than decisive, with
/// the verdict certifying and `limits` still carrying `embeddings_partial`. A
/// ratio threshold cannot deliver that, and the reason is structural rather than
/// a matter of taste.
///
/// `state` follows `limiting_factor`, and this reading refuses on `bound` as
/// well as on `status`. `bound` is `at_least` whenever the answer cannot be
/// shown to be whole, which is exactly what 2 unranked entities mean, and
/// [`disagreements`] records `bound: at_least` under a certified verdict as a
/// contradiction in its own right. So a threshold that only skipped the `status`
/// branch would change nothing, and one that reached `state` would have to carry
/// `status` to `complete` and `bound` to `exact`, asserting that the printed
/// counts are the whole set over a class the same object publishes as `partial`.
/// That is two fields in one response disagreeing about one fact, which is the
/// defect the embedding class was just fixed to remove, reintroduced one field
/// over.
///
/// So the threshold is refused and the wording is fixed instead. The reader now
/// gets `classes.embeddings: "partial"`, `limits: ["embeddings_partial"]`, the
/// exact counters beside them, and a `substrate_partial` whose published meaning
/// says "short of whole" rather than "observed absent". Those agree, and they
/// let a reader size the shortfall and act proportionately, which is what a
/// verdict word alone could never do. The one claim still withheld is that the
/// counts are whole, and over 2 unembedded entities that claim would be false.
///
/// The absence gate keeps its own separate refusal
/// ([`Envelope::negative_trust`], `coverage_partial`) at every ratio, because
/// the unembedded entities are precisely the set that could hold the thing a
/// reader is about to conclude does not exist.
fn completeness_reading(envelope: &Envelope) -> Reading {
    let Some(completeness) = &envelope.completeness else {
        return Reading::Silent;
    };
    if completeness.status != "complete" {
        return Reading::Inconclusive(vec![format!(
            "substrate_{}: the coverage classes this answer depended on were not all observed \
             present ({})",
            completeness.status,
            completeness.decided_by.join(", ")
        )]);
    }
    if completeness.bound != "exact" {
        return Reading::Inconclusive(vec![
            "counts_are_a_floor: this answer's own accounting reports its numbers as a lower \
             bound"
                .to_string(),
        ]);
    }
    Reading::Certified
}

/// How many same-name candidates the payload held out of its counts, read from
/// the one number the withheld plumbing already publishes.
fn withheld_candidate_count(payload: &Value) -> Option<u64> {
    payload
        .get("counts")
        .and_then(|counts| counts.get("receiver_name_candidates"))
        .and_then(Value::as_u64)
        .or_else(|| {
            payload
                .get("candidates")
                .and_then(Value::as_array)
                .map(|rows| rows.len() as u64)
        })
}

/// Whether the payload already names its withheld candidates as a degradation,
/// which the absence gate consumes on its own.
fn discloses_withheld_candidates(payload: &Value) -> bool {
    crate::negative::payload_degradation_labels(payload)
        .iter()
        .any(|label| label.ends_with(":receiver_name_candidates"))
}

/// Downgrade every verdict surface on an already-built response that the size
/// budget then shortened.
///
/// The budget runs after the verdict is serialized, so the cut cannot be known
/// when the verdict is computed. Downgrading only `_kin.completeness` here, as
/// this path used to, would leave `_kin.verdict` and `negative` certifying an
/// answer whose rows were removed on purpose.
pub fn mark_response_bounded(annotated: &mut Value) {
    if let Some(verdict) = annotated
        .get_mut(crate::envelope::ENVELOPE_KEY)
        .and_then(Value::as_object_mut)
        .and_then(|envelope| envelope.get_mut(VERDICT_KEY))
        .and_then(Value::as_object_mut)
    {
        verdict.insert("state".to_string(), json!(INCONCLUSIVE));
        verdict.insert("safe_to_conclude_absent".to_string(), json!(false));
        // The budget cut leads and the codes already there follow, the same way
        // the absence object below keeps its own reasons; a factor that was
        // replaced outright lost every other reason the answer had.
        let factor = lead_with_code(
            verdict.get("limiting_factor").and_then(Value::as_str),
            RESPONSE_BOUNDED_CODE,
        );
        verdict.insert("limiting_factor".to_string(), json!(factor));
        if let Some(inputs) = verdict.get_mut("inputs").and_then(Value::as_object_mut) {
            inputs.insert("response_budget".to_string(), json!(INCONCLUSIVE));
        }
    }
    if let Some(negative) = annotated
        .get_mut(crate::negative::NEGATIVE_KEY)
        .and_then(Value::as_object_mut)
    {
        negative.insert("safe_to_conclude_absent".to_string(), json!(false));
        negative.insert("trust".to_string(), json!(INCONCLUSIVE));
        let reason = match negative.get("trust_reason").and_then(Value::as_str) {
            Some(existing) if existing.contains(RESPONSE_BOUNDED_FACTOR) => existing.to_string(),
            Some(existing) if !existing.is_empty() => {
                format!("{RESPONSE_BOUNDED_FACTOR}; {existing}")
            }
            _ => RESPONSE_BOUNDED_FACTOR.to_string(),
        };
        negative.insert("trust_reason".to_string(), json!(reason.clone()));
        if let Some(advice) = negative.get("advice").and_then(Value::as_str) {
            let head = advice.split(". Limiting factor:").next().unwrap_or(advice);
            negative.insert(
                "advice".to_string(),
                json!(format!("{head}. Limiting factor: {reason}")),
            );
        }
    }
}

/// A source observation is sampled after the query. Add its refusal to an
/// already serialized verdict without discarding earlier limiting inputs.
pub(crate) fn qualify_source_observation(payload: &mut Value, reason: &'static str) {
    if let Some(negative) = payload.get_mut(crate::negative::NEGATIVE_KEY) {
        crate::negative::qualify_source_observation(negative, reason);
    }
    let Some(envelope) = payload
        .get_mut(crate::envelope::ENVELOPE_KEY)
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    let Some(previous) = envelope.get(VERDICT_KEY).and_then(Value::as_object) else {
        return;
    };
    let mut inputs = previous
        .get("inputs")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    inputs.insert("source_derivation".into(), json!(INCONCLUSIVE));
    let limiting_factor = lead_with_code(
        previous.get("limiting_factor").and_then(Value::as_str),
        reason
            .split(':')
            .next()
            .unwrap_or("derived_source_unproven"),
    );
    // A folded gap downgrades a certified answer only to complete except it,
    // and leaves an answer some other input refused exactly as refused.
    let previous_state = previous.get("state").and_then(Value::as_str);
    let except_residue = matches!(previous_state, Some(CERTIFIED) | Some(CERTIFIED_EXCEPT))
        && factor_is_residue_only(&limiting_factor);
    let verdict = Verdict {
        scorer: previous.get("scorer").cloned(),
        certified: false,
        except_residue,
        absence_claim: if previous.get("absence_claim").and_then(Value::as_str)
            == Some("not_applicable")
        {
            AbsenceClaim::NotApplicable
        } else {
            AbsenceClaim::NotAuthoritative
        },
        limiting_factor: Some(limiting_factor),
        inputs,
    };
    if let Some(value) = envelope.get_mut("completeness") {
        if let Ok(typed) = serde_json::from_value(value.clone()) {
            let mut completeness = Some(typed);
            verdict.project_onto_completeness(&mut completeness);
            *value = serde_json::to_value(completeness).expect("completeness serializes");
        }
    }
    envelope.insert(VERDICT_KEY.into(), verdict.to_value());
    if let Some(edge) = payload
        .get_mut(crate::edge_coverage::EDGE_COVERAGE_KEY)
        .and_then(Value::as_object_mut)
    {
        let limits = edge.entry("limits").or_insert_with(|| json!([]));
        if let Some(limits) = limits.as_array_mut() {
            let value = json!("source_derivation:inconclusive");
            if !limits.contains(&value) {
                limits.push(value);
            }
        }
    }
}

/// Every way a built response contradicts its own single verdict, as one message
/// per disagreement.
///
/// This is the guard that keeps the collapse from being undone one block at a
/// time. It reads the fully built response, so it sees what a client sees rather
/// than what any producer intended, and an empty result is the only shape that
/// may ship.
///
/// Only fields a reader ACTS on are checked. `_kin.completeness.status` and
/// `classes` are raw observation and may legitimately read `complete` and
/// `present` under an inconclusive verdict, because the substrate being whole is
/// not the same claim as the answer being whole.
pub fn disagreements(response: &Value) -> Vec<String> {
    let Some(verdict) = response
        .get(crate::envelope::ENVELOPE_KEY)
        .and_then(|envelope| envelope.get(VERDICT_KEY))
    else {
        return Vec::new();
    };
    let certified = verdict.get("state").and_then(Value::as_str) == Some(CERTIFIED);
    let verdict_absent = verdict
        .get("safe_to_conclude_absent")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut found = Vec::new();

    if let Some(negative) = response.get(crate::negative::NEGATIVE_KEY) {
        let negative_absent = negative
            .get("safe_to_conclude_absent")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if negative_absent != verdict_absent {
            found.push(format!(
                "negative.safe_to_conclude_absent is {negative_absent} while \
                 _kin.verdict.safe_to_conclude_absent is {verdict_absent}"
            ));
        }
        if negative.get("trust").and_then(Value::as_str) == Some("authoritative") && !certified {
            found.push(
                "negative.trust reads authoritative under an inconclusive _kin.verdict".to_string(),
            );
        }
        if let Some(advice) = negative.get("advice").and_then(Value::as_str) {
            if advice.contains("Absence is authoritative") && !certified {
                found.push(
                    "negative.advice certifies an absence under an inconclusive _kin.verdict"
                        .to_string(),
                );
            }
        }
    }

    if let Some(completeness) = response
        .get(crate::envelope::ENVELOPE_KEY)
        .and_then(|envelope| envelope.get("completeness"))
    {
        if completeness.get("bound").and_then(Value::as_str) == Some("exact") && !certified {
            found.push(
                "_kin.completeness.bound reads exact under an inconclusive _kin.verdict"
                    .to_string(),
            );
        }
        if completeness
            .get("counted")
            .and_then(|counted| counted.get("exact"))
            .and_then(Value::as_bool)
            == Some(true)
            && !certified
        {
            found.push(
                "_kin.completeness.counted.exact is true under an inconclusive _kin.verdict"
                    .to_string(),
            );
        }
    }

    // A partial page of a paged answer carries the ANSWER's verdict and its own
    // page-scoped completeness: the rows it holds are a floor of the answer by
    // construction, and the rest arrive on later pages. Its `at_least` bound and
    // its share of the rows describe the page, not a second verdict on the
    // answer, so they are checked on the reassembled answer instead.
    let partial_page = response.pointer("/_kin/page/complete") == Some(&Value::Bool(false));

    // The five arms above all read one direction: a surface claiming
    // certification under an inconclusive verdict. Nothing read the other way,
    // which is why a shipped envelope could carry a CERTIFIED verdict over a
    // completeness that refused and still grade clean. A contradiction does not
    // care which side is the optimistic one.
    if certified {
        if let Some(completeness) = response
            .get(crate::envelope::ENVELOPE_KEY)
            .and_then(|envelope| envelope.get("completeness"))
        {
            let bound = completeness.get("bound").and_then(Value::as_str);
            if matches!(bound, Some("at_least")) && !partial_page {
                found.push(
                    "_kin.completeness.bound reads at_least under a certified _kin.verdict"
                        .to_string(),
                );
            }
            if completeness.get("status").and_then(Value::as_str) == Some("unknown") {
                found.push(
                    "_kin.completeness.status reads unknown under a certified _kin.verdict"
                        .to_string(),
                );
            }
        }
        if let Some(negative) = response.get(crate::negative::NEGATIVE_KEY) {
            if matches!(
                negative.get("trust").and_then(Value::as_str),
                Some("inconclusive") | Some("unreliable")
            ) {
                found.push("negative.trust refuses under a certified _kin.verdict".to_string());
            }
        }
    }

    if !partial_page {
        found.extend(headline_count_disagreements(response));
    }
    found
}

/// Where a headline count a reader acts on contradicts evidence the same
/// response is holding.
///
/// The count and the withheld rows are one accounting with one source, so the
/// three places it surfaces have to carry the same number. A `total_upstream` of
/// zero beside a populated `candidates` array is the exact shape that made a
/// reader delete working code, and it may not ship without
/// `unconfirmed_candidates` naming the held rows at the count.
fn headline_count_disagreements(response: &Value) -> Vec<String> {
    let Some(candidates) = response.get("candidates").and_then(Value::as_array) else {
        return Vec::new();
    };
    // Focal choices are not withheld reference rows. They have their own
    // accounting, including brief and explicitly omitted choices. Reference
    // accounting takes precedence even if a malformed response also claims an
    // ambiguous focal, so the marker cannot hide a real counter disagreement.
    let reference_accounting = [
        "/total_upstream",
        "/unconfirmed_candidates",
        "/counts/receiver_name_candidates",
        "/counts/unresolved_name_candidates",
        "/_kin/completeness/counted/withheld_candidates",
    ]
    .iter()
    .any(|pointer| response.pointer(pointer).is_some());
    if response.get("ambiguous_focal").and_then(Value::as_bool) == Some(true)
        && !reference_accounting
    {
        return focal_choice_count_disagreements(response, candidates.len() as u64);
    }
    let withheld = candidates.len() as u64;
    if withheld == 0 {
        return Vec::new();
    }
    let mut found = Vec::new();
    match response
        .get("unconfirmed_candidates")
        .and_then(Value::as_u64)
    {
        None => found.push(format!(
            "the response carries {withheld} candidate row(s) and no unconfirmed_candidates \
             beside its headline count"
        )),
        Some(reported) if reported != withheld => found.push(format!(
            "unconfirmed_candidates reads {reported} against {withheld} candidate row(s)"
        )),
        Some(_) => {}
    }
    // Two grounds hold a row back, and `counts` reports them apart: the
    // receiver fan-out, and an edge under the request's `min_resolution` floor.
    // The accounting is the SUM, because that is what `candidates` holds. Read
    // against the fan-out alone this said `reads 0 against 18` on every answer
    // that withheld a below-floor row, which is the check calling a correct
    // response broken.
    let counts = response.get("counts");
    let counted = |key: &str| {
        counts
            .and_then(|counts| counts.get(key))
            .and_then(Value::as_u64)
    };
    let receiver = counted("receiver_name_candidates");
    let unresolved = counted("unresolved_name_candidates");
    if let Some(total) = match (receiver, unresolved) {
        (None, None) => None,
        (a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
    } {
        if total != withheld {
            found.push(format!(
                "counts.receiver_name_candidates plus counts.unresolved_name_candidates reads \
                 {total} against {withheld} candidate row(s)"
            ));
        }
    }
    if let Some(reported) = response
        .get(crate::envelope::ENVELOPE_KEY)
        .and_then(|envelope| envelope.get("completeness"))
        .and_then(|completeness| completeness.get("counted"))
        .and_then(|counted| counted.get("withheld_candidates"))
        .and_then(Value::as_u64)
    {
        if reported != withheld {
            found.push(format!(
                "_kin.completeness.counted.withheld_candidates reads {reported} against \
                 {withheld} candidate row(s)"
            ));
        }
    }
    found
}

/// Name resolution keeps the total when detailed choices become brief rows
/// or an explicitly omitted suffix. Check that accounting without inventing a
/// reference-count headline for a response that answered about no focal.
fn focal_choice_count_disagreements(response: &Value, detailed: u64) -> Vec<String> {
    let brief = match response.get("more_candidates") {
        None => 0,
        Some(Value::Array(rows)) => rows.len() as u64,
        Some(_) => return vec!["more_candidates is not an array of focal choices".to_string()],
    };
    let omitted = match response.get("omitted_candidates") {
        None => 0,
        Some(value) => match value.as_u64() {
            Some(count) => count,
            None => return vec!["omitted_candidates is not a nonnegative count".to_string()],
        },
    };
    let Some(total) = detailed
        .checked_add(brief)
        .and_then(|listed| listed.checked_add(omitted))
    else {
        return vec!["focal choice accounting overflows its count".to_string()];
    };
    match response.get("candidate_count").and_then(Value::as_u64) {
        None => vec!["focal choices carry no valid candidate_count".to_string()],
        Some(reported) if reported != total => vec![format!(
            "candidate_count reads {reported} against {total} focal choice(s) \
             in candidates, more_candidates and omitted_candidates"
        )],
        Some(_) => Vec::new(),
    }
}

#[cfg(test)]
#[path = "../tests_support/clause_codes.rs"]
mod clause_codes_tests;

#[cfg(test)]
mod tests {
    use super::*;

    /// A `refs` answer that lists what names an unparsed entity in its text
    /// is matched by name, so it is never complete, with places or with none.
    /// A source reply that lists the same rows beside the entity's
    /// description answers another question and is not qualified by them.
    #[test]
    fn what_names_an_unparsed_entity_by_name_is_never_a_complete_answer() {
        let named = |rows: Value| {
            json!({
                "entity": {"entity_id": "artifact", "name": "schema.sql", "kind": "SQL"},
                "named_in": rows,
                "matched_by": "name",
                "named_in_total": rows.as_array().map_or(0, Vec::len),
            })
        };
        for payload in [
            named(
                json!([{"entity_id": "init_db", "name": "init_db", "line": "open(\"schema.sql\")"}]),
            ),
            named(json!([])),
        ] {
            let verdict = Verdict::compute("find_references", &payload, &Envelope::daemon(), None)
                .expect("a refs answer about an unparsed entity carries a verdict");
            assert!(!verdict.certified, "{payload}");
            let factor = verdict.limiting_factor.unwrap_or_default();
            assert!(
                factor
                    .split(CLAUSE_SEPARATOR)
                    .any(|code| code == MATCHED_BY_NAME),
                "{factor}"
            );
            assert_eq!(verdict.inputs["named_in"], json!(INCONCLUSIVE));
        }
        assert!(matches!(
            named_in_reading("get_entity_source", &named(json!([]))),
            Reading::Silent
        ));
        // Every other answer's inputs stay as they were.
        let other = json!({"references": [], "focal_entity": {"id": "f", "name": "f"}});
        if let Some(verdict) =
            Verdict::compute("find_references", &other, &Envelope::daemon(), None)
        {
            assert!(
                verdict.inputs.get("named_in").is_none(),
                "{:?}",
                verdict.inputs
            );
        }
    }

    /// A store-level limit carries the one step that lifts it, and an
    /// answer-level one carries none.
    #[test]
    fn a_store_written_by_an_older_build_names_kin_upgrade_on_the_verdict() {
        let behind = remedy_for(Some("store_semantics_behind; call_sites_owed")).unwrap();
        assert!(
            behind.starts_with("Run `kin upgrade` in this repository"),
            "{behind}"
        );
        let unknown = remedy_for(Some("call_sites_owed; store_semantics_unknown")).unwrap();
        assert!(unknown.contains("then run `kin upgrade`"), "{unknown}");
        assert_eq!(
            remedy_for(Some("call_sites_owed; local_binding_unproven")),
            None
        );
        assert_eq!(remedy_for(None), None);
    }

    #[test]
    fn enrichment_bounds_populated_and_empty_answers_without_certifying_other_relations() {
        use kin_model::EntityStore as _;
        let graph = kin_db::InMemoryGraph::new();
        let focal = crate::call_sites::fixture::spanned_entity(
            "pending",
            "pending.py",
            kin_model::LanguageId::Python,
            0,
            "def pending():\n    return unresolved()\n",
        );
        graph.upsert_entity(&focal).unwrap();
        // Exercise the producer's prose through the negative block's joined
        // clauses. A separator inside one explanation would mint an unknown
        // second clause when that block is read back into the verdict.
        let observation =
            kin_review::enrichment::observe_selected_impact(&graph, &[focal], false).unwrap();
        let limitation = observation.limitation.as_deref().unwrap();
        assert!(!limitation.contains(CLAUSE_SEPARATOR), "{limitation}");
        assert!(limitation.contains("does not prove"));
        let observation = serde_json::to_value(observation).unwrap();
        let envelope = Envelope::daemon().with_health(&json!({
            "initialized": true, "graph_loaded": true, "reconciliation_status": "clean",
        }));
        for payload in [
            json!({"entity_impacts": [], "enrichment": observation}),
            json!({"entity_impacts": [{"consumer_count": 1}], "enrichment": observation}),
        ] {
            let negative =
                crate::negative::negative_for("impact_analysis", &payload, &envelope, &[]).unwrap();
            assert_eq!(negative["trust"], "inconclusive");
            assert!(negative["trust_reason"]
                .as_str()
                .unwrap()
                .contains("enrichment_incomplete"));
            let verdict = Verdict::compute("impact_analysis", &payload, &envelope, Some(&negative))
                .unwrap()
                .to_value();
            assert_eq!(verdict["state"], "inconclusive");
            assert_eq!(verdict["inputs"]["enrichment"], "inconclusive");
            let factor = verdict["limiting_factor"].as_str().unwrap();
            assert!(factor
                .split(CLAUSE_SEPARATOR)
                .any(|code| code == "enrichment_incomplete"));
            assert!(!factor.contains(UNLISTED_CLAUSE_CODE), "{verdict}");
            assert_eq!(verdict["safe_to_conclude_absent"], false);
        }
        let observation = json!({"enrichment": {"status": "no_recorded_call_site_debt"}});
        assert!(matches!(enrichment_reading(&observation), Reading::Silent));
        assert!(
            Verdict::compute("semantic_review", &observation, &Envelope::daemon(), None).is_none()
        );
    }

    /// The freshness field carries two readings and they are graded apart.
    ///
    /// The ADMISSION CLOCK weighs nothing. An earlier version of the change
    /// that wired it refused when no admission was recorded, and the
    /// acceptance suite's anti-vacuity control caught it
    /// certifying nothing: absence of the in-memory clock is the ordinary state
    /// of a healthy fresh store, so refusing on it floors every answer on every
    /// such store. Those arms are written against what the clock can honestly
    /// claim rather than against what it was hoped to do.
    ///
    /// The REPLAYED SAMPLE refuses, because it is an observation rather than an
    /// absence: a named failure mode, a counted number of live attempts, and a
    /// measured age. Its arms are the ones that must stay separate from the
    /// clock's, since a graph selected for this answer can be stale while HEAD
    /// was admitted seconds ago.
    ///
    /// The clock arms are driven through `with_health` using the producer's own
    /// field names, and both are `skip_serializing_if = "Option::is_none"`
    /// there, which is why the no-clock arm omits the key rather than setting it
    /// null.
    /// Fail loud on the agent-facing surface, and stop being loud once the
    /// recovery has happened.
    ///
    /// Three arms because two of them would prove nothing. `inconclusive` on a
    /// lossy store means nothing without the healthy control that certifies on
    /// the same payload, and neither means anything without the settled arm,
    /// which is what stops this being a permanent floor over every store that
    /// ever dropped an event.
    ///
    /// The fixtures spell the wire shape by hand because this crate does not
    /// depend on the producer's. The producer is
    /// `kin_cli::commands::resources::WatcherLossState`, written by the daemon's
    /// `watcher_loss::standing`, and the daemon-side test that grades the pair
    /// is `repository_admit::tests::only_an_explicit_full_admission_clears_a_watcher_loss`.
    /// An empty graph is a gap on every answer, and above all on the status answer,
    /// which has no absence gate of its own to say so.
    mod graph_empty {
        use super::*;

        fn status_envelope(entity_count: u64) -> Envelope {
            Envelope::daemon().with_selected_graph_observation(
                crate::envelope::DurabilityCounts {
                    live_entities: entity_count,
                    durable_entities: None,
                    live_relations: Some(0),
                    durable_relations: None,
                },
                0,
                0,
                0,
            )
        }

        #[test]
        fn an_empty_graph_makes_the_status_answer_inconclusive_and_names_the_gap() {
            let verdict = Verdict::compute(
                "kin_graph_status",
                &json!({ "entity_count": 0 }),
                &status_envelope(0),
                None,
            )
            .expect("an empty graph must put a verdict on the status answer");
            assert!(!verdict.certified, "a zero is not a certified answer");
            assert_eq!(
                verdict.inputs.get("graph_empty").and_then(Value::as_str),
                Some(INCONCLUSIVE)
            );
            let factor = verdict.limiting_factor.as_deref().unwrap_or_default();
            assert_eq!(
                factor.split(CLAUSE_SEPARATOR).next(),
                Some("graph_empty"),
                "the factor must name the gap: {factor}"
            );
            assert!(
                CLAUSE_CODES
                    .iter()
                    .any(|entry| entry.code == "graph_empty" && entry.meaning.contains("ask again")),
                "and the code's one written meaning must say what to do about it"
            );
            assert!(
                !factor.contains("graph_admission"),
                "the admission clock must not be what refuses: {factor}"
            );
        }

        /// The control: a populated graph gives this input nothing to say, so the
        /// status answer stays exactly as it was.
        #[test]
        fn a_populated_graph_gives_the_input_nothing_to_say() {
            let verdict = Verdict::compute(
                "kin_graph_status",
                &json!({ "entity_count": 152 }),
                &status_envelope(152),
                None,
            );
            assert!(
                verdict.is_none(),
                "no input spoke, so no verdict: {:?}",
                verdict.map(|verdict| verdict.limiting_factor)
            );
        }

        /// The absence gate names an empty graph with the same label, so an empty
        /// retrieval answer carries the clause once, not twice.
        #[test]
        fn the_clause_shares_the_absence_gate_s_label() {
            let Reading::Inconclusive(clauses) = graph_empty_reading(&status_envelope(0)) else {
                panic!("an empty graph must refuse");
            };
            assert_eq!(clause_label(&clauses[0]), "graph_empty");
        }
    }

    mod watcher_loss {
        use super::*;

        fn envelope_with(watcher_loss: Option<Value>) -> Envelope {
            let mut reconcile = json!({
                "untracked_path_count": 0,
                "untracked_observed_age_seconds": 0,
                "last_admission_success_at": "2026-09-09T00:00:00Z",
                "last_admission_success_age_seconds": 12,
            });
            if let Some(loss) = watcher_loss {
                reconcile["watcher_loss"] = loss;
            }
            Envelope::daemon().with_health(&json!({
                "graph_loaded": true,
                "initialized": true,
                "reconcile": reconcile,
            }))
        }

        fn standing_loss() -> Value {
            json!({
                "generation": 2,
                "recovered_through": 0,
                "at": "2026-09-09T06:00:00Z",
                "reason": "rescan: kernel dropped",
                "disclosure": "the filesystem watcher lost events (loss generation 2, recovered \
                               through 0)",
            })
        }

        /// The control. Without it, `inconclusive` below could come from
        /// anything on this fixture and would say nothing about the loss.
        #[test]
        fn a_store_with_no_watcher_loss_certifies_on_the_same_payload() {
            let envelope = envelope_with(None);
            assert!(
                envelope.watcher_loss.is_none(),
                "the control fixture must carry no loss: {:?}",
                envelope.watcher_loss
            );
            assert!(matches!(watcher_loss_reading(&envelope), Reading::Silent));

            let verdict = Verdict::compute(
                "find_references",
                &call_only_reference_payload(),
                &envelope,
                None,
            )
            .expect("the readings are not all silent");
            assert!(
                verdict.certified,
                "the control must certify, or the arm below proves nothing: {:?}",
                verdict.limiting_factor
            );
        }

        /// A store whose watcher lost events answers inconclusive, and the
        /// reader is told the generation and the one command that clears it.
        #[test]
        fn a_standing_loss_refuses_the_verdict_and_names_the_recovery() {
            let envelope = envelope_with(Some(standing_loss()));
            let observed = envelope
                .watcher_loss
                .as_ref()
                .expect("a standing loss is read off the wire");
            assert_eq!(observed.generation, 2);
            assert_eq!(observed.recovered_through, 0);
            assert_eq!(
                serde_json::to_value(observed).unwrap()["reason"],
                "rescan: kernel dropped"
            );

            let verdict = Verdict::compute(
                "find_references",
                &populated_reference_payload("present"),
                &envelope,
                None,
            )
            .expect("the readings are not all silent")
            .to_value();

            assert_eq!(verdict["state"], json!(INCONCLUSIVE), "{verdict}");
            assert_eq!(
                verdict["inputs"]["watcher_loss"],
                json!(INCONCLUSIVE),
                "{verdict}"
            );
            let factor = verdict["limiting_factor"]
                .as_str()
                .expect("a refusing verdict names its factor");
            assert!(
                factor
                    .split(CLAUSE_SEPARATOR)
                    .any(|code| code == "watcher_events_lost"),
                "the code survives composition: {factor}"
            );
            assert_eq!(
                envelope.watcher_loss.as_ref().map(|loss| loss.generation),
                Some(2),
                "the reader is told the generation by `_kin.watcher_loss` beside the code"
            );
            assert!(
                CLAUSE_CODES.iter().any(|entry| {
                    entry.code == "watcher_events_lost" && entry.meaning.contains("kin admit")
                }),
                "and the one command that clears it by the code's written meaning"
            );
        }

        #[test]
        fn an_unreadable_loss_record_keeps_its_error_and_refuses_certification() {
            let envelope = envelope_with(Some(json!({
                "generation": 0,
                "recovered_through": 0,
                "read_error": "invalid watcher-loss schema",
            })));
            assert!(envelope.watcher_loss.is_some());
            let verdict = Verdict::compute(
                "find_references",
                &populated_reference_payload("present"),
                &envelope,
                None,
            )
            .expect("an unreadable loss record needs a verdict")
            .to_value();
            assert_eq!(verdict["state"], INCONCLUSIVE);
            assert!(verdict["limiting_factor"]
                .as_str()
                .unwrap()
                .split(CLAUSE_SEPARATOR)
                .any(|code| code == "watcher_loss_unreadable"));
            let written = serde_json::to_value(envelope.for_state_change()).unwrap();
            assert_eq!(
                written["watcher_loss"]["read_error"],
                "invalid watcher-loss schema"
            );
            assert!(written["watcher_loss"].get("reason").is_none());
        }

        /// The bound on all of it. A record a completed full admission covered
        /// stops refusing, so this is a state a store leaves rather than a floor
        /// it lives under.
        #[test]
        fn a_record_a_full_admission_covered_stops_refusing() {
            let mut settled = standing_loss();
            settled["recovered_through"] = json!(2);
            let envelope = envelope_with(Some(settled));

            assert!(
                envelope.watcher_loss.is_none(),
                "a covered generation is not a standing loss: {:?}",
                envelope.watcher_loss
            );
            let verdict = Verdict::compute(
                "find_references",
                &call_only_reference_payload(),
                &envelope,
                None,
            )
            .expect("the readings are not all silent");
            assert!(
                verdict.certified,
                "the verdict returns once the recovery has happened: {:?}",
                verdict.limiting_factor
            );
        }
    }

    mod tracked_changes {
        use super::*;

        fn envelope_with(changed: Option<(u64, Value)>, unchecked: Option<&str>) -> Envelope {
            let mut reconcile = json!({
                "untracked_path_count": 0,
                "untracked_observed_age_seconds": 0,
                "last_admission_success_at": "2026-09-09T00:00:00Z",
                "last_admission_success_age_seconds": 12,
            });
            if let Some((count, sample)) = changed {
                reconcile["changed_path_count"] = json!(count);
                reconcile["changed_paths_sample"] = sample;
            }
            if let Some(reason) = unchecked {
                reconcile["changed_paths_unchecked"] = json!(reason);
            }
            Envelope::daemon().with_health(&json!({
                "graph_loaded": true,
                "initialized": true,
                "reconcile": reconcile,
            }))
        }

        /// The control, on the same payload and the same store with nothing
        /// owed. Without it, `inconclusive` below could come from anything on
        /// this fixture and would say nothing about the tracked files.
        #[test]
        fn a_store_that_owes_no_tracked_file_certifies_on_the_same_payload() {
            let envelope = envelope_with(Some((0, json!([]))), None);
            assert!(
                envelope.behind.is_none(),
                "a store that owes nothing publishes no behind block: {:?}",
                envelope.behind
            );
            assert!(matches!(
                tracked_changes_reading(&envelope),
                Reading::Silent
            ));
            let verdict = Verdict::compute(
                "find_references",
                &call_only_reference_payload(),
                &envelope,
                None,
            )
            .expect("the readings are not all silent");
            assert!(
                verdict.certified,
                "the control must certify, or the arm below proves nothing: {:?}",
                verdict.limiting_factor
            );
        }

        /// A populated answer over a store still holding the old bytes of a
        /// tracked file the host edited while no daemon watched is not
        /// certified: the rows it carries can name code the file no longer
        /// declares. The reader is told which files and what clears them.
        #[test]
        fn a_populated_answer_over_an_unadmitted_tracked_edit_is_inconclusive() {
            let envelope = envelope_with(Some((2, json!(["src/doomed.rs", "src/lib.rs"]))), None);
            let behind = envelope
                .behind
                .as_ref()
                .expect("the owed tracked files are read off the wire");
            assert_eq!(behind.changed_paths, 2);
            assert_eq!(behind.changed_sample, vec!["src/doomed.rs", "src/lib.rs"]);

            let verdict = Verdict::compute(
                "find_references",
                &populated_reference_payload("present"),
                &envelope,
                None,
            )
            .expect("the readings are not all silent")
            .to_value();
            assert_eq!(verdict["state"], json!(INCONCLUSIVE), "{verdict}");
            assert_eq!(
                verdict["inputs"]["tracked_changes"],
                json!(INCONCLUSIVE),
                "{verdict}"
            );
            let factor = verdict["limiting_factor"]
                .as_str()
                .expect("a refusing verdict names its factor");
            assert!(
                factor
                    .split(CLAUSE_SEPARATOR)
                    .any(|code| code == "tracked_changes_unadmitted"),
                "the code survives composition: {factor}"
            );
            assert!(
                CLAUSE_CODES.iter().any(|entry| {
                    entry.code == "tracked_changes_unadmitted"
                        && entry.meaning.contains("kin admit")
                }),
                "the code's written meaning names what clears it"
            );
            // The absence half rides the behind block, so an empty answer is
            // withheld for the same reason and names the tracked files too.
            assert!(
                behind
                    .limiting_factor()
                    .contains("tracked path(s) changed or removed"),
                "{}",
                behind.limiting_factor()
            );
        }

        /// A check that could not run is not a working copy with nothing owed.
        #[test]
        fn a_startup_check_that_could_not_run_refuses_certification() {
            let envelope = envelope_with(None, Some("the walk failed"));
            assert!(envelope
                .behind
                .as_ref()
                .is_some_and(|behind| behind.changed_unchecked));
            let verdict = Verdict::compute(
                "find_references",
                &populated_reference_payload("present"),
                &envelope,
                None,
            )
            .expect("an unchecked working copy needs a verdict")
            .to_value();
            assert_eq!(verdict["state"], INCONCLUSIVE, "{verdict}");
            assert!(verdict["limiting_factor"]
                .as_str()
                .unwrap()
                .split(CLAUSE_SEPARATOR)
                .any(|code| code == "tracked_changes_unchecked"));
        }
    }

    mod graph_freshness {
        use super::*;

        fn envelope_with(reconcile: Value) -> Envelope {
            Envelope::daemon().with_health(&json!({
                "graph_loaded": true,
                "initialized": true,
                "reconcile": reconcile,
            }))
        }

        // Both carry the untracked measurement stamp, because a zero with no
        // stamp is now its own disclosure and `behind` is present for it. These
        // two are about the ADMISSION clock, which is a different reading, so
        // they say plainly that the working copy was measured and level.
        fn with_clock() -> Envelope {
            envelope_with(json!({
                "untracked_path_count": 0,
                "untracked_observed_age_seconds": 0,
                "last_admission_success_at": "2026-08-26T00:00:00Z",
                "last_admission_success_age_seconds": 12,
            }))
        }

        fn without_clock() -> Envelope {
            envelope_with(json!({
                "untracked_path_count": 0,
                "untracked_observed_age_seconds": 0,
            }))
        }

        /// The disclosure, which is what step 1 actually delivers. Both stores
        /// below hold zero unadmitted paths, so `behind` is silent by design and
        /// used to take the clock down with it.
        #[test]
        fn the_clock_is_published_even_when_nothing_is_unadmitted() {
            let recorded = with_clock();
            assert!(
                recorded.behind.is_none(),
                "the count gate is what used to discard this: {:?}",
                recorded.behind
            );
            assert!(
                matches!(
                    recorded.freshness,
                    Some(crate::envelope::GraphFreshness::Recorded { .. })
                ),
                "a clean working copy must not hide a clock the daemon sent: {:?}",
                recorded.freshness
            );
            assert!(
                matches!(
                    without_clock().freshness,
                    Some(crate::envelope::GraphFreshness::NoAdmissionRecorded)
                ),
                "and the absence of one is itself a reading, not a parse failure"
            );
        }

        /// A runtime that reported no reconcile block has nothing to publish.
        /// Distinct from a block that carries no clock, and the two must not
        /// collapse.
        #[test]
        fn a_runtime_reporting_no_reconcile_block_publishes_nothing() {
            let envelope = Envelope::daemon().with_health(&json!({ "graph_loaded": true }));
            assert!(envelope.freshness.is_none());
        }

        /// The seam weighs nothing, with a clock. Certifying here would state
        /// agreement this cannot support: a present clock proves an admission
        /// happened at some time, not that the store is current, and the
        /// verdict's note asserts every input agreed.
        #[test]
        fn a_recorded_clock_does_not_certify() {
            assert!(matches!(
                graph_freshness_reading(&with_clock()),
                Reading::Silent
            ));
        }

        /// The seam weighs nothing, without one. Refusing here floors every
        /// answer on every freshly initialized store, because the wire's clock is
        /// the daemon's in-memory record and a daemon that has not yet completed
        /// an admission pass carries none. This arm is the one the acceptance
        /// control already proved can fail.
        #[test]
        fn an_unrecorded_clock_does_not_refuse() {
            assert!(matches!(
                graph_freshness_reading(&without_clock()),
                Reading::Silent
            ));
        }

        /// The replayed sample the status producer publishes: the selected graph
        /// could not be sampled live, so the counters beside it are an
        /// observation from a measured while ago.
        fn replayed_sample() -> Envelope {
            with_clock().with_selected_graph_staleness(
                "the authority epoch moved under the sample",
                4_200,
                Some(19),
                3,
            )
        }

        /// A coverage reading with nothing wrong with it, which is what makes
        /// the arm below worth having: without the freshness input this exact
        /// envelope certifies.
        fn complete_coverage() -> crate::envelope::Completeness {
            crate::envelope::Completeness {
                status: "complete".to_string(),
                bound: "exact".to_string(),
                substrate: "graph".to_string(),
                classes: Map::new(),
                decided_by: Vec::new(),
                counted: None,
                reference_resolution: None,
                limits: Vec::new(),
            }
        }

        /// The replayed sample refuses, and it refuses with the sentence the
        /// envelope publishes rather than with a second copy of it.
        #[test]
        fn a_replayed_sample_refuses_with_the_clause_the_envelope_publishes() {
            let envelope = replayed_sample();
            let reading = graph_freshness_reading(&envelope);
            let state = reading.state();
            let Reading::Inconclusive(clauses) = reading else {
                panic!("a replayed sample is an observation, not an absence: {state}");
            };
            assert_eq!(
                clauses,
                vec![envelope
                    .freshness
                    .as_ref()
                    .expect("the fixture sets a freshness")
                    .limiting_factor()
                    .expect("a stale sample names a limiting factor")],
                "the verdict refuses with the type's own sentence"
            );
            let clause = &clauses[0];
            assert!(
                clause.starts_with("selected_graph_sample_stale:")
                    && clause.contains("after 3 attempt(s)")
                    && clause.contains("the authority epoch moved under the sample")
                    && clause.contains("4200 ms earlier"),
                "the factor names the failure mode, the attempts and the age: {clause}"
            );
        }

        /// Complete coverage over counters that replay an earlier
        /// observation. `completeness` is a reading of the SUBSTRATE
        /// and cannot see that the sample was not live, so before this arm
        /// existed `_kin.verdict` read certified here.
        #[test]
        fn a_replayed_sample_with_complete_coverage_reads_inconclusive() {
            let mut envelope = replayed_sample();
            envelope.completeness = Some(complete_coverage());

            let verdict = Verdict::compute(
                "find_references",
                &call_only_reference_payload(),
                &envelope,
                None,
            )
            .expect("the readings are not all silent");

            assert!(
                !verdict.certified,
                "a replayed sample cannot be certified over: {:?}",
                verdict.limiting_factor
            );
            assert_eq!(
                verdict
                    .inputs
                    .get("graph_freshness")
                    .and_then(Value::as_str),
                Some(INCONCLUSIVE),
                "and the refusing input is named in the stamp: {:?}",
                verdict.inputs
            );
            assert!(
                verdict
                    .limiting_factor
                    .as_deref()
                    .unwrap_or_default()
                    .contains("selected_graph_sample_stale"),
                "the factor names it: {:?}",
                verdict.limiting_factor
            );

            // The control that makes the arm above mean something: the same
            // coverage under a live sample still certifies, so the refusal is
            // the freshness reading rather than anything else in this fixture.
            let mut live = with_clock();
            live.completeness = Some(complete_coverage());
            // Named for the sample rather than for the outcome. A local called
            // `certified` matches CodeQL's certificate-name heuristic for
            // `rust/cleartext-logging`, and `assert!` counts as a log sink, so
            // the earlier spelling raised a high-severity alert on this test.
            let live_sample = Verdict::compute(
                "find_references",
                &call_only_reference_payload(),
                &live,
                None,
            )
            .expect("the readings are not all silent");
            assert!(
                live_sample.certified,
                "the fixture certifies without the stale sample: {:?}",
                live_sample.limiting_factor
            );
        }

        /// The readings array and the stamp's derivation agree on every name,
        /// which neither side can prove on its own.
        ///
        /// kin#1123 asserts `edge_coverage_limits` picks up an input the
        /// function was never told about, and it happens to use
        /// `graph_freshness` as that unknown name, with the map built by hand.
        /// The arms above assert `compute` puts `graph_freshness` into the map.
        /// Both hardcode the string, so renaming the reading leaves both green
        /// while the real behaviour breaks: the stamp would go on naming a key
        /// nothing emits, and the emitted key would be one nothing names.
        ///
        /// So this asserts the join rather than either end. It reads the names
        /// `compute` actually produced and requires the stamp to name each one
        /// when that input refuses, which is a property over the real input set
        /// and cannot be satisfied by a string written twice.
        #[test]
        fn every_input_compute_emits_is_one_the_stamp_can_name() {
            let verdict = Verdict::compute(
                "find_references",
                &json!({ "references": [] }),
                &with_clock(),
                None,
            )
            .expect("the readings are not all silent");
            let names: Vec<String> = verdict.inputs.keys().cloned().collect();
            assert!(
                names.iter().any(|name| name == "graph_freshness"),
                "the seam must be in the stamp's input set at all: {names:?}"
            );

            for name in names {
                if name == "edge_coverage" {
                    continue;
                }
                let mut inputs = Map::new();
                inputs.insert("edge_coverage".to_string(), json!(CERTIFIED));
                inputs.insert(name.clone(), json!(INCONCLUSIVE));
                let refusing = Verdict {
                    scorer: None,
                    certified: false,
                    except_residue: false,
                    absence_claim: AbsenceClaim::NotAuthoritative,
                    limiting_factor: Some(format!("{name}: refusing")),
                    inputs,
                };
                assert!(
                    refusing
                        .edge_coverage_limits()
                        .contains(&format!("{name}:inconclusive")),
                    "the stamp must name {name}, which compute emits, without being told about it"
                );
            }
        }

        /// End to end: neither ADMISSION-CLOCK store may pick up a freshness
        /// clause, and the input is present in the stamp as `not_applicable`
        /// rather than absent, so the seam is visible to a reader.
        ///
        /// Scoped to the clock deliberately. A replayed sample is exempt because
        /// it is a different reading with a different basis, and it is required
        /// to put its clause in the factor by
        /// `a_replayed_sample_with_complete_coverage_reads_inconclusive` below.
        /// Widened here from the admission label alone to every freshness label,
        /// so a future arm that starts refusing on the clock cannot slip past
        /// this by choosing a different word.
        #[test]
        fn neither_admission_clock_state_puts_a_freshness_clause_in_the_verdict() {
            for envelope in [with_clock(), without_clock()] {
                let verdict = Verdict::compute(
                    "find_references",
                    &json!({ "references": [] }),
                    &envelope,
                    None,
                );
                let Some(verdict) = verdict else { continue };
                assert_eq!(
                    verdict
                        .inputs
                        .get("graph_freshness")
                        .and_then(Value::as_str),
                    Some(NOT_APPLICABLE),
                    "the seam is wired and weighs nothing: {:?}",
                    verdict.inputs
                );
                let factor = verdict.limiting_factor.as_deref().unwrap_or_default();
                assert!(
                    !factor.contains("graph_admission")
                        && !factor.contains("selected_graph_sample_stale"),
                    "no freshness clause may reach the factor from an admission clock: {:?}",
                    verdict.limiting_factor
                );
            }
        }
    }

    /// FIR-2673 finding 1, the case the stranger read as a refusal.
    ///
    /// A populated answer, every input clean, verdict certified. The old bool
    /// was FALSE here, correctly, because no absence is claimed, and the note
    /// beside it promised that an absence in the answer was authoritative. A
    /// consumer branching on the bool got the opposite of the verdict, and the
    /// one object said both things at once.
    /// A reference answer whose call-site block names only a listed residue.
    fn residue_payload(clauses: &[&str]) -> Value {
        let mut payload = call_only_reference_payload();
        payload["call_sites"] = json!({ "settled": clauses.is_empty(), "clauses": clauses });
        payload
    }

    const NAMED_RESIDUE: &str = "named_leftovers: 13 call site(s) whose callee or caller body \
                                 spells pop are not proven: 13 are listed by caller and line";
    const VALUE_RESIDUE: &str = "value_calls_unmeasured: 1256 unsettled call site(s) in 400 \
                                 caller(s) never spell a call name of the focal";

    #[test]
    fn reference_query_certainty_does_not_borrow_completion_from_a_call_census() {
        // Class presence and resolver availability do not attest completion of
        // non-call references. Even a settled Call ledger says nothing about
        // retained Reference sites whose enrichment could not settle.
        for kinds in [
            json!(["references"]),
            json!(["calls", "references"]),
            json!(["References"]),
            json!(["Calls", "Imports", "References"]),
        ] {
            for clauses in [
                Vec::new(),
                vec!["unkeyed_lookups_counted: 2 lookups counted"],
            ] {
                let mut payload = residue_payload(&clauses);
                payload["relation_kinds"] = kinds.clone();
                payload["edge_coverage"]["requested_classes"] = kinds.clone();
                let result = crate::finalize_with_envelope(
                    crate::types::ToolCallResult::text(payload.to_string()),
                    Envelope::daemon().with_health(&json!({
                        "initialized": true, "graph_loaded": true, "graph_generation": 12,
                    })),
                    "find_references",
                );
                let crate::types::ContentBlock::Text { text } = &result.content[0];
                let response: Value = serde_json::from_str(text).unwrap();
                let verdict = &response["_kin"]["verdict"];
                assert_eq!(verdict["state"], INCONCLUSIVE, "{response}");
                assert!(
                    !verdict["limiting_factor"]
                        .as_str()
                        .is_some_and(|factor| factor.contains(UNLISTED_CLAUSE_CODE)),
                    "{response}"
                );
                assert!(
                    verdict["limiting_factor"]
                        .as_str()
                        .is_some_and(|factor| factor.contains("reference_completion_unmeasured")),
                    "{response}"
                );
                assert_eq!(verdict["safe_to_conclude_absent"], false, "{response}");
                assert_eq!(response["negative"]["trust"], INCONCLUSIVE, "{response}");
                assert!(
                    response["negative"]["trust_reason"]
                        .as_str()
                        .is_some_and(|factor| factor.contains("reference_completion_unmeasured")),
                    "{response}"
                );
                assert!(response["_kin"].get("self_check").is_none(), "{response}");
            }
        }
    }

    #[test]
    fn reference_query_certainty_keeps_call_only_certification() {
        for kind in ["calls", "Calls", "CALLS"] {
            for (clauses, expected) in [
                (Vec::new(), CERTIFIED),
                (
                    vec!["unkeyed_lookups_counted: 2 lookups counted"],
                    CERTIFIED_EXCEPT,
                ),
            ] {
                let mut payload = residue_payload(&clauses);
                payload["relation_kinds"] = json!([kind]);
                payload["edge_coverage"]["requested_classes"] = json!(["calls"]);
                let verdict =
                    Verdict::compute("find_references", &payload, &Envelope::daemon(), None)
                        .unwrap()
                        .to_value();
                assert_eq!(verdict["state"], expected, "{verdict}");
                assert!(
                    !verdict["limiting_factor"]
                        .as_str()
                        .is_some_and(|factor| factor.contains("reference_completion_unmeasured")),
                    "{verdict}"
                );
            }
        }
    }

    #[test]
    fn an_answer_whose_only_refusals_are_a_listed_residue_is_certified_except() {
        let payload = residue_payload(&[NAMED_RESIDUE, VALUE_RESIDUE]);
        let negative = json!({
            "interpretation": "qualified_answer",
            "trust": "inconclusive",
            "trust_reason": format!("{NAMED_RESIDUE}; {VALUE_RESIDUE}"),
        });
        let verdict = Verdict::compute(
            "find_references",
            &payload,
            &Envelope::daemon(),
            Some(&negative),
        )
        .expect("a retrieval payload carries a verdict");
        let value = verdict.to_value();
        assert_eq!(value["state"], json!(CERTIFIED_EXCEPT), "{value}");
        assert_eq!(
            value["limiting_factor"],
            json!("named_leftovers; value_calls_unmeasured"),
            "{value}"
        );
        assert_eq!(value["safe_to_conclude_absent"], json!(false), "{value}");
        assert_eq!(
            value["inputs"]["call_sites"],
            json!(INCONCLUSIVE),
            "{value}"
        );
        // The counts are a floor with a named remainder, and say so.
        let mut completeness = Some(crate::envelope::Completeness {
            status: "complete".to_string(),
            bound: "exact".to_string(),
            substrate: "edges".to_string(),
            classes: Map::new(),
            decided_by: Vec::new(),
            counted: None,
            reference_resolution: None,
            limits: Vec::new(),
        });
        verdict.project_onto_completeness(&mut completeness);
        let completeness = completeness.unwrap();
        assert_eq!(completeness.bound, "at_least");
        assert_eq!(completeness.limits, vec![VERDICT_EXCEPT_LIMIT.to_string()]);
    }

    /// A folded binding gap downgrades a certified answer only to complete
    /// except it, never past a refusal another input made, and never licenses
    /// an absence.
    #[test]
    fn a_folded_binding_gap_is_complete_except_and_never_an_absence() {
        const FOLDED: &str = "local_binding_unrelated: the bindings sit outside the answer";
        assert!(is_residue_code(LOCAL_BINDING_UNRELATED));
        assert!(factor_is_residue_only(
            "local_binding_unrelated; named_leftovers"
        ));
        assert!(!factor_is_residue_only(
            "local_binding_unrelated; local_binding_outstanding"
        ));
        assert!(CLAUSE_CODES
            .iter()
            .any(|entry| entry.code == LOCAL_BINDING_UNRELATED));
        for (state, factor, claim, expected) in [
            (CERTIFIED, Value::Null, "authoritative", CERTIFIED_EXCEPT),
            (
                CERTIFIED_EXCEPT,
                json!("named_leftovers"),
                "not_applicable",
                CERTIFIED_EXCEPT,
            ),
            (
                INCONCLUSIVE,
                json!("method_call_resolution_incomplete"),
                "not_authoritative",
                INCONCLUSIVE,
            ),
        ] {
            let mut payload = json!({"_kin": {"verdict": {
                "state": state, "limiting_factor": factor, "absence_claim": claim,
                "safe_to_conclude_absent": claim == "authoritative", "inputs": {},
            }}});
            qualify_source_observation(&mut payload, FOLDED);
            let verdict = &payload["_kin"]["verdict"];
            assert_eq!(verdict["state"], json!(expected), "{state}: {verdict}");
            assert_eq!(
                verdict["safe_to_conclude_absent"],
                json!(false),
                "{verdict}"
            );
            assert!(
                verdict["limiting_factor"]
                    .as_str()
                    .unwrap()
                    .starts_with(LOCAL_BINDING_UNRELATED),
                "{verdict}"
            );
        }
    }

    #[test]
    fn a_residue_beside_any_other_refusal_stays_inconclusive() {
        let owed = "call_sites_owed: 1 of the 9 callers in the store's callers that could call \
                    the focal have call sites the graph has not settled yet";
        let payload = residue_payload(&[owed, NAMED_RESIDUE]);
        let negative = json!({
            "interpretation": "qualified_answer",
            "trust": "inconclusive",
            "trust_reason": format!("{owed}; {NAMED_RESIDUE}"),
        });
        let value = Verdict::compute(
            "find_references",
            &payload,
            &Envelope::daemon(),
            Some(&negative),
        )
        .expect("a retrieval payload carries a verdict")
        .to_value();
        assert_eq!(value["state"], json!(INCONCLUSIVE), "{value}");
    }

    #[test]
    fn an_empty_answer_with_a_residue_never_licenses_its_absence() {
        let mut payload = residue_payload(&[VALUE_RESIDUE]);
        payload["references"] = json!([]);
        payload["total_upstream"] = json!(0);
        let negative = json!({
            "interpretation": "absence_claimed",
            "trust": "inconclusive",
            "trust_reason": VALUE_RESIDUE,
        });
        let value = Verdict::compute(
            "find_references",
            &payload,
            &Envelope::daemon(),
            Some(&negative),
        )
        .expect("a retrieval payload carries a verdict")
        .to_value();
        assert_eq!(value["state"], json!(CERTIFIED_EXCEPT), "{value}");
        assert_eq!(
            value["absence_claim"],
            json!("not_authoritative"),
            "{value}"
        );
        assert_eq!(value["safe_to_conclude_absent"], json!(false), "{value}");
    }

    /// Unkeyed lookups are counted, not listed, and the answer reads complete
    /// except that count. They are still lookups that could reach the focal,
    /// so an empty answer beside them never licenses its absence.
    #[test]
    fn an_empty_answer_beside_unkeyed_lookups_never_licenses_its_absence() {
        const UNKEYED: &str = "unkeyed_lookups_counted: 14 lookups by a name computed at run \
                               time could reach the focal as they could reach anything their \
                               receiver holds, and none spells a call name";
        let mut payload = residue_payload(&[UNKEYED]);
        payload["references"] = json!([]);
        payload["total_upstream"] = json!(0);
        let negative = json!({
            "interpretation": "absence_claimed",
            "trust": "inconclusive",
            "trust_reason": UNKEYED,
        });
        let value = Verdict::compute(
            "find_references",
            &payload,
            &Envelope::daemon(),
            Some(&negative),
        )
        .expect("a retrieval payload carries a verdict")
        .to_value();
        assert_eq!(value["state"], json!(CERTIFIED_EXCEPT), "{value}");
        assert_eq!(
            value["limiting_factor"],
            json!("unkeyed_lookups_counted"),
            "{value}"
        );
        assert_eq!(value["safe_to_conclude_absent"], json!(false), "{value}");
    }

    #[test]
    fn a_certified_except_response_reads_as_one_verdict() {
        let mut response = agreeing_response();
        response[crate::envelope::ENVELOPE_KEY][VERDICT_KEY]["state"] = json!(CERTIFIED_EXCEPT);
        response[crate::envelope::ENVELOPE_KEY][VERDICT_KEY]["safe_to_conclude_absent"] =
            json!(false);
        response[crate::envelope::ENVELOPE_KEY][VERDICT_KEY]["limiting_factor"] =
            json!("named_leftovers");
        if let Some(negative) = response.get_mut(crate::negative::NEGATIVE_KEY) {
            negative["trust"] = json!("inconclusive");
            negative["safe_to_conclude_absent"] = json!(false);
            negative["advice"] = json!("Check the listed call sites.");
        }
        if let Some(completeness) = response[crate::envelope::ENVELOPE_KEY].get_mut("completeness")
        {
            completeness["bound"] = json!("at_least");
            completeness["counted"]["exact"] = json!(false);
        }
        assert_eq!(disagreements(&response), Vec::<String>::new(), "{response}");
        // And it can never sit under a completeness that claims whole counts.
        response[crate::envelope::ENVELOPE_KEY]["completeness"]["bound"] = json!("exact");
        assert!(!disagreements(&response).is_empty(), "{response}");
    }

    #[test]
    fn a_budget_cut_turns_certified_except_inconclusive() {
        let mut annotated = json!({
            "_kin": {"verdict": {
                "state": CERTIFIED_EXCEPT,
                "safe_to_conclude_absent": false,
                "limiting_factor": "named_leftovers",
                "inputs": {},
            }},
        });
        mark_response_bounded(&mut annotated);
        assert_eq!(annotated["_kin"]["verdict"]["state"], json!(INCONCLUSIVE));
        assert_eq!(
            annotated["_kin"]["verdict"]["limiting_factor"],
            json!("response_bounded; named_leftovers")
        );
    }

    #[test]
    fn a_populated_certified_answer_claims_no_absence_and_promises_none() {
        let payload = call_only_reference_payload();
        let negative = json!({ "interpretation": "qualified_answer", "trust": "authoritative" });
        let verdict = Verdict::compute(
            "find_references",
            &payload,
            &Envelope::daemon(),
            Some(&negative),
        )
        .expect("a retrieval payload carries a verdict")
        .to_value();

        assert_eq!(verdict["state"], json!(CERTIFIED), "{verdict}");
        assert_eq!(
            verdict["absence_claim"],
            json!("not_applicable"),
            "a populated answer claims no absence: {verdict}"
        );
        assert!(
            verdict.get("note").is_none(),
            "v2 carries no sentence, so none can promise an absence the answer does not claim: \
             {verdict}"
        );
        // The legacy bool stays false here, and that is correct rather than a
        // bug: it is now DEFINED from the tri-state, so it can no longer
        // contradict the claim beside it.
        assert_eq!(
            verdict["safe_to_conclude_absent"],
            json!(false),
            "{verdict}"
        );
    }

    /// The inverse, and the half that stops the split trading a false certify
    /// for a field that never says yes.
    ///
    /// An empty answer with every requested class present must still certify
    /// its absence, or a fix that answers `not_applicable` to everything would
    /// pass the check above and say nothing.
    #[test]
    fn an_empty_certified_answer_still_certifies_its_absence() {
        let mut payload = call_only_reference_payload();
        payload["references"] = json!([]);
        payload["total_upstream"] = json!(0);
        let negative = json!({ "interpretation": "absence_claimed", "trust": "authoritative" });
        let verdict = Verdict::compute(
            "find_references",
            &payload,
            &Envelope::daemon(),
            Some(&negative),
        )
        .expect("a retrieval payload carries a verdict")
        .to_value();

        assert_eq!(verdict["state"], json!(CERTIFIED), "{verdict}");
        assert_eq!(
            verdict["absence_claim"],
            json!("authoritative"),
            "an empty certified answer's absence is authoritative: {verdict}"
        );
        assert_eq!(verdict["limiting_factor"], Value::Null, "{verdict}");
        assert_eq!(verdict["safe_to_conclude_absent"], json!(true), "{verdict}");
        assert!(
            verdict.get("note").is_none(),
            "the authoritative absence is the two fields above, never a sentence: {verdict}"
        );
    }

    /// The absence gate's `trust_reason` is a JOIN, and its clauses must
    /// deduplicate against the later readings' like any others.
    ///
    /// This is the regression a unit test did not have. Taking `trust_reason`
    /// as one opaque clause looked conservative and was not: the negative block
    /// composes it by repeated concatenation, so it carries a
    /// `retrieval_degraded` clause of its own, and as one blob that clause
    /// stopped matching the `degradations` reading's. One live answer then named
    /// the same gap twice, and the `two_reasons` acceptance check caught it on a
    /// real payload after every unit test here passed.
    ///
    /// Splitting a wire string that IS a join recovers a boundary; splitting a
    /// factor Kin had just joined itself invented one. Same operation, opposite
    /// sides of the same seam, and only one of them is parsing prose.
    #[test]
    fn a_trust_reason_that_repeats_a_later_reading_says_that_gap_once() {
        let negative = json!({
            "trust": "inconclusive",
            "trust_reason": "response_bounded: the response budget withheld part of this answer; \
                             retrieval_degraded: this query reported degradations \
                             [embed_worker:failed], so it did not run at full capability",
        });
        let readings = [
            (
                "absence_gate",
                absence_gate_reading("find_references", &json!({}), Some(&negative)),
            ),
            (
                "degradations",
                Reading::Inconclusive(vec![
                    "retrieval_degraded: this query reported degradations \
                                            [embed_worker:failed], so it did not run at full \
                                            capability"
                        .to_string(),
                ]),
            ),
        ];
        let factor = compose_limiting_factor(&readings).expect("two inputs refused");
        assert_eq!(
            factor
                .split(CLAUSE_SEPARATOR)
                .filter(|code| *code == "retrieval_degraded")
                .count(),
            1,
            "the gap the trust_reason and the degradations reading both carry is said once: \
             {factor}"
        );
        assert!(
            factor
                .split(CLAUSE_SEPARATOR)
                .any(|code| code == "response_bounded"),
            "and the trust_reason's other clause survives rather than being swallowed: {factor}"
        );
    }

    /// No clause may contain the string that separates clauses.
    ///
    /// The invariant the FIR-2723 fix rests on, asserted rather than assumed.
    /// Clauses are carried as a list now, so Kin no longer mis-parses its own
    /// factor, but the rendered sentence is still one string, and any reader
    /// that splits it on the separator sees whatever the prose contains. Two
    /// gap texts used to carry one, `cross_file_edges_absent` and
    /// `name_filter_narrowed_to_zero`, and each arrived at the reader as a
    /// labelled clause plus a bare fragment with no label at all.
    ///
    /// This drives the real producer over the shapes that reach it rather than
    /// re-listing the texts, because a test that restates the strings is a
    /// second copy of them and goes stale the day one is edited.
    #[test]
    fn no_clause_carries_the_separator_that_divides_clauses() {
        let shapes = [
            ("find_references", populated_reference_payload("absent")),
            ("find_references", populated_reference_payload("unproduced")),
            ("find_references", populated_reference_payload("unknown")),
            ("impact_analysis", populated_reference_payload("absent")),
            ("trace_data_flow", populated_reference_payload("absent")),
        ];
        let mut seen = 0;
        for (tool, payload) in &shapes {
            for clause in crate::negative::absence_coverage_clauses(tool, payload) {
                seen += 1;
                assert!(
                    !clause.contains(CLAUSE_SEPARATOR),
                    "{tool}: a clause carries the separator, so any reader that splits the \
                     rendered factor will cut it into a labelled clause and an unlabelled \
                     fragment: {clause}"
                );
            }
        }
        assert!(
            seen > 0,
            "no clause was produced by any shape, so this asserted nothing"
        );
    }

    /// `unproduced` carries two reasons, and this reading says which one it has.
    ///
    /// The state was minted for a linker that resolved import sites and emitted
    /// no entity-level edge for any of them. A class this build mints for no
    /// language at all arrives at the same state by a different road, and the
    /// sentence written for the first is false for the second: there are no
    /// resolved sites to blame, and a reader who believes there are goes hunting
    /// a linker bug that is not there. Both arms run, because a split that only
    /// ever renders one of its branches is not a split, and the payload's own
    /// evidence is what decides which arm this is rather than the language name.
    #[test]
    fn the_reading_says_which_of_the_two_reasons_unproduced_carries() {
        let mut payload = populated_reference_payload("unproduced");
        payload["edge_coverage"]["language"] = json!("Rust");

        let Reading::Inconclusive(linker) = edge_coverage_reading("find_references", &payload)
        else {
            panic!("an unproduced class bounds the answer: {payload}");
        };
        assert!(
            linker
                .iter()
                .any(|clause| clause.contains("although the source carries")),
            "with nothing saying the build mints none, the linker sentence stands: {linker:?}"
        );

        payload["edge_coverage"]["unproduced_evidence"] = json!({
            "imports": {
                "this_build_mints_no_entity_level_edge_for": ["Rust"],
                "entity_level_import_edges": 0,
            }
        });
        let Reading::Inconclusive(build) = edge_coverage_reading("find_references", &payload)
        else {
            panic!("the class still bounds the answer: {payload}");
        };
        assert!(
            build
                .iter()
                .any(|clause| clause.contains("mints no entity-level imports edge for Rust at all")),
            "the build gap gets its own words: {build:?}"
        );
        assert!(
            !build
                .iter()
                .any(|clause| clause.contains("the source carries")),
            "a class no build mints has no resolved site to blame: {build:?}"
        );
        assert!(
            build
                .iter()
                .all(|clause| clause.starts_with("cross_file_edges_unproduced:")),
            "the label is what a consumer keys on and it does not change: {build:?}"
        );
    }

    /// A label the closed list does not carry goes out as the one listed code
    /// that says so. It still refuses, because the reading that carried it
    /// refused, and it is never minted as a code no reader can look up.
    #[test]
    fn an_unlisted_label_is_sent_as_unlisted_clause_and_still_refuses() {
        let readings = [
            (
                "degradations",
                Reading::Inconclusive(vec![
                    "not_a_listed_code: a producer nobody registered".to_string()
                ]),
            ),
            (
                "completeness",
                Reading::Inconclusive(vec![
                    "counts_are_a_floor: the numbers are a floor".to_string()
                ]),
            ),
        ];
        let factor = compose_limiting_factor(&readings).expect("two inputs refused");
        assert_eq!(factor, "unlisted_clause; counts_are_a_floor");
        assert!(
            CLAUSE_CODES
                .iter()
                .any(|entry| entry.code == UNLISTED_CLAUSE_CODE),
            "the fallback is itself a listed code"
        );
    }

    /// FIR-2672, second finding. A verdict with two independent reasons names
    /// both: the class gap decided the state and the failed embedding worker
    /// stayed in the sentence after it, and the gap the absence gate and the
    /// coverage reading both carry is said once. Remove a clause and the
    /// reader loses one of the two things wrong with the answer.
    #[test]
    fn every_refusing_input_keeps_its_clause_in_the_factor() {
        let readings = [
            (
                "absence_gate",
                Reading::Inconclusive(vec![
                    "cross_file_edges_absent: the graph holds no cross-file imports edges for \
                     python"
                        .to_string(),
                ]),
            ),
            (
                "edge_coverage",
                Reading::Inconclusive(vec![
                    "cross_file_edges_absent: the graph was not observed to hold imports edges"
                        .to_string(),
                ]),
            ),
            ("withheld_candidates", Reading::Certified),
            (
                "degradations",
                Reading::Inconclusive(vec![
                    "retrieval_degraded: this query reported degradations [embed_worker_failed], \
                     so it did not run at full capability"
                        .to_string(),
                ]),
            ),
            ("completeness", Reading::Silent),
        ];
        let factor = compose_limiting_factor(&readings).expect("two inputs refused");
        assert_eq!(factor, "cross_file_edges_absent; retrieval_degraded");
        assert!(
            compose_limiting_factor(&[
                ("absence_gate", Reading::Certified),
                ("degradations", Reading::Silent),
            ])
            .is_none(),
            "no refusing input, no factor"
        );
    }

    /// The same two reasons through `Verdict::compute` itself: a short class
    /// and a run degradation on one populated answer. The class gap decides
    /// and leads, the degradation follows in the same sentence, and the class
    /// gap the absence gate and the coverage reading both carry is said once.
    #[test]
    fn a_verdict_with_two_refusing_inputs_names_both_in_order() {
        let mut payload = populated_reference_payload("absent");
        payload["degradations"] = json!([{"component": "embed_worker", "reason": "failed"}]);
        let verdict = Verdict::compute("find_references", &payload, &Envelope::daemon(), None)
            .expect("a retrieval payload carries a verdict")
            .to_value();
        assert_eq!(verdict["state"], json!(INCONCLUSIVE), "{verdict}");
        assert_eq!(
            verdict["inputs"]["edge_coverage"],
            json!(INCONCLUSIVE),
            "{verdict}"
        );
        assert_eq!(
            verdict["inputs"]["degradations"],
            json!(INCONCLUSIVE),
            "{verdict}"
        );
        let factor = verdict["limiting_factor"].as_str().expect("named");
        let class_gap = factor
            .find("cross_file_edges_absent")
            .unwrap_or_else(|| panic!("the class gap is named: {factor}"));
        let degraded = factor
            .find("retrieval_degraded")
            .unwrap_or_else(|| panic!("the degradation stays named beside it: {factor}"));
        assert!(class_gap < degraded, "the structural gap leads: {factor}");
        assert_eq!(
            factor
                .split(CLAUSE_SEPARATOR)
                .filter(|code| *code == "cross_file_edges_absent")
                .count(),
            1,
            "one fact, one code: {factor}"
        );
        // Under envelope v1 the two readings carried two sentences for this one
        // gap and the readings' order decided which survived dedupe: the absence
        // gate's, which said the gap is in extraction rather than in the caller's
        // code and why the present classes do not stand in for the missing one, or
        // the coverage reading's, which said only that the edges were not
        // observed. Under v2 both collapse to one code, so what a reader is told
        // no longer depends on that order; it depends on the code's one written
        // meaning, which is what has to carry the two things the specific
        // sentence carried.
        let meaning = CLAUSE_CODES
            .iter()
            .find(|entry| entry.code == "cross_file_edges_absent")
            .map(|entry| entry.meaning)
            .expect("the class gap is a listed code");
        assert!(
            meaning.contains("rather than in the code"),
            "the code's meaning tells the reader the gap is not in their code: {meaning}"
        );
        assert!(
            meaning.contains("do not stand in for"),
            "and why the classes that are present do not compensate: {meaning}"
        );
        // The degradation's own label rides the payload, which is where the
        // `retrieval_degraded` code sends a reader.
        assert!(
            crate::negative::payload_degradation_labels(&payload)
                .iter()
                .any(|label| label == "embed_worker:failed"),
            "{factor}"
        );
    }

    /// A response whose blocks all agree with a certified verdict.
    fn agreeing_response() -> Value {
        json!({
            "total_upstream": 2,
            "unconfirmed_candidates": 0,
            "candidates": [],
            "_kin": {
                "verdict": {
                    "state": CERTIFIED,
                    "safe_to_conclude_absent": true,
                    "limiting_factor": Value::Null,
                },
                "completeness": {
                    "status": "complete",
                    "bound": "exact",
                    "counted": {"reported": 2, "exact": true},
                    "note": "the counts here are the whole set.",
                },
            },
            "negative": {
                "safe_to_conclude_absent": true,
                "trust": "authoritative",
                "advice": "Absence is authoritative: safe to treat the target as genuinely absent or unused.",
            },
        })
    }

    /// The scanner must be able to say "no disagreement", or every assertion
    /// built on it passes for the wrong reason.
    #[test]
    fn an_agreeing_response_reports_no_disagreement() {
        assert!(
            disagreements(&agreeing_response()).is_empty(),
            "{:?}",
            disagreements(&agreeing_response())
        );
    }

    /// A response with no verdict at all is not a disagreement: mutations,
    /// session calls and everything else that is not retrieval carry none.
    #[test]
    fn a_response_without_a_verdict_is_not_scanned() {
        assert!(disagreements(&json!({"committed": true})).is_empty());
    }

    /// FIR-2463 case (c). The old two-verdict shape, verbatim from the shipped
    /// v0.5.42 payload the stranger quoted: `completeness` calling an answer
    /// complete, exact and "the whole set" while `negative` on the same object
    /// refuses to certify it. Forcing that shape back has to be caught at every
    /// field a reader acts on, not just the first.
    #[test]
    fn the_old_two_verdict_shape_cannot_serialize() {
        let mut response = agreeing_response();
        response["_kin"]["verdict"]["state"] = json!(INCONCLUSIVE);
        response["_kin"]["verdict"]["safe_to_conclude_absent"] = json!(false);

        let found = disagreements(&response);
        for expected in [
            "negative.safe_to_conclude_absent is true while _kin.verdict.safe_to_conclude_absent \
             is false",
            "negative.trust reads authoritative under an inconclusive _kin.verdict",
            "negative.advice certifies an absence under an inconclusive _kin.verdict",
            "_kin.completeness.bound reads exact under an inconclusive _kin.verdict",
            "_kin.completeness.counted.exact is true under an inconclusive _kin.verdict",
        ] {
            assert!(
                found.iter().any(|message| message == expected),
                "the scanner missed `{expected}`: {found:?}"
            );
        }
    }

    /// The headline-count half of the same invariant. A zero beside a populated
    /// `candidates` array may not ship without `unconfirmed_candidates` naming
    /// the held rows at the count.
    #[test]
    fn a_headline_that_hides_its_held_rows_cannot_serialize() {
        let mut response = agreeing_response();
        response["total_upstream"] = json!(0);
        response["candidates"] = json!([{"name": "Session.send", "reference_lines": [784]}]);
        response
            .as_object_mut()
            .unwrap()
            .remove("unconfirmed_candidates");

        let missing = disagreements(&response);
        assert!(
            missing.iter().any(|message| message
                == "the response carries 1 candidate row(s) and no unconfirmed_candidates beside \
                    its headline count"),
            "{missing:?}"
        );

        // Present but wrong is caught too, at each of the three placements the
        // one withheld number surfaces at.
        response["unconfirmed_candidates"] = json!(0);
        response["counts"] =
            json!({"receiver_name_candidates": 0, "unresolved_name_candidates": 0});
        response["_kin"]["completeness"]["counted"]["withheld_candidates"] = json!(4);
        let skewed = disagreements(&response);
        for expected in [
            "unconfirmed_candidates reads 0 against 1 candidate row(s)",
            "counts.receiver_name_candidates plus counts.unresolved_name_candidates reads 0 \
             against 1 candidate row(s)",
            "_kin.completeness.counted.withheld_candidates reads 4 against 1 candidate row(s)",
        ] {
            assert!(
                skewed.iter().any(|message| message == expected),
                "the scanner missed `{expected}`: {skewed:?}"
            );
        }

        // And the agreeing form clears, so the check is not simply always-on.
        response["unconfirmed_candidates"] = json!(1);
        response["counts"] =
            json!({"receiver_name_candidates": 1, "unresolved_name_candidates": 0});
        response["_kin"]["completeness"]["counted"]["withheld_candidates"] = json!(1);
        assert!(
            headline_count_disagreements(&response).is_empty(),
            "{:?}",
            headline_count_disagreements(&response)
        );

        // The accounting is the SUM of the two grounds, so a row held by the
        // resolution floor alone agrees just as well as one held by the fan-out.
        // Read against the fan-out count alone this response was reported broken
        // on every answer that withheld a below-floor row.
        response["counts"] =
            json!({"receiver_name_candidates": 0, "unresolved_name_candidates": 1});
        assert!(
            headline_count_disagreements(&response).is_empty(),
            "{:?}",
            headline_count_disagreements(&response)
        );
    }

    #[test]
    fn focal_choice_counts_include_brief_and_omitted_candidates() {
        let mut response = json!({
            "ambiguous_focal": true,
            "candidate_count": 5,
            "candidates": [{"entity_id": "function"}, {"entity_id": "module"}],
            "more_candidates": [{"entity_id": "another"}],
            "omitted_candidates": 2,
        });
        assert!(headline_count_disagreements(&response).is_empty());

        response["candidate_count"] = json!(2);
        let found = headline_count_disagreements(&response);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("candidate_count reads 2 against 5"));
        response["candidate_count"] = json!(5);
        for (field, invalid) in [
            ("candidate_count", Value::Null),
            ("more_candidates", json!(1)),
            ("omitted_candidates", json!(-1)),
            ("omitted_candidates", json!(u64::MAX)),
        ] {
            let mut malformed = response.clone();
            malformed[field] = invalid;
            assert!(
                !headline_count_disagreements(&malformed).is_empty(),
                "malformed {field} must not bypass accounting: {malformed}"
            );
        }
        response["candidates"] = json!([]);
        response["more_candidates"] = json!([]);
        response["omitted_candidates"] = json!(5);
        assert!(headline_count_disagreements(&response).is_empty());
        response["omitted_candidates"] = json!(4);
        assert!(!headline_count_disagreements(&response).is_empty());
    }

    #[test]
    fn reference_counter_checks_take_precedence_over_focal_choice_markers() {
        for reference_fields in [
            json!({"total_upstream": 0}),
            json!({"unconfirmed_candidates": 1}),
            json!({"counts": {"receiver_name_candidates": 1}}),
            json!({"counts": {"unresolved_name_candidates": 1}}),
            json!({"_kin": {"completeness": {"counted": {"withheld_candidates": 1}}}}),
        ] {
            let mut response = json!({
                "ambiguous_focal": true,
                "candidate_count": 2,
                "candidates": [{"entity_id": "first"}, {"entity_id": "second"}],
            });
            response
                .as_object_mut()
                .unwrap()
                .extend(reference_fields.as_object().unwrap().clone());
            let found = headline_count_disagreements(&response);
            assert!(
                found
                    .iter()
                    .any(|message| message.contains("unconfirmed_candidates")),
                "reference accounting must still check its own headline: {response}, {found:?}"
            );
        }
    }

    /// The substrate reading stays raw. `status: "complete"` and a `classes` map
    /// are the evidence the verdict was computed FROM, so an inconclusive
    /// verdict beside them is the intended shape rather than a contradiction.
    /// Scanning them would force the fix to throw the evidence away.
    #[test]
    fn a_raw_substrate_observation_is_not_a_competing_verdict() {
        let mut response = agreeing_response();
        response["_kin"]["verdict"]["state"] = json!(INCONCLUSIVE);
        response["_kin"]["verdict"]["safe_to_conclude_absent"] = json!(false);
        response["negative"] = json!({
            "safe_to_conclude_absent": false,
            "trust": INCONCLUSIVE,
            "advice": "Absence is NOT authoritative: do not conclude the target is unused.",
        });
        response["_kin"]["completeness"] = json!({
            "status": "complete",
            "classes": {"calls": "present", "imports": "absent"},
            "bound": "at_least",
            "counted": {"reported": 2, "exact": false},
            "note": "The counts here are a lower bound.",
        });
        assert!(
            disagreements(&response).is_empty(),
            "{:?}",
            disagreements(&response)
        );
    }

    /// A response no retrieval input spoke about must carry no verdict at all.
    ///
    /// Certification on no evidence is the failure this module would otherwise
    /// introduce. A mutation payload has no absence gate, no edge coverage and
    /// no completeness signal, so an input that answers `certified` for a
    /// concept the payload does not carry would put a certified verdict on every
    /// commit response. Both remaining inputs stay silent instead.
    #[test]
    fn a_non_retrieval_payload_carries_no_verdict() {
        let payload = json!({"committed": true, "change_id": "abc"});
        let envelope = Envelope::daemon();
        assert!(
            Verdict::compute("kin_transaction_commit", &payload, &envelope, None).is_none(),
            "a payload no input spoke about must not be certified"
        );
    }

    /// The other direction: a retrieval payload that genuinely withheld nothing
    /// and reported no degradation still certifies, so the silence above is not
    /// a blanket refusal.
    #[test]
    fn a_clean_retrieval_payload_still_certifies() {
        let payload = json!({
            "references": [{"name": "caller"}],
            "relation_kinds": ["calls"],
            "counts": {"receiver_name_candidates": 0},
            "degradations": [],
            "edge_coverage": {
                "language": "Rust",
                "classes": {"calls": "present"},
                "reference_enrichment": "supported",
            },
        });
        let verdict = Verdict::compute("find_references", &payload, &Envelope::daemon(), None)
            .expect("a retrieval payload carries a verdict");
        assert_eq!(
            verdict.to_value()["state"],
            json!(CERTIFIED),
            "{}",
            verdict.to_value()
        );
    }

    /// A query-vector producer mismatch must reach the single verdict.
    ///
    /// The ruling is that a mismatch leaves the answer usable for ranking hints
    /// and never certified: the vector half is not dropped, but `state` goes
    /// inconclusive and `safe_to_conclude_absent` goes false while the mismatch
    /// stands. That happens through `degradations`, and the fragile part is not
    /// the array being non-empty, it is the label surviving
    /// `describes_the_store_not_the_run`. The corpus-scale arm below is what
    /// separates those two: it is a real degradation entry that must NOT move
    /// the verdict, so if the producer arm refused merely because the array was
    /// populated, that arm would refuse too.
    ///
    /// An authoritative absence object is supplied because
    /// `safe_to_conclude_absent` is `false` on any answer that claims no
    /// absence, whatever its quality. Without it the assertion below would read
    /// false in every arm and prove nothing about the mismatch.
    /// A retrieval arm's scorer that abstained withdraws the verdict and names
    /// the gap, and its report rides under `_kin.verdict.scorer` either way, so
    /// a caller finds the calibrated probability where it reads the verdict.
    #[test]
    fn a_scorer_abstention_is_a_listed_gap_and_its_report_rides_the_verdict() {
        let answer = |scorer: Value| {
            json!({
                "entities": [{"id": "e1", "name": "refill"}],
                "degradations": [],
                "scorer": scorer,
            })
        };
        let compute = |payload: &Value| {
            Verdict::compute("semantic_locate", payload, &Envelope::daemon(), None)
                .expect("a retrieval payload carries a verdict")
                .to_value()
        };

        let confident = compute(&answer(json!({
            "name": "cross-encoder",
            "scored": 40,
            "threshold": 0.5,
            "top_probability": 0.91,
            "abstained": false,
        })));
        assert_ne!(
            confident["limiting_factor"]
                .as_str()
                .unwrap_or_default()
                .contains("none_of_these"),
            true,
            "{confident}"
        );
        assert_eq!(
            confident["inputs"]["scorer"],
            json!(CERTIFIED),
            "{confident}"
        );
        assert_eq!(confident["scorer"]["top_probability"], json!(0.91));

        let abstained = compute(&answer(json!({
            "name": "cross-encoder",
            "scored": 40,
            "threshold": 0.5,
            "top_probability": 0.12,
            "abstained": true,
        })));
        assert_eq!(abstained["state"], json!(INCONCLUSIVE), "{abstained}");
        let factor = abstained["limiting_factor"].as_str().unwrap_or_default();
        assert!(factor.contains("none_of_these"), "{abstained}");
        assert_eq!(abstained["inputs"]["scorer"], json!(INCONCLUSIVE));
        assert_eq!(abstained["scorer"]["abstained"], json!(true));
        assert!(
            CLAUSE_CODES
                .iter()
                .any(|entry| entry.code == "none_of_these"),
            "the code is listed where the docs table is checked against"
        );

        // A scorer that could not run is the ledger's to report, not a second
        // clause here.
        let unavailable = compute(&answer(json!({
            "name": "decision-engine",
            "scored": 40,
            "threshold": 0.5,
            "abstained": false,
            "unavailable": "not_in_build",
        })));
        assert!(
            unavailable["inputs"].get("scorer").is_none(),
            "a scorer that could not run is the ledger's to report: {unavailable}"
        );

        let no_scorer = compute(&json!({"entities": [], "degradations": []}));
        assert!(no_scorer.get("scorer").is_none(), "{no_scorer}");
        assert!(
            no_scorer["inputs"].get("scorer").is_none(),
            "an answer with no scorer lists the inputs it always did: {no_scorer}"
        );
    }

    #[test]
    fn a_query_producer_mismatch_leaves_the_answer_uncertified() {
        let clean = || {
            json!({
                "references": [{"name": "caller"}],
                "relation_kinds": ["calls"],
                "counts": {"receiver_name_candidates": 0},
                "degradations": [],
                "edge_coverage": {
                    "language": "Rust",
                    "classes": {"calls": "present"},
                    "reference_enrichment": "supported",
                },
            })
        };
        let absence = json!({"trust": "authoritative"});
        let compute = |payload: &Value| {
            Verdict::compute(
                "find_references",
                payload,
                &Envelope::daemon(),
                Some(&absence),
            )
            .expect("a retrieval payload carries a verdict")
            .to_value()
        };

        // Control that must certify: nothing degraded.
        let baseline = compute(&clean());
        assert_eq!(baseline["state"], json!(CERTIFIED), "{baseline}");
        assert_eq!(
            baseline["safe_to_conclude_absent"],
            json!(true),
            "the baseline must certify its absence, or the mismatch arm below \
             cannot show that the mismatch is what withdrew it: {baseline}"
        );

        // The reason string comes from the policy that produces it, so a rename
        // there breaks this test instead of quietly decoupling the two.
        let mismatch_reason = kin_core::vector_producer_policy::QueryProducerVerdict::Mismatched {
            query: "metal".to_string(),
            index: "cpu".to_string(),
        }
        .reason()
        .expect("a mismatch names a machine reason");

        let mut mismatched = clean();
        mismatched["degradations"] =
            json!([{"component": "vector_index", "reason": mismatch_reason}]);
        let verdict = compute(&mismatched);
        assert_eq!(verdict["state"], json!(INCONCLUSIVE), "{verdict}");
        assert_eq!(
            verdict["safe_to_conclude_absent"],
            json!(false),
            "{verdict}"
        );
        assert_eq!(
            verdict["inputs"]["degradations"],
            json!(INCONCLUSIVE),
            "{verdict}"
        );
        let factor = verdict["limiting_factor"]
            .as_str()
            .expect("an inconclusive verdict names its factor");
        assert!(
            factor
                .split(CLAUSE_SEPARATOR)
                .any(|code| code == "retrieval_degraded"),
            "the factor must carry the degradation code: {factor}"
        );
        assert!(
            CLAUSE_CODES.iter().any(|entry| {
                entry.code == "retrieval_degraded" && entry.meaning.contains("degradations")
            }) && mismatch_reason.contains("producer"),
            "and the code's written meaning must send the reader to the degradations, where the \
             mismatch {mismatch_reason} itself is recorded"
        );

        // The separating control: query-shape guidance is a real degradation
        // entry that advises how to read the result rather than reporting a
        // lost capability, so it must leave the verdict certified. If this went
        // inconclusive, the arm above would prove only that the array was
        // non-empty. The pair is read from the constants the exemption itself
        // keys on, so a rename there breaks this control rather than silently
        // turning it into a second copy of the arm above.
        let mut corpus = clean();
        corpus["degradations"] = json!([{
            "component": crate::negative::QUERY_SHAPE_COMPONENT,
            "reason": crate::negative::DESCRIPTION_ENTITY_RANKING_REASON,
        }]);
        let scaled = compute(&corpus);
        assert_eq!(scaled["state"], json!(CERTIFIED), "{scaled}");
        assert_eq!(scaled["safe_to_conclude_absent"], json!(true), "{scaled}");
    }

    /// A policy note is disclosed and never refuses; a real degradation beside
    /// it still does, and an entry that does not say it is a note refuses.
    ///
    /// Measured on a Python web framework: every `semantic_locate` reply read
    /// inconclusive on `retrieval_degraded`, and the only entries behind it were
    /// the surface omitting non-entity rows by design, the artifact admit limit,
    /// and the caller's own `include_tests: false` filter. The labels here are
    /// arbitrary on purpose: the reader decides by the entry's `severity`, not
    /// by a list of names.
    #[test]
    fn a_policy_note_is_disclosed_without_refusing_the_verdict() {
        let compute = |degradations: Value| {
            let payload = json!({
                "entities": [{"name": "handler"}],
                "degradations": degradations,
            });
            Verdict::compute("semantic_locate", &payload, &Envelope::daemon(), None)
                .expect("a retrieval payload carries a verdict")
                .to_value()
        };
        let note = |component: &str, reason: &str| json!({"component": component, "reason": reason, "severity": "note"});

        let notes_only = compute(json!([
            note("entity_coverage", "non_entity_candidates_omitted"),
            note("artifact_candidates", "over_admit_limit"),
            note("graph_role_filter", "tests_withheld"),
            note("any_surface", "any_policy"),
        ]));
        assert_eq!(
            notes_only["inputs"]["degradations"],
            json!(CERTIFIED),
            "{notes_only}"
        );

        let mixed = compute(json!([
            note("entity_coverage", "non_entity_candidates_omitted"),
            {"component": "vector_index", "reason": "query_failed"},
        ]));
        assert_eq!(
            mixed["inputs"]["degradations"],
            json!(INCONCLUSIVE),
            "{mixed}"
        );
        assert_eq!(
            mixed["limiting_factor"],
            json!("retrieval_degraded"),
            "{mixed}"
        );
        assert_eq!(
            crate::negative::payload_degradation_labels(&json!({"degradations": [
                note("entity_coverage", "non_entity_candidates_omitted"),
                {"component": "vector_index", "reason": "query_failed"},
            ]})),
            vec!["vector_index:query_failed".to_string()],
            "a note must not be named as a degradation"
        );

        // Old payloads carry no severity, and an unknown word is not a note.
        for severity in [None, Some("warning"), Some("NOTE")] {
            let mut entry = json!({"component": "graph_role_filter", "reason": "tests_withheld"});
            if let Some(severity) = severity {
                entry["severity"] = json!(severity);
            }
            let refused = compute(json!([entry]));
            assert_eq!(
                refused["inputs"]["degradations"],
                json!(INCONCLUSIVE),
                "{severity:?}: {refused}"
            );
        }
    }

    /// FIR-2496. The `edge_coverage` input used to answer `certified` for an
    /// observation that named no class to check, which is every observation a
    /// tool traversing no edge publishes. The shipped v0.5.43 verdict block read
    /// `"inputs": {"edge_coverage": "certified"}` over `"classes": {}` on three
    /// searches, two of which were wrong: `SCHEMA` was a module constant the
    /// Python extractor skips and `build_match_query` sat in a file the graph
    /// had not admitted. Agreement inferred from silence is not agreement.
    #[test]
    fn an_unmeasured_class_map_is_not_an_agreeing_input() {
        let payload = json!({
            "results": [],
            "total_matches": 0,
            "edge_coverage": {
                "scope": "absence_scope",
                "language": "Python",
                "requested_classes": [],
                "classes": {},
                "reference_enrichment": "available",
                "scope_entities": 51,
            },
        });
        let verdict = Verdict::compute("semantic_search", &payload, &Envelope::daemon(), None)
            .expect("a retrieval payload carries a verdict");
        let value = verdict.to_value();
        assert_eq!(value["state"], json!(INCONCLUSIVE), "{value}");
        assert_eq!(
            value["inputs"]["edge_coverage"],
            json!(INCONCLUSIVE),
            "{value}"
        );
        assert!(
            value["limiting_factor"]
                .as_str()
                .expect("an inconclusive verdict names its limiting factor")
                .contains("absence_coverage_unmeasured"),
            "{value}"
        );

        // The control that makes it a reading of the observation rather than of
        // the tool name: one measured class over the same language, same filter,
        // same store, and the same answer certifies.
        let mut measured = payload.clone();
        measured["edge_coverage"]["classes"] = json!({"calls": "present"});
        let verdict = Verdict::compute("semantic_search", &measured, &Envelope::daemon(), None)
            .expect("a retrieval payload carries a verdict");
        assert_eq!(
            verdict.to_value()["inputs"]["edge_coverage"],
            json!(CERTIFIED),
            "{}",
            verdict.to_value()
        );
    }

    /// The other half of the same rule. An answer that resolved no language has
    /// no language's extractor coverage to be missing, and this input must not
    /// invent one: the tools that produce that observation carry their own
    /// sharper reason (an unresolved focal, an empty filtered region), and a
    /// correct refusal beside an unrelated reason is what this module exists to
    /// prevent.
    #[test]
    fn an_observation_that_resolved_no_language_is_not_refused_for_one() {
        let payload = json!({
            "entities": [],
            "relations": [],
            "relation_count": 0,
            "entity_count": 0,
            "edge_coverage": {
                "scope": "absence_scope",
                "language": crate::edge_coverage::NO_RESOLVED_LANGUAGE,
                "requested_classes": [],
                "classes": {},
                "reference_enrichment": "unknown",
            },
        });
        let verdict = Verdict::compute("graph_neighborhood", &payload, &Envelope::daemon(), None)
            .expect("a retrieval payload carries a verdict");
        assert_eq!(
            verdict.to_value()["inputs"]["edge_coverage"],
            json!(CERTIFIED),
            "the coverage input says nothing about an answer with no language: {}",
            verdict.to_value()
        );
    }

    /// A populated reference answer whose every other input is clean, with one
    /// requested class in the state given.
    /// One focal id for the fixture, named once so the payload's `focal_entity`
    /// and the cross-repo anchor cannot drift apart into a response that would
    /// never be produced.
    const FIXTURE_FOCAL_ID: &str = "0195f2a1-0000-7000-8000-00000000f0ca";

    fn populated_reference_payload(imports: &str) -> Value {
        json!({
            "references": [{"name": "index_note"}, {"name": "test_blank_code_masks_fences"}],
            "total_upstream": 2,
            "relation_kinds": ["calls", "imports", "references"],
            "counts": {"receiver_name_candidates": 0},
            "degradations": [],
            // Complete the way a real answer is complete. The producer always
            // writes an anchor beside the roots and always writes `focal_entity`,
            // and the absence gate has always required the anchor to name the
            // focal this query asked about. A fixture carrying roots and no
            // anchor describes a response the handler cannot emit.
            "focal_entity": {"id": FIXTURE_FOCAL_ID},
            "cross_repo": {
                "status": "available",
                "authority_complete": true,
                "authority_revision": "sha256:complete",
                "authority_roots": {"local": "local-root"},
                "authority_anchor": {"repo_id": "local", "entity_id": FIXTURE_FOCAL_ID},
            },
            "focal_resolution": {
                "addressed_by": "entity_id",
                "same_name_candidates": 1,
                "matched": "exact_focal_name",
                "other_candidates": [],
            },
            "edge_coverage": {
                "scope": "language",
                "language": "Python",
                "requested_classes": ["calls", "imports", "references"],
                "classes": {"calls": "present", "imports": imports, "references": "present"},
                "reference_enrichment": "available",
                "budget_exhausted": false,
            },
        })
    }

    fn call_only_reference_payload() -> Value {
        let mut payload = populated_reference_payload("present");
        payload["relation_kinds"] = json!(["calls"]);
        payload["edge_coverage"]["requested_classes"] = json!(["calls"]);
        payload
    }

    /// FIR-2764. A populated answer whose configured spine answered incompletely
    /// is a lower bound, and the verdict has to say so.
    ///
    /// The absence gate already weighs cross-repo authority, and weighs it well,
    /// but only on an answer that claims an absence. A populated answer is
    /// presented as the reference set, so a spine that could not answer means
    /// every cross-repo row is missing from a set nothing marks as partial.
    /// Before this input the verdict read six other blocks, none of which can
    /// see the spine, and certified.
    #[test]
    fn a_populated_answer_over_an_incomplete_spine_is_a_lower_bound() {
        let mut payload = populated_reference_payload("present");
        payload["cross_repo"]["authority_complete"] = json!(false);
        let negative = json!({ "interpretation": "qualified_answer", "trust": "authoritative" });
        let verdict = Verdict::compute(
            "find_references",
            &payload,
            &Envelope::daemon(),
            Some(&negative),
        )
        .expect("a retrieval payload carries a verdict")
        .to_value();

        assert_eq!(
            verdict["state"],
            json!(INCONCLUSIVE),
            "a set presented as whole while the spine could not answer: {verdict}"
        );
        assert_eq!(
            verdict["inputs"]["cross_repo"],
            json!(INCONCLUSIVE),
            "{verdict}"
        );
        let factor = verdict["limiting_factor"].as_str().expect("a factor");
        assert!(
            factor.contains("cross_repo_authority_incomplete"),
            "the factor must name the spine rather than something else: {factor}"
        );
    }

    /// The same shape with an `unavailable` spine that named its condition.
    #[test]
    fn a_populated_answer_over_a_stale_spine_is_a_lower_bound() {
        let mut payload = populated_reference_payload("present");
        payload["cross_repo"] = json!({
            "status": "unavailable",
            "code": "spine_root_stale",
            "reason": "spine root mismatch for repository local",
        });
        let negative = json!({ "interpretation": "qualified_answer", "trust": "authoritative" });
        let verdict = Verdict::compute(
            "find_references",
            &payload,
            &Envelope::daemon(),
            Some(&negative),
        )
        .expect("a retrieval payload carries a verdict")
        .to_value();

        assert_eq!(verdict["state"], json!(INCONCLUSIVE), "{verdict}");
        let factor = verdict["limiting_factor"].as_str().expect("a factor");
        assert!(
            factor.contains("spine_root_stale"),
            "the producer's own code names the condition: {factor}"
        );
    }

    /// The controls that keep this input from becoming a floor under every
    /// answer, which is the regression the absence gate's own scoping comment
    /// warns about. Each of these is an ordinary state of a healthy install, and
    /// each must leave the verdict certified with no factor at all.
    ///
    /// `unavailable` with no code is `kin refs` on every invocation: it resolves
    /// its binding from `KIN_REPO_ID`, which is an override rather than
    /// something an install sets. Weighing it would mark every CLI reference
    /// answer on every machine a lower bound.
    #[test]
    fn the_ordinary_states_of_a_healthy_install_limit_nothing() {
        let mut broken: Vec<String> = Vec::new();
        for (case, cross_repo) in [
            ("no spine configured", json!({ "status": "not_configured" })),
            (
                "a repository the spine has not registered",
                json!({
                    "status": "unavailable",
                    "code": "spine_repo_unregistered",
                    "reason": "repository local has no registered spine authority",
                }),
            ),
            (
                "a caller that never bound to a repository",
                json!({
                    "status": "unavailable",
                    "reason": "KIN_REPO_ID is missing or blank; cross-repo authority cannot \
                               bind this graph to a repository",
                }),
            ),
        ] {
            let mut payload = call_only_reference_payload();
            payload["cross_repo"] = cross_repo;
            let negative =
                json!({ "interpretation": "qualified_answer", "trust": "authoritative" });
            let verdict = Verdict::compute(
                "find_references",
                &payload,
                &Envelope::daemon(),
                Some(&negative),
            )
            .expect("a retrieval payload carries a verdict")
            .to_value();

            // Collected rather than asserted in place. A bare assert aborts the
            // loop on the first case, so a break confined to a later one is
            // invisible behind an earlier one failing first, and the mutation
            // written for it reads as covered while proving nothing.
            if verdict["state"] != json!(CERTIFIED) {
                broken.push(format!(
                    "{case} is a fact about the install, not a limit on the answer: {verdict}"
                ));
            }
            if verdict["limiting_factor"] != Value::Null {
                broken.push(format!("{case} must contribute no clause: {verdict}"));
            }
            if verdict["inputs"]["cross_repo"] != json!(NOT_APPLICABLE) {
                broken.push(format!(
                    "{case} must read silent rather than certifying: {verdict}"
                ));
            }
        }
        assert!(
            broken.is_empty(),
            "{} of the ordinary install states limited an answer:\n{}",
            broken.len(),
            broken.join("\n")
        );
    }

    /// A complete spine certifies rather than staying silent, so the input
    /// contributes agreement on the healthy path instead of merely not refusing.
    #[test]
    fn a_complete_spine_certifies_as_its_own_named_input() {
        let payload = call_only_reference_payload();
        let negative = json!({ "interpretation": "qualified_answer", "trust": "authoritative" });
        let verdict = Verdict::compute(
            "find_references",
            &payload,
            &Envelope::daemon(),
            Some(&negative),
        )
        .expect("a retrieval payload carries a verdict")
        .to_value();

        assert_eq!(verdict["state"], json!(CERTIFIED), "{verdict}");
        assert_eq!(
            verdict["inputs"]["cross_repo"],
            json!(CERTIFIED),
            "{verdict}"
        );
        assert_eq!(verdict["limiting_factor"], Value::Null, "{verdict}");
    }

    /// The same gap seen by two readings is named once.
    ///
    /// On an answer that claims an absence, the gate puts the cross-repo clause
    /// in `trust_reason` and the absence-gate reading splits it back out, while
    /// this input builds the same clause directly. Both speak, and the composer
    /// dedupes by label, so the reader gets one clause rather than the same
    /// sentence twice. That is the FIR-2672 rule this input has to obey rather
    /// than re-break.
    #[test]
    fn one_gap_seen_by_two_readings_is_named_once() {
        let mut payload = populated_reference_payload("present");
        payload["cross_repo"]["authority_complete"] = json!(false);
        let negative = json!({
            "interpretation": "absence_claim",
            "trust": "inconclusive",
            "trust_reason": "cross_repo_authority_incomplete: spine topology or requested \
                             relation subtype is incomplete at revision sha256:complete",
        });
        let verdict = Verdict::compute(
            "find_references",
            &payload,
            &Envelope::daemon(),
            Some(&negative),
        )
        .expect("a retrieval payload carries a verdict")
        .to_value();

        let factor = verdict["limiting_factor"].as_str().expect("a factor");
        assert_eq!(
            factor.matches("cross_repo_authority_incomplete").count(),
            1,
            "the gap is one gap however many readings noticed it: {factor}"
        );
    }

    /// The other tool that carries a cross-repo block reaches the same input.
    ///
    /// The dispatch names two tools, and the absence gate names the same two, so
    /// the strings are written twice in one file. A rename that moved one and not
    /// the other would leave this arm matching nothing, and nothing about a
    /// find_references test can see that: the verdict would simply stop weighing
    /// cross-repo authority on bulk answers and no assertion anywhere would go
    /// red.
    ///
    /// So the arm gets its own case, with the healthy direction beside the
    /// refusing one. A dispatch that stopped matching would certify both.
    #[test]
    fn the_bulk_tool_reaches_the_same_cross_repo_input() {
        let complete = json!({
            "status": "available",
            "authority_complete": true,
            "authority_revision": "sha256:complete",
            "authority_roots": {"local": "local-root"},
            "relation_subtype_complete": true,
            "verdicts_complete": true,
        });
        let mut incomplete = complete.clone();
        incomplete["authority_complete"] = json!(false);

        for (case, cross_repo, expected_state, expected_input) in [
            ("a complete bulk authority", complete, CERTIFIED, CERTIFIED),
            (
                "a bulk authority that answered incompletely",
                incomplete,
                INCONCLUSIVE,
                INCONCLUSIVE,
            ),
        ] {
            let payload = json!({
                "results": [{"name": "index_note", "has_references": true}],
                "degradations": [],
                "cross_repo": cross_repo,
            });
            let negative =
                json!({ "interpretation": "qualified_answer", "trust": "authoritative" });
            let verdict = Verdict::compute(
                "bulk_check_references",
                &payload,
                &Envelope::daemon(),
                Some(&negative),
            )
            .expect("a retrieval payload carries a verdict")
            .to_value();

            assert_eq!(
                verdict["inputs"]["cross_repo"],
                json!(expected_input),
                "{case}: the bulk dispatch arm must be reached: {verdict}"
            );
            assert_eq!(verdict["state"], json!(expected_state), "{case}: {verdict}");
        }
    }

    /// FIR-2672, the sole-cause case. Every input is clean except one requested
    /// class the answer could not read, and that alone makes the verdict
    /// inconclusive and names the class, for each of the three states a class
    /// can be short in. The shipped 0.5.52 verdict certified this exact shape
    /// with `imports: absent`, because the reading weighed `calls` alone.
    #[test]
    fn a_requested_class_the_answer_could_not_read_is_the_sole_cause_of_an_inconclusive_verdict() {
        for (state, leading) in [
            ("absent", "cross_file_edges_absent"),
            ("unproduced", "cross_file_edges_unproduced"),
            ("unknown", "edge_coverage_unknown"),
        ] {
            let verdict = Verdict::compute(
                "find_references",
                &populated_reference_payload(state),
                &Envelope::daemon(),
                None,
            )
            .expect("a retrieval payload carries a verdict")
            .to_value();
            assert_eq!(verdict["state"], json!(INCONCLUSIVE), "{state}: {verdict}");
            assert_eq!(
                verdict["inputs"]["edge_coverage"],
                json!(INCONCLUSIVE),
                "{state}: the coverage input itself refuses: {verdict}"
            );
            let factor = verdict["limiting_factor"]
                .as_str()
                .expect("an inconclusive verdict names its limiting factor");
            assert_eq!(
                factor.split(CLAUSE_SEPARATOR).next(),
                Some(leading),
                "{state}: the factor leads with the class's own state: {factor}"
            );
            assert_eq!(
                populated_reference_payload(state)["edge_coverage"]["classes"]["imports"],
                json!(state),
                "{state}: and the class is named by the block the code points at"
            );
            for input in ["withheld_candidates", "degradations"] {
                assert_ne!(
                    verdict["inputs"][input],
                    json!(INCONCLUSIVE),
                    "{state}: {input} was clean and must not be what refused: {verdict}"
                );
            }
        }
    }

    /// A present class proves that edges exist, not that every requested
    /// non-call site was settled. Keep that gap distinct from class presence.
    #[test]
    fn present_edge_classes_do_not_establish_non_call_completion() {
        let verdict = Verdict::compute(
            "find_references",
            &populated_reference_payload("present"),
            &Envelope::daemon(),
            None,
        )
        .expect("a retrieval payload carries a verdict")
        .to_value();
        assert_eq!(verdict["state"], json!(INCONCLUSIVE), "{verdict}");
        assert_eq!(
            verdict["limiting_factor"], "reference_completion_unmeasured",
            "{verdict}"
        );
        assert_eq!(verdict["inputs"]["edge_coverage"], json!(CERTIFIED));
        assert_eq!(verdict["inputs"]["absence_gate"], json!(CERTIFIED));
    }

    /// A reference payload over an escape census that took the mention
    /// reading `mentions`, with `unkeyed` lookups counted beside it.
    fn mention_census_payload(kinds: Value, mentions: Value, unkeyed: u64) -> Value {
        let mut payload = populated_reference_payload("present");
        payload["relation_kinds"] = kinds.clone();
        payload["edge_coverage"]["requested_classes"] = kinds;
        let mut residue = json!({ "mentions": mentions });
        let mut clauses = Vec::new();
        if unkeyed > 0 {
            residue["unkeyed_lookups"] = json!({"count": unkeyed, "sites": []});
            clauses.push(format!(
                "unkeyed_lookups_counted: {unkeyed} lookups by a name computed at run time"
            ));
        }
        payload["call_sites"] = json!({
            "settled": clauses.is_empty(),
            "clauses": clauses,
            "focal_escape": {"escape": "contained", "entities_checked": 1767, "residue": residue},
        });
        payload
    }

    fn default_kinds() -> Value {
        json!(["calls", "imports", "references"])
    }

    #[test]
    fn impact_zero_consumers_need_their_own_non_call_census() {
        let measured = mention_census_payload(
            default_kinds(),
            json!({"measured": true, "count": 0, "matched": {}}),
            0,
        );
        let mut first = measured["call_sites"].clone();
        first["entity_id"] = json!("first");
        let mut impact = json!({
            "entity_impacts": [{"entity_id": "first", "consumer_count": 0}],
            "call_sites": first,
        });
        assert!(impact_consumer_absence_gaps(&impact).is_empty());

        impact["call_sites"]["focal_escape"]["residue"]["mentions"]["measured"] = json!(false);
        let gaps = impact_consumer_absence_gaps(&impact);
        assert_eq!(gaps.len(), 1);
        assert!(gaps[0].starts_with("reference_completion_unmeasured:"));
        assert!(gaps[0].contains("first"));
        assert!(Verdict::pre_negative_gaps(&impact).contains(&gaps[0]));
        let verdict = Verdict::compute("impact_analysis", &impact, &Envelope::daemon(), None)
            .unwrap()
            .to_value();
        assert_eq!(verdict["state"], INCONCLUSIVE);
        assert_eq!(verdict["safe_to_conclude_absent"], false);
        assert_eq!(verdict["inputs"]["impact_consumer_absence"], INCONCLUSIVE);

        // A complete first focal must not certify an unmeasured second zero.
        impact["call_sites"] = json!({"settled": true, "clauses": [], "focals": [first]});
        impact["entity_impacts"] = json!([
            {"entity_id": "first", "consumer_count": 0},
            {"entity_id": "second", "consumer_count": 0},
        ]);
        let gaps = impact_consumer_absence_gaps(&impact);
        assert_eq!(gaps.len(), 1, "{gaps:?}");
        assert!(gaps[0].contains("second"));

        // Positive direct callers remain independently gradeable.
        impact["entity_impacts"] = json!([{"entity_id": "second", "consumer_count": 1}]);
        assert!(impact_consumer_absence_gaps(&impact).is_empty());
    }

    #[test]
    fn impact_zero_with_unmatched_mentions_remains_qualified() {
        let mut census = mention_census_payload(
            default_kinds(),
            json!({"measured": true, "count": 1, "matched": {},
            "unmatched": {"count": 1, "sites": [
                {"file": "consumer.py", "line": 4, "kind": "annotation"}
            ]}}),
            0,
        )["call_sites"]
            .clone();
        census["entity_id"] = json!("target");
        let impact = json!({
            "entity_impacts": [{"entity_id": "target", "consumer_count": 0}],
            "call_sites": census,
        });
        let gaps = impact_consumer_absence_gaps(&impact);
        assert_eq!(gaps.len(), 1);
        assert!(gaps[0].starts_with("reference_mentions_unmatched:"));
        assert!(gaps[0].contains("consumer.py:4"));
        let verdict = Verdict::compute("impact_analysis", &impact, &Envelope::daemon(), None)
            .unwrap()
            .to_value();
        assert_eq!(verdict["safe_to_conclude_absent"], false);
    }

    /// The census matched every non-call read that could read the focal
    /// against a stored row of a requested kind: non-call completion holds,
    /// and an absence over it is authoritative.
    #[test]
    fn a_measured_mention_reading_with_nothing_unmatched_certifies_non_call_completion() {
        let mentions =
            json!({"measured": true, "count": 2, "matched": {"references": 1, "imports": 1}});
        let payload = mention_census_payload(default_kinds(), mentions, 0);
        assert!(Verdict::pre_negative_gaps(&payload).is_empty());
        let verdict = Verdict::compute("find_references", &payload, &Envelope::daemon(), None)
            .expect("a retrieval payload carries a verdict")
            .to_value();
        assert_eq!(verdict["state"], json!(CERTIFIED), "{verdict}");
        assert_eq!(
            verdict["inputs"]["reference_completion"],
            json!(CERTIFIED),
            "{verdict}"
        );

        // `_make_timedelta`: nothing reads it, so the empty answer is an
        // authoritative absence.
        let mut empty = mention_census_payload(
            default_kinds(),
            json!({"measured": true, "count": 0, "matched": {}}),
            0,
        );
        empty["references"] = json!([]);
        empty["total_upstream"] = json!(0);
        let result = crate::finalize_with_envelope(
            crate::types::ToolCallResult::text(empty.to_string()),
            Envelope::daemon().with_health(&json!({
                "initialized": true, "graph_loaded": true, "graph_generation": 12,
            })),
            "find_references",
        );
        let crate::types::ContentBlock::Text { text } = &result.content[0];
        let response: Value = serde_json::from_str(text).unwrap();
        let verdict = &response["_kin"]["verdict"];
        assert!(
            !verdict["limiting_factor"].as_str().is_some_and(|factor| {
                factor.contains("reference_completion_unmeasured")
                    || factor.contains("reference_mentions_unmatched")
            }),
            "{response}"
        );
        assert_eq!(verdict["state"], json!(CERTIFIED), "{response}");
        assert_eq!(verdict["safe_to_conclude_absent"], true, "{response}");
    }

    /// A focal with members is referenced through them without its name
    /// being spelled, so a measured reading still certifies its imports and
    /// never its references.
    #[test]
    fn a_reading_over_a_focal_with_members_certifies_imports_and_not_references() {
        let mentions = json!({"measured": true, "count": 1, "matched": {"imports": 1},
            "references_unmeasured": "the focal holds members"});
        let imports = mention_census_payload(json!(["imports"]), mentions.clone(), 0);
        assert!(reference_completion_gap(&imports).is_none());
        for kinds in [json!(["references"]), json!(["imports", "references"])] {
            let payload = mention_census_payload(kinds, mentions.clone(), 0);
            assert!(reference_completion_gap(&payload)
                .is_some_and(|gap| gap.starts_with("reference_completion_unmeasured")));
        }
    }

    /// A read no stored row carries keeps a gap of its own, which names the
    /// file and line rather than calling all of non-call completion
    /// unmeasured, and is a listed residue.
    #[test]
    fn an_unmatched_mention_keeps_a_gap_that_names_its_file_and_line() {
        let mentions = json!({
            "measured": true,
            "count": 3,
            "matched": {"references": 2},
            "unmatched": {"count": 1, "sites": [
                {"file": "src/flask/cli.py", "line": 42, "kind": "comparison", "name": "find_best_app"},
            ]},
        });
        let payload = mention_census_payload(default_kinds(), mentions, 0);
        let gaps = Verdict::pre_negative_gaps(&payload);
        assert_eq!(gaps.len(), 1, "{gaps:?}");
        assert!(
            gaps[0].starts_with("reference_mentions_unmatched: 1 non-call reads"),
            "{gaps:?}"
        );
        assert!(
            gaps[0].contains("src/flask/cli.py:42 (comparison)"),
            "{gaps:?}"
        );
        assert!(!gaps[0].contains(CLAUSE_SEPARATOR), "{gaps:?}");
        let verdict = Verdict::compute("find_references", &payload, &Envelope::daemon(), None)
            .unwrap()
            .to_value();
        assert_eq!(
            verdict["limiting_factor"], "reference_mentions_unmatched",
            "{verdict}"
        );
        assert_eq!(verdict["state"], json!(CERTIFIED_EXCEPT), "{verdict}");
        assert_eq!(verdict["safe_to_conclude_absent"], false, "{verdict}");
    }

    /// Rows of a kind the query did not ask for are not in its answer.
    #[test]
    fn a_read_carried_only_by_an_unrequested_kind_is_unmatched_for_that_query() {
        let mentions =
            json!({"measured": true, "count": 2, "matched": {"references": 1, "imports": 1}});
        let payload = mention_census_payload(json!(["references"]), mentions.clone(), 0);
        let gaps = Verdict::pre_negative_gaps(&payload);
        assert_eq!(gaps.len(), 1, "{gaps:?}");
        assert!(
            gaps[0].starts_with("reference_mentions_unmatched:"),
            "{gaps:?}"
        );
        assert!(gaps[0].contains("carried only by imports rows"), "{gaps:?}");
        let both = mention_census_payload(json!(["imports", "References"]), mentions, 0);
        assert!(Verdict::pre_negative_gaps(&both).is_empty());
    }

    /// A store whose records predate the mention reading, or a census that
    /// took none, keeps `reference_completion_unmeasured`, as before.
    #[test]
    fn records_that_predate_mentions_keep_reference_completion_unmeasured() {
        let unlisted = mention_census_payload(
            default_kinds(),
            json!({"measured": false, "reason": "a source unit of the domain has a value-occurrence record that lists no mentions"}),
            0,
        );
        let mut unknown = populated_reference_payload("present");
        unknown["call_sites"] = json!({
            "settled": true,
            "clauses": [],
            "focal_escape": {"escape": "unknown", "reason": "the selected tree could not be read"},
        });
        let mut untaken = mention_census_payload(default_kinds(), json!({}), 0);
        untaken["call_sites"]["focal_escape"]["residue"] = json!({});
        for payload in [unlisted, unknown, untaken] {
            let gaps = Verdict::pre_negative_gaps(&payload);
            assert_eq!(gaps.len(), 1, "{gaps:?}");
            assert_eq!(
                gaps,
                vec!["reference_completion_unmeasured: non-call completion is unmeasured"],
                "word for word what it was"
            );
            let verdict = Verdict::compute("find_references", &payload, &Envelope::daemon(), None)
                .unwrap()
                .to_value();
            assert_eq!(
                verdict["limiting_factor"], "reference_completion_unmeasured",
                "{verdict}"
            );
            assert_eq!(verdict["state"], json!(INCONCLUSIVE), "{verdict}");
        }
    }

    /// `SessionInterface.open_session`: lookups by a computed name could
    /// reach it, which bounds its non-call reads exactly as it bounds its
    /// calls, under the call census's own code.
    #[test]
    fn an_unkeyed_lookup_bounds_non_call_completion_under_its_own_code() {
        let mentions = json!({"measured": true, "count": 2, "matched": {"references": 2}});
        let payload = mention_census_payload(default_kinds(), mentions, 14);
        let verdict = Verdict::compute("find_references", &payload, &Envelope::daemon(), None)
            .unwrap()
            .to_value();
        assert_eq!(
            verdict["limiting_factor"], "unkeyed_lookups_counted",
            "{verdict}"
        );
        assert_eq!(verdict["state"], json!(CERTIFIED_EXCEPT), "{verdict}");
        assert_eq!(
            verdict["inputs"]["reference_completion"],
            json!(INCONCLUSIVE)
        );
        let gaps = Verdict::pre_negative_gaps(&payload);
        assert_eq!(
            gaps.iter()
                .filter(|gap| gap.starts_with("unkeyed_lookups_counted"))
                .count(),
            1,
            "the call census and the reference reading name one gap once: {gaps:?}"
        );
    }

    /// The budget path downgrades every verdict surface together. Leaving
    /// `negative` or `_kin.verdict` certifying an answer whose rows were removed
    /// is the same defect arriving through the one path that removes answers on
    /// purpose.
    /// An input this function never heard of still limits the stamp.
    ///
    /// The list used to be four names written out, which is "every input except
    /// `edge_coverage`" in a five-input world and silently wrong in a six-input
    /// one. A reading added later would not appear, and the failure is the
    /// quiet kind: a missing name yields a SHORTER list, never an error, so the
    /// block goes on presenting itself as licensing on its own beside an input
    /// that refuses.
    ///
    /// This test is shaped like the mutation that catches it. The name is
    /// deliberately one no code in this file mentions, so it cannot pass by
    /// being enumerated somewhere; the hardcoded version fails it, and any
    /// future hardcoded version will fail it too. The pair of assertions is the
    /// point: an unknown input that REFUSES must appear, and an unknown input
    /// that certifies must not, or a function returning every input it sees
    /// would pass the first assertion alone.
    #[test]
    fn an_input_this_function_has_never_heard_of_still_limits_the_stamp() {
        let build = |unknown_state: &str| {
            let mut inputs = Map::new();
            inputs.insert("edge_coverage".to_string(), json!(CERTIFIED));
            inputs.insert("completeness".to_string(), json!(CERTIFIED));
            inputs.insert("graph_freshness".to_string(), json!(unknown_state));
            Verdict {
                scorer: None,
                certified: false,
                except_residue: false,
                absence_claim: AbsenceClaim::NotAuthoritative,
                limiting_factor: Some("graph_freshness: the store is stale".to_string()),
                inputs,
            }
        };

        let limits = build(INCONCLUSIVE).edge_coverage_limits();
        assert!(
            limits.contains(&"graph_freshness:inconclusive".to_string()),
            "an input added after this function was written must still limit the block: {limits:?}"
        );

        assert!(
            build(CERTIFIED).edge_coverage_limits().is_empty(),
            "an unknown input that certifies limits nothing, or the function is just \
             listing its inputs"
        );
    }

    /// `edge_coverage` never limits itself, whatever the derivation does.
    ///
    /// The old hardcoded list excluded it by not mentioning it. Deriving from
    /// the map means the exclusion is now a filter clause, which is a line
    /// someone can delete, so it gets an assertion rather than a comment.
    #[test]
    fn the_edge_coverage_input_is_never_one_of_its_own_limits() {
        let mut inputs = Map::new();
        inputs.insert("edge_coverage".to_string(), json!(CERTIFIED));
        inputs.insert("completeness".to_string(), json!(INCONCLUSIVE));
        let verdict = Verdict {
            scorer: None,
            certified: false,
            except_residue: false,
            absence_claim: AbsenceClaim::NotAuthoritative,
            limiting_factor: Some("completeness: unknown".to_string()),
            inputs,
        };

        let limits = verdict.edge_coverage_limits();
        assert_eq!(limits, vec!["completeness:inconclusive".to_string()]);
        assert!(
            !limits
                .iter()
                .any(|limit| limit.starts_with("edge_coverage")),
            "the block cannot cite itself as the reason it does not license: {limits:?}"
        );
    }

    /// A CERTIFIED verdict over a completeness that refuses is a contradiction,
    /// and until now nothing looked for it.
    ///
    /// The five arms that existed all read one direction: a surface claiming
    /// certification under an inconclusive verdict. None read the reverse, so an
    /// envelope whose verdict certified while its own completeness reported
    /// `status: unknown` or `bound: at_least` graded clean.
    ///
    /// This is the only shape that can falsify the arm. Deleting a detector
    /// cannot make anything go red, because a detector's absence is silence, so
    /// the proof has to give it something to detect and confirm it speaks. The
    /// end-to-end suite cannot do it either: `disagreements` is reached from a
    /// `debug_assert!`, which a release build compiles out.
    #[test]
    fn a_certified_verdict_over_a_refusing_completeness_is_a_disagreement() {
        let mut response = agreeing_response();
        // Everything else still agrees; only completeness refuses.
        response["_kin"]["completeness"]["status"] = json!("unknown");
        response["_kin"]["completeness"]["bound"] = json!("at_least");

        let found = disagreements(&response);
        assert!(
            found
                .iter()
                .any(|line| line.contains("bound reads at_least under a certified")),
            "a certified verdict over an at_least bound must be reported: {found:?}"
        );
        assert!(
            found
                .iter()
                .any(|line| line.contains("status reads unknown under a certified")),
            "a certified verdict over an unknown status must be reported: {found:?}"
        );
    }

    /// The same arm on the negative block, and the control beside it.
    ///
    /// The control is the half that matters: an untouched agreeing response must
    /// report NOTHING. Without it this pair would pass just as happily if the
    /// arm reported a disagreement on every response it was handed, which is a
    /// detector that fires always and is no more useful than one that never
    /// fires.
    #[test]
    fn a_certified_verdict_over_a_refusing_negative_is_a_disagreement() {
        let mut response = agreeing_response();
        response["negative"]["trust"] = json!("inconclusive");
        response["negative"]["safe_to_conclude_absent"] = json!(true);

        let found = disagreements(&response);
        assert!(
            found
                .iter()
                .any(|line| line.contains("negative.trust refuses under a certified")),
            "a certified verdict over a refusing negative must be reported: {found:?}"
        );

        assert!(
            disagreements(&agreeing_response()).is_empty(),
            "control: an agreeing response must report no disagreement at all"
        );
    }

    #[test]
    fn a_budget_cut_downgrades_the_verdict_and_the_absence_together() {
        let mut response = agreeing_response();
        response["negative"]["trust_reason"] = json!("structural_authoritative");
        mark_response_bounded(&mut response);
        mark_response_bounded(&mut response);

        assert_eq!(response["_kin"]["verdict"]["state"], json!(INCONCLUSIVE));
        assert_eq!(
            response["_kin"]["verdict"]["safe_to_conclude_absent"],
            json!(false)
        );
        assert_eq!(
            response["negative"]["safe_to_conclude_absent"],
            json!(false)
        );
        assert_eq!(response["negative"]["trust"], json!(INCONCLUSIVE));
        assert!(response["negative"]["trust_reason"]
            .as_str()
            .unwrap()
            .starts_with("response_bounded"));
        assert!(response["negative"]["advice"]
            .as_str()
            .unwrap()
            .contains("Limiting factor: response_bounded"));
        assert_eq!(
            response["_kin"]["verdict"]["limiting_factor"]
                .as_str()
                .unwrap()
                .split(CLAUSE_SEPARATOR)
                .filter(|code| *code == "response_bounded")
                .count(),
            1,
            "the convergence loop may reapply the downgrade without repeating its factor"
        );
        // The absence object keeps its sentences until PR 2 moves negative.rs, so
        // its reason is still `label: text` clauses and is counted as such.
        assert_eq!(
            response["negative"]["trust_reason"]
                .as_str()
                .unwrap()
                .matches("response_bounded:")
                .count(),
            1,
            "the absence reason is idempotent too"
        );
    }

    /// This is the shape the outside-graph input refuses. A `semantic_locate` for "where
    /// `router.param` callbacks are registered and stored, and how a request is
    /// dispatched through the middleware stack" on express 5.2.1 came back
    /// `state: "certified"`, `limiting_factor: null`, `bound: "exact"`,
    /// `status: "complete"`. Express's router is the external `router` package,
    /// which that graph never admitted, so every word of the verdict was true
    /// about the graph and silent about the question.
    ///
    /// The block is the evidence and this is the verdict read off it: one
    /// inconclusive input is enough, and the module is named in the sentence a
    /// reader acts on rather than left to be inferred from a count.
    #[test]
    fn a_question_naming_an_unadmitted_package_is_never_certified() {
        let mut payload = populated_reference_payload("present");
        payload["outside_graph"] = json!({
            "asked_about": ["router"],
            "symbols": [{ "symbol": "Router", "modules": ["router"] }],
            "targets_examined": 19,
            "budget_exhausted": false,
        });
        let verdict = Verdict::compute(
            "semantic_locate",
            &payload,
            &Envelope::daemon(),
            Some(&json!({ "interpretation": "qualified_answer" })),
        )
        .expect("a retrieval payload carries a verdict");
        let rendered = verdict.to_value();

        assert_eq!(
            rendered["state"],
            json!(INCONCLUSIVE),
            "a question whose answer may live outside this graph cannot be certified: {rendered}"
        );
        assert_eq!(
            rendered["inputs"]["outside_graph"],
            json!(INCONCLUSIVE),
            "the new input is the one that refused: {rendered}"
        );
        let factor = rendered["limiting_factor"]
            .as_str()
            .expect("an inconclusive verdict names its limiting factor");
        assert!(
            factor.contains("dependency_outside_graph"),
            "the clause carries its own label: {factor}"
        );
        assert_eq!(
            payload["outside_graph"]["symbols"],
            json!([{ "symbol": "Router", "modules": ["router"] }]),
            "and the block the code points at names the module, which is what a reader acts on"
        );

        // The same verdict takes `complete` and `exact` off the completeness
        // block, so the two halves of the response cannot disagree.
        let mut completeness = Some(crate::envelope::Completeness {
            status: "complete".to_string(),
            bound: "exact".to_string(),
            substrate: "edges".to_string(),
            classes: Map::new(),
            decided_by: Vec::new(),
            counted: Some(json!({ "exact": true })),
            reference_resolution: None,
            limits: Vec::new(),
        });
        verdict.project_onto_completeness(&mut completeness);
        let completeness = completeness.expect("the block survives projection");
        assert_ne!(
            completeness.status, "complete",
            "a response inconclusive about the question may not call its counts whole"
        );
        assert_eq!(completeness.bound, "at_least");
    }

    /// The control the case above is worth nothing without: the same question on
    /// the same tool, with no unadmitted module named, still certifies. A gate
    /// that answers "uncertain" to everything has stopped being a gate.
    #[test]
    fn a_question_naming_nothing_outside_the_graph_still_certifies() {
        let payload = populated_reference_payload("present");
        assert!(
            payload.get("outside_graph").is_none(),
            "the control carries no block, which is the ordinary case"
        );
        let verdict = Verdict::compute(
            "semantic_locate",
            &payload,
            &Envelope::daemon(),
            Some(&json!({ "interpretation": "qualified_answer" })),
        )
        .expect("a retrieval payload carries a verdict")
        .to_value();
        assert_eq!(
            verdict["state"],
            json!(CERTIFIED),
            "the verdict must keep its ability to say yes: {verdict}"
        );
        assert_eq!(
            verdict["inputs"]["outside_graph"],
            json!(NOT_APPLICABLE),
            "an input with nothing to say stays silent rather than certifying: {verdict}"
        );
    }

    /// The `call_sites` block is a named input of its own. While a site in the
    /// block's scope is owed, unresolved, server-failed, not in any build, a
    /// binding or proven under a stale proof context, the answer is
    /// inconclusive under that state's code; a settled scope certifies; and an
    /// answer that carries no block is not qualified by one.
    mod call_sites {
        use super::*;
        use kin_model::{
            CallSite, CallSiteLedger, CallSiteState, CallSiteTally, CallerSites, EntityId,
            ExternalReferenceId, Hash256, ResolutionRecordId, ServerFailure, UnresolvedReason,
        };

        fn ledger_of(states: Vec<CallSiteState>) -> CallSiteLedger {
            CallSiteLedger {
                caller: EntityId(uuid::Uuid::from_u128(1)),
                behavior_hash: Hash256::from_bytes([0; 32]),
                body_hash: Hash256::from_bytes([0; 32]),
                context: ResolutionRecordId(uuid::Uuid::from_u128(2)),
                census: states.len() as u32,
                sites: states
                    .into_iter()
                    .enumerate()
                    .map(|(at, state)| CallSite {
                        offset: at as u32 * 4,
                        length: 3,
                        state,
                    })
                    .collect(),
            }
        }

        fn tally_of(reading: CallerSites) -> CallSiteTally {
            let mut tally = CallSiteTally::default();
            tally.add(&reading);
            tally
        }

        fn current(states: Vec<CallSiteState>) -> CallSiteTally {
            tally_of(CallerSites::Current(ledger_of(states)))
        }

        fn settled() -> CallSiteTally {
            current(vec![
                CallSiteState::ProvenTarget {
                    target: EntityId(uuid::Uuid::from_u128(3)),
                },
                CallSiteState::ProvenExternal {
                    target: ExternalReferenceId(uuid::Uuid::from_u128(4)),
                },
                CallSiteState::ProvenOutside,
            ])
        }

        /// Every unsettled kind, each with the one code it calls for.
        fn unsettled() -> Vec<(CallSiteTally, &'static str)> {
            vec![
                (
                    current(vec![CallSiteState::Unresolved {
                        reason: UnresolvedReason::NoAnswer,
                    }]),
                    "call_sites_unresolved",
                ),
                (
                    current(vec![CallSiteState::ServerFailed {
                        reason: ServerFailure::Timeout,
                    }]),
                    "call_sites_server_failed",
                ),
                (
                    current(vec![CallSiteState::NotInBuild {
                        reason: "no target compiles this file".to_string(),
                    }]),
                    "call_sites_not_in_build",
                ),
                (
                    current(vec![CallSiteState::Binding { may_call: None }]),
                    "binding_unproven",
                ),
                (
                    tally_of(CallerSites::Stale(ledger_of(vec![
                        CallSiteState::ProvenOutside,
                    ]))),
                    "proof_context_stale",
                ),
                (
                    tally_of(CallerSites::Unverified {
                        ledger: ledger_of(vec![CallSiteState::ProvenOutside]),
                        reason: "resolver failed; validation never completed".into(),
                    }),
                    "proof_context_unverified",
                ),
                (tally_of(CallerSites::OwedEnrichment), "call_sites_owed"),
                (tally_of(CallerSites::OwedDerivation), "call_sites_owed"),
                (
                    tally_of(CallerSites::NoResolver {
                        language: kin_model::LanguageId::Python,
                        why: kin_model::NoResolver::EnrichmentOff,
                    }),
                    "call_sites_unproven_no_resolver",
                ),
                (
                    tally_of(CallerSites::NoResolver {
                        language: kin_model::LanguageId::Go,
                        why: kin_model::NoResolver::ServerCannotStart {
                            reason: "gopls: no Go toolchain; install Go".to_string(),
                        },
                    }),
                    "call_sites_unproven_no_resolver",
                ),
            ]
        }

        fn verdict_over(block: Option<Value>) -> Value {
            let mut payload = call_only_reference_payload();
            if let Some(block) = block {
                payload[crate::call_sites::CALL_SITES_KEY] = block;
            }
            Verdict::compute("find_references", &payload, &Envelope::daemon(), None)
                .expect("a retrieval payload carries a verdict")
                .to_value()
        }

        #[test]
        fn each_unsettled_state_makes_the_answer_inconclusive_under_its_own_code() {
            for (tally, code) in unsettled() {
                let block = crate::call_sites::block_json(&tally, "the focal's own body");
                assert_eq!(block["settled"], json!(false), "{code}: {block}");
                let verdict = verdict_over(Some(block));
                assert_eq!(verdict["state"], json!(INCONCLUSIVE), "{code}: {verdict}");
                assert_eq!(
                    verdict["inputs"]["call_sites"],
                    json!(INCONCLUSIVE),
                    "{code}: the block is the input that refused: {verdict}"
                );
                assert_eq!(
                    verdict["limiting_factor"],
                    json!(code),
                    "{code}: the factor is the state's own code and nothing else: {verdict}"
                );
                assert!(
                    CLAUSE_CODES.iter().any(|entry| entry.code == code),
                    "{code} is a listed code"
                );
            }
        }

        #[test]
        fn a_settled_block_certifies_and_an_answer_without_one_is_not_qualified_by_it() {
            let block = crate::call_sites::block_json(&settled(), "the focal's own body");
            assert_eq!(block["settled"], json!(true), "{block}");
            assert_eq!(block["clauses"], json!([]), "{block}");
            let verdict = verdict_over(Some(block));
            assert_eq!(verdict["state"], json!(CERTIFIED), "{verdict}");
            assert_eq!(
                verdict["inputs"]["call_sites"],
                json!(CERTIFIED),
                "{verdict}"
            );

            let verdict = verdict_over(None);
            assert_eq!(verdict["state"], json!(CERTIFIED), "{verdict}");
            assert_eq!(
                verdict["inputs"]["call_sites"],
                json!(NOT_APPLICABLE),
                "an answer with no block has nothing to say about its sites: {verdict}"
            );
        }

        #[test]
        fn two_unsettled_states_are_both_named_and_no_clause_carries_the_separator() {
            let mut tally = current(vec![
                CallSiteState::Binding { may_call: None },
                CallSiteState::Unresolved {
                    reason: UnresolvedReason::NoAnswer,
                },
            ]);
            tally.add(&CallerSites::OwedEnrichment);
            let block = crate::call_sites::block_json(&tally, crate::call_sites::FAMILY_SCOPE);
            for clause in block["clauses"].as_array().expect("clauses") {
                let clause = clause.as_str().expect("a clause is a string");
                assert!(
                    !clause.contains(CLAUSE_SEPARATOR),
                    "a clause carries the separator: {clause}"
                );
            }
            let verdict = verdict_over(Some(block));
            assert_eq!(
                verdict["limiting_factor"],
                json!("binding_unproven; call_sites_owed; call_sites_unresolved"),
                "{verdict}"
            );
        }
    }
}
