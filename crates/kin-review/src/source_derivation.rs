// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Source binding is an observation about admitted bytes and parser evidence.
//! It does not prove that every declaration, import, dispatch or answer is complete.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use kin_db::SourceDerivationFacts;
use kin_model::{ArtifactId, GraphNodeId, Hash256, Relation, RepoPath, TreeEntry};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceBinding {
    Current,
    Stale,
    Unproven,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DerivationCoverage {
    Complete,
    Incomplete,
    Unproven,
}

/// Absence of recorded prior-local debt is not proof of general resolution.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PriorLocalBindingStatus {
    NoRecordedDebt,
    Outstanding,
    #[default]
    Unproven,
}

pub(crate) enum ExactBindingRecord<'a> {
    Unavailable,
    Absent,
    Present(&'a Relation),
}

pub(crate) struct LocalBindingObservation {
    pub status: PriorLocalBindingStatus,
    pub count: Option<usize>,
    pub reason: Option<String>,
    /// Canonically validated debt, retained only for exact owner attribution.
    pub debt: Option<kin_model::binding_debt::LocalBindingDebt>,
}

impl LocalBindingObservation {
    fn unproven(reason: &str) -> Self {
        Self {
            status: PriorLocalBindingStatus::Unproven,
            count: None,
            reason: Some(reason.chars().take(256).collect()),
            debt: None,
        }
    }
}

/// Exact lookup and source-owned claims are both needed: neither an unrelated
/// reserved-ID occupant nor a malformed wrong-ID claim may become absence.
pub(crate) fn inspect_local_binding(
    file: &str,
    artifact: ArtifactId,
    body: Hash256,
    relations: &[&Relation],
    exact: ExactBindingRecord<'_>,
    history: &kin_model::BindingHistoryObservation,
) -> LocalBindingObservation {
    let reserved = kin_index::binding_debt::local_binding_debt_id(artifact);
    let occupant = match exact {
        ExactBindingRecord::Unavailable => {
            return LocalBindingObservation::unproven("exact binding identity lookup unavailable");
        }
        ExactBindingRecord::Absent => None,
        ExactBindingRecord::Present(relation) if relation.id == reserved => Some(relation),
        ExactBindingRecord::Present(_) => {
            return LocalBindingObservation::unproven(
                "exact lookup returned another binding identity",
            );
        }
    };
    let node = GraphNodeId::Artifact(artifact);
    let mut candidates: Vec<_> = relations
        .iter()
        .copied()
        .filter(|relation| relation.src == node || relation.id == reserved)
        .collect();
    let adjacent: Vec<_> = candidates
        .iter()
        .copied()
        .filter(|relation| relation.id == reserved)
        .collect();
    match adjacent.as_slice() {
        [] => {
            if let Some(occupant) = occupant {
                candidates.push(occupant);
            }
        }
        [found] if occupant == Some(*found) => {}
        _ => {
            return LocalBindingObservation::unproven(
                "exact binding identity disagrees with source evidence",
            )
        }
    }
    match kin_index::binding_debt::inspect_local_binding_debt(
        &kin_model::FilePathId::new(file),
        artifact,
        body,
        &candidates,
    ) {
        Ok(None)
            if !matches!(
                history,
                kin_model::BindingHistoryObservation::Checked { .. }
            ) =>
        {
            LocalBindingObservation::unproven(
                "prior binding history has no checked authority-bound witness",
            )
        }
        Ok(None) => LocalBindingObservation {
            status: PriorLocalBindingStatus::NoRecordedDebt,
            count: Some(0),
            reason: None,
            debt: None,
        },
        Ok(Some(debt)) => LocalBindingObservation {
            status: PriorLocalBindingStatus::Outstanding,
            count: Some(debt.obligations.len()),
            reason: Some("prior local bindings remain outstanding".into()),
            debt: Some(debt),
        },
        Err(error) => LocalBindingObservation::unproven(&error),
    }
}

/// The key under which [`SourceDerivationReport::local_binding_by_language`]
/// records binding state no language names: a source whose path names no
/// adapter language or is not UTF-8, and the inventory's own missing witness.
/// It bears on an answer in every language.
pub const UNATTRIBUTED_LANGUAGE: &str = "unattributed";

/// The key [`SourceDerivationReport::local_binding_by_language`] records a
/// language under: its serialized name, as `edge_coverage.language` spells it.
pub fn language_key(language: kin_model::LanguageId) -> String {
    serde_json::to_value(language)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| format!("{language:?}"))
}

/// The [`language_key`] of the source at `path`, by the extension its adapter
/// is chosen by, or [`UNATTRIBUTED_LANGUAGE`] when no adapter language is named.
fn path_language_key(path: Option<&str>) -> String {
    path.and_then(kin_model::language_of_path)
        .map(language_key)
        .unwrap_or_else(|| UNATTRIBUTED_LANGUAGE.to_string())
}

/// Prior-local binding state of the admitted sources of one language.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LanguageBinding {
    pub prior_local_binding: PriorLocalBindingStatus,
    /// Null when some source of this language could not be inspected.
    pub outstanding_local_binding_obligations: Option<usize>,
    /// Sources of this language carrying outstanding or unproven binding.
    pub files: usize,
}

impl LanguageBinding {
    fn clear() -> Self {
        Self {
            prior_local_binding: PriorLocalBindingStatus::NoRecordedDebt,
            outstanding_local_binding_obligations: Some(0),
            files: 0,
        }
    }

    fn absorb(&mut self, status: PriorLocalBindingStatus, count: Option<usize>) {
        self.prior_local_binding = merge_binding(self.prior_local_binding, status);
        self.outstanding_local_binding_obligations = self
            .outstanding_local_binding_obligations
            .zip(count)
            .and_then(|(left, right)| left.checked_add(right));
    }
}

/// The same admitted binding obligations partitioned by proven current owner.
/// Unattributed obligations remain in every caller-role scope. Excluding a
/// role changes an answer's scope, never the repository's binding debt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BindingRolePartition {
    pub test: LanguageBinding,
    pub non_test: LanguageBinding,
    pub unattributed: LanguageBinding,
    /// Distinct sources retained when Test obligations are excluded. Summing
    /// the two role buckets' source counts would double-count mixed sources.
    pub retained_files: usize,
}

impl BindingRolePartition {
    fn clear() -> Self {
        Self {
            test: LanguageBinding::clear(),
            non_test: LanguageBinding::clear(),
            unattributed: LanguageBinding::clear(),
            retained_files: 0,
        }
    }

    fn unattributed(status: PriorLocalBindingStatus, count: Option<usize>, source: bool) -> Self {
        let mut partition = Self::clear();
        partition.unattributed.absorb(status, count);
        partition.unattributed.files = usize::from(source);
        partition.retained_files = usize::from(source);
        partition
    }

    fn absorb(&mut self, other: &Self) {
        self.retained_files = self.retained_files.saturating_add(other.retained_files);
        for (target, incoming) in [
            (&mut self.test, &other.test),
            (&mut self.non_test, &other.non_test),
            (&mut self.unattributed, &other.unattributed),
        ] {
            target.absorb(
                incoming.prior_local_binding,
                incoming.outstanding_local_binding_obligations,
            );
            target.files = target.files.saturating_add(incoming.files);
        }
    }

    fn reading(&self, include_tests: bool) -> LanguageBinding {
        let mut reading = LanguageBinding::clear();
        for entry in [&self.non_test, &self.unattributed]
            .into_iter()
            .chain(include_tests.then_some(&self.test))
        {
            reading.absorb(
                entry.prior_local_binding,
                entry.outstanding_local_binding_obligations,
            );
            // This count describes role-source memberships, not distinct sources.
            reading.files = reading.files.saturating_add(entry.files);
        }
        if !include_tests {
            reading.files = self.retained_files;
        }
        reading
    }
}

/// Attribute only an unchanged, canonically observed occurrence to its exact
/// current declaration. Identity alone can survive a source edit, so changed,
/// missing or ambiguous ownership remains in the unattributed bucket.
fn binding_roles(
    binding: &LocalBindingObservation,
    owners: &HashMap<kin_model::EntityId, Vec<&kin_db::SourceEntityBinding>>,
    file: &str,
    body: Hash256,
    source_current_and_parsed: bool,
    history: &kin_model::BindingHistoryObservation,
) -> (BindingRolePartition, Vec<OutstandingBinding>) {
    let language = path_language_key(Some(file));
    let item = |obligation: &kin_model::binding_debt::LocalBindingObligation,
                role: ObligationRole,
                owner: Option<kin_model::EntityId>| OutstandingBinding {
        language: language.clone(),
        role,
        owner,
        source_name: obligation.source_name.clone(),
        target_name: obligation.target_name.clone(),
    };
    let unattributed = || {
        (
            BindingRolePartition::unattributed(binding.status, binding.count, true),
            binding
                .debt
                .as_ref()
                .map(|debt| {
                    debt.obligations
                        .iter()
                        .map(|obligation| item(obligation, ObligationRole::Unattributed, None))
                        .collect()
                })
                .unwrap_or_default(),
        )
    };
    let Some(debt) = binding.debt.as_ref().filter(|debt| {
        source_current_and_parsed
            && matches!(
                history,
                kin_model::BindingHistoryObservation::Checked { .. }
            )
            && debt.source_file.0 == file
            && debt.observed_source_digest == body
    }) else {
        return unattributed();
    };
    let mut partition = BindingRolePartition::clear();
    let mut items = Vec::with_capacity(debt.obligations.len());
    for obligation in &debt.obligations {
        let owner =
            obligation
                .retired_relation
                .src
                .as_entity()
                .and_then(|id| owners.get(&id))
                .and_then(|owners| match owners.as_slice() {
                    [owner] => Some(*owner),
                    _ => None,
                })
                .filter(|owner| {
                    if owner.file.0 != file
                        || owner.digest != Some(body)
                        || obligation.source_digest != body
                        || obligation
                            .prior_source_file
                            .as_ref()
                            .is_some_and(|prior| prior.0 != file)
                    {
                        return false;
                    }
                    let Some((start, end)) = owner.declaration_bytes else {
                        return false;
                    };
                    // Resolution/provenance markers carry no occurrences and need
                    // no span. A claimed occurrence without an owner-bound span
                    // cannot be assigned to an excluded caller role.
                    if obligation.retired_relation.evidence.iter().any(|evidence| {
                        evidence.occurrence_count > 0 && evidence.source_span.is_none()
                    }) {
                        return false;
                    }
                    let mut sites = obligation
                        .retired_relation
                        .evidence
                        .iter()
                        .filter_map(|evidence| evidence.source_span.as_ref())
                        .peekable();
                    sites.peek().is_some()
                        && sites.all(|site| {
                            site.file.0 == file
                                && start <= site.start_byte
                                && site.start_byte < site.end_byte
                                && site.end_byte <= end
                        })
                });
        let (bucket, role) = match owner.map(|owner| owner.role) {
            Some(kin_model::EntityRole::Test) => (&mut partition.test, ObligationRole::Test),
            Some(_) => (&mut partition.non_test, ObligationRole::NonTest),
            None => (&mut partition.unattributed, ObligationRole::Unattributed),
        };
        bucket.absorb(PriorLocalBindingStatus::Outstanding, Some(1));
        bucket.files = 1;
        items.push(item(obligation, role, owner.map(|owner| owner.entity_id)));
    }
    partition.retained_files =
        usize::from(partition.non_test.files > 0 || partition.unattributed.files > 0);
    (partition, items)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceDerivationIssue {
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceDerivationReport {
    pub inventory: String,
    pub body_binding: SourceBinding,
    pub parse_coverage: DerivationCoverage,
    pub call_extraction: DerivationCoverage,
    pub import_resolution: DerivationCoverage,
    /// A missing field from an older reader is unproven, never no-debt evidence.
    #[serde(default)]
    pub prior_local_binding: PriorLocalBindingStatus,
    pub outstanding_local_binding_obligations: Option<usize>,
    /// The two fields above split by the language of the source that carries
    /// the binding state, listing only languages with outstanding or unproven
    /// binding, and [`UNATTRIBUTED_LANGUAGE`] for what no language names. An
    /// answer weighs only the languages that could refer to what it is about.
    /// `None` from a producer that did not split it, or an inspection that did
    /// not run: a reader then weighs the whole inventory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_binding_by_language: Option<BTreeMap<String, LanguageBinding>>,
    /// Optional exact-owner attribution of the same language totals. Older or
    /// incomplete producers cannot narrow binding debt by caller role.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_binding_by_language_and_role: Option<BTreeMap<String, BindingRolePartition>>,
    /// Null when inspection did not complete; zero is a measured empty inventory.
    pub full_adapter_sources: Option<usize>,
    pub excluded_artifacts: Option<usize>,
    pub issue_count: usize,
    pub issues: Vec<SourceDerivationIssue>,
    /// The old live call-shape proof, preserved independently of import resolution.
    pub call_shape_parse_coverage_complete: bool,
    /// Regions where a checked file's parse recovered from a syntax error.
    /// Each file's certificate lists them.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub parse_error_regions: usize,
    /// Call sites in checked files that no named call relation carries. Each
    /// file's certificate lists them with their reason.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub call_extraction_gaps: usize,
    /// Import statements in checked files that reached no repository file and
    /// are not provably external. Each file's certificate lists them.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub unresolved_imports: usize,
    /// Artifacts whose own facts exceeded the inspection limits, so they were
    /// not checked. The rest of the inventory was.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub unchecked_artifacts: usize,
    /// Up to four of those artifacts' paths.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unchecked_paths: Vec<String>,
    /// Every outstanding prior-local binding obligation, by the language and
    /// caller role of the source that carries it, the entity whose code holds
    /// it and the names involved, while the inventory holds at most
    /// [`ITEMIZED_OBLIGATIONS_MAX`]. An answer can then show that no
    /// obligation in its scope touches a row it lists or spells a name it is
    /// about. Identities and names only, never a path.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub local_binding_obligations: Vec<OutstandingBinding>,
    /// The inventory held more outstanding obligations than are itemized, so
    /// `local_binding_obligations` is not the whole set.
    #[serde(default, skip_serializing_if = "is_false")]
    pub local_binding_obligations_capped: bool,
}

/// Most outstanding binding obligations a report itemizes.
pub const ITEMIZED_OBLIGATIONS_MAX: usize = 64;

fn is_false(value: &bool) -> bool {
    !*value
}

/// The caller role of the source an obligation sits in, as the role partition
/// attributes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObligationRole {
    Test,
    NonTest,
    Unattributed,
}

/// One outstanding prior-local binding obligation, as an answer weighs it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutstandingBinding {
    /// The language key of the source that carries it, as
    /// [`SourceDerivationReport::local_binding_by_language`] spells it.
    pub language: String,
    pub role: ObligationRole,
    /// The entity whose code holds the occurrence, when its owner is exactly
    /// attributed. `None` when no single current declaration owns it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<kin_model::EntityId>,
    /// The local spelling the retired binding was made through.
    pub source_name: String,
    /// The name the retired binding pointed to.
    pub target_name: String,
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

pub(crate) struct FileDerivation {
    pub binding: SourceBinding,
    pub parse: DerivationCoverage,
    pub extraction: DerivationCoverage,
    pub imports: DerivationCoverage,
    pub complete: bool,
    pub parse_error_regions: usize,
    pub call_extraction_gaps: usize,
    pub unresolved_imports: usize,
}

pub(crate) fn full_adapter_path(path: &RepoPath) -> Result<bool, ()> {
    let path =
        kin_index::host_path_from_repo_path(std::path::Path::new(""), path).map_err(|_| ())?;
    Ok(matches!(
        kin_index::FileClassifier::classify(&path),
        kin_index::FileClassification::EntitySource
    ))
}

/// Shared proof for both the old impact predicate and bounded MCP observations.
/// A legacy certificate needs a nonempty unanimous current entity body; an empty
/// file needs the explicit factory-validated binding.
pub(crate) fn inspect_file(
    file: &str,
    artifact: ArtifactId,
    body: Hash256,
    digests: &[Option<Hash256>],
    relations: &[&Relation],
) -> FileDerivation {
    use DerivationCoverage::{Complete, Incomplete, Unproven};
    let mut binding = SourceBinding::Current;
    if digests.iter().flatten().any(|digest| *digest != body) {
        binding = SourceBinding::Stale;
    } else if digests.iter().any(Option::is_none) {
        binding = SourceBinding::Unproven;
    }
    let unanimous_current =
        !digests.is_empty() && digests.iter().all(|digest| *digest == Some(body));
    let node = GraphNodeId::Artifact(artifact);
    let mut recognized = Vec::new();
    let mut incomplete_marker = false;
    for relation in relations
        .iter()
        .copied()
        .filter(|relation| relation.src == node)
    {
        let has_rule = relation.evidence.iter().any(|evidence| {
            matches!(
                evidence.parser_rule.as_deref(),
                Some(
                    kin_index::CALL_SHAPE_PARSE_COVERAGE_FULL_V1
                        | kin_index::CALL_SHAPE_PARSE_COVERAGE_INCOMPLETE_V1
                        | kin_index::CALL_SHAPE_EXTRACTION_COVERAGE_INCOMPLETE_V1
                )
            )
        });
        incomplete_marker |= relation.evidence.iter().any(|evidence| {
            matches!(
                evidence.parser_rule.as_deref(),
                Some(
                    kin_index::CALL_SHAPE_PARSE_COVERAGE_INCOMPLETE_V1
                        | kin_index::CALL_SHAPE_EXTRACTION_COVERAGE_INCOMPLETE_V1
                )
            )
        });
        if has_rule {
            recognized.push(relation);
        }
    }
    let mut result = FileDerivation {
        binding,
        parse: Unproven,
        extraction: Unproven,
        imports: Unproven,
        complete: false,
        parse_error_regions: 0,
        call_extraction_gaps: 0,
        unresolved_imports: 0,
    };
    let [certificate] = recognized.as_slice() else {
        if result.binding != SourceBinding::Stale {
            result.binding = SourceBinding::Unproven;
        }
        return result;
    };
    // The current format carries each fact on its own. The format before it
    // is still read the way it always was, until re-derivation replaces it.
    let detail = if kin_index::is_parse_coverage_relation(certificate, file, artifact) {
        kin_index::coverage_certificate::CertificateDetail::from_certificate(certificate)
    } else {
        None
    };
    if detail.is_none()
        && !kin_index::is_previous_format_parse_coverage_relation(certificate, file, artifact)
    {
        if result.binding != SourceBinding::Stale {
            result.binding = SourceBinding::Unproven;
        }
        return result;
    }
    // Located by label, never by position: the certificate grew a
    // base-resolution entry between the call and source-digest ones, and a
    // positional read would answer about the wrong entry the day it did.
    let is_full =
        kin_index::coverage_evidence(certificate, kin_index::CALL_SHAPE_PARSE_COVERAGE_FULL_V1)
            .is_some();
    let certificate_current = match kin_index::parse_coverage_source_digest(certificate) {
        Some(digest) if digest == body => true,
        Some(_) => {
            result.binding = SourceBinding::Stale;
            false
        }
        None if is_full && unanimous_current => true,
        None => {
            if result.binding != SourceBinding::Stale {
                result.binding = SourceBinding::Unproven;
            }
            false
        }
    };
    if certificate_current {
        if let Some(detail) = &detail {
            read_detail(&mut result, certificate, detail, is_full);
        } else if is_full {
            result.parse = Complete;
            result.extraction = Complete;
        } else if kin_index::coverage_evidence(
            certificate,
            kin_index::CALL_SHAPE_PARSE_COVERAGE_INCOMPLETE_V1,
        )
        .is_some()
        {
            result.parse = Incomplete;
            result.extraction = Unproven;
        } else if kin_index::coverage_evidence(
            certificate,
            kin_index::CALL_SHAPE_EXTRACTION_COVERAGE_INCOMPLETE_V1,
        )
        .is_some()
        {
            result.extraction = Incomplete;
        }
        let Some(imports) =
            kin_index::coverage_evidence(certificate, kin_index::IMPORT_RESOLUTION_COVERAGE_V1)
        else {
            return result;
        };
        if detail.is_some() {
            // Set by `read_detail`, from each import's own outcome.
        } else {
            result.imports = if imports
                .token
                .as_deref()
                .and_then(|token| token.parse::<u32>().ok())
                == Some(imports.occurrence_count)
            {
                Complete
            } else {
                Incomplete
            };
        }
    }
    result.complete = result.binding == SourceBinding::Current
        && certificate_current
        && is_full
        && !incomplete_marker;
    result
}

/// Read a current-format certificate's per-fact entries.
///
/// Parse completeness comes from its own entry, so a file whose parse was
/// clean reads complete however its calls came out. Call extraction is
/// complete when every call site the adapter counted is carried by a named call
/// relation; a named call whose receiver the parser could not type is a
/// resolution question the language-server sweep settles, not an extraction
/// gap. An adapter that tracks no gaps keeps the combined reading. Imports are
/// complete when every statement either reached a repository file or is
/// provably external.
fn read_detail(
    result: &mut FileDerivation,
    certificate: &kin_model::Relation,
    detail: &kin_index::coverage_certificate::CertificateDetail,
    is_full: bool,
) {
    use DerivationCoverage::{Complete, Incomplete, Unproven};
    result.parse = match &detail.parse {
        Some(kin_model::ParseCompleteness::Full) => Complete,
        Some(_) => Incomplete,
        None => Unproven,
    };
    result.parse_error_regions = detail.parse_errors;
    result.extraction = match detail.extraction_complete() {
        Some(true) => Complete,
        Some(false) => Incomplete,
        None if is_full => Complete,
        None if kin_index::coverage_evidence(
            certificate,
            kin_index::CALL_SHAPE_EXTRACTION_COVERAGE_INCOMPLETE_V1,
        )
        .is_some() =>
        {
            Incomplete
        }
        None => Unproven,
    };
    result.call_extraction_gaps = detail
        .call_extraction
        .as_ref()
        .map_or(0, |extraction| extraction.gaps.len());
    result.unresolved_imports = detail.unresolved_imports.len();
    // Every statement must be accounted for as resolved or provably external.
    // Counts that do not add up are not a measurement of completeness.
    let accounted =
        kin_index::coverage_evidence(certificate, kin_index::IMPORT_RESOLUTION_COVERAGE_V1)
            .and_then(|imports| {
                let resolved: usize = imports.token.as_deref()?.parse().ok()?;
                Some(resolved + detail.imports_external == imports.occurrence_count as usize)
            })
            .unwrap_or(false);
    result.imports = if accounted && detail.unresolved_imports.is_empty() {
        Complete
    } else {
        Incomplete
    };
}

impl SourceDerivationReport {
    pub fn unproven(reason: &str) -> Self {
        let mut report = Self::empty();
        report.body_binding = SourceBinding::Unproven;
        report.full_adapter_sources = None;
        report.excluded_artifacts = None;
        report.parse_coverage = DerivationCoverage::Unproven;
        report.call_extraction = DerivationCoverage::Unproven;
        report.import_resolution = DerivationCoverage::Unproven;
        report.prior_local_binding = PriorLocalBindingStatus::Unproven;
        report.outstanding_local_binding_obligations = None;
        report.local_binding_by_language = None;
        report.local_binding_by_language_and_role = None;
        report.call_shape_parse_coverage_complete = false;
        report.issue(reason, None);
        report
    }
    fn empty() -> Self {
        Self {
            inventory: "graph_admitted_full_adapter_sources".into(),
            body_binding: SourceBinding::Current,
            parse_coverage: DerivationCoverage::Complete,
            call_extraction: DerivationCoverage::Complete,
            import_resolution: DerivationCoverage::Complete,
            prior_local_binding: PriorLocalBindingStatus::NoRecordedDebt,
            outstanding_local_binding_obligations: Some(0),
            local_binding_by_language: Some(BTreeMap::new()),
            local_binding_by_language_and_role: Some(BTreeMap::new()),
            full_adapter_sources: Some(0),
            excluded_artifacts: Some(0),
            issue_count: 0,
            issues: Vec::new(),
            call_shape_parse_coverage_complete: true,
            parse_error_regions: 0,
            call_extraction_gaps: 0,
            unresolved_imports: 0,
            unchecked_artifacts: 0,
            unchecked_paths: Vec::new(),
            local_binding_obligations: Vec::new(),
            local_binding_obligations_capped: false,
        }
    }

    /// Itemize `obligations`, or mark the list capped once the inventory holds
    /// more than [`ITEMIZED_OBLIGATIONS_MAX`].
    fn itemize(&mut self, obligations: Vec<OutstandingBinding>) {
        if self.local_binding_obligations_capped || obligations.is_empty() {
            return;
        }
        if self.local_binding_obligations.len() + obligations.len() > ITEMIZED_OBLIGATIONS_MAX {
            self.local_binding_obligations.clear();
            self.local_binding_obligations_capped = true;
            return;
        }
        self.local_binding_obligations.extend(obligations);
    }
    fn issue(&mut self, reason: &str, path: Option<&str>) {
        self.issue_count += 1;
        if self.issues.len() < 4 {
            self.issues.push(SourceDerivationIssue {
                reason: reason.into(),
                path: path.map(|path| path.chars().take(256).collect()),
            });
        }
    }

    /// Merge one source's prior-local binding reading, or the inventory's
    /// own when `source` is `None`, into the whole-inventory fields and into
    /// the language `key` names.
    fn record_binding(
        &mut self,
        key: String,
        status: PriorLocalBindingStatus,
        count: Option<usize>,
        source: bool,
    ) {
        self.record_binding_roles(key, status, count, source, None);
    }

    fn record_binding_roles(
        &mut self,
        key: String,
        status: PriorLocalBindingStatus,
        count: Option<usize>,
        source: bool,
        roles: Option<BindingRolePartition>,
    ) {
        self.prior_local_binding = merge_binding(self.prior_local_binding, status);
        self.outstanding_local_binding_obligations = self
            .outstanding_local_binding_obligations
            .zip(count)
            .and_then(|(left, right)| left.checked_add(right));
        if status == PriorLocalBindingStatus::NoRecordedDebt && count == Some(0) {
            return;
        }
        if let Some(split) = &mut self.local_binding_by_language {
            let entry = split
                .entry(key.clone())
                .or_insert_with(LanguageBinding::clear);
            entry.absorb(status, count);
            entry.files += usize::from(source);
        }
        if let Some(split) = &mut self.local_binding_by_language_and_role {
            let roles =
                roles.unwrap_or_else(|| BindingRolePartition::unattributed(status, count, source));
            let entry = split.entry(key).or_insert_with(BindingRolePartition::clear);
            entry.absorb(&roles);
        }
    }

    /// The validated language readings for a query excluding stored Test-role
    /// callers. None keeps the all-role reading: absent or inconsistent splits
    /// never remove debt. Global totals and the persisted debt remain unchanged.
    pub fn binding_languages_excluding_tests(&self) -> Option<BTreeMap<String, LanguageBinding>> {
        let all = self.local_binding_by_language.as_ref()?;
        let roles = self.local_binding_by_language_and_role.as_ref()?;
        if all.len() != roles.len() {
            return None;
        }
        let mut whole = LanguageBinding::clear();
        let mut scoped = BTreeMap::new();
        for (language, expected) in all {
            let partition = roles.get(language)?;
            for bucket in [
                &partition.test,
                &partition.non_test,
                &partition.unattributed,
            ] {
                match (
                    bucket.prior_local_binding,
                    bucket.outstanding_local_binding_obligations,
                ) {
                    (PriorLocalBindingStatus::NoRecordedDebt, Some(0))
                    | (PriorLocalBindingStatus::Unproven, None)
                    | (PriorLocalBindingStatus::Outstanding, None) => {}
                    (PriorLocalBindingStatus::Outstanding, Some(count)) if count > 0 => {}
                    _ => return None,
                }
            }
            // Only a measured number of Test-owned obligations can be excluded.
            match (
                partition.test.prior_local_binding,
                partition.test.outstanding_local_binding_obligations,
            ) {
                (PriorLocalBindingStatus::NoRecordedDebt, Some(0)) => {}
                (PriorLocalBindingStatus::Outstanding, Some(count)) if count > 0 => {}
                _ => return None,
            }
            let total = partition.reading(true);
            if total.prior_local_binding != expected.prior_local_binding
                || total.outstanding_local_binding_obligations
                    != expected.outstanding_local_binding_obligations
            {
                return None;
            }
            whole.absorb(
                expected.prior_local_binding,
                expected.outstanding_local_binding_obligations,
            );
            let remaining = partition.reading(false);
            if remaining.prior_local_binding != PriorLocalBindingStatus::NoRecordedDebt
                || remaining.outstanding_local_binding_obligations != Some(0)
            {
                scoped.insert(language.clone(), remaining);
            }
        }
        (whole.prior_local_binding == self.prior_local_binding
            && whole.outstanding_local_binding_obligations
                == self.outstanding_local_binding_obligations)
            .then_some(scoped)
    }
}

fn merge_coverage(left: DerivationCoverage, right: DerivationCoverage) -> DerivationCoverage {
    use DerivationCoverage::*;
    match (left, right) {
        (Incomplete, _) | (_, Incomplete) => Incomplete,
        (Unproven, _) | (_, Unproven) => Unproven,
        _ => Complete,
    }
}

/// Evaluate already bounded graph facts. The producer states whether these facts
/// cover the whole inventory or selected paths; a clean subset is not a whole
/// repository assertion.
pub fn inspect_source_derivation(facts: &SourceDerivationFacts) -> SourceDerivationReport {
    let mut inspector = SourceDerivationInspector::new();
    inspector.absorb(facts);
    inspector.finish()
}

/// Accumulates one inspection over facts that arrive in parts, such as the
/// per-artifact items of [`kin_db::InMemoryGraph::for_each_source_derivation_artifact`].
/// Feeding it every part of an inventory yields the report one single-copy
/// read of that inventory would, without holding the whole inventory's facts.
pub struct SourceDerivationInspector {
    report: SourceDerivationReport,
    sources: BTreeSet<String>,
    binding_history: kin_model::BindingHistoryObservation,
}

impl Default for SourceDerivationInspector {
    fn default() -> Self {
        Self::new()
    }
}

impl SourceDerivationInspector {
    pub fn new() -> Self {
        Self {
            report: SourceDerivationReport::empty(),
            sources: BTreeSet::new(),
            binding_history: kin_model::BindingHistoryObservation::Unproven,
        }
    }

    /// One artifact the inspection could not check because its own facts
    /// exceeded a limit. The inventory's other artifacts still count, and this
    /// one is named rather than refusing them all.
    pub fn unchecked(&mut self, path: &kin_model::RepoPath, limit: kin_db::SourceDerivationLimit) {
        let report = &mut self.report;
        report.unchecked_artifacts += 1;
        if report.unchecked_paths.len() < 4 {
            report.unchecked_paths.push(
                path.as_utf8()
                    .unwrap_or("<non-utf8 path>")
                    .chars()
                    .take(256)
                    .collect(),
            );
        }
        if let Some(count) = &mut report.full_adapter_sources {
            *count += 1;
        }
        if report.body_binding != SourceBinding::Stale {
            report.body_binding = SourceBinding::Unproven;
        }
        report.parse_coverage = merge_coverage(report.parse_coverage, DerivationCoverage::Unproven);
        report.call_extraction =
            merge_coverage(report.call_extraction, DerivationCoverage::Unproven);
        report.import_resolution =
            merge_coverage(report.import_resolution, DerivationCoverage::Unproven);
        report.call_shape_parse_coverage_complete = false;
        report.record_binding(
            path_language_key(path.as_utf8()),
            PriorLocalBindingStatus::Unproven,
            None,
            true,
        );
        report.issue(
            &format!("source derivation inspection of one artifact exceeded its {limit:?} limit"),
            path.as_utf8(),
        );
    }

    /// The graph's binding-history observation, for an inspection whose
    /// parts may be empty.
    pub fn set_binding_history(&mut self, history: kin_model::BindingHistoryObservation) {
        self.binding_history = history;
    }

    pub fn finish(mut self) -> SourceDerivationReport {
        let report = &mut self.report;
        if report.full_adapter_sources == Some(0)
            && !matches!(
                self.binding_history,
                kin_model::BindingHistoryObservation::Checked { .. }
            )
        {
            report.record_binding(
                UNATTRIBUTED_LANGUAGE.to_string(),
                PriorLocalBindingStatus::Unproven,
                None,
                false,
            );
            report.issue(
                "prior binding history has no checked authority-bound witness",
                None,
            );
        }
        self.report
    }

    /// Absorb one part of the inventory.
    pub fn absorb(&mut self, facts: &SourceDerivationFacts) {
        self.binding_history = facts.binding_history.clone();
        let report = &mut self.report;
        let sources = &mut self.sources;
        let mut entities: BTreeMap<&str, Vec<Option<Hash256>>> = BTreeMap::new();
        let mut owners: HashMap<kin_model::EntityId, Vec<&kin_db::SourceEntityBinding>> =
            HashMap::new();
        for entity in &facts.entities {
            owners.entry(entity.entity_id).or_default().push(entity);
            entities
                .entry(&entity.file.0)
                .or_default()
                .push(entity.digest);
        }
        let opaque: BTreeMap<_, _> = facts
            .opaque
            .iter()
            .map(|fact| (fact.file.0.as_str(), fact.hash))
            .collect();
        let mut relations: HashMap<_, Vec<_>> = HashMap::new();
        for relation in &facts.relations {
            relations.entry(relation.src).or_default().push(relation);
        }
        let mut reservations_by_artifact: HashMap<_, Vec<_>> = HashMap::new();
        for reserved in &facts.reserved_relations {
            reservations_by_artifact
                .entry(reserved.artifact)
                .or_default()
                .push(reserved);
        }
        for artifact in &facts.artifacts {
            let TreeEntry::Blob { hash, .. } = artifact.entry else {
                if let Some(count) = &mut report.excluded_artifacts {
                    *count += 1;
                }
                continue;
            };
            match full_adapter_path(&artifact.path) {
                Ok(false) => {
                    if let Some(count) = &mut report.excluded_artifacts {
                        *count += 1;
                    }
                    continue;
                }
                Err(()) => {
                    report.full_adapter_sources = None;
                    report.excluded_artifacts = None;
                    if report.body_binding != SourceBinding::Stale {
                        report.body_binding = SourceBinding::Unproven;
                    }
                    report.parse_coverage =
                        merge_coverage(report.parse_coverage, DerivationCoverage::Unproven);
                    report.call_extraction =
                        merge_coverage(report.call_extraction, DerivationCoverage::Unproven);
                    report.import_resolution =
                        merge_coverage(report.import_resolution, DerivationCoverage::Unproven);
                    report.call_shape_parse_coverage_complete = false;
                    report.record_binding(
                        UNATTRIBUTED_LANGUAGE.to_string(),
                        PriorLocalBindingStatus::Unproven,
                        None,
                        true,
                    );
                    report.issue("source_path_unrepresentable", None);
                    continue;
                }
                Ok(true) => {}
            }
            let Some(file) = artifact.path.as_utf8() else {
                if let Some(count) = &mut report.full_adapter_sources {
                    *count += 1;
                }
                if report.body_binding != SourceBinding::Stale {
                    report.body_binding = SourceBinding::Unproven;
                }
                report.parse_coverage =
                    merge_coverage(report.parse_coverage, DerivationCoverage::Unproven);
                report.call_extraction =
                    merge_coverage(report.call_extraction, DerivationCoverage::Unproven);
                report.import_resolution =
                    merge_coverage(report.import_resolution, DerivationCoverage::Unproven);
                report.call_shape_parse_coverage_complete = false;
                report.record_binding(
                    UNATTRIBUTED_LANGUAGE.to_string(),
                    PriorLocalBindingStatus::Unproven,
                    None,
                    true,
                );
                report.issue("source_path_not_utf8", None);
                continue;
            };
            if opaque.get(file) == Some(&hash) {
                if let Some(count) = &mut report.excluded_artifacts {
                    *count += 1;
                }
                continue;
            }
            if let Some(count) = &mut report.full_adapter_sources {
                *count += 1;
            }
            sources.insert(file.to_string());
            let empty = Vec::new();
            let inspected = inspect_file(
                file,
                artifact.artifact_id,
                hash,
                entities.get(file).map(Vec::as_slice).unwrap_or_default(),
                relations
                    .get(&GraphNodeId::Artifact(artifact.artifact_id))
                    .unwrap_or(&empty),
            );
            let exact = match reservations_by_artifact
                .get(&artifact.artifact_id)
                .map(Vec::as_slice)
                .unwrap_or_default()
            {
                [reserved]
                    if reserved.id
                        == kin_index::binding_debt::local_binding_debt_id(artifact.artifact_id) =>
                {
                    match &reserved.relation {
                        Some(relation) => ExactBindingRecord::Present(relation),
                        None => ExactBindingRecord::Absent,
                    }
                }
                _ => ExactBindingRecord::Unavailable,
            };
            let binding = inspect_local_binding(
                file,
                artifact.artifact_id,
                hash,
                relations
                    .get(&GraphNodeId::Artifact(artifact.artifact_id))
                    .unwrap_or(&empty),
                exact,
                &facts.binding_history,
            );
            let (roles, obligations) = binding_roles(
                &binding,
                &owners,
                file,
                hash,
                inspected.binding == SourceBinding::Current
                    && inspected.parse == DerivationCoverage::Complete,
                &facts.binding_history,
            );
            report.itemize(obligations);
            report.record_binding_roles(
                path_language_key(Some(file)),
                binding.status,
                binding.count,
                true,
                Some(roles),
            );
            if let Some(reason) = binding.reason {
                report.issue(
                    &format!(
                        "local_binding_{}: {reason}",
                        if binding.status == PriorLocalBindingStatus::Outstanding {
                            "outstanding"
                        } else {
                            "unproven"
                        }
                    ),
                    Some(file),
                );
            }
            if inspected.binding != SourceBinding::Current {
                if report.body_binding != SourceBinding::Stale {
                    report.body_binding = inspected.binding;
                }
                report.issue(
                    if inspected.binding == SourceBinding::Stale {
                        "derived_source_digest_mismatch"
                    } else {
                        "derived_source_binding_unproven"
                    },
                    Some(file),
                );
            }
            report.parse_coverage = merge_coverage(report.parse_coverage, inspected.parse);
            report.call_extraction = merge_coverage(report.call_extraction, inspected.extraction);
            report.import_resolution = merge_coverage(report.import_resolution, inspected.imports);
            report.call_shape_parse_coverage_complete &= inspected.complete;
            report.parse_error_regions += inspected.parse_error_regions;
            report.call_extraction_gaps += inspected.call_extraction_gaps;
            report.unresolved_imports += inspected.unresolved_imports;
        }
        for file in entities
            .keys()
            .copied()
            .chain(facts.layouts.iter().map(|layout| layout.file.0.as_str()))
        {
            if !sources.contains(file) {
                report.body_binding = SourceBinding::Stale;
                report.parse_coverage =
                    merge_coverage(report.parse_coverage, DerivationCoverage::Unproven);
                report.call_extraction =
                    merge_coverage(report.call_extraction, DerivationCoverage::Unproven);
                report.import_resolution =
                    merge_coverage(report.import_resolution, DerivationCoverage::Unproven);
                report.call_shape_parse_coverage_complete = false;
                report.record_binding(
                    path_language_key(Some(file)),
                    PriorLocalBindingStatus::Unproven,
                    None,
                    true,
                );
                report.issue("held_source_outside_admitted_inventory", Some(file));
            }
        }
        if facts.layouts.iter().any(|layout| !layout.full) {
            report.parse_coverage = DerivationCoverage::Incomplete;
            report.call_shape_parse_coverage_complete = false;
        }
        for path in &facts.missing_paths {
            if let Ok(true) = full_adapter_path(path) {
                if report.body_binding != SourceBinding::Stale {
                    report.body_binding = SourceBinding::Unproven;
                }
                report.parse_coverage =
                    merge_coverage(report.parse_coverage, DerivationCoverage::Unproven);
                report.call_extraction =
                    merge_coverage(report.call_extraction, DerivationCoverage::Unproven);
                report.import_resolution =
                    merge_coverage(report.import_resolution, DerivationCoverage::Unproven);
                report.call_shape_parse_coverage_complete = false;
                report.record_binding(
                    path_language_key(path.as_utf8()),
                    PriorLocalBindingStatus::Unproven,
                    None,
                    true,
                );
                report.issue("selected_source_not_admitted", path.as_utf8());
            }
        }
    }
}

/// Inspect `graph`'s whole admitted inventory one artifact at a time.
///
/// The per-artifact walk applies the inspection limits to each artifact's own
/// facts, so no repository is refused for its size; an artifact whose facts
/// alone exceed them is listed as unchecked and the rest are still inspected.
pub fn inspect_inventory(graph: &kin_db::InMemoryGraph) -> SourceDerivationReport {
    let mut inspector = SourceDerivationInspector::new();
    let history = graph.for_each_source_derivation_artifact(
        kin_db::SourceDerivationLimits::default(),
        kin_index::binding_debt::local_binding_debt_id,
        |item| match item {
            kin_db::SourceDerivationItem::Facts(facts) => inspector.absorb(&facts),
            kin_db::SourceDerivationItem::Unchecked { path, limit } => {
                inspector.unchecked(&path, limit)
            }
        },
    );
    inspector.set_binding_history(history);
    inspector.finish()
}

/// How many graphs' inventory reports [`inspect_inventory_cached`] keeps.
const INVENTORY_CACHE_GRAPHS: usize = 4;

/// [`inspect_inventory`], recorded per graph and truth epoch.
///
/// The report is a function of graph truth, and every write to graph truth
/// advances the graph's truth epoch, so a report taken at one epoch answers
/// every query at that epoch. On a large repository the inspection walks
/// thousands of artifacts; answering each query from the recorded report is
/// what lets every answer carry a whole-inventory observation. A report is
/// recorded only when the epoch did not move while it was taken, and the
/// graph is held by a weak reference, so a dropped graph's report is never
/// served to another graph at the same address.
pub fn inspect_inventory_cached(
    graph: &std::sync::Arc<kin_db::InMemoryGraph>,
) -> SourceDerivationReport {
    type Entry = (
        std::sync::Weak<kin_db::InMemoryGraph>,
        u64,
        SourceDerivationReport,
    );
    static CACHE: std::sync::Mutex<Vec<Entry>> = std::sync::Mutex::new(Vec::new());
    let before = graph.truth_epoch();
    {
        let cache = CACHE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some((_, _, report)) = cache.iter().find(|(held, epoch, _)| {
            *epoch == before
                && held
                    .upgrade()
                    .is_some_and(|held| std::sync::Arc::ptr_eq(&held, graph))
        }) {
            return report.clone();
        }
    }
    let report = inspect_inventory(graph);
    if graph.truth_epoch() == before {
        let mut cache = CACHE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        cache.retain(|(held, _, _)| {
            held.upgrade()
                .is_some_and(|held| !std::sync::Arc::ptr_eq(&held, graph))
        });
        if cache.len() >= INVENTORY_CACHE_GRAPHS {
            cache.remove(0);
        }
        cache.push((std::sync::Arc::downgrade(graph), before, report.clone()));
    }
    report
}

/// The more pessimistic of two prior-local binding readings: outstanding over
/// unproven over no recorded debt.
pub fn merge_binding(
    left: PriorLocalBindingStatus,
    right: PriorLocalBindingStatus,
) -> PriorLocalBindingStatus {
    use PriorLocalBindingStatus::*;
    match (left, right) {
        (Outstanding, _) | (_, Outstanding) => Outstanding,
        (Unproven, _) | (_, Unproven) => Unproven,
        _ => NoRecordedDebt,
    }
}

#[cfg(test)]
mod language_split_tests {
    use super::*;

    #[test]
    fn binding_state_is_attributed_to_the_language_that_carries_it() {
        let mut inspector = SourceDerivationInspector::new();
        for path in [
            "compiler/src/debug.rs",
            "compiler/src/lib.rs",
            "vendor/blob.bin",
        ] {
            inspector.unchecked(
                &RepoPath::from_utf8(path).unwrap(),
                kin_db::SourceDerivationLimit::Relations,
            );
        }
        let report = inspector.finish();
        assert_eq!(
            report.prior_local_binding,
            PriorLocalBindingStatus::Unproven
        );
        assert_eq!(report.outstanding_local_binding_obligations, None);
        let split = report.local_binding_by_language.unwrap();
        assert_eq!(
            split[&language_key(kin_model::LanguageId::Rust)],
            LanguageBinding {
                prior_local_binding: PriorLocalBindingStatus::Unproven,
                outstanding_local_binding_obligations: None,
                files: 2,
            }
        );
        // A path no adapter language names bears on every answer.
        assert_eq!(split[UNATTRIBUTED_LANGUAGE].files, 1);
        assert_eq!(split.len(), 2, "{split:?}");
    }

    #[test]
    fn an_inventory_without_a_witness_is_unattributed_and_an_unrun_one_unsplit() {
        let report = SourceDerivationInspector::new().finish();
        assert_eq!(
            report.prior_local_binding,
            PriorLocalBindingStatus::Unproven
        );
        let split = report.local_binding_by_language.unwrap();
        assert_eq!(
            split.keys().collect::<Vec<_>>(),
            vec![UNATTRIBUTED_LANGUAGE]
        );
        assert_eq!(split[UNATTRIBUTED_LANGUAGE].files, 0);
        // An inspection that did not run measured no split at all.
        assert_eq!(
            SourceDerivationReport::unproven("refused").local_binding_by_language,
            None
        );
        assert_eq!(
            language_key(kin_model::LanguageId::JavaScript),
            "JavaScript"
        );
    }
}
