// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! One state for every call site in every caller a sweep finishes.
//!
//! When the sweep finishes a file, each entity with source text in it gets a
//! [`CallSiteLedger`]: the number of call expressions the parser reads in its
//! body (its census) and one [`CallSiteState`] for each of them, keyed by the
//! callee token's offset and length inside the entity. An entity that makes no
//! call gets a ledger with a census of zero, so a caller with no ledger is
//! always one whose enrichment is owed.
//!
//! The census comes from parsing the admitted text again with the parser that
//! derived the file, so it counts the call expressions the linker recorded its
//! sites at, including calls whose callee the parser could not name. Each
//! expression belongs to the entity a language server's answer about it is
//! attributed to: the innermost entity holding the line of its callee token.
//! Equally innermost owners retain each admitted occurrence, with an explicit
//! ownership gap rather than an arbitrary proof or a false empty census.
//!
//! A site's state is, in order:
//!
//! - not in build, when no build of the repository compiles the file;
//! - unresolved (callee not placeable), when the text names no callee token;
//! - what every answer at its callee token proves, merged as settlement merges
//!   them: a repository entity, a named external symbol, a declaration
//!   outside the repository no symbol names, or answers that disagree;
//! - otherwise what the question came to: a value binding, an answer outside
//!   the graph, no answer, a refusal, or a timeout, crash or protocol error;
//! - otherwise what ended the pass before the question was asked.
//!
//! Then the file's `Calls` proofs are made to agree with its ledgers. A
//! proven site is carried by an edge to its target under the ledger's proof
//! context, with an evidence record added where no edge carried it yet. When
//! the file's passes all finished, a proof at a census site this pass settled
//! differently or did not prove leaves its edge, and an edge left with no
//! site is retired. Outside the census, only a definite answer can replace
//! the old proof. Unanswered implicit calls, such as property accesses, stay
//! unchanged under their prior context. A file whose passes failed is never
//! retracted; its proofs are only stamped and completed.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use kin_lsp::call_sites::{
    self, CalleeToken, ExternalNames, SiteAnswer, UnprovenAnswer, UnprovenSite,
};
use kin_model::{
    site_key, CallSite, CallSiteLedger, CallSiteState, Entity, EntityId, ExternalReference,
    ExternalReferenceId, GraphNodeId, Hash256, Relation, RelationEvidence, RelationKind,
    RelationOrigin, ResolutionRecordId, ServerFailure, SourceSpan, UnresolvedReason,
};

use crate::call_site_settlement::{site_proofs, SiteProof};

/// One call expression Kin's parser reads in a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CallExpression {
    /// Byte offsets of the whole expression in the file.
    pub(crate) start_byte: usize,
    pub(crate) end_byte: usize,
    /// Its callee token, when the text names one plainly (see
    /// [`call_sites::callee_token`]).
    pub(crate) callee: Option<CalleeToken>,
    /// The zero-based line of the callee token, or of the expression when it
    /// has none: the line a server was asked at, and so the line its caller
    /// is found at.
    pub(crate) line: u32,
    /// Whether the callee is a bare name a function or a block around the
    /// call binds (see [`kin_parser::value_escape::local_callees`]): the
    /// call runs what that local name holds, whatever any other declaration
    /// of the name is.
    pub(crate) local_callee: bool,
}

fn source_adapter(file: &str, text: &str) -> Option<&'static dyn kin_parser::LanguageAdapter> {
    static REGISTRY: std::sync::OnceLock<kin_parser::AdapterRegistry> = std::sync::OnceLock::new();
    let registry = REGISTRY.get_or_init(kin_parser::AdapterRegistry::new);
    let ext = Path::new(file).extension()?.to_str()?;
    registry.get_by_extension_and_content(ext, text.as_bytes())
}

/// The syntax tree of the admitted source, for projecting recorded call sites.
pub(crate) fn source_tree(file: &str, text: &str) -> Option<call_sites::SourceTree> {
    source_adapter(file, text)?.parse(text.as_bytes()).ok()
}

/// Every call expression Kin's parser reads in `file`, whose admitted text is
/// `text`, in source order and each once. `None` when no adapter parses the
/// file or the parse fails.
///
/// The parser is the one that derived the file's entities and the linker's
/// call edges, run again over the same bytes, so the expressions are the ones
/// the linker recorded its call sites at: every `Calls` relation's site, and
/// every call the parser could not represent as a relation and recorded as a
/// gap in its caller's call coverage instead.
#[cfg(test)]
pub(crate) fn call_expressions(file: &str, text: &str) -> Option<Vec<CallExpression>> {
    call_expressions_with(file, text, &[])
}

/// [`call_expressions`], with the call sites the graph's own call edges record
/// in the file as well: `recorded` holds each one's byte range and first line.
///
/// The linker records calls the parser's relations do not carry, such as a
/// call it infers inside a macro's token tree. A site the graph serves a call
/// edge at is a call site whatever produced it, so it has a state too.
pub(crate) fn call_expressions_with(
    file: &str,
    text: &str,
    recorded: &[(usize, usize, u32)],
) -> Option<Vec<CallExpression>> {
    let adapter = source_adapter(file, text)?;
    let tree = adapter.parse(text.as_bytes()).ok()?;
    let output = adapter
        .extract(&tree, text.as_bytes(), &kin_model::FilePathId::new(file))
        .ok()?;
    let mut sites: BTreeMap<(usize, usize), u32> = BTreeMap::new();
    for relation in &output.relations {
        let gap = kin_parser::is_scoped_call_extraction_incomplete_marker(relation);
        let is_call = relation.kind == RelationKind::Calls || gap;
        if !is_call {
            continue;
        }
        let Some(site) = relation.site.as_ref() else {
            continue;
        };
        if site.end_byte <= site.start_byte || site.end_byte > text.len() {
            continue;
        }
        // A scoped coverage gap may cover a whole definition or parameter.
        // Keep that negative evidence in the parser's certificate, but do not
        // count the container as a second call beside the calls within it.
        if gap {
            let Some(call) = tree
                .root_node()
                .named_descendant_for_byte_range(site.start_byte, site.end_byte)
            else {
                continue;
            };
            if call.start_byte() != site.start_byte
                || call.end_byte() != site.end_byte
                || !matches!(call.kind(), "call" | "call_expression" | "decorator")
            {
                continue;
            }
        }
        sites.insert((site.start_byte, site.end_byte), site.start_line);
    }
    for (start_byte, end_byte, start_line) in recorded {
        if *end_byte <= *start_byte || *end_byte > text.len() {
            continue;
        }
        sites.entry((*start_byte, *end_byte)).or_insert(*start_line);
    }
    let lines = LineStarts::new(text);
    let local: std::collections::HashSet<(usize, usize)> =
        kin_parser::value_escape::local_callees(&tree, text.as_bytes(), adapter.language_id())
            .into_iter()
            .collect();
    Some(
        sites
            .into_iter()
            .map(|((start_byte, end_byte), start_line)| {
                let span = SourceSpan {
                    file: kin_model::FilePathId::new(file),
                    start_byte,
                    end_byte,
                    start_line,
                    start_col: 0,
                    end_line: start_line,
                    end_col: 0,
                };
                let callee = call_sites::callee_token_in_tree(&tree, text, &span)
                    .or_else(|| call_sites::callee_token(file, text, &span));
                let line = callee
                    .as_ref()
                    .map_or(start_line, |token| lines.line_of(token.start_byte));
                let local_callee = callee.as_ref().is_some_and(|token| {
                    !token.through_member && local.contains(&(token.start_byte, token.end_byte))
                });
                CallExpression {
                    start_byte,
                    end_byte,
                    callee,
                    line,
                    local_callee,
                }
            })
            .collect(),
    )
}

/// The byte offset each line of a text starts at.
struct LineStarts(Vec<usize>);

impl LineStarts {
    fn new(text: &str) -> Self {
        let mut starts = vec![0];
        starts.extend(
            text.bytes()
                .enumerate()
                .filter(|(_, byte)| *byte == b'\n')
                .map(|(at, _)| at + 1),
        );
        Self(starts)
    }

    /// The zero-based line holding byte `offset`.
    fn line_of(&self, offset: usize) -> u32 {
        let line = self.0.partition_point(|start| *start <= offset);
        line.saturating_sub(1) as u32
    }
}

/// A call site a parser or linker edge proves: the caller, the call
/// expression's byte range, and the declaration it reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StaticProof {
    pub(crate) caller: EntityId,
    pub(crate) start_byte: usize,
    pub(crate) end_byte: usize,
    pub(crate) target: EntityId,
}

/// Whether the resolver left a site open for a static proof to settle: it
/// failed, gave no answer, or was never asked. An answer that places the
/// callee outside the graph, answers that disagree, and a reflective access
/// are the resolver's own findings, and a static proof never overrides them;
/// nor does it override a value binding or a file no build compiles.
fn open_to_static_proof(state: &CallSiteState) -> bool {
    match state {
        CallSiteState::ServerFailed { .. } => true,
        CallSiteState::Unresolved { reason } => !matches!(
            reason,
            UnresolvedReason::OutsideTheGraph
                | UnresolvedReason::AnswersDisagree
                | UnresolvedReason::Reflective
        ),
        _ => false,
    }
}

/// Whether the resolver left a site open for the parser's own reading of its
/// callee to settle: every state a static proof may settle, and a caller no
/// build compiles, since which name a scope binds needs no build. A static
/// proof is applied first, so a site it settled is no longer open here.
fn open_to_local_binding(state: &CallSiteState) -> bool {
    open_to_static_proof(state) || matches!(state, CallSiteState::NotInBuild { .. })
}

/// How the passes over a file ended, for the sites they left unanswered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PassEnding {
    /// The definitions pass asked about every identifier it planned to.
    Complete,
    /// The pass stopped before asking about every identifier, or never
    /// answered, for this reason.
    Stopped(ServerFailure),
    /// The server declined the pass as a whole: an answer with nothing in it.
    Declined,
    /// The server answered none of the file's questions, which is how a
    /// server reports a file no package it loaded holds.
    NothingAnswered,
}

/// The reason a site in a file no build compiles carries.
pub(crate) const NOT_IN_BUILD_REASON: &str =
    "no build of the repository compiles this file, so no configuration of its resolver answers for it";

/// The reason a site in a file whose server answered nothing carries.
const NOTHING_ANSWERED_REASON: &str =
    "the resolver answered none of this file's questions, which is how it reports a file no package it loaded holds";

/// Everything one file's passes left that its ledgers are built from.
#[derive(Clone, Copy)]
pub(crate) struct FilePass<'a> {
    pub(crate) file: &'a str,
    pub(crate) text: &'a str,
    /// The file's URI as the entity index knows it.
    pub(crate) uri: &'a str,
    pub(crate) index: &'a kin_lsp::EntityIndex,
    /// The entities the file declares.
    pub(crate) entities: &'a [&'a Entity],
    /// Every definite answer the passes gave at an identifier or a call range.
    pub(crate) answers: &'a [SiteAnswer],
    pub(crate) names: &'a ExternalNames,
    /// Every identifier the definitions pass asked about and could not prove.
    pub(crate) unproven: &'a [UnprovenSite],
    /// The call sites the graph's call edges of other origins record in the
    /// file, as byte range and first line (see [`call_expressions_with`]).
    pub(crate) recorded: &'a [(usize, usize, u32)],
    /// The sites at which a parser or linker call edge proves its destination
    /// (see [`kin_index::occurrence::static_proof_sites`]), which settle a site
    /// the resolver left open.
    pub(crate) static_proofs: &'a [StaticProof],
    /// The call expressions, by byte range, the file's coverage certificate
    /// records as proved to reach no declaration of the repository: the
    /// receiver's value-origin facts type it as a type outside it (see
    /// [`kin_index::receiver_outside_calls`]). Each settles a site the
    /// resolver left open as [`CallSiteState::ProvenOutside`].
    pub(crate) outside_calls: &'a [(usize, usize)],
    /// The language-server `References` relations this pass produced, which a
    /// retraction keeps.
    pub(crate) produced_references: &'a Vec<Relation>,
    pub(crate) ending: PassEnding,
    /// Whether no build of the repository compiles the file.
    pub(crate) not_in_build: bool,
    /// The proof context the server answered under.
    pub(crate) context: ResolutionRecordId,
    /// The body of the file the passes were asked about.
    pub(crate) body: Hash256,
}

/// How many sites of one file's ledgers came to each state, and what the
/// census could not attribute.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct LedgerCounts {
    /// Call expressions the parser read in the file.
    pub(crate) expressions: usize,
    /// Of those, the ones no entity with source text holds at their line, or
    /// whose key would fall before their caller's text.
    pub(crate) unattributed: usize,
    /// Sites by state, spelled as payloads spell it.
    pub(crate) by_state: BTreeMap<&'static str, usize>,
}

/// One file's ledgers, with what a proven site's edge carries it with.
#[derive(Debug, Clone, Default)]
pub(crate) struct FileLedgers {
    /// One ledger for each entity with source text in the file.
    pub(crate) ledgers: BTreeMap<EntityId, CallSiteLedger>,
    /// The external symbols a proven site names, which must be in the graph
    /// before an edge or a ledger names them.
    pub(crate) external: BTreeMap<ExternalReferenceId, ExternalReference>,
    /// For every proven site, by caller and key, the evidence record an edge
    /// carries it with: the answer's own span and rule.
    evidence: HashMap<(EntityId, u32, u32), RelationEvidence>,
    /// What this pass proved at every span it answered, by caller and byte
    /// range: the node the call reaches.
    answered: HashMap<(EntityId, usize, usize), GraphNodeId>,
    pub(crate) counts: LedgerCounts,
}

/// The order a site's evidence rule is preferred in: a definition answer,
/// then one through an alias, then a call range.
fn rule_rank(rule: &str) -> u8 {
    match rule {
        call_sites::DEFINITION_RULE => 0,
        call_sites::DEFINITION_ALIAS_RULE => 1,
        _ => 2,
    }
}

/// The state a question that proved nothing leaves its site in.
fn unproven_state(answer: UnprovenAnswer) -> CallSiteState {
    match answer {
        // A decline is the server answering that nothing applies here, which
        // is an empty answer.
        UnprovenAnswer::NoAnswer | UnprovenAnswer::Declined => CallSiteState::Unresolved {
            reason: UnresolvedReason::NoAnswer,
        },
        // A refusal is the server failing at this position. Read as `no_answer`
        // it blamed the site for what the server did: on React every site a
        // wedged tsserver failed at said the server found nothing there.
        UnprovenAnswer::Refused => CallSiteState::Unresolved {
            reason: UnresolvedReason::ServerRefused,
        },
        UnprovenAnswer::Binding => CallSiteState::Binding { may_call: None },
        UnprovenAnswer::OutsideTheGraph => CallSiteState::Unresolved {
            reason: UnresolvedReason::OutsideTheGraph,
        },
        UnprovenAnswer::AnswersDisagree => CallSiteState::Unresolved {
            reason: UnresolvedReason::AnswersDisagree,
        },
        UnprovenAnswer::Timeout => CallSiteState::ServerFailed {
            reason: ServerFailure::Timeout,
        },
        UnprovenAnswer::Crash => CallSiteState::ServerFailed {
            reason: ServerFailure::Crash,
        },
        UnprovenAnswer::ProtocolError => CallSiteState::ServerFailed {
            reason: ServerFailure::ProtocolError,
        },
    }
}

/// Whether one unproven answer outranks another at the same site: a failure
/// to answer outranks an answer, since the site was not settled, and a
/// refusal, the server failing at the site, outranks an answer too.
fn outranks(new: UnprovenAnswer, held: UnprovenAnswer) -> bool {
    let rank = |answer| match answer {
        UnprovenAnswer::Timeout | UnprovenAnswer::Crash | UnprovenAnswer::ProtocolError => 2,
        UnprovenAnswer::Refused => 1,
        _ => 0,
    };
    rank(new) > rank(held)
}

/// Build one ledger for every entity with source text in `pass.file`.
///
/// `None` when the parser cannot read the file again, which leaves every
/// caller in it owed.
pub(crate) fn build_ledgers(pass: &FilePass<'_>) -> Option<FileLedgers> {
    let expressions = call_expressions_with(pass.file, pass.text, pass.recorded)?;
    let proofs = site_proofs(pass.file, pass.answers, pass.names);
    let mut unproven: HashMap<(EntityId, usize, usize), UnprovenAnswer> = HashMap::new();
    for site in pass.unproven {
        let key = (site.source, site.start_byte, site.end_byte);
        match unproven.get(&key) {
            Some(held) if !outranks(site.answer, *held) => {}
            _ => {
                unproven.insert(key, site.answer);
            }
        }
    }
    // The answer each proven span's evidence is recorded from.
    let mut answer_at: HashMap<(EntityId, usize, usize), &SiteAnswer> = HashMap::new();
    for answer in pass.answers {
        if answer.site.file.0 != pass.file {
            continue;
        }
        let key = (answer.source, answer.site.start_byte, answer.site.end_byte);
        match answer_at.get(&key) {
            Some(held) if rule_rank(held.rule) <= rule_rank(answer.rule) => {}
            _ => {
                answer_at.insert(key, answer);
            }
        }
    }
    let starts: HashMap<EntityId, usize> = pass
        .entities
        .iter()
        .filter_map(|entity| Some((entity.id, entity.span.as_ref()?.start_byte)))
        .collect();
    let spans: HashMap<_, _> = pass
        .entities
        .iter()
        .filter_map(|entity| Some((entity.id, entity.span.as_ref()?)))
        .collect();
    let outside_calls: std::collections::HashSet<(usize, usize)> =
        pass.outside_calls.iter().copied().collect();
    // One static target per call expression. Two edges proving different
    // targets at one site prove neither.
    let mut static_targets: HashMap<(EntityId, usize, usize), Option<EntityId>> = HashMap::new();
    for proof in pass.static_proofs {
        static_targets
            .entry((proof.caller, proof.start_byte, proof.end_byte))
            .and_modify(|held| {
                if *held != Some(proof.target) {
                    *held = None;
                }
            })
            .or_insert(Some(proof.target));
    }

    let mut ledgers = FileLedgers::default();
    for (key, proof) in &proofs {
        let node = match proof {
            SiteProof::Entity(target) => GraphNodeId::Entity(*target),
            SiteProof::External(reference) => GraphNodeId::ExternalReference(reference.id),
            SiteProof::Outside | SiteProof::Disagree => continue,
        };
        ledgers.answered.insert(*key, node);
    }
    ledgers.counts.expressions = expressions.len();
    let mut sites: BTreeMap<EntityId, BTreeMap<(u32, u32), CallSiteState>> = BTreeMap::new();
    for expression in &expressions {
        let owners: Vec<_> = pass
            .index
            .find_all_at(pass.uri, expression.line)
            .into_iter()
            .filter(|owner| {
                spans.get(&owner.id).is_some_and(|span| {
                    expression.start_byte >= span.start_byte && expression.end_byte <= span.end_byte
                })
            })
            .collect();
        if owners.is_empty() {
            ledgers.counts.unattributed += 1;
            continue;
        }
        let ambiguous = owners.len() > 1;
        for owner in owners {
            let caller = owner.id;
            let (start, end) = expression
                .callee
                .as_ref()
                .map_or((expression.start_byte, expression.end_byte), |token| {
                    (token.start_byte, token.end_byte)
                });
            let Some((offset, length)) = site_key(starts[&caller], start, end) else {
                ledgers.counts.unattributed += 1;
                continue;
            };
            let lookup = (caller, start, end);
            let state = if pass.not_in_build {
                CallSiteState::NotInBuild {
                    reason: NOT_IN_BUILD_REASON.to_string(),
                }
            } else if ambiguous || expression.callee.is_none() {
                CallSiteState::Unresolved {
                    reason: UnresolvedReason::CalleeNotPlaceable,
                }
            } else if let Some(proof) = proofs.get(&lookup) {
                let state = match proof {
                    SiteProof::Entity(target) => CallSiteState::ProvenTarget { target: *target },
                    SiteProof::External(reference) => {
                        ledgers.external.insert(reference.id, reference.clone());
                        CallSiteState::ProvenExternal {
                            target: reference.id,
                        }
                    }
                    SiteProof::Outside => CallSiteState::ProvenOutside,
                    SiteProof::Disagree => CallSiteState::Unresolved {
                        reason: UnresolvedReason::AnswersDisagree,
                    },
                };
                if state.proven_node().is_some() {
                    if let Some(answer) = answer_at.get(&lookup) {
                        ledgers.evidence.insert(
                            (caller, offset, length),
                            RelationEvidence {
                                token: Some(pass.context.context_token()),
                                ..call_sites::site_evidence(answer.rule, answer.site.clone())
                            },
                        );
                    }
                }
                state
            } else if let Some(answer) = unproven.get(&lookup) {
                unproven_state(*answer)
            } else {
                match pass.ending {
                    PassEnding::Complete => CallSiteState::Unresolved {
                        reason: UnresolvedReason::CalleeNotPlaceable,
                    },
                    PassEnding::Stopped(failure) => CallSiteState::ServerFailed { reason: failure },
                    PassEnding::Declined => CallSiteState::Unresolved {
                        reason: UnresolvedReason::NoAnswer,
                    },
                    PassEnding::NothingAnswered => CallSiteState::NotInBuild {
                        reason: NOTHING_ANSWERED_REASON.to_string(),
                    },
                }
            };
            let state = match static_targets
                .get(&(caller, expression.start_byte, expression.end_byte))
                .copied()
            {
                Some(Some(target))
                    if !ambiguous && target != caller && open_to_static_proof(&state) =>
                {
                    CallSiteState::ProvenStatic { target }
                }
                // The linker proved the call reaches no declaration of the
                // repository, and no edge of its claims otherwise.
                None if !ambiguous
                    && open_to_static_proof(&state)
                    && outside_calls.contains(&(expression.start_byte, expression.end_byte)) =>
                {
                    CallSiteState::ProvenOutside
                }
                _ => state,
            };
            // A callee the caller's own code binds is a call through that
            // binding, which is what a resolver that answers says of it too.
            let state = if !ambiguous && expression.local_callee && open_to_local_binding(&state) {
                CallSiteState::Binding { may_call: None }
            } else {
                state
            };
            sites
                .entry(caller)
                .or_default()
                .entry((offset, length))
                .or_insert(state);
        }
    }
    for entity in pass.entities {
        if entity.span.is_none() {
            continue;
        }
        let held = sites.remove(&entity.id).unwrap_or_default();
        let sites: Vec<CallSite> = held
            .into_iter()
            .map(|((offset, length), state)| {
                *ledgers.counts.by_state.entry(state.wire()).or_insert(0) += 1;
                CallSite {
                    offset,
                    length,
                    state,
                }
            })
            .collect();
        ledgers.ledgers.insert(
            entity.id,
            CallSiteLedger {
                caller: entity.id,
                behavior_hash: entity.fingerprint.behavior_hash,
                body_hash: pass.body,
                context: pass.context,
                census: sites.len() as u32,
                sites,
            },
        );
    }
    Some(ledgers)
}

/// What making one file's `Calls` proofs agree with its ledgers changes.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ProofRewrite {
    /// Edges written as they now stand: stamped with the ledger's context,
    /// completed with a site no edge carried, or narrowed by a retraction.
    pub(crate) written: Vec<Relation>,
    /// Edges left with no site, retired.
    pub(crate) retired: Vec<Relation>,
    /// Retired edges whose every recorded site a positive answer contradicted.
    /// An unanswered or ambiguous site may lose its old proof, but earns no
    /// census credit for that loss.
    pub(crate) settled_retirements: HashSet<kin_model::RelationId>,
    /// Evidence records a retraction removed.
    pub(crate) retracted_records: usize,
    /// Proven sites no edge carried, now carried.
    pub(crate) completed_sites: usize,
}

/// Make the language-server `Calls` edges from the file's callers agree with
/// their ledgers.
///
/// `held` are the edges the graph holds from the file's callers, of every
/// origin; only language-server `Calls` edges are touched. With `retract`,
/// the file's passes all finished and a proof at a census site the ledger
/// settled otherwise leaves. A positive answer outside the census can also
/// replace that site's prior target. An unanswered span outside the census
/// stays unchanged: explicit calls do not measure implicit property calls.
/// Without `retract`, nothing is removed.
pub(crate) fn rewrite_proofs(
    ledgers: &FileLedgers,
    pass: &FilePass<'_>,
    held: &[Relation],
    retract: bool,
) -> ProofRewrite {
    let token = pass.context.context_token();
    let proofs = crate::call_site_settlement::site_proofs(pass.file, pass.answers, pass.names);
    let starts: HashMap<EntityId, usize> = pass
        .entities
        .iter()
        .filter_map(|entity| Some((entity.id, entity.span.as_ref()?.start_byte)))
        .collect();
    let mut rewrite = ProofRewrite::default();
    // Every edge as it stands after the rewrite, by caller and destination.
    let mut edges: HashMap<(EntityId, GraphNodeId), Relation> = HashMap::new();
    let mut original: HashMap<kin_model::RelationId, &Relation> = HashMap::new();
    let mut carried: HashSet<(EntityId, GraphNodeId, u32, u32)> = HashSet::new();
    for edge in held {
        if edge.kind != RelationKind::Calls || edge.origin != RelationOrigin::Lsp {
            continue;
        }
        let GraphNodeId::Entity(caller) = edge.src else {
            continue;
        };
        let (Some(ledger), Some(start)) = (ledgers.ledgers.get(&caller), starts.get(&caller))
        else {
            continue;
        };
        original.insert(edge.id, edge);
        let mut kept = Vec::with_capacity(edge.evidence.len());
        for record in &edge.evidence {
            let Some(span) = record
                .source_span
                .as_ref()
                .filter(|span| span.file.0 == pass.file)
            else {
                kept.push(record.clone());
                continue;
            };
            let key = site_key(*start, span.start_byte, span.end_byte);
            let site = key.and_then(|(offset, length)| ledger.site(offset, length));
            let proves_here = match site {
                Some(site) => site.state.proven_node() == Some(edge.dst),
                None => {
                    ledgers
                        .answered
                        .get(&(caller, span.start_byte, span.end_byte))
                        == Some(&edge.dst)
                }
            };
            if proves_here {
                let mut stamped = record.clone();
                stamped.token = Some(token.clone());
                if let (Some((offset, length)), Some(_)) = (key, site) {
                    carried.insert((caller, edge.dst, offset, length));
                }
                kept.push(stamped);
            } else if retract
                && (site.is_some()
                    || ledgers
                        .answered
                        .contains_key(&(caller, span.start_byte, span.end_byte)))
            {
                rewrite.retracted_records += 1;
            } else {
                // A property/getter access can be a call-hierarchy proof
                // without being an explicit parser call. No census site and
                // no answer leaves its prior evidence unmeasured, not
                // disproven. Retain its old context rather than stamping it
                // with this pass's context.
                kept.push(record.clone());
            }
        }
        let mut edge = edge.clone();
        edge.evidence = kept;
        edges.insert((caller, edge.dst), edge);
    }
    // A proven site no edge carries yet gets its answer's record, on the edge
    // to its target.
    for (caller, ledger) in &ledgers.ledgers {
        for site in &ledger.sites {
            let Some(target) = site.state.proven_node() else {
                continue;
            };
            if target == GraphNodeId::Entity(*caller)
                || carried.contains(&(*caller, target, site.offset, site.length))
            {
                continue;
            }
            let Some(record) = ledgers
                .evidence
                .get(&(*caller, site.offset, site.length))
                .cloned()
            else {
                continue;
            };
            rewrite.completed_sites += 1;
            carried.insert((*caller, target, site.offset, site.length));
            match edges.get_mut(&(*caller, target)) {
                Some(edge) => edge.evidence.push(record),
                None => {
                    let edge = match target {
                        GraphNodeId::Entity(entity) => {
                            call_sites::proven_call(*caller, entity, vec![record])
                        }
                        GraphNodeId::ExternalReference(reference) => {
                            call_sites::proven_external_call(*caller, reference, vec![record])
                        }
                        _ => continue,
                    };
                    edges.insert((*caller, target), edge);
                }
            }
        }
    }
    let mut edges: Vec<Relation> = edges.into_values().collect();
    edges.sort_by_key(|edge| edge.id);
    for edge in edges {
        let mut edge = edge;
        if original.get(&edge.id).is_some_and(|held| **held == edge) {
            continue;
        }
        // A changed edge takes the order the site union keeps evidence in, so
        // the next union of the same sites merges back to it and writes
        // nothing: a record with no span first, then by file, line, column
        // and rule.
        edge.evidence.sort_by_key(union_order);
        edge.evidence.dedup();
        match original.get(&edge.id) {
            Some(held) if **held == edge => {}
            _ if !edge
                .evidence
                .iter()
                .any(|record| record.source_span.is_some()) =>
            {
                if let Some(held) = original.get(&edge.id) {
                    let spans: Vec<_> = held
                        .evidence
                        .iter()
                        .filter_map(|record| record.source_span.as_ref())
                        .collect();
                    if let GraphNodeId::Entity(caller) = held.src {
                        if !spans.is_empty()
                            && !pass.not_in_build
                            && spans.iter().all(|span| {
                                if span.file.0 != pass.file {
                                    return false;
                                }
                                let lookup = (caller, span.start_byte, span.end_byte);
                                let target = match proofs.get(&lookup) {
                                    Some(SiteProof::Entity(target)) => {
                                        Some(GraphNodeId::Entity(*target))
                                    }
                                    Some(SiteProof::External(target)) => {
                                        Some(GraphNodeId::ExternalReference(target.id))
                                    }
                                    Some(SiteProof::Outside) => None,
                                    Some(SiteProof::Disagree) | None => return false,
                                };
                                let site =
                                    site_key(starts[&caller], span.start_byte, span.end_byte)
                                        .and_then(|(offset, length)| {
                                            ledgers.ledgers[&caller].site(offset, length)
                                        });
                                match target {
                                    Some(target) => {
                                        held.dst != target
                                            && match site {
                                                // A raw answer cannot override an ambiguous or
                                                // otherwise unproven census site.
                                                Some(site) => {
                                                    site.state.proven_node() == Some(target)
                                                }
                                                // An implicit property call has no census site;
                                                // its exact positive answer still proves its target.
                                                None => {
                                                    ledgers.answered.get(&lookup) == Some(&target)
                                                }
                                            }
                                    }
                                    // An unnamed outside declaration disproves a repository
                                    // target, but cannot distinguish two outside symbols.
                                    None => {
                                        matches!(held.dst, GraphNodeId::Entity(_))
                                            && site.is_some_and(|site| {
                                                site.state == CallSiteState::ProvenOutside
                                            })
                                    }
                                }
                            })
                        {
                            rewrite.settled_retirements.insert(held.id);
                        }
                    }
                    rewrite.retired.push((*held).clone());
                }
            }
            _ => rewrite.written.push(edge),
        }
    }
    rewrite
}

/// Settle the `UsesType` evidence a file's entities hold against the type
/// answers this pass positively disproved (see
/// [`kin_lsp::enrichment::DisprovedTypeUse`]).
///
/// A type answer that lands on a declaration's name line, off its name, is
/// about a parameter declared there. A build that compared lines alone read
/// it as the declaration and recorded the entity that asked as using it as a
/// type. Asked again, the identifier gets the same answer, and the pass now
/// says it is not about that declaration.
///
/// An edge is touched only at the exact identifier such an answer was asked
/// at: the record under the type answer's rule at that site is retired. The
/// edge goes with it when no record with a site is left, and stays, narrowed,
/// when another identifier still proves it. A record the pass did not
/// disprove is not judged here, and without `retract` nothing is: as for a
/// contradicted reference, only a pass that finished removes evidence.
///
/// Returns the edges to retire and the edges narrowed.
pub(crate) fn disproved_type_uses(
    file: &str,
    held: &[Relation],
    disproved: &[kin_lsp::enrichment::DisprovedTypeUse],
    retract: bool,
) -> (Vec<Relation>, Vec<Relation>) {
    let mut retired = Vec::new();
    let mut narrowed = Vec::new();
    if !retract || disproved.is_empty() {
        return (retired, narrowed);
    }
    for edge in held {
        let (GraphNodeId::Entity(source), GraphNodeId::Entity(target)) = (edge.src, edge.dst)
        else {
            continue;
        };
        if edge.kind != RelationKind::UsesType || edge.origin != RelationOrigin::Lsp {
            continue;
        }
        let contradicted = |record: &RelationEvidence| {
            record.parser_rule.as_deref() == Some(kin_lsp::enrichment::TYPE_ANSWER_RULE)
                && record.source_span.as_ref().is_some_and(|span| {
                    span.file.0 == file
                        && disproved.iter().any(|answer| {
                            answer.source == source
                                && answer.target == target
                                && answer.site == *span
                        })
                })
        };
        if !edge.evidence.iter().any(contradicted) {
            continue;
        }
        let kept: Vec<RelationEvidence> = edge
            .evidence
            .iter()
            .filter(|record| !contradicted(record))
            .cloned()
            .collect();
        if kept.iter().any(|record| record.source_span.is_some()) {
            narrowed.push(Relation {
                evidence: kept,
                ..edge.clone()
            });
        } else {
            retired.push(edge.clone());
        }
    }
    (retired, narrowed)
}

/// The rules a file's definitions pass records a `References` edge under,
/// at an identifier in the file it asked about.
///
/// A site recorded as undecided keeps its record under
/// [`kin_model::LSP_REFERENCE_UNDECIDED_RULE`], and is owned the same way, so
/// a later pass over new inputs asks about it again and a decisive answer
/// replaces or retires it.
const DEFINITION_PASS_RULES: [&str; 4] = [
    call_sites::DEFINITION_RULE,
    call_sites::DEFINITION_ALIAS_RULE,
    "lsp_member_on_module",
    kin_model::LSP_REFERENCE_UNDECIDED_RULE,
];

/// Reference proofs are settled per exact owned evidence site. A pass that
/// failed to reproduce a positive answer does not contradict it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ReferenceUncertainty {
    /// Sites still owed a decisive answer.
    pub(crate) sites: usize,
    pub(crate) retryable: bool,
    /// Sites this pass recorded as undecided, because the passes the bound
    /// allows over unchanged inputs gave none of them a decisive answer.
    pub(crate) settled: usize,
}

/// `record` restamped as an undecided site: the same span and token, under
/// [`kin_model::LSP_REFERENCE_UNDECIDED_RULE`].
fn undecided(record: &RelationEvidence) -> RelationEvidence {
    RelationEvidence {
        parser_rule: Some(kin_model::LSP_REFERENCE_UNDECIDED_RULE.to_string()),
        ..record.clone()
    }
}

/// `record` as a decided site: an undecided record a decisive answer now
/// backs goes back to the definitions pass's rule, and any other record is
/// kept as it is.
fn decided(record: &RelationEvidence) -> RelationEvidence {
    if record.parser_rule.as_deref() == Some(kin_model::LSP_REFERENCE_UNDECIDED_RULE) {
        RelationEvidence {
            parser_rule: Some(call_sites::DEFINITION_RULE.to_string()),
            ..record.clone()
        }
    } else {
        record.clone()
    }
}

/// Settle the `References` evidence a file's callers hold against what this
/// pass answered at each owned site.
///
/// `held` is read from the graph after this pass's own relations landed, so it
/// carries this pass's evidence beside whatever earlier passes left. A record
/// this pass produced again, on an edge from the same caller to the same
/// declaration at the same site, is current evidence and stays, whatever else
/// the site's answer named: a definition answer with two locations mints an
/// edge to each, and a member bound on a module beside an outside definition
/// mints one while the site answer leaves the repository. Asking again over
/// the same inputs gets the same answer, so such a record is no obligation.
///
/// A record this pass did not produce is settled by the site's fresh answers:
/// one unique positive answer naming its declaration keeps it, one naming
/// another declaration contradicts it and a retracting pass removes it, and
/// anything else (no answer, answers that disagree, several targets) leaves it
/// in place as uncertain. Uncertainty is retryable when the pass did not
/// finish or the site's question failed transiently.
///
/// `settle` says this pass is the last the bound allows over unchanged inputs
/// (see [`crate::owed_enrichment::reference_settles`]). Then a site left
/// without a decisive answer, by a pass that finished and with no transient
/// failure at the site, is recorded as undecided instead of owed: its record
/// stays on its edge under [`kin_model::LSP_REFERENCE_UNDECIDED_RULE`], so the
/// reference is still served and every answer that reads it names the site.
///
/// Returns the edges to retire, the edges narrowed, and the uncertainty left.
pub(crate) fn stale_references(
    pass: &FilePass<'_>,
    held: &[Relation],
    retract: bool,
    settle: bool,
) -> (Vec<Relation>, Vec<Relation>, ReferenceUncertainty) {
    let mut retired = Vec::new();
    let mut narrowed = Vec::new();
    let mut uncertain = ReferenceUncertainty::default();
    let callers: HashSet<EntityId> = pass
        .entities
        .iter()
        .filter(|entity| entity.span.is_some())
        .map(|entity| entity.id)
        .collect();
    let owned = |record: &RelationEvidence| {
        record
            .source_span
            .as_ref()
            .is_some_and(|span| span.file.0 == pass.file)
            && record
                .parser_rule
                .as_deref()
                .is_some_and(|rule| DEFINITION_PASS_RULES.contains(&rule))
    };
    for edge in held {
        let GraphNodeId::Entity(caller) = edge.src else {
            continue;
        };
        if edge.kind != RelationKind::References
            || edge.origin != RelationOrigin::Lsp
            || !callers.contains(&caller)
        {
            continue;
        }
        let mut kept = Vec::new();
        for record in &edge.evidence {
            if !owned(record) {
                kept.push(record.clone());
                continue;
            }
            let span = record.source_span.as_ref().expect("owned site has a span");
            // Produced again by this pass: current, and owed nothing. Without
            // this, a cold sweep held back every file with one ambiguous site
            // as permanently uncertain over evidence it had just minted, and
            // no later sweep could finish it, since the same inputs give the
            // same answer.
            let reproduced = pass.produced_references.iter().any(|fresh| {
                fresh.kind == RelationKind::References
                    && fresh.origin == RelationOrigin::Lsp
                    && fresh.src == edge.src
                    && fresh.dst == edge.dst
                    && fresh
                        .evidence
                        .iter()
                        .any(|proof| owned(proof) && proof.source_span.as_ref() == Some(span))
            });
            if reproduced {
                // A fresh record at this site on this edge now proves it, so
                // an undecided one beside it says nothing more.
                if record.parser_rule.as_deref() != Some(kin_model::LSP_REFERENCE_UNDECIDED_RULE) {
                    kept.push(record.clone());
                }
                continue;
            }
            let mut targets = Vec::new();
            for fresh in pass.produced_references {
                if fresh.kind == RelationKind::References
                    && fresh.origin == RelationOrigin::Lsp
                    && fresh.src == edge.src
                    && fresh
                        .evidence
                        .iter()
                        .any(|proof| owned(proof) && proof.source_span.as_ref() == Some(span))
                    && !targets.contains(&Some(fresh.dst))
                {
                    targets.push(Some(fresh.dst));
                }
            }
            for answer in pass.answers {
                if answer.source != caller
                    || answer.site != *span
                    || !DEFINITION_PASS_RULES.contains(&answer.rule)
                {
                    continue;
                }
                let target = match answer.target {
                    kin_lsp::call_sites::SiteTarget::Entity(id) => Some(GraphNodeId::Entity(id)),
                    kin_lsp::call_sites::SiteTarget::Outside(_) => None,
                };
                if !targets.contains(&target) {
                    targets.push(target);
                }
            }
            let refusals: Vec<_> = pass
                .unproven
                .iter()
                .filter(|site| {
                    site.source == caller
                        && site.start_byte == span.start_byte
                        && site.end_byte == span.end_byte
                })
                .map(|site| site.answer)
                .collect();
            let disagreed = refusals.contains(&UnprovenAnswer::AnswersDisagree);
            if targets.len() == 1 && !disagreed {
                if targets[0] == Some(edge.dst) {
                    kept.push(decided(record));
                } else if !retract {
                    kept.push(record.clone());
                    uncertain.sites += 1;
                    uncertain.retryable = true;
                }
                // A different unique positive answer at this exact site is a
                // contradiction, not an absence of evidence.
            } else {
                let transient = !retract
                    || refusals.iter().any(|answer| {
                        matches!(
                            answer,
                            UnprovenAnswer::Timeout
                                | UnprovenAnswer::Crash
                                | UnprovenAnswer::ProtocolError
                        )
                    });
                if settle && !transient {
                    kept.push(undecided(record));
                    uncertain.settled += 1;
                } else {
                    kept.push(record.clone());
                    uncertain.sites += 1;
                    uncertain.retryable |= transient;
                }
            }
        }
        if kept == edge.evidence {
            continue;
        }
        if kept.iter().any(|record| record.source_span.is_some()) {
            let mut edge = edge.clone();
            edge.evidence = kept;
            narrowed.push(edge);
        } else {
            retired.push(edge.clone());
        }
    }
    (retired, narrowed, uncertain)
}

/// The key the daemon's site union orders an edge's evidence by.
fn union_order(record: &RelationEvidence) -> (String, u32, u32, u32, u32, String) {
    let span = record.source_span.as_ref();
    (
        span.map(|span| span.file.0.clone()).unwrap_or_default(),
        span.map_or(0, |span| span.start_line),
        span.map_or(0, |span| span.start_col),
        span.map_or(0, |span| span.end_line),
        span.map_or(0, |span| span.end_col),
        record.parser_rule.clone().unwrap_or_default(),
    )
}

/// Check every ledger against the edges its caller holds once `rewrite` is
/// applied to `held`, and keep only the ledgers that hold.
///
/// A ledger that does not hold proves a site no edge carries under its
/// context, which only a write the graph refused leaves behind. It is not
/// recorded, so its caller stays owed and the next sweep proves it again.
pub(crate) fn backed_ledgers(
    ledgers: &FileLedgers,
    pass: &FilePass<'_>,
    held: &[Relation],
    rewrite: &ProofRewrite,
) -> (Vec<CallSiteLedger>, Vec<(EntityId, String)>) {
    let mut after: BTreeMap<kin_model::RelationId, &Relation> =
        held.iter().map(|edge| (edge.id, edge)).collect();
    for edge in &rewrite.retired {
        after.remove(&edge.id);
    }
    for edge in &rewrite.written {
        after.insert(edge.id, edge);
    }
    let mut by_caller: HashMap<EntityId, Vec<&Relation>> = HashMap::new();
    for edge in after.values() {
        if let GraphNodeId::Entity(caller) = edge.src {
            by_caller.entry(caller).or_default().push(edge);
        }
    }
    let starts: HashMap<EntityId, usize> = pass
        .entities
        .iter()
        .filter_map(|entity| Some((entity.id, entity.span.as_ref()?.start_byte)))
        .collect();
    let mut backed = Vec::with_capacity(ledgers.ledgers.len());
    let mut refused = Vec::new();
    for (caller, ledger) in &ledgers.ledgers {
        let edges = by_caller.get(caller).map(Vec::as_slice).unwrap_or(&[]);
        let start = starts.get(caller).copied().unwrap_or(0);
        match kin_model::ResolutionRecord::CallSites(ledger.clone())
            .validate()
            .and_then(|()| ledger.validate_backing(start, pass.file, edges.iter().copied()))
        {
            Ok(()) => backed.push(ledger.clone()),
            Err(error) => refused.push((*caller, error.to_string())),
        }
    }
    (backed, refused)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kin_lsp::call_sites::{SiteTarget, CALL_HIERARCHY_RULE, DEFINITION_RULE};
    use kin_model::{
        EntityKind, EntityMetadata, EntityRole, FilePathId, FingerprintAlgorithm,
        SemanticFingerprint, Visibility,
    };

    const FILE: &str = "app/run.py";
    /// `run` makes four calls: `send` through a receiver, `helper`, a call
    /// through a subscript with no callee token, and `cb`, a parameter.
    const TEXT: &str = "def run(self, cb):\n    self.adapter.send(x)\n    helper(y)\n    handlers[k](z)\n    cb()\n\ndef helper(y):\n    return y\n";

    fn entity(name: &str, text_of: &str, kind: EntityKind) -> Entity {
        let start = TEXT.find(text_of).expect("the declaration is in the text");
        let end = TEXT[start..]
            .find("\n\n")
            .map_or(TEXT.len(), |offset| start + offset);
        let start_line = TEXT[..start].matches('\n').count() as u32;
        let end_line = TEXT[..end].matches('\n').count() as u32;
        Entity {
            id: EntityId::new(),
            kind,
            name: name.to_string(),
            language: kin_model::LanguageId::Python,
            fingerprint: SemanticFingerprint {
                algorithm: FingerprintAlgorithm::V1TreeSitter,
                ast_hash: Hash256::from_bytes([1; 32]),
                signature_hash: Hash256::from_bytes([2; 32]),
                behavior_hash: Hash256::from_bytes([3; 32]),
                equivalence_hash: Hash256::from_bytes([0; 32]),
                stability_score: 1.0,
            },
            file_origin: Some(FilePathId::new(FILE)),
            span: Some(SourceSpan {
                file: FilePathId::new(FILE),
                start_byte: start,
                end_byte: end,
                start_line,
                start_col: 0,
                end_line,
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

    fn token(name: &str, nth: usize) -> SourceSpan {
        let start = TEXT
            .match_indices(&format!("{name}("))
            .nth(nth)
            .map(|(at, _)| at)
            .expect("the call is in the text");
        let line = TEXT[..start].matches('\n').count() as u32;
        SourceSpan {
            file: FilePathId::new(FILE),
            start_byte: start,
            end_byte: start + name.len(),
            start_line: line,
            start_col: 0,
            end_line: line,
            end_col: 0,
        }
    }

    struct World {
        run: Entity,
        helper: Entity,
        send: Entity,
        index: kin_lsp::EntityIndex,
        root: std::path::PathBuf,
        context: ResolutionRecordId,
    }

    fn world() -> World {
        let run = entity("run", "def run", EntityKind::Function);
        let helper = entity("helper", "def helper", EntityKind::Function);
        let mut send = entity("Adapter.send", "def helper", EntityKind::Method);
        send.file_origin = Some(FilePathId::new("app/adapter.py"));
        if let Some(span) = send.span.as_mut() {
            span.file = FilePathId::new("app/adapter.py");
        }
        let root = std::path::PathBuf::from("/work/repo");
        let refs: Vec<kin_lsp::EntityRef> = [&run, &helper]
            .into_iter()
            .map(|entity| crate::daemon::lsp_entity_ref(entity, FILE).expect("a spanned entity"))
            .collect();
        World {
            index: kin_lsp::EntityIndex::new(refs, &root),
            run,
            helper,
            send,
            root,
            context: ResolutionRecordId(uuid::Uuid::from_u128(77)),
        }
    }

    fn uri(world: &World) -> String {
        kin_lsp::protocol::path_to_uri(&world.root.join(FILE))
    }

    fn pass<'a>(
        world: &'a World,
        uri: &'a str,
        entities: &'a [&'a Entity],
        answers: &'a [SiteAnswer],
        unproven: &'a [UnprovenSite],
        ending: PassEnding,
    ) -> FilePass<'a> {
        static NAMES: std::sync::OnceLock<ExternalNames> = std::sync::OnceLock::new();
        static NO_REFERENCES: std::sync::OnceLock<Vec<Relation>> = std::sync::OnceLock::new();
        FilePass {
            file: FILE,
            text: TEXT,
            uri,
            index: &world.index,
            entities,
            answers,
            names: NAMES.get_or_init(ExternalNames::new),
            unproven,
            recorded: &[],
            static_proofs: &[],
            outside_calls: &[],
            produced_references: NO_REFERENCES.get_or_init(Default::default),
            ending,
            not_in_build: false,
            context: world.context,
            body: Hash256::from_bytes([9; 32]),
        }
    }

    fn states(ledger: &CallSiteLedger) -> Vec<(String, &'static str)> {
        ledger
            .sites
            .iter()
            .map(|site| {
                let start = site.offset as usize;
                let at = TEXT.find("def run").unwrap() + start;
                (
                    TEXT[at..at + site.length as usize].to_string(),
                    site.state.wire(),
                )
            })
            .collect()
    }

    /// React's devtools renderer at the benchmark tree, cut down: `attach`
    /// declares its own `measureInstance` and calls it from its body and
    /// from a closure, beside a call to an import and one to a module-level
    /// function.
    const RENDERER: &str = "import {getDisplayName} from './utils';\n\
        export function attach(hook, rendererID, renderer, global) {\n\
        \x20 function measureInstance(instance) {\n\
        \x20   return getDisplayName(instance);\n\
        \x20 }\n\
        \x20 function updateSuspenseNode(fiberInstance) {\n\
        \x20   return measureInstance(fiberInstance);\n\
        \x20 }\n\
        \x20 const newSuspenseNode = createSuspenseNode(hook);\n\
        \x20 newSuspenseNode.rects = measureInstance(hook);\n\
        \x20 return updateSuspenseNode(renderer);\n\
        }\n\
        \n\
        function createSuspenseNode(instance) {\n\
        \x20 return {rects: null, instance};\n\
        }\n";

    /// The state each call of `attach` gets when the passes over the
    /// renderer end as `ending` with `unproven` and `answers` on record.
    fn renderer_states(
        answers: &[SiteAnswer],
        unproven: &[UnprovenSite],
        ending: PassEnding,
        not_in_build: bool,
    ) -> (Entity, Vec<(String, &'static str)>) {
        let file = "packages/react-devtools-shared/src/backend/fiber/renderer.js";
        let start = RENDERER.find("export function attach").unwrap();
        let end = RENDERER.find("\n\nfunction createSuspenseNode").unwrap();
        let mut attach = entity("attach", "def run", EntityKind::Function);
        attach.language = kin_model::LanguageId::JavaScript;
        attach.file_origin = Some(FilePathId::new(file));
        attach.span = Some(SourceSpan {
            file: FilePathId::new(file),
            start_byte: start,
            end_byte: end,
            start_line: RENDERER[..start].matches('\n').count() as u32,
            start_col: 0,
            end_line: RENDERER[..end].matches('\n').count() as u32,
            end_col: 0,
        });
        let root = std::path::PathBuf::from("/work/react");
        let index = kin_lsp::EntityIndex::new(
            vec![crate::daemon::lsp_entity_ref(&attach, file).expect("a spanned entity")],
            &root,
        );
        let uri = kin_lsp::protocol::path_to_uri(&root.join(file));
        let entities = [&attach];
        let no_references = Vec::new();
        let names = ExternalNames::new();
        let pass = FilePass {
            file,
            text: RENDERER,
            uri: &uri,
            index: &index,
            entities: &entities,
            answers,
            names: &names,
            unproven,
            recorded: &[],
            static_proofs: &[],
            outside_calls: &[],
            produced_references: &no_references,
            ending,
            not_in_build,
            context: ResolutionRecordId(uuid::Uuid::from_u128(78)),
            body: Hash256::from_bytes([9; 32]),
        };
        let ledgers = build_ledgers(&pass).expect("the renderer parses");
        let states = ledgers.ledgers[&attach.id]
            .sites
            .iter()
            .map(|site| {
                let at = start + site.offset as usize;
                (
                    RENDERER[at..at + site.length as usize].to_string(),
                    site.state.wire(),
                )
            })
            .collect();
        (attach, states)
    }

    /// Where the renderer makes its `nth` call of `name`, as a byte range of
    /// the file.
    fn renderer_call(name: &str, nth: usize) -> (usize, usize) {
        let start = RENDERER
            .match_indices(&format!("{name}("))
            .filter(|(at, _)| !RENDERER[..*at].ends_with("function "))
            .nth(nth)
            .map(|(at, _)| at)
            .expect("the call is in the text");
        (start, start + name.len())
    }

    #[test]
    fn a_call_through_a_name_the_caller_binds_is_a_binding_when_the_resolver_is_silent() {
        // The server answered nothing in this file, as on a Flow source.
        let (_, states) = renderer_states(&[], &[], PassEnding::Declined, false);
        let state_of = |name: &str| -> Vec<&'static str> {
            states
                .iter()
                .filter(|(callee, _)| callee == name)
                .map(|(_, state)| *state)
                .collect()
        };
        assert_eq!(
            state_of("measureInstance"),
            ["binding", "binding"],
            "{states:?}"
        );
        assert_eq!(state_of("updateSuspenseNode"), ["binding"], "{states:?}");
        // An import and a module-level function are not the caller's own.
        assert_eq!(state_of("getDisplayName"), ["unresolved"], "{states:?}");
        assert_eq!(state_of("createSuspenseNode"), ["unresolved"], "{states:?}");

        // Which name a scope binds needs no build and no working server.
        let (_, states) = renderer_states(&[], &[], PassEnding::Declined, true);
        let local: Vec<&str> = states
            .iter()
            .filter(|(callee, _)| callee == "measureInstance")
            .map(|(_, state)| *state)
            .collect();
        assert_eq!(local, ["binding", "binding"], "{states:?}");
        assert!(
            states
                .iter()
                .any(|(callee, state)| callee == "getDisplayName" && *state == "not_in_build"),
            "{states:?}"
        );
        let (_, states) =
            renderer_states(&[], &[], PassEnding::Stopped(ServerFailure::Timeout), false);
        assert!(
            states
                .iter()
                .filter(|(callee, _)| callee == "measureInstance")
                .all(|(_, state)| *state == "binding"),
            "{states:?}"
        );
        assert!(
            states
                .iter()
                .any(|(callee, state)| callee == "getDisplayName" && *state == "server_failed"),
            "{states:?}"
        );
    }

    #[test]
    fn a_resolvers_own_finding_at_a_locally_bound_callee_stands() {
        let (attach, _) = renderer_states(&[], &[], PassEnding::Declined, false);
        let (start, end) = renderer_call("measureInstance", 0);
        let unproven = |answer| UnprovenSite {
            source: attach.id,
            start_byte: start,
            end_byte: end,
            answer,
        };
        for (answer, state) in [
            (UnprovenAnswer::OutsideTheGraph, "unresolved"),
            (UnprovenAnswer::AnswersDisagree, "unresolved"),
            (UnprovenAnswer::Binding, "binding"),
            (UnprovenAnswer::NoAnswer, "binding"),
        ] {
            let found = renderer_states_for(&attach, &[unproven(answer)]);
            assert_eq!(found[0], state, "{answer:?}: {found:?}");
            assert_eq!(
                found[1], "binding",
                "the other call is untouched: {found:?}"
            );
        }
    }

    /// The states of the two `measureInstance` calls of `attach`, in source
    /// order, with `unproven` on record against that same entity.
    fn renderer_states_for(attach: &Entity, unproven: &[UnprovenSite]) -> Vec<&'static str> {
        let file = "packages/react-devtools-shared/src/backend/fiber/renderer.js";
        let root = std::path::PathBuf::from("/work/react");
        let index = kin_lsp::EntityIndex::new(
            vec![crate::daemon::lsp_entity_ref(attach, file).expect("a spanned entity")],
            &root,
        );
        let uri = kin_lsp::protocol::path_to_uri(&root.join(file));
        let entities = [attach];
        let no_references = Vec::new();
        let names = ExternalNames::new();
        let pass = FilePass {
            file,
            text: RENDERER,
            uri: &uri,
            index: &index,
            entities: &entities,
            answers: &[],
            names: &names,
            unproven,
            recorded: &[],
            static_proofs: &[],
            outside_calls: &[],
            produced_references: &no_references,
            ending: PassEnding::Declined,
            not_in_build: false,
            context: ResolutionRecordId(uuid::Uuid::from_u128(78)),
            body: Hash256::from_bytes([9; 32]),
        };
        let start = attach.span.as_ref().unwrap().start_byte;
        let ledgers = build_ledgers(&pass).expect("the renderer parses");
        let mut found: Vec<(u32, &'static str)> = ledgers.ledgers[&attach.id]
            .sites
            .iter()
            .filter(|site| {
                let at = start + site.offset as usize;
                &RENDERER[at..at + site.length as usize] == "measureInstance"
            })
            .map(|site| (site.offset, site.state.wire()))
            .collect();
        found.sort();
        found.into_iter().map(|(_, state)| state).collect()
    }

    #[test]
    fn the_census_reads_every_call_expression_the_parser_reads_once() {
        let text =
            "def run(self):\n    self.adapter.send(x)\n    helper(send(y))\n    handlers[k](z)\n";
        let calls = call_expressions("app/run.py", text).expect("python parses");
        let named: Vec<(Option<String>, u32)> = calls
            .iter()
            .map(|call| {
                (
                    call.callee.as_ref().map(|token| token.name.clone()),
                    call.line,
                )
            })
            .collect();
        assert!(
            named.contains(&(Some("send".to_string()), 1)),
            "the member call is read with its callee: {named:?}"
        );
        assert!(
            named.contains(&(Some("helper".to_string()), 2)),
            "{named:?}"
        );
        assert!(
            named.contains(&(Some("send".to_string()), 2)),
            "a call inside another call's arguments is its own expression: {named:?}"
        );
        let mut keys: Vec<(usize, usize)> = calls
            .iter()
            .map(|call| (call.start_byte, call.end_byte))
            .collect();
        let before = keys.len();
        keys.dedup();
        assert_eq!(keys.len(), before, "each expression is read once");
        for call in &calls {
            if let Some(token) = &call.callee {
                assert_eq!(&text[token.start_byte..token.end_byte], token.name);
                assert!(call.start_byte <= token.start_byte && token.end_byte <= call.end_byte);
            }
        }

        let text = "export function main(): void {\n  const r = build(1);\n  r.run();\n  new Thing();\n}\n";
        let calls = call_expressions("src/main.ts", text).expect("typescript parses");
        let names: Vec<String> = calls
            .iter()
            .filter_map(|call| call.callee.as_ref().map(|token| token.name.clone()))
            .collect();
        assert!(names.contains(&"build".to_string()), "{names:?}");
        assert!(names.contains(&"run".to_string()), "{names:?}");

        let text = "fn main() {\n    let v = compute(2);\n    v.apply();\n}\n";
        let calls = call_expressions("src/main.rs", text).expect("rust parses");
        assert!(
            calls.iter().any(|call| call
                .callee
                .as_ref()
                .is_some_and(|token| token.name == "compute")),
            "{calls:?}"
        );

        assert_eq!(call_expressions("README", "text"), None, "no adapter");
    }

    #[test]
    fn the_reading_names_the_parsers_call_site_count_by_its_own_key() {
        assert_eq!(
            kin_model::call_site_reading::FILE_PARSED_CALL_SITES_KEY,
            kin_parser::FILE_PARSED_CALL_SITES_KEY
        );
    }

    #[test]
    fn a_reference_only_an_earlier_definitions_pass_produced_is_stale() {
        let world = world();
        let uri = uri(&world);
        let entities = [&world.run, &world.helper];
        let other = entity("Other.send", "def helper", EntityKind::Method);
        let reference = |target: &Entity, rule: &str, file: &str| {
            let mut span = token("send", 0);
            span.file = FilePathId::new(file);
            let mut edge = lsp_edge(&world.run, target, &[(span, rule, None)]);
            edge.kind = RelationKind::References;
            edge.id =
                kin_model::RelationId::resolver(RelationKind::References, &edge.src, &edge.dst);
            edge
        };
        let old = reference(&other, DEFINITION_RULE, FILE);
        let current = reference(&world.send, DEFINITION_RULE, FILE);
        let queried = reference(&other, kin_model::LSP_REFERENCES_RULE, FILE);
        let produced = vec![current.clone()];
        let mut complete = pass(&world, &uri, &entities, &[], &[], PassEnding::Complete);
        complete.produced_references = &produced;
        let mut mixed = reference(&other, DEFINITION_RULE, FILE);
        mixed.src = GraphNodeId::Entity(world.helper.id);
        mixed.id =
            kin_model::RelationId::resolver(RelationKind::References, &mixed.src, &mixed.dst);
        mixed.evidence.push(queried.evidence[0].clone());
        let mut helper_current = current.clone();
        helper_current.src = mixed.src;
        let produced = vec![produced[0].clone(), helper_current];
        complete.produced_references = &produced;
        let held = [old.clone(), current, queried, mixed.clone()];
        let (retired, narrowed, _) = stale_references(&complete, &held, true, false);
        assert_eq!(
            retired,
            [old],
            "the definitions pass's own edge that this pass did not produce is retired"
        );
        assert_eq!(
            narrowed.len(),
            1,
            "an edge another pass also recorded is narrowed"
        );
        assert_eq!(narrowed[0].id, mixed.id);
        assert_eq!(
            narrowed[0].evidence,
            [mixed.evidence[1].clone()],
            "to what the other pass recorded"
        );
        let (retired, narrowed, _) = stale_references(&complete, &held, false, false);
        assert!(retired.is_empty() && narrowed.is_empty());
    }

    /// The last pass the bound allows over unchanged inputs records a site with
    /// no decisive answer as undecided rather than owing it again: the record
    /// stays on its edge, restamped, and the file owes nothing for it. A pass
    /// that did not finish, or a site whose question failed transiently, still
    /// owes it. A later decisive answer takes the undecided stamp off, and a
    /// pass that produces the site again drops the undecided record beside it.
    #[test]
    fn a_reference_site_left_undecided_by_the_last_allowed_pass_settles() {
        let world = world();
        let uri = uri(&world);
        let entities = [&world.run];
        let a = token("send", 0);
        let b = token("helper", 0);
        let mut old = lsp_edge(
            &world.run,
            &world.send,
            &[
                (a.clone(), DEFINITION_RULE, None),
                (b.clone(), DEFINITION_RULE, None),
            ],
        );
        old.kind = RelationKind::References;
        old.id = kin_model::RelationId::resolver(old.kind, &old.src, &old.dst);
        let mut fresh = old.clone();
        fresh.evidence.truncate(1);
        let produced = vec![fresh];
        let no_answer = [UnprovenSite {
            source: world.run.id,
            start_byte: b.start_byte,
            end_byte: b.end_byte,
            answer: UnprovenAnswer::NoAnswer,
        }];
        let mut p = pass(
            &world,
            &uri,
            &entities,
            &[],
            &no_answer,
            PassEnding::Complete,
        );
        p.produced_references = &produced;

        // Not yet the last allowed pass: the site is owed, as before.
        let (_, narrowed, owed) = stale_references(&p, &[old.clone()], true, false);
        assert!(narrowed.is_empty());
        assert_eq!((owed.sites, owed.settled, owed.retryable), (1, 0, false));

        // The last allowed pass: the site settles as undecided.
        let (retired, narrowed, settled) = stale_references(&p, &[old.clone()], true, true);
        assert!(retired.is_empty());
        assert_eq!((settled.sites, settled.settled), (0, 1));
        assert_eq!(narrowed.len(), 1);
        assert_eq!(narrowed[0].evidence[0], old.evidence[0]);
        assert_eq!(
            narrowed[0].evidence[1].parser_rule.as_deref(),
            Some(kin_model::LSP_REFERENCE_UNDECIDED_RULE)
        );
        assert_eq!(
            narrowed[0].evidence[1].source_span,
            old.evidence[1].source_span
        );
        let held_undecided = narrowed[0].clone();

        // A pass that did not finish, or a transient failure at the site,
        // settles nothing.
        let (_, _, unfinished) = stale_references(&p, &[old.clone()], false, true);
        assert_eq!((unfinished.sites, unfinished.settled), (1, 0));
        assert!(unfinished.retryable);
        let timed_out = [UnprovenSite {
            answer: UnprovenAnswer::Timeout,
            ..no_answer[0].clone()
        }];
        p.unproven = &timed_out;
        let (_, _, transient) = stale_references(&p, &[old.clone()], true, true);
        assert_eq!((transient.sites, transient.settled), (1, 0));
        assert!(transient.retryable);

        // An undecided site is asked about again on new inputs. Still no
        // answer: it is owed again until the bound settles it again.
        p.unproven = &no_answer;
        let (_, narrowed, reopened) =
            stale_references(&p, std::slice::from_ref(&held_undecided), true, false);
        assert!(narrowed.is_empty());
        assert_eq!((reopened.sites, reopened.settled), (1, 0));

        // A decisive answer naming the record's declaration decides it.
        let decisive = [SiteAnswer {
            source: world.run.id,
            site: b.clone(),
            target: SiteTarget::Entity(world.send.id),
            rule: DEFINITION_RULE,
        }];
        p.answers = &decisive;
        p.unproven = &[];
        let (_, narrowed, decided) =
            stale_references(&p, std::slice::from_ref(&held_undecided), true, false);
        assert_eq!((decided.sites, decided.settled), (0, 0));
        assert_eq!(narrowed.len(), 1);
        assert_eq!(
            narrowed[0].evidence[1].parser_rule.as_deref(),
            Some(DEFINITION_RULE)
        );

        // A pass that produces the site again drops the undecided record.
        p.answers = &[];
        let mut both = held_undecided.clone();
        both.evidence.push(old.evidence[1].clone());
        let reproduced = vec![old.clone()];
        p.produced_references = &reproduced;
        let (_, narrowed, proven) = stale_references(&p, &[both], true, false);
        assert_eq!((proven.sites, proven.settled), (0, 0));
        assert_eq!(narrowed.len(), 1);
        assert_eq!(narrowed[0].evidence, old.evidence);
    }

    #[test]
    fn mixed_reference_sites_need_their_own_positive_answers() {
        let world = world();
        let uri = uri(&world);
        let entities = [&world.run];
        let a = token("send", 0);
        let b = token("helper", 0);
        let mut old = lsp_edge(
            &world.run,
            &world.send,
            &[
                (a.clone(), DEFINITION_RULE, None),
                (b.clone(), DEFINITION_RULE, None),
                (b.clone(), kin_model::LSP_REFERENCES_RULE, None),
            ],
        );
        old.kind = RelationKind::References;
        old.id = kin_model::RelationId::resolver(old.kind, &old.src, &old.dst);
        let mut fresh = old.clone();
        fresh.evidence.truncate(1);
        let produced = vec![fresh];
        let unknown = [UnprovenSite {
            source: world.run.id,
            start_byte: b.start_byte,
            end_byte: b.end_byte,
            answer: UnprovenAnswer::NoAnswer,
        }];
        let mut p = pass(&world, &uri, &entities, &[], &unknown, PassEnding::Complete);
        p.produced_references = &produced;
        let (retired, narrowed, unknown) = stale_references(&p, &[old.clone()], true, false);
        assert!(retired.is_empty() && narrowed.is_empty());
        assert_eq!(unknown.sites, 1);
        let different = SiteAnswer {
            source: world.run.id,
            site: b.clone(),
            target: SiteTarget::Entity(world.helper.id),
            rule: DEFINITION_RULE,
        };
        let answers = [different];
        p.answers = &answers;
        p.unproven = &[];
        let (retired, narrowed, unknown) = stale_references(&p, &[old.clone()], true, false);
        assert!(retired.is_empty());
        assert_eq!(unknown.sites, 0);
        assert_eq!(narrowed.len(), 1);
        assert_eq!(
            narrowed[0].evidence,
            [old.evidence[0].clone(), old.evidence[2].clone()]
        );
        let (_, _, deferred) = stale_references(&p, &[old.clone()], false, false);
        assert_eq!(deferred.sites, 1);
        assert!(deferred.retryable);
        // Two fresh answers at the site, neither naming the record's
        // declaration, leave it uncertain.
        let other = entity("Other.send", "def helper", EntityKind::Method);
        let mut to_helper = produced[0].clone();
        to_helper.dst = GraphNodeId::Entity(world.helper.id);
        let mut to_other = produced[0].clone();
        to_other.dst = GraphNodeId::Entity(other.id);
        let conflicted = vec![to_helper.clone(), to_other];
        p.produced_references = &conflicted;
        let (_, _, unknown) = stale_references(&p, &[old.clone()], true, false);
        assert_eq!(unknown.sites, 1);
        // One of them naming it is this pass producing the record again, so it
        // stays and is owed nothing.
        let reproduced = vec![produced[0].clone(), to_helper];
        p.produced_references = &reproduced;
        let (_, _, unknown) = stale_references(&p, &[old.clone()], true, false);
        assert_eq!(unknown.sites, 0);
        let disagree = [UnprovenSite {
            source: world.run.id,
            start_byte: b.start_byte,
            end_byte: b.end_byte,
            answer: UnprovenAnswer::AnswersDisagree,
        }];
        p.unproven = &disagree;
        p.produced_references = &produced;
        let (retired, narrowed, unknown) = stale_references(&p, &[old], true, false);
        assert!(retired.is_empty() && narrowed.is_empty());
        assert_eq!(unknown.sites, 1);
    }

    /// A sweep reads the edges a file's callers hold after its own relations
    /// landed, so what it settles includes what it just minted. A definition
    /// answer that disagrees with itself, one that names two declarations, and
    /// a member bound on a module beside a site answer that leaves the
    /// repository each mint positive References at a site with no unique
    /// answer. That evidence is current: settling it as uncertain held back
    /// 97 of express's 138 files on a cold sweep, and since the same inputs
    /// give the same answer, no later sweep could finish them.
    #[test]
    fn references_this_pass_minted_at_an_ambiguous_site_are_owed_nothing() {
        let world = world();
        let uri = uri(&world);
        let entities = [&world.run];
        let site = token("send", 0);
        let reference = |target: &Entity, rule: &str| {
            let mut edge = lsp_edge(&world.run, target, &[(site.clone(), rule, None)]);
            edge.kind = RelationKind::References;
            edge.id = kin_model::RelationId::resolver(edge.kind, &edge.src, &edge.dst);
            edge
        };
        let disagree = [UnprovenSite {
            source: world.run.id,
            start_byte: site.start_byte,
            end_byte: site.end_byte,
            answer: UnprovenAnswer::AnswersDisagree,
        }];
        let outside = [SiteAnswer {
            source: world.run.id,
            site: site.clone(),
            target: SiteTarget::Outside(kin_lsp::call_sites::OutsideLocation {
                uri: "file:///work/repo/node_modules/router/index.js".into(),
                range: kin_lsp::call_sites::LocationRange {
                    start_line: 0,
                    start_character: 0,
                    end_line: 0,
                    end_character: 4,
                },
            }),
            rule: DEFINITION_RULE,
        }];
        let one = vec![reference(&world.send, DEFINITION_RULE)];
        let two = vec![
            reference(&world.send, DEFINITION_RULE),
            reference(&world.helper, DEFINITION_RULE),
        ];
        let member = vec![reference(&world.send, "lsp_member_on_module")];
        let cases: [(&str, &Vec<Relation>, &[SiteAnswer], &[UnprovenSite]); 3] = [
            ("answers that disagree", &one, &[], &disagree),
            ("an answer naming two declarations", &two, &[], &disagree),
            (
                "a module member beside an outside answer",
                &member,
                &outside,
                &[],
            ),
        ];
        for (shape, minted, answers, unproven) in cases {
            for retract in [true, false] {
                let mut p = pass(
                    &world,
                    &uri,
                    &entities,
                    answers,
                    unproven,
                    PassEnding::Complete,
                );
                p.produced_references = minted;
                let (retired, narrowed, uncertain) = stale_references(&p, minted, retract, false);
                assert!(
                    retired.is_empty() && narrowed.is_empty(),
                    "{shape}: evidence this pass minted stays as it is"
                );
                assert_eq!(
                    uncertain,
                    ReferenceUncertainty::default(),
                    "{shape} (retract {retract}): evidence this pass minted is owed nothing"
                );
            }
        }
        // The control: the same edge retained from an earlier pass, which this
        // pass did not produce again, stays and is still owed, permanently,
        // because the answers at its site disagree.
        let none = Vec::new();
        let mut p = pass(
            &world,
            &uri,
            &entities,
            &[],
            &disagree,
            PassEnding::Complete,
        );
        p.produced_references = &none;
        let (retired, narrowed, uncertain) = stale_references(&p, &one, true, false);
        assert!(retired.is_empty() && narrowed.is_empty());
        assert_eq!(
            uncertain,
            ReferenceUncertainty {
                sites: 1,
                retryable: false,
                settled: 0,
            }
        );
    }

    #[test]
    fn a_call_site_the_graph_records_is_counted_whatever_recorded_it() {
        // `name()` inside a macro's token tree is no call the parser's
        // relations carry, and the linker records one there.
        let text = "fn build() {\n    quote! { async fn #name() {} };\n}\n";
        let at = text.find("name()").unwrap();
        let plain = call_expressions("src/lib.rs", text).unwrap();
        assert!(plain.iter().all(|call| call.start_byte != at), "{plain:?}");
        let with = call_expressions_with("src/lib.rs", text, &[(at, at + 6, 1)]).unwrap();
        let call = with
            .iter()
            .find(|call| call.start_byte == at)
            .expect("the recorded site is counted");
        assert_eq!(
            call.callee.as_ref().map(|token| token.name.as_str()),
            Some("name")
        );
        assert_eq!(call.line, 1);
        let again =
            call_expressions_with("src/lib.rs", text, &[(at, at + 6, 1), (at, at + 6, 1)]).unwrap();
        assert_eq!(again.len(), with.len(), "a site recorded twice is one site");
    }

    #[test]
    fn annotation_gap_containers_are_not_additional_calls() {
        let text =
            "def run(p: Annotated[str, Doc(\"description\")] = None):\n    return helper(p)\n";
        let calls = call_expressions("run.py", text).unwrap();
        let names: Vec<_> = calls
            .iter()
            .map(|call| call.callee.as_ref().map(|token| token.name.as_str()))
            .collect();
        assert_eq!(
            names,
            [Some("Doc"), Some("helper")],
            "a parameter gap is coverage evidence, not another call"
        );
    }

    #[test]
    fn tagged_and_interpolated_calls_keep_exact_callee_tokens() {
        let text = "function run(value) { return sql`select ${format(value.replace(/[()]/g, ''), sql`nested ${inner(value)}`)}`; }";
        let calls = call_expressions("run.ts", text).unwrap();
        let names: Vec<_> = calls
            .iter()
            .map(|call| call.callee.as_ref().map(|token| token.name.as_str()))
            .collect();
        assert_eq!(
            names,
            [
                Some("sql"),
                Some("format"),
                Some("replace"),
                Some("sql"),
                Some("inner")
            ]
        );
        for call in calls {
            let token = call.callee.unwrap();
            assert_eq!(&text[token.start_byte..token.end_byte], token.name);
        }
    }

    #[test]
    fn syntax_tokens_match_definition_proofs_and_keep_gap_certificates() {
        use kin_model::EntityStore;
        for (file, text, names) in [
            (
                "run.py",
                "def run(p: Annotated[str, Doc(\"description\")] = None):\n    return helper(p)\n",
                vec!["Doc", "helper"],
            ),
            (
                "run.ts",
                "function run(value) { return sql`select ${format(value.replace(/[()]/g, ''))}`; }",
                vec!["sql", "format", "replace"],
            ),
            (
                "run.js",
                "function run(value) { return `result ${format(value.replace(/[{}]/g, ''))}`; }",
                vec!["format", "replace"],
            ),
        ] {
            let mut world = world();
            world.run.file_origin = Some(FilePathId::new(file));
            world.run.span = Some(SourceSpan {
                file: FilePathId::new(file),
                start_byte: 0,
                end_byte: text.len(),
                start_line: 0,
                start_col: 0,
                end_line: text.matches('\n').count() as u32,
                end_col: 0,
            });
            world.index = kin_lsp::EntityIndex::new(
                vec![crate::daemon::lsp_entity_ref(&world.run, file).unwrap()],
                &world.root,
            );
            let uri = kin_lsp::protocol::path_to_uri(&world.root.join(file));
            let answers: Vec<_> = names
                .iter()
                .map(|name| {
                    let at = text.find(name).unwrap();
                    SiteAnswer {
                        source: world.run.id,
                        site: SourceSpan {
                            file: FilePathId::new(file),
                            start_byte: at,
                            end_byte: at + name.len(),
                            start_line: text[..at].matches('\n').count() as u32,
                            start_col: 0,
                            end_line: text[..at].matches('\n').count() as u32,
                            end_col: 0,
                        },
                        target: SiteTarget::Entity(world.send.id),
                        rule: DEFINITION_RULE,
                    }
                })
                .collect();
            let context_record =
                kin_model::ResolutionRecord::ProofContext(kin_model::ProofContext {
                    language: if file.ends_with(".py") {
                        kin_model::LanguageId::Python
                    } else if file.ends_with(".ts") {
                        kin_model::LanguageId::TypeScript
                    } else {
                        kin_model::LanguageId::JavaScript
                    },
                    resolver: "lsp:fixture".into(),
                    resolver_version: "1".into(),
                    configuration_hash: Hash256::from_bytes([1; 32]),
                    environment_hash: Hash256::from_bytes([2; 32]),
                    environment_summary: "syntax call fixture".into(),
                });
            world.context = context_record.id();
            let entities = [&world.run];
            let mut captured = pass(&world, &uri, &entities, &answers, &[], PassEnding::Complete);
            captured.file = file;
            captured.text = text;
            let ledgers = build_ledgers(&captured).unwrap();
            let run = &ledgers.ledgers[&world.run.id];
            assert_eq!(
                run.census as usize,
                names.len(),
                "{file}: actual calls only"
            );
            assert!(
                run.sites
                    .iter()
                    .all(|site| site.state.wire() == "proven_target"),
                "{file}: a definition proof at each exact callee token settles its site"
            );
            let rewritten = rewrite_proofs(&ledgers, &captured, &[], false);
            run.validate_backing(0, file, rewritten.written.iter())
                .unwrap();
            let graph = kin_db::InMemoryGraph::new();
            graph
                .upsert_entities_batch(&[world.run.clone(), world.send.clone()])
                .unwrap();
            graph
                .apply_resolution_record_deltas(&[kin_model::ResolutionRecordDelta::Added {
                    new: context_record,
                }])
                .unwrap();
            for relation in &rewritten.written {
                graph.upsert_relation(relation).unwrap();
            }
            graph
                .apply_resolution_record_deltas(&[kin_model::ResolutionRecordDelta::Added {
                    new: kin_model::ResolutionRecord::CallSites(run.clone()),
                }])
                .unwrap();
            let root = tempfile::tempdir().unwrap();
            let snapshot = root.path().join("snapshot.bin");
            kin_db::SnapshotManager::save_graph(&snapshot, &graph).unwrap();
            drop(graph);
            let reopened = kin_db::SnapshotManager::open_without_text_index(&snapshot).unwrap();
            let records = reopened.graph().list_resolution_records();
            let stored = records
                .iter()
                .find_map(|record| record.as_call_sites())
                .unwrap();
            assert_eq!(stored, run, "{file}: exact proof keys survive cold reopen");
            let backing = reopened
                .graph()
                .get_all_relations_for_entity(&world.run.id)
                .unwrap();
            stored.validate_backing(0, file, backing.iter()).unwrap();
            if file.ends_with(".py") {
                let registry = kin_parser::AdapterRegistry::new();
                let adapter = registry.get_by_extension("py").unwrap();
                let tree = adapter.parse(text.as_bytes()).unwrap();
                let output = adapter
                    .extract(&tree, text.as_bytes(), &FilePathId::new(file))
                    .unwrap();
                assert!(
                    output
                        .relations
                        .iter()
                        .any(kin_parser::is_scoped_call_extraction_incomplete_marker),
                    "excluding a non-call census entry never discharges negative coverage evidence"
                );
            }
        }
    }

    #[test]
    fn every_call_expression_in_a_caller_has_exactly_one_state() {
        let world = world();
        let uri = uri(&world);
        let entities = [&world.run, &world.helper];
        let answers = [SiteAnswer {
            source: world.run.id,
            site: token("send", 0),
            target: SiteTarget::Entity(world.send.id),
            rule: DEFINITION_RULE,
        }];
        let cb = token("cb", 0);
        let unproven = [
            UnprovenSite {
                source: world.run.id,
                start_byte: cb.start_byte,
                end_byte: cb.end_byte,
                answer: UnprovenAnswer::Binding,
            },
            UnprovenSite {
                source: world.run.id,
                start_byte: token("helper", 0).start_byte,
                end_byte: token("helper", 0).end_byte,
                answer: UnprovenAnswer::Timeout,
            },
        ];
        let pass = pass(
            &world,
            &uri,
            &entities,
            &answers,
            &unproven,
            PassEnding::Complete,
        );
        let ledgers = build_ledgers(&pass).expect("the file parses");
        let run = &ledgers.ledgers[&world.run.id];
        assert_eq!(run.census as usize, run.sites.len());
        assert_eq!(
            states(run),
            [
                ("send".to_string(), "proven_target"),
                ("helper".to_string(), "server_failed"),
                ("handlers[k](z)".to_string(), "unresolved"),
                ("cb".to_string(), "binding"),
            ]
        );
        let helper = &ledgers.ledgers[&world.helper.id];
        assert_eq!(helper.census, 0, "a caller with no call has a ledger too");
        assert_eq!(ledgers.counts.expressions, 4);
        assert_eq!(ledgers.counts.unattributed, 0);
        kin_model::ResolutionRecord::CallSites(run.clone())
            .validate()
            .expect("keys sorted and unique, census equals sites");

        // The pass that stopped early leaves the sites it never asked at
        // server-failed, and a file no build compiles is not in build.
        let stopped = self::pass(
            &world,
            &uri,
            &entities,
            &[],
            &[],
            PassEnding::Stopped(ServerFailure::Timeout),
        );
        let ledgers = build_ledgers(&stopped).unwrap();
        assert_eq!(
            states(&ledgers.ledgers[&world.run.id])
                .iter()
                .map(|(_, state)| *state)
                .collect::<Vec<_>>(),
            [
                "server_failed",
                "server_failed",
                "unresolved",
                "server_failed"
            ]
        );
        let mut outside_build = self::pass(&world, &uri, &entities, &[], &[], PassEnding::Complete);
        outside_build.not_in_build = true;
        let ledgers = build_ledgers(&outside_build).unwrap();
        assert!(ledgers.ledgers[&world.run.id]
            .sites
            .iter()
            .all(|site| site.state.wire() == "not_in_build"));
    }

    /// The call expression whose callee is `name`, as the census reads it.
    fn expression(name: &str) -> (usize, usize) {
        let calls = call_expressions(FILE, TEXT).expect("python parses");
        let call = calls
            .iter()
            .find(|call| call.callee.as_ref().is_some_and(|token| token.name == name))
            .expect("the call is in the census");
        (call.start_byte, call.end_byte)
    }

    #[test]
    fn a_site_the_resolver_left_open_is_settled_by_the_linkers_proof_and_no_other() {
        let world = world();
        let uri = uri(&world);
        let entities = [&world.run, &world.helper];
        let (helper_start, helper_end) = expression("helper");
        let (send_start, send_end) = expression("send");
        let helper_token = token("helper", 0);
        let send_token = token("send", 0);
        let unproven = [
            UnprovenSite {
                source: world.run.id,
                start_byte: helper_token.start_byte,
                end_byte: helper_token.end_byte,
                answer: UnprovenAnswer::NoAnswer,
            },
            UnprovenSite {
                source: world.run.id,
                start_byte: send_token.start_byte,
                end_byte: send_token.end_byte,
                answer: UnprovenAnswer::OutsideTheGraph,
            },
        ];
        let proofs = [
            StaticProof {
                caller: world.run.id,
                start_byte: helper_start,
                end_byte: helper_end,
                target: world.helper.id,
            },
            // The resolver placed `send` outside the graph. A static proof
            // never overrides the resolver's own finding.
            StaticProof {
                caller: world.run.id,
                start_byte: send_start,
                end_byte: send_end,
                target: world.send.id,
            },
        ];
        let mut settled = pass(
            &world,
            &uri,
            &entities,
            &[],
            &unproven,
            PassEnding::Complete,
        );
        settled.static_proofs = &proofs;
        let ledgers = build_ledgers(&settled).unwrap();
        let run = &ledgers.ledgers[&world.run.id];
        assert_eq!(
            states(run),
            [
                ("send".to_string(), "unresolved"),
                ("helper".to_string(), "proven_static"),
                ("handlers[k](z)".to_string(), "unresolved"),
                ("cb".to_string(), "unresolved"),
            ]
        );
        let helper_site = run
            .sites
            .iter()
            .find(|site| site.state.wire() == "proven_static")
            .unwrap();
        assert_eq!(helper_site.state.static_target(), Some(world.helper.id));
        assert_eq!(
            helper_site.state.proven_node(),
            None,
            "a static proof is never carried by a language-server edge"
        );
        run.validate_backing(world.run.span.as_ref().unwrap().start_byte, FILE, [])
            .expect("a static site needs no language-server edge");

        // Two static proofs naming different targets at one site prove
        // neither.
        let conflicting = [
            proofs[0],
            StaticProof {
                target: world.send.id,
                ..proofs[0]
            },
        ];
        settled.static_proofs = &conflicting;
        let ledgers = build_ledgers(&settled).unwrap();
        assert_eq!(
            states(&ledgers.ledgers[&world.run.id])[1],
            ("helper".to_string(), "unresolved")
        );
    }

    /// A call the linker proved reaches no declaration of the repository
    /// settles as `proven_outside` where the resolver left it open, and
    /// nowhere else: not over the resolver's own finding, and not where a
    /// linker edge claims a repository target. Falsify by dropping the
    /// `outside_calls` arm: the `send` site stays `unresolved` and remains a
    /// leftover of every focal named `send`.
    #[test]
    fn a_call_the_linker_proved_goes_outside_settles_only_where_the_resolver_left_it_open() {
        let world = world();
        let uri = uri(&world);
        let entities = [&world.run, &world.helper];
        let (helper_start, helper_end) = expression("helper");
        let (send_start, send_end) = expression("send");
        let (cb_start, cb_end) = expression("cb");
        let open = |name: &str, answer| {
            let span = token(name, 0);
            UnprovenSite {
                source: world.run.id,
                start_byte: span.start_byte,
                end_byte: span.end_byte,
                answer,
            }
        };
        let unproven = [
            open("send", UnprovenAnswer::NoAnswer),
            open("helper", UnprovenAnswer::NoAnswer),
            open("cb", UnprovenAnswer::OutsideTheGraph),
        ];
        let outside = [
            (send_start, send_end),
            (helper_start, helper_end),
            (cb_start, cb_end),
        ];
        let proofs = [StaticProof {
            caller: world.run.id,
            start_byte: helper_start,
            end_byte: helper_end,
            target: world.helper.id,
        }];
        let mut settled = pass(
            &world,
            &uri,
            &entities,
            &[],
            &unproven,
            PassEnding::Complete,
        );
        settled.outside_calls = &outside;
        settled.static_proofs = &proofs;
        let ledgers = build_ledgers(&settled).unwrap();
        let run = &ledgers.ledgers[&world.run.id];
        assert_eq!(
            states(run),
            [
                ("send".to_string(), "proven_outside"),
                ("helper".to_string(), "proven_static"),
                ("handlers[k](z)".to_string(), "unresolved"),
                ("cb".to_string(), "unresolved"),
            ]
        );
        run.validate_backing(world.run.span.as_ref().unwrap().start_byte, FILE, [])
            .expect("a site proven outside needs no carrying edge");
    }

    /// A server that failed at a site is never recorded as having found no
    /// location there. A refusal reads `server_refused`, a server whose state
    /// failed reads `server_failed`, and only an empty answer or a decline
    /// reads `no_answer`. Falsify by mapping a refusal to `no_answer`: every
    /// site a wedged tsserver failed at on React said the server found
    /// nothing there, and was settled.
    #[test]
    fn a_server_that_failed_at_a_site_never_reads_as_finding_nothing() {
        let world = world();
        let uri = uri(&world);
        let entities = [&world.run, &world.helper];
        let at = |name: &str, answer| {
            let span = token(name, 0);
            UnprovenSite {
                source: world.run.id,
                start_byte: span.start_byte,
                end_byte: span.end_byte,
                answer,
            }
        };
        let reason_at = |unproven: &[UnprovenSite]| {
            let pass = pass(&world, &uri, &entities, &[], unproven, PassEnding::Complete);
            let ledgers = build_ledgers(&pass).expect("the file parses");
            let run = &ledgers.ledgers[&world.run.id];
            run.sites
                .iter()
                .find(|site| {
                    let start = TEXT.find("def run").unwrap() + site.offset as usize;
                    TEXT[start..start + site.length as usize].starts_with("send")
                })
                .map(|site| site.state.clone())
                .expect("the send site has a state")
        };
        for (answer, expected) in [
            (
                UnprovenAnswer::Refused,
                CallSiteState::Unresolved {
                    reason: UnresolvedReason::ServerRefused,
                },
            ),
            (
                UnprovenAnswer::Crash,
                CallSiteState::ServerFailed {
                    reason: ServerFailure::Crash,
                },
            ),
            (
                UnprovenAnswer::Declined,
                CallSiteState::Unresolved {
                    reason: UnresolvedReason::NoAnswer,
                },
            ),
            (
                UnprovenAnswer::NoAnswer,
                CallSiteState::Unresolved {
                    reason: UnresolvedReason::NoAnswer,
                },
            ),
        ] {
            assert_eq!(reason_at(&[at("send", answer)]), expected, "{answer:?}");
        }
        assert_eq!(
            reason_at(&[
                at("send", UnprovenAnswer::NoAnswer),
                at("send", UnprovenAnswer::Refused)
            ]),
            CallSiteState::Unresolved {
                reason: UnresolvedReason::ServerRefused,
            },
            "a refusal outranks an empty answer at the same site"
        );
        assert_eq!(
            reason_at(&[
                at("send", UnprovenAnswer::Refused),
                at("send", UnprovenAnswer::Crash)
            ]),
            CallSiteState::ServerFailed {
                reason: ServerFailure::Crash,
            },
            "and a server that stopped outranks a refusal"
        );
        assert_eq!(UnresolvedReason::ServerRefused.wire(), "server_refused");
    }

    fn lsp_edge(
        caller: &Entity,
        target: &Entity,
        sites: &[(SourceSpan, &str, Option<String>)],
    ) -> Relation {
        call_sites::proven_call(
            caller.id,
            target.id,
            sites
                .iter()
                .map(|(span, rule, token)| RelationEvidence {
                    token: token.clone(),
                    ..call_sites::site_evidence(rule, span.clone())
                })
                .collect(),
        )
    }

    #[test]
    fn a_call_census_retains_unanswered_property_evidence_without_restamping_it() {
        const SOURCE: &str =
            "def run(self):\n    assert self.request.endpoint is not None\n    self.adapter.send()\n";
        let mut world = world();
        world.run.span.as_mut().unwrap().end_byte = SOURCE.len();
        world.run.span.as_mut().unwrap().end_line = 3;
        world.index = kin_lsp::EntityIndex::new(
            vec![crate::daemon::lsp_entity_ref(&world.run, FILE).unwrap()],
            &world.root,
        );
        let uri = uri(&world);
        let entities = [&world.run];
        let cited = |name: &str| {
            let start = SOURCE.find(name).unwrap();
            let line = SOURCE[..start].matches('\n').count() as u32;
            SourceSpan {
                file: FilePathId::new(FILE),
                start_byte: start,
                end_byte: start + name.len(),
                start_line: line,
                start_col: (start - SOURCE[..start].rfind('\n').unwrap() - 1) as u32,
                end_line: line,
                end_col: (start - SOURCE[..start].rfind('\n').unwrap() - 1 + name.len()) as u32,
            }
        };
        let property = cited("endpoint");
        let explicit = cited("send");
        let old_context = format!("ctx:{}", uuid::Uuid::from_u128(5));
        let property_edge = lsp_edge(
            &world.run,
            &world.helper,
            &[(
                property.clone(),
                CALL_HIERARCHY_RULE,
                Some(old_context.clone()),
            )],
        );
        let contradicted = lsp_edge(
            &world.run,
            &world.helper,
            &[(explicit.clone(), CALL_HIERARCHY_RULE, Some(old_context))],
        );
        // One held edge has both an unmeasured property and an explicit call.
        // The latter's current positive answer names a different target.
        let mut held = property_edge.clone();
        held.evidence.extend(contradicted.evidence);
        let answers = [SiteAnswer {
            source: world.run.id,
            site: explicit,
            target: SiteTarget::Entity(world.send.id),
            rule: DEFINITION_RULE,
        }];
        let mut complete = pass(&world, &uri, &entities, &answers, &[], PassEnding::Complete);
        complete.text = SOURCE;
        complete.body = Hash256::from_bytes(kin_blobs::digest(SOURCE.as_bytes()).0);
        let ledgers = build_ledgers(&complete).unwrap();
        assert_eq!(ledgers.counts.expressions, 1);
        let ledger = &ledgers.ledgers[&world.run.id];
        assert_eq!(ledger.sites.len(), 1);
        let property_key = site_key(0, property.start_byte, property.end_byte).unwrap();
        assert!(ledger.site(property_key.0, property_key.1).is_none());
        assert!(!ledgers.answered.contains_key(&(
            world.run.id,
            property.start_byte,
            property.end_byte
        )));

        let rewrite = rewrite_proofs(&ledgers, &complete, std::slice::from_ref(&held), true);
        assert!(rewrite.retired.is_empty());
        assert_eq!(rewrite.retracted_records, 1);
        let retained = rewrite
            .written
            .iter()
            .find(|edge| edge.id == held.id)
            .unwrap();
        assert_eq!(
            retained, &property_edge,
            "the unmeasured evidence stays exact"
        );
        assert_ne!(
            retained.evidence[0].token,
            Some(world.context.context_token())
        );
        assert!(rewrite.written.iter().any(|edge| {
            edge.dst == GraphNodeId::Entity(world.send.id)
                && edge
                    .evidence
                    .iter()
                    .all(|evidence| evidence.token == Some(world.context.context_token()))
        }));
        let (backed, refused) =
            backed_ledgers(&ledgers, &complete, std::slice::from_ref(&held), &rewrite);
        assert!(refused.is_empty(), "{refused:?}");
        assert_eq!(backed.len(), 1);

        // With only the unmeasured property, the original edge is left exact:
        // there is no synthetic write that could promote its proof context.
        let mut legacy_property = property_edge.clone();
        legacy_property.evidence[0].token = None;
        for held in [&property_edge, &legacy_property] {
            let rewrite = rewrite_proofs(&ledgers, &complete, std::slice::from_ref(held), true);
            assert!(rewrite.retired.is_empty());
            assert_eq!(rewrite.retracted_records, 0);
            assert!(rewrite.written.iter().all(|edge| edge.id != held.id));
        }

        // A definite answer at that exact property token still has authority:
        // the same target is stamped, and a different target retires the old
        // proof. Merely completing the explicit-call census did neither.
        for target in [&world.helper, &world.send] {
            let direct = [
                answers[0].clone(),
                SiteAnswer {
                    source: world.run.id,
                    site: property.clone(),
                    target: SiteTarget::Entity(target.id),
                    rule: CALL_HIERARCHY_RULE,
                },
            ];
            let observed = FilePass {
                answers: &direct,
                ..complete
            };
            let observed_ledgers = build_ledgers(&observed).unwrap();
            let rewrite = rewrite_proofs(
                &observed_ledgers,
                &observed,
                std::slice::from_ref(&property_edge),
                true,
            );
            if target.id == world.helper.id {
                let current = rewrite
                    .written
                    .iter()
                    .find(|edge| edge.id == property_edge.id)
                    .unwrap();
                assert_eq!(
                    current.evidence[0].token,
                    Some(world.context.context_token())
                );
                assert!(rewrite.retired.is_empty());
            } else {
                assert_eq!(rewrite.retired, vec![property_edge.clone()]);
                assert_eq!(rewrite.retracted_records, 1);
            }
        }
    }

    #[test]
    fn a_file_proven_again_keeps_only_the_proofs_its_ledgers_hold() {
        let world = world();
        let uri = uri(&world);
        let entities = [&world.run, &world.helper];
        let other = entity("Other.send", "def helper", EntityKind::Method);
        // This pass proves `send` goes to `Adapter.send`, and says `helper`
        // got no answer.
        let answers = [SiteAnswer {
            source: world.run.id,
            site: token("send", 0),
            target: SiteTarget::Entity(world.send.id),
            rule: DEFINITION_RULE,
        }];
        let helper_token = token("helper", 0);
        let unproven = [UnprovenSite {
            source: world.run.id,
            start_byte: helper_token.start_byte,
            end_byte: helper_token.end_byte,
            answer: UnprovenAnswer::NoAnswer,
        }];
        // The graph holds an older proof of `send` into `Other.send` under
        // another context, a legacy proof of `helper` with no context, and a
        // call-hierarchy record of `send` into `Adapter.send` with no context.
        let stale = lsp_edge(
            &world.run,
            &other,
            &[(
                token("send", 0),
                DEFINITION_RULE,
                Some(format!("ctx:{}", uuid::Uuid::from_u128(5))),
            )],
        );
        let legacy = lsp_edge(
            &world.run,
            &world.helper,
            &[(helper_token.clone(), CALL_HIERARCHY_RULE, None)],
        );
        let current = lsp_edge(
            &world.run,
            &world.send,
            &[(token("send", 0), CALL_HIERARCHY_RULE, None)],
        );
        let held = [stale.clone(), legacy.clone(), current.clone()];

        let complete = pass(
            &world,
            &uri,
            &entities,
            &answers,
            &unproven,
            PassEnding::Complete,
        );
        let ledgers = build_ledgers(&complete).unwrap();
        let rewrite = rewrite_proofs(&ledgers, &complete, &held, true);
        let retired: Vec<_> = rewrite.retired.iter().map(|edge| edge.id).collect();
        assert!(
            retired.contains(&stale.id),
            "the other context's proof is retracted"
        );
        assert!(
            retired.contains(&legacy.id),
            "so is the legacy proof the pass settled otherwise"
        );
        assert_eq!(
            rewrite.settled_retirements,
            [stale.id].into_iter().collect(),
            "the unanswered legacy call is lost proof, not a proven retirement"
        );
        let written = rewrite
            .written
            .iter()
            .find(|edge| edge.id == current.id)
            .expect("the current proof is stamped");
        assert!(written
            .evidence
            .iter()
            .all(|record| record.token == Some(world.context.context_token())));
        let (backed, refused) = backed_ledgers(&ledgers, &complete, &held, &rewrite);
        assert!(refused.is_empty(), "{refused:?}");
        assert_eq!(backed.len(), 2);

        // A file whose passes failed is never retracted: the proofs are only
        // stamped and completed.
        let failed = pass(
            &world,
            &uri,
            &entities,
            &answers,
            &unproven,
            PassEnding::Stopped(ServerFailure::Timeout),
        );
        let ledgers = build_ledgers(&failed).unwrap();
        let rewrite = rewrite_proofs(&ledgers, &failed, &held, false);
        assert!(rewrite.retired.is_empty());
        assert!(rewrite.settled_retirements.is_empty());
        assert_eq!(rewrite.retracted_records, 0);

        // A proven site no edge carries is completed on a new edge.
        let rewrite = rewrite_proofs(&ledgers, &failed, &[], false);
        assert_eq!(rewrite.completed_sites, 1);
        let minted = &rewrite.written[0];
        assert_eq!(minted.dst, GraphNodeId::Entity(world.send.id));
        assert_eq!(
            minted.evidence[0].token,
            Some(world.context.context_token())
        );
        let (backed, refused) = backed_ledgers(&ledgers, &failed, &[], &rewrite);
        assert!(refused.is_empty(), "{refused:?}");
        assert_eq!(backed.len(), 2);
        // Without the completion the proof is unbacked and its ledger is
        // not recorded.
        let (backed, refused) = backed_ledgers(&ledgers, &failed, &[], &ProofRewrite::default());
        assert_eq!(backed.len(), 1);
        assert_eq!(refused[0].0, world.run.id);
    }

    #[test]
    fn census_credit_requires_positive_contradiction_at_every_retired_proof_site() {
        let world = world();
        let uri = uri(&world);
        let entities = [&world.run];
        let held = lsp_edge(
            &world.run,
            &world.send,
            &[
                (token("send", 0), DEFINITION_RULE, None),
                (token("helper", 0), DEFINITION_RULE, None),
            ],
        );
        let answer = |name: &str, target| SiteAnswer {
            source: world.run.id,
            site: token(name, 0),
            target,
            rule: DEFINITION_RULE,
        };
        let new_target = SiteTarget::Entity(world.helper.id);
        let cases = [
            (
                "all sites contradicted",
                vec![
                    answer("send", new_target.clone()),
                    answer("helper", new_target.clone()),
                ],
                true,
            ),
            (
                "one site unanswered",
                vec![answer("send", new_target.clone())],
                false,
            ),
            (
                "conflicting answers",
                vec![
                    answer("send", new_target.clone()),
                    answer("send", SiteTarget::Entity(world.send.id)),
                    answer("helper", new_target),
                ],
                false,
            ),
        ];
        for (reason, answers, credited) in cases {
            let pass = pass(&world, &uri, &entities, &answers, &[], PassEnding::Complete);
            let ledgers = build_ledgers(&pass).unwrap();
            let rewrite = rewrite_proofs(&ledgers, &pass, std::slice::from_ref(&held), true);
            assert_eq!(rewrite.retired, vec![held.clone()], "{reason}");
            assert_eq!(
                rewrite.settled_retirements.contains(&held.id),
                credited,
                "{reason}"
            );
        }

        // Answers do not override the census refusing this file or finding
        // more than one owner for its callee token.
        let answers = [
            answer("send", SiteTarget::Entity(world.helper.id)),
            answer("helper", SiteTarget::Entity(world.helper.id)),
        ];
        let admitted = pass(&world, &uri, &entities, &answers, &[], PassEnding::Complete);
        let outside_build = FilePass {
            not_in_build: true,
            ..admitted
        };
        let overlapping = Entity {
            id: EntityId::new(),
            ..world.run.clone()
        };
        let overlapping_entities = [&world.run, &overlapping];
        let overlapping_index = kin_lsp::EntityIndex::new(
            overlapping_entities
                .iter()
                .map(|entity| crate::daemon::lsp_entity_ref(entity, FILE).unwrap())
                .collect(),
            &world.root,
        );
        let ambiguous_owner = FilePass {
            index: &overlapping_index,
            entities: &overlapping_entities,
            ..admitted
        };
        for (reason, pass) in [
            ("outside build", outside_build),
            ("ambiguous owner", ambiguous_owner),
        ] {
            let ledgers = build_ledgers(&pass).unwrap();
            let rewrite = rewrite_proofs(&ledgers, &pass, std::slice::from_ref(&held), true);
            assert_eq!(rewrite.retired, vec![held.clone()], "{reason}");
            assert!(
                rewrite.settled_retirements.is_empty(),
                "{reason}: rejected census proof earns no credit"
            );
        }

        let outside = SiteTarget::Outside(kin_lsp::call_sites::OutsideLocation {
            uri: "file:///external/library.py".into(),
            range: kin_lsp::call_sites::LocationRange {
                start_line: 0,
                start_character: 0,
                end_line: 0,
                end_character: 4,
            },
        });
        let answers = [answer("send", outside.clone()), answer("helper", outside)];
        let pass = pass(&world, &uri, &entities, &answers, &[], PassEnding::Complete);
        let ledgers = build_ledgers(&pass).unwrap();
        let old_external = call_sites::proven_external_call(
            world.run.id,
            ExternalReferenceId(uuid::Uuid::from_u128(9)),
            held.evidence.clone(),
        );
        for (old, credited) in [(&held, true), (&old_external, false)] {
            let rewrite = rewrite_proofs(&ledgers, &pass, std::slice::from_ref(old), true);
            assert_eq!(rewrite.retired, vec![old.clone()]);
            assert_eq!(
                rewrite.settled_retirements.contains(&old.id),
                credited,
                "an unnamed outside answer only disproves a repository target"
            );
        }
    }

    fn shared_call_context() -> kin_model::ResolutionRecord {
        kin_model::ResolutionRecord::ProofContext(kin_model::ProofContext {
            language: kin_model::LanguageId::JavaScript,
            resolver: "lsp:tsserver".into(),
            resolver_version: "fixture".into(),
            configuration_hash: Hash256::from_bytes([1; 32]),
            environment_hash: Hash256::from_bytes([2; 32]),
            environment_summary: "shared implementation fixture".into(),
        })
    }

    #[test]
    fn shared_implementation_censuses_survive_order_and_cold_reopen() {
        use kin_model::EntityStore;
        const FILE: &str = "src/roots.js";
        const TEXT: &str = "HydrationRoot.prototype.render = Root.prototype.render = function(children) {\n  helper(children);\n};\nHydrationRoot.prototype.unmount = Root.prototype.unmount = function() {\n  cleanup();\n  flush();\n};\nfunction normal() {\n  helper();\n}\nfunction helper() {}\nfunction cleanup() {}\nfunction flush() {}\n";
        let root = tempfile::tempdir().unwrap();
        let blobs = kin_blobs::BlobStore::new(root.path().join("blobs")).unwrap();
        let body = blobs.write(TEXT.as_bytes()).unwrap();
        let indexed = kin_index::IndexPipeline::new()
            .index_file_content_with_tests(&FilePathId::new(FILE), TEXT.as_bytes(), body)
            .unwrap()
            .indexed_file;
        let graph = kin_db::InMemoryGraph::new();
        graph.upsert_entities_batch(&indexed.entities).unwrap();
        let uri = kin_lsp::protocol::path_to_uri(&root.path().join(FILE));
        let context_record = shared_call_context();
        let context = context_record.id();
        graph
            .apply_resolution_record_deltas(&[kin_model::ResolutionRecordDelta::Added {
                new: context_record,
            }])
            .unwrap();
        let names = ExternalNames::new();
        let references = Vec::new();
        let build = |entities: &[Entity]| {
            let named = |name: &str| entities.iter().find(|e| e.name == name).unwrap();
            let at = TEXT.rfind("helper();").unwrap();
            let line = TEXT[..at].bytes().filter(|b| *b == b'\n').count() as u32;
            let span = SourceSpan {
                file: FilePathId::new(FILE),
                start_byte: at,
                end_byte: at + 6,
                start_line: line,
                end_line: line,
                start_col: 2,
                end_col: 8,
            };
            let shared_at = TEXT.find("helper(children)").unwrap();
            let shared_span = SourceSpan {
                file: FilePathId::new(FILE),
                start_byte: shared_at,
                end_byte: shared_at + 6,
                start_line: 1,
                end_line: 1,
                start_col: 2,
                end_col: 8,
            };
            let answers = [
                SiteAnswer {
                    source: named("normal").id,
                    site: span,
                    target: SiteTarget::Entity(named("helper").id),
                    rule: DEFINITION_RULE,
                },
                SiteAnswer {
                    source: named("Root.render").id,
                    site: shared_span.clone(),
                    target: SiteTarget::Entity(named("helper").id),
                    rule: DEFINITION_RULE,
                },
            ];
            let arbitrary = call_sites::proven_call(
                named("Root.render").id,
                named("helper").id,
                vec![call_sites::site_evidence(DEFINITION_RULE, shared_span)],
            );
            let refs = entities
                .iter()
                .filter_map(|entity| crate::daemon::lsp_entity_ref(entity, FILE))
                .collect();
            let index = kin_lsp::EntityIndex::new(refs, root.path());
            let entities = entities.iter().collect::<Vec<_>>();
            let pass = FilePass {
                file: FILE,
                text: TEXT,
                uri: &uri,
                index: &index,
                entities: &entities,
                answers: &answers,
                names: &names,
                unproven: &[],
                recorded: &[],
                static_proofs: &[],
                outside_calls: &[],
                produced_references: &references,
                ending: PassEnding::Complete,
                not_in_build: false,
                context,
                body: Hash256::from_bytes(body.0),
            };
            let ledgers = build_ledgers(&pass).unwrap();
            let rewrite = rewrite_proofs(&ledgers, &pass, std::slice::from_ref(&arbitrary), true);
            assert_eq!(
                rewrite.retired,
                [arbitrary],
                "a prior arbitrary owner's site proof must leave on a completed pass"
            );
            assert_eq!(
                rewrite.completed_sites, 1,
                "the uniquely owned normal call still gets its backing edge"
            );
            let (backed, refused) = backed_ledgers(&ledgers, &pass, &[], &rewrite);
            assert!(refused.is_empty(), "{refused:?}");
            assert_eq!(backed.len(), ledgers.ledgers.len());
            ledgers
        };
        let first = build(&indexed.entities);
        let mut reversed = indexed.entities.clone();
        reversed.reverse();
        assert_eq!(
            first.ledgers,
            build(&reversed).ledgers,
            "graph iteration order cannot pick a caller"
        );
        for (name, census) in [
            ("Root.render", 1),
            ("HydrationRoot.render", 1),
            ("Root.unmount", 2),
            ("HydrationRoot.unmount", 2),
            ("normal", 1),
        ] {
            let entity = indexed
                .entities
                .iter()
                .find(|e| e.name == name)
                .unwrap_or_else(|| {
                    panic!(
                        "{name} not parsed: {:?}",
                        indexed.entities.iter().map(|e| &e.name).collect::<Vec<_>>()
                    )
                });
            let ledger = &first.ledgers[&entity.id];
            assert_eq!(ledger.census, census, "{name} must not be falsely empty");
            if name == "normal" {
                assert!(
                    matches!(ledger.sites[0].state, CallSiteState::ProvenTarget { .. }),
                    "an ordinary uniquely owned call still resolves"
                );
            } else {
                assert!(
                    ledger.sites.iter().all(|site| matches!(
                        site.state,
                        CallSiteState::Unresolved {
                            reason: UnresolvedReason::CalleeNotPlaceable
                        }
                    )),
                    "ownership gaps require a valid per-owner proof before they resolve"
                );
            }
            let record = kin_model::ResolutionRecord::CallSites(ledger.clone());
            record.validate().unwrap();
            graph
                .apply_resolution_record_deltas(&[kin_model::ResolutionRecordDelta::Added {
                    new: record,
                }])
                .unwrap();
        }
        let normal = indexed
            .entities
            .iter()
            .find(|e| e.name == "normal")
            .unwrap();
        let helper = indexed
            .entities
            .iter()
            .find(|e| e.name == "helper")
            .unwrap();
        let at = TEXT.rfind("helper();").unwrap();
        let line = TEXT[..at].bytes().filter(|b| *b == b'\n').count() as u32;
        let span = SourceSpan {
            file: FilePathId::new(FILE),
            start_byte: at,
            end_byte: at + 6,
            start_line: line,
            end_line: line,
            start_col: 2,
            end_col: 8,
        };
        let mut evidence = call_sites::site_evidence(DEFINITION_RULE, span);
        evidence.token = Some(context.context_token());
        graph
            .upsert_relation(&call_sites::proven_call(
                normal.id,
                helper.id,
                vec![evidence],
            ))
            .unwrap();
        let snapshot = root.path().join("snapshot.bin");
        kin_db::SnapshotManager::save_graph(&snapshot, &graph).unwrap();
        drop(graph);
        let reopened = kin_db::SnapshotManager::open_without_text_index(&snapshot).unwrap();
        let graph = reopened.graph();
        let mut entities = graph.list_all_entities().unwrap();
        entities.reverse();
        let rebuilt = build(&entities);
        assert_eq!(
            first.ledgers, rebuilt.ledgers,
            "cold reopen preserves both admitted owners and their occurrences"
        );
        for record in graph.list_resolution_records() {
            if let Some(ledger) = record.as_call_sites() {
                assert_eq!(ledger, &rebuilt.ledgers[&ledger.caller]);
            }
        }
    }

    #[test]
    #[ignore = "requires prepared repository authority and its matching source"]
    fn prepared_shared_implementation_censuses_are_explicit_after_reopen() {
        use kin_model::EntityStore;
        let store = std::env::var_os("KIN_SHARED_CALL_STORE").expect("store fixture");
        let repository =
            std::env::var("KIN_SHARED_CALL_REPOSITORY_ID").expect("repository fixture identity");
        let workspace = kin_model::WorkspaceId(
            std::env::var("KIN_SHARED_CALL_WORKSPACE_ID")
                .expect("workspace fixture identity")
                .parse()
                .unwrap(),
        );
        let source = std::env::var_os("KIN_SHARED_CALL_SOURCE").expect("admitted source fixture");
        let backend = kin_db::LocalFileBackend::new(std::path::PathBuf::from(store));
        let frozen = kin_db::LocalRepositoryAuthorityFreeze::open_existing_read_only_within(
            kin_model::RepositoryId::new(repository).unwrap(),
            &backend,
            std::time::Duration::from_secs(10),
        )
        .unwrap();
        let selected = frozen
            .authority()
            .workspace_graph_snapshot(&workspace)
            .unwrap()
            .expect("durable selected workspace");
        let held = kin_db::InMemoryGraph::from_snapshot_without_text_index(selected).unwrap();
        let file = "packages/react-dom/src/client/ReactDOMRoot.js";
        let text = std::fs::read_to_string(source).unwrap();
        let body = kin_blobs::digest(text.as_bytes());
        let entities = held
            .list_all_entities()
            .unwrap()
            .into_iter()
            .filter(|e| e.file_origin.as_ref().is_some_and(|p| p.0 == file))
            .collect::<Vec<_>>();
        assert!(!entities.is_empty());
        for entity in &entities {
            assert_eq!(
                entity
                    .metadata
                    .extra
                    .get("blob_hash")
                    .and_then(|v| v.as_str()),
                Some(body.to_string().as_str()),
                "source must match the locked graph fixture"
            );
        }
        let root = tempfile::tempdir().unwrap();
        let uri = kin_lsp::protocol::path_to_uri(&root.path().join(file));
        drop(frozen);
        let refs: Vec<_> = entities
            .iter()
            .filter_map(|e| crate::daemon::lsp_entity_ref(e, file))
            .collect();
        let reversed_index =
            kin_lsp::EntityIndex::new(refs.iter().rev().cloned().collect(), root.path());
        let index = kin_lsp::EntityIndex::new(refs, root.path());
        let owners = entities.iter().collect::<Vec<_>>();
        let names = ExternalNames::new();
        let references = Vec::new();
        let mut pass = FilePass {
            file,
            text: &text,
            uri: &uri,
            index: &index,
            entities: &owners,
            answers: &[],
            names: &names,
            unproven: &[],
            recorded: &[],
            static_proofs: &[],
            outside_calls: &[],
            produced_references: &references,
            ending: PassEnding::Complete,
            not_in_build: false,
            context: shared_call_context().id(),
            body: Hash256::from_bytes(body.0),
        };
        let ledgers = build_ledgers(&pass).unwrap();
        pass.index = &reversed_index;
        assert_eq!(ledgers.ledgers, build_ledgers(&pass).unwrap().ledgers);
        let graph = kin_db::InMemoryGraph::new();
        graph.upsert_entities_batch(&entities).unwrap();
        graph
            .apply_resolution_record_deltas(&[kin_model::ResolutionRecordDelta::Added {
                new: shared_call_context(),
            }])
            .unwrap();
        for (name, census) in [
            ("ReactDOMRoot.render", 5),
            ("ReactDOMHydrationRoot.render", 5),
            ("ReactDOMRoot.unmount", 6),
            ("ReactDOMHydrationRoot.unmount", 6),
        ] {
            let entity = entities.iter().find(|e| e.name == name).unwrap();
            let ledger = &ledgers.ledgers[&entity.id];
            assert_eq!(
                ledger.census, census,
                "{name} keeps every actual shared occurrence"
            );
            assert!(ledger.sites.iter().all(|site| matches!(
                site.state,
                CallSiteState::Unresolved {
                    reason: UnresolvedReason::CalleeNotPlaceable
                }
            )));
            graph
                .apply_resolution_record_deltas(&[kin_model::ResolutionRecordDelta::Added {
                    new: kin_model::ResolutionRecord::CallSites(ledger.clone()),
                }])
                .unwrap();
        }
        let path = root.path().join("snapshot.bin");
        kin_db::SnapshotManager::save_graph(&path, &graph).unwrap();
        drop(graph);
        let reopened = kin_db::SnapshotManager::open_without_text_index(&path).unwrap();
        let mut reopened_entities = reopened.graph().list_all_entities().unwrap();
        reopened_entities.reverse();
        let reopened_owners = reopened_entities.iter().collect::<Vec<_>>();
        let reopened_index = kin_lsp::EntityIndex::new(
            reopened_entities
                .iter()
                .filter_map(|e| crate::daemon::lsp_entity_ref(e, file))
                .collect(),
            root.path(),
        );
        pass.entities = &reopened_owners;
        pass.index = &reopened_index;
        assert_eq!(ledgers.ledgers, build_ledgers(&pass).unwrap().ledgers);
        for record in reopened.graph().list_resolution_records() {
            if let Some(ledger) = record.as_call_sites() {
                assert_eq!(ledger, &ledgers.ledgers[&ledger.caller]);
            }
        }
    }
}
