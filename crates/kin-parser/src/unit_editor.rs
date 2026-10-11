// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! How a language's source unit is written when a caller addresses the unit
//! itself rather than an entity in it.
//!
//! A caller names a unit by language identity (a Go package and its role)
//! and a declaration by its name and kind, never by a path or an offset: the
//! form that creates the first declaration of an empty repository, and every
//! declaration kind a language has. What only the language knows is how a
//! declaration it is sent reads, where it goes in the unit, how the unit's
//! imports are kept, which units are tests, and which units may share a
//! directory. Adapters state that through
//! [`crate::LanguageAdapter::unit_editor`], and the planner that publishes the
//! result names no language.
//!
//! Nothing here reads or writes storage and nothing here is stored: the
//! caller supplies a unit's bytes from repository authority, and the result
//! is reparsed and published through the ordinary reconcile path.

use std::collections::BTreeSet;
use std::ops::Range;
use std::sync::OnceLock;

use kin_model::{Entity, EntityKind, LanguageId};

use crate::languages::AdapterRegistry;

/// What a unit holds, in its language's own terms.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum UnitRole {
    /// Ordinary source.
    #[default]
    Source,
    /// Test code, run only by the language's test runner.
    Test,
}

impl UnitRole {
    pub fn label(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::Test => "test",
        }
    }
}

/// A top-level declaration kind a caller may create in a unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UnitDeclarationKind {
    Function,
    Method,
    Struct,
    Interface,
    /// A named type or alias whose underlying type is neither a struct nor an
    /// interface.
    Type,
    Const,
    Var,
}

impl UnitDeclarationKind {
    /// The graph kind the adapter derives for this declaration.
    pub fn entity_kind(self) -> EntityKind {
        match self {
            Self::Function => EntityKind::Function,
            Self::Method => EntityKind::Method,
            Self::Struct => EntityKind::Class,
            Self::Interface => EntityKind::Interface,
            Self::Type => EntityKind::TypeAlias,
            Self::Const => EntityKind::Constant,
            Self::Var => EntityKind::StaticVar,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Function => "function",
            Self::Method => "method",
            Self::Struct => "struct",
            Self::Interface => "interface",
            Self::Type => "type",
            Self::Const => "const",
            Self::Var => "var",
        }
    }

    /// Whether the declaration declares a type a method may attach to.
    pub fn declares_type(self) -> bool {
        matches!(self, Self::Struct | Self::Interface | Self::Type)
    }
}

/// One top-level declaration, named the way the graph names its entities.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitDeclaration {
    pub kind: UnitDeclarationKind,
    /// Top-level entity names in declaration order, exactly as the adapter
    /// names them.
    pub names: Vec<String>,
    /// The type a method attaches to.
    pub receiver: Option<String>,
    /// The declaration's bytes, with whatever leading comments and
    /// attributes its entity's span holds.
    pub range: Range<usize>,
}

/// One import a unit holds. `alias` is the explicit name it binds.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UnitImport {
    pub path: String,
    pub alias: Option<String>,
}

/// What one unit edit asks for.
#[derive(Debug, Clone, Default)]
pub struct UnitEdit {
    /// Complete declaration bodies, placed in this order.
    pub declarations: Vec<String>,
    pub add_imports: Vec<UnitImport>,
    /// Import paths to remove. Absent paths are not an error.
    pub remove_imports: Vec<String>,
}

/// The unit's new bytes and where each requested declaration landed in them.
#[derive(Debug, Clone)]
pub struct UnitEditOutcome {
    pub source: Vec<u8>,
    /// One per requested declaration, in request order, located in `source`.
    pub created: Vec<UnitDeclaration>,
    pub imports_changed: bool,
}

/// What a language tells the unit-addressed planner.
pub trait UnitEditor: Send + Sync {
    /// Refuse a unit identity that is not the language's own: a file path, an
    /// escape, a name the toolchain reads specially. `package` names where
    /// the unit sits in its project and `name` the unit itself. A refusal is
    /// one sentence that says how to address the unit instead.
    fn validate_unit(&self, package: &str, name: &str, role: UnitRole) -> Result<(), String>;

    /// Where the unit lives, relative to its project's root, for an identity
    /// [`Self::validate_unit`] accepted. Kin derives it; callers never name it.
    fn unit_path(&self, package: &str, name: &str, role: UnitRole) -> Result<String, String>;

    /// The unit as a refusal names it: never a path.
    fn describe_unit(&self, package: &str, name: &str, role: UnitRole) -> String;

    /// The refusal for a repository holding `count` of this language's
    /// projects, each marked by a `manifest`, where a project-relative unit
    /// would not say which it means.
    fn several_projects(&self, count: usize, manifest: &str) -> String {
        format!(
            "this repository holds {count} projects with a {manifest}; addressing a unit by a \
             project-relative package needs exactly one, and repositories with several are not \
             supported yet"
        )
    }

    /// Read one declaration a caller sent: its kind, the names the graph
    /// will give it, and the type it attaches to. A refusal is one sentence.
    fn parse_declaration(&self, body: &str) -> Result<UnitDeclaration, String>;

    /// The unit's bytes with `edit` applied: each declaration placed where the
    /// language puts it, and the import block rewritten. `existing` is `None`
    /// for a unit that does not exist yet, which the editor writes whole under
    /// `unit_name`.
    fn edit(
        &self,
        existing: Option<&[u8]>,
        unit_name: &str,
        edit: &UnitEdit,
    ) -> Result<UnitEditOutcome, String>;

    /// Refuse an import the unit cannot hold as written.
    fn validate_import(&self, import: &UnitImport) -> Result<(), String>;

    /// Whether `name` is one identifier of the language.
    fn is_identifier(&self, name: &str) -> bool;

    /// Whether the unit at `path` is a test unit.
    fn is_test_unit(&self, path: &str) -> bool;

    /// The unit name `entity` says it is declared in, when its adapter
    /// records one: a Go entity's package clause.
    fn declared_unit<'e>(&self, entity: &'e Entity) -> Option<&'e str>;

    /// Whether every unit in one directory shares one namespace, as a Go
    /// package's units do, so a name one of them declares is taken in all.
    /// By default each unit's names are its own.
    fn directory_shares_names(&self) -> bool {
        false
    }

    /// Refuse a directory whose source units would declare `sources` and
    /// whose test units would declare `tests`, when the language does not
    /// allow units of those names to share a directory.
    fn check_directory(
        &self,
        sources: &BTreeSet<String>,
        tests: &BTreeSet<String>,
    ) -> Result<(), String>;
}

/// The unit editor `language`'s adapter implements, or `None` when no
/// registered adapter handles it or its adapter implements none.
pub fn unit_editor_for(language: LanguageId) -> Option<&'static dyn UnitEditor> {
    static REGISTRY: OnceLock<AdapterRegistry> = OnceLock::new();
    REGISTRY
        .get_or_init(AdapterRegistry::new)
        .get_by_language(language)?
        .unit_editor()
}

/// The root of `language`'s project in a tree holding `paths`: the
/// directory of the one manifest the language's adapter names (Go's
/// `go.mod`, a `package.json`, a `pyproject.toml`, a `Cargo.toml`), or the
/// repository root when there is none. Several are refused, because a
/// project-relative unit would not say which project it means.
pub fn project_root<'a>(
    language: LanguageId,
    paths: impl IntoIterator<Item = &'a kin_model::RepoPath>,
) -> Result<String, String> {
    static REGISTRY: OnceLock<AdapterRegistry> = OnceLock::new();
    let Some(manifest) = REGISTRY
        .get_or_init(AdapterRegistry::new)
        .get_by_language(language)
        .and_then(|adapter| adapter.project_manifest())
    else {
        return Ok(String::new());
    };
    let mut roots = paths
        .into_iter()
        .map(ToString::to_string)
        .filter_map(|path| match path.rsplit_once('/') {
            Some((directory, file)) if file == manifest.name => Some(directory.to_string()),
            None if path == manifest.name => Some(String::new()),
            _ => None,
        })
        .collect::<Vec<_>>();
    roots.sort();
    roots.dedup();
    match roots.len() {
        0 => Ok(String::new()),
        1 => Ok(roots.remove(0)),
        count => Err(match unit_editor_for(language) {
            Some(editor) => editor.several_projects(count, manifest.name),
            None => format!("this repository holds {count} {} projects", manifest.name),
        }),
    }
}

/// The languages whose adapter writes units, by their wire names, in
/// registration order.
pub fn unit_languages() -> Vec<LanguageId> {
    static REGISTRY: OnceLock<AdapterRegistry> = OnceLock::new();
    let registry = REGISTRY.get_or_init(AdapterRegistry::new);
    registry
        .supported_languages()
        .into_iter()
        .filter(|language| {
            registry
                .get_by_language(*language)
                .is_some_and(|adapter| adapter.unit_editor().is_some())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every deeply supported language writes whole units; Go's editor also
    /// keeps a package's directory to one package.
    #[test]
    fn the_deeply_supported_languages_edit_units_addressed_by_identity() {
        let go = unit_editor_for(LanguageId::Go).expect("Go edits units");
        let declaration = go
            .parse_declaration("func (s *Store) Len() int { return 0 }")
            .unwrap();
        assert_eq!(
            (declaration.kind, declaration.names, declaration.receiver),
            (
                UnitDeclarationKind::Method,
                vec!["Store.Len".to_string()],
                Some("Store".to_string())
            )
        );
        assert!(go.is_test_unit("internal/store/store_test.go"));
        assert!(!go.is_test_unit("internal/store/store.go"));
        let held = |names: &[&str]| names.iter().map(|name| name.to_string()).collect();
        go.check_directory(&held(&["store"]), &held(&["store", "store_test"]))
            .unwrap();
        assert!(go
            .check_directory(&held(&["store", "other"]), &BTreeSet::new())
            .unwrap_err()
            .contains("a Go package directory holds one"));
        for (language, package, name, path, body, declared) in [
            (
                LanguageId::Python,
                "shop.models",
                "ledger",
                "shop/models/ledger.py",
                "def total(values):\n    return sum(values)",
                "total",
            ),
            (
                LanguageId::TypeScript,
                "src/runs",
                "cache",
                "src/runs/cache.ts",
                "export function cacheKey(id: string): string {\n  return id;\n}",
                "cacheKey",
            ),
            (
                LanguageId::JavaScript,
                "src",
                "util",
                "src/util.js",
                "export const double = (value) => value * 2;",
                "double",
            ),
            (
                LanguageId::Rust,
                ".",
                "lib",
                "src/lib.rs",
                "/// One.\npub fn one() -> u8 {\n    1\n}",
                "one",
            ),
        ] {
            let editor = unit_editor_for(language).unwrap_or_else(|| panic!("{language:?}"));
            assert_eq!(
                editor.unit_path(package, name, UnitRole::Source).unwrap(),
                path
            );
            assert_eq!(
                editor.parse_declaration(body).unwrap().names,
                vec![declared.to_string()],
                "{language:?}"
            );
        }
        for language in [LanguageId::Java, LanguageId::Kotlin, LanguageId::C] {
            assert!(unit_editor_for(language).is_none(), "{language:?}");
        }
    }
}
