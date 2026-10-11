// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Language-server queries use a captured entity/tree universe. Network work
//! holds no edit lock; buffered answers regain that lock and prove freshness
//! before reading held evidence or changing the graph.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use kin_model::{
    Entity, EntityStore, FilePathId, GraphNodeId, Relation, RepoPath, ResolvedTree, TreeEntry,
};

use super::{DaemonState, EnrichmentWrite, PendingEnrichment};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refused {
    Stale,
    UnprovenSource,
}

pub(crate) struct QueryInputs {
    pub(crate) entities: Vec<Entity>,
    tree: ResolvedTree,
    ids: HashSet<kin_model::EntityId>,
    pub(crate) marker_epoch: u64,
    source: crate::state::GraphOwnedSourceView,
    epoch: AtomicU64,
    source_failed: AtomicBool,
    /// The type answers the entity passes disproved since the last
    /// settlement, each waiting for its file to be settled (see
    /// [`Self::note_disproved_type_uses`]).
    disproved_type_uses: std::sync::Mutex<Vec<kin_lsp::enrichment::DisprovedTypeUse>>,
}

impl QueryInputs {
    pub(crate) async fn capture(state: &DaemonState) -> Result<Self, Refused> {
        let _coordinated = state.coordination_gate.lock().await;
        let epoch = state.stable_graph_authority_epoch().ok_or(Refused::Stale)?;
        let tree = state.graph.resolved_tree();
        let entities = state
            .graph
            .list_all_entities()
            .map_err(|_| Refused::UnprovenSource)?;
        // A retained last-known-good declaration must not lend its old span to
        // newly admitted, unparsed bytes. Check the whole matching universe:
        // an empty answer depends on absent targets too, not just offered edges.
        for entity in &entities {
            let (Some(file), Some(span)) = (&entity.file_origin, &entity.span) else {
                continue;
            };
            let path = RepoPath::from_utf8(file.0.clone()).map_err(|_| Refused::UnprovenSource)?;
            let Some(artifact) = tree.artifact_at_path(&path) else {
                return Err(Refused::UnprovenSource);
            };
            let TreeEntry::Blob { hash, .. } = &artifact.entry else {
                return Err(Refused::UnprovenSource);
            };
            if span.file != *file
                || span.start_byte > span.end_byte
                || entity
                    .metadata
                    .extra
                    .get("blob_hash")
                    .and_then(|v| v.as_str())
                    != Some(hash.to_string().as_str())
            {
                return Err(Refused::UnprovenSource);
            }
        }
        let source = state
            .graph_owned_source_view()
            .map_err(|_| Refused::UnprovenSource)?;
        if !state.graph_authority_epoch_is_current(epoch) {
            return Err(Refused::Stale);
        }
        Ok(Self {
            ids: entities.iter().map(|e| e.id).collect(),
            marker_epoch: super::current_marker_epoch(state),
            entities,
            tree,
            source,
            epoch: AtomicU64::new(epoch),
            source_failed: AtomicBool::new(false),
            disproved_type_uses: Default::default(),
        })
    }

    /// Hold the type answers a pass over one entity disproved until the
    /// file they were asked in is settled, which retires the evidence an
    /// earlier build minted from the same answers (see
    /// [`crate::call_site_ledger::disproved_type_uses`]).
    pub(crate) fn note_disproved_type_uses(
        &self,
        found: impl IntoIterator<Item = kin_lsp::enrichment::DisprovedTypeUse>,
    ) {
        if let Ok(mut held) = self.disproved_type_uses.lock() {
            held.extend(found);
        }
    }

    /// Take the disproved type answers asked in `file`, among entities this
    /// capture holds.
    fn take_disproved_type_uses(&self, file: &str) -> Vec<kin_lsp::enrichment::DisprovedTypeUse> {
        let Ok(mut held) = self.disproved_type_uses.lock() else {
            return Vec::new();
        };
        let (taken, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut *held)
            .into_iter()
            .partition(|answer| answer.site.file.0 == file);
        *held = kept;
        taken
            .into_iter()
            .filter(|answer| self.ids.contains(&answer.source) && self.ids.contains(&answer.target))
            .collect()
    }

    /// Whether `other` captured the same entities and the same tree as this
    /// capture, so an answer proven against one is proven against the other.
    ///
    /// Their authority epochs may differ. Every graph write moves the epoch,
    /// including a reconcile of a watcher event that changed nothing, and an
    /// epoch alone cannot tell that from a write that moved these inputs.
    pub(crate) fn describes_same_graph(&self, other: &QueryInputs) -> bool {
        self.tree == other.tree && self.ids == other.ids && self.holds_entities(&other.entities)
    }

    /// Whether `entities` are exactly the entities this capture holds, each
    /// unchanged.
    fn holds_entities(&self, entities: &[Entity]) -> bool {
        let held: std::collections::HashMap<kin_model::EntityId, &Entity> = self
            .entities
            .iter()
            .map(|entity| (entity.id, entity))
            .collect();
        held.len() == entities.len()
            && entities
                .iter()
                .all(|entity| held.get(&entity.id) == Some(&entity))
    }

    /// Take the current authority epoch as this capture's own when the writes
    /// that moved it left the graph's tree and entities exactly as captured.
    ///
    /// Every graph write moves the epoch, including a reconcile of a watcher
    /// event that changed nothing ([`Self::describes_same_graph`]), and a
    /// sweep refused as stale starts over with new language servers. A
    /// reconcile of files a fresh checkout or `kin init` had just written
    /// lands a second or so after the daemon starts, which is inside the first
    /// file of its first sweep, so that sweep was thrown away whole and asked
    /// again. Answers proven against this capture are proven against a graph
    /// that holds the same tree and the same entities, so they still stand.
    /// Called with the coordination gate held; the epoch is read before and
    /// after the comparison, so a write that lands in between refuses.
    fn adopt_epoch_if_graph_unchanged(&self, state: &DaemonState) -> Result<(), Refused> {
        let epoch = state.stable_graph_authority_epoch().ok_or(Refused::Stale)?;
        if state.graph.resolved_tree() != self.tree {
            return Err(Refused::Stale);
        }
        let entities = state
            .graph
            .list_all_entities()
            .map_err(|_| Refused::Stale)?;
        if !self.holds_entities(&entities) || !state.graph_authority_epoch_is_current(epoch) {
            return Err(Refused::Stale);
        }
        self.epoch.store(epoch, Ordering::SeqCst);
        Ok(())
    }

    /// The blob this pass's captured tree holds at `path`, which is what an
    /// owed file's backoff is keyed to.
    pub(crate) fn blob(&self, path: &str) -> Option<String> {
        let path_id = RepoPath::from_utf8(path.to_string()).ok()?;
        match &self.tree.artifact_at_path(&path_id)?.entry {
            TreeEntry::Blob { hash, .. } => Some(hash.to_string()),
            _ => None,
        }
    }

    /// Exact absence from this admitted tree, not from the entity list. An
    /// unparsed or unsupported artifact still exists and cannot retire a failure.
    pub(crate) fn path_is_absent(&self, path: &str) -> bool {
        RepoPath::from_utf8(path.to_owned())
            .is_ok_and(|path| self.tree.artifact_at_path(&path).is_none())
    }

    pub(crate) fn document(&self, path: &str) -> Option<String> {
        // A server may mention external library files. Those have no authority
        // here and are not opened; an admitted source whose CAS read fails is
        // a different outcome and disqualifies this query's completion.
        let path_id = RepoPath::from_utf8(path.to_string()).ok()?;
        self.tree.artifact_at_path(&path_id)?;
        match self
            .source
            .load_text_from_tree(&FilePathId::new(path), &self.tree)
        {
            Ok(text) => Some(text),
            Err(_) => {
                self.source_failed.store(true, Ordering::SeqCst);
                None
            }
        }
    }

    pub(super) fn validate(&self, state: &DaemonState) -> Result<(), Refused> {
        if !state.graph_authority_epoch_is_current(self.epoch.load(Ordering::SeqCst)) {
            self.adopt_epoch_if_graph_unchanged(state)?;
        }
        if self.source_failed.load(Ordering::SeqCst) {
            return Err(Refused::UnprovenSource);
        }
        Ok(())
    }

    pub(crate) async fn current(&self, state: &DaemonState) -> Result<(), Refused> {
        let _coordinated = state.coordination_gate.lock().await;
        self.validate(state)
    }

    /// Publish the context this sweep settled without invalidating its own
    /// captured source universe. Validation can follow an awaited server start,
    /// so freshness must be checked after acquiring coordination, before writing.
    pub(crate) async fn record_context_validation(
        &self,
        state: &DaemonState,
        language: kin_model::LanguageId,
        settled: kin_model::ContextValidationState,
    ) -> Result<(), Refused> {
        // A turn, not only the coordination gate: a changed validation is a
        // graph write, and it lands between read attempts rather than inside
        // one (`crate::live_read_gate`).
        let _coordinated = state.coordinate_enrichment_write().await;
        self.validate(state)?;
        let before = self.epoch.load(Ordering::SeqCst);
        // A changed validation owns exactly one authority-mutation guard:
        // entry and drop each advance once. An identical validation opens no
        // writer. Accept only those edges, never an unrelated writer's.
        let after_write = before.checked_add(2).ok_or(Refused::Stale)?;
        let opened_mutation = super::record_context_validation(state, language, settled);
        let after = if opened_mutation { after_write } else { before };
        if !state.graph_authority_epoch_is_current(after) {
            return Err(Refused::Stale);
        }
        self.epoch.store(after, Ordering::SeqCst);
        Ok(())
    }

    pub(crate) async fn mark_completed(
        &self,
        state: &DaemonState,
        files: &[String],
        relations: EnrichmentWrite,
        published: bool,
    ) -> Result<bool, Refused> {
        let _coordinated = state.coordination_gate.lock().await;
        self.validate(state)?;
        // The marker is a semantic claim too. Keep this final validation and
        // insertion together, including the zero-relation negative case.
        Ok(super::mark_completed_sweep_files(
            state,
            files,
            self.marker_epoch,
            relations,
            published,
        ))
    }

    /// Record that the sweep finished `file`, under the same coordination and
    /// freshness proof the completion marker is written under, bound to the
    /// body this pass asked about. `Ok(false)` when it was declined, which
    /// costs a later resume one question.
    pub(crate) async fn record_file_completed(
        &self,
        state: &DaemonState,
        file: &str,
    ) -> Result<bool, Refused> {
        let _coordinated = state.coordination_gate.lock().await;
        self.validate(state)?;
        let Some(body) = self.blob_hash(file) else {
            return Ok(false);
        };
        Ok(super::record_sweep_file_completed(
            state,
            file,
            body,
            self.marker_epoch,
        ))
    }

    /// The body this pass's captured tree holds at `path`.
    fn blob_hash(&self, path: &str) -> Option<kin_model::Hash256> {
        let path_id = RepoPath::from_utf8(path.to_string()).ok()?;
        match &self.tree.artifact_at_path(&path_id)?.entry {
            TreeEntry::Blob { hash, .. } => Some(*hash),
            _ => None,
        }
    }

    /// Take one enrichment write's turn for a whole unit's write: its
    /// relations absorbed and flushed, then its guesses settled and its
    /// ledgers installed, under one turn and one coordination gate hold.
    ///
    /// A live read lands before the unit's first write or after its last,
    /// never between two of them, so it never sees the unit's proven edges
    /// beside name-only guesses the unit has not yet retracted
    /// (`crate::live_read_gate`). Everything slow, the language server's
    /// answers above all, is done before the turn is taken.
    pub(crate) async fn write_unit<'a>(&'a self, state: &'a DaemonState) -> UnitWrite<'a> {
        UnitWrite {
            _turn: state.coordinate_enrichment_write().await,
            inputs: self,
            state,
        }
    }

    /// [`UnitWrite::absorb`] as a unit of its own.
    #[cfg(test)]
    pub(crate) async fn absorb(
        &self,
        state: &DaemonState,
        pending: &mut PendingEnrichment,
        derived: Vec<Relation>,
    ) -> Result<EnrichmentWrite, Refused> {
        self.write_unit(state).await.absorb(pending, derived)
    }

    fn absorb_locked(
        &self,
        state: &DaemonState,
        pending: &mut PendingEnrichment,
        derived: Vec<Relation>,
    ) -> Result<EnrichmentWrite, Refused> {
        self.validate(state)?;
        // The LSP matching index offers local entity endpoints only. An
        // unknown endpoint must not exploit the graph's permissive upsert.
        for relation in &derived {
            for node in [&relation.src, &relation.dst] {
                let GraphNodeId::Entity(id) = node else {
                    return Err(Refused::UnprovenSource);
                };
                if !self.ids.contains(id) {
                    return Err(Refused::UnprovenSource);
                }
            }
        }
        let written = pending.absorb(derived, |batch| {
            super::install_lsp_relations_locked(state, batch)
        });
        self.accept_own_write(state)?;
        Ok(written)
    }

    /// [`UnitWrite::flush`] as a unit of its own.
    #[cfg(test)]
    pub(crate) async fn flush(
        &self,
        state: &DaemonState,
        pending: &mut PendingEnrichment,
    ) -> Result<EnrichmentWrite, Refused> {
        self.write_unit(state).await.flush(pending)
    }

    fn flush_locked(
        &self,
        state: &DaemonState,
        pending: &mut PendingEnrichment,
    ) -> Result<EnrichmentWrite, Refused> {
        // Even an empty buffer must pass: stale negative answers are not
        // successful enrichment and cannot set a durable completion marker.
        self.validate(state)?;
        let written = pending.flush(|batch| super::install_lsp_relations_locked(state, batch));
        self.accept_own_write(state)?;
        Ok(written)
    }

    fn settle_locked(
        &self,
        state: &DaemonState,
        file: &str,
        text: &str,
        answers: &[kin_lsp::call_sites::SiteAnswer],
        names: &kin_lsp::call_sites::ExternalNames,
        context: Option<&kin_model::ProofContext>,
    ) -> Result<EnrichmentWrite, Refused> {
        self.validate(state)?;
        // Every endpoint an answer names came from this capture's index.
        for answer in answers {
            if !self.ids.contains(&answer.source) {
                return Err(Refused::UnprovenSource);
            }
            if let kin_lsp::call_sites::SiteTarget::Entity(target) = answer.target {
                if !self.ids.contains(&target) {
                    return Err(Refused::UnprovenSource);
                }
            }
        }
        let written = super::settle_call_sites_locked(state, file, text, answers, names, context);
        self.accept_own_write(state)?;
        Ok(written)
    }

    /// [`UnitWrite::settle_file`] as a unit of its own.
    #[cfg(test)]
    pub(crate) async fn settle_file(
        &self,
        state: &DaemonState,
        text: &str,
        answers: &[kin_lsp::call_sites::SiteAnswer],
        names: &kin_lsp::call_sites::ExternalNames,
        context: &kin_model::ProofContext,
        ledger: LedgerRequest<'_>,
    ) -> Result<(EnrichmentWrite, super::LedgerWrite), Refused> {
        self.write_unit(state)
            .await
            .settle_file(text, answers, names, context, ledger)
    }

    fn settle_file_locked(
        &self,
        state: &DaemonState,
        text: &str,
        answers: &[kin_lsp::call_sites::SiteAnswer],
        names: &kin_lsp::call_sites::ExternalNames,
        context: &kin_model::ProofContext,
        ledger: LedgerRequest<'_>,
    ) -> Result<(EnrichmentWrite, super::LedgerWrite), Refused> {
        self.validate(state)?;
        for answer in answers {
            if !self.ids.contains(&answer.source) {
                return Err(Refused::UnprovenSource);
            }
            if let kin_lsp::call_sites::SiteTarget::Entity(target) = answer.target {
                if !self.ids.contains(&target) {
                    return Err(Refused::UnprovenSource);
                }
            }
        }
        // Fresh Reference site proofs must belong to this exact captured
        // declaration universe and body, just like the definite answers.
        for relation in ledger.produced_references {
            for node in [relation.src, relation.dst] {
                if let kin_model::GraphNodeId::Entity(id) = node {
                    if !self.ids.contains(&id) {
                        return Err(Refused::UnprovenSource);
                    }
                }
            }
            for proof in &relation.evidence {
                if let Some(span) = &proof.source_span {
                    if span.file.0 == ledger.file
                        && (span.start_byte >= span.end_byte
                            || text.get(span.start_byte..span.end_byte).is_none())
                    {
                        return Err(Refused::UnprovenSource);
                    }
                }
            }
        }
        let written = super::settle_call_sites_locked(
            state,
            ledger.file,
            text,
            answers,
            names,
            Some(context),
        );
        self.accept_own_write(state)?;
        let Some(body) = self.blob_hash(ledger.file) else {
            return Ok((written, super::LedgerWrite::default()));
        };
        let context_id = kin_model::ResolutionRecord::ProofContext(context.clone()).id();
        let pass = crate::call_site_ledger::FilePass {
            file: ledger.file,
            text,
            uri: ledger.uri,
            index: ledger.index,
            entities: ledger.entities,
            answers,
            names,
            unproven: ledger.unproven,
            recorded: &[],
            static_proofs: &[],
            outside_calls: &[],
            produced_references: ledger.produced_references,
            ending: ledger.ending,
            not_in_build: ledger.not_in_build,
            context: context_id,
            body,
        };
        let disproved = self.take_disproved_type_uses(ledger.file);
        let ledgers = super::install_call_site_ledgers_locked(
            state,
            &pass,
            context,
            ledger.retract,
            &disproved,
        );
        self.accept_own_write(state)?;
        Ok((written, ledgers))
    }

    fn accept_own_write(&self, state: &DaemonState) -> Result<(), Refused> {
        // Still under coordination. Only our synchronous relation installer
        // could have advanced the epoch here; never refresh after waiting on a
        // foreign edit. This permits files larger than one 256-edge batch.
        let epoch = state.stable_graph_authority_epoch().ok_or(Refused::Stale)?;
        self.epoch.store(epoch, Ordering::SeqCst);
        Ok(())
    }
}

/// One unit's enrichment write, from [`QueryInputs::write_unit`]. Every write
/// through it lands under the one turn it holds, with the coordination gate,
/// and each still proves the capture fresh before it changes the graph.
pub(crate) struct UnitWrite<'a> {
    _turn: crate::state::EnrichmentWriteTurn<'a>,
    inputs: &'a QueryInputs,
    state: &'a DaemonState,
}

impl UnitWrite<'_> {
    /// Buffer `derived`, installing each batch it fills.
    pub(crate) fn absorb(
        &self,
        pending: &mut PendingEnrichment,
        derived: Vec<Relation>,
    ) -> Result<EnrichmentWrite, Refused> {
        self.inputs.absorb_locked(self.state, pending, derived)
    }

    /// Install what is still buffered.
    pub(crate) fn flush(
        &self,
        pending: &mut PendingEnrichment,
    ) -> Result<EnrichmentWrite, Refused> {
        self.inputs.flush_locked(self.state, pending)
    }

    /// Settle the linker's name-only call guesses in `file` against what the
    /// server answered at their call sites, once the file's relations are in.
    ///
    /// `text` is the admitted source these answers were asked against, the
    /// same bytes the guesses' sites were parsed from while this capture's
    /// epoch holds.
    ///
    /// `names` are the external symbols the server's outside answers were
    /// named as, and `context` the proof context it answered under; with both,
    /// a call into a named symbol is proven as well as refuting guesses.
    pub(crate) fn settle(
        &self,
        file: &str,
        text: &str,
        answers: &[kin_lsp::call_sites::SiteAnswer],
        names: &kin_lsp::call_sites::ExternalNames,
        context: Option<&kin_model::ProofContext>,
    ) -> Result<EnrichmentWrite, Refused> {
        self.inputs
            .settle_locked(self.state, file, text, answers, names, context)
    }

    /// [`Self::settle`] for a file a sweep finished asking about, then give
    /// every caller in the file its call-site ledger and make the file's
    /// proofs agree with the ledgers (see
    /// [`super::install_call_site_ledgers_locked`]), in the same turn and
    /// under the same freshness proof.
    ///
    /// `ledger` carries what the passes left beyond their answers: the
    /// questions that proved nothing, how the passes ended, and whether the
    /// file is in any build. With `retract`, every pass over the file
    /// finished, so a proof its ledgers do not hold is retracted.
    pub(crate) fn settle_file(
        &self,
        text: &str,
        answers: &[kin_lsp::call_sites::SiteAnswer],
        names: &kin_lsp::call_sites::ExternalNames,
        context: &kin_model::ProofContext,
        ledger: LedgerRequest<'_>,
    ) -> Result<(EnrichmentWrite, super::LedgerWrite), Refused> {
        self.inputs
            .settle_file_locked(self.state, text, answers, names, context, ledger)
    }
}

/// What a sweep's passes over one file left for its call-site ledgers, beyond
/// their answers.
pub(crate) struct LedgerRequest<'a> {
    pub(crate) file: &'a str,
    /// The file's URI as `index` knows it.
    pub(crate) uri: &'a str,
    pub(crate) index: &'a kin_lsp::EntityIndex,
    /// The entities the file declares.
    pub(crate) entities: &'a [&'a Entity],
    /// Every identifier the definitions pass asked about and could not prove.
    pub(crate) unproven: &'a [kin_lsp::call_sites::UnprovenSite],
    /// The language-server `References` relations the passes over the file
    /// produced, which a retraction keeps.
    pub(crate) produced_references: &'a Vec<kin_model::Relation>,
    pub(crate) ending: crate::call_site_ledger::PassEnding,
    /// Whether no build of the repository compiles the file.
    pub(crate) not_in_build: bool,
    /// Whether every pass over the file finished, so a proof its ledgers do
    /// not hold may be retracted.
    pub(crate) retract: bool,
}

pub(crate) fn request_fresh_sweep(state: &DaemonState) {
    // Retry derivation, never replay obsolete relation payloads. The worker's
    // receive boundary drains this coalesced bit, retaining it on a full queue.
    state.lsp_sweep_pending.store(true, Ordering::SeqCst);
}
