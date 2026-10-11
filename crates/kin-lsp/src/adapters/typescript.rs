// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! TypeScript/JavaScript LSP adapter (typescript-language-server or vtsls).
//!
//! A monorepo often links one workspace package to another through the
//! directory the other's build writes (`"drizzle-orm": "workspace:./drizzle-orm/dist"`).
//! Until that build runs the directory does not exist, the import resolves to
//! nothing, and no call through it has a definition: in drizzle-orm's
//! integration tests, every call into the packages they test. Running the
//! build is running repository code, and writing a tsconfig into the
//! repository is writing into it, so this adapter does neither. It loads a
//! small tsserver plugin, kept outside the repository, that resolves such an
//! import to the package's source, and resolves the imports inside that source
//! with the package's own tsconfig, as a project reference would. An import
//! tsserver resolves on its own is left as it is.
//!
//! tsserver also gets a memory ceiling sized to the machine, since a project
//! that now includes its workspace packages' source is larger than one that
//! did not.
//!
//! Dependencies follow the contract: the repository's own `node_modules` when
//! every importer's direct dependencies are installed at the locked
//! versions, else Kin's analysis environment, the lock's packages fetched,
//! verified and laid out outside the repository (see
//! [`crate::analysis_env::js`]), else none, reported missing. tsserver reaches
//! Kin's layout through the same plugin: an import it cannot resolve from the
//! repository is resolved as if from the importing file's mirror in the
//! layout, where that importer's locked dependencies are, and the layout's
//! `@types` directories join the project's type roots. tsserver itself is the
//! TypeScript the lock pins, from whichever environment serves.
//!
//! The map from each workspace package to its source entry is data in the
//! [`ProjectModel`], computed here from the packages' manifests; the plugin
//! reads it and computes nothing of its own.
//!
//! Every request goes to the one semantic tsserver. By default the server also
//! runs a syntax-only tsserver that answers while a project loads, from the
//! open file alone: a call into another package is answered with its import
//! line. A sweep that opens one file after another switches projects and pays
//! a reload each time, so those shallow answers were most of what it got in a
//! large project. Without the syntax server a request waits for the project
//! instead.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use kin_model::LanguageId;
use serde::Serialize;

use super::contract::{
    DependencySource, Environment, EnvironmentBasis, ProjectModel, ProjectPackage, Provider,
    ProvisionReport, Toolchain,
};
use super::repo_scan;
use super::{LspAdapter, ServerLaunch};
use crate::analysis_env::{self, js as analysis};

pub struct TypeScriptAdapter;

/// The plugin's name, which is also its directory under `node_modules`.
pub const PLUGIN_NAME: &str = "kin-workspace-sources";

/// The plugin, as tsserver loads it.
const PLUGIN_SOURCE: &str = include_str!("tsserver_workspace_sources.js");

/// The environment variable that names the workspace file to the plugin.
pub const WORKSPACE_ENV: &str = "KIN_TSSERVER_WORKSPACE";

/// One package a JavaScript workspace declares.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorkspacePackage {
    /// The name other packages import it by.
    pub name: String,
    /// Its directory, which holds its `package.json`.
    pub dir: PathBuf,
    /// Its own `tsconfig.json`, when it has one.
    pub tsconfig: Option<PathBuf>,
}

/// The package globs a workspace declares, from `pnpm-workspace.yaml`, the
/// root `package.json`'s `workspaces`, and `lerna.json`.
pub fn workspace_patterns(root: &Path) -> Vec<String> {
    let mut patterns = Vec::new();
    if let Ok(text) = std::fs::read_to_string(root.join("pnpm-workspace.yaml")) {
        patterns.extend(pnpm_packages(&text));
    }
    let json = |file: &str| -> Option<serde_json::Value> {
        serde_json::from_str(&std::fs::read_to_string(root.join(file)).ok()?).ok()
    };
    if let Some(package) = json("package.json") {
        let workspaces = package.get("workspaces");
        let list = workspaces
            .and_then(|workspaces| workspaces.get("packages"))
            .or(workspaces);
        patterns.extend(strings(list));
    }
    if let Some(lerna) = json("lerna.json") {
        patterns.extend(strings(lerna.get("packages")));
    }
    patterns
}

fn strings(value: Option<&serde_json::Value>) -> Vec<String> {
    value
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// The `packages` list of a `pnpm-workspace.yaml`, in block or flow style.
/// Only that key is read, so a small reader serves rather than a YAML parser.
fn pnpm_packages(text: &str) -> Vec<String> {
    let unquote = |item: &str| {
        let item = item.split(" #").next().unwrap_or("").trim();
        item.trim_matches(|c| c == '\'' || c == '"').to_string()
    };
    let mut packages = Vec::new();
    let mut inside = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let top_level = !line.starts_with([' ', '\t', '-']);
        if top_level {
            inside = false;
            if let Some(rest) = trimmed.strip_prefix("packages:") {
                let rest = rest.trim();
                if let Some(flow) = rest.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
                    packages.extend(flow.split(',').map(unquote).filter(|p| !p.is_empty()));
                } else {
                    inside = true;
                }
            }
            continue;
        }
        if inside {
            if let Some(item) = trimmed.strip_prefix('-') {
                let item = unquote(item);
                if !item.is_empty() {
                    packages.push(item);
                }
            }
        }
    }
    packages
}

/// Whether a relative directory matches one workspace glob: `*` matches within
/// one path segment and `**` any number of segments.
fn glob_matches(pattern: &str, relative: &str) -> bool {
    fn segment(pattern: &[u8], name: &[u8]) -> bool {
        match (pattern.first(), name.first()) {
            (None, None) => true,
            (Some(b'*'), _) => {
                segment(&pattern[1..], name) || (!name.is_empty() && segment(pattern, &name[1..]))
            }
            (Some(b'?'), Some(_)) => segment(&pattern[1..], &name[1..]),
            (Some(p), Some(n)) if p == n => segment(&pattern[1..], &name[1..]),
            _ => false,
        }
    }
    fn path(pattern: &[&str], parts: &[&str]) -> bool {
        match pattern.split_first() {
            None => parts.is_empty(),
            Some((&"**", rest)) => {
                path(rest, parts) || (!parts.is_empty() && path(pattern, &parts[1..]))
            }
            Some((first, rest)) => parts.split_first().is_some_and(|(part, others)| {
                segment(first.as_bytes(), part.as_bytes()) && path(rest, others)
            }),
        }
    }
    let clean = |text: &str| -> Vec<String> {
        text.trim_start_matches("./")
            .trim_end_matches('/')
            .split('/')
            .filter(|part| !part.is_empty() && *part != ".")
            .map(str::to_string)
            .collect()
    };
    let pattern = clean(pattern);
    let parts = clean(relative);
    let pattern: Vec<&str> = pattern.iter().map(String::as_str).collect();
    let parts: Vec<&str> = parts.iter().map(String::as_str).collect();
    path(&pattern, &parts)
}

/// Every package the workspace at `root` declares, with its name and its own
/// tsconfig. A repository that declares no workspace has none.
pub fn workspace_packages(root: &Path) -> Vec<WorkspacePackage> {
    let patterns = workspace_patterns(root);
    let (exclude, include): (Vec<&String>, Vec<&String>) = patterns
        .iter()
        .partition(|pattern| pattern.starts_with('!'));
    if include.is_empty() {
        return Vec::new();
    }
    let mut packages = Vec::new();
    repo_scan::walk_files(root, &|_, _| true, &mut |path, name| {
        if name != "package.json" {
            return;
        }
        let Some(dir) = path.parent() else {
            return;
        };
        let Ok(relative) = dir.strip_prefix(root) else {
            return;
        };
        let relative = relative.to_string_lossy().replace('\\', "/");
        if relative.is_empty()
            || !include
                .iter()
                .any(|pattern| glob_matches(pattern, &relative))
            || exclude
                .iter()
                .any(|pattern| glob_matches(&pattern[1..], &relative))
        {
            return;
        }
        let Some(name) = std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            .and_then(|package| package.get("name")?.as_str().map(str::to_string))
        else {
            return;
        };
        let tsconfig = dir.join("tsconfig.json");
        packages.push(WorkspacePackage {
            name,
            dir: dir.to_path_buf(),
            tsconfig: tsconfig.is_file().then_some(tsconfig),
        });
    });
    packages
}

/// The environment variable that sets tsserver's heap ceiling in MB, for an
/// operator who knows the machine's other load better than its size does.
pub const MEMORY_ENV: &str = "KIN_TSSERVER_MAX_MEMORY_MB";

/// tsserver's heap ceiling in MB: [`MEMORY_ENV`] when it is set to at least
/// 1024, and otherwise a quarter of physical memory between 3 GB and 16 GB.
///
/// Node's own default is near the bottom of that range, and a sweep's
/// reference and call-hierarchy queries on a large monorepo exceed it: on
/// drizzle-orm tsserver died at 3.2 GB under Node's default and at 6.9 GB under
/// an 8 GB ceiling, a few minutes into the sweep, and answered nothing after.
/// Under 16 GB it lasted several times longer and the sweep proved over three
/// times as many edges.
pub fn ts_server_memory_mb() -> u64 {
    std::env::var(MEMORY_ENV)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|mb| *mb >= 1024)
        .unwrap_or_else(|| memory_ceiling_mb(physical_memory_bytes()))
}

fn memory_ceiling_mb(physical: Option<u64>) -> u64 {
    physical
        .map(|bytes| bytes / 4 / (1024 * 1024))
        .unwrap_or(4096)
        .clamp(3072, 16384)
}

#[cfg(unix)]
fn physical_memory_bytes() -> Option<u64> {
    // SAFETY: sysconf reads two system constants and has no preconditions.
    let (pages, page_size) = unsafe {
        (
            libc::sysconf(libc::_SC_PHYS_PAGES),
            libc::sysconf(libc::_SC_PAGESIZE),
        )
    };
    (pages > 0 && page_size > 0).then(|| pages as u64 * page_size as u64)
}

#[cfg(not(unix))]
fn physical_memory_bytes() -> Option<u64> {
    None
}

/// The options every launch sends.
fn base_options() -> serde_json::Value {
    serde_json::json!({
        "preferences": {
            "includeInlayParameterNameHints": "none",
            "includeInlayPropertyDeclarationTypeHints": false,
        },
        "maxTsServerMemory": ts_server_memory_mb(),
        "tsserver": { "useSyntaxServer": "never" },
    })
}

/// Kin's own directory for files a language server reads: `$KIN_HOME/cache/lsp`,
/// `~/.kin/cache/lsp` without it. Never inside a repository.
pub fn kin_lsp_cache_dir() -> PathBuf {
    std::env::var_os("KIN_HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|home| !home.is_empty())
                .map(|home| PathBuf::from(home).join(".kin"))
        })
        .unwrap_or_else(|| std::env::temp_dir().join("kin"))
        .join("cache")
        .join("lsp")
}

/// A stable 64-bit FNV-1a hash, to name files by their content or root.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// Write `text` to `path` unless it already holds exactly that, through a
/// temporary file and a rename, so a reader never sees half a file.
///
/// Every write stages under a name of its own. One daemon installs from its
/// readiness probe and its sweep at once, so a name keyed by the process alone
/// let one writer rename the other's staged file away before it could.
fn write_if_changed(path: &Path, text: &str) -> std::io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT_STAGING: AtomicU64 = AtomicU64::new(0);

    if std::fs::read_to_string(path).is_ok_and(|existing| existing == text) {
        return Ok(());
    }
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("a file path with no directory"))?;
    std::fs::create_dir_all(dir)?;
    let staging = dir.join(format!(
        ".{}.{}.{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id(),
        NEXT_STAGING.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&staging, text)?;
    let renamed = std::fs::rename(&staging, path);
    if renamed.is_err() {
        let _ = std::fs::remove_file(&staging);
    }
    renamed
}

/// What the plugin is told about one repository: its workspace packages,
/// the map from each package (and subpath) to its source, and Kin's
/// dependency layout when one serves.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PluginWorkspace {
    pub packages: Vec<WorkspacePackage>,
    /// [`ProjectModel::workspace_map`], as it is.
    #[serde(rename = "workspaceMap")]
    pub workspace_map: BTreeMap<String, PathBuf>,
    /// The root of Kin's `node_modules` layout, whose mirror of each
    /// importer's directory holds that importer's locked dependencies.
    pub layout: Option<PathBuf>,
}

/// What the workspace file tells the plugin, for a proof context: the same
/// document with every path inside `root` written relative to it, so the
/// same repository describes itself the same way wherever it is checked out,
/// and any change to its packages, its map or its dependency layout still
/// reads as a different configuration. Paths outside `root`, such as Kin's
/// dependency layout, stay as they are and are normalized with the rest of
/// the launch.
fn workspace_identity(workspace: &PluginWorkspace, root: &Path) -> String {
    let relative = |path: &Path| match path.strip_prefix(root) {
        Ok(inner) => format!(
            "${{workspace}}/{}",
            inner.to_string_lossy().replace('\\', "/")
        ),
        Err(_) => path.to_string_lossy().into_owned(),
    };
    let packages: Vec<serde_json::Value> = workspace
        .packages
        .iter()
        .map(|package| {
            serde_json::json!({
                "name": package.name,
                "dir": relative(&package.dir),
                "tsconfig": package.tsconfig.as_deref().map(relative),
            })
        })
        .collect();
    let map: BTreeMap<&String, String> = workspace
        .workspace_map
        .iter()
        .map(|(name, path)| (name, relative(path)))
        .collect();
    serde_json::json!({
        "packages": packages,
        "workspaceMap": map,
        "layout": workspace.layout.as_deref().map(relative),
    })
    .to_string()
}

/// Install the plugin under `cache` and describe the workspace to it: the
/// directory tsserver probes for the plugin, and the workspace file.
pub fn install_workspace_plugin(
    cache: &Path,
    root: &Path,
    packages: &[WorkspacePackage],
) -> std::io::Result<(PathBuf, PathBuf)> {
    install_plugin(
        cache,
        root,
        &PluginWorkspace {
            packages: packages.to_vec(),
            ..PluginWorkspace::default()
        },
    )
}

/// Install the plugin under `cache` and write what it reads for `root`.
pub fn install_plugin(
    cache: &Path,
    root: &Path,
    workspace: &PluginWorkspace,
) -> std::io::Result<(PathBuf, PathBuf)> {
    let location = cache.join(format!(
        "tsserver-plugin-{:016x}",
        fnv1a(PLUGIN_SOURCE.as_bytes())
    ));
    let plugin = location.join("node_modules").join(PLUGIN_NAME);
    write_if_changed(&plugin.join("index.js"), PLUGIN_SOURCE)?;
    write_if_changed(
        &plugin.join("package.json"),
        &serde_json::json!({
            "name": PLUGIN_NAME,
            "version": "1.0.0",
            "main": "index.js",
            "type": "commonjs",
            "private": true,
        })
        .to_string(),
    )?;
    let file = cache.join("tsserver-workspaces").join(format!(
        "{:016x}.json",
        fnv1a(root.to_string_lossy().as_bytes())
    ));
    let mut document = serde_json::to_value(workspace).map_err(std::io::Error::other)?;
    document["root"] = serde_json::json!(root);
    write_if_changed(
        &file,
        &serde_json::to_string_pretty(&document).map_err(std::io::Error::other)?,
    )?;
    Ok((location, file))
}

/// Source file extensions, in the order a source entry is looked for.
const SOURCE_EXTENSIONS: &[&str] = &[".ts", ".tsx", ".mts", ".cts", ".d.ts"];

/// JavaScript source extensions, in the order Node and TypeScript try them.
/// A JavaScript package has no TypeScript source to find, and tsserver
/// analyses its JavaScript with `allowJs`, as it does the importing file.
const JAVASCRIPT_EXTENSIONS: &[&str] = &[".js", ".jsx", ".mjs", ".cjs"];

/// Build output directories a manifest's entry points into, which the
/// source mirrors without them.
const BUILD_DIRS: &[&str] = &["dist", "build", "lib", "out", "esm", "cjs"];

/// The entry's path inside its package without a leading `./`, and without
/// the extension a manifest names it with.
fn entry_stem(entry: &str) -> String {
    let entry = entry.trim_start_matches("./");
    for extension in [
        ".d.ts", ".d.mts", ".d.cts", ".js", ".mjs", ".cjs", ".ts", ".tsx", ".mts", ".cts", ".jsx",
    ] {
        if let Some(stripped) = entry.strip_suffix(extension) {
            return stripped.to_string();
        }
    }
    entry.to_string()
}

/// The entry's path with its build directory taken off, when it starts with
/// one: `dist/index` is `index` in the source it was built from.
fn without_build_dir(stem: &str) -> Option<&str> {
    let (first, rest) = stem.split_once('/')?;
    BUILD_DIRS.contains(&first).then_some(rest)
}

/// The first of `candidate` with each of `extensions`, and then of
/// `candidate/index` with each, that is a file.
fn first_file(candidate: &Path, extensions: &[&str]) -> Option<PathBuf> {
    [candidate.to_path_buf(), candidate.join("index")]
        .into_iter()
        .flat_map(|base| {
            extensions
                .iter()
                .map(move |extension| PathBuf::from(format!("{}{extension}", base.display())))
        })
        .find(|file| file.is_file())
}

/// The source file an entry a manifest names (`./dist/index.d.ts`,
/// `./index.cjs`, `./index.js`) corresponds to, when there is one.
///
/// TypeScript source first: the entry's stem, with and without its build
/// directory, under `src` and the package itself, as a file or a directory's
/// index. Then JavaScript, as Node resolves it: the entry itself when it is a
/// JavaScript file that exists, the stem with each JavaScript extension or
/// as a directory's index, and last, for an entry inside a build directory
/// that is not built, the source that directory is built from. React's
/// `react` names `./index.js`, and before JavaScript was looked for it had no
/// entry at all, so `import {useContext} from 'react'` resolved to nothing
/// and every value it returned was `any`.
fn source_of(dir: &Path, entry: &str) -> Option<PathBuf> {
    let stem = entry_stem(entry);
    let mut stems = vec![stem.clone()];
    if let Some(rest) = without_build_dir(&stem) {
        stems.push(rest.to_string());
    }
    for stem in &stems {
        for base in [dir.join("src"), dir.to_path_buf()] {
            if let Some(file) = first_file(&base.join(stem), SOURCE_EXTENSIONS) {
                return Some(file);
            }
        }
    }
    let exact = dir.join(entry.trim_start_matches("./"));
    if exact.is_file()
        && JAVASCRIPT_EXTENSIONS
            .iter()
            .any(|extension| entry.ends_with(extension))
    {
        return Some(exact);
    }
    if let Some(file) = first_file(&dir.join(&stem), JAVASCRIPT_EXTENSIONS) {
        return Some(file);
    }
    let rest = without_build_dir(&stem)?;
    [dir.join("src").join(rest), dir.join(rest)]
        .iter()
        .find_map(|candidate| first_file(candidate, JAVASCRIPT_EXTENSIONS))
}

/// Whether a package's manifest names its entry points in build output that
/// is not in the repository as source: an entry inside a build directory, or
/// an entry file that does not exist. Such a package's other subpaths are
/// found in its `src` mirror, as the entry's own source is. A package whose
/// manifest names entries that exist, or names none, resolves subpaths the
/// way Node does, from the package directory itself.
fn entries_are_built(dir: &Path, entries: &[String]) -> bool {
    entries.iter().any(|entry| {
        let stem = entry_stem(entry);
        without_build_dir(&stem).is_some() || !dir.join(entry.trim_start_matches("./")).exists()
    })
}

/// The target an `exports` entry names for type information: the `types`
/// condition first, then `import`, `require` and `default`, each possibly
/// nested.
fn export_target(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(target) => Some(target.clone()),
        serde_json::Value::Object(conditions) => ["types", "import", "require", "node", "default"]
            .iter()
            .find_map(|condition| conditions.get(*condition).and_then(export_target)),
        serde_json::Value::Array(targets) => targets.iter().find_map(export_target),
        _ => None,
    }
}

/// A workspace package's entries in the workspace map: its name to its
/// source entry, each subpath its `exports` name to that subpath's source,
/// and `name/*` to the directory other subpaths resolve under.
pub fn source_entries(package: &WorkspacePackage) -> Vec<(String, PathBuf)> {
    let manifest: serde_json::Value = std::fs::read_to_string(package.dir.join("package.json"))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(serde_json::Value::Null);
    let exports = manifest.get("exports");
    let root_export = exports.and_then(|exports| match exports {
        serde_json::Value::Object(map) if map.keys().any(|key| key.starts_with('.')) => {
            map.get(".").and_then(export_target)
        }
        other => export_target(other),
    });
    let named: Vec<String> = ["types", "typings", "module", "main"]
        .iter()
        .filter_map(|field| manifest.get(*field)?.as_str().map(str::to_string))
        .collect();
    let declared: Vec<String> = root_export.into_iter().chain(named).collect();
    let main = declared
        .iter()
        .cloned()
        .chain(std::iter::once("index".to_string()))
        .find_map(|entry| source_of(&package.dir, &entry));
    let src = package.dir.join("src");
    let mirrored = src.is_dir() && entries_are_built(&package.dir, &declared);
    let mut entries = Vec::new();
    if let Some(main) = main {
        entries.push((package.name.clone(), main));
    }
    if let Some(serde_json::Value::Object(map)) = exports {
        for (key, value) in map {
            let Some(subpath) = key.strip_prefix("./") else {
                continue;
            };
            let Some(target) = export_target(value) else {
                continue;
            };
            if subpath.contains('*') {
                // A subpath pattern, `"./src/*": "./src/*"`: the pattern
                // resolves where its target says, under the source mirror
                // when the target is build output.
                if let Some(under) = pattern_target(&package.dir, &target, mirrored) {
                    entries.push((format!("{}/{subpath}", package.name), under));
                }
                continue;
            }
            if let Some(file) = source_of(&package.dir, &target) {
                entries.push((format!("{}/{subpath}", package.name), file));
            }
        }
    }
    // Any other subpath resolves under the package, as Node resolves it, or
    // under its source mirror when the package's entries are build output.
    let under = if mirrored { src } else { package.dir.clone() };
    entries.push((format!("{}/*", package.name), under.join("*")));
    entries
}

/// Where an `exports` subpath pattern's target (`./src/*`, `./dist/*.js`)
/// resolves in the repository: the target's directory with its `*`, under
/// the source mirror when it is build output and the package is `mirrored`.
/// The extension after the `*` is dropped, because the plugin finds a
/// source file by its stem.
fn pattern_target(dir: &Path, target: &str, mirrored: bool) -> Option<PathBuf> {
    let target = target.trim_start_matches("./");
    let (before, _) = target.split_once('*')?;
    let before = match (mirrored, without_build_dir(target)) {
        (true, Some(_)) => {
            let rest = without_build_dir(before).unwrap_or("");
            return Some(dir.join("src").join(format!("{rest}*")));
        }
        _ => before,
    };
    Some(dir.join(format!("{before}*")))
}

/// The project model of a JavaScript workspace: its declared packages, and
/// the map from each to the source it resolves to (see [`source_entries`]).
pub fn project_model(root: &Path) -> ProjectModel {
    let packages = workspace_packages(root);
    ProjectModel {
        roots: vec![root.to_path_buf()],
        workspace_map: packages.iter().flat_map(source_entries).collect(),
        packages: packages
            .into_iter()
            .map(|package| ProjectPackage {
                name: package.name,
                source_roots: vec![package.dir.clone()],
                dir: package.dir,
                config: package.tsconfig,
            })
            .collect(),
        ..ProjectModel::default()
    }
}

/// The workspace packages a project model holds, as the plugin reads them.
fn plugin_packages(model: &ProjectModel) -> Vec<WorkspacePackage> {
    model
        .packages
        .iter()
        .map(|package| WorkspacePackage {
            name: package.name.clone(),
            dir: package.dir.clone(),
            tsconfig: package.config.clone(),
        })
        .collect()
}

/// What environment discovery reads from the host. Injected so every branch
/// is testable without the network or the host's own packages.
#[derive(Debug, Clone)]
pub struct TsHost {
    pub vars: HashMap<String, String>,
    pub home: Option<PathBuf>,
    /// Kin's cache directory, `$KIN_HOME/cache`.
    pub cache: PathBuf,
    pub analysis_environments: bool,
    pub platform: (&'static str, &'static str),
}

impl TsHost {
    /// This process's host.
    pub fn current() -> Self {
        let vars: HashMap<String, String> = std::env::vars().collect();
        let home = vars.get("HOME").map(PathBuf::from);
        Self {
            vars,
            home,
            cache: analysis_env::kin_cache_dir(),
            analysis_environments: analysis_env::enabled(),
            platform: analysis::host_platform(),
        }
    }

    fn analysis_host(&self) -> analysis::Host<'_> {
        analysis::Host {
            vars: &self.vars,
            home: self.home.as_deref(),
            cache: &self.cache,
            analysis_environments: self.analysis_environments,
            platform: self.platform,
        }
    }
}

fn workspaces_of(model: &ProjectModel) -> Vec<(String, PathBuf)> {
    model
        .packages
        .iter()
        .map(|package| (package.name.clone(), package.dir.clone()))
        .collect()
}

/// The version a `package.json` names.
fn package_version(dir: &Path) -> Option<String> {
    std::fs::read_to_string(dir.join("package.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|json| json.get("version")?.as_str().map(str::to_string))
}

/// The environment tsserver analyses the repository at `root` against, from
/// the first provider that has one. Nothing here touches the network; what
/// only a download can supply is left in [`Environment::pending`].
pub fn environment_with(root: &Path, model: &ProjectModel, host: &TsHost) -> Environment {
    let assessment = analysis::assess(root, &workspaces_of(model), &host.analysis_host());
    let lock_file = assessment.lock.as_ref().map(|lock| lock.file.clone());
    let lock_name = assessment
        .lock
        .as_ref()
        .map_or_else(String::new, |lock| lock.format.describe());
    let base = lock_file
        .as_deref()
        .and_then(Path::parent)
        .unwrap_or(root)
        .to_path_buf();
    let packages = assessment.selected.len();
    // Where the importers' node_modules are: the repository's own, or the
    // mirror in Kin's layout.
    let (modules_root, provider, pending) = match &assessment.dependencies {
        analysis::Dependencies::None => (
            None,
            Provider::UserEnvironment {
                description: "no JavaScript dependencies".to_string(),
                checked_against: lock_file.clone(),
            },
            None,
        ),
        analysis::Dependencies::UserNodeModules => (
            Some(base.clone()),
            Provider::UserEnvironment {
                description: "the node_modules the package manager installed".to_string(),
                checked_against: lock_file.clone(),
            },
            None,
        ),
        analysis::Dependencies::KinLayout { dir, ready } => {
            let mirror = dir.join(base.strip_prefix(root).unwrap_or(Path::new("")));
            (
                ready.then_some(mirror),
                Provider::KinAnalysisEnvironment {
                    description: format!(
                        "{packages} locked package(s) laid out outside the repository{}",
                        assessment
                            .passed_over
                            .as_deref()
                            .map(|reason| format!(
                                "; the repository's node_modules was passed over: {reason}"
                            ))
                            .unwrap_or_default()
                    ),
                    basis: EnvironmentBasis::Lockfile {
                        path: lock_file.clone().unwrap_or_default(),
                    },
                },
                (!ready).then(|| format!("fetch {packages} locked package(s) and lay them out")),
            )
        }
        analysis::Dependencies::Missing(reason) => (
            None,
            Provider::Missing {
                reason: reason.clone(),
            },
            None,
        ),
    };
    let toolchain = match (&assessment.typescript, &assessment.lock) {
        (Some((importer, key)), Some(lock)) => {
            let version = lock
                .packages
                .get(key)
                .map_or_else(|| "unknown".to_string(), |package| package.version.clone());
            let location = modules_root
                .as_ref()
                .map(|modules| modules.join(importer).join("node_modules/typescript"))
                .filter(|dir| dir.join("lib/tsserver.js").is_file());
            let installed = location.as_deref().and_then(package_version);
            Some(Toolchain {
                name: "typescript".to_string(),
                version: installed.unwrap_or(version.clone()),
                pinned_by: format!(
                    "{lock_name} pins typescript {version} for {}{}",
                    if importer.is_empty() {
                        "the root"
                    } else {
                        importer
                    },
                    if location.is_some() {
                        ""
                    } else {
                        "; the language server's bundled TypeScript runs until it is available"
                    }
                ),
                location,
                substitute: None,
            })
        }
        _ => None,
    };
    let dependencies = match (&assessment.dependencies, modules_root.as_ref()) {
        (analysis::Dependencies::KinLayout { dir, .. }, _) => vec![DependencySource {
            description: format!("{packages} locked package(s) in Kin's node_modules layout"),
            location: Some(dir.clone()),
            lock: lock_file.clone(),
        }],
        (analysis::Dependencies::UserNodeModules, Some(modules)) => vec![DependencySource {
            description: "the repository's node_modules".to_string(),
            location: Some(modules.join("node_modules")),
            lock: lock_file.clone(),
        }],
        _ => Vec::new(),
    };
    Environment {
        toolchain,
        dependencies,
        provider,
        identity: assessment.identity,
        pending,
    }
}

/// A TypeScript installed outside the repository, which
/// typescript-language-server can run when the workspace has none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutsideTypeScript {
    /// The package directory, holding `package.json` and `lib/tsserver.js`.
    pub dir: PathBuf,
    /// The version its `package.json` names, when it names one.
    pub version: Option<String>,
}

impl OutsideTypeScript {
    /// The tsserver typescript-language-server is pointed at.
    pub fn tsserver(&self) -> PathBuf {
        self.dir.join("lib/tsserver.js")
    }

    /// How a launch label names it.
    fn describe(&self) -> String {
        match &self.version {
            Some(version) => format!("global TypeScript {version} from {}", self.dir.display()),
            None => format!("global TypeScript from {}", self.dir.display()),
        }
    }
}

/// The first file named `name` in the directories of `path`.
fn find_on_path(path: Option<&str>, name: &str) -> Option<PathBuf> {
    std::env::split_paths(path?)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// A `typescript` package directory that can serve tsserver: one whose
/// `lib/tsserver.js` exists. TypeScript 7 ships none, and
/// typescript-language-server cannot run it.
fn valid_typescript(dir: &Path) -> Option<OutsideTypeScript> {
    dir.join("lib/tsserver.js")
        .is_file()
        .then(|| OutsideTypeScript {
            dir: dir.to_path_buf(),
            version: package_version(dir),
        })
}

/// Find a TypeScript outside the repository at `root` that
/// typescript-language-server can run, or say where none was found.
///
/// typescript-language-server 5 looks in the workspace's
/// `node_modules/typescript`, then at `tsserver.fallbackPath`, then for a
/// `typescript` it can resolve from its own install, and refuses to
/// initialize when none of them is there. A repository whose dependencies are
/// not installed has none of the first, so this supplies the second: a
/// `typescript` package beside the `typescript-language-server` on `PATH`
/// (the same `node_modules` its symlink leads into, or its own nested one),
/// then the package behind a `tsserver` or `tsc` on `PATH`. A candidate
/// inside the repository is passed over, since the workspace's own TypeScript
/// is already tried first, and so is one without `lib/tsserver.js`.
///
/// The `Err` is the sentence a start failure carries when no TypeScript runs.
pub fn outside_typescript(root: &Path, host: &TsHost) -> Result<OutsideTypeScript, String> {
    let path = host.vars.get("PATH").map(String::as_str);
    // Candidates are followed through their symlinks, so the repository is
    // compared by its real path too.
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let outside = |dir: &Path| !dir.starts_with(&root);
    let mut looked: Vec<String> = Vec::new();

    match find_on_path(path, "typescript-language-server") {
        Some(server) => {
            let real = std::fs::canonicalize(&server).unwrap_or(server.clone());
            // The package directory is the ancestor that sits directly in a
            // `node_modules`: `.../node_modules/typescript-language-server`.
            let package = real.ancestors().find(|dir| {
                dir.parent()
                    .and_then(Path::file_name)
                    .is_some_and(|name| name == "node_modules")
            });
            match package {
                Some(package) => {
                    let modules = package.parent().unwrap_or(package);
                    for candidate in [
                        modules.join("typescript"),
                        package.join("node_modules/typescript"),
                    ] {
                        if !outside(&candidate) {
                            continue;
                        }
                        if let Some(found) = valid_typescript(&candidate) {
                            return Ok(found);
                        }
                    }
                    looked.push(format!(
                        "beside typescript-language-server in {}",
                        modules.display()
                    ));
                }
                None => looked.push(format!(
                    "beside typescript-language-server, which is not installed in a \
                     node_modules ({})",
                    real.display()
                )),
            }
        }
        None => looked.push("beside typescript-language-server, which is not on PATH".into()),
    }

    for binary in ["tsserver", "tsc"] {
        let Some(found) = find_on_path(path, binary) else {
            continue;
        };
        let real = std::fs::canonicalize(&found).unwrap_or(found);
        // `bin/tsc` and `bin/tsserver` sit one level inside the package.
        let package = real.ancestors().skip(1).take(3).find(|dir| {
            std::fs::read_to_string(dir.join("package.json"))
                .ok()
                .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
                .is_some_and(|json| json.get("name").and_then(|n| n.as_str()) == Some("typescript"))
        });
        if let Some(package) = package.filter(|dir| outside(dir)) {
            if let Some(found) = valid_typescript(package) {
                return Ok(found);
            }
            looked.push(format!(
                "behind {binary} at {}, which has no lib/tsserver.js",
                package.display()
            ));
        }
    }
    let home = host.home.as_deref();
    looked.push(format!(
        "for tsserver or tsc on PATH (looked in {})",
        crate::discovery::describe_search_path(path.map(std::ffi::OsStr::new), home)
    ));
    Err(format!(
        "No TypeScript with a lib/tsserver.js was found outside the repository: Kin looked {}",
        looked.join(", and ")
    ))
}

/// The launch for a repository, with the plugin kept under `cache`.
pub fn launch_with(root: &Path, cache: &Path) -> ServerLaunch {
    let model = project_model(root);
    let environment = Environment::from_provider(
        Provider::UserEnvironment {
            description: "not assessed".to_string(),
            checked_against: None,
        },
        &["typescript", "not assessed"],
    );
    configure_with(root, &model, &environment, cache)
}

/// What typescript-language-server logs, as an error, when its tsserver
/// exits: `[tsclient] [tsserver] Exited. Code: null. Signal: SIGABRT`, and
/// `Exited with error` when it failed to run. It stays up after that and
/// answers every request with an empty result.
pub const TSSERVER_EXIT_REPORT: &str = "[tsserver] Exited";

/// The launch for a model and an environment, with the plugin kept under
/// `cache`, on this process's host.
pub fn configure_with(
    root: &Path,
    model: &ProjectModel,
    environment: &Environment,
    cache: &Path,
) -> ServerLaunch {
    configure_on(root, model, environment, cache, &TsHost::current())
}

/// [`configure_with`] on a given host, whose `PATH` decides which TypeScript
/// outside the repository the server is offered.
pub fn configure_on(
    root: &Path,
    model: &ProjectModel,
    environment: &Environment,
    cache: &Path,
    host: &TsHost,
) -> ServerLaunch {
    ServerLaunch {
        backend_exit_report: Some(TSSERVER_EXIT_REPORT.to_string()),
        readiness: super::Readiness::PerDocument,
        ..configure_server(root, model, environment, cache, host)
    }
}

fn configure_server(
    root: &Path,
    model: &ProjectModel,
    environment: &Environment,
    cache: &Path,
    host: &TsHost,
) -> ServerLaunch {
    let mut options = base_options();
    let tsserver = environment
        .toolchain
        .as_ref()
        .and_then(|toolchain| toolchain.location.as_ref())
        .map(|dir| dir.join("lib/tsserver.js"));
    if let Some(tsserver) = &tsserver {
        options["tsserver"]["path"] = serde_json::json!(tsserver.display().to_string());
    }
    // With no TypeScript from the environment, the server is offered one
    // from outside the repository as its fallback. The workspace's own
    // `node_modules/typescript` still comes first, since the server tries it
    // before the fallback.
    let mut toolchain_gap = None;
    let mut typescript = environment.toolchain.as_ref().map_or_else(
        || "bundled TypeScript".to_string(),
        |toolchain| match &toolchain.location {
            Some(_) => format!("TypeScript {}", toolchain.version),
            None => "bundled TypeScript".to_string(),
        },
    );
    if tsserver.is_none() {
        if let Some(own) = valid_typescript(&root.join("node_modules/typescript")) {
            typescript = match own.version {
                Some(version) => format!("the workspace's TypeScript {version}"),
                None => "the workspace's TypeScript".to_string(),
            };
        } else {
            match outside_typescript(root, host) {
                Ok(found) => {
                    options["tsserver"]["fallbackPath"] =
                        serde_json::json!(found.tsserver().display().to_string());
                    typescript = found.describe();
                }
                Err(looked) => toolchain_gap = Some(looked),
            }
        }
    }
    let layout = match &environment.provider {
        Provider::KinAnalysisEnvironment { .. } if environment.pending.is_none() => environment
            .dependencies
            .first()
            .and_then(|source| source.location.clone())
            .filter(|dir| dir.join(".complete").is_file()),
        _ => None,
    };
    let dependencies = match (&environment.provider, &layout) {
        (Provider::KinAnalysisEnvironment { .. }, Some(_)) => format!(
            "Kin's analysis environment {}",
            &environment.identity.hex()[..12]
        ),
        (Provider::KinAnalysisEnvironment { .. }, None) => {
            "no dependencies until Kin's analysis environment is fetched".to_string()
        }
        (Provider::UserEnvironment { description, .. }, _) => description.clone(),
        (Provider::Missing { reason }, _) => {
            format!("no dependencies (environment missing: {reason})")
        }
    };
    let workspace = PluginWorkspace {
        packages: plugin_packages(model),
        workspace_map: model.workspace_map.clone(),
        layout,
    };
    if workspace.packages.is_empty() && workspace.layout.is_none() {
        return ServerLaunch {
            initialization_options: Some(options),
            label: format!("typescript-language-server with {typescript}; {dependencies}"),
            toolchain_gap,
            ..ServerLaunch::default()
        };
    }
    match install_plugin(cache, root, &workspace) {
        Ok((location, file)) => {
            options["plugins"] = serde_json::json!([{
                "name": PLUGIN_NAME,
                "location": location.display().to_string(),
            }]);
            ServerLaunch {
                initialization_options: Some(options),
                env: vec![(WORKSPACE_ENV.to_string(), file.display().to_string())],
                env_identity: vec![(
                    WORKSPACE_ENV.to_string(),
                    workspace_identity(&workspace, root),
                )],
                label: format!(
                    "typescript-language-server with {typescript}, {} workspace package(s) \
                     resolvable from source; {dependencies}",
                    workspace.packages.len()
                ),
                toolchain_gap,
                ..ServerLaunch::default()
            }
        }
        Err(error) => {
            tracing::warn!(
                %error,
                cache = %cache.display(),
                "could not install the workspace-source plugin; workspace packages reached \
                 through an unbuilt output directory stay unresolved, and so does Kin's \
                 dependency layout"
            );
            ServerLaunch {
                initialization_options: Some(options),
                label: format!(
                    "typescript-language-server with {typescript}, without the workspace-source \
                     plugin; {dependencies}"
                ),
                toolchain_gap,
                ..ServerLaunch::default()
            }
        }
    }
}

/// Fetch what the repository's environment has pending, with the host it
/// runs on. `None` when nothing is pending.
pub fn provision_with(
    root: &Path,
    host: &TsHost,
    fetcher: &dyn analysis_env::fetch::Fetcher,
) -> Option<ProvisionReport> {
    let model = project_model(root);
    let environment = environment_with(root, &model, host);
    environment.pending.as_ref()?;
    Some(analysis::provision(
        root,
        &workspaces_of(&model),
        &host.analysis_host(),
        fetcher,
    ))
}

impl LspAdapter for TypeScriptAdapter {
    fn language_id(&self) -> LanguageId {
        LanguageId::TypeScript
    }

    fn server_command(&self) -> &str {
        "typescript-language-server"
    }

    fn server_args(&self) -> Vec<String> {
        vec!["--stdio".to_string()]
    }

    fn file_extensions(&self) -> &[&str] {
        &["ts", "tsx", "js", "jsx"]
    }

    /// The options without the workspace plugin, which [`Self::launch`] adds
    /// once it has installed the plugin.
    fn initialization_options(&self, _workspace_root: &Path) -> Option<serde_json::Value> {
        Some(base_options())
    }

    fn project_model(&self, workspace_root: &Path) -> ProjectModel {
        project_model(workspace_root)
    }

    fn environment(&self, workspace_root: &Path, model: &ProjectModel) -> Environment {
        environment_with(workspace_root, model, &TsHost::current())
    }

    fn configure(
        &self,
        workspace_root: &Path,
        model: &ProjectModel,
        environment: &Environment,
    ) -> ServerLaunch {
        configure_with(workspace_root, model, environment, &kin_lsp_cache_dir())
    }

    fn provision(&self, workspace_root: &Path) -> Option<ProvisionReport> {
        let host = TsHost::current();
        if !host.analysis_environments {
            return None;
        }
        let registries =
            analysis::Registries::read(workspace_root, &host.vars, host.home.as_deref());
        let fetcher = match analysis_env::fetch::HttpFetcher::new(&registries.network) {
            Ok(fetcher) => registries
                .authorizations()
                .into_iter()
                .fold(fetcher, |fetcher, (prefix, value)| {
                    fetcher.with_authorization(prefix, value)
                }),
            Err(reason) => {
                return Some(ProvisionReport {
                    failure: Some(reason),
                    ..ProvisionReport::default()
                });
            }
        };
        provision_with(workspace_root, &host, &fetcher)
    }

    fn requires_workspace_indexing(&self) -> bool {
        true
    }

    fn estimated_index_time_secs(&self) -> u32 {
        10
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::repo_scan::Fixture;

    #[test]
    fn pnpm_packages_are_read_in_block_and_flow_style() {
        let block = "packages:\n  - drizzle-orm\n  - 'drizzle-*' # all\n  - \"!**/test/**\"\nonlyBuiltDependencies:\n  - esbuild\n";
        assert_eq!(
            pnpm_packages(block),
            vec!["drizzle-orm", "drizzle-*", "!**/test/**"]
        );
        assert_eq!(
            pnpm_packages("packages: ['apps/*', \"libs/**\"]\n"),
            vec!["apps/*", "libs/**"]
        );
    }

    #[test]
    fn workspace_globs_match_segments_and_depths() {
        assert!(glob_matches("packages/*", "packages/core"));
        assert!(!glob_matches("packages/*", "packages/core/nested"));
        assert!(glob_matches("packages/**", "packages/core/nested"));
        assert!(glob_matches("./drizzle-*", "drizzle-zod"));
        assert!(glob_matches("**/test/**", "a/test/b"));
        assert!(!glob_matches("drizzle-orm", "drizzle-orm-old"));
    }

    /// drizzle-orm's shape: a pnpm workspace whose packages each carry a
    /// tsconfig. A package the globs do not name, a negated one, and anything
    /// under node_modules are not workspace packages.
    #[test]
    fn workspace_packages_come_from_the_declared_globs() {
        let repo = Fixture::new("ts-workspace");
        repo.write(
            "pnpm-workspace.yaml",
            "packages:\n  - drizzle-orm\n  - drizzle-seed\n  - integration-tests\n  - 'examples/*'\n  - '!examples/skip'\n",
        );
        repo.write("package.json", "{\"name\": \"root\"}");
        repo.write("drizzle-orm/package.json", "{\"name\": \"drizzle-orm\"}");
        repo.write("drizzle-orm/tsconfig.json", "{}");
        repo.write("drizzle-seed/package.json", "{\"name\": \"drizzle-seed\"}");
        repo.write("integration-tests/package.json", "{\"private\": true}");
        repo.write("examples/one/package.json", "{\"name\": \"example-one\"}");
        repo.write("examples/skip/package.json", "{\"name\": \"example-skip\"}");
        repo.write("undeclared/package.json", "{\"name\": \"undeclared\"}");
        repo.write(
            "node_modules/drizzle-orm/package.json",
            "{\"name\": \"drizzle-orm\"}",
        );

        let packages = workspace_packages(&repo.root);
        let names: Vec<&str> = packages.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["drizzle-orm", "drizzle-seed", "example-one"]);
        assert_eq!(
            packages[0].tsconfig,
            Some(repo.root.join("drizzle-orm/tsconfig.json"))
        );
        assert_eq!(packages[1].tsconfig, None);
    }

    #[test]
    fn npm_and_yarn_workspaces_are_read_from_package_json() {
        let repo = Fixture::new("ts-npm-workspace");
        repo.write(
            "package.json",
            "{\"name\": \"root\", \"workspaces\": {\"packages\": [\"packages/*\"]}}",
        );
        repo.write("packages/a/package.json", "{\"name\": \"@scope/a\"}");
        assert_eq!(
            workspace_packages(&repo.root)
                .iter()
                .map(|p| p.name.clone())
                .collect::<Vec<_>>(),
            vec!["@scope/a"]
        );
    }

    /// With workspace packages, the launch installs the plugin under Kin's
    /// cache, never in the repository, names it in the options, and points the
    /// plugin at the workspace file through the environment.
    #[test]
    fn a_workspace_launch_installs_the_plugin_outside_the_repository() {
        let repo = Fixture::new("ts-launch");
        let cache = Fixture::new("ts-cache");
        repo.write("pnpm-workspace.yaml", "packages:\n  - lib\n");
        repo.write("lib/package.json", "{\"name\": \"lib\"}");
        repo.write("lib/tsconfig.json", "{}");

        let launch = launch_with(&repo.root, &cache.root);
        assert_eq!(launch.readiness, super::super::Readiness::PerDocument);
        assert_eq!(
            launch.backend_exit_report.as_deref(),
            Some(TSSERVER_EXIT_REPORT)
        );
        let options = launch.initialization_options.expect("options");
        let plugin = &options["plugins"][0];
        assert_eq!(plugin["name"], PLUGIN_NAME);
        let location = PathBuf::from(plugin["location"].as_str().unwrap());
        assert!(location.starts_with(&cache.root), "{}", location.display());
        let installed = location.join("node_modules").join(PLUGIN_NAME);
        assert_eq!(
            std::fs::read_to_string(installed.join("index.js")).unwrap(),
            PLUGIN_SOURCE
        );
        let (name, workspace) = &launch.env[0];
        assert_eq!(name, WORKSPACE_ENV);
        let written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(workspace).unwrap()).unwrap();
        assert_eq!(written["packages"][0]["name"], "lib");
        assert_eq!(
            written["packages"][0]["tsconfig"],
            repo.root.join("lib/tsconfig.json").display().to_string()
        );
        assert!(options["maxTsServerMemory"].as_u64().unwrap() >= 3072);
        assert_eq!(
            options["tsserver"]["useSyntaxServer"], "never",
            "answers come from the semantic server only"
        );

        // Nothing was written into the repository.
        let mut written_in_repo = Vec::new();
        repo_scan::walk_files(&repo.root, &|_, _| true, &mut |path, _| {
            written_in_repo.push(path.strip_prefix(&repo.root).unwrap().to_path_buf())
        });
        written_in_repo.sort();
        assert_eq!(
            written_in_repo,
            vec![
                PathBuf::from("lib/package.json"),
                PathBuf::from("lib/tsconfig.json"),
                PathBuf::from("pnpm-workspace.yaml"),
            ]
        );
    }

    /// One repository checked out at two paths gives one proof context,
    /// although the workspace file each launch names is keyed by its absolute
    /// path. A different workspace is a different configuration.
    #[test]
    fn a_workspace_launch_proves_the_same_wherever_the_repository_is() {
        let cache = Fixture::new("ts-relocation-cache");
        let checkout = |name: &str, packages: &[&str]| {
            let repo = Fixture::new(name);
            let globs: String = packages.iter().map(|p| format!("  - {p}\n")).collect();
            repo.write("pnpm-workspace.yaml", &format!("packages:\n{globs}"));
            for package in packages {
                repo.write(
                    &format!("{package}/package.json"),
                    &format!("{{\"name\": \"{package}\"}}"),
                );
                repo.write(&format!("{package}/tsconfig.json"), "{}");
            }
            repo
        };
        let basis = |repo: &Fixture| {
            let launch = launch_with(&repo.root, &cache.root);
            let basis = crate::proof_context::ProofBasis::of(
                &launch,
                &repo.root,
                "typescript-language-server",
                Some("typescript-language-server"),
                Some("4.3.3"),
            );
            (launch.env[0].1.clone(), basis)
        };

        let (here_file, here) = basis(&checkout("ts-relocation-here", &["lib"]));
        let (there_file, there) = basis(&checkout("ts-relocation-there", &["lib"]));
        assert_ne!(
            here_file, there_file,
            "each checkout names its own workspace file"
        );
        assert_eq!(here, there, "and both prove under one configuration");

        // One package either way, so the launch label matches and only what
        // the workspace file says can tell the two apart.
        let (_, other) = basis(&checkout("ts-relocation-other", &["core"]));
        assert_ne!(
            here, other,
            "a workspace with other packages is another configuration"
        );
    }

    /// The daemon installs from its readiness probe and its sweep at the same
    /// time. Concurrent installs into one cache all succeed and leave whole
    /// files and no staging debris.
    #[test]
    fn concurrent_installs_into_one_cache_all_succeed() {
        let repo = Fixture::new("ts-concurrent");
        let cache = Fixture::new("ts-concurrent-cache");
        repo.write("pnpm-workspace.yaml", "packages:\n  - lib\n");
        repo.write("lib/package.json", "{\"name\": \"lib\"}");
        let packages = workspace_packages(&repo.root);
        std::thread::scope(|scope| {
            let installs: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| install_workspace_plugin(&cache.root, &repo.root, &packages))
                })
                .collect();
            for install in installs {
                install
                    .join()
                    .unwrap()
                    .expect("every concurrent install succeeds");
            }
        });
        let mut leftovers = Vec::new();
        repo_scan::walk_files(&cache.root, &|_, _| true, &mut |_, name| {
            if name.ends_with(".tmp") {
                leftovers.push(name.to_string());
            }
        });
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    /// A host whose `PATH` is `path` and nothing else.
    fn host_with_path(cache: &Path, path: &str) -> TsHost {
        TsHost {
            vars: HashMap::from([("PATH".to_string(), path.to_string())]),
            ..ts_host(cache, false)
        }
    }

    /// A global npm prefix the way Homebrew lays one out: `bin/` holding a
    /// symlink into `lib/node_modules/<package>`.
    #[cfg(unix)]
    fn npm_prefix(prefix: &Fixture, typescript: Option<(&str, bool)>) -> PathBuf {
        prefix.write(
            "lib/node_modules/typescript-language-server/lib/cli.mjs",
            "#!/usr/bin/env node\n",
        );
        prefix.write(
            "lib/node_modules/typescript-language-server/package.json",
            "{\"name\": \"typescript-language-server\", \"version\": \"5.1.0\"}",
        );
        if let Some((version, tsserver)) = typescript {
            prefix.write(
                "lib/node_modules/typescript/package.json",
                &format!("{{\"name\": \"typescript\", \"version\": \"{version}\"}}"),
            );
            prefix.write(
                "lib/node_modules/typescript/bin/tsc",
                "#!/usr/bin/env node\n",
            );
            if tsserver {
                prefix.write("lib/node_modules/typescript/lib/tsserver.js", "");
            }
        }
        let bin = prefix.root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::os::unix::fs::symlink(
            "../lib/node_modules/typescript-language-server/lib/cli.mjs",
            bin.join("typescript-language-server"),
        )
        .unwrap();
        bin
    }

    /// The field report: a repository with no node_modules. The server is
    /// offered the TypeScript installed beside it, by symlink, as its
    /// fallback, and the label says which TypeScript runs.
    #[cfg(unix)]
    #[test]
    fn without_dependencies_the_server_is_offered_the_typescript_beside_it() {
        let repo = Fixture::new("ts-outside-repo");
        let cache = Fixture::new("ts-outside-cache");
        let prefix = Fixture::new("ts-outside-prefix");
        let bin = npm_prefix(&prefix, Some(("5.9.3", true)));
        let host = host_with_path(&cache.root, &format!("/kin-test-nowhere:{}", bin.display()));

        let found = outside_typescript(&repo.root, &host).expect("the sibling TypeScript");
        let expected =
            std::fs::canonicalize(prefix.root.join("lib/node_modules/typescript")).unwrap();
        assert_eq!(found.dir, expected);
        assert_eq!(found.version.as_deref(), Some("5.9.3"));

        let model = project_model(&repo.root);
        let environment = Environment::from_provider(
            Provider::Missing {
                reason: "no lockfile, and no node_modules".to_string(),
            },
            &["typescript", "missing"],
        );
        let launch = configure_on(&repo.root, &model, &environment, &cache.root, &host);
        let options = launch.initialization_options.expect("options");
        assert_eq!(
            options["tsserver"]["fallbackPath"],
            expected.join("lib/tsserver.js").display().to_string()
        );
        assert!(
            options["tsserver"].get("path").is_none(),
            "the workspace's own TypeScript still comes first"
        );
        assert!(
            launch.label.contains(&format!(
                "global TypeScript 5.9.3 from {}",
                expected.display()
            )),
            "{}",
            launch.label
        );
        assert!(
            launch
                .label
                .contains("environment missing: no lockfile, and no node_modules"),
            "the label names why dependencies are missing: {}",
            launch.label
        );
        assert_eq!(launch.toolchain_gap, None);
    }

    /// TypeScript 7 ships no `lib/tsserver.js`, so one beside the server is
    /// passed over, and a `tsc` on PATH leading to a valid package is used.
    #[cfg(unix)]
    #[test]
    fn a_typescript_without_tsserver_is_passed_over_for_one_behind_tsc() {
        let repo = Fixture::new("ts7-repo");
        let cache = Fixture::new("ts7-cache");
        let native = Fixture::new("ts7-prefix");
        let native_bin = npm_prefix(&native, Some(("7.0.0", false)));
        let classic = Fixture::new("ts5-prefix");
        classic.write(
            "lib/node_modules/typescript/package.json",
            "{\"name\": \"typescript\", \"version\": \"5.8.2\"}",
        );
        classic.write(
            "lib/node_modules/typescript/bin/tsc",
            "#!/usr/bin/env node\n",
        );
        classic.write("lib/node_modules/typescript/lib/tsserver.js", "");
        let classic_bin = classic.root.join("bin");
        std::fs::create_dir_all(&classic_bin).unwrap();
        std::os::unix::fs::symlink(
            "../lib/node_modules/typescript/bin/tsc",
            classic_bin.join("tsc"),
        )
        .unwrap();

        let only_native = host_with_path(&cache.root, &native_bin.display().to_string());
        let looked = outside_typescript(&repo.root, &only_native)
            .expect_err("TypeScript 7 alone cannot serve tsserver");
        assert!(
            looked.contains("beside typescript-language-server in"),
            "{looked}"
        );
        assert!(
            looked.contains("for tsserver or tsc on PATH (looked in"),
            "{looked}"
        );

        let both = host_with_path(
            &cache.root,
            &format!("{}:{}", native_bin.display(), classic_bin.display()),
        );
        let found = outside_typescript(&repo.root, &both).expect("the TypeScript behind tsc");
        assert_eq!(found.version.as_deref(), Some("5.8.2"));
        assert!(found.tsserver().is_file());
    }

    /// No TypeScript anywhere: the launch carries where Kin looked, so a
    /// start that fails says it plainly, and nothing is offered.
    #[cfg(unix)]
    #[test]
    fn with_no_typescript_anywhere_the_launch_says_where_kin_looked() {
        let repo = Fixture::new("ts-none-repo");
        let cache = Fixture::new("ts-none-cache");
        let prefix = Fixture::new("ts-none-prefix");
        let bin = npm_prefix(&prefix, None);
        let host = host_with_path(&cache.root, &bin.display().to_string());
        let model = project_model(&repo.root);
        let environment = Environment::from_provider(
            Provider::Missing {
                reason: "no lockfile".to_string(),
            },
            &["typescript", "missing"],
        );
        let launch = configure_on(&repo.root, &model, &environment, &cache.root, &host);
        let gap = launch.toolchain_gap.expect("no TypeScript is a named gap");
        assert!(
            gap.starts_with("No TypeScript with a lib/tsserver.js was found"),
            "{gap}"
        );
        assert!(gap.contains(&bin.display().to_string()), "{gap}");
        assert!(launch.initialization_options.unwrap()["tsserver"]
            .get("fallbackPath")
            .is_none());
    }

    /// A TypeScript inside the repository is never the fallback: the
    /// workspace's own is already tried first, by the server itself.
    #[cfg(unix)]
    #[test]
    fn a_typescript_inside_the_repository_is_not_the_fallback() {
        let repo = Fixture::new("ts-inside-repo");
        let cache = Fixture::new("ts-inside-cache");
        repo.write(
            "tools/node_modules/typescript/package.json",
            "{\"name\": \"typescript\", \"version\": \"5.4.0\"}",
        );
        repo.write("tools/node_modules/typescript/bin/tsc", "");
        repo.write("tools/node_modules/typescript/lib/tsserver.js", "");
        let bin = repo.root.join("tools/bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::os::unix::fs::symlink("../node_modules/typescript/bin/tsc", bin.join("tsc")).unwrap();
        let host = host_with_path(&cache.root, &bin.display().to_string());
        assert!(outside_typescript(&repo.root, &host).is_err());
    }

    fn ts_host(cache: &Path, on: bool) -> TsHost {
        TsHost {
            vars: HashMap::new(),
            home: None,
            cache: cache.to_path_buf(),
            analysis_environments: on,
            platform: ("darwin", "arm64"),
        }
    }

    /// The workspace map is data in the model: each package to its source
    /// entry (found from a manifest entry that points at unbuilt output),
    /// each exported subpath to its source, and `name/*` to the directory
    /// other subpaths resolve under.
    #[test]
    fn the_workspace_map_names_each_packages_source_entry() {
        let repo = Fixture::new("ts-workspace-map");
        repo.write("pnpm-workspace.yaml", "packages:\n  - lib\n  - kit\n");
        repo.write(
            "lib/package.json",
            "{\"name\": \"@x/lib\", \"types\": \"./dist/index.d.ts\", \"main\": \"./dist/index.cjs\"}",
        );
        repo.write("lib/tsconfig.json", "{}");
        repo.write("lib/src/index.ts", "export const a = 1;\n");
        repo.write("lib/src/pg/index.ts", "export const b = 1;\n");
        repo.write(
            "kit/package.json",
            "{\"name\": \"kit\", \"exports\": {\".\": {\"import\": {\"types\": \"./index.d.mts\", \"default\": \"./index.mjs\"}}, \"./api\": {\"types\": \"./api.d.ts\"}}}",
        );
        repo.write("kit/src/index.ts", "");
        repo.write("kit/src/api.ts", "");
        let model = TypeScriptAdapter.project_model(&repo.root);
        assert_eq!(
            model.workspace_map["@x/lib"],
            repo.root.join("lib/src/index.ts")
        );
        assert_eq!(model.workspace_map["@x/lib/*"], repo.root.join("lib/src/*"));
        assert_eq!(
            model.workspace_map["kit"],
            repo.root.join("kit/src/index.ts")
        );
        assert_eq!(
            model.workspace_map["kit/api"],
            repo.root.join("kit/src/api.ts")
        );
        assert_eq!(
            model
                .packages
                .iter()
                .find(|p| p.name == "@x/lib")
                .unwrap()
                .config,
            Some(repo.root.join("lib/tsconfig.json"))
        );
    }

    /// A JavaScript workspace package resolves as Node resolves it: its
    /// entry is the JavaScript file its manifest names, an `exports` subpath
    /// pattern resolves where its target says, and any other subpath resolves
    /// under the package itself, not under a `src` mirror, because nothing
    /// in its manifest is build output. A TypeScript package whose manifest
    /// names build output keeps its `src` mirror. Falsify by looking only for
    /// TypeScript source, or by mirroring every package with a `src`: React's
    /// `react` had no entry, and `react-reconciler/src/X` resolved to
    /// `react-reconciler/src/src/X`.
    #[test]
    fn a_javascript_workspace_package_resolves_as_node_resolves_it() {
        let repo = Fixture::new("js-workspace-map");
        repo.write(
            "package.json",
            "{\"private\": true, \"workspaces\": [\"packages/*\"]}",
        );
        repo.write(
            "packages/react/package.json",
            "{\"name\": \"react\", \"main\": \"index.js\", \"exports\": {\".\": {\"react-server\": \"./react.react-server.js\", \"default\": \"./index.js\"}, \"./jsx-runtime\": {\"default\": \"./jsx-runtime.js\"}, \"./src/*\": \"./src/*\", \"./package.json\": \"./package.json\"}}",
        );
        repo.write(
            "packages/react/index.js",
            "export * from './src/ReactClient';\n",
        );
        repo.write("packages/react/jsx-runtime.js", "");
        repo.write("packages/react/src/ReactClient.js", "");
        repo.write(
            "packages/react-reconciler/package.json",
            "{\"name\": \"react-reconciler\", \"main\": \"index.js\"}",
        );
        repo.write("packages/react-reconciler/index.js", "");
        repo.write("packages/react-reconciler/src/ReactFiber.js", "");
        repo.write(
            "packages/shared/package.json",
            "{\"name\": \"shared\", \"private\": true}",
        );
        repo.write("packages/shared/assign.js", "");
        repo.write(
            "packages/devtools/package.json",
            "{\"name\": \"devtools\", \"private\": true}",
        );
        repo.write("packages/devtools/src/store.js", "");
        repo.write(
            "packages/built/package.json",
            "{\"name\": \"built\", \"main\": \"./dist/index.js\", \"exports\": {\".\": \"./dist/index.js\", \"./features/*\": \"./dist/features/*.js\"}}",
        );
        repo.write("packages/built/src/index.js", "");
        repo.write("packages/built/src/features/a.js", "");

        let model = TypeScriptAdapter.project_model(&repo.root);
        let map = &model.workspace_map;
        let at = |path: &str| repo.root.join(path);
        assert_eq!(map["react"], at("packages/react/index.js"));
        assert_eq!(
            map["react/jsx-runtime"],
            at("packages/react/jsx-runtime.js")
        );
        assert_eq!(map["react/src/*"], at("packages/react/src/*"));
        assert_eq!(map["react/*"], at("packages/react/*"));
        assert!(!map.contains_key("react/package.json"));
        assert_eq!(
            map["react-reconciler"],
            at("packages/react-reconciler/index.js")
        );
        assert_eq!(
            map["react-reconciler/*"],
            at("packages/react-reconciler/*"),
            "`react-reconciler/src/ReactFiber` is the package's own src"
        );
        assert_eq!(map["shared/*"], at("packages/shared/*"));
        assert!(!map.contains_key("shared"), "no entry file to name");
        assert_eq!(map["devtools/*"], at("packages/devtools/*"));
        // Build output that is not built resolves to its source.
        assert_eq!(map["built"], at("packages/built/src/index.js"));
        assert_eq!(map["built/*"], at("packages/built/src/*"));
        assert_eq!(map["built/features/*"], at("packages/built/src/features/*"));
    }

    /// The repository's own node_modules serves when it holds the lock's
    /// versions; otherwise Kin's layout is pending, or with the switch off the
    /// environment is missing and says why.
    #[test]
    fn the_environment_is_the_users_node_modules_only_when_it_matches_the_lock() {
        let repo = Fixture::new("ts-environment");
        let cache = Fixture::new("ts-environment-cache");
        repo.write("package.json", "{\"name\": \"app\"}");
        repo.write(
            "pnpm-lock.yaml",
            "lockfileVersion: '9.0'\n\nimporters:\n\n  .:\n    devDependencies:\n      typescript:\n        specifier: ^5\n        version: 5.6.3\n\n\
             packages:\n\n  typescript@5.6.3:\n    resolution: {integrity: sha512-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==}\n\n\
             snapshots:\n\n  typescript@5.6.3: {}\n",
        );
        let model = TypeScriptAdapter.project_model(&repo.root);
        let off = environment_with(&repo.root, &model, &ts_host(&cache.root, false));
        assert!(
            off.missing_reason()
                .is_some_and(|reason| reason.contains("analysis environments are off")),
            "{:?}",
            off.provider
        );
        let pending = environment_with(&repo.root, &model, &ts_host(&cache.root, true));
        assert_eq!(
            pending.pending.as_deref(),
            Some("fetch 1 locked package(s) and lay them out")
        );
        let toolchain = pending.toolchain.clone().unwrap();
        assert_eq!(toolchain.version, "5.6.3");
        assert!(
            toolchain.location.is_none(),
            "the bundled TypeScript until then"
        );

        repo.write(
            "node_modules/typescript/package.json",
            "{\"name\": \"typescript\", \"version\": \"5.6.3\"}",
        );
        repo.write("node_modules/typescript/lib/tsserver.js", "");
        let installed = environment_with(&repo.root, &model, &ts_host(&cache.root, true));
        assert!(matches!(
            installed.provider,
            Provider::UserEnvironment { .. }
        ));
        let toolchain = installed.toolchain.clone().unwrap();
        assert_eq!(toolchain.version, "5.6.3");
        assert_eq!(
            toolchain.location,
            Some(repo.root.join("node_modules/typescript"))
        );
        let launch = configure_with(&repo.root, &model, &installed, &cache.root);
        assert_eq!(
            launch.initialization_options.unwrap()["tsserver"]["path"],
            repo.root
                .join("node_modules/typescript/lib/tsserver.js")
                .display()
                .to_string(),
            "tsserver is the TypeScript the lock pins"
        );
    }

    #[test]
    fn a_repository_without_a_workspace_loads_no_plugin() {
        let repo = Fixture::new("ts-plain");
        let cache = Fixture::new("ts-plain-cache");
        repo.write("package.json", "{\"name\": \"plain\"}");
        let launch = launch_with(&repo.root, &cache.root);
        assert!(launch.env.is_empty());
        assert!(launch
            .initialization_options
            .unwrap()
            .get("plugins")
            .is_none());
        assert!(std::fs::read_dir(&cache.root).unwrap().next().is_none());
    }

    #[test]
    fn the_memory_ceiling_is_a_quarter_of_memory_within_bounds() {
        const GB: u64 = 1024 * 1024 * 1024;
        assert_eq!(memory_ceiling_mb(Some(128 * GB)), 16384);
        assert_eq!(memory_ceiling_mb(Some(32 * GB)), 8192);
        assert_eq!(memory_ceiling_mb(Some(16 * GB)), 4096);
        assert_eq!(memory_ceiling_mb(Some(8 * GB)), 3072);
        assert_eq!(memory_ceiling_mb(None), 4096);
    }
}
