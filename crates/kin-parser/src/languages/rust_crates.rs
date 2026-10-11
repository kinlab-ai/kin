// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Which Rust crate a source is the root of, read from the Cargo manifests of
//! the tree it is indexed from.
//!
//! Rust code names a crate by its crate name, `use example_oauth::...` for a
//! package `example-oauth`, so a crate root's module takes that name and a row
//! reads `example_oauth :: main` rather than the directory the crate sits in.
//! Cargo decides both the name and which sources are roots:
//!
//! - a library is named by `[lib] name`, or else by its package's name, and
//!   its root is `[lib] path`, or else `src/lib.rs`;
//! - a binary, example, test or bench is named by its target's name, and its
//!   root is the target's `path`, or else the one Cargo infers: `src/main.rs`
//!   for the binary named after the package, `src/bin/<name>.rs` or
//!   `src/bin/<name>/main.rs`, and the same shapes under `examples`, `tests`
//!   and `benches`. Cargo infers a kind's targets unless its `auto` flag says
//!   not to, and a 2015-edition package that writes out any target of a kind
//!   infers no more of that kind.
//!
//! Every name is read with `-` as `_`, the way rustc spells a crate.
//!
//! A source of a package that is no target's root is a plain module, named by
//! its stem, or by its directory for a `mod.rs`, so `src/outer/main.rs` is
//! `main`. Only a source no package manifest governs falls back to its path
//! ([`conventional_root_name`]).
//!
//! A manifest that does not parse as TOML, as one half typed in an editor
//! does, is read for the few keys a crate's naming needs, so saving it midway
//! does not rename every root beneath it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use kin_model::FilePathId;

use crate::project_manifest::{ProjectFiles, UnitContext, UnreadableManifest};

/// The manifest a Cargo package or workspace declares itself in.
const MANIFEST: &str = "Cargo.toml";

/// What the Cargo manifests of a source's tree make of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CrateRole {
    /// The root of a crate, carrying the crate name Rust code writes for it.
    Root(String),
    /// A source of a package that is no target's root: a plain module.
    Module,
    /// No package manifest governs it, so only its path can say.
    Ungoverned,
}

/// A Cargo target kind beside the library, with the directory Cargo infers
/// its roots in and the flag that turns that inference off.
struct TargetKind {
    table: &'static str,
    directory: &'static str,
    automatic: &'static str,
}

const TARGET_KINDS: [TargetKind; 4] = [
    TargetKind {
        table: "bin",
        directory: "src/bin",
        automatic: "autobins",
    },
    TargetKind {
        table: "example",
        directory: "examples",
        automatic: "autoexamples",
    },
    TargetKind {
        table: "test",
        directory: "tests",
        automatic: "autotests",
    },
    TargetKind {
        table: "bench",
        directory: "benches",
        automatic: "autobenches",
    },
];

/// The name the source at `file_id` takes under the Cargo manifests `project`
/// holds: a crate root's crate name, a plain module's own name, and for a
/// source no package governs the name its path gives
/// ([`crate::adapter::file_module_surface_name`]). Every Rust source is named
/// here, so a reader can tell whether a held module still carries the name a
/// parse would give it without parsing.
pub(crate) fn unit_context(
    file_id: &FilePathId,
    project: &dyn ProjectFiles,
) -> Result<UnitContext, UnreadableManifest> {
    let path = file_id.0.as_str();
    let module_name = match crate_role(path, project)? {
        CrateRole::Root(name) => Some(name),
        CrateRole::Module => plain_module_name(path),
        CrateRole::Ungoverned => crate::adapter::file_module_surface_name(None, file_id),
    };
    Ok(UnitContext { module_name })
}

/// What the Cargo manifests `project` holds make of the source at `path`.
///
/// The package that owns a source is the one whose manifest is nearest above
/// it. A manifest that declares only a workspace owns no source, so the
/// search climbs past it. A manifest the tree holds that cannot be read
/// refuses the answer rather than reading as absent.
pub fn crate_role(path: &str, project: &dyn ProjectFiles) -> Result<CrateRole, UnreadableManifest> {
    if !path.ends_with(".rs") {
        return Ok(CrateRole::Ungoverned);
    }
    let mut directory = parent(path);
    loop {
        if let Some(text) = project.manifest(&manifest_in(directory))? {
            let manifest = parsed(&text);
            if let Some(package) = manifest.get("package").and_then(toml::Value::as_table) {
                return Ok(
                    match package_root_name(path, directory, &manifest, package, project)? {
                        Some(name) => CrateRole::Root(name),
                        None => CrateRole::Module,
                    },
                );
            }
        }
        if directory.is_empty() {
            return Ok(CrateRole::Ungoverned);
        }
        directory = parent(directory);
    }
}

/// The crate name a source takes from its path alone, where the path lays out
/// a crate root the way Cargo infers one, or `None` for any other path.
///
/// `<crate>/src/lib.rs` and `<crate>/src/main.rs` are named by `<crate>`, so
/// `axum-macros/src/lib.rs` is `axum_macros`; `src/bin/<name>/main.rs` and
/// `<name>/main.rs` under `examples`, `tests` or `benches` are named by
/// `<name>`. A root with no directory to name it, such as the repository's own
/// `src/lib.rs`, is `crate`, the name Rust code inside it writes. Any other
/// `lib.rs` or `main.rs` is not laid out as a root, so it answers `None` and is
/// named by its stem, as `src/outer/main.rs` is `main`.
pub fn conventional_root_name(path: &str) -> Option<String> {
    let segments: Vec<&str> = path
        .split('/')
        .filter(|segment| !segment.is_empty() && *segment != ".")
        .collect();
    let (file, directories) = segments.split_last()?;
    if !matches!(*file, "lib.rs" | "main.rs") {
        return None;
    }
    match directories {
        [] | ["src"] => Some("crate".to_string()),
        [.., owner, "src"] => Some(crate_name(owner)),
        [.., "src", "bin", target] if *file == "main.rs" => Some(crate_name(target)),
        [.., "examples" | "tests" | "benches", target] if *file == "main.rs" => {
            Some(crate_name(target))
        }
        _ => None,
    }
}

/// A plain module's own name: its directory's for a `mod.rs`, otherwise its
/// stem.
fn plain_module_name(path: &str) -> Option<String> {
    let stem = file_stem(path);
    let name = if stem == "mod" {
        parent(path).rsplit('/').next().unwrap_or_default()
    } else {
        stem
    };
    (!name.is_empty()).then(|| name.to_string())
}

/// The crate name of the target of `manifest` whose root is `path`, or `None`
/// when `path` roots none of its targets. `base` is the manifest's directory.
fn package_root_name(
    path: &str,
    base: &str,
    manifest: &toml::Table,
    package: &toml::Table,
    project: &dyn ProjectFiles,
) -> Result<Option<String>, UnreadableManifest> {
    let Some(package_name) = package.get("name").and_then(toml::Value::as_str) else {
        return Ok(None);
    };
    let at = |relative: &str| join(base, relative).is_some_and(|root| root == path);

    match manifest.get("lib") {
        Some(lib) => {
            let Some(lib) = lib.as_table() else {
                return Ok(None);
            };
            let root = lib
                .get("path")
                .and_then(toml::Value::as_str)
                .unwrap_or("src/lib.rs");
            if at(root) {
                let name = lib
                    .get("name")
                    .and_then(toml::Value::as_str)
                    .unwrap_or(package_name);
                return Ok(valid_crate_name(name));
            }
        }
        None if flag(package, "autolib", true) && at("src/lib.rs") => {
            return Ok(valid_crate_name(package_name));
        }
        None => {}
    }

    let edition_2015 = edition(package, base, project)?.as_deref() == Some("2015");
    for kind in &TARGET_KINDS {
        let written = manifest.get(kind.table);
        let targets: Vec<&toml::Table> = written
            .and_then(toml::Value::as_array)
            .map(|targets| targets.iter().filter_map(toml::Value::as_table).collect())
            .unwrap_or_default();
        let mut named = Vec::with_capacity(targets.len());
        for target in targets {
            let Some(name) = target.get("name").and_then(toml::Value::as_str) else {
                continue;
            };
            named.push(name);
            let rooted = match target.get("path").and_then(toml::Value::as_str) {
                Some(root) => at(root),
                None => {
                    at(&format!("{}/{name}.rs", kind.directory))
                        || at(&format!("{}/{name}/main.rs", kind.directory))
                        || (kind.table == "bin" && name == package_name && at("src/main.rs"))
                }
            };
            if rooted {
                return Ok(valid_crate_name(name));
            }
        }
        // Cargo infers a kind's targets unless its flag says not to, and a
        // 2015-edition package that writes out targets of a kind infers no
        // more of that kind.
        let inferred_by_default = written.is_none() || !edition_2015;
        if !flag(package, kind.automatic, inferred_by_default) {
            continue;
        }
        let relative = if base.is_empty() {
            Some(path)
        } else {
            path.strip_prefix(base)
                .and_then(|rest| rest.strip_prefix('/'))
        };
        let Some(relative) = relative else {
            continue;
        };
        let discovered = if kind.table == "bin" && relative == "src/main.rs" {
            Some(package_name)
        } else {
            relative
                .strip_prefix(kind.directory)
                .and_then(|rest| rest.strip_prefix('/'))
                .and_then(|rest| match rest.split_once('/') {
                    None => rest.strip_suffix(".rs"),
                    Some((name, "main.rs")) => Some(name),
                    Some(_) => None,
                })
        };
        if let Some(name) = discovered.filter(|name| !named.contains(name)) {
            return Ok(valid_crate_name(name));
        }
    }
    Ok(None)
}

/// The edition a package is written in: the one it states, the one its
/// workspace states for it, or 2015 when it states none.
fn edition(
    package: &toml::Table,
    base: &str,
    project: &dyn ProjectFiles,
) -> Result<Option<String>, UnreadableManifest> {
    Ok(match package.get("edition") {
        None => Some("2015".to_string()),
        Some(toml::Value::String(edition)) => Some(edition.clone()),
        Some(toml::Value::Table(inherited))
            if inherited.get("workspace").and_then(toml::Value::as_bool) == Some(true) =>
        {
            let mut directory = base;
            loop {
                if let Some(text) = project.manifest(&manifest_in(directory))? {
                    if let Some(workspace) = parsed(&text).get("workspace") {
                        break workspace
                            .get("package")
                            .and_then(|package| package.get("edition"))
                            .and_then(toml::Value::as_str)
                            .map(str::to_string);
                    }
                }
                if directory.is_empty() {
                    break None;
                }
                directory = parent(directory);
            }
        }
        Some(_) => None,
    })
}

/// A boolean the package table sets, or `default` when it sets none.
fn flag(package: &toml::Table, key: &str, default: bool) -> bool {
    package
        .get(key)
        .and_then(toml::Value::as_bool)
        .unwrap_or(default)
}

/// A target name as rustc spells its crate, or `None` for one no crate can
/// carry.
fn valid_crate_name(name: &str) -> Option<String> {
    let name = crate_name(name);
    (!name.is_empty()
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_'))
    .then_some(name)
}

fn crate_name(name: &str) -> String {
    name.replace('-', "_")
}

/// A manifest read as a TOML table, kept by its text: the same bytes always
/// read the same way, and a tree of many sources reads the few manifests above
/// them once per source. Text that does not parse is read by
/// [`tolerant_table`].
fn parsed(text: &Arc<str>) -> Arc<toml::Table> {
    static PARSED: OnceLock<Mutex<HashMap<Arc<str>, Arc<toml::Table>>>> = OnceLock::new();
    const KEPT: usize = 256;
    let cache = PARSED.get_or_init(Default::default);
    if let Some(kept) = cache.lock().ok().and_then(|kept| kept.get(text).cloned()) {
        return kept;
    }
    let table =
        Arc::new(toml::from_str::<toml::Table>(text).unwrap_or_else(|_| tolerant_table(text)));
    if let Ok(mut kept) = cache.lock() {
        if kept.len() >= KEPT {
            kept.clear();
        }
        kept.insert(text.clone(), table.clone());
    }
    table
}

/// The keys a crate's naming reads, from a manifest that does not parse as a
/// whole: each `[package]`, `[lib]`, `[workspace]`, `[workspace.package]` and
/// `[[bin]]`, `[[example]]`, `[[test]]` or `[[bench]]` header, and under it
/// each line that sets a key to a quoted string or a boolean, with
/// `edition.workspace = true` read as the package inheriting its edition. A
/// line it cannot read is skipped, so a half-typed dependency table elsewhere
/// leaves the crate named as it was.
fn tolerant_table(text: &str) -> toml::Table {
    let mut table = toml::Table::new();
    // Where the lines under the current header go: a table path, and whether
    // the header opened a new element of an array of tables.
    let mut current: Option<(Vec<&str>, bool)> = None;
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or_default().trim();
        if let Some(header) = line
            .strip_prefix("[[")
            .and_then(|rest| rest.strip_suffix("]]"))
        {
            let header = header.trim();
            current = matches!(header, "bin" | "example" | "test" | "bench").then(|| {
                let targets = table
                    .entry(header.to_string())
                    .or_insert_with(|| toml::Value::Array(Vec::new()));
                if let Some(targets) = targets.as_array_mut() {
                    targets.push(toml::Value::Table(toml::Table::new()));
                }
                (vec![header], true)
            });
            continue;
        }
        if let Some(header) = line
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
        {
            current = match header.trim() {
                "package" => Some((vec!["package"], false)),
                "lib" => Some((vec!["lib"], false)),
                "workspace" => Some((vec!["workspace"], false)),
                "workspace.package" => Some((vec!["workspace", "package"], false)),
                _ => None,
            };
            if let Some((path, _)) = &current {
                tolerant_slot(&mut table, path, false);
            }
            continue;
        }
        let Some((path, element)) = &current else {
            continue;
        };
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        let value = if value == "true" || value == "false" {
            toml::Value::Boolean(value == "true")
        } else if let Some(text) = value
            .strip_prefix('"')
            .and_then(|rest| rest.strip_suffix('"'))
            .or_else(|| {
                value
                    .strip_prefix('\'')
                    .and_then(|rest| rest.strip_suffix('\''))
            })
        {
            toml::Value::String(text.to_string())
        } else {
            continue;
        };
        let Some(slot) = tolerant_slot(&mut table, path, *element) else {
            continue;
        };
        match key {
            "edition.workspace" => {
                if value.as_bool() == Some(true) {
                    let mut inherited = toml::Table::new();
                    inherited.insert("workspace".to_string(), toml::Value::Boolean(true));
                    slot.insert("edition".to_string(), toml::Value::Table(inherited));
                }
            }
            key if !key.contains(['.', ' ', '"', '\'']) => {
                slot.insert(key.to_string(), value);
            }
            _ => {}
        }
    }
    table
}

/// The table the tolerant reader writes `path` into, made if missing, or the
/// last element of the array of tables at `path` when `element`.
fn tolerant_slot<'t>(
    table: &'t mut toml::Table,
    path: &[&str],
    element: bool,
) -> Option<&'t mut toml::Table> {
    let (last, outer) = path.split_last()?;
    let mut at = table;
    for segment in outer {
        at = at
            .entry(segment.to_string())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()))
            .as_table_mut()?;
    }
    if element {
        return at
            .get_mut(*last)?
            .as_array_mut()?
            .last_mut()?
            .as_table_mut();
    }
    at.entry(last.to_string())
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
}

/// The repository-relative path of the manifest in `directory`.
fn manifest_in(directory: &str) -> String {
    if directory.is_empty() {
        MANIFEST.to_string()
    } else {
        format!("{directory}/{MANIFEST}")
    }
}

fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(parent, _)| parent)
}

fn file_stem(path: &str) -> &str {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.rsplit_once('.').map_or(name, |(stem, _)| stem)
}

/// `relative` resolved against the manifest directory `base`, as a
/// repository-relative path, or `None` for a path Cargo would read outside the
/// repository.
fn join(base: &str, relative: &str) -> Option<String> {
    if relative.starts_with('/') {
        return None;
    }
    let mut parts: Vec<&str> = base.split('/').filter(|part| !part.is_empty()).collect();
    for part in relative.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            part => parts.push(part),
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tree's manifests, by path, and the paths whose manifest cannot be
    /// read.
    struct Manifests(Vec<(&'static str, &'static str)>, Vec<&'static str>);

    impl ProjectFiles for Manifests {
        fn manifest(&self, path: &str) -> Result<Option<Arc<str>>, UnreadableManifest> {
            if self.1.contains(&path) {
                return Err(UnreadableManifest {
                    path: path.to_string(),
                    reason: "its bytes are missing".to_string(),
                });
            }
            Ok(self
                .0
                .iter()
                .find(|(at, _)| *at == path)
                .map(|(_, text)| Arc::from(*text)))
        }
    }

    fn named(path: &str, manifests: &[(&'static str, &'static str)]) -> Option<String> {
        unit_context(
            &FilePathId::new(path),
            &Manifests(manifests.to_vec(), Vec::new()),
        )
        .unwrap()
        .module_name
    }

    #[test]
    fn a_library_root_takes_its_package_name_read_the_way_rust_writes_it() {
        let manifests = [(
            "axum-macros/Cargo.toml",
            "[package]\nname = \"axum-macros\"\nedition = \"2021\"\n",
        )];
        assert_eq!(
            named("axum-macros/src/lib.rs", &manifests).as_deref(),
            Some("axum_macros")
        );
    }

    #[test]
    fn a_binary_root_takes_its_package_name_whatever_its_directory_is_called() {
        let manifests = [(
            "examples/oauth/Cargo.toml",
            "[package]\nname = \"example-oauth\"\nversion = \"0.1.0\"\nedition = \"2021\"\npublish = false\n",
        )];
        assert_eq!(
            named("examples/oauth/src/main.rs", &manifests).as_deref(),
            Some("example_oauth")
        );
    }

    #[test]
    fn a_single_crate_repository_names_its_root_by_its_package() {
        let manifests = [(
            "Cargo.toml",
            "[package]\nname = \"ripgrep-lite\"\nedition = \"2021\"\n",
        )];
        assert_eq!(
            named("src/lib.rs", &manifests).as_deref(),
            Some("ripgrep_lite")
        );
        assert_eq!(
            named("src/main.rs", &manifests).as_deref(),
            Some("ripgrep_lite")
        );
    }

    #[test]
    fn a_workspace_crate_takes_its_package_name_not_its_directory() {
        let manifests = [
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\"crates/*\"]\n\n[workspace.package]\nedition = \"2021\"\n",
            ),
            (
                "crates/db/Cargo.toml",
                "[package]\nname = \"kin-db\"\nedition.workspace = true\n",
            ),
        ];
        assert_eq!(
            named("crates/db/src/lib.rs", &manifests).as_deref(),
            Some("kin_db")
        );
        // Below the root, a source is a plain module named by its stem, or by
        // its directory for a `mod.rs`.
        assert_eq!(
            named("crates/db/src/store.rs", &manifests).as_deref(),
            Some("store")
        );
        assert_eq!(
            named("crates/db/src/engine/mod.rs", &manifests).as_deref(),
            Some("engine")
        );
    }

    #[test]
    fn a_source_no_manifest_governs_keeps_the_path_rule() {
        assert_eq!(
            named("examples/oauth/src/main.rs", &[]).as_deref(),
            Some("oauth")
        );
        assert_eq!(named("src/store.rs", &[]).as_deref(), Some("store"));
        // A workspace manifest owns no source, so its members' sources
        // without a manifest of their own are not governed by it.
        let manifests = [("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n")];
        assert_eq!(
            named("crates/db/src/lib.rs", &manifests).as_deref(),
            Some("db")
        );
    }

    #[test]
    fn written_targets_name_their_roots_and_take_them_from_inference() {
        let manifests = [(
            "Cargo.toml",
            "[package]\nname = \"ripgrep\"\nedition = \"2021\"\n\n[lib]\nname = \"grep_lib\"\npath = \"crates/lib/mod.rs\"\n\n[[bin]]\nname = \"rg\"\npath = \"crates/core/main.rs\"\n\n[[example]]\nname = \"demo-one\"\n",
        )];
        assert_eq!(
            named("crates/core/main.rs", &manifests).as_deref(),
            Some("rg")
        );
        assert_eq!(
            named("crates/lib/mod.rs", &manifests).as_deref(),
            Some("grep_lib")
        );
        assert_eq!(
            named("examples/demo-one.rs", &manifests).as_deref(),
            Some("demo_one")
        );
        // `src/lib.rs` is not the library's root once `[lib] path` moves it.
        assert_eq!(named("src/lib.rs", &manifests).as_deref(), Some("lib"));
        // An inferred binary still roots its own source.
        assert_eq!(
            named("src/bin/tool/main.rs", &manifests).as_deref(),
            Some("tool")
        );
        assert_eq!(named("src/bin/fmt.rs", &manifests).as_deref(), Some("fmt"));
    }

    #[test]
    fn a_main_or_lib_that_roots_no_target_is_a_plain_module() {
        let manifests = [(
            "Cargo.toml",
            "[package]\nname = \"outer\"\nedition = \"2021\"\n",
        )];
        assert_eq!(
            named("src/outer/main.rs", &manifests).as_deref(),
            Some("main")
        );
        assert_eq!(
            named("src/nested/lib.rs", &manifests).as_deref(),
            Some("lib")
        );
        assert_eq!(
            named("crates/core/main.rs", &manifests).as_deref(),
            Some("main")
        );
    }

    /// Cargo decides 2015-edition inference kind by kind: writing out `[lib]`
    /// leaves binaries inferred, and writing out a `[[bin]]` stops the other
    /// binaries being inferred while examples still are.
    #[test]
    fn edition_2015_turns_off_inference_only_for_a_kind_it_writes_out() {
        let lib_only = [(
            "Cargo.toml",
            "[package]\nname = \"old\"\n\n[lib]\nname = \"old_lib\"\n",
        )];
        assert_eq!(named("src/lib.rs", &lib_only).as_deref(), Some("old_lib"));
        assert_eq!(named("src/main.rs", &lib_only).as_deref(), Some("old"));
        assert_eq!(named("src/bin/tool.rs", &lib_only).as_deref(), Some("tool"));

        let one_bin = [(
            "Cargo.toml",
            "[package]\nname = \"old\"\n\n[[bin]]\nname = \"cli\"\npath = \"src/cli.rs\"\n",
        )];
        assert_eq!(named("src/cli.rs", &one_bin).as_deref(), Some("cli"));
        assert_eq!(named("src/main.rs", &one_bin).as_deref(), Some("main"));
        assert_eq!(
            named("src/bin/tool/main.rs", &one_bin).as_deref(),
            Some("main")
        );
        assert_eq!(named("examples/demo.rs", &one_bin).as_deref(), Some("demo"));
        assert_eq!(
            crate_role("examples/demo.rs", &Manifests(one_bin.to_vec(), Vec::new())),
            Ok(CrateRole::Root("demo".to_string()))
        );

        // Asked for, the inference stays on.
        let asked = [(
            "Cargo.toml",
            "[package]\nname = \"old\"\nautobins = true\n\n[[bin]]\nname = \"cli\"\npath = \"src/cli.rs\"\n",
        )];
        assert_eq!(
            crate_role("src/main.rs", &Manifests(asked.to_vec(), Vec::new())),
            Ok(CrateRole::Root("old".to_string()))
        );
    }

    #[test]
    fn the_path_rule_names_only_the_roots_cargo_lays_out() {
        for (path, expected) in [
            ("axum-macros/src/lib.rs", Some("axum_macros")),
            ("examples/oauth/src/main.rs", Some("oauth")),
            ("crates/kin-model/src/lib.rs", Some("kin_model")),
            ("src/bin/tool/main.rs", Some("tool")),
            ("examples/hello/main.rs", Some("hello")),
            ("src/lib.rs", Some("crate")),
            ("lib.rs", Some("crate")),
            ("src/outer/main.rs", None),
            ("crates/core/main.rs", None),
            ("src/store.rs", None),
        ] {
            assert_eq!(conventional_root_name(path).as_deref(), expected, "{path}");
        }
    }

    /// A manifest half typed in an editor still names its crate: the keys a
    /// crate's naming reads are read line by line.
    #[test]
    fn a_manifest_that_does_not_parse_is_read_for_its_crate_names() {
        let half_typed = [(
            "Cargo.toml",
            "[package]\nname = \"kin-db\" # the store\nedition.workspace = true\n\n[dependencies]\nserde = { version = \"1\", features = [\"der\n\n[[bin]]\nname = 'kin-db-tool'\npath = 'src/tool.rs'\n",
        )];
        assert!(toml::from_str::<toml::Table>(half_typed[0].1).is_err());
        assert_eq!(named("src/lib.rs", &half_typed).as_deref(), Some("kin_db"));
        assert_eq!(
            named("src/tool.rs", &half_typed).as_deref(),
            Some("kin_db_tool")
        );
        assert_eq!(named("src/main.rs", &half_typed).as_deref(), Some("kin_db"));
        // Text with no table the naming reads governs nothing.
        let unreadable_text = [("Cargo.toml", "[package\nname = ")];
        assert_eq!(
            crate_role(
                "src/lib.rs",
                &Manifests(unreadable_text.to_vec(), Vec::new())
            ),
            Ok(CrateRole::Ungoverned)
        );
    }

    /// A manifest the tree holds that cannot be read refuses the naming of
    /// the sources beneath it, rather than naming them as if it were absent.
    #[test]
    fn an_unreadable_manifest_refuses_the_names_beneath_it() {
        let project = Manifests(Vec::new(), vec!["crates/db/Cargo.toml"]);
        let refused = unit_context(&FilePathId::new("crates/db/src/lib.rs"), &project);
        assert_eq!(
            refused.unwrap_err().path,
            "crates/db/Cargo.toml".to_string()
        );
        // A source outside it is not refused.
        assert_eq!(
            unit_context(&FilePathId::new("crates/app/src/lib.rs"), &project)
                .unwrap()
                .module_name
                .as_deref(),
            Some("app")
        );
    }
}
