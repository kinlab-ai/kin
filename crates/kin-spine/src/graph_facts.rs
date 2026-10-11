// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! What a repository's graph says about its entities beyond the metadata rows
//! the spine resolves against.
//!
//! Two facts decide what a cross-repo edge may claim, and an [`EntityEntry`]
//! carries neither:
//!
//! - **Which entities are inferred members.** A member derived from a
//!   generator, such as a key a loop assigns onto an object, is a candidate
//!   declaration. The local graph reads every edge that touches one as
//!   `name_only`, and the spine represents it the same way: an edge into or
//!   out of it carries [`DERIVED_MEMBER_EDGE_CONFIDENCE`], which classifies
//!   `name_only` wherever the edge is read. The member still enters the spine,
//!   and so does the rest of its repository.
//! - **Each entity's language.** Code can reference a declaration only across
//!   a boundary its runtime can cross ([`language_can_reference`]). The
//!   resolver binds an import only to a declaration its language could reach,
//!   and an anchor's cross-repo authority waits only on the repositories that
//!   could hold a reference into it.
//!
//! A repository registered without facts, which is what a durable publication
//! installs, is unknown on both counts, and every rule here treats unknown as
//! "could": no edge is gated and no stale repository is excused.
//!
//! [`EntityEntry`]: crate::EntityEntry

use std::collections::{HashMap, HashSet};

use kin_model::{Entity, EntityId, LanguageId};

/// The confidence of every cross-repo edge that touches an inferred member.
///
/// The local graph caps an inferred member's edges at the receiver-name
/// fan-out tier, 0.3, the weakest value on its resolution ladder, and reads
/// them `name_only`: a candidate, never a counted reference. A spine edge
/// carries its confidence and nothing else, so the same value carries the same
/// reading across the repository boundary.
pub const DERIVED_MEMBER_EDGE_CONFIDENCE: f32 = 0.3;

/// Language and derivation facts for one registered repository's entities.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepoGraphFacts {
    languages: HashMap<EntityId, LanguageId>,
    repo_languages: HashSet<LanguageId>,
    derived_members: HashSet<EntityId>,
}

impl RepoGraphFacts {
    /// The facts a captured graph holds about each of its entities.
    pub fn from_entities(entities: &[Entity]) -> Self {
        let mut facts = Self::default();
        for entity in entities {
            facts.languages.insert(entity.id, entity.language);
            facts.repo_languages.insert(entity.language);
            if kin_model::is_derived_member(entity) {
                facts.derived_members.insert(entity.id);
            }
        }
        facts
    }

    /// The language the graph records for `entity`, if it holds the entity.
    pub fn language(&self, entity: &EntityId) -> Option<LanguageId> {
        self.languages.get(entity).copied()
    }

    /// Whether `entity` is an inferred member: every cross-repo edge that
    /// touches it is a candidate.
    pub fn is_derived_member(&self, entity: &EntityId) -> bool {
        self.derived_members.contains(entity)
    }

    /// How many inferred members the graph holds.
    pub fn derived_member_count(&self) -> usize {
        self.derived_members.len()
    }

    /// Whether any code in this repository could hold a reference to a
    /// declaration written in `target`.
    pub fn could_reference(&self, target: LanguageId) -> bool {
        self.repo_languages
            .iter()
            .any(|&language| language_can_reference(language, target))
    }
}

/// The runtime a language's code executes on, which is the boundary a static
/// reference cannot cross without a foreign-function interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Runtime {
    /// JavaScript engines: TypeScript compiles to JavaScript and imports it.
    JavaScript,
    /// The JVM: Kotlin and Java call each other directly.
    Jvm,
    /// The .NET runtime.
    Clr,
    Python,
    Ruby,
    Php,
    /// A native, compiled artifact. Each of these exports declarations over the
    /// C ABI, and every code runtime above can bind that ABI through its own
    /// foreign-function interface.
    Native,
    /// Configuration, which references modules of its own kind and holds no
    /// code another runtime calls.
    Configuration,
}

fn runtime(language: LanguageId) -> Runtime {
    match language {
        LanguageId::TypeScript | LanguageId::JavaScript => Runtime::JavaScript,
        LanguageId::Java | LanguageId::Kotlin => Runtime::Jvm,
        LanguageId::CSharp => Runtime::Clr,
        LanguageId::Python => Runtime::Python,
        LanguageId::Ruby => Runtime::Ruby,
        LanguageId::Php => Runtime::Php,
        LanguageId::C | LanguageId::Cpp | LanguageId::Rust | LanguageId::Go | LanguageId::Swift => {
            Runtime::Native
        }
        LanguageId::Hcl => Runtime::Configuration,
    }
}

/// Whether code written in `from` can hold a static reference to a
/// declaration written in `into`.
///
/// Code references what its own runtime loads, and any code can bind a native
/// declaration through a foreign-function interface. Nothing else crosses: a
/// Python module cannot import a JavaScript function, and a C library does not
/// statically name a Python one. The rule reads runtimes, never a pair of
/// named languages, and it errs toward "could" wherever an interface exists.
pub fn language_can_reference(from: LanguageId, into: LanguageId) -> bool {
    let (from, into) = (runtime(from), runtime(into));
    from == into || (into == Runtime::Native && from != Runtime::Configuration)
}

/// The facts of every registered repository, captured under one lock so a
/// batch of resolutions reads one consistent set.
#[derive(Debug, Clone, Default)]
pub(crate) struct FactsSnapshot {
    pub(crate) repos: HashMap<String, std::sync::Arc<RepoGraphFacts>>,
}

impl FactsSnapshot {
    /// Whether code in `source_language` could reference `entity` in `repo`.
    /// Unknown on either side is "could".
    pub(crate) fn admits(
        &self,
        source_language: Option<LanguageId>,
        repo: &str,
        entity: &EntityId,
    ) -> bool {
        let Some(source_language) = source_language else {
            return true;
        };
        match self
            .repos
            .get(repo)
            .and_then(|facts| facts.language(entity))
        {
            Some(target_language) => language_can_reference(source_language, target_language),
            None => true,
        }
    }

    /// Whether `entity` in `repo` is an inferred member.
    pub(crate) fn is_derived_member(&self, repo: &str, entity: &EntityId) -> bool {
        self.repos
            .get(repo)
            .is_some_and(|facts| facts.is_derived_member(entity))
    }
}

/// Parse the language hint an [`UnresolvedImport`] carries, which is the
/// display name of the importing entity's language.
///
/// [`UnresolvedImport`]: crate::UnresolvedImport
pub(crate) fn language_from_hint(hint: &str) -> Option<LanguageId> {
    Some(match hint {
        "typescript" => LanguageId::TypeScript,
        "javascript" => LanguageId::JavaScript,
        "python" => LanguageId::Python,
        "go" => LanguageId::Go,
        "java" => LanguageId::Java,
        "rust" => LanguageId::Rust,
        "c" => LanguageId::C,
        "cpp" => LanguageId::Cpp,
        "csharp" => LanguageId::CSharp,
        "ruby" => LanguageId::Ruby,
        "php" => LanguageId::Php,
        "swift" => LanguageId::Swift,
        "kotlin" => LanguageId::Kotlin,
        "hcl" => LanguageId::Hcl,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [LanguageId; 14] = [
        LanguageId::TypeScript,
        LanguageId::JavaScript,
        LanguageId::Python,
        LanguageId::Go,
        LanguageId::Java,
        LanguageId::Rust,
        LanguageId::C,
        LanguageId::Cpp,
        LanguageId::CSharp,
        LanguageId::Ruby,
        LanguageId::Php,
        LanguageId::Swift,
        LanguageId::Kotlin,
        LanguageId::Hcl,
    ];

    #[test]
    fn every_language_hint_round_trips_through_its_display_name() {
        for language in ALL {
            assert_eq!(
                language_from_hint(&language.to_string()),
                Some(language),
                "{language}"
            );
        }
        assert_eq!(language_from_hint("cobol"), None);
    }

    #[test]
    fn a_language_references_its_own_runtime_and_any_native_declaration() {
        for language in ALL {
            assert!(language_can_reference(language, language), "{language}");
        }
        assert!(language_can_reference(
            LanguageId::TypeScript,
            LanguageId::JavaScript
        ));
        assert!(language_can_reference(LanguageId::Kotlin, LanguageId::Java));
        assert!(language_can_reference(LanguageId::Python, LanguageId::C));
        assert!(language_can_reference(
            LanguageId::JavaScript,
            LanguageId::Cpp
        ));
        assert!(language_can_reference(LanguageId::Rust, LanguageId::Go));
        // Managed runtimes do not reach each other, and native code does not
        // statically name managed code.
        assert!(!language_can_reference(
            LanguageId::Python,
            LanguageId::JavaScript
        ));
        assert!(!language_can_reference(
            LanguageId::TypeScript,
            LanguageId::Python
        ));
        assert!(!language_can_reference(LanguageId::C, LanguageId::Python));
        assert!(!language_can_reference(LanguageId::Hcl, LanguageId::C));
        assert!(!language_can_reference(LanguageId::Java, LanguageId::Ruby));
    }
}
