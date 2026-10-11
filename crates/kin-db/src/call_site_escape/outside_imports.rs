// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Whether a receiver a file imports comes from outside the repository.
//!
//! `json.load` in a file that says `import json` reads the `load` of the
//! module `json`. When no file of the repository can be that module, the read
//! gets a declaration of some other project, whatever this repository names
//! `load`. The census counts such a read apart instead of listing it as a use
//! that could hold a same-named focal, and a reference answer says what a
//! listed call's receiver is imported from.
//!
//! Two recorded facts decide it, and nothing is searched for:
//!
//! - the reading file's record binds the receiver at module scope by one
//!   import statement and nothing else, and resolves a receiver read on the
//!   read's own line to that statement ([`receiver_import`]);
//! - the selected tree holds no file an import of the module's first name
//!   could find ([`ImportRoots::outside`]).
//!
//! Only Python is read. Its import of `a.b` finds `a` as a module file or a
//! directory under an import root. A directory that holds `__init__.py` is a
//! package, reached through its own name and never a root, except for the
//! file that is run from inside it, whose own directory comes first. So a
//! file can supply `a` only as `<root>/a.py`, `<root>/a.pyi` or anything under
//! `<root>/a/`, where `<root>` is the repository root, a directory that is not
//! a package, or the importing file's own directory. A repository whose root
//! is itself a package is imported under a name its tree never writes, so
//! nothing is judged outside it.
//!
//! What this cannot see is a module the repository supplies some other way: a
//! path entry added while the program runs, a package installed under a name
//! its directory does not have, or a compiled module with no Python file.

use std::collections::{HashMap, HashSet};

use kin_model::{
    readable_record_version, FilePathId, Hash256, LanguageId, MentionForm, OccurrenceBinding,
    ReceiverKind, ValueOccurrence, VALUE_ESCAPE_RECORD_KEY,
};
use serde_json::Value;

use super::{blob_of, EscapeCensusGraph};

/// Whether `path` is a file a Python import can find.
pub(super) fn is_python_path(path: &str) -> bool {
    matches!(path.rsplit_once('.'), Some((_, "py" | "pyi")))
}

/// Whether `name` is one identifier: letters, digits and underscores, not
/// starting with a digit.
fn is_identifier(name: &str) -> bool {
    let mut characters = name.chars();
    characters
        .next()
        .is_some_and(|first| first == '_' || first.is_alphabetic())
        && characters.all(|character| character == '_' || character.is_alphanumeric())
}

/// The directory of a repository path, empty for a file at the root.
fn directory(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(directory, _)| directory)
}

/// The names an import could find in the selected tree, each with the
/// directories it would be found under, and the directories that are
/// packages.
#[derive(Debug, Default)]
pub struct ImportRoots {
    supplies: HashMap<String, HashSet<String>>,
    packages: HashSet<String>,
}

impl ImportRoots {
    /// Read the Python files among `paths`, every other path ignored.
    pub fn of<'p>(paths: impl IntoIterator<Item = &'p str>) -> Self {
        let mut roots = Self::default();
        for path in paths {
            let Some((stem, "py" | "pyi")) = path.rsplit_once('.') else {
                continue;
            };
            let segments: Vec<&str> = stem.split('/').collect();
            let Some((file, directories)) = segments.split_last() else {
                continue;
            };
            let above = directories.join("/");
            if *file == "__init__" {
                roots.packages.insert(above);
            } else {
                roots
                    .supplies
                    .entry((*file).to_string())
                    .or_default()
                    .insert(above);
            }
            // A directory on the way to a Python file is a package, regular
            // or namespace, of whatever directory holds it.
            for (depth, name) in directories.iter().enumerate() {
                roots
                    .supplies
                    .entry((*name).to_string())
                    .or_default()
                    .insert(directories[..depth].join("/"));
            }
        }
        roots
    }

    /// Whether an import of `module`, written in `importer`, names a module
    /// no file of the tree can supply. `false` for a relative import, for a
    /// name that is not a plain dotted one, and whenever the tree leaves it
    /// open.
    pub fn outside(&self, importer: &str, module: &str) -> bool {
        if !is_python_path(importer) || self.packages.contains("") {
            return false;
        }
        let Some(first) = module
            .split('.')
            .next()
            .filter(|first| is_identifier(first))
        else {
            return false;
        };
        if !module.split('.').all(is_identifier) {
            return false;
        }
        let own = directory(importer);
        !self.supplies.get(first).is_some_and(|under| {
            under
                .iter()
                .any(|root| root.is_empty() || root == own || !self.packages.contains(root))
        })
    }
}

/// The module the receiver of a member read is imported from, as the reading
/// file's `record` binds it: `json` for `json.load` under `import json` or
/// `import json as json`, `flask` for it under `from flask import json`.
///
/// Answers only for a bare name the adapter classed as a module-level
/// binding with imports and no declaration ([`ReceiverKind::Namespace`]), and
/// only when a receiver read of that name on the same line resolved to an
/// import binding with one module on record. That is a name its module scope
/// binds by one import statement and nothing else, read where no nearer
/// scope binds it.
pub fn receiver_import(record: &Value, receiver: &str, line: u32) -> Option<String> {
    if !is_identifier(receiver) {
        return None;
    }
    record
        .get("values")?
        .get(receiver)?
        .as_array()?
        .iter()
        .filter_map(|held| serde_json::from_value::<ValueOccurrence>(held.clone()).ok())
        .find(|held| {
            held.line == line
                && held.binding == OccurrenceBinding::Import
                && held.form == Some(MentionForm::Receiver)
                && held.module.is_some()
        })
        .and_then(|held| held.module)
}

/// The receiver and module of a member read whose receiver its file imports
/// from outside the repository, or `None` when either fact is not on record.
pub(super) fn outside_receiver(
    roots: &ImportRoots,
    importer: &str,
    record: &Value,
    occurrence: &ValueOccurrence,
) -> Option<(String, String)> {
    if occurrence.binding != OccurrenceBinding::Member
        || occurrence.receiver_kind != Some(ReceiverKind::Namespace)
    {
        return None;
    }
    let receiver = occurrence.receiver.as_deref()?;
    let module = receiver_import(record, receiver, occurrence.line)?;
    roots
        .outside(importer, &module)
        .then(|| (receiver.to_string(), module))
}

/// What a receiver read in a file is imported from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiverImport {
    /// The module its import statement names, as written.
    pub module: String,
    /// Whether no file of the repository can supply that module.
    pub outside_repository: bool,
}

/// One name an import statement of a file binds, as the file's record holds
/// it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ImportedName {
    /// The name the file reads it by.
    pub name: String,
    /// The module the import statement names, as written.
    pub module: String,
    /// The name the statement imports, when the file binds it under another.
    pub imported: Option<String>,
    /// Whether no file of the repository can supply the module.
    pub outside_repository: bool,
}

/// Every name `record` binds by an import statement, with the module it
/// names: each `from m import f`, and each `import m` whose name the file
/// reads where its module scope binds it by that one statement. An `import m`
/// the file never reads leaves no record and is not among them.
fn imported_names(record: &Value) -> Vec<(String, String, Option<String>)> {
    let mut names: Vec<(String, String, Option<String>)> = Vec::new();
    let Some(values) = record.get("values").and_then(Value::as_object) else {
        return names;
    };
    for (name, held) in values {
        for occurrence in held.as_array().into_iter().flatten() {
            let Ok(occurrence) = serde_json::from_value::<ValueOccurrence>(occurrence.clone())
            else {
                continue;
            };
            if occurrence.binding != OccurrenceBinding::Import {
                continue;
            }
            let Some(module) = occurrence.module else {
                continue;
            };
            // `from m import f as g` is recorded under `f` with `g` as its
            // alias; a read of `g` is recorded under `g` with `f` as what the
            // statement imports. Both name the same binding.
            if occurrence.aliases.is_empty() {
                names.push((name.clone(), module, occurrence.imported));
            } else {
                for alias in occurrence.aliases {
                    names.push((alias, module.clone(), Some(name.clone())));
                }
            }
        }
    }
    names.sort();
    names.dedup();
    names
}

/// The imports behind the receivers of a graph's call sites, read on demand:
/// each file's record the first time a site in it asks, and the tree once,
/// the first time a record binds a receiver by an import. A reading whose
/// sites are made on no imported name reads no tree at all.
pub struct ReceiverImports<'g> {
    graph: &'g dyn EscapeCensusGraph,
    /// The tree's Python files by path, with the import roots they make, or
    /// `None` inside when the tree cannot be read.
    tree: Option<Option<(HashMap<String, Hash256>, ImportRoots)>>,
    /// Each file's record with the content identity of the bytes it was
    /// parsed from, as the entity carrying it says.
    records: HashMap<String, Option<(Value, Option<Hash256>)>>,
}

impl<'g> ReceiverImports<'g> {
    pub fn new(graph: &'g dyn EscapeCensusGraph) -> Self {
        Self {
            graph,
            tree: None,
            records: HashMap::new(),
        }
    }

    /// The record an entity of the Python file at `path` carries, in a
    /// version this build reads, not yet checked against the tree.
    fn held(&mut self, path: &FilePathId) -> Option<&(Value, Option<Hash256>)> {
        if !is_python_path(&path.0) {
            return None;
        }
        let graph = self.graph;
        self.records
            .entry(path.0.clone())
            .or_insert_with(|| {
                graph.entities_in(path)?.iter().find_map(|entity| {
                    let value = entity.metadata.extra.get(VALUE_ESCAPE_RECORD_KEY)?;
                    value
                        .get("v")
                        .and_then(Value::as_u64)
                        .is_some_and(readable_record_version)
                        .then(|| (value.clone(), blob_of(entity)))
                })
            })
            .as_ref()
    }

    /// The tree's import roots, when the record held for `path` was parsed
    /// from the bytes the tree holds there.
    fn roots_for(&mut self, path: &FilePathId) -> Option<&ImportRoots> {
        let parsed_from = self.records.get(&path.0)?.as_ref()?.1?;
        let graph = self.graph;
        let (blobs, roots) = self
            .tree
            .get_or_insert_with(|| {
                let files = graph.census_files(&[LanguageId::Python])?;
                let roots = ImportRoots::of(files.iter().map(|file| file.path.0.as_str()));
                let blobs = files
                    .into_iter()
                    .map(|file| (file.path.0, file.blob))
                    .collect();
                Some((blobs, roots))
            })
            .as_ref()?;
        (blobs.get(&path.0) == Some(&parsed_from)).then_some(roots)
    }

    /// What `receiver`, read as a receiver on the 0-based `line` of `path`,
    /// is imported from, or `None` when the file's record does not bind it by
    /// one import statement (see [`receiver_import`]) or is not the record of
    /// the bytes the tree holds.
    pub fn of(&mut self, path: &FilePathId, receiver: &str, line: u32) -> Option<ReceiverImport> {
        let module = receiver_import(&self.held(path)?.0, receiver, line)?;
        let roots = self.roots_for(path)?;
        Some(ReceiverImport {
            outside_repository: roots.outside(&path.0, &module),
            module,
        })
    }

    /// Every name the file at `path` binds by an import statement its record
    /// holds, by name, or `None` when the file is not one this reads or its
    /// record cannot be read. See [`ImportedName`] for what a record holds.
    pub fn bound_in(&mut self, path: &FilePathId) -> Option<Vec<ImportedName>> {
        let names = imported_names(&self.held(path)?.0);
        let roots = self.roots_for(path)?;
        Some(
            names
                .into_iter()
                .map(|(name, module, imported)| ImportedName {
                    outside_repository: roots.outside(&path.0, &module),
                    name,
                    module,
                    imported,
                })
                .collect(),
        )
    }
}
