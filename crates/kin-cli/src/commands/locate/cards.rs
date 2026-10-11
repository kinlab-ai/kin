// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Entity cards, and the description-query candidates they produce without
//! embeddings.
//!
//! A card is one lexical document per entity, built from the graph: the
//! entity's name split into words, its signature, its doc summary, where it
//! lives (owner and module path), and the names of its direct neighbours.
//! Every word is stemmed with the same stemmer the entity view reads
//! descriptive evidence with, so `parsing`, `parser` and `parse` meet.
//!
//! Cards are built once per graph truth epoch and held, which is the index
//! time for this arm: a query reads the held index and a handful of graph
//! relations, and never a file. Candidates are the card BM25 ranking fused by
//! reciprocal rank with a graph expansion of its leading hits (their owner,
//! their callees and their callers), and they enter the pipeline in the pool
//! the embedding seeds occupy on the fused arm, so every later stage runs
//! unchanged and the comparison between arms is the candidates alone.

use std::sync::{Arc, Mutex};

use super::{
    descriptive_query_concepts, evidence_stem, locate_env_f32, locate_env_usize,
    query_names_entity, split_identifier_parts, EntityDiscovery, FxHashMap, FxHashSet,
};
use anyhow::Result;
use kin_model::{Entity, EntityFilter, EntityId, EntityKind, EntityRole, EntityStore, GraphNodeId};
use kin_model::{Relation, RelationKind};

const CARD_WEIGHT_NAME: f32 = 4.0;
const CARD_WEIGHT_DOC: f32 = 2.5;
const CARD_WEIGHT_SIGNATURE: f32 = 2.0;
const CARD_WEIGHT_PLACE: f32 = 1.0;
const CARD_WEIGHT_NEIGHBOURS: f32 = 0.8;

/// Graph-expansion weights: how much a neighbour of a card hit is worth by
/// the edge that reached it. The owner says where a hit lives, a callee is
/// what it does, a caller is who asks for it.
const EXPAND_WEIGHT_OWNER: f32 = 1.0;
const EXPAND_WEIGHT_CALLEE: f32 = 0.8;
const EXPAND_WEIGHT_CALLER: f32 = 0.6;

/// What one card is made of, in the graph's own words.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct CardSource {
    pub name: String,
    pub kind: String,
    pub signature: String,
    pub doc: String,
    /// The owning entity's name, when a `Contains` edge names one.
    pub owner: Option<String>,
    /// The repo-relative file the entity is defined in.
    pub path: String,
    /// Direct neighbour names, owner and callees first, capped.
    pub neighbours: Vec<String>,
}

impl CardSource {
    /// The weighted, stemmed fields the card's BM25 document is built from.
    fn fields(&self) -> Vec<(String, f32)> {
        let mut fields = Vec::with_capacity(5);
        let mut push = |text: String, weight: f32| {
            if !text.is_empty() {
                fields.push((text, weight));
            }
        };
        push(stemmed_words(&self.name), CARD_WEIGHT_NAME);
        push(stemmed_words(&self.doc), CARD_WEIGHT_DOC);
        push(stemmed_words(&self.signature), CARD_WEIGHT_SIGNATURE);
        let place = match &self.owner {
            Some(owner) => format!("{owner} {}", path_words(&self.path)),
            None => path_words(&self.path),
        };
        push(stemmed_words(&place), CARD_WEIGHT_PLACE);
        push(
            stemmed_words(&self.neighbours.join(" ")),
            CARD_WEIGHT_NEIGHBOURS,
        );
        fields
    }

    /// The line that names the card's entity, which is what a scorer reads
    /// above the entity's code: the graph's own words rather than stems, in the
    /// order a reader weighs them. Of the documentation it carries the first
    /// line, cut to `doc_chars`. The neighbours stay on the card: the code
    /// under the line shows what the entity calls.
    pub(crate) fn naming_line(&self, doc_chars: usize) -> String {
        let mut text = format!("{} {}", self.kind, self.name);
        if !self.signature.is_empty() && self.signature != self.name {
            text.push_str(" | ");
            text.push_str(&self.signature);
        }
        let headline: String = self
            .doc
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("")
            .chars()
            .take(doc_chars)
            .collect();
        if !headline.is_empty() {
            text.push_str(" | ");
            text.push_str(&headline);
        }
        text.push_str(" | in ");
        if let Some(owner) = &self.owner {
            text.push_str(owner);
            text.push_str(" at ");
        }
        text.push_str(&self.path);
        text
    }
}

/// Every stem in `text`, identifier parts split first, sorted and joined by a
/// space so the card tokenizer sees each once.
fn stemmed_words(text: &str) -> String {
    let mut stems: Vec<String> = FxHashSet::from_iter(
        text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .filter(|raw| !raw.is_empty())
            .flat_map(split_identifier_parts)
            .filter(|part| part.len() >= 2 && !part.bytes().all(|byte| byte.is_ascii_digit()))
            .map(|part| evidence_stem(&part)),
    )
    .into_iter()
    .collect();
    stems.sort_unstable();
    stems.join(" ")
}

/// A path's directory and file-stem words, without the extension.
pub(super) fn path_words(path: &str) -> String {
    let without_extension = match path.rsplit_once('.') {
        Some((stem, extension)) if !extension.contains('/') => stem,
        _ => path,
    };
    without_extension
        .split(['/', '\\', '.', '-'])
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// The neighbour relations a card and an expansion read, and which side of
/// `id` each sits on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Neighbour {
    Owner,
    Callee,
    Caller,
    Child,
    Related,
}

fn classify(relation: &Relation, id: &EntityId) -> Option<(Neighbour, EntityId)> {
    let (GraphNodeId::Entity(src), GraphNodeId::Entity(dst)) = (&relation.src, &relation.dst)
    else {
        return None;
    };
    let outgoing = src == id;
    let other = if outgoing { *dst } else { *src };
    if other == *id {
        return None;
    }
    let role = match (relation.kind, outgoing) {
        (RelationKind::Contains, false) => Neighbour::Owner,
        (RelationKind::Contains, true) => Neighbour::Child,
        (RelationKind::Calls, true) => Neighbour::Callee,
        (RelationKind::Calls, false) => Neighbour::Caller,
        (
            RelationKind::Extends
            | RelationKind::Implements
            | RelationKind::Overrides
            | RelationKind::Instantiates
            | RelationKind::UsesType,
            _,
        ) => Neighbour::Related,
        _ => return None,
    };
    Some((role, other))
}

/// The card for one entity, given its relations and a name lookup.
fn card_source(
    entity: &Entity,
    relations: &[Relation],
    names: &FxHashMap<EntityId, String>,
    neighbour_cap: usize,
) -> CardSource {
    let mut neighbours: Vec<(Neighbour, &str)> = relations
        .iter()
        .filter_map(|relation| classify(relation, &entity.id))
        .filter_map(|(role, other)| names.get(&other).map(|name| (role, name.as_str())))
        .collect();
    neighbours.sort_unstable();
    neighbours.dedup_by(|a, b| a.1 == b.1);
    let owner = neighbours
        .iter()
        .find(|(role, _)| *role == Neighbour::Owner)
        .map(|(_, name)| (*name).to_string());
    CardSource {
        name: entity.name.clone(),
        kind: format!("{:?}", entity.kind).to_ascii_lowercase(),
        signature: entity.signature.clone(),
        doc: entity.doc_summary.clone().unwrap_or_default(),
        owner,
        path: entity
            .file_origin
            .as_ref()
            .map(|origin| origin.0.clone())
            .unwrap_or_default(),
        neighbours: neighbours
            .into_iter()
            .take(neighbour_cap)
            .map(|(_, name)| name.to_string())
            .collect(),
    }
}

/// The card of one entity, built on demand from the graph. What a scorer reads
/// on an arm that holds no card index.
pub(crate) fn card_source_for(graph: &kin_db::InMemoryGraph, id: &EntityId) -> Option<CardSource> {
    let entity = graph.get_entity(id).ok()??;
    let relations = graph.get_all_relations_for_entity(id).unwrap_or_default();
    let mut names: FxHashMap<EntityId, String> = FxHashMap::default();
    for relation in &relations {
        if let Some((_, other)) = classify(relation, id) {
            if let Ok(Some(neighbour)) = graph.get_entity(&other) {
                names.insert(other, neighbour.name);
            }
        }
    }
    Some(card_source(
        &entity,
        &relations,
        &names,
        locate_env_usize("KIN_LOCATE_CARD_NEIGHBOURS", 24),
    ))
}

/// The held card index for one graph truth epoch.
pub(crate) struct EntityCardIndex {
    index: kin_search::TextIndex<EntityId>,
    sources: FxHashMap<EntityId, CardSource>,
    stamp: (u64, u64),
    /// How long building this index took, for the cold-versus-warm split a
    /// latency measurement reports.
    pub build_ms: u128,
}

impl EntityCardIndex {
    /// Build every card from the graph. Reads entities and relations only.
    pub(crate) fn build(graph: &kin_db::InMemoryGraph) -> Result<Self> {
        let started = std::time::Instant::now();
        let stamp = (graph.instance_id(), graph.truth_epoch());
        let neighbour_cap = locate_env_usize("KIN_LOCATE_CARD_NEIGHBOURS", 24);
        let mut entities = graph.query_entities(&EntityFilter::default())?;
        entities.sort_unstable_by_key(|entity| entity.id);
        let names: FxHashMap<EntityId, String> = entities
            .iter()
            .map(|entity| (entity.id, entity.name.clone()))
            .collect();
        let mut sources: FxHashMap<EntityId, CardSource> = FxHashMap::default();
        for entity in &entities {
            if entity.file_origin.is_none() || entity.name.is_empty() {
                continue;
            }
            let relations = graph.get_all_relations_for_entity(&entity.id)?;
            sources.insert(
                entity.id,
                card_source(entity, &relations, &names, neighbour_cap),
            );
        }
        let mut documents: Vec<(EntityId, Vec<(String, f32)>)> = sources
            .iter()
            .map(|(id, source)| (*id, source.fields()))
            .collect();
        documents.sort_unstable_by_key(|(id, _)| *id);
        let index = kin_search::TextIndex::new();
        index.rebuild_all_owned(documents)?;
        index.commit()?;
        let build_ms = started.elapsed().as_millis();
        tracing::info!(
            cards = sources.len(),
            build_ms,
            "built the entity card index for the cards retrieval arm"
        );
        Ok(Self {
            index,
            sources,
            stamp,
            build_ms,
        })
    }

    /// Cards held.
    pub(crate) fn len(&self) -> usize {
        self.sources.len()
    }

    /// The card of one entity, when it has one.
    pub(crate) fn source(&self, id: &EntityId) -> Option<&CardSource> {
        self.sources.get(id)
    }

    /// BM25 over the cards for a stemmed query.
    fn search(&self, query: &str, limit: usize) -> Result<Vec<(EntityId, f32)>> {
        Ok(self.index.fuzzy_search(query, limit)?)
    }
}

/// The held indexes, most recent first. More than one because a hosted daemon
/// serves several graphs, and one slot would rebuild on every alternation.
static CARD_INDEXES: Mutex<Vec<Arc<EntityCardIndex>>> = Mutex::new(Vec::new());
/// Serializes builds, so two queries arriving at a new epoch build it once.
static CARD_BUILD: Mutex<()> = Mutex::new(());
const CARD_INDEX_SLOTS: usize = 4;

fn held_card_index(stamp: (u64, u64)) -> Option<Arc<EntityCardIndex>> {
    CARD_INDEXES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .iter()
        .find(|held| held.stamp == stamp)
        .cloned()
}

/// The card index for `graph` at its current truth epoch, and whether this
/// call built it. Built at most once per epoch and held; a graph that moved
/// since gets a fresh build.
pub(crate) fn card_index_for(
    graph: &kin_db::InMemoryGraph,
) -> Result<(Arc<EntityCardIndex>, bool)> {
    let stamp = (graph.instance_id(), graph.truth_epoch());
    if let Some(held) = held_card_index(stamp) {
        return Ok((held, false));
    }
    let _building = CARD_BUILD
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(held) = held_card_index(stamp) {
        return Ok((held, false));
    }
    let built = Arc::new(EntityCardIndex::build(graph)?);
    let mut held = CARD_INDEXES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // An older epoch of the same graph is superseded, not kept beside it.
    held.retain(|index| index.stamp.0 != stamp.0);
    held.insert(0, Arc::clone(&built));
    held.truncate(CARD_INDEX_SLOTS);
    Ok((built, true))
}

/// The card index held for `graph` at its current epoch, without building one.
#[cfg(test)]
pub(super) fn card_index_is_held_for_test(
    graph: &kin_db::InMemoryGraph,
) -> Option<Arc<EntityCardIndex>> {
    held_card_index((graph.instance_id(), graph.truth_epoch()))
}

/// Build and hold the card index for `graph` now, so the first description
/// query does not pay for it. A daemon that serves a cards arm calls this once
/// its graph has loaded.
pub fn warm_entity_cards(graph: &kin_db::InMemoryGraph) -> Result<usize> {
    let cards = card_index_for(graph)?.0.len();
    // A description is ranked against the evidence index, which reads these
    // cards, so the first description pays for neither.
    super::evidence::evidence_index_for(graph)?;
    Ok(cards)
}

/// The stemmed BM25 query a description reads as: its concept words, the
/// same ones the entity view weighs descriptive evidence by.
pub(crate) fn card_query(text: &str) -> Option<String> {
    let concepts = descriptive_query_concepts(text);
    (!concepts.is_empty()).then(|| concepts.join(" "))
}

/// What one run of the cards arm produced, for the note an answer carries.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct CardRun {
    pub cards: usize,
    /// Set when this query built the card index, with how long it took.
    pub built_ms: Option<u128>,
    pub card_hits: usize,
    pub graph_hits: usize,
    pub pooled: usize,
}

/// The cards arm's candidate pool, as discovery seeds in the slot the fused
/// arm fills with embedding seeds.
pub(crate) fn card_entity_seeds(
    text: &str,
    graph: &kin_db::InMemoryGraph,
    test_query: bool,
) -> Result<(
    std::collections::HashMap<EntityId, EntityDiscovery>,
    CardRun,
)> {
    let (index, built) = card_index_for(graph)?;
    let mut run = CardRun {
        cards: index.len(),
        built_ms: built.then_some(index.build_ms),
        ..CardRun::default()
    };
    let mut seeds = std::collections::HashMap::new();
    let Some(query) = card_query(text) else {
        return Ok((seeds, run));
    };
    let card_limit = locate_env_usize("KIN_LOCATE_CARD_LIMIT", 100);
    let card_ranked = index.search(&query, card_limit)?;
    run.card_hits = card_ranked.len();

    let expand_from = locate_env_usize("KIN_LOCATE_CARD_EXPAND_FROM", 10);
    let per_hit = locate_env_usize("KIN_LOCATE_CARD_EXPAND_PER_HIT", 8);
    let graph_ranked = expand_through_graph(graph, &index, &card_ranked, expand_from, per_hit)?;
    run.graph_hits = graph_ranked.len();

    let rrf_k = locate_env_f32("KIN_LOCATE_RRF_K", 60.0).max(1.0);
    let pool_limit = locate_env_usize("KIN_LOCATE_CARD_POOL", 100);
    let pool = fuse_by_rank(&[&card_ranked, &graph_ranked], rrf_k, pool_limit);
    run.pooled = pool.len();
    let card_ids: FxHashSet<EntityId> = card_ranked.iter().map(|(id, _)| *id).collect();
    let graph_ids: FxHashSet<EntityId> = graph_ranked.iter().map(|(id, _)| *id).collect();
    let top = pool
        .first()
        .map(|(_, score)| *score)
        .unwrap_or(0.0)
        .max(f32::EPSILON);
    for (id, fused) in pool {
        let Some(entity) = graph.get_entity(&id)? else {
            continue;
        };
        let kind_mult = match entity.kind {
            EntityKind::Function
            | EntityKind::Method
            | EntityKind::Class
            | EntityKind::TraitDef
            | EntityKind::Interface
            | EntityKind::Module => 2.0,
            EntityKind::EnumDef => 1.5,
            _ => 1.0,
        };
        let role_mult = if !test_query && entity.role == EntityRole::Test {
            0.1
        } else {
            1.0
        };
        let entry: &mut EntityDiscovery = seeds.entry(id).or_default();
        entry.score = (fused / top) * kind_mult * 20.0 * role_mult;
        entry.exact_name |= query_names_entity(text, &entity.name);
        if card_ids.contains(&id) {
            entry.signals.push("cards");
        }
        if graph_ids.contains(&id) {
            entry.signals.push("graph");
        }
    }
    Ok((seeds, run))
}

/// The neighbours of the leading card hits, ranked: each neighbour scores the
/// reciprocal rank of every hit that reached it, weighted by the edge.
pub(super) fn expand_through_graph(
    graph: &kin_db::InMemoryGraph,
    index: &EntityCardIndex,
    card_ranked: &[(EntityId, f32)],
    expand_from: usize,
    per_hit: usize,
) -> Result<Vec<(EntityId, f32)>> {
    let mut scores: FxHashMap<EntityId, f32> = FxHashMap::default();
    for (rank, (hit, _)) in card_ranked.iter().take(expand_from).enumerate() {
        let relations = graph.get_all_relations_for_entity(hit)?;
        let mut reached: Vec<(Neighbour, EntityId)> = relations
            .iter()
            .filter_map(|relation| classify(relation, hit))
            .filter(|(role, _)| {
                matches!(
                    role,
                    Neighbour::Owner | Neighbour::Callee | Neighbour::Caller
                )
            })
            // A neighbour with no card has no file to resolve to, so it could
            // not reach an answer row either.
            .filter(|(_, other)| index.source(other).is_some())
            .collect();
        reached.sort_unstable();
        reached.dedup_by(|a, b| a.1 == b.1);
        for (role, other) in reached.into_iter().take(per_hit) {
            let weight = match role {
                Neighbour::Owner => EXPAND_WEIGHT_OWNER,
                Neighbour::Callee => EXPAND_WEIGHT_CALLEE,
                _ => EXPAND_WEIGHT_CALLER,
            };
            *scores.entry(other).or_default() += weight / (rank as f32 + 1.0);
        }
    }
    let mut ranked: Vec<(EntityId, f32)> = scores.into_iter().collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    Ok(ranked)
}

/// Reciprocal rank fusion over ranked id lists: each list contributes
/// `1 / (k + rank + 1)`. Ties break on id so the pool is deterministic.
fn fuse_by_rank(lists: &[&[(EntityId, f32)]], k: f32, limit: usize) -> Vec<(EntityId, f32)> {
    let mut fused: FxHashMap<EntityId, f32> = FxHashMap::default();
    for list in lists {
        for (rank, (id, _)) in list.iter().enumerate() {
            *fused.entry(*id).or_default() += 1.0 / (k + rank as f32 + 1.0);
        }
    }
    let mut ranked: Vec<(EntityId, f32)> = fused.into_iter().collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked.truncate(limit);
    ranked
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use kin_model::{
        EntityMetadata, FilePathId, FingerprintAlgorithm, Hash256, LanguageId, RelationId,
        RelationOrigin, SemanticFingerprint, SourceSpan, Visibility,
    };

    pub(crate) fn card_entity(name: &str, path: &str, doc: Option<&str>) -> Entity {
        Entity {
            id: EntityId::new(),
            kind: EntityKind::Function,
            name: name.to_string(),
            language: LanguageId::Python,
            fingerprint: SemanticFingerprint {
                algorithm: FingerprintAlgorithm::V1TreeSitter,
                ast_hash: Hash256::from_bytes([0; 32]),
                signature_hash: Hash256::from_bytes([1; 32]),
                behavior_hash: Hash256::from_bytes([2; 32]),
                equivalence_hash: Hash256::from_bytes([0; 32]),
                stability_score: 1.0,
            },
            file_origin: Some(FilePathId::new(path)),
            span: Some(SourceSpan {
                file: FilePathId::new(path),
                start_byte: 0,
                end_byte: 0,
                start_line: 1,
                start_col: 1,
                end_line: 3,
                end_col: 1,
            }),
            signature: format!("def {name}()"),
            visibility: Visibility::Public,
            role: EntityRole::Source,
            doc_summary: doc.map(str::to_string),
            metadata: EntityMetadata::default(),
            lineage_parent: None,
            created_in: None,
            superseded_by: None,
        }
    }

    pub(crate) fn edge(src: EntityId, dst: EntityId, kind: RelationKind) -> Relation {
        Relation {
            id: RelationId::new(),
            kind,
            src: GraphNodeId::Entity(src),
            dst: GraphNodeId::Entity(dst),
            confidence: 1.0,
            origin: RelationOrigin::Parsed,
            created_in: None,
            import_source: None,
            evidence: Vec::new(),
        }
    }

    #[test]
    fn a_card_carries_split_stemmed_words_from_every_part_of_the_entity() {
        let graph = kin_db::InMemoryGraph::new();
        let owner = card_entity("RateLimiter", "net/throttle.py", None);
        let entity = card_entity(
            "refillTokenBucket",
            "net/throttle.py",
            Some("Adds tokens back as time passes."),
        );
        let callee = card_entity("monotonic_clock", "net/clock.py", None);
        for e in [&owner, &entity, &callee] {
            graph.upsert_entity(e).unwrap();
        }
        graph
            .upsert_relation(&edge(owner.id, entity.id, RelationKind::Contains))
            .unwrap();
        graph
            .upsert_relation(&edge(entity.id, callee.id, RelationKind::Calls))
            .unwrap();

        let index = EntityCardIndex::build(&graph).unwrap();
        let card = index.source(&entity.id).unwrap();
        assert_eq!(card.owner.as_deref(), Some("RateLimiter"));
        assert_eq!(card.neighbours, vec!["RateLimiter", "monotonic_clock"]);
        let fields: Vec<String> = card.fields().into_iter().map(|(text, _)| text).collect();
        let all = fields.join(" ");
        for stem in [
            "refill", "token", "bucket", "add", "tim", "pass", "throttl", "clock",
        ] {
            assert!(
                all.split(' ').any(|word| word == stem),
                "the card must carry the stem {stem:?}: {fields:?}"
            );
        }
        assert_eq!(
            card.naming_line(160),
            "function refillTokenBucket | def refillTokenBucket() | Adds tokens back as time \
             passes. | in RateLimiter at net/throttle.py"
        );
        let long = CardSource {
            doc: format!("\n  {}\n\n  :param rate: tokens a second", "x".repeat(200)),
            ..card.clone()
        };
        assert!(
            long.naming_line(160)
                .contains(&format!(" | {} | in ", "x".repeat(160))),
            "the documentation's first line, cut: {}",
            long.naming_line(160)
        );
    }

    /// A description that shares no identifier with the answer still reaches it
    /// through its doc words, and the answer's owner and callee come in through
    /// the graph even though no word of the query names them.
    #[test]
    fn description_words_reach_an_entity_and_the_graph_brings_its_neighbours() {
        let graph = kin_db::InMemoryGraph::new();
        let owner = card_entity("RateLimiter", "net/throttle.py", None);
        let target = card_entity(
            "refill",
            "net/throttle.py",
            Some("Top up the bucket with tokens as time passes."),
        );
        let helper = card_entity("monotonic_clock", "net/clock.py", None);
        let unrelated = card_entity("render_page", "web/view.py", Some("Draw the HTML page."));
        for e in [&owner, &target, &helper, &unrelated] {
            graph.upsert_entity(e).unwrap();
        }
        graph
            .upsert_relation(&edge(owner.id, target.id, RelationKind::Contains))
            .unwrap();
        graph
            .upsert_relation(&edge(target.id, helper.id, RelationKind::Calls))
            .unwrap();

        let (seeds, run) =
            card_entity_seeds("how are tokens topped up over time", &graph, false).unwrap();
        assert!(run.card_hits >= 1, "{run:?}");
        let target_seed = seeds
            .get(&target.id)
            .expect("the described entity is a candidate");
        assert!(target_seed.signals.contains(&"cards"));
        assert!(
            seeds
                .get(&owner.id)
                .is_some_and(|seed| seed.signals.contains(&"graph")),
            "the owner arrives through graph expansion"
        );
        assert!(
            seeds
                .get(&helper.id)
                .is_some_and(|seed| seed.signals.contains(&"graph")),
            "the callee arrives through graph expansion"
        );
        assert!(
            !seeds.contains_key(&unrelated.id),
            "an entity that shares no concept and no edge is not a candidate"
        );
        assert!(
            seeds[&target.id].score > seeds[&helper.id].score,
            "the card hit outranks what the graph brought in beside it"
        );
    }

    #[test]
    fn the_card_index_is_held_per_epoch_and_rebuilt_when_truth_moves() {
        let graph = kin_db::InMemoryGraph::new();
        graph
            .upsert_entity(&card_entity("parse_config", "cfg/load.py", None))
            .unwrap();
        let (first, built) = card_index_for(&graph).unwrap();
        assert!(built);
        let (again, rebuilt) = card_index_for(&graph).unwrap();
        assert!(!rebuilt, "an unchanged graph reuses the held index");
        assert!(Arc::ptr_eq(&first, &again));

        graph
            .upsert_entity(&card_entity("write_config", "cfg/save.py", None))
            .unwrap();
        let (moved, built_again) = card_index_for(&graph).unwrap();
        assert!(built_again, "a truth write invalidates the held cards");
        assert_eq!(moved.len(), 2);

        let other = kin_db::InMemoryGraph::new();
        other
            .upsert_entity(&card_entity("parse_config", "cfg/load.py", None))
            .unwrap();
        let (_, other_built) = card_index_for(&other).unwrap();
        assert!(other_built, "another graph never reads this graph's cards");
    }

    #[test]
    fn a_query_with_no_concept_words_produces_no_candidates() {
        assert_eq!(card_query("what is it"), None);
        assert_eq!(card_query("how does this"), None);
        assert!(card_query("where are tokens refilled").is_some());
    }

    #[test]
    fn rank_fusion_rewards_agreement_and_breaks_ties_on_id() {
        let a = EntityId::new();
        let b = EntityId::new();
        let c = EntityId::new();
        let cards = vec![(a, 9.0), (b, 5.0)];
        let graph = vec![(b, 1.0), (c, 0.5)];
        let fused = fuse_by_rank(&[&cards, &graph], 60.0, 10);
        assert_eq!(
            fused[0].0, b,
            "the id both lists hold ranks first: {fused:?}"
        );
        assert_eq!(fused.len(), 3);
        let limited = fuse_by_rank(&[&cards, &graph], 60.0, 1);
        assert_eq!(limited.len(), 1);
    }
}
