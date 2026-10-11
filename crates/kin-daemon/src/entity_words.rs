// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Naming pending work the way a reader knows it: by entity.
//!
//! A refusal or a report about uncommitted work has to say what that work is,
//! and Kin holds it as entities, so that is how it is named. A changed entity
//! is named by its kind and name (function `sign`). A member Kin does not
//! parse is the unparsed entity it is, named by its kind and its own name
//! (Markdown `notes.md`). Parsed source whose change moved no entity is said
//! to be exactly that. Where any of it is projected is structured detail for
//! the projection and the editor, never a word in the sentence.

use std::collections::{BTreeMap, BTreeSet};

use kin_model::{
    Entity, EntityDelta, EntityKind, RelationDelta, RelationOrigin, RepoPath, ResolvedTree,
    TreeDelta, WorkspaceSemanticOverlay,
};

/// How many names one clause lists before it counts the rest, so a crowded
/// refusal cannot bury its own headline.
const NAMED: usize = 5;

/// An entity kind as a sentence spells it: `function`, `type alias`.
///
/// The serialized word every Kin surface publishes, with its underscores read
/// as spaces. The one kind whose word would describe a projection rather than
/// an entity is spoken as the module it is.
pub(crate) fn kind_word(kind: EntityKind) -> String {
    match kind {
        EntityKind::File => "module".to_string(),
        other => kin_mcp::module_code::kind_word(other).replace('_', " "),
    }
}

/// One entity as a sentence names it: its kind and its name.
pub(crate) fn entity_words(entity: &Entity) -> String {
    format!("{} `{}`", kind_word(entity.kind), entity.name)
}

/// The own name of the member projected at `path`: its last segment, the name
/// an unparsed entity is addressed by.
pub(crate) fn unit_name(path: &RepoPath) -> String {
    let place = String::from_utf8_lossy(path.as_bytes());
    place.rsplit('/').next().unwrap_or(&place).to_string()
}

/// The member projected at `path`, named as an entity rather than a place.
///
/// Source Kin parses is named as source, because its entities are what a
/// reader acts on and this names the unit that holds them. Anything else is
/// an unparsed entity, named by the kind its name states (`Markdown`,
/// `licence`, `PNG image`) and by its own name.
pub(crate) fn unit_words(path: &RepoPath) -> String {
    let name = unit_name(path);
    if kin_mcp::artifact_entity::is_code(path) {
        return format!("source `{name}`");
    }
    let place = String::from_utf8_lossy(path.as_bytes());
    format!("{} `{name}`", unparsed_kind(&place, &name))
}

/// What an unparsed entity is, from its name alone.
///
/// The registry is asked as text first and then as bytes, with no content,
/// so only what a name states can answer. A name that states nothing is
/// still an unparsed entity, and is called one.
fn unparsed_kind(place: &str, name: &str) -> String {
    let as_text = kin_index::kind::describe(place, name, true, &[]).kind;
    if as_text != "text" {
        return as_text;
    }
    let as_bytes = kin_index::kind::describe(place, name, false, &[]).kind;
    if as_bytes != "binary" {
        return as_bytes;
    }
    "unparsed entity".to_string()
}

/// `names`, at most [`NAMED`] of them, with the rest counted.
pub(crate) fn listed(names: &[String]) -> String {
    let shown = names
        .iter()
        .take(NAMED)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    match names.len().checked_sub(NAMED) {
        Some(remaining) if remaining > 0 => format!("{shown} and {remaining} more"),
        _ => shown,
    }
}

const fn plural(count: usize, one: &'static str, many: &'static str) -> &'static str {
    if count == 1 {
        one
    } else {
        many
    }
}

/// `count` unsaved changes in the working copy, in words.
pub(crate) fn unsaved_changes(count: usize) -> String {
    format!(
        "{count} unsaved {} in the working copy",
        plural(count, "change", "changes")
    )
}

/// The entities a workspace overlay changes, keyed by the unit each is
/// declared in.
///
/// A delta is filed under the unit on both of its sides, so a unit asked for
/// by where it was and a unit asked for by where it went both find an entity
/// that moved between them. An entity with no unit is kept apart: it is still
/// pending work, and a summary of the whole workspace names it.
struct ChangedEntities {
    by_unit: BTreeMap<String, Vec<(String, String)>>,
    unplaced: Vec<(String, String)>,
}

impl ChangedEntities {
    fn of(overlay: &WorkspaceSemanticOverlay) -> Self {
        let mut by_unit: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
        let mut unplaced = Vec::new();
        for delta in overlay.entity_deltas() {
            let (sides, named) = match delta {
                EntityDelta::Added { new } => (vec![new], new),
                EntityDelta::Modified { old, new } => (vec![old, new], new),
                EntityDelta::Removed { old } => (vec![old], old),
            };
            let entry = (named.id.to_string(), entity_words(named));
            let units = sides
                .into_iter()
                .filter_map(declared_in)
                .collect::<BTreeSet<_>>();
            if units.is_empty() {
                unplaced.push(entry);
                continue;
            }
            for unit in units {
                by_unit.entry(unit).or_default().push(entry.clone());
            }
        }
        Self { by_unit, unplaced }
    }

    fn in_unit(&self, path: &RepoPath) -> &[(String, String)] {
        path.as_utf8()
            .and_then(|place| self.by_unit.get(place))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
}

/// The unit an entity is declared in, as the graph records it.
fn declared_in(entity: &Entity) -> Option<String> {
    entity
        .file_origin
        .as_ref()
        .or_else(|| entity.span.as_ref().map(|span| &span.file))
        .map(|unit| unit.0.clone())
}

/// Pending work, named.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct PendingWork {
    /// Changed entities, unparsed ones among them, each named once.
    entities: Vec<String>,
    /// Parsed source whose change moved no entity.
    quiet: Vec<String>,
    /// Relations an author changed. Language-server enrichment is Kin's own
    /// derived state and is not counted.
    relations: usize,
}

impl PendingWork {
    /// The pending work at `units`, read from `overlay`: the entities each one
    /// changed, or the unit itself where no changed entity names it.
    pub(crate) fn at<'a>(
        units: impl IntoIterator<Item = &'a RepoPath>,
        overlay: &WorkspaceSemanticOverlay,
    ) -> Self {
        let changed = ChangedEntities::of(overlay);
        let mut work = Self::default();
        let mut seen = BTreeSet::new();
        for unit in units {
            work.take_unit(unit, &changed, &mut seen);
        }
        work
    }

    /// Everything a workspace holds uncommitted: every entity its overlay
    /// changed, every unit of `units` no changed entity names, and the
    /// relations an author changed.
    pub(crate) fn of_workspace<'a>(
        units: impl IntoIterator<Item = &'a RepoPath>,
        overlay: &WorkspaceSemanticOverlay,
    ) -> Self {
        let changed = ChangedEntities::of(overlay);
        let mut work = Self::default();
        let mut seen = BTreeSet::new();
        for unit in units {
            work.take_unit(unit, &changed, &mut seen);
        }
        for entries in changed.by_unit.values() {
            work.take_entities(entries, &mut seen);
        }
        work.take_entities(&changed.unplaced, &mut seen);
        work.relations = overlay
            .relation_deltas()
            .iter()
            .filter(|delta| match delta {
                RelationDelta::Added { new } | RelationDelta::Modified { new, .. } => {
                    new.origin != RelationOrigin::Lsp
                }
                RelationDelta::Removed { .. } => true,
            })
            .count();
        work
    }

    fn take_unit(
        &mut self,
        unit: &RepoPath,
        changed: &ChangedEntities,
        seen: &mut BTreeSet<String>,
    ) {
        let entities = changed.in_unit(unit);
        if !entities.is_empty() {
            self.take_entities(entities, seen);
        } else if kin_mcp::artifact_entity::is_code(unit) {
            self.quiet.push(unit_words(unit));
        } else {
            self.entities.push(unit_words(unit));
        }
    }

    fn take_entities(&mut self, entries: &[(String, String)], seen: &mut BTreeSet<String>) {
        for (id, words) in entries {
            if seen.insert(id.clone()) {
                self.entities.push(words.clone());
            }
        }
    }

    /// How many things this names or counts, for a sentence choosing between
    /// "it" and "them".
    pub(crate) fn count(&self) -> usize {
        self.entities.len() + self.quiet.len() + self.relations
    }

    /// The named entities alone, listed, or the source no entity names:
    /// `function `sign`, class `Signer``.
    pub(crate) fn names(&self) -> String {
        if !self.entities.is_empty() {
            return listed(&self.entities);
        }
        if !self.quiet.is_empty() {
            return format!("{}, where no entity changed", listed(&self.quiet));
        }
        unsaved_changes(self.relations.max(1))
    }

    /// The whole of it as a noun phrase: `3 changed entities (function
    /// `sign`, ...)`, and the changes no entity shows as a count.
    pub(crate) fn words(&self) -> String {
        self.clauses(true)
    }

    /// The same phrase with counts alone, for a headline whose detail names
    /// each one: `3 changed entities`.
    pub(crate) fn counted(&self) -> String {
        self.clauses(false)
    }

    fn clauses(&self, named: bool) -> String {
        let named_as = |names: &[String]| {
            if named {
                format!(" ({})", listed(names))
            } else {
                String::new()
            }
        };
        let mut clauses = Vec::new();
        if !self.entities.is_empty() {
            clauses.push(format!(
                "{} changed {}{}",
                self.entities.len(),
                plural(self.entities.len(), "entity", "entities"),
                named_as(&self.entities)
            ));
        }
        if self.relations > 0 {
            clauses.push(format!(
                "{} changed {}",
                self.relations,
                plural(self.relations, "relation", "relations")
            ));
        }
        if !self.quiet.is_empty() {
            clauses.push(format!(
                "{} that {} no entity{}",
                unsaved_changes(self.quiet.len()),
                plural(self.quiet.len(), "changes", "change"),
                named_as(&self.quiet)
            ));
        }
        match clauses.len() {
            0 => "unsaved changes in the working copy".to_string(),
            1 => clauses.remove(0),
            _ => {
                let last = clauses.pop().unwrap_or_default();
                format!("{} and {last}", clauses.join(", "))
            }
        }
    }
}

/// The units pending between a workspace's base tree and its own tree, each
/// at the place it holds in the workspace, or held at its base when the
/// pending work removed it.
pub(crate) fn pending_units(
    base: &ResolvedTree,
    tree: &ResolvedTree,
) -> kin_core::Result<Vec<RepoPath>> {
    Ok(kin_core::exact_tree_correction(base, tree)?
        .into_iter()
        .map(|delta| match delta {
            TreeDelta::Added { new, .. } | TreeDelta::Updated { new, .. } => new.path,
            TreeDelta::Removed { old, .. } => old.path,
        })
        .collect())
}

/// Working-copy content Kin has not taken in, named by entity: the clause a
/// transition refuses with before `remedy`.
///
/// The working copy differs from what the graph holds for these members, so
/// no entity has been derived from what is there now. Each is named as the
/// member it is, never by where it is projected.
pub(crate) fn drift_refusal<'a>(
    drifted: impl IntoIterator<Item = &'a RepoPath>,
    remedy: &str,
) -> String {
    let units = drifted.into_iter().map(unit_words).collect::<Vec<_>>();
    format!(
        "Kin has not taken in {} ({}); {remedy}",
        unsaved_changes(units.len()),
        listed(&units)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use kin_model::{
        EntityId, EntityMetadata, EntityRole, FilePathId, FingerprintAlgorithm, Hash256,
        LanguageId, SemanticFingerprint, Visibility,
    };

    fn path(value: &str) -> RepoPath {
        RepoPath::from_utf8(value).unwrap()
    }

    fn entity(name: &str, kind: EntityKind, unit: &str) -> Entity {
        Entity {
            id: EntityId::new(),
            kind,
            name: name.to_string(),
            language: LanguageId::Python,
            fingerprint: SemanticFingerprint {
                algorithm: FingerprintAlgorithm::V1TreeSitter,
                ast_hash: Hash256::from_bytes([1; 32]),
                signature_hash: Hash256::from_bytes([2; 32]),
                behavior_hash: Hash256::from_bytes([3; 32]),
                equivalence_hash: Hash256::from_bytes([0; 32]),
                stability_score: 1.0,
            },
            file_origin: Some(FilePathId::new(unit)),
            span: None,
            signature: String::new(),
            visibility: Visibility::Public,
            role: EntityRole::Source,
            doc_summary: None,
            metadata: EntityMetadata::default(),
            lineage_parent: None,
            created_in: None,
            superseded_by: None,
        }
    }

    fn overlay(deltas: Vec<EntityDelta>) -> WorkspaceSemanticOverlay {
        WorkspaceSemanticOverlay::new(deltas, Vec::new()).unwrap()
    }

    fn assert_speaks_in_entities(sentence: &str, places: &[&str]) {
        for place in places {
            assert!(
                !sentence.contains(place),
                "the sentence names a place: {sentence}"
            );
        }
        assert!(!sentence.contains("path"), "{sentence}");
        assert!(!sentence.contains("file"), "{sentence}");
    }

    /// Pending source names the entities its change moved, never the unit's
    /// place.
    #[test]
    fn pending_source_is_named_by_the_entities_it_changed() {
        let unit = path("pkg/signing/core.py");
        let sign = entity("sign", EntityKind::Function, "pkg/signing/core.py");
        let signer = entity("Signer", EntityKind::Class, "pkg/signing/core.py");
        let work = PendingWork::at(
            [&unit],
            &overlay(vec![
                EntityDelta::Added { new: sign },
                EntityDelta::Added { new: signer },
            ]),
        );
        let words = work.words();
        assert!(words.starts_with("2 changed entities ("), "{words}");
        assert!(words.contains("function `sign`"), "{words}");
        assert!(words.contains("class `Signer`"), "{words}");
        assert_speaks_in_entities(&words, &["pkg/signing/core.py", "pkg/signing"]);
    }

    /// A member Kin does not parse is the unparsed entity it is, named by its
    /// kind and its own name.
    #[test]
    fn an_unparsed_member_is_named_by_its_kind_and_name() {
        let notes = path("docs/guide/notes.md");
        let compose = path("deploy/compose.yaml");
        let work = PendingWork::at([&notes, &compose], &overlay(Vec::new()));
        let words = work.words();
        assert!(words.contains("Markdown `notes.md`"), "{words}");
        assert!(
            words.contains("Compose configuration `compose.yaml`"),
            "{words}"
        );
        assert_speaks_in_entities(&words, &["docs/guide", "deploy/compose.yaml"]);
    }

    /// Source whose change moved no entity is said to be exactly that, and is
    /// counted as unsaved changes in the working copy.
    #[test]
    fn source_whose_change_moved_no_entity_is_counted_as_unsaved_changes() {
        let unit = path("pkg/quiet.py");
        let words = PendingWork::at([&unit], &overlay(Vec::new())).words();
        assert_eq!(
            words,
            "1 unsaved change in the working copy that changes no entity (source `quiet.py`)"
        );
    }

    /// A long list names five and counts the rest.
    #[test]
    fn a_crowded_summary_names_five_and_counts_the_rest() {
        let unit = path("pkg/many.py");
        let deltas = (0..8)
            .map(|index| EntityDelta::Added {
                new: entity(&format!("step{index}"), EntityKind::Function, "pkg/many.py"),
            })
            .collect();
        let words = PendingWork::at([&unit], &overlay(deltas)).words();
        assert!(words.starts_with("8 changed entities ("), "{words}");
        assert!(words.ends_with("and 3 more)"), "{words}");
    }

    /// Drift names the members whose working copy differs, never their place.
    #[test]
    fn drift_is_named_by_entity_and_counted_as_unsaved_changes() {
        let compose = path("selected/compose.yaml");
        let core = path("pkg/core.py");
        let sentence = drift_refusal([&compose, &core], "admit them first");
        assert!(
            sentence.starts_with("Kin has not taken in 2 unsaved changes in the working copy ("),
            "{sentence}"
        );
        assert!(sentence.contains("source `core.py`"), "{sentence}");
        assert!(sentence.ends_with("; admit them first"), "{sentence}");
        assert_speaks_in_entities(&sentence, &["selected/compose.yaml", "pkg/core.py"]);
    }

    /// The one entity kind whose serialized word names a projection is spoken
    /// as the module it is, and compound kinds read as words.
    #[test]
    fn kind_words_read_as_words() {
        assert_eq!(kind_word(EntityKind::Function), "function");
        assert_eq!(kind_word(EntityKind::TypeAlias), "type alias");
        assert_eq!(kind_word(EntityKind::File), "module");
    }
}
