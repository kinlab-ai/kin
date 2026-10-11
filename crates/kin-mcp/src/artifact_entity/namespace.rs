// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The module an artifact is placed under.
//!
//! An artifact is addressed the way code is, `module :: name`, and its module
//! follows the code beside it: the module name code in the same place has, or
//! would have by the same derivation
//! ([`kin_ranking::module_qualifier`]). A schema that sits with the modules
//! of `shop` is `shop :: schema.sql`; a logo one directory below them is
//! `shop.static :: logo.png`; documentation beside a `conf` module that code
//! reads as `docs.conf` is `docs :: index.rst`.
//!
//! Nothing above the root gives code a name, so an artifact at the root would
//! have none. It takes the project name the root manifest declares, when the
//! tree holds one: `flask :: LICENSE`. With no manifest it is addressed by
//! its name and id alone, as an entity with no module is. Which manifests
//! there are, and how each declares a name, is the language adapters' to say
//! ([`kin_parser::project_manifest`]); each adapter also says whether the
//! manifest's directory names a level without code beside it.
//!
//! [`Namespaces::of`] is the whole rule. Every surface that writes or reads
//! an artifact's module asks it, so a change of rule is a change here.

use std::collections::{BTreeMap, HashMap, HashSet};

use kin_model::entity::{Entity, EntityKind};
use kin_model::graph::{EntityFilter, GraphStore};
use kin_model::{ResolvedTree, TreeEntry};
use kin_ranking::module_qualifier::ModuleIndex;

use kin_parser::project_manifest::ProjectManifest;

/// The directory part of a repository-relative place, empty at the root.
fn parent(place: &str) -> &str {
    place.rsplit_once('/').map_or("", |(dir, _)| dir)
}

/// A directory's own name.
fn base(dir: &str) -> &str {
    dir.rsplit('/').next().unwrap_or(dir)
}

/// Whether a directory's own name can stand as one segment of a dotted
/// module, by the rule the module derivation holds a directory to.
fn names_a_level(name: &str) -> bool {
    !name.is_empty()
        && !name
            .chars()
            .any(|ch| ch == '.' || ch == ':' || ch.is_whitespace() || ch.is_control())
}

/// Where every artifact of one tree is placed.
#[derive(Debug, Clone, Default)]
pub struct Namespaces {
    /// The namespace the code directly in a directory gives it.
    beside_code: HashMap<String, Vec<String>>,
    /// Directories whose held manifest names a namespace level.
    packages: HashSet<String>,
    /// The project name the root manifest declares.
    root: Option<String>,
}

impl Namespaces {
    /// Read the placement of code from `store`'s module records and `tree`.
    /// `root` is the project name the root manifest declares
    /// ([`declared_project_name`]), when the caller could read it.
    pub fn load<G: GraphStore + ?Sized>(
        store: &G,
        tree: &ResolvedTree,
        root: Option<String>,
    ) -> Result<Self, String> {
        let modules = store
            .query_entities(&EntityFilter {
                kinds: Some(vec![EntityKind::Module]),
                ..Default::default()
            })
            .map_err(|error| format!("graph read failed: {error}"))?;
        // The module that stands for each source: the widest it declares.
        let mut surfaces: BTreeMap<String, Entity> = BTreeMap::new();
        for module in &modules {
            let Some(origin) = module.file_origin.as_ref().map(|origin| origin.0.clone()) else {
                continue;
            };
            let width = |entity: &Entity| {
                entity
                    .span
                    .as_ref()
                    .map_or(0, |span| span.end_byte.saturating_sub(span.start_byte))
            };
            let wider = surfaces.get(&origin).is_none_or(|held| {
                width(module) > width(held) || (width(module) == width(held) && module.id < held.id)
            });
            if wider {
                surfaces.insert(origin, module.clone());
            }
        }
        let index = ModuleIndex::from_store_modules(store, modules);
        let mut beside_code: HashMap<String, Vec<String>> = HashMap::new();
        for (origin, module) in &surfaces {
            let dir = parent(origin);
            let mut namespace = index.path(module);
            // A package's own module stands for its directory; any other
            // module is one name inside it.
            if module.name != base(dir) {
                namespace.pop();
            }
            // Sources of one directory agree in every layout the module
            // derivation reads; where two languages share one, the longer
            // namespace is the more specific place.
            let longer = beside_code.get(dir).is_none_or(|held| {
                namespace.len() > held.len() || (namespace.len() == held.len() && namespace < *held)
            });
            if longer {
                beside_code.insert(dir.to_string(), namespace);
            }
        }
        let manifests = kin_parser::project_manifest::all();
        let packages = tree
            .artifacts_by_path()
            .filter_map(|artifact| artifact.path.as_utf8())
            .filter(|place| {
                manifests
                    .iter()
                    .any(|manifest| manifest.directory_names_level && manifest.name == base(place))
            })
            .map(|place| parent(place).to_string())
            .filter(|dir| !dir.is_empty() && names_a_level(base(dir)))
            .collect();
        Ok(Self {
            beside_code,
            packages,
            root: root.filter(|name| names_a_level(name)),
        })
    }

    /// The module the artifact projected at `place` is placed under, or
    /// `None` for one with no module, which is addressed by name and id.
    ///
    /// The one derivation: the namespace of the code beside it; otherwise the
    /// namespace of the nearest directory above that has one, by its code or
    /// by a package manifest, followed by the name of each directory between;
    /// otherwise those directory names from the root. An artifact at the root
    /// takes the declared project name.
    pub fn of(&self, place: &str) -> Option<String> {
        let mut dir = parent(place);
        if dir.is_empty() {
            return self.root.clone();
        }
        let mut below: Vec<&str> = Vec::new();
        let mut namespace = loop {
            if let Some(namespace) = self.beside_code.get(dir) {
                break namespace.clone();
            }
            if self.packages.contains(dir) {
                break vec![base(dir).to_string()];
            }
            if names_a_level(base(dir)) {
                below.push(base(dir));
            }
            dir = parent(dir);
            if dir.is_empty() {
                break Vec::new();
            }
        };
        namespace.extend(below.into_iter().rev().map(str::to_string));
        (!namespace.is_empty()).then(|| namespace.join("."))
    }
}

/// The root manifest `tree` holds that can declare a project name, and the
/// content to read it from: the first in the adapters' root precedence order
/// ([`kin_parser::project_manifest::all`]) that the root holds.
pub fn root_manifest(tree: &ResolvedTree) -> Option<(ProjectManifest, kin_model::Hash256)> {
    kin_parser::project_manifest::all()
        .iter()
        .find_map(|manifest| {
            let path = kin_model::RepoPath::from_utf8(manifest.name.to_string()).ok()?;
            match tree.artifact_at_path(&path)?.entry {
                TreeEntry::Blob { hash, .. } => Some((*manifest, hash)),
                _ => None,
            }
        })
}

/// The project name `manifest` declares in `text`, as one module segment:
/// lower case, and `None` when it could not stand as one.
pub fn declared_project_name(manifest: &ProjectManifest, text: &str) -> Option<String> {
    let name = (manifest.declared_name)(text)?.trim().to_ascii_lowercase();
    names_a_level(&name).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree_with_manifests(places: &[&str]) -> ResolvedTree {
        use kin_model::{ArtifactId, Hash256, RepoPath, ResolvedArtifact};
        ResolvedTree::from_artifacts(places.iter().enumerate().map(|(index, place)| {
            ResolvedArtifact::new(
                ArtifactId(uuid::Uuid::from_u128(index as u128 + 1)),
                RepoPath::from_utf8((*place).to_string()).unwrap(),
                TreeEntry::blob(Hash256::from_bytes([index as u8; 32]), false),
            )
        }))
        .unwrap()
    }

    #[test]
    fn mixed_language_roots_keep_the_existing_manifest_precedence() {
        // Artifact order is deliberately different from root precedence.
        for (held, expected) in [
            (
                vec!["go.mod", "Cargo.toml", "package.json", "pyproject.toml"],
                "pyproject.toml",
            ),
            (vec!["go.mod", "Cargo.toml", "package.json"], "package.json"),
            (vec!["go.mod", "Cargo.toml"], "Cargo.toml"),
            (vec!["go.mod"], "go.mod"),
        ] {
            let tree = tree_with_manifests(&held);
            let (manifest, hash) = root_manifest(&tree).unwrap();
            assert_eq!(manifest.name, expected);
            let expected_path = kin_model::RepoPath::from_utf8(expected.to_string()).unwrap();
            assert_eq!(
                tree.artifact_at_path(&expected_path).unwrap().entry,
                TreeEntry::blob(hash, false)
            );
        }
        assert!(root_manifest(&tree_with_manifests(&["nested/pyproject.toml"])).is_none());
    }

    #[test]
    fn manifest_adapters_preserve_existing_nested_namespace_boundaries() {
        let tree = tree_with_manifests(&[
            "projects/python/pyproject.toml",
            "projects/javascript/package.json",
            "projects/rust/Cargo.toml",
            "projects/go/go.mod",
            "projects/ordinary/README.md",
        ]);
        let graph = kin_db::InMemoryGraph::default();
        let namespaces = Namespaces::load(&graph, &tree, None).unwrap();
        assert_eq!(
            namespaces
                .of("projects/javascript/docs/guide.md")
                .as_deref(),
            Some("javascript.docs")
        );
        // Root naming manifests do not, on their own, rename nested
        // artifacts. Their namespaces continue to follow nearby code.
        for project in ["python", "rust", "go", "ordinary"] {
            assert_eq!(
                namespaces.of(&format!("projects/{project}/docs/guide.md")),
                Some(format!("projects.{project}.docs")),
            );
        }
    }

    #[test]
    fn a_declared_project_name_is_one_lower_case_segment() {
        use kin_parser::project_manifest::{CARGO_TOML, PYPROJECT};
        assert_eq!(
            declared_project_name(&PYPROJECT, "[project]\nname = \"Flask\"\n").as_deref(),
            Some("flask")
        );
        assert_eq!(
            declared_project_name(&CARGO_TOML, "[package]\nname = \"kin-mcp\"\n").as_deref(),
            Some("kin-mcp")
        );
        // A name that would read as two segments names no module.
        assert_eq!(
            declared_project_name(&PYPROJECT, "[project]\nname = \"zope.interface\"\n"),
            None
        );
    }

    fn placed(
        beside_code: &[(&str, &[&str])],
        packages: &[&str],
        root: Option<&str>,
    ) -> Namespaces {
        Namespaces {
            beside_code: beside_code
                .iter()
                .map(|(dir, namespace)| {
                    (
                        dir.to_string(),
                        namespace.iter().map(|part| part.to_string()).collect(),
                    )
                })
                .collect(),
            packages: packages.iter().map(|dir| dir.to_string()).collect(),
            root: root.map(str::to_string),
        }
    }

    #[test]
    fn an_artifact_follows_the_code_beside_it_and_the_root_takes_the_project_name() {
        let namespaces = placed(
            &[
                ("src/shop", &["shop"]),
                ("docs", &["docs"]),
                ("crates/kin/src", &[]),
            ],
            &["packages/react-dom"],
            Some("shop"),
        );
        for (place, expected) in [
            // Beside code.
            ("src/shop/schema.sql", Some("shop")),
            ("docs/index.rst", Some("docs")),
            // Below code: each directory between adds its name.
            ("src/shop/static/logo.png", Some("shop.static")),
            ("src/shop/static/icons/a.svg", Some("shop.static.icons")),
            ("docs/_static/logo.png", Some("docs._static")),
            // A package by its manifest.
            ("packages/react-dom/package.json", Some("react-dom")),
            ("packages/react-dom/npm/README.md", Some("react-dom.npm")),
            // No code above: named from the root, as code there would be.
            ("examples/data/items.csv", Some("examples.data")),
            // A directory that cannot be one segment names no level.
            (".github/workflows/ci.yml", Some("workflows")),
            // Code with no namespace of its own gives none.
            ("crates/kin/src/base.schema.json", None),
            ("crates/kin/src/fixtures/a.json", Some("fixtures")),
            // The root takes the project name.
            ("LICENSE", Some("shop")),
        ] {
            assert_eq!(namespaces.of(place).as_deref(), expected, "{place}");
        }
        // With no manifest the root has no module.
        assert_eq!(placed(&[], &[], None).of("LICENSE"), None);
        assert_eq!(
            placed(&[], &[], None).of("docs/guide.md").as_deref(),
            Some("docs")
        );
    }
}
