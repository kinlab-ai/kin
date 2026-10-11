// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Selected-graph source observations. These only qualify an answer; matching
//! source bytes cannot certify extraction, resolution or answer completeness.

use kin_model::LanguageId;
use kin_review::source_derivation::{
    PriorLocalBindingStatus, SourceBinding, SourceDerivationReport,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::envelope::AbsenceSubstrate;
use crate::{ContentBlock, ToolCallResult};

pub const KEY: &str = "source_derivation";

/// A source qualification computed before inventories leave the private
/// producer. It qualifies one tool's answer; it never certifies that answer.
pub const QUALIFICATION_KEY: &str = "source_qualification";
const QUALIFICATION_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SourceReading {
    NoLimitation,
    AuthorityUnavailable,
    ReadmissionFailed,
    StaleDerivation,
    UnprovenDerivation,
    LocalBindingOutstanding,
    LocalBindingUnproven,
    /// Every outstanding obligation in the answer's scope is itemized, and
    /// none sits in code the answer lists or spells a name it is about: the
    /// obligations are counted beside the answer, not part of its
    /// uncertainty, and never license an absence.
    LocalBindingUnrelated,
}

impl SourceReading {
    fn limiting_factor(self) -> Option<&'static str> {
        match self {
            Self::NoLimitation => None,
            Self::AuthorityUnavailable => Some("derived_source_unproven: the selected graph authority changed or could not be established, so current source binding is unproven"),
            Self::ReadmissionFailed => Some("semantic_readmission_failed: semantic readmission failed for admitted source, so these useful graph rows may describe last-good derived source, so they cannot establish complete current repository semantics"),
            Self::StaleDerivation => Some("derived_source_stale: graph-held source derivation differs from admitted source bytes, so returned rows may describe last-good source and do not establish current completeness"),
            Self::UnprovenDerivation => Some("derived_source_unproven: bounded graph evidence did not establish source binding for the checked scope, so current completeness cannot be inferred"),
            Self::LocalBindingOutstanding => Some("local_binding_outstanding: previously local source bindings remain unresolved, so current body and parse evidence cannot establish complete dependency or call-shape knowledge"),
            Self::LocalBindingUnproven => Some("local_binding_unproven: prior-local binding evidence could not be validated for the checked scope, so complete dependency or call-shape knowledge is unproven"),
            Self::LocalBindingUnrelated => Some("local_binding_unrelated: the outstanding local-name bindings in this answer's scope sit outside every entity it lists and spell none of their names, so they are counted beside the answer"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceQualification {
    version: u32,
    tool: String,
    scope: SourceObservationScope,
    source: SourceReading,
    relations: SourceReading,
    history: SourceReading,
}

impl SourceQualification {
    fn capture(tool: &str, observation: &SourceDerivationObservation) -> Self {
        Self {
            version: QUALIFICATION_VERSION,
            tool: tool.into(),
            scope: observation.scope,
            source: observation.reading(AbsenceSubstrate::EntityIndex),
            relations: observation.reading(AbsenceSubstrate::Relations),
            history: observation.reading(AbsenceSubstrate::History),
        }
    }

    pub(crate) fn valid_for(&self, tool: &str) -> bool {
        if self.version != QUALIFICATION_VERSION
            || self.tool != tool
            || !source_tool(tool)
            || self.history != SourceReading::NoLimitation
        {
            return false;
        }
        match self.scope {
            SourceObservationScope::SelectedHistorical => {
                self.source == SourceReading::NoLimitation
                    && self.relations == SourceReading::NoLimitation
            }
            SourceObservationScope::SelectedScopeUnavailable => {
                self.source == SourceReading::AuthorityUnavailable
                    && self.relations == SourceReading::AuthorityUnavailable
            }
            SourceObservationScope::LiveHead => match self.source {
                SourceReading::NoLimitation => matches!(
                    self.relations,
                    SourceReading::NoLimitation
                        | SourceReading::LocalBindingOutstanding
                        | SourceReading::LocalBindingUnproven
                        | SourceReading::LocalBindingUnrelated
                ),
                SourceReading::ReadmissionFailed
                | SourceReading::StaleDerivation
                | SourceReading::UnprovenDerivation => self.source == self.relations,
                _ => false,
            },
        }
    }

    pub(crate) fn is_historical(&self) -> bool {
        self.scope == SourceObservationScope::SelectedHistorical
    }

    pub(crate) fn limiting_factor(&self, substrate: AbsenceSubstrate) -> Option<&'static str> {
        match substrate {
            AbsenceSubstrate::History => self.history,
            AbsenceSubstrate::Relations => self.relations,
            AbsenceSubstrate::Vectors | AbsenceSubstrate::EntityIndex => self.source,
        }
        .limiting_factor()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceObservationScope {
    LiveHead,
    SelectedHistorical,
    SelectedScopeUnavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmissionFailureObservation {
    pub consecutive_failures: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// Which evidence the answer needs for local dependency bindings. A missing
/// field from an older producer remains conservative.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalBindingRequirement {
    #[default]
    Required,
    /// The answer reads source/declarations or ranks entities, not bindings.
    NotRequired,
    /// A store-wide calls reading established current, durable-context-valid
    /// ledgers, live proven targets and no unattributed expressions.
    CurrentCallSites,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceDerivationObservation {
    pub scope: SourceObservationScope,
    /// `selected_paths` or `admitted_inventory`. Neither covers excluded source tiers.
    pub checked_scope: String,
    /// Distinct from the earlier query's reads; no atomic payload attestation.
    pub sampled: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report: Option<SourceDerivationReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub admission_failure: Option<AdmissionFailureObservation>,
    #[serde(default)]
    pub local_binding_requirement: LocalBindingRequirement,
    /// Which sources' prior-local binding debt bears on this answer: those of
    /// a language that could hold a reference to an entity the answer names.
    /// Debt elsewhere stays disclosed in `report` and does not qualify the
    /// answer. `None`, from an older producer or for an answer that names no
    /// local entity, weighs the whole checked inventory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding_scope: Option<BindingScope>,
}

/// The languages whose sources could refer to what an answer is about, and
/// what the report's binding state reads over them alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BindingScope {
    /// False excludes only obligations attributed to current Test-role callers.
    /// Older observations retain their original all-role reading.
    #[serde(
        default = "all_caller_roles",
        skip_serializing_if = "is_all_caller_roles"
    )]
    pub include_tests: bool,
    /// Every language whose admitted sources could hold a reference to an
    /// entity the answer names (see [`referring_languages`]).
    pub languages: Vec<LanguageId>,
    /// What named those entities: `answer_entities` or `resolved_focal`.
    pub basis: String,
    /// The report's prior-local binding over these languages and whatever no
    /// language names. Disclosure only: the verdict recomputes it from the
    /// report, so a reader never trusts this copy.
    #[serde(default)]
    pub prior_local_binding: PriorLocalBindingStatus,
    #[serde(default)]
    pub outstanding_local_binding_obligations: Option<usize>,
    /// How many of those obligations the answer showed unrelated to it: each
    /// is itemized, none sits in an entity the answer names, and none spells
    /// one's name. Set only when that holds for every one in scope, so the
    /// count is the whole of the scoped debt; it is never an absence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unrelated_obligations: Option<usize>,
}

fn all_caller_roles() -> bool {
    true
}
fn is_all_caller_roles(value: &bool) -> bool {
    *value
}

/// The languages whose sources could hold a reference to an entity of
/// `language`: its calling-language domain, which the call-site census proves
/// over, widened by every cross-language binding a Kin producer can record.
/// The linker binds Java and Kotlin to each other, and a Swift source can
/// import C and C++ declarations through a module map. Wider is always sound
/// here: it only keeps more debt in an answer's scope.
pub fn referring_languages(language: LanguageId) -> Vec<LanguageId> {
    let mut languages = kin_model::calling_languages(language);
    let widened: &[LanguageId] = match language {
        LanguageId::Java | LanguageId::Kotlin => &[LanguageId::Java, LanguageId::Kotlin],
        LanguageId::C | LanguageId::Cpp => &[LanguageId::Swift],
        _ => &[],
    };
    for extra in widened {
        if !languages.contains(extra) {
            languages.push(*extra);
        }
    }
    languages
}

impl SourceDerivationObservation {
    pub fn unavailable(reason: &str) -> Self {
        Self {
            scope: SourceObservationScope::SelectedScopeUnavailable,
            checked_scope: "selected_authority_unavailable".into(),
            sampled: "selected_graph_after_query".into(),
            report: Some(SourceDerivationReport::unproven(reason)),
            admission_failure: None,
            local_binding_requirement: LocalBindingRequirement::Required,
            binding_scope: None,
        }
    }

    pub fn historical() -> Self {
        Self {
            scope: SourceObservationScope::SelectedHistorical,
            checked_scope: "historical_not_assessed_against_live_source".into(),
            sampled: "selected_graph_after_query".into(),
            report: None,
            admission_failure: None,
            local_binding_requirement: LocalBindingRequirement::Required,
            binding_scope: None,
        }
    }

    /// The prior-local binding status and obligation count that bear on this
    /// answer.
    ///
    /// The whole checked inventory's, unless the answer carries a binding
    /// scope and the report splits binding by language. Then only the
    /// languages in the scope count, together with whatever the report could
    /// not attribute to a language. A split that does not add up to the
    /// inventory's own reading is not a measurement, and the inventory's
    /// reading stands.
    pub fn local_binding_in_scope(&self) -> Option<(PriorLocalBindingStatus, Option<usize>)> {
        let report = self.report.as_ref()?;
        let mut whole = (
            report.prior_local_binding,
            report.outstanding_local_binding_obligations,
        );
        let Some(scope) = &self.binding_scope else {
            return Some(whole);
        };
        let scoped_roles = (!scope.include_tests)
            .then(|| report.binding_languages_excluding_tests())
            .flatten();
        let split = if let Some(split) = scoped_roles.as_ref() {
            whole = split.values().fold(
                (PriorLocalBindingStatus::NoRecordedDebt, Some(0)),
                |reading, binding| {
                    merge_binding_reading(
                        reading,
                        (
                            binding.prior_local_binding,
                            binding.outstanding_local_binding_obligations,
                        ),
                    )
                },
            );
            split
        } else if let Some(split) = &report.local_binding_by_language {
            split
        } else {
            return Some(whole);
        };
        let keys: Vec<String> = scope
            .languages
            .iter()
            .map(|language| kin_review::source_derivation::language_key(*language))
            .collect();
        let mut all = (PriorLocalBindingStatus::NoRecordedDebt, Some(0usize));
        let mut scoped = all;
        for (key, binding) in split {
            let reading = (
                binding.prior_local_binding,
                binding.outstanding_local_binding_obligations,
            );
            all = merge_binding_reading(all, reading);
            if key == kin_review::source_derivation::UNATTRIBUTED_LANGUAGE || keys.contains(key) {
                scoped = merge_binding_reading(scoped, reading);
            }
        }
        Some(if all == whole { scoped } else { whole })
    }

    /// Scope this answer's binding debt to the languages that could refer to
    /// `entities`. No entity leaves the whole inventory in scope.
    pub fn scope_bindings_to<'e>(
        &mut self,
        entities: impl IntoIterator<Item = &'e kin_model::Entity>,
        basis: &str,
    ) {
        let mut languages: Vec<LanguageId> = Vec::new();
        for entity in entities {
            for language in referring_languages(entity.language) {
                if !languages.contains(&language) {
                    languages.push(language);
                }
            }
        }
        if languages.is_empty() {
            self.binding_scope = None;
            return;
        }
        languages.sort_by_key(|language| kin_review::source_derivation::language_key(*language));
        let mut scope = BindingScope {
            include_tests: true,
            languages,
            basis: basis.into(),
            prior_local_binding: PriorLocalBindingStatus::Unproven,
            outstanding_local_binding_obligations: None,
            unrelated_obligations: None,
        };
        self.binding_scope = Some(scope.clone());
        if let Some((status, count)) = self.local_binding_in_scope() {
            scope.prior_local_binding = status;
            scope.outstanding_local_binding_obligations = count;
        }
        self.binding_scope = Some(scope);
    }

    fn reading(&self, substrate: AbsenceSubstrate) -> SourceReading {
        if substrate == AbsenceSubstrate::History
            || self.scope == SourceObservationScope::SelectedHistorical
        {
            return SourceReading::NoLimitation;
        }
        if self.scope == SourceObservationScope::SelectedScopeUnavailable {
            return SourceReading::AuthorityUnavailable;
        }
        // A store-wide failure need not affect a selected file whose current
        // binding was independently established. Keep the failure visible, but
        // do not turn it into an unrelated file-level trust downgrade.
        if (self.checked_scope != "selected_paths"
            || self
                .report
                .as_ref()
                .is_none_or(|report| report.body_binding != SourceBinding::Current))
            && self
                .admission_failure
                .as_ref()
                .is_some_and(|failure| failure.consecutive_failures > 0)
        {
            return SourceReading::ReadmissionFailed;
        }
        match self.report.as_ref().map(|report| report.body_binding) {
            Some(SourceBinding::Current)
                if substrate != AbsenceSubstrate::Relations
                    || self.local_binding_requirement == LocalBindingRequirement::NotRequired =>
            {
                SourceReading::NoLimitation
            }
            Some(SourceBinding::Current) => match self.local_binding_in_scope() {
                Some((PriorLocalBindingStatus::NoRecordedDebt, Some(0))) => {
                    SourceReading::NoLimitation
                }
                Some((PriorLocalBindingStatus::Outstanding, count)) => match &self.binding_scope {
                    Some(scope) if count.is_some() && scope.unrelated_obligations == count => {
                        SourceReading::LocalBindingUnrelated
                    }
                    _ => SourceReading::LocalBindingOutstanding,
                },
                Some((PriorLocalBindingStatus::Unproven, None | Some(0)))
                    if self.local_binding_requirement
                        == LocalBindingRequirement::CurrentCallSites =>
                {
                    SourceReading::NoLimitation
                }
                _ => SourceReading::LocalBindingUnproven,
            },
            Some(SourceBinding::Stale) => SourceReading::StaleDerivation,
            Some(SourceBinding::Unproven) | None => SourceReading::UnprovenDerivation,
        }
    }

    pub fn limiting_factor(&self, substrate: AbsenceSubstrate) -> Option<&'static str> {
        self.reading(substrate).limiting_factor()
    }
}

/// The provenance clause a compact reply carries when the checked inventory
/// holds prior-local binding debt that its binding scope leaves out, so that
/// debt stays disclosed as context where the full observation is folded away.
/// `None` when nothing lies outside the scope, or `observation` does not read.
pub(crate) fn out_of_scope_binding_clause(observation: &Value) -> Option<String> {
    let observed: SourceDerivationObservation = serde_json::from_value(observation.clone()).ok()?;
    let scope = observed.binding_scope.as_ref()?;
    let report = observed.report.as_ref()?;
    let (status, _) = observed.local_binding_in_scope()?;
    if status != PriorLocalBindingStatus::NoRecordedDebt
        || report.prior_local_binding == PriorLocalBindingStatus::NoRecordedDebt
    {
        return None;
    }
    let keys: Vec<String> = scope
        .languages
        .iter()
        .map(|language| kin_review::source_derivation::language_key(*language))
        .collect();
    let outside: Vec<String> = report
        .local_binding_by_language
        .as_ref()?
        .iter()
        .filter(|(key, _)| !keys.contains(key))
        .map(
            |(key, binding)| match binding.outstanding_local_binding_obligations {
                Some(count) => format!("{key} {count}"),
                None => format!("{key} unproven"),
            },
        )
        .collect();
    (!outside.is_empty()).then(|| {
        format!(
            "prior-local binding debt outside this answer's languages ({})",
            outside.join(", ")
        )
    })
}

/// Two binding readings merged: the more pessimistic status, and the sum of
/// their obligation counts, unknown when either is.
fn merge_binding_reading(
    left: (PriorLocalBindingStatus, Option<usize>),
    right: (PriorLocalBindingStatus, Option<usize>),
) -> (PriorLocalBindingStatus, Option<usize>) {
    (
        kin_review::source_derivation::merge_binding(left.0, right.0),
        left.1.zip(right.1).and_then(|(a, b)| a.checked_add(b)),
    )
}

/// Only source-derived queries read this substrate. Work/session/history and
/// exact artifact-byte reads have their own authorities and are not relabeled.
pub fn observes_sources(tool: &str, args: &std::collections::HashMap<String, Value>) -> bool {
    // resolve_diff gives current entity/files modes precedence. With neither,
    // semantic_diff reads committed changes only; impact/review still walk live
    // relations and must retain their source observation.
    if tool == "semantic_diff"
        && !["entity_ids", "files"].iter().any(|key| {
            args.get(*key)
                .and_then(Value::as_array)
                .is_some_and(|values| values.iter().any(Value::is_string))
        })
    {
        return false;
    }
    source_tool(tool)
}

fn source_tool(tool: &str) -> bool {
    matches!(
        tool,
        "semantic_search"
            | "semantic_locate"
            | "get_entity"
            | "get_entity_source"
            | "get_entity_body"
            | "get_entity_sources"
            | "get_context_pack"
            | "trace_computation"
            | "trace_data_flow"
            | "trace_path"
            | "find_references"
            | "bulk_check_references"
            | "explore_codebase"
            | "dead_code"
            | "find_dead_code_seeded"
            | "graph_neighborhood"
            | "lexical_lookup"
            | "semantic_diff"
            | "impact_analysis"
            | "semantic_review"
            | "kin_graph_status"
    )
}

/// Source-only tools do not ask whether dependencies or references are
/// complete. Context, traversal and entity records with attached calls do.
pub fn local_bindings_required(tool: &str) -> bool {
    !matches!(
        tool,
        "semantic_search"
            | "semantic_locate"
            | "lexical_lookup"
            | "get_entity_source"
            | "get_entity_body"
            | "get_entity_sources"
    )
}

/// Resolve the source paths of a bounded semantic answer from graph identities,
/// never from presentation paths supplied by the payload. Inbound, traversal
/// and inventory answers retain inventory scope: returned rows cannot establish
/// that an unseen caller's admitted source is current.
pub fn selected_answer_paths<G: kin_model::GraphStore + ?Sized>(
    tool: &str,
    result: &ToolCallResult,
    store: &G,
) -> Option<Vec<kin_model::RepoPath>> {
    if result.is_error == Some(true)
        || !matches!(
            tool,
            "get_entity" | "get_entity_source" | "get_entity_body" | "get_entity_sources"
        )
    {
        return None;
    }
    let mut ids = std::collections::BTreeSet::new();
    for block in &result.content {
        let ContentBlock::Text { text } = block;
        let value: Value = serde_json::from_str(text).ok()?;
        // A partial batch also answers a missing-source question. Its successful
        // rows do not establish the derivation state behind the missing rows.
        if tool == "get_entity_sources" && value.get("total_requested") != value.get("returned") {
            return None;
        }
        answer_identities(&value, &mut ids);
    }
    let mut paths = std::collections::BTreeMap::new();
    for id in ids {
        // A relation or outside-symbol identity is not a local entity.
        let Some(entity) = store.get_entity(&id).ok()? else {
            continue;
        };
        let file = entity
            .span
            .as_ref()
            .map(|span| &span.file)
            .or(entity.file_origin.as_ref())?;
        let path = kin_model::RepoPath::from_utf8(&file.0).ok()?;
        paths.insert(path.as_bytes().to_vec(), path);
    }
    (!paths.is_empty()).then(|| paths.into_values().collect())
}

/// Every graph identity a payload names by id, wherever it sits: the focal,
/// candidates, rows, callers and targets. Presentation paths and names are
/// never read.
fn answer_identities(value: &Value, found: &mut std::collections::BTreeSet<kin_model::EntityId>) {
    match value {
        Value::Object(object) => {
            for (key, child) in object {
                if matches!(key.as_str(), "id" | "entity_id" | "caller_id" | "target_id") {
                    if let Some(id) = child
                        .as_str()
                        .and_then(|id| uuid::Uuid::parse_str(id).ok())
                        .map(kin_model::EntityId)
                    {
                        found.insert(id);
                    }
                }
                answer_identities(child, found);
            }
        }
        Value::Array(values) => {
            for child in values {
                answer_identities(child, found);
            }
        }
        _ => {}
    }
}

/// The local entities `result` names by graph identity. A block that is not
/// JSON, or an identity the graph does not hold (a relation, an outside
/// symbol, a change), names none.
fn answer_entities<G: kin_model::GraphStore + ?Sized>(
    result: &ToolCallResult,
    store: &G,
) -> Vec<kin_model::Entity> {
    let mut ids = std::collections::BTreeSet::new();
    for block in &result.content {
        let ContentBlock::Text { text } = block;
        if let Ok(value) = serde_json::from_str::<Value>(text) {
            answer_identities(&value, &mut ids);
        }
    }
    ids.into_iter()
        .filter_map(|id| store.get_entity(&id).ok().flatten())
        .collect()
}

/// Inspect the original declared kinds, without filtering unknown classes into
/// an apparently calls-only answer.
fn calls_only_answer(payload: &Value) -> bool {
    payload
        .get("relation_kinds")
        .and_then(Value::as_array)
        .is_some_and(|kinds| {
            !kinds.is_empty()
                && kinds.iter().all(|kind| {
                    kind.as_str()
                        .is_some_and(|kind| kind.eq_ignore_ascii_case("calls"))
                })
        })
}

impl SourceDerivationObservation {
    /// Select the binding requirement separately from the source-path scope.
    /// A path subset proves no call-candidate completeness by itself.
    pub fn qualify_for_answer<G: kin_model::GraphStore + ?Sized>(
        &mut self,
        tool: &str,
        result: &ToolCallResult,
        store: &G,
    ) {
        self.local_binding_requirement = if local_bindings_required(tool) {
            LocalBindingRequirement::Required
        } else {
            LocalBindingRequirement::NotRequired
        };
        if self.local_binding_requirement == LocalBindingRequirement::NotRequired {
            return;
        }
        let entities = answer_entities(result, store);
        self.qualify_answer_scope(tool, result, store, &entities);
        self.fold_unrelated_bindings(&entities);
    }

    /// Mark the scoped binding debt unrelated to the answer when the report
    /// itemizes every outstanding obligation in scope and none of them sits
    /// in code of an entity the answer names (its focal, its rows) or spells
    /// one of their names. An obligation whose owner is not exactly
    /// attributed could sit anywhere, so it keeps the debt in the answer.
    /// The count stays disclosed in `binding_scope`; it is never an absence.
    ///
    /// Public so a surface that grades the same answer from an observation it
    /// takes before the query, as `kin refs` does for the focals a name
    /// resolves to, folds the same debt the same way.
    pub fn fold_unrelated_bindings(&mut self, entities: &[kin_model::Entity]) {
        let Some((PriorLocalBindingStatus::Outstanding, Some(count))) =
            self.local_binding_in_scope()
        else {
            return;
        };
        let (Some(report), Some(scope)) = (self.report.as_ref(), self.binding_scope.as_ref())
        else {
            return;
        };
        if count == 0 || report.local_binding_obligations_capped {
            return;
        }
        let keys: Vec<String> = scope
            .languages
            .iter()
            .map(|language| kin_review::source_derivation::language_key(*language))
            .collect();
        let in_scope: Vec<&kin_review::source_derivation::OutstandingBinding> = report
            .local_binding_obligations
            .iter()
            .filter(|obligation| {
                (obligation.language == kin_review::source_derivation::UNATTRIBUTED_LANGUAGE
                    || keys.contains(&obligation.language))
                    && (scope.include_tests
                        || obligation.role != kin_review::source_derivation::ObligationRole::Test)
            })
            .collect();
        if in_scope.len() != count {
            return;
        }
        let leaf = |name: &str| -> String {
            name.rsplit(['.', ':', '#', '/'])
                .next()
                .unwrap_or(name)
                .to_string()
        };
        let ids: std::collections::HashSet<kin_model::EntityId> =
            entities.iter().map(|entity| entity.id).collect();
        let names: std::collections::HashSet<String> =
            entities.iter().map(|entity| leaf(&entity.name)).collect();
        let related = in_scope.iter().any(|obligation| {
            obligation.owner.is_none_or(|owner| ids.contains(&owner))
                || names.contains(&leaf(&obligation.source_name))
                || names.contains(&leaf(&obligation.target_name))
        });
        if !related {
            if let Some(scope) = self.binding_scope.as_mut() {
                scope.unrelated_obligations = Some(count);
            }
        }
    }

    fn qualify_answer_scope<G: kin_model::GraphStore + ?Sized>(
        &mut self,
        tool: &str,
        result: &ToolCallResult,
        store: &G,
        entities: &[kin_model::Entity],
    ) {
        // A reference into an entity can only come from a source in a language
        // that can refer to it, so binding debt in any other language cannot
        // hide one. The answer's entities, the focal among them, name those
        // languages; an answer that names none keeps the whole inventory.
        self.scope_bindings_to(entities, "answer_entities");
        // References are the surface that can explicitly ask for calls alone.
        // Missing classes, a union, or shared-name sections remain conservative.
        if tool != "find_references" || result.content.len() != 1 {
            return;
        }
        let ContentBlock::Text { text } = &result.content[0];
        let Ok(payload) = serde_json::from_str::<Value>(text) else {
            return;
        };
        let include_tests = payload
            .pointer("/caller_scope/include_tests")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        if !include_tests {
            if let Some(scope) = &mut self.binding_scope {
                scope.include_tests = false;
            }
            if let Some((status, count)) = self.local_binding_in_scope() {
                if let Some(scope) = &mut self.binding_scope {
                    scope.prior_local_binding = status;
                    scope.outstanding_local_binding_obligations = count;
                }
            }
        }
        if !calls_only_answer(&payload) {
            return;
        }
        let Some(id) = payload
            .pointer("/focal_entity/id")
            .and_then(Value::as_str)
            .and_then(|id| uuid::Uuid::parse_str(id).ok())
            .map(kin_model::EntityId)
        else {
            return;
        };
        let Ok(Some(focal)) = store.get_entity(&id) else {
            return;
        };
        self.qualify_current_calls_scoped(store, &focal, include_tests);
    }

    /// The caller must establish that its answer reads calls alone. This
    /// producer scans the store, not only the focal's import family.
    pub fn qualify_current_calls<G: kin_model::GraphStore + ?Sized>(
        &mut self,
        store: &G,
        focal: &kin_model::Entity,
    ) {
        self.qualify_current_calls_scoped(store, focal, true);
    }

    fn qualify_current_calls_scoped<G: kin_model::GraphStore + ?Sized>(
        &mut self,
        store: &G,
        focal: &kin_model::Entity,
        include_tests: bool,
    ) {
        if crate::call_sites::calls_evidence_for_scoped(store, focal, include_tests)
            .is_ok_and(|evidence| evidence.settled)
        {
            self.local_binding_requirement = LocalBindingRequirement::CurrentCallSites;
        }
    }
}

/// Preserve raw useful rows and qualify any existing trust summary. Stdio later
/// recomputes its canonical verdict with this same typed observation.
pub fn disclose(
    result: ToolCallResult,
    observation: &SourceDerivationObservation,
) -> ToolCallResult {
    disclose_for(result, observation, "")
}

/// [`disclose`] for `tool`'s result. A rejected request is passed through as
/// it is: it read no source, so an observation of the sources behind it would
/// qualify nothing, and wrapping its sentence in one buried it. A name miss is
/// an answer about the graph and keeps the observation.
pub fn disclose_for(
    mut result: ToolCallResult,
    observation: &SourceDerivationObservation,
    tool: &str,
) -> ToolCallResult {
    if result.is_error == Some(true) && !tool.is_empty() {
        let miss = result.content.first().is_some_and(|block| {
            let ContentBlock::Text { text } = block;
            let message = serde_json::from_str::<Value>(text)
                .ok()
                .and_then(|value| {
                    value
                        .get("message")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .unwrap_or_else(|| text.clone());
            crate::negative::is_qualified_resolution_miss(tool, &message)
        });
        if !miss {
            return result;
        }
    }
    for block in &mut result.content {
        let ContentBlock::Text { text } = block;
        let mut value = match serde_json::from_str::<Value>(text) {
            Ok(value @ Value::Object(_)) => value,
            Ok(value) => json!({"result":value}),
            Err(_) => json!({"message":text}),
        };
        value[KEY] = serde_json::to_value(observation).expect("source observation serializes");
        if source_tool(tool) && tool != "kin_graph_status" && !crate::trace_pages::is_page(&value) {
            value[QUALIFICATION_KEY] =
                serde_json::to_value(SourceQualification::capture(tool, observation))
                    .expect("source qualification serializes");
        }
        qualify_existing(&mut value, observation);
        *text = serde_json::to_string(&value).expect("source observation payload serializes");
    }
    result
}

pub(crate) fn qualify_existing(value: &mut Value, observation: &SourceDerivationObservation) {
    if let Some(envelope) = value.get_mut("_kin").and_then(Value::as_object_mut) {
        envelope.insert(
            KEY.into(),
            serde_json::to_value(observation).expect("source observation serializes"),
        );
    }
    let Some(reason) = observation.limiting_factor(AbsenceSubstrate::Relations) else {
        return;
    };
    crate::verdict::qualify_source_observation(value, reason);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::{Envelope, NegativeClass};
    fn observation(binding: SourceBinding) -> SourceDerivationObservation {
        let mut report = SourceDerivationReport::unproven("fixture");
        report.body_binding = binding;
        SourceDerivationObservation {
            scope: SourceObservationScope::LiveHead,
            checked_scope: "admitted_inventory".into(),
            sampled: "selected_graph_after_query".into(),
            report: Some(report),
            admission_failure: None,
            local_binding_requirement: LocalBindingRequirement::Required,
            binding_scope: None,
        }
    }
    fn payload(result: &ToolCallResult) -> Value {
        let ContentBlock::Text { text } = result.content.first().unwrap();
        serde_json::from_str(text).unwrap()
    }
    fn healthy_envelope() -> Envelope {
        Envelope::daemon().with_health(&json!({"initialized":true,"graph_loaded":true,"graph_entity_count":1,"graph_relation_count":1,"durable_entity_count":1,"durable_relation_count":1,"reconcile":{"untracked_path_count":0,"untracked_observed_age_seconds":0,"last_admission_success_at":"2026-09-19T00:00:00Z","last_admission_success_age_seconds":0}}))
    }

    /// The daemon serializes and projects before the stdio seam deserializes
    /// and invokes the canonical finalizer. Keep that order in these controls.
    fn public_source_result(result: ToolCallResult, tool: &str) -> ToolCallResult {
        let mut wire = serde_json::to_value(result).unwrap();
        crate::semantic_surface::present_result(&mut wire, tool);
        serde_json::from_value(wire).unwrap()
    }

    #[test]
    fn public_source_qualification_preserves_bodies_and_absence_readings() {
        let tool = "get_entity_source";
        let body = "fn endpoint() { let literal = \"/private/source/real.rs\"; }";
        let mut current = observation(SourceBinding::Current);
        current.local_binding_requirement = LocalBindingRequirement::NotRequired;
        let mut stale = current.clone();
        stale.report.as_mut().unwrap().body_binding = SourceBinding::Stale;
        let mut unproven = current.clone();
        unproven.report.as_mut().unwrap().body_binding = SourceBinding::Unproven;
        let mut failed = current.clone();
        failed.admission_failure = Some(AdmissionFailureObservation {
            consecutive_failures: 1,
            last_error: Some("cannot admit /private/source/failed.rs".into()),
        });
        for (observed, expected_reason) in [
            (current, None),
            (stale, Some("derived_source_stale")),
            (unproven, Some("derived_source_unproven")),
            (failed, Some("semantic_readmission_failed")),
            (
                SourceDerivationObservation::unavailable("selected_authority_changed"),
                Some("derived_source_unproven"),
            ),
            (SourceDerivationObservation::historical(), None),
        ] {
            for raw in [
                ToolCallResult::text(
                    json!({"body":body,"span_coherence":"digest_verified",
                    "file_path":"/private/source/physical.rs","start_line":7})
                    .to_string(),
                ),
                ToolCallResult::error("Entity not found: missing"),
            ] {
                let missing = raw.is_error == Some(true);
                let private = disclose_for(raw, &observed, tool);
                let baseline = payload(&crate::envelope::finalize(
                    private.clone(),
                    healthy_envelope(),
                    tool,
                ));
                let public = public_source_result(private, tool);
                let public_payload = payload(&public);
                assert!(public_payload.get(KEY).is_none(), "{public_payload}");
                assert!(public_payload.get("file_path").is_none());
                assert_eq!(public_payload[QUALIFICATION_KEY]["tool"], tool);
                assert!(!public_payload
                    .to_string()
                    .contains("/private/source/failed.rs"));
                if !missing {
                    assert_eq!(
                        public_payload["body"], body,
                        "program literals are exact data"
                    );
                }
                let forwarded =
                    payload(&crate::envelope::finalize(public, healthy_envelope(), tool));
                assert_eq!(
                    forwarded["_kin"]["verdict"]["inputs"][KEY],
                    baseline["_kin"]["verdict"]["inputs"][KEY],
                    "{forwarded}"
                );
                assert_eq!(
                    forwarded["_kin"]["verdict"]["state"], baseline["_kin"]["verdict"]["state"],
                    "{forwarded}"
                );
                if let Some(reason) = expected_reason {
                    assert_eq!(forwarded["_kin"]["verdict"]["state"], "inconclusive");
                    assert!(
                        forwarded["_kin"]["verdict"]["limiting_factor"]
                            .as_str()
                            .unwrap()
                            .contains(reason),
                        "{forwarded}"
                    );
                } else {
                    assert_eq!(
                        forwarded["_kin"]["verdict"]["state"], "certified",
                        "{forwarded}"
                    );
                }
                if missing {
                    assert_eq!(
                        forwarded["negative"]["safe_to_conclude_absent"],
                        expected_reason.is_none(),
                        "{forwarded}"
                    );
                }
            }
        }
    }

    #[test]
    fn public_source_qualification_preserves_historical_upgrade_boundary() {
        let upgraded = kin_core::hydration_semantics::HydrationStanding::Rederived {
            under: 10,
            created_under: Some(9),
            derives: 10,
        };
        let mut current = observation(SourceBinding::Current);
        current.local_binding_requirement = LocalBindingRequirement::NotRequired;
        for (observed, historical) in [
            (current, false),
            (SourceDerivationObservation::historical(), true),
        ] {
            let public = public_source_result(
                disclose_for(
                    ToolCallResult::error("Entity not found: missing"),
                    &observed,
                    "get_entity_source",
                ),
                "get_entity_source",
            );
            let value = payload(&crate::envelope::finalize(
                public,
                healthy_envelope().with_hydration_semantics_observation(Some(&upgraded)),
                "get_entity_source",
            ));
            assert_eq!(
                value["negative"]["safe_to_conclude_absent"], !historical,
                "{value}"
            );
            if historical {
                assert!(
                    value["_kin"]["verdict"]["limiting_factor"]
                        .as_str()
                        .unwrap()
                        .contains("history_predates_upgrade"),
                    "{value}"
                );
            }
        }
    }

    #[test]
    fn public_source_qualification_rejects_missing_or_invalid_contracts() {
        let tool = "get_entity_source";
        let mut current = observation(SourceBinding::Current);
        current.local_binding_requirement = LocalBindingRequirement::NotRequired;
        let public = public_source_result(
            disclose_for(
                ToolCallResult::text(json!({"body":"fn endpoint() {}"}).to_string()),
                &current,
                tool,
            ),
            tool,
        );
        let original = payload(&public);
        let mut variants = Vec::new();
        for (key, value) in [
            ("version", json!(2)),
            ("tool", json!("graph_neighborhood")),
            ("relations", json!("unknown_reading")),
            ("relations", json!("authority_unavailable")),
            ("extra", json!(true)),
        ] {
            let mut invalid = original.clone();
            invalid[QUALIFICATION_KEY][key] = value;
            variants.push((tool, invalid));
        }
        let mut absent_substrate = original.clone();
        absent_substrate[QUALIFICATION_KEY]
            .as_object_mut()
            .unwrap()
            .remove("relations");
        variants.push((tool, absent_substrate));
        let mut absent = original.clone();
        absent.as_object_mut().unwrap().remove(QUALIFICATION_KEY);
        variants.push((tool, absent));
        let mut unknown_tool = original;
        unknown_tool[QUALIFICATION_KEY]["tool"] = json!("unknown_source_tool");
        variants.push(("unknown_source_tool", unknown_tool));
        for (receiver_tool, mut invalid) in variants {
            invalid["_kin"] = json!({"verdict":{"state":"certified"}});
            let value = payload(&crate::envelope::finalize(
                ToolCallResult::text(invalid.to_string()),
                healthy_envelope(),
                receiver_tool,
            ));
            assert_eq!(value["_kin"]["verdict"]["state"], "inconclusive", "{value}");
            assert_eq!(
                value["_kin"]["verdict"]["inputs"][KEY], "inconclusive",
                "{value}"
            );
            assert!(
                value["_kin"]["verdict"]["limiting_factor"]
                    .as_str()
                    .unwrap()
                    .contains("derived_source_unproven"),
                "{value}"
            );
        }
    }

    #[test]
    fn public_source_qualification_limits_existing_verdicts_without_erasing_gaps() {
        let tool = "get_entity_source";
        let public = public_source_result(
            disclose_for(
                ToolCallResult::text(json!({"body":"fn endpoint() {}"}).to_string()),
                &observation(SourceBinding::Stale),
                tool,
            ),
            tool,
        );
        for previous in ["certified", "inconclusive"] {
            let mut supplied = payload(&public);
            supplied["_kin"] = json!({"verdict":{"state":previous,
                "limiting_factor":"response_bounded",
                "inputs":{"response_budget":"inconclusive"}}});
            supplied["negative"] = json!({"trust":"authoritative",
                "safe_to_conclude_absent":true,"trust_reason":"prior reading"});
            let value = payload(&crate::envelope::finalize(
                ToolCallResult::text(supplied.to_string()),
                healthy_envelope(),
                tool,
            ));
            assert_eq!(value["_kin"]["verdict"]["state"], "inconclusive");
            assert_eq!(value["_kin"]["verdict"]["inputs"][KEY], "inconclusive");
            let factor = value["_kin"]["verdict"]["limiting_factor"]
                .as_str()
                .unwrap();
            assert!(factor.contains("derived_source_stale"), "{value}");
            assert!(factor.contains("response_bounded"), "{value}");
            assert_eq!(
                value["negative"]["safe_to_conclude_absent"], false,
                "{value}"
            );
        }
    }

    #[test]
    fn public_source_qualification_preserves_sampled_language_binding_scope() {
        let (graph, sources) = mixed_language_store("compiler/src/debug.rs", "compiler/src/lib.rs");
        for (focal, outstanding) in [
            (&sources["web/store.js"].entity, false),
            (&sources["compiler/src/lib.rs"].entity, true),
        ] {
            for (tool, answer) in answers_about(focal) {
                let mut observed = inventory_observation(&graph);
                observed.qualify_for_answer(tool, &answer, &graph);
                assert_eq!(binding_factor(&observed).is_some(), outstanding);
                let private = disclose_for(answer, &observed, tool);
                let baseline = payload(&crate::envelope::finalize(
                    private.clone(),
                    healthy_envelope(),
                    tool,
                ));
                let public = public_source_result(private, tool);
                let projected = payload(&public);
                assert!(projected.get(KEY).is_none());
                assert!(!projected.to_string().contains("compiler/src/debug.rs"));
                assert_eq!(projected[QUALIFICATION_KEY]["source"], "no_limitation");
                assert_eq!(
                    projected[QUALIFICATION_KEY]["relations"],
                    if outstanding {
                        "local_binding_outstanding"
                    } else {
                        "no_limitation"
                    }
                );
                let finalized =
                    payload(&crate::envelope::finalize(public, healthy_envelope(), tool));
                assert_eq!(
                    finalized["_kin"]["verdict"]["inputs"][KEY],
                    baseline["_kin"]["verdict"]["inputs"][KEY],
                    "{tool}: {finalized}"
                );
                assert_eq!(
                    finalized["_kin"]["verdict"]["inputs"][KEY]
                        .as_str()
                        .unwrap_or("not_applicable"),
                    if outstanding {
                        "inconclusive"
                    } else {
                        "not_applicable"
                    },
                    "{tool}: {finalized}"
                );
            }
        }
    }

    #[test]
    fn source_only_answers_do_not_inherit_unrelated_local_binding_debt() {
        let graph = kin_db::InMemoryGraph::new();
        let result = ToolCallResult::text(json!({"results":[]}).to_string());
        for tool in [
            "get_entity_source",
            "get_entity_body",
            "get_entity_sources",
            "semantic_search",
            "semantic_locate",
            "lexical_lookup",
        ] {
            for binding in [
                PriorLocalBindingStatus::Unproven,
                PriorLocalBindingStatus::Outstanding,
            ] {
                let mut current = observation(SourceBinding::Current);
                current.report.as_mut().unwrap().prior_local_binding = binding;
                current
                    .report
                    .as_mut()
                    .unwrap()
                    .outstanding_local_binding_obligations = Some(3);
                current.qualify_for_answer(tool, &result, &graph);
                assert_eq!(
                    current.local_binding_requirement,
                    LocalBindingRequirement::NotRequired
                );
                assert_eq!(
                    current.limiting_factor(AbsenceSubstrate::Relations),
                    None,
                    "{tool}"
                );
                assert_eq!(
                    current.report.as_ref().unwrap().prior_local_binding,
                    binding,
                    "the observation still discloses real binding state"
                );
                for body in [SourceBinding::Stale, SourceBinding::Unproven] {
                    current.report.as_mut().unwrap().body_binding = body;
                    assert!(
                        current
                            .limiting_factor(AbsenceSubstrate::Relations)
                            .is_some(),
                        "source-only still needs current body binding"
                    );
                }
            }
        }
    }

    #[test]
    fn relation_answers_and_legacy_observations_retain_binding_requirements() {
        let graph = kin_db::InMemoryGraph::new();
        let result = ToolCallResult::text(json!({"results":[]}).to_string());
        for tool in [
            "find_references",
            "get_context_pack",
            "impact_analysis",
            "semantic_review",
            "graph_neighborhood",
            "trace_data_flow",
            "unknown_tool",
        ] {
            let mut current = observation(SourceBinding::Current);
            current.qualify_for_answer(tool, &result, &graph);
            assert_eq!(
                current.local_binding_requirement,
                LocalBindingRequirement::Required
            );
            assert!(
                current
                    .limiting_factor(AbsenceSubstrate::Relations)
                    .unwrap()
                    .starts_with("local_binding_unproven:"),
                "{tool}"
            );
        }
        let mut legacy = serde_json::to_value(observation(SourceBinding::Current)).unwrap();
        legacy
            .as_object_mut()
            .unwrap()
            .remove("local_binding_requirement");
        let reopened: SourceDerivationObservation = serde_json::from_value(legacy).unwrap();
        assert_eq!(
            reopened.local_binding_requirement,
            LocalBindingRequirement::Required
        );
        assert!(reopened
            .limiting_factor(AbsenceSubstrate::Relations)
            .is_some());
        assert_eq!(
            reopened.limiting_factor(AbsenceSubstrate::EntityIndex),
            None
        );
        assert_eq!(reopened.limiting_factor(AbsenceSubstrate::Vectors), None);
    }

    #[test]
    fn current_call_sites_can_replace_missing_witness_but_never_recorded_debt() {
        let mut current = observation(SourceBinding::Current);
        current.local_binding_requirement = LocalBindingRequirement::CurrentCallSites;
        assert_eq!(current.limiting_factor(AbsenceSubstrate::Relations), None);
        current.report.as_mut().unwrap().prior_local_binding = PriorLocalBindingStatus::Outstanding;
        current
            .report
            .as_mut()
            .unwrap()
            .outstanding_local_binding_obligations = Some(1);
        assert!(current
            .limiting_factor(AbsenceSubstrate::Relations)
            .unwrap()
            .starts_with("local_binding_outstanding:"));
        current.report.as_mut().unwrap().prior_local_binding = PriorLocalBindingStatus::Unproven;
        assert!(
            current
                .limiting_factor(AbsenceSubstrate::Relations)
                .is_some(),
            "an unknown status with a positive obligation count is still debt"
        );
    }

    #[test]
    fn calls_only_qualification_requires_current_storewide_ledger_evidence() {
        use crate::call_sites::fixture::{admit, ledger, proof_context, spanned_entity};
        use kin_model::*;
        let graph = kin_db::InMemoryGraph::new();
        let focal_body = "def focal():\n    return 1\n";
        let caller_body = "def caller():\n    return focal()\n";
        let mut focal = spanned_entity("focal", "focal.py", LanguageId::Python, 0, focal_body);
        focal
            .metadata
            .extra
            .insert(kin_parser::FILE_PARSED_CALL_SITES_KEY.into(), json!(0));
        let mut caller = spanned_entity(
            "caller",
            "unimported.py",
            LanguageId::Python,
            0,
            caller_body,
        );
        caller.metadata.extra.insert(
            kin_parser::extract::EMBEDDING_BODY_PREVIEW_KEY.into(),
            json!(caller_body),
        );
        caller
            .metadata
            .extra
            .insert(kin_parser::FILE_PARSED_CALL_SITES_KEY.into(), json!(1));
        let context = proof_context(LanguageId::Python, "current");
        let context_id = context.id();
        admit(
            &graph,
            &[&focal, &caller],
            vec![
                context,
                ledger(&focal, focal_body, context_id, Vec::new()),
                ledger(
                    &caller,
                    caller_body,
                    context_id,
                    vec![("focal", CallSiteState::ProvenTarget { target: focal.id })],
                ),
            ],
        );
        // A replacement proof also needs the admitted source inventory and
        // each file's parse census; entity ledgers alone cannot prove there
        // is no admitted file whose calls were never attributed.
        graph
            .apply_transaction_delta(&TransactionDelta {
                tree_deltas: [("focal.py", focal_body), ("unimported.py", caller_body)]
                    .into_iter()
                    .map(|(path, body)| TreeDelta::Added {
                        artifact_id: ArtifactId::new(),
                        new: LocatedEntry::new(
                            RepoPath::from_utf8(path).unwrap(),
                            TreeEntry::blob(kin_blobs::digest(body.as_bytes()), false),
                        ),
                    })
                    .collect(),
                ..Default::default()
            })
            .unwrap();
        let answer = ToolCallResult::text(
            json!({
                "focal_entity":{"id":focal.id}, "references":[], "relation_kinds":["Calls"]
            })
            .to_string(),
        );
        let mut observed = observation(SourceBinding::Current);
        observed.qualify_for_answer("find_references", &answer, &graph);
        assert_eq!(
            observed.local_binding_requirement,
            LocalBindingRequirement::CurrentCallSites
        );
        assert_eq!(observed.limiting_factor(AbsenceSubstrate::Relations), None);

        // This caller has no import edge to the focal's file. Its unresolved
        // matching call must still revoke the replacement proof.
        let old = graph
            .lookup_resolution_record(&ResolutionRecordId::call_sites(caller.id))
            .unwrap()
            .unwrap();
        let new = ledger(
            &caller,
            caller_body,
            context_id,
            vec![(
                "focal",
                CallSiteState::Unresolved {
                    reason: UnresolvedReason::NoAnswer,
                },
            )],
        );
        graph
            .apply_transaction_delta(&TransactionDelta {
                resolution_record_deltas: vec![ResolutionRecordDelta::Modified { old, new }],
                ..Default::default()
            })
            .unwrap();
        observed.qualify_for_answer("find_references", &answer, &graph);
        assert_eq!(
            observed.local_binding_requirement,
            LocalBindingRequirement::Required
        );
        assert!(observed
            .limiting_factor(AbsenceSubstrate::Relations)
            .unwrap()
            .starts_with("local_binding_unproven:"));
    }

    fn scoped_entity(file: &str) -> kin_model::Entity {
        use kin_model::*;
        Entity {
            id: EntityId::new(),
            kind: EntityKind::Function,
            name: "focal".into(),
            language: LanguageId::Rust,
            fingerprint: SemanticFingerprint {
                algorithm: FingerprintAlgorithm::V1TreeSitter,
                ast_hash: Hash256::from_bytes([1; 32]),
                signature_hash: Hash256::from_bytes([2; 32]),
                behavior_hash: Hash256::from_bytes([3; 32]),
                equivalence_hash: Hash256::from_bytes([4; 32]),
                stability_score: 1.0,
            },
            file_origin: Some(FilePathId::new(file)),
            span: None,
            signature: "fn focal()".into(),
            visibility: Visibility::Public,
            role: EntityRole::Source,
            doc_summary: None,
            metadata: EntityMetadata::default(),
            lineage_parent: None,
            created_in: None,
            superseded_by: None,
        }
    }

    #[test]
    fn answer_path_scope_uses_graph_identity_and_never_narrows_broad_queries() {
        use kin_model::EntityStore;
        let graph = kin_db::InMemoryGraph::new();
        let mut focal = scoped_entity("presentation.rs");
        focal.span = Some(kin_model::SourceSpan {
            file: kin_model::FilePathId::new("truth.rs"),
            start_byte: 0,
            end_byte: 3,
            start_line: 0,
            end_line: 0,
            start_col: 0,
            end_col: 3,
        });
        graph.upsert_entity(&focal).unwrap();
        let other = scoped_entity("caller.rs");
        graph.upsert_entity(&other).unwrap();
        let result = ToolCallResult::text(
            json!({
                "focal_entity":{"id":focal.id,"file_path":"wrong.rs"},
                "references":[{"entity_id":other.id,"file_path":"also-wrong.rs"}]
            })
            .to_string(),
        );
        assert_eq!(
            selected_answer_paths("get_entity_sources", &result, &graph),
            Some(vec![
                kin_model::RepoPath::from_utf8("caller.rs").unwrap(),
                kin_model::RepoPath::from_utf8("truth.rs").unwrap(),
            ])
        );
        for tool in [
            "find_references",
            "bulk_check_references",
            "get_context_pack",
            "graph_neighborhood",
            "trace_computation",
            "trace_data_flow",
            "trace_path",
            "impact_analysis",
            "semantic_review",
            "semantic_search",
            "semantic_locate",
            "dead_code",
            "find_dead_code_seeded",
            "kin_graph_status",
            "explore_codebase",
            "unknown_tool",
        ] {
            assert_eq!(selected_answer_paths(tool, &result, &graph), None, "{tool}");
        }
        let partial = ToolCallResult::text(
            json!({
                "total_requested":2, "returned":1,
                "results":[{"id":focal.id}, {"id":kin_model::EntityId::new(), "reason":"not_found"}]
            })
            .to_string(),
        );
        assert_eq!(
            selected_answer_paths("get_entity_sources", &partial, &graph),
            None
        );
        let unknown = ToolCallResult::text(
            json!({"focal_entity":{"id":kin_model::EntityId::new()}}).to_string(),
        );
        assert_eq!(
            selected_answer_paths("get_entity_source", &unknown, &graph),
            None
        );
        assert_eq!(
            selected_answer_paths(
                "get_entity_source",
                &ToolCallResult::error("no source"),
                &graph
            ),
            None
        );
    }

    #[test]
    fn unknown_or_mixed_relation_classes_cannot_claim_calls_only() {
        assert!(calls_only_answer(&json!({"relation_kinds":["Calls"]})));
        for value in [
            json!({}),
            json!({"relation_kinds":[]}),
            json!({"relation_kinds":["Calls", "unknown"]}),
            json!({"relation_kinds":["Calls", "References"]}),
            json!({"relation_kinds":["Calls", null]}),
        ] {
            assert!(!calls_only_answer(&value), "{value}");
        }
    }

    #[test]
    fn unseen_stale_source_still_bounds_empty_inbound_answers() {
        use kin_model::*;
        let graph = kin_db::InMemoryGraph::new();
        let mut focal_id = EntityId::new();
        for (file, admitted_byte, derived_byte) in [("focal.rs", 1, 1), ("unreturned.rs", 2, 1)] {
            let artifact = ArtifactId::new();
            let mut entity = scoped_entity(file);
            if file == "focal.rs" {
                focal_id = entity.id;
            }
            let derived = Hash256::from_bytes([derived_byte; 32]);
            entity
                .metadata
                .extra
                .insert("blob_hash".into(), json!(derived.to_string()));
            graph
                .apply_transaction_delta(&TransactionDelta {
                    tree_deltas: vec![TreeDelta::Added {
                        artifact_id: artifact,
                        new: LocatedEntry::new(
                            RepoPath::from_utf8(file).unwrap(),
                            TreeEntry::blob(Hash256::from_bytes([admitted_byte; 32]), false),
                        ),
                    }],
                    ..Default::default()
                })
                .unwrap();
            let mut certificate = kin_index::build_parse_coverage_relation(
                &kin_index::FileParseData {
                    file_path: file.into(),
                    entities: vec![entity.clone()],
                    relations: Vec::new(),
                    imports: Vec::new(),
                },
                artifact,
                &ParseCompleteness::Full,
                &std::collections::HashSet::<String>::new(),
            );
            kin_index::bind_parse_coverage_source(&mut certificate, file, derived);
            graph.upsert_entity(&entity).unwrap();
            graph.upsert_relation(&certificate).unwrap();
        }
        let empty = ToolCallResult::text(
            json!({
                "focal_entity":{"id":focal_id}, "references":[], "relation_kinds":["Calls"]
            })
            .to_string(),
        );
        let body_paths = selected_answer_paths("get_entity_body", &empty, &graph).unwrap();
        let body_facts = graph
            .source_derivation_facts_with_reserved_relation(
                kin_db::SourceDerivationLimits::default(),
                Some(&body_paths),
                kin_index::binding_debt::local_binding_debt_id,
            )
            .unwrap();
        assert_eq!(
            kin_review::source_derivation::inspect_source_derivation(&body_facts).body_binding,
            SourceBinding::Current,
            "the returned focal's source really is current"
        );
        for tool in [
            "find_references",
            "bulk_check_references",
            "impact_analysis",
            "semantic_review",
        ] {
            let paths = selected_answer_paths(tool, &empty, &graph);
            assert!(
                paths.is_none(),
                "{tool} must inspect unseen possible callers"
            );
            let facts = graph
                .source_derivation_facts_with_reserved_relation(
                    kin_db::SourceDerivationLimits::default(),
                    paths.as_deref(),
                    kin_index::binding_debt::local_binding_debt_id,
                )
                .unwrap();
            let mut observation = observation(SourceBinding::Current);
            observation.report = Some(kin_review::source_derivation::inspect_source_derivation(
                &facts,
            ));
            // Even complete current call ledgers cannot rebind stale admitted bytes.
            observation.local_binding_requirement = LocalBindingRequirement::CurrentCallSites;
            assert_eq!(
                observation.report.as_ref().unwrap().body_binding,
                SourceBinding::Stale
            );
            let value = payload(&disclose(empty.clone(), &observation));
            assert!(observation
                .limiting_factor(AbsenceSubstrate::Relations)
                .unwrap()
                .starts_with("derived_source_stale:"));
            assert_eq!(
                value["source_derivation"]["report"]["body_binding"],
                "stale"
            );
        }
    }

    #[test]
    fn unparsed_admitted_file_bounds_inbound_and_trace_but_not_targeted_body() {
        use kin_model::*;
        let graph = kin_db::InMemoryGraph::new();
        let artifact = ArtifactId::new();
        let hash = Hash256::from_bytes([0x51; 32]);
        let mut focal = scoped_entity("focal.rs");
        focal
            .metadata
            .extra
            .insert("blob_hash".into(), json!(hash.to_string()));
        graph
            .apply_transaction_delta(&TransactionDelta {
                tree_deltas: vec![
                    TreeDelta::Added {
                        artifact_id: artifact,
                        new: LocatedEntry::new(
                            RepoPath::from_utf8("focal.rs").unwrap(),
                            TreeEntry::blob(hash, false),
                        ),
                    },
                    TreeDelta::Added {
                        artifact_id: ArtifactId::new(),
                        new: LocatedEntry::new(
                            RepoPath::from_utf8("new_caller.rs").unwrap(),
                            TreeEntry::blob(Hash256::from_bytes([0x52; 32]), false),
                        ),
                    },
                ],
                ..Default::default()
            })
            .unwrap();
        let mut certificate = kin_index::build_parse_coverage_relation(
            &kin_index::FileParseData {
                file_path: "focal.rs".into(),
                entities: vec![focal.clone()],
                relations: Vec::new(),
                imports: Vec::new(),
            },
            artifact,
            &ParseCompleteness::Full,
            &std::collections::HashSet::<String>::new(),
        );
        kin_index::bind_parse_coverage_source(&mut certificate, "focal.rs", hash);
        graph.upsert_entity(&focal).unwrap();
        graph.upsert_relation(&certificate).unwrap();
        // The admitted potential caller has neither an entity nor a parse
        // certificate. An entity-only scan would silently omit this debt.
        let answer = ToolCallResult::text(
            json!({
                "focal_entity":{"id":focal.id}, "references":[], "relation_kinds":["Calls"]
            })
            .to_string(),
        );
        let inspect = |paths: Option<&[RepoPath]>| {
            graph
                .source_derivation_facts_with_reserved_relation(
                    kin_db::SourceDerivationLimits::default(),
                    paths,
                    kin_index::binding_debt::local_binding_debt_id,
                )
                .unwrap()
        };
        let body_paths = selected_answer_paths("get_entity_body", &answer, &graph).unwrap();
        let body_facts = inspect(Some(&body_paths));
        assert_eq!(body_facts.artifacts.len(), 1);
        let mut body = observation(SourceBinding::Current);
        body.checked_scope = "selected_paths".into();
        body.report = Some(kin_review::source_derivation::inspect_source_derivation(
            &body_facts,
        ));
        body.qualify_for_answer("get_entity_body", &answer, &graph);
        assert_eq!(
            body.report.as_ref().unwrap().body_binding,
            SourceBinding::Current
        );
        assert_eq!(body.limiting_factor(AbsenceSubstrate::Relations), None);

        for tool in [
            "find_references",
            "bulk_check_references",
            "impact_analysis",
            "semantic_review",
            "trace_data_flow",
        ] {
            let paths = selected_answer_paths(tool, &answer, &graph);
            assert!(paths.is_none(), "{tool} must retain the admitted inventory");
            let facts = inspect(paths.as_deref());
            assert_eq!(facts.artifacts.len(), 2);
            assert_eq!(facts.entities.len(), 1);
            let mut observed = observation(SourceBinding::Current);
            observed.report = Some(kin_review::source_derivation::inspect_source_derivation(
                &facts,
            ));
            observed.qualify_for_answer(tool, &answer, &graph);
            if tool == "find_references" {
                // Even granting the strongest permitted calls-only replacement
                // cannot prove the source of a file not yet parsed into callers.
                observed.local_binding_requirement = LocalBindingRequirement::CurrentCallSites;
            } else {
                assert_eq!(
                    observed.local_binding_requirement,
                    LocalBindingRequirement::Required
                );
            }
            assert_eq!(
                observed.report.as_ref().unwrap().body_binding,
                SourceBinding::Unproven
            );
            let finalized = crate::envelope::finalize(
                disclose(answer.clone(), &observed),
                healthy_envelope(),
                tool,
            );
            let value = payload(&finalized);
            assert_eq!(
                value["_kin"][KEY]["report"]["body_binding"], "unproven",
                "{tool}"
            );
            assert_eq!(
                value["_kin"]["verdict"]["safe_to_conclude_absent"], false,
                "{tool}"
            );
            assert!(
                value["_kin"]["verdict"]["limiting_factor"]
                    .as_str()
                    .unwrap()
                    .contains("derived_source_unproven"),
                "{tool}: {value}"
            );
        }
    }

    #[test]
    fn stale_source_qualifies_populated_and_resolution_miss_answers() {
        let observation = observation(SourceBinding::Stale);
        for result in [
            ToolCallResult::text(json!({"entity_impacts":[{"entity_name":"target"}]}).to_string()),
            ToolCallResult::error("Entity not found: missing"),
        ] {
            let is_error = result.is_error;
            let raw = disclose(result, &observation);
            let finalized = crate::envelope::finalize(
                raw,
                healthy_envelope(),
                if is_error == Some(true) {
                    "find_references"
                } else {
                    "impact_analysis"
                },
            );
            let value = payload(&finalized);
            assert_eq!(finalized.is_error, is_error);
            assert_eq!(value["_kin"][KEY]["report"]["body_binding"], "stale");
            assert_eq!(value["_kin"]["verdict"]["state"], "inconclusive");
            assert!(!value["_kin"]["verdict"]["limiting_factor"]
                .as_str()
                .unwrap()
                .contains("unlisted_clause"));
            assert!(value["_kin"]["verdict"]["limiting_factor"]
                .as_str()
                .unwrap()
                .contains("derived_source_stale"));
            if is_error == Some(true) {
                assert_eq!(value["negative"]["trust"], "inconclusive");
                assert_eq!(value["negative"]["safe_to_conclude_absent"], false);
            }
        }
    }
    #[test]
    fn answer_only_keeps_source_observation_and_canonical_warning() {
        let result = disclose(
            ToolCallResult::text(
                json!({"references":[],"focal_entity":{"name":"target"}}).to_string(),
            ),
            &observation(SourceBinding::Unproven),
        );
        let budget = crate::budget::ResponseBudget::from_arguments(
            &[("answer_only".into(), json!(true))].into_iter().collect(),
        );
        let value = payload(&crate::envelope::finalize_bounded(
            result,
            healthy_envelope(),
            "find_references",
            &budget,
        ));
        assert_eq!(value["_kin"][KEY]["report"]["body_binding"], "unproven");
        assert_eq!(value["_kin"]["verdict"]["state"], "inconclusive");
    }
    #[test]
    fn binding_does_not_certify_and_history_does_not_inherit_head_failure() {
        let current = observation(SourceBinding::Current);
        let raw = disclose(
            ToolCallResult::text(json!({"results":[]}).to_string()),
            &current,
        );
        let value = payload(&crate::envelope::finalize(
            raw,
            healthy_envelope(),
            "semantic_search",
        ));
        assert_ne!(value["_kin"]["verdict"]["state"], "certified");
        let historical = SourceDerivationObservation::historical();
        let mut envelope = healthy_envelope();
        envelope.source_derivation = Some(historical);
        assert!(
            envelope
                .negative_trust(NegativeClass::Structural, AbsenceSubstrate::EntityIndex)
                .0
        );
        let mut failure = observation(SourceBinding::Stale);
        failure.admission_failure = Some(AdmissionFailureObservation {
            consecutive_failures: 1,
            last_error: Some("injected".into()),
        });
        envelope.source_derivation = Some(failure);
        assert!(
            envelope
                .negative_trust(NegativeClass::Structural, AbsenceSubstrate::History)
                .0
        );
        assert!(
            !envelope
                .negative_trust(NegativeClass::Structural, AbsenceSubstrate::Relations)
                .0
        );
    }
    #[test]
    fn existing_raw_trust_is_only_downgraded_and_malformed_metadata_refuses() {
        let focal = "00000000-0000-0000-0000-000000000001";
        let original = crate::envelope::finalize(
            ToolCallResult::text(json!({"references":[],"total_upstream":0,"counts":{"receiver_name_candidates":0},"degradations":[],"focal_entity":{"id":focal},"cross_repo":{"status":"available","authority_complete":true,"authority_revision":"sha256:complete","authority_roots":{"local":"local-root"},"authority_anchor":{"repo_id":"local","entity_id":focal}},"focal_resolution":{"addressed_by":"entity_id","same_name_candidates":1,"matched":"exact_focal_name","other_candidates":[]},"edge_coverage":{"scope":"language","language":"Python","requested_classes":["calls","imports","references"],"classes":{"calls":"present","imports":"present","references":"present"},"reference_enrichment":"available","budget_exhausted":false}}).to_string()),
            healthy_envelope(), "find_references");
        assert_eq!(
            payload(&original)["_kin"]["verdict"]["state"],
            "certified",
            "{}",
            payload(&original)
        );
        let raw = disclose(original, &observation(SourceBinding::Stale));
        let repeated = disclose(raw.clone(), &observation(SourceBinding::Stale));
        assert_eq!(
            payload(&raw),
            payload(&repeated),
            "late qualification is idempotent"
        );
        for value in [
            payload(&raw),
            payload(&crate::envelope::finalize(
                raw,
                healthy_envelope(),
                "find_references",
            )),
        ] {
            assert_eq!(value["negative"]["safe_to_conclude_absent"], false);
            assert_eq!(value["_kin"]["verdict"]["state"], "inconclusive");
            assert_eq!(
                value["_kin"]["verdict"]["absence_claim"],
                "not_authoritative"
            );
            assert_eq!(value["_kin"]["verdict"]["inputs"][KEY], "inconclusive");
            assert!(value["negative"]["advice"]
                .as_str()
                .unwrap()
                .contains("Absence is NOT authoritative"));
            assert!(value["edge_coverage"]["limits"]
                .as_array()
                .unwrap()
                .contains(&json!("source_derivation:inconclusive")));
            assert!(crate::verdict::disagreements(&value).is_empty(), "{value}");
        }
        let envelope = healthy_envelope().with_payload_metadata(&json!({KEY:"invalid"}));
        assert!(
            !envelope
                .negative_trust(NegativeClass::Structural, AbsenceSubstrate::EntityIndex)
                .0
        );
    }
    #[test]
    fn history_only_diff_and_unrelated_selected_file_keep_their_own_scope() {
        for args in [
            json!({"change_ids":["change"]}),
            json!({"base":"old","head":"new"}),
        ] {
            let args = serde_json::from_value(args).unwrap();
            assert!(!observes_sources("semantic_diff", &args));
            assert!(observes_sources("impact_analysis", &args));
        }
        for args in [
            json!({"entity_ids":["entity"],"base":"old","head":"new"}),
            json!({"files":["file.py"],"change_ids":["change"]}),
        ] {
            assert!(observes_sources(
                "semantic_diff",
                &serde_json::from_value(args).unwrap()
            ));
        }
        let mut selected = observation(SourceBinding::Current);
        selected.checked_scope = "selected_paths".into();
        selected.report.as_mut().unwrap().prior_local_binding =
            PriorLocalBindingStatus::NoRecordedDebt;
        selected
            .report
            .as_mut()
            .unwrap()
            .outstanding_local_binding_obligations = Some(0);
        selected.admission_failure = Some(AdmissionFailureObservation {
            consecutive_failures: 1,
            last_error: Some("other path failed".into()),
        });
        assert_eq!(
            selected.limiting_factor(AbsenceSubstrate::EntityIndex),
            None
        );
        selected.report.as_mut().unwrap().body_binding = SourceBinding::Stale;
        assert!(selected
            .limiting_factor(AbsenceSubstrate::EntityIndex)
            .unwrap()
            .contains("semantic_readmission_failed"));
    }
    #[test]
    fn small_budgets_keep_the_observation_and_do_not_certify() {
        for answer_only in [false, true] {
            let raw = disclose(
                ToolCallResult::text(
                    json!({"references":[],"focal_entity":{"id":"focal"}}).to_string(),
                ),
                &observation(SourceBinding::Stale),
            );
            let budget = crate::budget::ResponseBudget::from_arguments(
                &[
                    ("answer_only".into(), json!(answer_only)),
                    ("max_chars".into(), json!(2000)),
                ]
                .into_iter()
                .collect(),
            );
            let result = crate::envelope::finalize_bounded(
                raw,
                healthy_envelope(),
                "find_references",
                &budget,
            );
            let value = payload(&result);
            assert_eq!(value["_kin"][KEY]["report"]["body_binding"], "stale");
            assert_eq!(value["_kin"]["verdict"]["state"], "inconclusive");
            assert!(crate::verdict::disagreements(&value).is_empty(), "{value}");
        }
    }
    #[test]
    fn current_parse_and_prior_local_debt_stay_separate_in_raw_and_stdio_trust() {
        for status in [
            PriorLocalBindingStatus::Outstanding,
            PriorLocalBindingStatus::Unproven,
        ] {
            let mut observed = observation(SourceBinding::Current);
            let report = observed.report.as_mut().unwrap();
            report.parse_coverage = kin_review::source_derivation::DerivationCoverage::Complete;
            report.call_shape_parse_coverage_complete = true;
            report.prior_local_binding = status;
            report.outstanding_local_binding_obligations =
                (status == PriorLocalBindingStatus::Outstanding).then_some(2);
            let expected = if status == PriorLocalBindingStatus::Outstanding {
                "local_binding_outstanding"
            } else {
                "local_binding_unproven"
            };
            let raw = disclose(ToolCallResult::text(json!({"references":[],"focal_entity":{"id":"focal"},"negative":{"trust":"authoritative","safe_to_conclude_absent":true}}).to_string()), &observed);
            assert_eq!(payload(&raw)["negative"]["safe_to_conclude_absent"], false);
            for answer_only in [false, true] {
                let budget = crate::budget::ResponseBudget::from_arguments(
                    &[
                        ("answer_only".into(), json!(answer_only)),
                        ("max_chars".into(), json!(2000)),
                    ]
                    .into_iter()
                    .collect(),
                );
                let value = payload(&crate::envelope::finalize_bounded(
                    raw.clone(),
                    healthy_envelope(),
                    "find_references",
                    &budget,
                ));
                assert_eq!(value["_kin"][KEY]["report"]["body_binding"], "current");
                assert_eq!(value["_kin"][KEY]["report"]["parse_coverage"], "complete");
                assert_eq!(value["_kin"]["verdict"]["state"], "inconclusive");
                assert!(
                    value["_kin"]["verdict"]["limiting_factor"]
                        .as_str()
                        .unwrap()
                        .contains(expected),
                    "{value}"
                );
                // Tiny responses may omit the duplicate negative object;
                // the canonical inconclusive verdict and reason must survive.
                if let Some(negative) = value.get("negative") {
                    assert_eq!(negative["safe_to_conclude_absent"], false);
                }
                assert!(crate::verdict::disagreements(&value).is_empty(), "{value}");
            }
            observed.scope = SourceObservationScope::SelectedHistorical;
            assert_eq!(observed.limiting_factor(AbsenceSubstrate::Relations), None);
        }
    }

    /// A store `kin upgrade` brought current does not hide what its binding
    /// history still owes. With the hydration record current, outstanding or
    /// unproven prior-local binding is still what limits a live answer, named
    /// as such rather than as a store-semantics gap.
    #[test]
    fn an_upgraded_store_keeps_its_binding_limiting_factor_honest() {
        let upgraded = kin_core::hydration_semantics::HydrationStanding::Rederived {
            under: 10,
            created_under: Some(9),
            derives: 10,
        };
        for (status, outstanding, expected) in [
            (
                PriorLocalBindingStatus::Outstanding,
                Some(2),
                "local_binding_outstanding",
            ),
            (
                PriorLocalBindingStatus::Unproven,
                None,
                "local_binding_unproven",
            ),
        ] {
            let mut observed = observation(SourceBinding::Current);
            let report = observed.report.as_mut().unwrap();
            report.parse_coverage = kin_review::source_derivation::DerivationCoverage::Complete;
            report.call_shape_parse_coverage_complete = true;
            report.prior_local_binding = status;
            report.outstanding_local_binding_obligations = outstanding;
            let raw = disclose(ToolCallResult::text(json!({"references":[],"focal_entity":{"id":"focal"},"negative":{"trust":"authoritative","safe_to_conclude_absent":true}}).to_string()), &observed);
            let value = payload(&crate::envelope::finalize(
                raw,
                healthy_envelope().with_hydration_semantics_observation(Some(&upgraded)),
                "find_references",
            ));
            assert_eq!(value["_kin"]["hydration_semantics"]["standing"], "current");
            assert_eq!(value["_kin"]["hydration_semantics"]["upgraded_under"], 10);
            assert_eq!(value["_kin"]["verdict"]["state"], "inconclusive", "{value}");
            let factor = value["_kin"]["verdict"]["limiting_factor"]
                .as_str()
                .unwrap_or_default();
            assert!(factor.starts_with(expected), "{value}");
            assert!(crate::verdict::disagreements(&value).is_empty(), "{value}");
        }
    }

    #[test]
    fn older_source_metadata_cannot_default_to_no_binding_debt() {
        let mut value = serde_json::to_value(observation(SourceBinding::Current)).unwrap();
        value["report"]
            .as_object_mut()
            .unwrap()
            .remove("prior_local_binding");
        value["report"]
            .as_object_mut()
            .unwrap()
            .remove("outstanding_local_binding_obligations");
        let older: SourceDerivationObservation = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(
            older.report.as_ref().unwrap().prior_local_binding,
            PriorLocalBindingStatus::Unproven
        );
        assert_eq!(
            older
                .report
                .as_ref()
                .unwrap()
                .outstanding_local_binding_obligations,
            None
        );
        assert!(older
            .limiting_factor(AbsenceSubstrate::Relations)
            .unwrap()
            .contains("local_binding_unproven"));
        value["report"]["prior_local_binding"] = json!("unknown_future_status");
        let envelope = healthy_envelope().with_payload_metadata(&json!({KEY:value}));
        assert!(
            !envelope
                .negative_trust(NegativeClass::Structural, AbsenceSubstrate::Relations)
                .0
        );
    }
    #[test]
    fn no_recorded_debt_requires_observed_zero_not_missing_or_contradictory_count() {
        let mut clear = observation(SourceBinding::Current);
        let report = clear.report.as_mut().unwrap();
        report.prior_local_binding = PriorLocalBindingStatus::NoRecordedDebt;
        report.outstanding_local_binding_obligations = Some(0);
        assert_eq!(clear.limiting_factor(AbsenceSubstrate::Relations), None);
        let valid = serde_json::to_value(clear).unwrap();
        for count in [None, Some(json!(null)), Some(json!(1))] {
            let mut value = valid.clone();
            if let Some(count) = count {
                value["report"]["outstanding_local_binding_obligations"] = count;
            } else {
                value["report"]
                    .as_object_mut()
                    .unwrap()
                    .remove("outstanding_local_binding_obligations");
            }
            let observed: SourceDerivationObservation =
                serde_json::from_value(value.clone()).unwrap();
            assert!(observed
                .limiting_factor(AbsenceSubstrate::Relations)
                .unwrap()
                .contains("local_binding_unproven"));
            let envelope = healthy_envelope().with_payload_metadata(&json!({KEY:value}));
            assert!(
                !envelope
                    .negative_trust(NegativeClass::Structural, AbsenceSubstrate::Relations)
                    .0
            );
        }
    }

    /// One admitted, parsed source in the mixed-language fixture.
    struct FixtureSource {
        artifact: kin_model::ArtifactId,
        entity: kin_model::Entity,
    }

    /// A repository holding JavaScript and Rust sources, every one admitted
    /// with a current parse certificate under a checked binding history, and
    /// one prior-local binding debt, withdrawn from `debt_in`'s entity to
    /// `debt_to`'s. The files are `web/store.js` (the JavaScript focal),
    /// `web/view.js` (its caller), `compiler/src/lib.rs` (a Rust focal) and
    /// `compiler/src/debug.rs` (a Rust caller of it).
    fn mixed_language_store(
        debt_in: &str,
        debt_to: &str,
    ) -> (
        kin_db::InMemoryGraph,
        std::collections::BTreeMap<&'static str, FixtureSource>,
    ) {
        use kin_model::*;
        let graph = kin_db::InMemoryGraph::new();
        let mut sources = std::collections::BTreeMap::new();
        for (index, (file, name, language)) in [
            ("web/store.js", "containsElement", LanguageId::JavaScript),
            ("web/view.js", "view", LanguageId::JavaScript),
            ("compiler/src/lib.rs", "lower", LanguageId::Rust),
            ("compiler/src/debug.rs", "print", LanguageId::Rust),
        ]
        .into_iter()
        .enumerate()
        {
            let artifact = ArtifactId::new();
            let hash = Hash256::from_bytes([0x60 + index as u8; 32]);
            let mut entity = scoped_entity(file);
            entity.name = name.into();
            entity.language = language;
            entity
                .metadata
                .extra
                .insert("blob_hash".into(), json!(hash.to_string()));
            graph
                .apply_transaction_delta(&TransactionDelta {
                    tree_deltas: vec![TreeDelta::Added {
                        artifact_id: artifact,
                        new: LocatedEntry::new(
                            RepoPath::from_utf8(file).unwrap(),
                            TreeEntry::blob(hash, false),
                        ),
                    }],
                    ..Default::default()
                })
                .unwrap();
            let mut certificate = kin_index::build_parse_coverage_relation(
                &kin_index::FileParseData {
                    file_path: file.into(),
                    entities: vec![entity.clone()],
                    relations: Vec::new(),
                    imports: Vec::new(),
                },
                artifact,
                &ParseCompleteness::Full,
                &std::collections::HashSet::<String>::new(),
            );
            kin_index::bind_parse_coverage_source(&mut certificate, file, hash);
            graph.upsert_entity(&entity).unwrap();
            graph.upsert_relation(&certificate).unwrap();
            sources.insert(file, FixtureSource { artifact, entity });
        }
        let source = &sources[debt_in];
        let target = &sources[debt_to];
        let hash = |file: &str| {
            let index = [
                "web/store.js",
                "web/view.js",
                "compiler/src/lib.rs",
                "compiler/src/debug.rs",
            ]
            .iter()
            .position(|known| *known == file)
            .unwrap();
            Hash256::from_bytes([0x60 + index as u8; 32])
        };
        let retired = Relation {
            id: RelationId::new(),
            kind: RelationKind::Calls,
            src: GraphNodeId::Entity(source.entity.id),
            dst: GraphNodeId::Entity(target.entity.id),
            confidence: 1.0,
            origin: RelationOrigin::Parsed,
            created_in: None,
            import_source: None,
            evidence: Vec::new(),
        };
        let debt = kin_model::binding_debt::build_local_binding_debt(
            source.artifact,
            kin_model::binding_debt::LocalBindingDebt {
                source_file: FilePathId::new(debt_in),
                observed_source_digest: hash(debt_in),
                obligations: vec![kin_model::binding_debt::LocalBindingObligation {
                    retired_relation: retired,
                    source_name: source.entity.name.clone(),
                    source_digest: hash(debt_in),
                    prior_source_file: None,
                    target_artifact: target.artifact,
                    target_file: FilePathId::new(debt_to),
                    target_name: target.entity.name.clone(),
                }],
            },
        )
        .unwrap();
        graph.upsert_relation(&debt).unwrap();
        (graph, sources)
    }

    /// The whole-inventory observation the daemon takes, read the way it reads
    /// it, under a checked binding history.
    fn inventory_observation(graph: &kin_db::InMemoryGraph) -> SourceDerivationObservation {
        let mut facts = graph
            .source_derivation_facts_with_reserved_relation(
                kin_db::SourceDerivationLimits::default(),
                None,
                kin_index::binding_debt::local_binding_debt_id,
            )
            .unwrap();
        facts.binding_history = kin_model::BindingHistoryObservation::Checked {
            lineage: kin_model::Hash256::from_bytes([0x5a; 32]),
            generation: 1,
        };
        let mut observed = observation(SourceBinding::Current);
        observed.report = Some(kin_review::source_derivation::inspect_source_derivation(
            &facts,
        ));
        observed
    }

    /// One answer per tool that reads this observation, each naming `entity`
    /// the way that tool's payload does.
    fn answers_about(entity: &kin_model::Entity) -> Vec<(&'static str, ToolCallResult)> {
        let id = entity.id.to_string();
        vec![
            (
                "find_references",
                ToolCallResult::text(
                    json!({"focal_entity":{"id":id}, "references":[],
                           "relation_kinds":["calls","imports","references"]})
                    .to_string(),
                ),
            ),
            (
                "impact_analysis",
                ToolCallResult::text(
                    json!({"entity_impacts":[{"entity_id":id, "dependents":[]}]}).to_string(),
                ),
            ),
            (
                "trace_data_flow",
                ToolCallResult::text(json!({"focal":{"entity_id":id}, "steps":[]}).to_string()),
            ),
            (
                "get_context_pack",
                ToolCallResult::text(
                    json!({"focal_entity":{"id":id}, "dependents":[], "dependencies":[]})
                        .to_string(),
                ),
            ),
        ]
    }

    fn binding_factor(observed: &SourceDerivationObservation) -> Option<&'static str> {
        observed
            .limiting_factor(AbsenceSubstrate::Relations)
            .filter(|factor| factor.starts_with("local_binding"))
    }

    #[test]
    fn inventory_report_splits_prior_local_binding_by_source_language() {
        let (graph, _) = mixed_language_store("compiler/src/debug.rs", "compiler/src/lib.rs");
        let observed = inventory_observation(&graph);
        let report = observed.report.as_ref().unwrap();
        assert_eq!(report.body_binding, SourceBinding::Current, "{report:?}");
        assert_eq!(
            report.prior_local_binding,
            PriorLocalBindingStatus::Outstanding
        );
        assert_eq!(report.outstanding_local_binding_obligations, Some(1));
        let split = report.local_binding_by_language.as_ref().unwrap();
        assert_eq!(
            split.keys().collect::<Vec<_>>(),
            vec!["Rust"],
            "only the language carrying debt is listed: {split:?}"
        );
        assert_eq!(
            split["Rust"],
            kin_review::source_derivation::LanguageBinding {
                prior_local_binding: PriorLocalBindingStatus::Outstanding,
                outstanding_local_binding_obligations: Some(1),
                files: 1,
            }
        );
        assert_eq!(
            report.issues[0].path.as_deref(),
            Some("compiler/src/debug.rs")
        );
    }

    #[test]
    fn out_of_scope_binding_debt_does_not_downgrade_a_settled_answer() {
        let (graph, sources) = mixed_language_store("compiler/src/debug.rs", "compiler/src/lib.rs");
        let focal = &sources["web/store.js"].entity;
        for (tool, answer) in answers_about(focal) {
            for settled in [false, true] {
                let mut observed = inventory_observation(&graph);
                observed.qualify_for_answer(tool, &answer, &graph);
                if settled {
                    observed.local_binding_requirement = LocalBindingRequirement::CurrentCallSites;
                }
                let scope = observed
                    .binding_scope
                    .clone()
                    .expect("the answer names its focal");
                assert_eq!(
                    scope.languages,
                    vec![LanguageId::JavaScript, LanguageId::TypeScript],
                    "{tool}"
                );
                assert_eq!(scope.basis, "answer_entities");
                assert_eq!(
                    scope.prior_local_binding,
                    PriorLocalBindingStatus::NoRecordedDebt,
                    "{tool}"
                );
                assert_eq!(scope.outstanding_local_binding_obligations, Some(0));
                assert_eq!(
                    observed.limiting_factor(AbsenceSubstrate::Relations),
                    None,
                    "{tool}"
                );
                // The debt is still disclosed, as context, in the report.
                let report = observed.report.as_ref().unwrap();
                assert_eq!(
                    report.prior_local_binding,
                    PriorLocalBindingStatus::Outstanding
                );
                assert_eq!(report.outstanding_local_binding_obligations, Some(1));
                // The stdio finalizer re-reads the observation from the payload
                // and reaches the same reading.
                let value = payload(&crate::envelope::finalize(
                    disclose(answer.clone(), &observed),
                    healthy_envelope(),
                    tool,
                ));
                if crate::envelope::carries_compact_envelope(tool) {
                    // A compact reply folds the observation into its one
                    // provenance line, which still names the debt it left out.
                    assert!(
                        value["_kin"]["provenance"].as_str().unwrap().contains(
                            "prior-local binding debt outside this answer's languages (Rust 1)"
                        ),
                        "{tool}: {value}"
                    );
                } else {
                    assert_eq!(
                        value["_kin"][KEY]["report"]["prior_local_binding"], "outstanding",
                        "{tool}: {value}"
                    );
                    assert_eq!(
                        value["_kin"][KEY]["binding_scope"]["prior_local_binding"],
                        "no_recorded_debt",
                        "{tool}: {value}"
                    );
                }
                assert!(
                    !value["_kin"]["verdict"]["limiting_factor"]
                        .as_str()
                        .unwrap_or_default()
                        .contains("local_binding"),
                    "{tool}: {value}"
                );
            }
        }
    }

    #[test]
    fn in_scope_binding_debt_still_downgrades_the_answer() {
        // The caller in the focal's own language carries the debt, and a
        // TypeScript focal is reachable from JavaScript sources too.
        let (graph, sources) = mixed_language_store("web/view.js", "web/store.js");
        let mut typescript = sources["web/store.js"].entity.clone();
        typescript.id = kin_model::EntityId::new();
        typescript.language = LanguageId::TypeScript;
        kin_model::EntityStore::upsert_entity(&graph, &typescript).unwrap();
        for focal in [&sources["web/store.js"].entity, &typescript] {
            for (tool, answer) in answers_about(focal) {
                for settled in [false, true] {
                    let mut observed = inventory_observation(&graph);
                    observed.qualify_for_answer(tool, &answer, &graph);
                    if settled {
                        // Settled call evidence never pays recorded debt in scope.
                        observed.local_binding_requirement =
                            LocalBindingRequirement::CurrentCallSites;
                    }
                    assert_eq!(
                        observed.binding_scope.as_ref().unwrap().prior_local_binding,
                        PriorLocalBindingStatus::Outstanding,
                        "{tool}"
                    );
                    assert!(
                        binding_factor(&observed)
                            .is_some_and(|factor| factor.starts_with("local_binding_outstanding:")),
                        "{tool}: {observed:?}"
                    );
                    let value = payload(&crate::envelope::finalize(
                        disclose(answer.clone(), &observed),
                        healthy_envelope(),
                        tool,
                    ));
                    assert_eq!(value["_kin"]["verdict"]["state"], "inconclusive", "{tool}");
                    assert!(
                        value["_kin"]["verdict"]["limiting_factor"]
                            .as_str()
                            .unwrap()
                            .contains("local_binding_outstanding"),
                        "{tool}: {value}"
                    );
                }
            }
        }
    }

    #[test]
    fn mixed_language_answers_keep_their_own_binding_debt() {
        let (graph, sources) = mixed_language_store("compiler/src/debug.rs", "compiler/src/lib.rs");
        let javascript = &sources["web/store.js"].entity;
        let rust = &sources["compiler/src/lib.rs"].entity;
        for (tool, answer) in answers_about(javascript) {
            let mut observed = inventory_observation(&graph);
            observed.qualify_for_answer(tool, &answer, &graph);
            assert_eq!(binding_factor(&observed), None, "{tool}");
        }
        for (tool, answer) in answers_about(rust) {
            let mut observed = inventory_observation(&graph);
            observed.qualify_for_answer(tool, &answer, &graph);
            assert_eq!(
                observed.binding_scope.as_ref().unwrap().languages,
                vec![LanguageId::Rust],
                "{tool}"
            );
            assert!(
                binding_factor(&observed)
                    .is_some_and(|factor| factor.starts_with("local_binding_outstanding:")),
                "{tool}"
            );
        }
        // An answer naming entities of both languages weighs both.
        let both = ToolCallResult::text(
            json!({"focals":[{"entity_id":javascript.id}, {"entity_id":rust.id}]}).to_string(),
        );
        let mut observed = inventory_observation(&graph);
        observed.qualify_for_answer("get_context_pack", &both, &graph);
        assert!(binding_factor(&observed).is_some());
        // An answer that names no local entity keeps the whole inventory.
        for unnamed in [
            ToolCallResult::text(json!({"references":[]}).to_string()),
            ToolCallResult::text(
                json!({"focal_entity":{"id":kin_model::EntityId::new()}}).to_string(),
            ),
            ToolCallResult::error("Entity not found: missing"),
        ] {
            let mut observed = inventory_observation(&graph);
            observed.qualify_for_answer("find_references", &unnamed, &graph);
            assert_eq!(observed.binding_scope, None);
            assert!(binding_factor(&observed).is_some());
        }
        // Source-only reads keep ignoring binding debt, scoped or not.
        let mut source_only = inventory_observation(&graph);
        source_only.qualify_for_answer("get_entity_source", &answers_about(rust)[0].1, &graph);
        assert_eq!(
            source_only.limiting_factor(AbsenceSubstrate::Relations),
            None
        );
    }

    #[test]
    fn a_scope_never_weighs_less_than_the_measured_split_supports() {
        let (graph, sources) = mixed_language_store("compiler/src/debug.rs", "compiler/src/lib.rs");
        let javascript = &sources["web/store.js"].entity;
        let answer = &answers_about(javascript)[0].1;
        // Unattributed binding state bears on every language.
        let mut observed = inventory_observation(&graph);
        observed.qualify_for_answer("find_references", answer, &graph);
        let split = observed
            .report
            .as_mut()
            .unwrap()
            .local_binding_by_language
            .as_mut()
            .unwrap();
        split.insert(
            kin_review::source_derivation::UNATTRIBUTED_LANGUAGE.into(),
            kin_review::source_derivation::LanguageBinding {
                prior_local_binding: PriorLocalBindingStatus::Unproven,
                outstanding_local_binding_obligations: None,
                files: 0,
            },
        );
        // A split that no longer adds up to the inventory's reading is not a
        // measurement, so the whole inventory's outstanding debt stands.
        assert!(binding_factor(&observed)
            .unwrap()
            .starts_with("local_binding_outstanding:"));
        let report = observed.report.as_mut().unwrap();
        report.prior_local_binding = PriorLocalBindingStatus::Unproven;
        report.outstanding_local_binding_obligations = None;
        assert!(binding_factor(&observed)
            .unwrap()
            .starts_with("local_binding_unproven:"));
        // A report from a producer that did not split, and an observation from
        // one that did not scope, both weigh the whole inventory.
        for older in [true, false] {
            let mut observed = inventory_observation(&graph);
            observed.qualify_for_answer("find_references", answer, &graph);
            if older {
                observed.report.as_mut().unwrap().local_binding_by_language = None;
            } else {
                let mut value = serde_json::to_value(&observed).unwrap();
                value.as_object_mut().unwrap().remove("binding_scope");
                observed = serde_json::from_value(value).unwrap();
            }
            assert!(binding_factor(&observed)
                .unwrap()
                .starts_with("local_binding_outstanding:"));
        }
        // A scope that names the debt's language through interop keeps it.
        for (focal, referring) in [
            (LanguageId::Kotlin, LanguageId::Java),
            (LanguageId::C, LanguageId::Swift),
            (LanguageId::Cpp, LanguageId::C),
            (LanguageId::TypeScript, LanguageId::JavaScript),
        ] {
            assert!(referring_languages(focal).contains(&referring), "{focal:?}");
        }
        assert_eq!(
            referring_languages(LanguageId::Python),
            vec![LanguageId::Python]
        );
    }

    /// A JavaScript repository whose one prior-local binding debt is exactly
    /// attributed to `helper` in `web/util.js`: the retired call sits inside
    /// `helper`'s declaration, was made through `source_name`, and pointed
    /// at `target_name`. `web/store.js` holds the focal `containsElement`
    /// and `web/view.js` its caller `view`.
    fn attributed_debt_store(
        source_name: &str,
        target_name: &str,
    ) -> (
        kin_db::InMemoryGraph,
        std::collections::BTreeMap<&'static str, FixtureSource>,
    ) {
        use kin_model::*;
        let graph = kin_db::InMemoryGraph::new();
        let mut sources = std::collections::BTreeMap::new();
        let files = [
            ("web/store.js", "containsElement"),
            ("web/view.js", "view"),
            ("web/util.js", "helper"),
        ];
        for (index, (file, name)) in files.into_iter().enumerate() {
            let artifact = ArtifactId::new();
            let hash = Hash256::from_bytes([0x70 + index as u8; 32]);
            let mut entity = scoped_entity(file);
            entity.name = name.into();
            entity.language = LanguageId::JavaScript;
            entity.span = Some(SourceSpan {
                file: FilePathId::new(file),
                start_byte: 0,
                end_byte: 100,
                start_line: 0,
                end_line: 5,
                start_col: 0,
                end_col: 0,
            });
            entity
                .metadata
                .extra
                .insert("blob_hash".into(), json!(hash.to_string()));
            graph
                .apply_transaction_delta(&TransactionDelta {
                    tree_deltas: vec![TreeDelta::Added {
                        artifact_id: artifact,
                        new: LocatedEntry::new(
                            RepoPath::from_utf8(file).unwrap(),
                            TreeEntry::blob(hash, false),
                        ),
                    }],
                    ..Default::default()
                })
                .unwrap();
            let mut certificate = kin_index::build_parse_coverage_relation(
                &kin_index::FileParseData {
                    file_path: file.into(),
                    entities: vec![entity.clone()],
                    relations: Vec::new(),
                    imports: Vec::new(),
                },
                artifact,
                &ParseCompleteness::Full,
                &std::collections::HashSet::<String>::new(),
            );
            kin_index::bind_parse_coverage_source(&mut certificate, file, hash);
            graph.upsert_entity(&entity).unwrap();
            graph.upsert_relation(&certificate).unwrap();
            sources.insert(file, FixtureSource { artifact, entity });
        }
        let source = &sources["web/util.js"];
        let target = &sources["web/view.js"];
        let body = Hash256::from_bytes([0x72; 32]);
        let retired = Relation {
            id: RelationId::new(),
            kind: RelationKind::Calls,
            src: GraphNodeId::Entity(source.entity.id),
            dst: GraphNodeId::Entity(target.entity.id),
            confidence: 1.0,
            origin: RelationOrigin::Parsed,
            created_in: None,
            import_source: None,
            evidence: vec![RelationEvidence {
                source_span: Some(SourceSpan {
                    file: FilePathId::new("web/util.js"),
                    start_byte: 10,
                    end_byte: 15,
                    start_line: 1,
                    end_line: 1,
                    start_col: 2,
                    end_col: 7,
                }),
                occurrence_count: 1,
                ..RelationEvidence::default()
            }],
        };
        let debt = kin_model::binding_debt::build_local_binding_debt(
            source.artifact,
            kin_model::binding_debt::LocalBindingDebt {
                source_file: FilePathId::new("web/util.js"),
                observed_source_digest: body,
                obligations: vec![kin_model::binding_debt::LocalBindingObligation {
                    retired_relation: retired,
                    source_name: source_name.into(),
                    source_digest: body,
                    prior_source_file: None,
                    target_artifact: target.artifact,
                    target_file: FilePathId::new("web/view.js"),
                    target_name: target_name.into(),
                }],
            },
        )
        .unwrap();
        graph.upsert_relation(&debt).unwrap();
        (graph, sources)
    }

    fn references_answer(focal: &kin_model::Entity, rows: &[&kin_model::Entity]) -> ToolCallResult {
        ToolCallResult::text(
            json!({"focal_entity":{"id":focal.id, "name":focal.name},
                   "references":rows.iter().map(|row| json!({"entity_id":row.id, "name":row.name}))
                       .collect::<Vec<_>>(),
                   "relation_kinds":["calls","imports","references"]})
            .to_string(),
        )
    }

    #[test]
    fn the_report_itemizes_each_outstanding_obligation_by_identity_and_name() {
        let (graph, sources) = attributed_debt_store("fmt", "format");
        let observed = inventory_observation(&graph);
        let report = observed.report.as_ref().unwrap();
        assert_eq!(report.outstanding_local_binding_obligations, Some(1));
        assert!(!report.local_binding_obligations_capped);
        assert_eq!(
            report.local_binding_obligations,
            vec![kin_review::source_derivation::OutstandingBinding {
                language: "JavaScript".into(),
                role: kin_review::source_derivation::ObligationRole::NonTest,
                owner: Some(sources["web/util.js"].entity.id),
                source_name: "fmt".into(),
                target_name: "format".into(),
            }]
        );
        // Identities and names only: the itemized list never carries a path.
        let value = serde_json::to_value(&report.local_binding_obligations).unwrap();
        assert!(!value.to_string().contains("web/"), "{value}");
    }

    #[test]
    fn binding_debt_outside_every_listed_entity_and_name_is_folded_never_an_absence() {
        let (graph, sources) = attributed_debt_store("fmt", "format");
        let focal = &sources["web/store.js"].entity;
        for rows in [vec![], vec![&sources["web/view.js"].entity]] {
            let answer = references_answer(focal, &rows);
            let mut observed = inventory_observation(&graph);
            observed.qualify_for_answer("find_references", &answer, &graph);
            let scope = observed.binding_scope.as_ref().unwrap();
            assert_eq!(scope.outstanding_local_binding_obligations, Some(1));
            assert_eq!(scope.unrelated_obligations, Some(1), "{scope:?}");
            assert!(
                binding_factor(&observed)
                    .is_some_and(|factor| factor.starts_with("local_binding_unrelated:")),
                "{observed:?}"
            );
            // The stdio finalizer reads the same fold back from the payload,
            // counts it beside the answer, and never licenses an absence.
            let value = payload(&crate::envelope::finalize(
                disclose(answer.clone(), &observed),
                healthy_envelope(),
                "find_references",
            ));
            let verdict = &value["_kin"]["verdict"];
            assert_ne!(verdict["state"], "certified", "{value}");
            assert_eq!(verdict["safe_to_conclude_absent"], false, "{value}");
            assert!(
                verdict["limiting_factor"]
                    .as_str()
                    .unwrap()
                    .contains("local_binding_unrelated"),
                "{value}"
            );
            assert!(
                !verdict["limiting_factor"]
                    .as_str()
                    .unwrap()
                    .contains("local_binding_outstanding"),
                "{value}"
            );
        }
    }

    #[test]
    fn binding_debt_that_spells_an_answer_name_or_sits_in_a_listed_row_stays_open() {
        // A binding made through or to the focal's own name keeps the debt.
        for (source_name, target_name) in [
            ("containsElement", "format"),
            ("fmt", "containsElement"),
            ("fmt", "utils.containsElement"),
        ] {
            let (graph, sources) = attributed_debt_store(source_name, target_name);
            let answer = references_answer(&sources["web/store.js"].entity, &[]);
            let mut observed = inventory_observation(&graph);
            observed.qualify_for_answer("find_references", &answer, &graph);
            assert_eq!(
                observed
                    .binding_scope
                    .as_ref()
                    .unwrap()
                    .unrelated_obligations,
                None,
                "{source_name} -> {target_name}"
            );
            assert!(binding_factor(&observed)
                .is_some_and(|factor| factor.starts_with("local_binding_outstanding:")));
        }
        // So does one inside the code of a row the answer lists.
        let (graph, sources) = attributed_debt_store("fmt", "format");
        let answer = references_answer(
            &sources["web/store.js"].entity,
            &[&sources["web/util.js"].entity],
        );
        let mut observed = inventory_observation(&graph);
        observed.qualify_for_answer("find_references", &answer, &graph);
        assert_eq!(
            observed
                .binding_scope
                .as_ref()
                .unwrap()
                .unrelated_obligations,
            None
        );
        assert!(binding_factor(&observed)
            .is_some_and(|factor| factor.starts_with("local_binding_outstanding:")));
        // A list that is not the whole set proves nothing about the rest.
        let answer = references_answer(&sources["web/store.js"].entity, &[]);
        let mut observed = inventory_observation(&graph);
        let report = observed.report.as_mut().unwrap();
        report.local_binding_obligations.clear();
        report.local_binding_obligations_capped = true;
        observed.qualify_for_answer("find_references", &answer, &graph);
        assert!(binding_factor(&observed)
            .is_some_and(|factor| factor.starts_with("local_binding_outstanding:")));
        // An obligation no single declaration owns could sit in any row.
        let mut observed = inventory_observation(&graph);
        observed.report.as_mut().unwrap().local_binding_obligations[0].owner = None;
        observed.qualify_for_answer("find_references", &answer, &graph);
        assert!(binding_factor(&observed)
            .is_some_and(|factor| factor.starts_with("local_binding_outstanding:")));
    }
}
