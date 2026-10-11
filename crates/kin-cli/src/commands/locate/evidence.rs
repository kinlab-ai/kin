// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The evidence index: what a description is ranked against when no vector
//! is read.
//!
//! One document per entity, built from what the graph holds about it: its own
//! name, the name it hangs off, its signature, its doc summary, its body, where
//! it lives, and the names of its direct neighbours. A description is scored
//! against every field at once with field-weighted BM25: per query word the
//! fields' term frequencies are normalised by that field's own length,
//! weighted, and summed, and the sum saturates once. A word in the name and
//! in the body is then one piece of evidence weighed twice, not two rows of
//! unrelated score, and a long body does not dilute a short docstring.
//!
//! This is the ranking a description gets on an arm that reads no embedding.
//! The card index beside it ([`super::cards`]) holds a set of stems per field
//! and no body, which measured 33 percent of described functions in the top
//! five on Flask against 51 percent for this scoring over the same words.
//!
//! Document frequencies and average lengths come from the population a query
//! ranks, not from the whole store: library code alone unless tests are asked
//! for, and within that the functions and methods. Test code is two thirds of
//! the entities in a typical store, so weights taken over all of it price
//! library words by how tests use them; classes, constants and modules are
//! ranked beside functions but do not say which words are rare.
//!
//! Everything here is a function of the graph and the query: entities are read
//! in id order, a query's words are scored in query order into one accumulator
//! per entity, and ties break on the entity id.

use std::sync::{Arc, Mutex};

use super::cards;
use super::{evidence_stem, qualified_name_tail, FxHashMap, FxHashSet};
use anyhow::Result;
use kin_model::{Entity, EntityFilter, EntityId, EntityKind, EntityRole, EntityStore};

/// The fields of an evidence document, in posting order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum Field {
    /// The entity's own name: the last segment of a qualified name.
    Name = 0,
    /// What the name hangs off: `Flask` for `Flask.make_response`.
    Owner = 1,
    Signature = 2,
    Doc = 3,
    /// The body the graph holds for the entity. A container contributes its
    /// whole own code, with independently modeled descendants excluded.
    Body = 4,
    /// The owning entity's name and the words of the path the entity is filed
    /// under. Where an entity lives is one of its fields, nothing more.
    Place = 5,
    /// Names of the owner, callees, callers, members and related types.
    Neighbours = 6,
}

const FIELD_COUNT: usize = 7;

/// Field weights. Name, doc and signature are the card's own weights; the body
/// enters at one. Measured on 256 Flask commit descriptions that never name the
/// function they changed: these weights put it in the top five for 54 percent,
/// against 40 percent with the body weight at zero.
const FIELD_WEIGHTS: [f32; FIELD_COUNT] = [4.0, 1.0, 2.0, 2.5, 1.0, 1.0, 0.8];

const BM25_K1: f32 = 1.2;
const BM25_B: f32 = 0.75;

/// How much a row of each kind is worth beside a function with the same
/// evidence. A description of behaviour is most often about a function; a
/// class, an alias or a constant can be the answer and stays on the page, a
/// little lower. Measured on the same queries: without it, classes and
/// constants shown beside functions cost three points at the top five.
fn kind_prior(kind: EntityKind) -> f32 {
    match kind {
        EntityKind::Function
        | EntityKind::Method
        | EntityKind::Test
        | EntityKind::Macro
        | EntityKind::ApiEndpoint => 1.0,
        EntityKind::Class
        | EntityKind::Interface
        | EntityKind::TraitDef
        | EntityKind::EnumDef
        | EntityKind::TypeAlias
        | EntityKind::Schema
        | EntityKind::EventContract => 0.85,
        EntityKind::Constant
        | EntityKind::StaticVar
        | EntityKind::Field
        | EntityKind::EnumVariant
        | EntityKind::Module
        | EntityKind::Package
        | EntityKind::File
        | EntityKind::DocumentNode => 0.6,
    }
}

/// Whether an entity's body is its members' bodies rather than its own.
fn is_container(kind: EntityKind) -> bool {
    matches!(
        kind,
        EntityKind::Class
            | EntityKind::Interface
            | EntityKind::TraitDef
            | EntityKind::EnumDef
            | EntityKind::Module
            | EntityKind::Package
            | EntityKind::File
    )
}

/// Words that carry no evidence in a description, sorted for binary search.
///
/// Closed-class English only. The words a caller uses to say what kind of code
/// they want (`call`, `file`, `type`, `test`, `class`) are NOT here: in a
/// sentence they are vocabulary ("the test client", "file upload", "call the
/// teardown handlers"), and dropping them cost three points at the top five.
const DESCRIPTION_STOPWORDS: &[&str] = &[
    "a",
    "about",
    "above",
    "after",
    "again",
    "against",
    "all",
    "also",
    "an",
    "and",
    "any",
    "are",
    "as",
    "at",
    "be",
    "because",
    "been",
    "before",
    "being",
    "below",
    "between",
    "both",
    "but",
    "by",
    "can",
    "could",
    "did",
    "do",
    "does",
    "doing",
    "done",
    "down",
    "during",
    "each",
    "eg",
    "else",
    "etc",
    "every",
    "for",
    "from",
    "had",
    "has",
    "have",
    "having",
    "he",
    "here",
    "how",
    "i",
    "ie",
    "if",
    "in",
    "into",
    "is",
    "it",
    "its",
    "just",
    "may",
    "me",
    "might",
    "more",
    "most",
    "must",
    "my",
    "no",
    "nor",
    "not",
    "now",
    "of",
    "off",
    "on",
    "once",
    "only",
    "onto",
    "or",
    "other",
    "our",
    "out",
    "over",
    "own",
    "per",
    "perform",
    "previously",
    "same",
    "shall",
    "she",
    "should",
    "so",
    "some",
    "such",
    "than",
    "that",
    "the",
    "their",
    "them",
    "then",
    "there",
    "these",
    "they",
    "this",
    "those",
    "through",
    "to",
    "too",
    "under",
    "until",
    "up",
    "us",
    "very",
    "via",
    "vs",
    "was",
    "we",
    "were",
    "what",
    "when",
    "where",
    "which",
    "while",
    "who",
    "whom",
    "whose",
    "why",
    "will",
    "with",
    "without",
    "would",
    "you",
    "your",
];

fn is_description_stopword(word: &str) -> bool {
    DESCRIPTION_STOPWORDS.binary_search(&word).is_ok()
}

/// The words of one identifier, lowercased: `url_for` gives `url`, `for`;
/// `makeResponse` gives `make`, `response`; `HTTPException` gives `http`,
/// `exception`; `sha256` gives `sha`, `256`.
///
/// An acronym ends where a capitalised word begins, so `JSONProvider` is
/// `json` and `provider` rather than one word nothing else in the repository
/// spells.
fn identifier_words(raw: &str) -> Vec<String> {
    let mut words = Vec::new();
    for segment in raw.split(['_', '-']) {
        let chars: Vec<char> = segment.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            let start = i;
            if chars[i].is_ascii_digit() {
                while i < chars.len() && chars[i].is_ascii_digit() {
                    i += 1;
                }
            } else if chars[i].is_uppercase() {
                while i < chars.len() && chars[i].is_uppercase() {
                    i += 1;
                }
                if i < chars.len() && chars[i].is_lowercase() {
                    // The last capital opens the word that follows.
                    if i - start > 1 {
                        words.push(
                            chars[start..i - 1]
                                .iter()
                                .collect::<String>()
                                .to_lowercase(),
                        );
                    }
                    let word_start = i - 1;
                    while i < chars.len() && chars[i].is_lowercase() {
                        i += 1;
                    }
                    words.push(
                        chars[word_start..i]
                            .iter()
                            .collect::<String>()
                            .to_lowercase(),
                    );
                    continue;
                }
            } else if chars[i].is_lowercase() {
                while i < chars.len() && chars[i].is_lowercase() {
                    i += 1;
                }
            } else {
                // A letter with no case, or anything else the splitter let
                // through: one character, its own word.
                i += 1;
            }
            words.push(chars[start..i].iter().collect::<String>().to_lowercase());
        }
    }
    words
}

/// One word's stem. [`evidence_stem`], with a final `y` after a consonant read
/// as `i`, so a plural meets its singular whichever way English spells it:
/// `cookies` and `cookie` both reach `cooki`, `queries` and `query` both reach
/// `queri`, `applies`, `applied` and `apply` all reach `appli`. Without it
/// `cookies` stems to `cooky` and never meets `cookie`.
fn stem(word: &str) -> String {
    let mut stem = evidence_stem(word);
    let bytes = stem.as_bytes();
    if bytes.len() > 3
        && bytes[bytes.len() - 1] == b'y'
        && !b"aeiou".contains(&bytes[bytes.len() - 2])
    {
        stem.pop();
        stem.push('i');
    }
    stem
}

/// Every evidence word of a text, repeats kept: identifiers are split into
/// their words and each is stemmed, and an identifier of more than one word is
/// also kept whole, lowercased, so `url_for` in a description meets `url_for`
/// in a body exactly as well as it meets `url` and `for` apart.
///
/// `query` drops the words a description carries no evidence in
/// ([`DESCRIPTION_STOPWORDS`]). A document keeps them: nothing asks for them,
/// and a field's length should be its length.
fn evidence_words(text: &str, query: bool) -> Vec<String> {
    let mut words = Vec::new();
    for raw in text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')) {
        if raw.is_empty() {
            continue;
        }
        let parts = identifier_words(raw);
        if parts.len() > 1 {
            let whole = raw.trim_matches('_').to_ascii_lowercase();
            if whole.len() > 1 {
                words.push(whole);
            }
        }
        for part in parts {
            if part.len() < 2 || part.bytes().all(|byte| byte.is_ascii_digit()) {
                continue;
            }
            if query && is_description_stopword(&part) {
                continue;
            }
            words.push(stem(&part));
        }
    }
    words
}

/// The words a description is scored by, each once, in the order it uses them.
pub(crate) fn description_terms(text: &str) -> Vec<String> {
    let mut seen: FxHashSet<String> = FxHashSet::default();
    evidence_words(text, true)
        .into_iter()
        .filter(|word| seen.insert(word.clone()))
        .collect()
}

/// One field of one document a term occurs in.
#[derive(Clone, Copy, Debug)]
struct Hit {
    doc: u32,
    field: u8,
    tf: u16,
}

/// Where a term occurs, and in how many documents of each population
/// ([`population_index`]).
#[derive(Debug, Default)]
struct Postings {
    /// Ordered by document, then field.
    hits: Vec<Hit>,
    documents: [u32; POPULATIONS],
}

/// The populations statistics are kept for: every entity or library code
/// alone, and every kind or callables alone.
const POPULATIONS: usize = 4;

fn population_index(library_only: bool, callables_only: bool) -> usize {
    usize::from(library_only) + 2 * usize::from(callables_only)
}

/// Whether an entity is of the kinds a description is most often about, the
/// ones [`kind_prior`] prices whole. Their words set the weights: a store's
/// classes, constants and modules are ranked too, but a word is common or rare
/// by how functions and methods use it. Taking the statistics over every kind
/// cost two points of top-one on the measured queries.
fn is_callable(kind: EntityKind) -> bool {
    kind_prior(kind) >= 1.0
}

#[derive(Debug)]
struct Document {
    id: EntityId,
    kind: EntityKind,
    test: bool,
    lengths: [u32; FIELD_COUNT],
}

/// Counts a ranking's weights are taken over.
#[derive(Debug, Default, Clone, Copy)]
struct Population {
    documents: usize,
    average_length: [f32; FIELD_COUNT],
}

/// One ranked entity.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct EvidenceHit {
    pub id: EntityId,
    pub score: f32,
}

/// The held evidence index for one graph truth epoch.
pub(crate) struct EvidenceIndex {
    documents: Vec<Document>,
    postings: FxHashMap<String, Postings>,
    populations: [Population; POPULATIONS],
    stamp: (u64, u64),
    /// How long building this index took.
    pub build_ms: u128,
}

/// What one document is built from.
pub(crate) struct EvidenceSource<'a> {
    pub entity: &'a Entity,
    pub card: &'a cards::CardSource,
}

impl EvidenceSource<'_> {
    fn fields(&self) -> [String; FIELD_COUNT] {
        let name = self.entity.name.as_str();
        let tail = qualified_name_tail(name);
        let owner = name[..name.len() - tail.len()].trim_end_matches(['.', ':']);
        // A container is ranked on its own code, the statements in its span
        // outside its members, and never on its members' bodies, which their
        // own rows carry. Without it a class was ranked on its name and
        // documentation alone, and a description of a dict in its class-level
        // code could not reach it.
        let key = if is_container(self.entity.kind) {
            kin_index::pipeline::OWN_CODE_KEY
        } else {
            kin_parser::extract::EMBEDDING_BODY_PREVIEW_KEY
        };
        let body = self
            .entity
            .metadata
            .extra
            .get(key)
            .and_then(|value| value.as_str())
            .unwrap_or("");
        let place = match &self.card.owner {
            Some(owner) => format!("{owner} {}", cards::path_words(&self.card.path)),
            None => cards::path_words(&self.card.path),
        };
        let mut fields: [String; FIELD_COUNT] = Default::default();
        fields[Field::Name as usize] = tail.to_string();
        fields[Field::Owner as usize] = owner.to_string();
        fields[Field::Signature as usize] = self.card.signature.clone();
        fields[Field::Doc as usize] = self.card.doc.clone();
        fields[Field::Body as usize] = body.to_string();
        fields[Field::Place as usize] = place;
        fields[Field::Neighbours as usize] = self.card.neighbours.join(" ");
        fields
    }
}

impl EvidenceIndex {
    /// Build every document from the graph and the cards already read from it.
    pub(crate) fn build(graph: &kin_db::InMemoryGraph) -> Result<Self> {
        let started = std::time::Instant::now();
        let stamp = (graph.instance_id(), graph.truth_epoch());
        let (cards, _) = cards::card_index_for(graph)?;
        let mut entities = graph.query_entities(&EntityFilter::default())?;
        entities.sort_unstable_by_key(|entity| entity.id);
        let sources: Vec<EvidenceSource<'_>> = entities
            .iter()
            .filter_map(|entity| {
                cards
                    .source(&entity.id)
                    .map(|card| EvidenceSource { entity, card })
            })
            .collect();
        let mut index = Self::from_sources(&sources);
        index.stamp = stamp;
        index.build_ms = started.elapsed().as_millis();
        tracing::info!(
            documents = index.documents.len(),
            terms = index.postings.len(),
            build_ms = index.build_ms,
            "built the evidence index descriptions are ranked against"
        );
        Ok(index)
    }

    /// The index over `sources`, which the caller hands over in entity id order.
    pub(crate) fn from_sources(sources: &[EvidenceSource<'_>]) -> Self {
        let mut documents: Vec<Document> = Vec::with_capacity(sources.len());
        let mut postings: FxHashMap<String, Postings> = FxHashMap::default();
        let mut totals = [[0u64; FIELD_COUNT]; POPULATIONS];
        let mut counts = [0usize; POPULATIONS];
        for (doc, source) in sources.iter().enumerate() {
            let test = source.entity.role == EntityRole::Test;
            let mut lengths = [0u32; FIELD_COUNT];
            // Term -> its frequency in each field of this document.
            let mut terms: FxHashMap<String, [u16; FIELD_COUNT]> = FxHashMap::default();
            for (field, text) in source.fields().iter().enumerate() {
                for word in evidence_words(text, false) {
                    lengths[field] += 1;
                    let frequency = &mut terms.entry(word).or_default()[field];
                    *frequency = frequency.saturating_add(1);
                }
            }
            // The populations this document belongs to.
            let callable = is_callable(source.entity.kind);
            let member = |which: usize| {
                let library_only = which % 2 == 1;
                let callables_only = which >= 2;
                !(library_only && test) && (!callables_only || callable)
            };
            for which in (0..POPULATIONS).filter(|which| member(*which)) {
                for (field, length) in lengths.iter().enumerate() {
                    totals[which][field] += u64::from(*length);
                }
                counts[which] += 1;
            }
            // Sorted so the posting lists do not depend on a map's order.
            let mut terms: Vec<(String, [u16; FIELD_COUNT])> = terms.into_iter().collect();
            terms.sort_unstable_by(|a, b| a.0.cmp(&b.0));
            for (term, frequencies) in terms {
                let entry = postings.entry(term).or_default();
                for which in (0..POPULATIONS).filter(|which| member(*which)) {
                    entry.documents[which] += 1;
                }
                for (field, tf) in frequencies.iter().enumerate() {
                    if *tf > 0 {
                        entry.hits.push(Hit {
                            doc: doc as u32,
                            field: field as u8,
                            tf: *tf,
                        });
                    }
                }
            }
            documents.push(Document {
                id: source.entity.id,
                kind: source.entity.kind,
                test,
                lengths,
            });
        }
        let population = |which: usize| {
            let documents = counts[which];
            let mut average_length = [1.0f32; FIELD_COUNT];
            if documents > 0 {
                for (field, average) in average_length.iter_mut().enumerate() {
                    let mean = totals[which][field] as f32 / documents as f32;
                    // A field nothing fills has no length to normalise by.
                    *average = if mean > 0.0 { mean } else { 1.0 };
                }
            }
            Population {
                documents,
                average_length,
            }
        };
        Self {
            populations: [population(0), population(1), population(2), population(3)],
            documents,
            postings,
            stamp: (0, 0),
            build_ms: 0,
        }
    }

    /// The entities a description is about, best first.
    ///
    /// `include_tests` chooses both who is ranked and whose statistics weigh
    /// the words: library code alone, or everything. Within that, the
    /// statistics are the callables' ([`is_callable`]), or every kind's in a
    /// store that holds no callable at all.
    pub(crate) fn rank(
        &self,
        terms: &[String],
        include_tests: bool,
        limit: usize,
    ) -> Vec<EvidenceHit> {
        let callables = population_index(!include_tests, true);
        let which = if self.populations[callables].documents > 0 {
            callables
        } else {
            population_index(!include_tests, false)
        };
        let population = &self.populations[which];
        if population.documents == 0 || terms.is_empty() {
            return Vec::new();
        }
        let total = population.documents as f32;
        let mut scores = vec![0.0f32; self.documents.len()];
        for term in terms {
            let Some(postings) = self.postings.get(term) else {
                continue;
            };
            // Zero is a real count: a word no callable carries is as rare as a
            // word can be, and the class or constant that does carry it is
            // still scored.
            let in_documents = postings.documents[which] as f32;
            let idf = ((total - in_documents + 0.5) / (in_documents + 0.5) + 1.0).ln();
            let mut at = 0;
            while at < postings.hits.len() {
                let doc = postings.hits[at].doc as usize;
                let document = &self.documents[doc];
                let mut weighted = 0.0f32;
                while at < postings.hits.len() && postings.hits[at].doc as usize == doc {
                    let hit = postings.hits[at];
                    let field = hit.field as usize;
                    let relative =
                        document.lengths[field] as f32 / population.average_length[field];
                    weighted += FIELD_WEIGHTS[field] * f32::from(hit.tf)
                        / (1.0 - BM25_B + BM25_B * relative);
                    at += 1;
                }
                if document.test && !include_tests {
                    continue;
                }
                scores[doc] += idf * weighted / (BM25_K1 + weighted);
            }
        }
        let mut ranked: Vec<EvidenceHit> = scores
            .iter()
            .enumerate()
            .filter(|(_, score)| **score > 0.0)
            .map(|(doc, score)| EvidenceHit {
                id: self.documents[doc].id,
                score: score * kind_prior(self.documents[doc].kind),
            })
            .collect();
        ranked.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.id.cmp(&b.id)));
        ranked.truncate(limit);
        ranked
    }
}

/// The graph's own candidates for a description: the owner, the callees and
/// the callers of its leading hits that carry no word of it themselves.
///
/// They follow every row the words found and never precede one. A caller that
/// shares no word with a bug report can still be where the bug is, and a reader
/// or a scorer going down the page should meet it after the evidence, not
/// instead of it. Each is priced as a share of the weakest ranked row, in the
/// order the expansion ranks them ([`cards::expand_through_graph`]).
pub(crate) fn graph_tail(
    graph: &kin_db::InMemoryGraph,
    ranked: &[EvidenceHit],
    include_tests: bool,
    room: usize,
) -> Result<Vec<EvidenceHit>> {
    let Some(weakest) = ranked.last().map(|hit| hit.score) else {
        return Ok(Vec::new());
    };
    if room == 0 {
        return Ok(Vec::new());
    }
    let (cards, _) = cards::card_index_for(graph)?;
    let leading: Vec<(EntityId, f32)> = ranked.iter().map(|hit| (hit.id, hit.score)).collect();
    let expand_from = super::locate_env_usize("KIN_LOCATE_CARD_EXPAND_FROM", 10);
    let per_hit = super::locate_env_usize("KIN_LOCATE_CARD_EXPAND_PER_HIT", 8);
    let reached = cards::expand_through_graph(graph, &cards, &leading, expand_from, per_hit)?;
    let held: FxHashSet<EntityId> = ranked.iter().map(|hit| hit.id).collect();
    let strongest = reached
        .iter()
        .map(|(_, score)| *score)
        .fold(0.0f32, f32::max)
        .max(f32::EPSILON);
    let mut tail = Vec::new();
    for (id, score) in reached {
        if tail.len() >= room {
            break;
        }
        if held.contains(&id) {
            continue;
        }
        let Some(entity) = graph.get_entity(&id)? else {
            continue;
        };
        if entity.role == EntityRole::Test && !include_tests {
            continue;
        }
        tail.push(EvidenceHit {
            id,
            score: weakest * 0.5 * (score / strongest),
        });
    }
    Ok(tail)
}

/// The held indexes, most recent first, one per graph a daemon serves.
static EVIDENCE_INDEXES: Mutex<Vec<Arc<EvidenceIndex>>> = Mutex::new(Vec::new());
/// Serializes builds, so two queries arriving at a new epoch build it once.
static EVIDENCE_BUILD: Mutex<()> = Mutex::new(());
const EVIDENCE_INDEX_SLOTS: usize = 4;

fn held_evidence_index(stamp: (u64, u64)) -> Option<Arc<EvidenceIndex>> {
    EVIDENCE_INDEXES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .iter()
        .find(|held| held.stamp == stamp)
        .cloned()
}

/// The evidence index for `graph` at its current truth epoch. Built at most
/// once per epoch and held; a graph that moved since gets a fresh build.
pub(crate) fn evidence_index_for(graph: &kin_db::InMemoryGraph) -> Result<Arc<EvidenceIndex>> {
    let stamp = (graph.instance_id(), graph.truth_epoch());
    if let Some(held) = held_evidence_index(stamp) {
        return Ok(held);
    }
    let _building = EVIDENCE_BUILD
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(held) = held_evidence_index(stamp) {
        return Ok(held);
    }
    let built = Arc::new(EvidenceIndex::build(graph)?);
    let mut held = EVIDENCE_INDEXES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    held.retain(|index| index.stamp.0 != stamp.0);
    held.insert(0, Arc::clone(&built));
    held.truncate(EVIDENCE_INDEX_SLOTS);
    Ok(built)
}

#[cfg(test)]
mod tests {
    use super::super::cards::tests::{card_entity, edge};
    use super::*;
    use kin_model::RelationKind;

    fn with_body(mut entity: Entity, body: &str) -> Entity {
        entity.metadata.extra.insert(
            kin_parser::extract::EMBEDDING_BODY_PREVIEW_KEY.to_string(),
            serde_json::Value::String(body.to_string()),
        );
        entity
    }

    fn ranked_names(
        graph: &kin_db::InMemoryGraph,
        query: &str,
        include_tests: bool,
    ) -> Vec<String> {
        let index = EvidenceIndex::build(graph).unwrap();
        index
            .rank(&description_terms(query), include_tests, 20)
            .into_iter()
            .map(|hit| graph.get_entity(&hit.id).unwrap().unwrap().name)
            .collect()
    }

    /// The pilot's case: a model looked ten turns for where
    /// SESSION_REFRESH_EACH_REQUEST gets its default, a key in the
    /// class-level `default_config` dict of Flask's application class. A class
    /// was ranked on its name and documentation alone, so no description of
    /// that dict could reach it. It is ranked on its own code now, and its
    /// methods' bodies still are not its evidence.
    #[test]
    fn a_class_is_ranked_on_its_own_code_and_not_on_its_members() {
        let graph = kin_db::InMemoryGraph::new();
        let mut flask = card_entity("Flask", "src/flask/app.py", Some("The flask object."));
        flask.kind = EntityKind::Class;
        flask.metadata.extra.insert(
            kin_index::pipeline::OWN_CODE_KEY.to_string(),
            serde_json::Value::String(
                "class Flask(App): default_config = ImmutableDict({\"DEBUG\": None, \
                 \"SESSION_REFRESH_EACH_REQUEST\": True})"
                    .to_string(),
            ),
        );
        // A preview of the whole class names a method's work; it is not the
        // class's evidence.
        flask.metadata.extra.insert(
            kin_parser::extract::EMBEDDING_BODY_PREVIEW_KEY.to_string(),
            serde_json::Value::String("class Flask(App): def run(self): serve forever".to_string()),
        );
        let other = with_body(
            card_entity("make_config", "src/flask/config.py", None),
            "def make_config(): return Config(defaults)",
        );
        for entity in [&flask, &other] {
            graph.upsert_entity(entity).unwrap();
        }
        let ranked = ranked_names(&graph, "default config session refresh each request", false);
        assert_eq!(
            ranked.first().map(String::as_str),
            Some("Flask"),
            "{ranked:?}"
        );
        assert!(
            !ranked_names(&graph, "serve forever", false).contains(&"Flask".to_string()),
            "a member's body is not the class's evidence"
        );
    }

    #[test]
    fn a_description_finds_middle_evidence_in_a_long_container() {
        let graph = kin_db::InMemoryGraph::new();
        let padding = "padding café ".repeat(1_000);
        // An independently modeled attribute owns its value; use a bare
        // class-level call so this evidence remains the container's own code.
        let source = format!(
            "class Settings:\n    register(\
             {{\"head\": \"{padding}\", \"lunar_flux_alignment\": True, \
             \"tail\": \"{padding}\"}})\n\n\
             \x20\x20\x20\x20def execute(self):\n        return obscured_member_beacon()\n"
        );
        let indexed = kin_index::IndexPipeline::new()
            .index_file_content_with_tests(
                &kin_model::FilePathId::new("settings.py"),
                source.as_bytes(),
                kin_blobs::digest(source.as_bytes()),
            )
            .unwrap()
            .indexed_file;
        for entity in &indexed.entities {
            graph.upsert_entity(entity).unwrap();
        }
        assert_eq!(
            ranked_names(&graph, "lunar flux alignment", false)
                .first()
                .map(String::as_str),
            Some("Settings"),
            "the class's middle evidence must be reachable from the persisted graph"
        );
        assert!(
            !ranked_names(&graph, "obscured member beacon", false)
                .contains(&"Settings".to_string()),
            "a member's body is not its container's ranking evidence"
        );
    }

    #[test]
    fn an_identifier_is_split_at_every_word_boundary_and_kept_whole() {
        assert_eq!(identifier_words("url_for"), ["url", "for"]);
        assert_eq!(identifier_words("makeResponse"), ["make", "response"]);
        assert_eq!(identifier_words("HTTPException"), ["http", "exception"]);
        assert_eq!(identifier_words("JSONProvider"), ["json", "provider"]);
        assert_eq!(
            identifier_words("SECRET_KEY_FALLBACKS"),
            ["secret", "key", "fallbacks"]
        );
        assert_eq!(identifier_words("sha256"), ["sha", "256"]);
        assert_eq!(identifier_words("plain"), ["plain"]);
        assert_eq!(
            evidence_words("get_signing_serializer", false),
            ["get_signing_serializer", "get", "sign", "serializ"]
        );
        // One word has no whole form to add.
        assert_eq!(evidence_words("Serializer", false), ["serializ"]);
    }

    #[test]
    fn a_description_keeps_the_words_that_say_what_kind_of_code() {
        // `call`, `file`, `test`, `type` and `class` are vocabulary in a
        // sentence; only closed-class English is dropped.
        assert_eq!(
            description_terms("the test client should call the file upload handler of this class"),
            ["test", "client", "call", "fil", "upload", "handl", "class"]
        );
        // A word is scored once however often the description repeats it, and
        // a plural is its singular.
        assert_eq!(description_terms("cookie cookies Cookie"), ["cooki"]);
        assert_eq!(description_terms("query queries"), ["queri"]);
        assert_eq!(description_terms("apply applies applied"), ["appli"]);
        // A vowel before the `y`, or a short word, is left as it is.
        assert_eq!(description_terms("key keys delay"), ["key", "delay"]);
    }

    #[test]
    fn a_function_is_found_by_words_only_its_body_holds() {
        // The measured failure: a docless function whose name says nothing the
        // description says. "enable secret key rotation with the
        // SECRET_KEY_FALLBACKS config" is about `get_signing_serializer`.
        let graph = kin_db::InMemoryGraph::new();
        let answer = with_body(
            card_entity("get_signing_serializer", "src/sessions.py", None),
            "def get_signing_serializer(self, app): keys = [] if fallbacks := app.config[\"SECRET_KEY_FALLBACKS\"]: keys.extend(fallbacks) keys.append(app.secret_key) return URLSafeTimedSerializer(keys)",
        );
        let named_alike = with_body(
            card_entity(
                "DebugFilesKeyError",
                "src/debughelpers.py",
                Some("Raised for a missing key."),
            ),
            "class DebugFilesKeyError(KeyError, AssertionError): pass",
        );
        let unrelated = with_body(
            card_entity(
                "render_template",
                "src/templating.py",
                Some("Render a template by name."),
            ),
            "def render_template(name, **context): return _render(app, template, context)",
        );
        for entity in [&answer, &named_alike, &unrelated] {
            graph.upsert_entity(entity).unwrap();
        }
        let ranked = ranked_names(
            &graph,
            "enable secret key rotation with the SECRET_KEY_FALLBACKS config",
            false,
        );
        assert_eq!(
            ranked.first().map(String::as_str),
            Some("get_signing_serializer"),
            "{ranked:?}"
        );
    }

    #[test]
    fn a_word_in_two_fields_is_one_piece_of_evidence() {
        // `cookie` three times over in one entity must not outrank an entity
        // that carries both words of the description.
        let graph = kin_db::InMemoryGraph::new();
        let both = with_body(
            card_entity(
                "save_session",
                "src/sessions.py",
                Some("Write the session cookie."),
            ),
            "def save_session(self, app, session, response): response.set_cookie(name, val)",
        );
        let repeats = with_body(
            card_entity(
                "cookie_jar",
                "src/testing.py",
                Some("The cookie jar of cookies."),
            ),
            "def cookie_jar(self): return self._cookies or CookieJar(cookie_policy)",
        );
        for entity in [&both, &repeats] {
            graph.upsert_entity(entity).unwrap();
        }
        let ranked = ranked_names(&graph, "session cookie is not saved", false);
        assert_eq!(
            ranked.first().map(String::as_str),
            Some("save_session"),
            "{ranked:?}"
        );
    }

    #[test]
    fn test_code_is_ranked_only_when_asked_for_and_does_not_set_the_weights() {
        let graph = kin_db::InMemoryGraph::new();
        let library = with_body(
            card_entity(
                "teardown_request",
                "src/scaffold.py",
                Some("Register a teardown function."),
            ),
            "def teardown_request(self, f): self.teardown_request_funcs[None].append(f)",
        );
        let mut helper = with_body(
            card_entity("teardown_request", "tests/test_basic.py", None),
            "def teardown_request(exc): called.append(True) teardown teardown",
        );
        helper.role = EntityRole::Test;
        let mut test = with_body(
            card_entity("test_teardown_request_handler", "tests/test_basic.py", None),
            "def test_teardown_request_handler(app, client): teardown teardown request",
        );
        test.role = EntityRole::Test;
        for entity in [&library, &helper, &test] {
            graph.upsert_entity(entity).unwrap();
        }
        let index = EvidenceIndex::build(&graph).unwrap();
        let terms = description_terms("teardown request handlers are called in the wrong order");
        let without: Vec<EntityId> = index
            .rank(&terms, false, 20)
            .iter()
            .map(|hit| hit.id)
            .collect();
        assert_eq!(
            without,
            vec![library.id],
            "test code must be left out by default"
        );
        let with: FxHashSet<EntityId> = index
            .rank(&terms, true, 20)
            .iter()
            .map(|hit| hit.id)
            .collect();
        assert!(with.contains(&helper.id) && with.contains(&test.id) && with.contains(&library.id));
    }

    #[test]
    fn a_container_is_not_scored_by_its_members_bodies() {
        let graph = kin_db::InMemoryGraph::new();
        let mut class = with_body(
            card_entity("Flask", "src/app.py", Some("The application object.")),
            "class Flask(App): def make_response(self, rv): status = headers = None if isinstance(rv, tuple): ...",
        );
        class.kind = EntityKind::Class;
        let method = with_body(
            card_entity(
                "Flask.make_response",
                "src/app.py",
                Some("Convert the return value from a view."),
            ),
            "def make_response(self, rv): status = headers = None if isinstance(rv, tuple): ...",
        );
        for entity in [&class, &method] {
            graph.upsert_entity(entity).unwrap();
        }
        graph
            .upsert_relation(&edge(class.id, method.id, RelationKind::Contains))
            .unwrap();
        let ranked = ranked_names(
            &graph,
            "a tuple returned from a view loses its headers",
            false,
        );
        assert_eq!(
            ranked.first().map(String::as_str),
            Some("Flask.make_response"),
            "{ranked:?}"
        );
    }

    #[test]
    fn a_word_is_common_or_rare_by_how_functions_use_it() {
        // Thirty constants documented with `retry` would make the word nearly
        // worthless if they were counted, and `render_view` would lead on
        // `page` alone. Among the two functions each word is in one of them,
        // so the function that says `retry` three times leads.
        let graph = kin_db::InMemoryGraph::new();
        let retries = with_body(
            card_entity("send_with_backoff", "src/net.py", None),
            "def send_with_backoff(request): retry retry retry",
        );
        let renders = with_body(
            card_entity("render_view", "src/web.py", None),
            "def render_view(template, context, request, response, session, headers): return page",
        );
        graph.upsert_entity(&retries).unwrap();
        graph.upsert_entity(&renders).unwrap();
        for n in 0..30 {
            let mut constant = card_entity(
                &format!("LIMIT_{n}"),
                "src/limits.py",
                Some("How long to wait before a retry."),
            );
            constant.kind = EntityKind::Constant;
            graph.upsert_entity(&constant).unwrap();
        }
        let ranked = ranked_names(&graph, "retry the page", false);
        assert_eq!(
            ranked.first().map(String::as_str),
            Some("send_with_backoff"),
            "{ranked:?}"
        );
        assert!(
            ranked.iter().any(|name| name.starts_with("LIMIT_")),
            "a constant that carries the word is still ranked: {ranked:?}"
        );
    }

    #[test]
    fn a_store_with_no_function_is_still_ranked() {
        let graph = kin_db::InMemoryGraph::new();
        let mut settings =
            card_entity("Settings", "conf/settings.py", Some("Deployment settings."));
        settings.kind = EntityKind::Class;
        let mut limits = card_entity("Limits", "conf/limits.py", Some("Rate limits per tenant."));
        limits.kind = EntityKind::Class;
        graph.upsert_entity(&settings).unwrap();
        graph.upsert_entity(&limits).unwrap();
        let ranked = ranked_names(&graph, "where are the rate limits for a tenant", false);
        assert_eq!(
            ranked.first().map(String::as_str),
            Some("Limits"),
            "{ranked:?}"
        );
    }

    #[test]
    fn the_same_description_ranks_the_same_way_twice() {
        let graph = kin_db::InMemoryGraph::new();
        for n in 0..40 {
            let entity = with_body(
                card_entity(
                    &format!("handler_{n}"),
                    "src/handlers.py",
                    Some("Handle one request."),
                ),
                "def handler(request): return dispatch(request, session)",
            );
            graph.upsert_entity(&entity).unwrap();
        }
        let terms = description_terms("request handler dispatch session");
        let first = EvidenceIndex::build(&graph)
            .unwrap()
            .rank(&terms, false, 40);
        let second = EvidenceIndex::build(&graph)
            .unwrap()
            .rank(&terms, false, 40);
        assert_eq!(first.len(), 40);
        assert_eq!(
            first, second,
            "equal scores must break on the entity id, not on build order"
        );
        assert!(
            first.windows(2).all(|pair| pair[0].id < pair[1].id),
            "all forty tie, so id decides"
        );
    }
}
