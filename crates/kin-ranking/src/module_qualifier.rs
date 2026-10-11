// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Module-qualified names: `app._make_timedelta`, `flask.app._make_timedelta`,
//! `Scheduler.advanceTimers`.
//!
//! Several files routinely define one name. A caller who can type only a name
//! needs a spelling that pins one of them, and the graph already holds the
//! vocabulary for it. Every adapter mints a [`EntityKind::Module`] entity for a
//! source file's module surface, named by the language's own coordinate: a
//! Python file's stem or a package's directory, a JavaScript file's stem or an
//! `index` file's directory, a Go, Java or Kotlin file's declared package leaf.
//! A module-qualified name is the entity's own name preceded by that module's
//! name, and optionally by the names of the modules that enclose it.
//!
//! The qualifier is matched against Module entities, never against a path the
//! caller typed. Which module holds an entity is read from the graph's own
//! records: the Module entity that shares the entity's file, and the Module
//! entities that sit in each directory above it under that directory's name.
//! Nothing here touches the filesystem.
//!
//! A JavaScript or TypeScript package is one more level read the same way. A
//! package is a directory that holds a `package.json`, and most have no
//! `index` file at their root for an adapter to name after the directory:
//! `react-dom-bindings` keeps every source file under `src/`. Without the
//! package level those files are named by their stem alone
//! (`ReactDOMComponent`), which no question about "the functions in
//! react-dom-bindings" can be answered against. So a directory above a
//! JavaScript or TypeScript file also counts when the tree the graph holds
//! has a `package.json` directly in it, and names the level
//! (`react-dom-bindings.ReactDOMComponent`). The folders between a package
//! root and a file stay out of the name, as they do under an `index` file.
//! The manifest is looked up in the graph's own tree record by its exact
//! path; its bytes are never read, so the level carries the directory's name
//! and not the name the manifest declares. A scoped package
//! (`@scope/name`) therefore shows as its folder, `name`. A store whose tree
//! holds no manifest names its modules exactly as before.

use std::collections::{HashMap, HashSet};

use kin_model::entity::{Entity, EntityKind};
use kin_model::graph::{EntityFilter, EntityStore, GraphStore};
use kin_model::ids::{LanguageId, RepoPath};
use kin_model::EntityId;

/// The file whose presence directly in a directory makes that directory a
/// JavaScript or TypeScript package.
const PACKAGE_MANIFEST: &str = "package.json";

/// The Module entities a store holds, indexed the two ways a module chain is
/// read: by the file each one is the surface of, and by the directory it sits
/// in under its own name.
#[derive(Debug, Default, Clone)]
pub struct ModuleIndex {
    /// File path to the widest Module entity in that file: its module surface.
    by_file: HashMap<String, (EntityId, String)>,
    /// Directory to the names of the Module entities that sit directly in it.
    by_dir: HashMap<String, Vec<String>>,
    /// The directories above a JavaScript or TypeScript module that are
    /// packages: the graph's tree holds a manifest directly in each.
    package_dirs: HashSet<String>,
}

impl ModuleIndex {
    /// Index `modules`, which should be every Module entity the store holds.
    /// Knows no package directory: see [`Self::from_store_modules`] for an
    /// index that reads them from a store's tree.
    pub fn from_modules(modules: impl IntoIterator<Item = Entity>) -> Self {
        let mut widest: HashMap<String, (usize, EntityId, String)> = HashMap::new();
        let mut by_dir: HashMap<String, Vec<String>> = HashMap::new();
        for module in modules {
            if module.kind != EntityKind::Module {
                continue;
            }
            let Some(file) = module.file_origin.as_ref().map(|path| path.0.clone()) else {
                continue;
            };
            let width = module
                .span
                .as_ref()
                .map(|span| span.end_byte.saturating_sub(span.start_byte))
                .unwrap_or(0);
            let keep = match widest.get(&file) {
                None => true,
                Some((best, best_id, _)) => {
                    width > *best || (width == *best && module.id < *best_id)
                }
            };
            by_dir
                .entry(parent_dir(&file).to_string())
                .or_default()
                .push(module.name.clone());
            if keep {
                widest.insert(file, (width, module.id, module.name));
            }
        }
        Self {
            by_file: widest
                .into_iter()
                .map(|(file, (_, id, name))| (file, (id, name)))
                .collect(),
            by_dir,
            package_dirs: HashSet::new(),
        }
    }

    /// Index `modules`, every Module entity `store` holds, and read from the
    /// tree `store` holds which directories above its JavaScript and
    /// TypeScript modules are packages. One exact path lookup per distinct
    /// directory; a tree that holds no manifest leaves the index as
    /// [`Self::from_modules`] builds it.
    pub fn from_store_modules<G: GraphStore + ?Sized>(
        store: &G,
        modules: impl IntoIterator<Item = Entity>,
    ) -> Self {
        let mut above: HashSet<String> = HashSet::new();
        let index = Self::from_modules(modules.into_iter().inspect(|module| {
            if module.kind != EntityKind::Module || !package_directories(module.language) {
                return;
            }
            let Some(file) = module.file_origin.as_ref() else {
                return;
            };
            // A directory already seen has had every directory above it seen.
            let mut dir = parent_dir(&file.0);
            while !dir.is_empty() && above.insert(dir.to_string()) {
                dir = parent_dir(dir);
            }
        }));
        index.with_package_directories(
            above
                .into_iter()
                .filter(|dir| holds_package_manifest(store, dir)),
        )
    }

    /// This index with `directories` counted as packages, for a caller that
    /// already knows which directories hold a manifest. A directory whose
    /// name cannot be one segment of a dotted module path is left out.
    pub fn with_package_directories(
        mut self,
        directories: impl IntoIterator<Item = String>,
    ) -> Self {
        self.package_dirs.extend(
            directories
                .into_iter()
                .filter(|dir| names_a_level(base_name(dir))),
        );
        self
    }

    /// Every Module entity `store` holds, indexed, with the package
    /// directories its tree holds.
    pub fn load<G: GraphStore>(store: &G) -> Result<Self, <G as GraphStore>::Error> {
        let modules = store.query_entities(&EntityFilter {
            kinds: Some(vec![EntityKind::Module]),
            ..Default::default()
        })?;
        Ok(Self::from_store_modules(store, modules))
    }

    /// The module names that qualify `entity`, outermost first: the names of
    /// the enclosing modules the graph holds, then the entity's own module.
    /// Empty for an entity with no file or whose file has no module surface.
    ///
    /// Each directory above the entity's file counts once, and only when the
    /// graph holds a Module entity named after that directory that sits in it:
    /// a Python package's `__init__`, a JavaScript `index`, a Go or Java
    /// package's files. For a JavaScript or TypeScript entity a directory also
    /// counts when the index knows it as a package, one whose tree holds a
    /// manifest. A directory that is neither is skipped rather than
    /// ending the chain, so a JavaScript package root still qualifies the file
    /// four folders below it. The directory that holds the file itself is
    /// skipped when the file's own module already carries its name, because
    /// that module stands for it: a package file, or a Go file whose package is
    /// its directory.
    ///
    /// A module entity is never its own qualifier: the chain of a file's
    /// module surface is the modules around it.
    pub fn chain(&self, entity: &Entity) -> Vec<String> {
        let Some(file) = entity.file_origin.as_ref().map(|path| path.0.as_str()) else {
            return Vec::new();
        };
        let Some((own_id, own_name)) = self.by_file.get(file) else {
            return Vec::new();
        };
        let packages = package_directories(entity.language);
        let levels = directory_levels(file, |dir, base| {
            self.by_dir
                .get(dir)
                .is_some_and(|names| names.iter().any(|name| name == base))
                || (packages && self.package_dirs.contains(dir))
        });
        let mut outer = enclosing_module_names(&levels, own_name, namespace_directories(entity));
        if *own_id != entity.id {
            outer.push(own_name.clone());
        }
        outer
    }

    /// The module path that places `entity`, outermost first: its
    /// [`Self::chain`], followed by its own name when `entity` is its file's
    /// module surface. Every declaration in a file and the file's module share
    /// one path, the one a reply's `module` field spells (`flask.sansio.app`).
    /// Empty for an entity with no file or whose file has no module surface.
    pub fn path(&self, entity: &Entity) -> Vec<String> {
        let mut path = self.chain(entity);
        let surface = entity
            .file_origin
            .as_ref()
            .and_then(|file| self.by_file.get(file.0.as_str()));
        if let Some((own_id, own_name)) = surface {
            if *own_id == entity.id {
                path.push(own_name.clone());
            }
        }
        path
    }
}

/// Whether directories without a module of their own still name a module
/// level inside an enclosing package. Python's are namespace packages
/// (PEP 420): `flask/sansio/app.py` imports as `flask.sansio.app` with no
/// `sansio/__init__.py`. A JavaScript folder without an `index` is not part
/// of any import name, so it stays skipped.
fn namespace_directories(entity: &Entity) -> bool {
    entity.language == LanguageId::Python
}

/// Whether a directory that holds a package manifest names a module level for
/// an entity of `language`. JavaScript and TypeScript only: theirs is the
/// packaging in which a package root commonly holds no source file an adapter
/// could name after the directory.
fn package_directories(language: LanguageId) -> bool {
    matches!(language, LanguageId::JavaScript | LanguageId::TypeScript)
}

/// Whether `base`, a directory's own name, can stand as one segment of a
/// dotted module path. A name that holds a `.` would read as two segments and
/// one with a space would break a row's `module :: name`, so neither names a
/// level and its files keep the name they had.
fn names_a_level(base: &str) -> bool {
    !base.is_empty()
        && !base
            .chars()
            .any(|ch| ch == '.' || ch == ':' || ch.is_whitespace() || ch.is_control())
}

/// Whether the tree `store` holds has a package manifest directly in `dir`.
/// A lookup of one exact path in the graph's own tree record: no file is
/// opened, and a tree that does not hold the path answers `false`.
fn holds_package_manifest<G: GraphStore + ?Sized>(store: &G, dir: &str) -> bool {
    names_a_level(base_name(dir))
        && RepoPath::from_utf8(format!("{dir}/{PACKAGE_MANIFEST}"))
            .ok()
            .is_some_and(|path| EntityStore::artifact_id_at_path(store, &path).is_some())
}

/// The last segment of a directory path: the directory's own name.
fn base_name(dir: &str) -> &str {
    dir.rsplit('/').next().unwrap_or(dir)
}

/// Each directory above `file`, nearest first, with whether the graph holds a
/// module named after it that sits directly in it.
fn directory_levels(file: &str, mut named: impl FnMut(&str, &str) -> bool) -> Vec<(&str, bool)> {
    let mut levels = Vec::new();
    let mut dir = parent_dir(file);
    while !dir.is_empty() {
        let base = dir.rsplit('/').next().unwrap_or(dir);
        levels.push((base, named(dir, base)));
        dir = parent_dir(dir);
    }
    levels
}

/// The enclosing module names for a file whose own module is `own_name`,
/// outermost first, from its directory levels (nearest first). A level counts
/// when the graph holds a module named after it, or, with
/// `namespace_directories`, when it lies inside the outermost such package.
/// The nearest level is skipped when the file's own module stands for it.
fn enclosing_module_names(
    levels: &[(&str, bool)],
    own_name: &str,
    namespace_directories: bool,
) -> Vec<String> {
    // With namespace directories, every level inside the outermost package
    // counts. A Python file in no package at all (a tests/ folder) is named
    // from the repository root, as pytest's rootdir import names it, so its
    // several `conftest` modules stay apart.
    let root = if namespace_directories && !levels.is_empty() {
        Some(
            levels
                .iter()
                .rposition(|(_, named)| *named)
                .unwrap_or(levels.len() - 1),
        )
    } else {
        None
    };
    let mut outer: Vec<String> = levels
        .iter()
        .enumerate()
        .filter(|(depth, (base, named))| {
            let stands_for_it = *depth == 0 && own_name == *base;
            !stands_for_it && (*named || root.is_some_and(|root| *depth <= root))
        })
        .map(|(_, (base, _))| base.to_string())
        .collect();
    outer.reverse();
    outer
}

/// Bound on modules sharing one directory name read while qualifying a module.
const MAX_SAME_NAMED_MODULES: usize = 512;

/// The dotted name of `module` read from the graph's own module records, the
/// same chain [`ModuleIndex::chain`] gives its members followed by the module's
/// own name: `flask.sansio.app`. Reads one bounded name lookup per enclosing
/// directory instead of every module the store holds, and for a JavaScript or
/// TypeScript module one tree lookup for that directory's package manifest,
/// so it agrees with an index [`ModuleIndex::load`] built. `None` when the module
/// has no file or a lookup would exceed its bound, so no partial name is shown.
pub fn module_path<G: GraphStore + ?Sized>(
    store: &G,
    module: &Entity,
) -> Result<Option<String>, <G as GraphStore>::Error> {
    if module.kind != EntityKind::Module || module.name.is_empty() {
        return Ok(None);
    }
    let Some(file) = module.file_origin.as_ref().map(|path| path.0.as_str()) else {
        return Ok(None);
    };
    let packages = package_directories(module.language);
    let mut bounded = true;
    let mut lookups: Vec<(String, bool)> = Vec::new();
    let mut dir = parent_dir(file);
    while !dir.is_empty() {
        let base = dir.rsplit('/').next().unwrap_or(dir);
        let page = store.query_entities_page(
            &EntityFilter {
                kinds: Some(vec![EntityKind::Module]),
                name_pattern: Some(base.to_string()),
                ..Default::default()
            },
            &kin_model::graph::EntityPage::first(MAX_SAME_NAMED_MODULES),
        )?;
        if page.total_matching > MAX_SAME_NAMED_MODULES || page.next_offset.is_some() {
            bounded = false;
            break;
        }
        let named = page.entities.iter().any(|candidate| {
            candidate.name == base
                && candidate
                    .file_origin
                    .as_ref()
                    .is_some_and(|origin| parent_dir(&origin.0) == dir)
        }) || (packages && holds_package_manifest(store, dir));
        lookups.push((dir.to_string(), named));
        dir = parent_dir(dir);
    }
    if !bounded {
        return Ok(None);
    }
    let levels = directory_levels(file, |dir, _| {
        lookups
            .iter()
            .any(|(looked, named)| looked == dir && *named)
    });
    let mut names = enclosing_module_names(&levels, &module.name, namespace_directories(module));
    names.push(module.name.clone());
    Ok(Some(names.join(".")))
}

/// The directory part of a repository-relative path, empty at the root.
fn parent_dir(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

/// Where a qualified query can be split into a module qualifier and the name
/// it qualifies, longest name first: `flask.app._make_timedelta` splits as
/// (`flask`, `app._make_timedelta`) and then (`flask.app`, `_make_timedelta`).
///
/// Both separators the graph's names use are read, `.` and `::`, and neither is
/// read inside angle brackets or parentheses, so `Router<S>::route` keeps its
/// generic argument. A split that leaves an empty segment is skipped.
pub fn qualifier_splits(query: &str) -> Vec<(Vec<String>, String)> {
    let separators = separator_positions(query);
    let mut splits = Vec::new();
    for (index, (at, width)) in separators.iter().enumerate() {
        let tail = &query[at + width..];
        if tail.is_empty() {
            continue;
        }
        let mut segments = Vec::with_capacity(index + 1);
        let mut start = 0;
        for (sep_at, sep_width) in &separators[..=index] {
            segments.push(query[start..*sep_at].to_string());
            start = sep_at + sep_width;
        }
        if segments.iter().any(String::is_empty) {
            continue;
        }
        splits.push((segments, tail.to_string()));
    }
    splits
}

fn separator_positions(query: &str) -> Vec<(usize, usize)> {
    let bytes = query.as_bytes();
    let mut depth = 0usize;
    let mut positions = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'<' | b'(' => depth += 1,
            b'>' | b')' => depth = depth.saturating_sub(1),
            b':' if depth == 0 && bytes.get(index + 1) == Some(&b':') => {
                positions.push((index, 2));
                index += 2;
                continue;
            }
            b'.' if depth == 0 => positions.push((index, 1)),
            _ => {}
        }
        index += 1;
    }
    positions
}

/// Whether `qualifier` names `chain`'s innermost modules, in order.
pub fn qualifier_matches_chain(qualifier: &[String], chain: &[String]) -> bool {
    !qualifier.is_empty() && chain.ends_with(qualifier)
}

/// The shortest spelling that names each candidate apart from the others, for
/// a listing a caller can paste back: the entity's own name when no other
/// candidate carries it, otherwise that name behind as few of its enclosing
/// module names as it takes. `None` when even the whole chain does not tell a
/// candidate apart, which leaves its entity id as the only address.
pub fn distinguishing_names(
    candidates: &[Entity],
    index: &ModuleIndex,
) -> HashMap<EntityId, String> {
    let chains: Vec<Vec<String>> = candidates
        .iter()
        .map(|candidate| index.chain(candidate))
        .collect();
    let mut names = HashMap::new();
    for (position, candidate) in candidates.iter().enumerate() {
        let chain = &chains[position];
        for depth in 0..=chain.len() {
            let suffix = &chain[chain.len() - depth..];
            let shared = candidates.iter().enumerate().any(|(other, rival)| {
                other != position && rival.name == candidate.name && chains[other].ends_with(suffix)
            });
            if !shared {
                let mut spelled = suffix.join(".");
                if !spelled.is_empty() {
                    spelled.push('.');
                }
                spelled.push_str(&candidate.name);
                names.insert(candidate.id, spelled);
                break;
            }
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use kin_model::entity::{
        EntityMetadata, EntityRole, FingerprintAlgorithm, SemanticFingerprint, SourceSpan,
        Visibility,
    };
    use kin_model::ids::{FilePathId, Hash256, LanguageId};

    fn entity(name: &str, kind: EntityKind, file: &str, width: usize) -> Entity {
        let id = EntityId::new();
        Entity {
            id,
            kind,
            name: name.to_string(),
            language: LanguageId::Python,
            fingerprint: SemanticFingerprint {
                algorithm: FingerprintAlgorithm::V1TreeSitter,
                ast_hash: Hash256::from_bytes([0; 32]),
                signature_hash: Hash256::from_bytes([0; 32]),
                behavior_hash: Hash256::from_bytes([0; 32]),
                equivalence_hash: Hash256::from_bytes([0; 32]),
                stability_score: 1.0,
            },
            file_origin: Some(FilePathId::new(file)),
            span: Some(SourceSpan {
                file: FilePathId::new(file),
                start_byte: 0,
                end_byte: width,
                start_line: 1,
                start_col: 0,
                end_line: 1,
                end_col: 0,
            }),
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

    fn module(name: &str, file: &str) -> Entity {
        entity(name, EntityKind::Module, file, 10_000)
    }

    /// The flask layout the head-to-head asked about: one package holding
    /// `app.py`, and a subpackage holding another `app.py`, each defining
    /// `_make_timedelta`.
    fn flask_layout() -> (ModuleIndex, Entity, Entity) {
        let index = ModuleIndex::from_modules(vec![
            module("flask", "src/flask/__init__.py"),
            module("app", "src/flask/app.py"),
            module("sansio", "src/flask/sansio/__init__.py"),
            module("app", "src/flask/sansio/app.py"),
        ]);
        let top = entity(
            "_make_timedelta",
            EntityKind::Function,
            "src/flask/app.py",
            40,
        );
        let sansio = entity(
            "_make_timedelta",
            EntityKind::Function,
            "src/flask/sansio/app.py",
            40,
        );
        (index, top, sansio)
    }

    #[test]
    fn a_chain_reads_enclosing_packages_then_the_file_module() {
        let (index, top, sansio) = flask_layout();
        assert_eq!(index.chain(&top), vec!["flask", "app"]);
        assert_eq!(index.chain(&sansio), vec!["flask", "sansio", "app"]);
    }

    /// Flask as it ships: `sansio` is a namespace package with no
    /// `__init__.py`, so no module entity is named after it. Python still
    /// imports `flask.sansio.app`, and the two `app` modules must stay apart.
    #[test]
    fn a_python_namespace_directory_inside_a_package_still_qualifies() {
        let index = ModuleIndex::from_modules(vec![
            module("flask", "src/flask/__init__.py"),
            module("app", "src/flask/app.py"),
            module("app", "src/flask/sansio/app.py"),
        ]);
        let top = entity(
            "_make_timedelta",
            EntityKind::Function,
            "src/flask/app.py",
            40,
        );
        let sansio = entity(
            "_make_timedelta",
            EntityKind::Function,
            "src/flask/sansio/app.py",
            40,
        );
        assert_eq!(index.chain(&top), vec!["flask", "app"]);
        assert_eq!(index.chain(&sansio), vec!["flask", "sansio", "app"]);
        let names = distinguishing_names(&[top.clone(), sansio.clone()], &index);
        assert_eq!(names[&top.id], "flask.app._make_timedelta");
        assert_eq!(names[&sansio.id], "sansio.app._make_timedelta");
        // A directory above the outermost package is not a module level.
        let top = entity(
            "_make_timedelta",
            EntityKind::Function,
            "src/flask/app.py",
            40,
        );
        assert!(!index.chain(&top).contains(&"src".to_string()));
        // A file in no package is named from the repository root, so the
        // test suite's and an example's conftest stay apart.
        let fixture = entity(
            "leak_detector",
            EntityKind::Function,
            "tests/conftest.py",
            40,
        );
        let example = entity(
            "app",
            EntityKind::Function,
            "examples/tutorial/tests/conftest.py",
            40,
        );
        let with_tests = ModuleIndex::from_modules(vec![
            module("conftest", "tests/conftest.py"),
            module("conftest", "examples/tutorial/tests/conftest.py"),
        ]);
        assert_eq!(with_tests.chain(&fixture), vec!["tests", "conftest"]);
        assert_eq!(
            with_tests.chain(&example),
            vec!["examples", "tutorial", "tests", "conftest"]
        );
    }

    /// A file's module surface and the declarations in it share one path,
    /// so a module that calls something is placed where its members are.
    #[test]
    fn a_path_places_a_module_surface_beside_its_members() {
        let (index, top, sansio) = flask_layout();
        assert_eq!(index.path(&top), vec!["flask", "app"]);
        assert_eq!(index.path(&sansio), vec!["flask", "sansio", "app"]);
        let package = module("flask", "src/flask/__init__.py");
        let file = module("app", "src/flask/sansio/app.py");
        let index = ModuleIndex::from_modules(vec![package.clone(), file.clone()]);
        assert_eq!(index.path(&package), vec!["flask"]);
        assert_eq!(index.path(&file), vec!["flask", "sansio", "app"]);
        let unplaced = entity("x", EntityKind::Function, "scripts/run.sh", 4);
        assert!(index.path(&unplaced).is_empty());
    }

    #[test]
    fn a_package_file_is_not_its_own_qualifier() {
        let (index, _, _) = flask_layout();
        let in_init = entity("Flask", EntityKind::Class, "src/flask/__init__.py", 40);
        assert_eq!(index.chain(&in_init), vec!["flask"]);
        let the_package = ModuleIndex::from_modules(vec![module("flask", "src/flask/__init__.py")]);
        let surface = module("flask", "src/flask/__init__.py");
        let index_with_surface = ModuleIndex::from_modules(vec![surface.clone()]);
        assert!(index_with_surface.chain(&surface).is_empty());
        assert_eq!(the_package.chain(&in_init), vec!["flask"]);
    }

    #[test]
    fn a_directory_without_a_module_entity_is_skipped_not_a_stop() {
        let index = ModuleIndex::from_modules(vec![
            module("scheduler", "packages/scheduler/index.js"),
            module("Scheduler", "packages/scheduler/src/forks/Scheduler.js"),
            module(
                "SchedulerMock",
                "packages/scheduler/src/forks/SchedulerMock.js",
            ),
        ]);
        let mut real = entity(
            "advanceTimers",
            EntityKind::Function,
            "packages/scheduler/src/forks/Scheduler.js",
            40,
        );
        let mut mock = entity(
            "advanceTimers",
            EntityKind::Function,
            "packages/scheduler/src/forks/SchedulerMock.js",
            40,
        );
        real.language = LanguageId::JavaScript;
        mock.language = LanguageId::JavaScript;
        assert_eq!(index.chain(&real), vec!["scheduler", "Scheduler"]);
        assert_eq!(index.chain(&mock), vec!["scheduler", "SchedulerMock"]);
        let names = distinguishing_names(&[real.clone(), mock.clone()], &index);
        assert_eq!(names[&real.id], "Scheduler.advanceTimers");
        assert_eq!(names[&mock.id], "SchedulerMock.advanceTimers");
    }

    fn script(name: &str, kind: EntityKind, file: &str) -> Entity {
        let width = if kind == EntityKind::Module {
            10_000
        } else {
            40
        };
        let mut entity = entity(name, kind, file, width);
        entity.language = LanguageId::JavaScript;
        entity
    }

    fn segments(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    /// The layout a scored question asked about: a package whose root holds a
    /// manifest and no `index` file, with every source file under `src/`.
    /// The package names the level; the folders between it and the file do
    /// not. The spelling a reply printed before the package level existed,
    /// the file's module alone, still names the same entity.
    #[test]
    fn a_package_directory_names_a_level_for_a_script_file() {
        let file = "packages/react-dom-bindings/src/client/ReactDOMComponent.js";
        let surface = script("ReactDOMComponent", EntityKind::Module, file);
        let member = script("getStylesObjectFromElement", EntityKind::Function, file);
        let bare = ModuleIndex::from_modules(vec![surface.clone()]);
        assert_eq!(bare.path(&member), vec!["ReactDOMComponent"]);

        let index = bare.with_package_directories(["packages/react-dom-bindings".to_string()]);
        assert_eq!(
            index.path(&member),
            vec!["react-dom-bindings", "ReactDOMComponent"]
        );
        assert_eq!(index.path(&surface), index.path(&member));
        assert_eq!(index.chain(&surface), vec!["react-dom-bindings"]);
        for spelling in [
            &["ReactDOMComponent"][..],
            &["react-dom-bindings", "ReactDOMComponent"][..],
        ] {
            assert!(
                qualifier_matches_chain(&segments(spelling), &index.chain(&member)),
                "{spelling:?}"
            );
        }
        assert!(!qualifier_matches_chain(
            &segments(&["react-dom", "ReactDOMComponent"]),
            &index.chain(&member)
        ));
    }

    /// A package whose root does hold an `index` file is named once, by
    /// either record, and every package directory above a file counts.
    #[test]
    fn a_package_directory_counts_once_beside_an_index_module() {
        let root = script(
            "react-reconciler",
            EntityKind::Module,
            "packages/react-reconciler/index.js",
        );
        let work_loop = "packages/react-reconciler/src/ReactFiberWorkLoop.js";
        let nested = "compiler/packages/snap/src/runner.js";
        let index = ModuleIndex::from_modules(vec![
            root.clone(),
            script("ReactFiberWorkLoop", EntityKind::Module, work_loop),
            script("runner", EntityKind::Module, nested),
        ])
        .with_package_directories([
            "packages/react-reconciler".to_string(),
            "compiler".to_string(),
            "compiler/packages/snap".to_string(),
        ]);
        let member = script("scheduleUpdateOnFiber", EntityKind::Function, work_loop);
        assert_eq!(
            index.path(&member),
            vec!["react-reconciler", "ReactFiberWorkLoop"]
        );
        // The index file's module already carries the directory's name.
        assert_eq!(index.path(&root), vec!["react-reconciler"]);
        let inner = script("run", EntityKind::Function, nested);
        assert_eq!(index.path(&inner), vec!["compiler", "snap", "runner"]);
    }

    /// The package level is JavaScript's and TypeScript's alone, and a
    /// directory whose name cannot be one dotted segment names nothing.
    #[test]
    fn a_package_directory_names_nothing_it_cannot_name() {
        let python = "tools/build/release.py";
        let dotted = "sites/kin.dev/src/page.js";
        let index = ModuleIndex::from_modules(vec![
            module("release", python),
            script("page", EntityKind::Module, dotted),
        ])
        .with_package_directories(["tools/build".to_string(), "sites/kin.dev".to_string()]);
        // A Python file in no package is named from the repository root, with
        // or without a manifest beside it.
        let release = entity("cut", EntityKind::Function, python, 40);
        assert_eq!(index.path(&release), vec!["tools", "build", "release"]);
        let page = script("render", EntityKind::Function, dotted);
        assert_eq!(index.path(&page), vec!["page"]);
        let mut typed = script("render", EntityKind::Function, dotted);
        typed.language = LanguageId::TypeScript;
        assert_eq!(index.path(&typed), vec!["page"]);
    }

    #[test]
    fn a_package_per_directory_module_stands_for_its_own_directory() {
        // Go and Java name a file's module by its package, which is usually its
        // directory, so the directory is not counted a second time.
        let index = ModuleIndex::from_modules(vec![
            module("storage", "pkg/storage/disk.go"),
            module("storage", "pkg/storage/memory.go"),
            module("pkg", "pkg/doc.go"),
        ]);
        let open = entity("Open", EntityKind::Function, "pkg/storage/disk.go", 40);
        assert_eq!(index.chain(&open), vec!["pkg", "storage"]);
    }

    #[test]
    fn distinguishing_names_use_as_few_modules_as_it_takes() {
        let (index, top, sansio) = flask_layout();
        let names = distinguishing_names(&[top.clone(), sansio.clone()], &index);
        assert_eq!(names[&top.id], "flask.app._make_timedelta");
        assert_eq!(names[&sansio.id], "sansio.app._make_timedelta");
        // A name no other candidate carries needs no module at all.
        let lone = entity("Flask.get", EntityKind::Method, "src/flask/app.py", 40);
        let other = entity("Blueprint.get", EntityKind::Method, "src/flask/app.py", 40);
        let names = distinguishing_names(&[lone.clone(), other], &index);
        assert_eq!(names[&lone.id], "Flask.get");
    }

    #[test]
    fn twins_in_one_file_have_no_distinguishing_name() {
        let (index, top, _) = flask_layout();
        let twin = entity(
            "_make_timedelta",
            EntityKind::Function,
            "src/flask/app.py",
            40,
        );
        let names = distinguishing_names(&[top.clone(), twin.clone()], &index);
        assert!(!names.contains_key(&top.id) && !names.contains_key(&twin.id));
    }

    #[test]
    fn splits_run_longest_tail_first_and_keep_generic_arguments_whole() {
        assert_eq!(
            qualifier_splits("flask.app._make_timedelta"),
            vec![
                (vec!["flask".to_string()], "app._make_timedelta".to_string()),
                (
                    vec!["flask".to_string(), "app".to_string()],
                    "_make_timedelta".to_string()
                ),
            ]
        );
        assert_eq!(
            qualifier_splits("routing::Router<S::Body>::route"),
            vec![
                (
                    vec!["routing".to_string()],
                    "Router<S::Body>::route".to_string()
                ),
                (
                    vec!["routing".to_string(), "Router<S::Body>".to_string()],
                    "route".to_string()
                ),
            ]
        );
        assert!(qualifier_splits("advanceTimers").is_empty());
        assert!(qualifier_splits(".hidden").is_empty());
        assert!(qualifier_splits("trailing.").is_empty());
    }

    #[test]
    fn a_qualifier_matches_only_the_innermost_modules_in_order() {
        let chain = vec!["flask".to_string(), "sansio".to_string(), "app".to_string()];
        let q = |parts: &[&str]| parts.iter().map(|p| p.to_string()).collect::<Vec<_>>();
        assert!(qualifier_matches_chain(&q(&["app"]), &chain));
        assert!(qualifier_matches_chain(&q(&["sansio", "app"]), &chain));
        assert!(qualifier_matches_chain(
            &q(&["flask", "sansio", "app"]),
            &chain
        ));
        assert!(!qualifier_matches_chain(&q(&["flask", "app"]), &chain));
        assert!(!qualifier_matches_chain(&q(&["sansio"]), &chain));
        assert!(!qualifier_matches_chain(&[], &chain));
    }
}
