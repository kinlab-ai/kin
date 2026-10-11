// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The manifest a language's projects declare their name in.
//!
//! A language adapter names its manifest through
//! [`crate::LanguageAdapter::project_manifest`], and nothing outside the
//! adapters knows which manifest belongs to which language or how a name is
//! spelled in it. Kin reads a project's name from the manifest at a
//! repository's root to place what sits there beside no code, as
//! `flask :: LICENSE`.
//!
//! A unit is also named from its project where its language does so: a Rust
//! crate root takes its crate's name from the Cargo manifest above it. The
//! manifests reach an adapter at ingestion through [`ProjectFiles`], and what
//! it reads from them travels with the parse as a [`UnitContext`].

/// The manifest at the root of a language's projects, and how it declares
/// the project's name.
#[derive(Debug, Clone, Copy)]
pub struct ProjectManifest {
    /// The manifest's own name at a project's root, as `pyproject.toml`.
    pub name: &'static str,
    /// Priority when several manifests share a repository root, lowest
    /// first. Independent of adapter registration order so adding a language
    /// does not silently rename the root's artifacts.
    pub root_precedence: u8,
    /// Whether the directory holding this manifest names a namespace level
    /// even without code beside it. Root naming and namespace boundaries are
    /// separate facts: a root naming refactor must not change module names.
    pub directory_names_level: bool,
    /// Whether its adapter names units from it
    /// ([`crate::LanguageAdapter::unit_context`]), so that changing it can
    /// rename units whose own bytes did not change.
    pub names_units: bool,
    /// The project name the manifest's text declares, as written there, or
    /// `None` when it declares none.
    pub declared_name: fn(&str) -> Option<String>,
}

/// A Python project's: `[project] name`, or Poetry's `[tool.poetry] name`.
pub const PYPROJECT: ProjectManifest = ProjectManifest {
    name: "pyproject.toml",
    root_precedence: 0,
    directory_names_level: false,
    names_units: false,
    declared_name: |text| toml_name(text, &["project", "tool.poetry"]),
};

/// A JavaScript or TypeScript package's: `name`, known by the part after its
/// scope.
pub const PACKAGE_JSON: ProjectManifest = ProjectManifest {
    name: "package.json",
    root_precedence: 1,
    directory_names_level: true,
    names_units: false,
    declared_name: |text| {
        let value: serde_json::Value = serde_json::from_str(text).ok()?;
        let name = value.get("name")?.as_str()?;
        Some(name.rsplit('/').next().unwrap_or(name).to_string())
    },
};

/// A Rust crate's: `[package] name`.
pub const CARGO_TOML: ProjectManifest = ProjectManifest {
    name: "Cargo.toml",
    root_precedence: 2,
    directory_names_level: false,
    names_units: true,
    declared_name: |text| toml_name(text, &["package"]),
};

/// A Go module's: the last segment of its module path.
pub const GO_MOD: ProjectManifest = ProjectManifest {
    name: "go.mod",
    root_precedence: 3,
    directory_names_level: false,
    names_units: false,
    declared_name: |text| {
        text.lines().find_map(|line| {
            let module = line.trim().strip_prefix("module")?;
            module
                .starts_with(char::is_whitespace)
                .then(|| module.trim().trim_matches('"'))
                .map(|module| module.rsplit('/').next().unwrap_or(module).to_string())
        })
    },
};

/// The `name` a TOML document sets in the first of `tables` that sets one.
/// Reads the two spellings a manifest uses, `name = "x"` under a table
/// header, and nothing else of the format.
fn toml_name(text: &str, tables: &[&str]) -> Option<String> {
    tables.iter().find_map(|wanted| {
        let mut inside = false;
        text.lines().find_map(|line| {
            let line = line.trim();
            if let Some(header) = line.strip_prefix('[') {
                inside = header.trim_end_matches(']').trim() == *wanted && !header.starts_with('[');
                return None;
            }
            if !inside {
                return None;
            }
            let value = line.strip_prefix("name")?.trim_start().strip_prefix('=')?;
            let value = value.split('#').next().unwrap_or(value).trim();
            let quoted = value
                .strip_prefix('"')
                .and_then(|rest| rest.strip_suffix('"'))
                .or_else(|| {
                    value
                        .strip_prefix('\'')
                        .and_then(|rest| rest.strip_suffix('\''))
                })?;
            (!quoted.is_empty()).then(|| quoted.to_string())
        })
    })
}

/// Every manifest a registered adapter names, once each, in root precedence
/// order. The manifest's name breaks a tie.
pub fn all() -> &'static [ProjectManifest] {
    static ALL: std::sync::OnceLock<Vec<ProjectManifest>> = std::sync::OnceLock::new();
    ALL.get_or_init(|| crate::languages::AdapterRegistry::new().project_manifests())
}

/// Whether the repository-relative `path` names a manifest some registered
/// adapter reads, judged by its last segment alone.
pub fn is_manifest_path(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    all().iter().any(|manifest| manifest.name == name)
}

/// Whether the repository-relative `path` names a manifest some registered
/// adapter names units from ([`ProjectManifest::names_units`]), so that a
/// change to it can rename units whose own bytes did not change.
pub fn is_naming_manifest_path(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    all()
        .iter()
        .any(|manifest| manifest.names_units && manifest.name == name)
}

/// Whether a change to the repository-relative `path` can change what a
/// unit-naming manifest declares: `path` is one, or holds text in the same
/// format, as the target a linked `Cargo.toml` leads to can. Judged by name
/// alone, so a change to a source file never reaches for the tree.
pub fn could_hold_naming_manifest(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    let extension = |name: &str| {
        name.rsplit_once('.')
            .map(|(_, extension)| extension.to_string())
    };
    is_naming_manifest_path(path)
        || all().iter().any(|manifest| {
            manifest.names_units
                && extension(manifest.name).is_some()
                && extension(manifest.name) == extension(name)
        })
}

/// The project manifests of the tree a unit is indexed from, read at
/// ingestion.
///
/// Some languages name a unit from its project rather than from its path:
/// Rust code calls a crate root by the crate name its Cargo manifest gives,
/// which its directory need not spell. An adapter asks for the manifests it
/// needs by path through [`crate::LanguageAdapter::unit_context`], before the
/// unit is parsed, so a parse stays a function of the unit's path, its bytes
/// and what its project declares about it.
///
/// Only manifests answer. An implementation answers `Ok(None)` for any path
/// [`is_manifest_path`] refuses, so this is never a way to read some other
/// part of the tree.
pub trait ProjectFiles {
    /// The text of the manifest at the repository-relative `path`: `Ok(None)`
    /// when the tree holds no manifest there, and [`UnreadableManifest`] when
    /// it holds one whose text this reader cannot produce. A unit named under
    /// an unreadable manifest is not parsed, rather than named as if the
    /// manifest were absent.
    fn manifest(&self, path: &str) -> Result<Option<std::sync::Arc<str>>, UnreadableManifest>;

    /// The other repository-relative paths the manifest path `path` reads
    /// through when the tree holds a link there: each path a link in the
    /// chain names, whether or not the tree holds anything at it, ending at
    /// the body the manifest reads. Empty for a manifest held as a body, for
    /// none, or where the reader does not say. A change at any of them can
    /// change the manifest's text without a change at `path`.
    fn manifest_reads(&self, path: &str) -> Vec<String> {
        let _ = path;
        Vec::new()
    }
}

/// A manifest the tree holds that cannot be read: its bytes are missing from
/// the store, are not UTF-8, or the tree holds a link at its path rather than
/// a body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnreadableManifest {
    /// The manifest's repository-relative path.
    pub path: String,
    /// Why it cannot be read.
    pub reason: String,
}

impl std::fmt::Display for UnreadableManifest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "the project manifest {} cannot be read ({}), so the units it names are not parsed",
            self.path, self.reason
        )
    }
}

impl std::error::Error for UnreadableManifest {}

/// A tree whose manifests are not known. Every unit is named from its path,
/// as one in a tree that holds no manifest is.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoProjectFiles;

impl ProjectFiles for NoProjectFiles {
    fn manifest(&self, _path: &str) -> Result<Option<std::sync::Arc<str>>, UnreadableManifest> {
        Ok(None)
    }
}

/// What a unit's project declares about it, read before the unit is parsed.
///
/// Part of what a parse is a function of, so a parse kept for reuse is kept
/// under it as well as under the unit's path and bytes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct UnitContext {
    /// The name the unit's own module takes, where its language names it from
    /// its path and its project: a Rust crate root takes its crate's name and
    /// any other Rust source the name its path gives. `None` for a language
    /// that names its units some other way, which the unit's parse decides.
    pub module_name: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_manifest_reads_the_name_its_project_declares() {
        for (manifest, text, expected) in [
            (
                PYPROJECT,
                "[build-system]\nname = \"wrong\"\n\n[project]\nname = \"Flask\"  # the project\nversion = \"3\"\n",
                Some("Flask"),
            ),
            (PYPROJECT, "[tool.poetry]\nname = 'shop'\n", Some("shop")),
            (PYPROJECT, "[project]\nversion = \"1\"\n", None),
            (
                CARGO_TOML,
                "[workspace]\nmembers = []\n\n[package]\nname = \"kin-mcp\"\n",
                Some("kin-mcp"),
            ),
            (CARGO_TOML, "[workspace]\nmembers = []\n", None),
            (
                PACKAGE_JSON,
                "{\"name\": \"@scope/react-dom\", \"version\": \"1\"}",
                Some("react-dom"),
            ),
            (PACKAGE_JSON, "{\"private\": true}", None),
            (
                GO_MOD,
                "module github.com/example/server\n\ngo 1.22\n",
                Some("server"),
            ),
        ] {
            assert_eq!(
                (manifest.declared_name)(text).as_deref(),
                expected,
                "{}: {text}",
                manifest.name
            );
        }
    }

    #[test]
    fn only_text_in_a_naming_manifests_format_could_hold_one() {
        assert!(could_hold_naming_manifest("crates/db/Cargo.toml"));
        assert!(could_hold_naming_manifest("shared/app.cargo.toml"));
        assert!(!could_hold_naming_manifest("crates/db/src/lib.rs"));
        assert!(!could_hold_naming_manifest("web/package.json"));
    }

    #[test]
    fn only_a_manifest_units_are_named_from_reads_as_a_naming_manifest() {
        assert!(is_naming_manifest_path("crates/db/Cargo.toml"));
        assert!(is_manifest_path("web/package.json"));
        assert!(!is_naming_manifest_path("web/package.json"));
        assert!(!is_naming_manifest_path("pyproject.toml"));
        assert!(!is_naming_manifest_path("go.mod"));
    }

    #[test]
    fn only_a_manifest_name_reads_as_a_manifest_path() {
        assert!(is_manifest_path("Cargo.toml"));
        assert!(is_manifest_path("crates/db/Cargo.toml"));
        assert!(is_manifest_path("web/package.json"));
        assert!(!is_manifest_path("crates/db/src/lib.rs"));
        assert!(!is_manifest_path("crates/db/Cargo.lock"));
        assert!(!is_manifest_path("notCargo.toml"));
    }

    #[test]
    fn the_registered_adapters_name_each_manifest_once() {
        let names: Vec<&str> = all().iter().map(|manifest| manifest.name).collect();
        assert_eq!(
            names,
            ["pyproject.toml", "package.json", "Cargo.toml", "go.mod"]
        );
    }
}
