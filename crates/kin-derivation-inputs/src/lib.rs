// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Build-time keys for the data Kin derives and stores.
//!
//! Kin keeps data it derives from source: the entities and relations it parses
//! and links, and records built from them. A store one build made is current
//! for another build only when both would derive the same data. Each derived
//! product therefore carries a key, and the key has to move exactly when code
//! that decides the product moves. Hand-kept version numbers did not do that:
//! they were missed, raised late, and taken twice by changes made side by side.
//!
//! The key here is taken over the code itself.
//!
//! ## Classification
//!
//! Every crate whose sources can decide a product carries a
//! [`CLASSIFICATION_FILE`] beside its manifest. It names every file under the
//! roots it declares, and every dependency its manifest lists. Each one is
//! either deciding named [`PRODUCTS`], or excluded with a one-line reason:
//!
//! ```toml
//! roots = ["src"]
//!
//! [[decides]]
//! products = ["graph"]
//! files = ["src/lib.rs", "src/parse.rs"]
//!
//! [[excluded]]
//! reason = "tests"
//! files = ["src/parse/tests.rs"]
//!
//! [dependencies]
//! kin-model = { decides = ["graph"] }
//! serde = { reason = "serialization; the record format carries its own schema" }
//! ```
//!
//! A file under a root in neither list, in both, or listed twice fails the
//! build, as does a listed file that is gone, an exclusion without a reason, a
//! dependency the manifest lists that is not classified, and a classified
//! dependency the manifest does not list. So a new file or dependency forces
//! the decision instead of defaulting either way. When in doubt, a file
//! decides: leaving out a file that decides lets stale data read as current
//! with no sign, while keeping one that does not only costs a re-derivation.
//!
//! ## Composition
//!
//! A composer is the crate that owns a product's record. Its build script calls
//! [`build_script`], which starts at the composer's own classification and
//! follows every workspace dependency classified as deciding the product,
//! through the dependency's own classification, as far as the chain goes. The
//! key covers every deciding file it reaches, and every registry dependency
//! classified as deciding, by the name, version, source and checksum
//! `Cargo.lock` records. A grammar release therefore moves the key as surely
//! as an edit to the code that reads its trees.
//!
//! A deciding Rust file is read as its tokens, without comments or the
//! whitespace between them, and without its items under `#[cfg(test)]` (see
//! [`kept`]): a comment, a formatting pass or a test changes nothing Kin
//! derives, so none of them moves a key. String literals stay whole. Other
//! files are taken byte for byte.
//!
//! A deciding file can hold items no derivation runs, such as a hook only a
//! reader calls. A classification may name them to leave out of the key:
//!
//! ```toml
//! [[excluded_items]]
//! file = "src/parse.rs"
//! reason = "the name syntax a reader quotes; nothing at ingest calls it"
//! items = ["impl Adapter for Python / fn name_syntax", "fn quote_name"]
//! ```
//!
//! Everything a classification does not name is kept, so a new function
//! decides until someone shows it does not. The build fails when a named
//! item is gone, and when any code that decides the same product names an
//! excluded item, its methods or the members of an excluded block: a
//! derivation could then reach it. That check reads identifiers, not calls,
//! so it errs toward failing, and what it finds is either left out too or
//! kept.
//!
//! The composer must build only inside its workspace, because it reads its
//! neighbours by relative path. It is downstream of everything it reads, so
//! reading them adds no rebuild: an edit to any of them rebuilds the composer
//! anyway. Its build watches only what it reads (the classifications, the
//! manifests, `Cargo.lock` and the deciding files), never a whole directory,
//! so an edit to a test or an excluded file reruns nothing downstream. A new
//! file nobody classified is therefore found when the build next runs the
//! check: on a clean build, as every CI build is, or after the next edit to
//! anything watched.
//!
//! ## Changes made side by side
//!
//! A classification does not change when a file it names changes. Two changes
//! to different deciding code, made from one base, each move the key, and
//! their merge moves it to a third value nobody chose. There is no number to
//! collide on, and a store either change derived alone reads as derived by
//! another build under the merge.

mod items;
mod rust_items;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

pub use items::{implemented_trait, items, kept, Kept, RustItem};
pub use rust_items::without_tests;

/// The classification a crate carries beside its manifest.
pub const CLASSIFICATION_FILE: &str = "derivation-inputs.toml";

/// Every product a classification may name.
///
/// `graph` is the entities and relations a store serves, as parsing, linking
/// and history replay derive them from source.
pub const PRODUCTS: &[&str] = &["graph"];

/// The domain of every key, so a change to what a key covers reads as a
/// different key rather than a colliding one.
const DOMAIN: &str = "kin.derivation-inputs.v1";

/// Names a root may hold that are nobody's source: the desktop's own litter.
const IGNORED_NAMES: &[&str] = &[".DS_Store"];

/// One input a key covers: a file by its path from the workspace root, or a
/// registry dependency by the manifest that names it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Input {
    pub name: String,
    pub digest: String,
}

/// The keys a composer's build takes, the inputs behind each, and every path
/// they were read from.
#[derive(Debug, Clone, Default)]
pub struct Composition {
    keys: BTreeMap<String, String>,
    inputs: BTreeMap<String, Vec<Input>>,
    watched: BTreeSet<PathBuf>,
    /// Each product's deciding Rust inputs: name, path and the items its
    /// classification leaves out.
    rust_inputs: BTreeMap<String, Vec<(String, PathBuf, Vec<String>)>>,
}

impl Composition {
    /// The key of `product`, as `sha256:<hex>`.
    pub fn key(&self, product: &str) -> Option<&str> {
        self.keys.get(product).map(String::as_str)
    }

    /// The inputs `product`'s key covers, sorted by name.
    pub fn inputs(&self, product: &str) -> &[Input] {
        self.inputs.get(product).map_or(&[], Vec::as_slice)
    }

    /// Every path the keys were read from, so a build script can watch each.
    pub fn watched(&self) -> impl Iterator<Item = &Path> {
        self.watched.iter().map(PathBuf::as_path)
    }

    /// The Rust source a composer includes: one constant per product holding
    /// its key, and one listing the inputs behind it.
    pub fn generated_source(&self) -> String {
        let mut source = String::from(
            "// Generated by kin-derivation-inputs from the derivation-inputs.toml files\n\
             // this crate's build reached. Do not edit.\n",
        );
        for (product, key) in &self.keys {
            let constant = constant_name(product);
            let _ = writeln!(
                source,
                "\n/// The key of the `{product}` product.\npub const {constant}: &str = {key:?};"
            );
            let _ = writeln!(
                source,
                "\n/// Each input the `{product}` key covers, with its digest.\n\
                 pub const {constant}_INPUTS: &[(&str, &str)] = &["
            );
            for input in self.inputs(product) {
                let _ = writeln!(source, "    ({:?}, {:?}),", input.name, input.digest);
            }
            source.push_str("];\n");
        }
        source
    }

    /// Tell Cargo what to watch and write [`Composition::generated_source`] to
    /// `OUT_DIR/derivation_inputs.rs`. For a build script only.
    pub fn emit(&self) {
        for path in self.watched() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
        let out_dir = std::env::var_os("OUT_DIR").expect("OUT_DIR is set for a build script");
        let out = PathBuf::from(out_dir).join("derivation_inputs.rs");
        fs::write(&out, self.generated_source())
            .unwrap_or_else(|error| panic!("write {}: {error}", out.display()));
    }
}

/// What a composer's build script runs: compose `products` from the crate
/// being built and emit them, or fail the build naming every problem.
///
/// The crate includes the result with
/// `include!(concat!(env!("OUT_DIR"), "/derivation_inputs.rs"))`.
pub fn build_script(products: &[&str]) {
    let manifest_dir = PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set for a build"),
    );
    match compose(&manifest_dir, products) {
        Ok(composition) => composition.emit(),
        Err(problems) => {
            for problem in &problems {
                println!("cargo:warning={problem}");
            }
            panic!(
                "the derivation keys cannot be taken; see kin-derivation-inputs: {}",
                problems.join("; ")
            );
        }
    }
}

/// Compose the keys of `products` from the crate at `manifest_dir`, or every
/// reason they cannot be taken.
pub fn compose(manifest_dir: &Path, products: &[&str]) -> Result<Composition, Vec<String>> {
    let mut problems = Vec::new();
    for product in products {
        if !PRODUCTS.contains(product) {
            problems.push(format!(
                "`{product}` is not a product kin-derivation-inputs knows; add it to PRODUCTS \
                 first"
            ));
        }
    }
    let Some(root) = workspace_root(manifest_dir) else {
        problems.push(format!(
            "{} is inside no Cargo workspace, and a composer reads its neighbours through one",
            manifest_dir.display()
        ));
        return Err(problems);
    };
    let workspace = match read_toml(&root.join("Cargo.toml")) {
        Ok(manifest) => manifest,
        Err(problem) => {
            problems.push(problem);
            return Err(problems);
        }
    };
    let lock = match Lockfile::read(&root.join("Cargo.lock")) {
        Ok(lock) => lock,
        Err(problem) => {
            problems.push(problem);
            return Err(problems);
        }
    };

    let mut composition = Composition::default();
    composition.watched.insert(root.join("Cargo.toml"));
    composition.watched.insert(root.join("Cargo.lock"));
    let mut loaded: BTreeMap<PathBuf, Option<Crate>> = BTreeMap::new();

    for product in products {
        let mut inputs = Vec::new();
        // The identifiers each deciding Rust input keeps, and the items left
        // out of each, for the check that nothing deciding names one.
        let mut identifiers: Vec<(String, BTreeSet<String>)> = Vec::new();
        let mut left_out: Vec<(String, String, Vec<String>)> = Vec::new();
        let mut traits: BTreeSet<String> = BTreeSet::new();
        let mut seen = BTreeSet::new();
        let mut queue = vec![normalize(manifest_dir)];
        while let Some(directory) = queue.pop() {
            if !seen.insert(directory.clone()) {
                continue;
            }
            let entry = loaded.entry(directory.clone()).or_insert_with(|| {
                match Crate::load(&root, &directory, &workspace, &mut composition.watched) {
                    Ok(krate) => Some(krate),
                    Err(mut found) => {
                        problems.append(&mut found);
                        None
                    }
                }
            });
            let Some(krate) = entry.as_ref() else {
                continue;
            };
            for (file, class) in &krate.classification.files {
                if class.decides(product) {
                    let name = format!("{}/{file}", krate.label);
                    let path = krate.directory.join(file);
                    composition.watched.insert(path.clone());
                    let exclude = krate
                        .classification
                        .excluded_items
                        .get(file)
                        .map_or(&[][..], Vec::as_slice);
                    match file_digest(&name, &path, exclude) {
                        Ok((digest, kept)) => {
                            if let Some(kept) = kept {
                                composition
                                    .rust_inputs
                                    .entry((*product).to_string())
                                    .or_default()
                                    .push((name.clone(), path.clone(), exclude.to_vec()));
                                traits.extend(kept.traits);
                                for (item, names) in kept.excluded {
                                    left_out.push((name.clone(), item, names));
                                }
                                identifiers.push((name.clone(), kept.identifiers));
                            }
                            inputs.push(Input { name, digest });
                        }
                        Err(problem) => problems.push(problem),
                    }
                }
            }
            for (dependency, class) in &krate.classification.dependencies {
                if !class.decides(product) {
                    continue;
                }
                let Some(spec) = krate.dependencies.get(dependency) else {
                    continue;
                };
                match &spec.path {
                    Some(path) => queue.push(path.clone()),
                    None => match lock.resolve(&krate.package, &spec.package) {
                        Ok(entries) => inputs.push(Input {
                            name: format!("{}/Cargo.toml {dependency}", krate.label),
                            digest: sha256_of(entries.join("\n").as_bytes()),
                        }),
                        Err(problem) => problems.push(format!("{}: {problem}", krate.label)),
                    },
                }
            }
        }
        let excluded_paths: BTreeSet<&str> =
            left_out.iter().map(|(_, item, _)| item.as_str()).collect();
        for (file, item, _) in &left_out {
            let segments: Vec<&str> = item.split(" / ").collect();
            for (depth, segment) in segments.iter().enumerate() {
                let Some(implemented) = items::implemented_trait(segment) else {
                    continue;
                };
                if depth + 1 == segments.len() {
                    problems.push(format!(
                        "{file} leaves out `{item}`, an implementation of `{implemented}`, whose \
                         methods a derivation can run without naming them; leave out its members \
                         one by one instead"
                    ));
                } else if !traits.contains(implemented) {
                    problems.push(format!(
                        "{file} leaves out `{item}`, a member of an implementation of \
                         `{implemented}`, which no code that decides `{product}` declares, so a \
                         derivation can run it without naming it (through a sort, a format or a \
                         serializer); keep it"
                    ));
                } else {
                    let member = segments[depth + 1..].join(" / ");
                    let declared = format!("trait {implemented} / {member}");
                    if !excluded_paths.contains(declared.as_str()) {
                        problems.push(format!(
                            "{file} leaves out `{item}`, but not `{declared}`, so a derivation \
                             calling the trait method could run it unnamed; leave out the \
                             trait's member too, or keep this one"
                        ));
                    }
                }
            }
        }
        for (file, item, names) in &left_out {
            for name in names {
                let naming: Vec<&str> = identifiers
                    .iter()
                    .filter(|(_, kept)| kept.contains(name))
                    .map(|(input, _)| input.as_str())
                    .collect();
                if !naming.is_empty() {
                    problems.push(format!(
                        "{file} leaves `{item}` out of the `{product}` key, and code that decides \
                         it names `{name}` ({}), so a derivation may run it: leave those uses out \
                         too if no derivation reaches them, otherwise keep the item",
                        naming.join(", ")
                    ));
                }
            }
        }
        inputs.sort();
        composition
            .keys
            .insert((*product).to_string(), key_of(product, &inputs));
        composition.inputs.insert((*product).to_string(), inputs);
    }

    problems.sort();
    problems.dedup();
    if problems.is_empty() {
        Ok(composition)
    } else {
        Err(problems)
    }
}

/// Functions of `product`'s deciding Rust inputs that no kept code deciding
/// it names, by input, as a classification would list them to leave out.
///
/// A candidate is a top-level function, a function of an inherent `impl` or
/// of an inline `mod`, or a trait's method together with every
/// implementation of it the deciding inputs hold. Leaving one out can stop
/// another being named, so this repeats until nothing more leaves. It reads
/// identifiers, as the build's own check does, and proposes only what that
/// check would accept; a person still writes each reason.
pub fn suggest_exclusions(
    manifest_dir: &Path,
    product: &str,
) -> Result<BTreeMap<String, Vec<String>>, Vec<String>> {
    let composition = compose(manifest_dir, &[product])?;
    let inputs = composition
        .rust_inputs
        .get(product)
        .cloned()
        .unwrap_or_default();
    let mut sources = Vec::new();
    for (name, path, exclude) in &inputs {
        let text = fs::read_to_string(path).map_err(|error| vec![format!("{name}: {error}")])?;
        let found = items::items(&text).map_err(|reason| vec![format!("{name} {reason}")])?;
        sources.push((name.clone(), text, exclude.clone(), found));
    }
    // A unit leaves together: (input index, path) members, and the name that keeps it.
    let mut units: Vec<(Vec<(usize, String)>, String)> = Vec::new();
    let mut trait_groups: BTreeMap<(String, String), Vec<(usize, String)>> = BTreeMap::new();
    let mut declared_traits = BTreeSet::new();
    for (index, (_, _, exclude, found)) in sources.iter().enumerate() {
        let mut seen: BTreeMap<String, usize> = BTreeMap::new();
        for item in found {
            *seen.entry(item.path.clone()).or_default() += 1;
            for member in &item.members {
                *seen.entry(member.path.clone()).or_default() += 1;
            }
        }
        let unique = |path: &str| seen.get(path) == Some(&1) && !exclude.iter().any(|e| e == path);
        for item in found {
            if item.is_test_only() {
                continue;
            }
            let own = item.path.as_str();
            if own.starts_with("fn ") && unique(own) {
                units.push((
                    vec![(index, own.to_string())],
                    item.name.clone().unwrap_or_default(),
                ));
            } else if let Some(name) = own.strip_prefix("trait ") {
                declared_traits.insert(name.to_string());
                for member in &item.members {
                    if member.path.contains(" / fn ")
                        && !member.is_test_only()
                        && unique(&member.path)
                    {
                        let method = member.name.clone().unwrap_or_default();
                        trait_groups
                            .entry((name.to_string(), method))
                            .or_default()
                            .push((index, member.path.clone()));
                    }
                }
            } else if own.starts_with("impl ") || own.starts_with("mod ") {
                let implemented = items::implemented_trait(own).map(str::to_string);
                for member in &item.members {
                    if !member.path.contains(" / fn ")
                        || member.is_test_only()
                        || !unique(&member.path)
                    {
                        continue;
                    }
                    let method = member.name.clone().unwrap_or_default();
                    match &implemented {
                        Some(trait_name) => trait_groups
                            .entry((trait_name.clone(), method))
                            .or_default()
                            .push((index, member.path.clone())),
                        None => units.push((vec![(index, member.path.clone())], method)),
                    }
                }
            }
        }
    }
    for ((trait_name, method), members) in trait_groups {
        let declared = format!("trait {trait_name} / fn {method}");
        if declared_traits.contains(&trait_name)
            && members.iter().any(|(_, path)| *path == declared)
        {
            units.push((members, method));
        }
    }
    loop {
        let mut excluded: Vec<Vec<String>> = sources.iter().map(|(_, _, e, _)| e.clone()).collect();
        for (members, _) in &units {
            for (index, path) in members {
                excluded[*index].push(path.clone());
            }
        }
        let mut named = BTreeSet::new();
        for (index, (name, text, _, _)) in sources.iter().enumerate() {
            let kept = items::kept(text, &excluded[index])
                .map_err(|reason| vec![format!("{name} {reason}")])?;
            named.extend(kept.identifiers);
        }
        let before = units.len();
        units.retain(|(_, name)| !named.contains(name));
        if units.len() == before {
            break;
        }
    }
    let mut suggested: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (members, _) in units {
        for (index, path) in members {
            suggested
                .entry(sources[index].0.clone())
                .or_default()
                .push(path);
        }
    }
    for paths in suggested.values_mut() {
        paths.sort();
    }
    Ok(suggested)
}

/// The key over `inputs`, which are sorted by name.
fn key_of(product: &str, inputs: &[Input]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(DOMAIN.as_bytes());
    hasher.update(b"\0");
    hasher.update(product.as_bytes());
    hasher.update(b"\0");
    for input in inputs {
        hasher.update(input.name.as_bytes());
        hasher.update(b"\0");
        hasher.update(input.digest.as_bytes());
        hasher.update(b"\0");
    }
    format!("sha256:{}", hex(&hasher.finalize()))
}

/// The digest of one deciding file, and for a Rust file what it kept: its
/// tokens without comments, test items or the items `exclude` names. Any other
/// file is taken byte for byte.
fn file_digest(
    name: &str,
    path: &Path,
    exclude: &[String],
) -> Result<(String, Option<Kept>), String> {
    let bytes = fs::read(path).map_err(|error| format!("{name} could not be read: {error}"))?;
    if !name.ends_with(".rs") {
        return Ok((sha256_of(&bytes), None));
    }
    let text = String::from_utf8(bytes).map_err(|_| format!("{name} is not UTF-8"))?;
    let kept = items::kept(&text, exclude).map_err(|reason| format!("{name} {reason}"))?;
    Ok((sha256_of(kept.text.as_bytes()), Some(kept)))
}

fn sha256_of(bytes: &[u8]) -> String {
    format!("sha256:{}", hex(&Sha256::digest(bytes)))
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(64), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

fn constant_name(product: &str) -> String {
    product
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// The nearest directory at or above `start` whose manifest declares a
/// workspace.
fn workspace_root(start: &Path) -> Option<PathBuf> {
    let start = normalize(start);
    start
        .ancestors()
        .find(|directory| {
            fs::read_to_string(directory.join("Cargo.toml"))
                .ok()
                .and_then(|text| text.parse::<toml::Table>().ok())
                .is_some_and(|manifest| manifest.contains_key("workspace"))
        })
        .map(Path::to_path_buf)
}

/// `path` made absolute with `.` and `..` resolved by name, so one crate
/// reached by two relative paths is one crate.
fn normalize(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|current| current.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    let mut normal = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normal.pop();
            }
            other => normal.push(other),
        }
    }
    normal
}

fn read_toml(path: &Path) -> Result<toml::Table, String> {
    let text = fs::read_to_string(path)
        .map_err(|error| format!("{} could not be read: {error}", path.display()))?;
    text.parse::<toml::Table>()
        .map_err(|error| format!("{} is not valid TOML: {error}", path.display()))
}

/// What a classification says about one file or dependency.
#[derive(Debug, Clone)]
enum Class {
    Decides(BTreeSet<String>),
    Excluded,
}

impl Class {
    fn decides(&self, product: &str) -> bool {
        matches!(self, Self::Decides(products) if products.contains(product))
    }
}

/// A crate's [`CLASSIFICATION_FILE`], parsed.
#[derive(Debug, Default)]
struct Classification {
    roots: Vec<String>,
    files: BTreeMap<String, Class>,
    /// Items of deciding Rust files left out of every key, by file.
    excluded_items: BTreeMap<String, Vec<String>>,
    dependencies: BTreeMap<String, Class>,
}

impl Classification {
    /// Parse `text`, naming problems against `label`.
    fn parse(label: &str, text: &str) -> Result<Self, Vec<String>> {
        let at = format!("{label}/{CLASSIFICATION_FILE}");
        let table = text
            .parse::<toml::Table>()
            .map_err(|error| vec![format!("{at} is not valid TOML: {error}")])?;
        let mut problems = Vec::new();
        let mut classification = Self::default();
        for key in table.keys() {
            if ![
                "roots",
                "decides",
                "excluded",
                "excluded_items",
                "dependencies",
            ]
            .contains(&key.as_str())
            {
                problems.push(format!(
                    "{at} has `{key}`, which is none of roots, decides, excluded, excluded_items \
                     or dependencies"
                ));
            }
        }
        match table.get("roots").map(strings) {
            Some(Some(roots)) if !roots.is_empty() => {
                for root in &roots {
                    if !is_relative_path(root) {
                        problems.push(format!(
                            "{at}: root `{root}` is not a plain path inside the crate"
                        ));
                    }
                }
                classification.roots = roots;
            }
            _ => problems.push(format!(
                "{at} needs `roots`, the directories and files whose every file it classifies"
            )),
        }
        for (group_key, deciding) in [("decides", true), ("excluded", false)] {
            let Some(groups) = table.get(group_key) else {
                continue;
            };
            let Some(groups) = groups.as_array() else {
                problems.push(format!("{at}: `{group_key}` must be an array of tables"));
                continue;
            };
            for (index, group) in groups.iter().enumerate() {
                let here = format!("{at}: {group_key}[{index}]");
                let Some(group) = group.as_table() else {
                    problems.push(format!("{here} must be a table"));
                    continue;
                };
                let detail = if deciding { "products" } else { "reason" };
                for key in group.keys() {
                    if key != "files" && key != detail {
                        problems.push(format!(
                            "{here} has `{key}`; it takes `files` and `{detail}`"
                        ));
                    }
                }
                let class = if deciding {
                    match group.get("products").map(strings) {
                        Some(Some(products)) if !products.is_empty() => {
                            product_class(&here, products, &mut problems)
                        }
                        _ => {
                            problems.push(format!("{here} needs `products`, a list of products"));
                            continue;
                        }
                    }
                } else {
                    match group.get("reason").and_then(toml::Value::as_str) {
                        Some(reason) if !reason.trim().is_empty() => Class::Excluded,
                        _ => {
                            problems.push(format!(
                                "{here} needs a `reason` saying why its files decide nothing"
                            ));
                            continue;
                        }
                    }
                };
                let Some(Some(files)) = group.get("files").map(strings) else {
                    problems.push(format!("{here} needs `files`, a list of paths"));
                    continue;
                };
                for file in files {
                    if !is_relative_path(&file) {
                        problems.push(format!(
                            "{here}: `{file}` is not a plain path inside the crate"
                        ));
                    } else if classification
                        .files
                        .insert(file.clone(), class.clone())
                        .is_some()
                    {
                        problems.push(format!(
                            "{at} lists {file} more than once; each file belongs in exactly one \
                             group"
                        ));
                    }
                }
            }
        }
        if let Some(groups) = table.get("excluded_items") {
            match groups.as_array() {
                Some(groups) => {
                    for (index, group) in groups.iter().enumerate() {
                        let here = format!("{at}: excluded_items[{index}]");
                        let Some(group) = group.as_table() else {
                            problems.push(format!("{here} must be a table"));
                            continue;
                        };
                        for key in group.keys() {
                            if !["file", "reason", "items"].contains(&key.as_str()) {
                                problems.push(format!(
                                    "{here} has `{key}`; it takes `file`, `reason` and `items`"
                                ));
                            }
                        }
                        let file = group.get("file").and_then(toml::Value::as_str);
                        let reason = group.get("reason").and_then(toml::Value::as_str);
                        let items = group.get("items").and_then(strings);
                        match (file, reason, items) {
                            (Some(file), Some(reason), Some(items))
                                if is_relative_path(file)
                                    && file.ends_with(".rs")
                                    && !reason.trim().is_empty()
                                    && !items.is_empty() =>
                            {
                                let listed = classification
                                    .excluded_items
                                    .entry(file.to_string())
                                    .or_default();
                                for item in items {
                                    if listed.contains(&item) {
                                        problems.push(format!(
                                            "{at} excludes `{item}` in {file} twice"
                                        ));
                                    } else {
                                        listed.push(item);
                                    }
                                }
                            }
                            _ => problems.push(format!(
                                "{here} needs `file`, a Rust file in the crate, a `reason` saying \
                                 why its items decide nothing, and `items`, the paths of the items"
                            )),
                        }
                    }
                }
                None => problems.push(format!("{at}: `excluded_items` must be an array of tables")),
            }
        }
        if let Some(dependencies) = table.get("dependencies") {
            match dependencies.as_table() {
                Some(dependencies) => {
                    for (name, value) in dependencies {
                        let here = format!("{at}: dependency `{name}`");
                        let Some(entry) = value.as_table() else {
                            problems.push(format!(
                                "{here} must be `{{ decides = [...] }}` or `{{ reason = \"...\" }}`"
                            ));
                            continue;
                        };
                        let class = match (entry.get("decides"), entry.get("reason")) {
                            (Some(products), None) if entry.len() == 1 => match strings(products) {
                                Some(products) if !products.is_empty() => {
                                    product_class(&here, products, &mut problems)
                                }
                                _ => {
                                    problems.push(format!(
                                        "{here}: `decides` must list at least one product"
                                    ));
                                    continue;
                                }
                            },
                            (None, Some(reason)) if entry.len() == 1 => {
                                if reason
                                    .as_str()
                                    .is_some_and(|reason| !reason.trim().is_empty())
                                {
                                    Class::Excluded
                                } else {
                                    problems.push(format!("{here}: `reason` must say why"));
                                    continue;
                                }
                            }
                            _ => {
                                problems.push(format!(
                                    "{here} must carry exactly one of `decides` or `reason`"
                                ));
                                continue;
                            }
                        };
                        classification.dependencies.insert(name.clone(), class);
                    }
                }
                None => problems.push(format!("{at}: `dependencies` must be a table")),
            }
        }
        if problems.is_empty() {
            Ok(classification)
        } else {
            Err(problems)
        }
    }
}

fn product_class(here: &str, products: Vec<String>, problems: &mut Vec<String>) -> Class {
    for product in &products {
        if !PRODUCTS.contains(&product.as_str()) {
            problems.push(format!(
                "{here} names `{product}`, which is not a product; the products are {}",
                PRODUCTS.join(", ")
            ));
        }
    }
    Class::Decides(products.into_iter().collect())
}

fn strings(value: &toml::Value) -> Option<Vec<String>> {
    value
        .as_array()?
        .iter()
        .map(|item| item.as_str().map(str::to_string))
        .collect()
}

/// A path written with `/`, inside the crate, with no `.` or `..` part.
fn is_relative_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\\')
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

/// One dependency a crate's manifest lists.
#[derive(Debug)]
struct DependencySpec {
    /// The package name `Cargo.lock` knows it by.
    package: String,
    /// Its directory when it is a crate of this workspace.
    path: Option<PathBuf>,
}

/// A crate the composition reached: its classification, checked against its
/// files and its manifest.
#[derive(Debug)]
struct Crate {
    directory: PathBuf,
    /// Its directory from the workspace root, with `/`.
    label: String,
    package: String,
    dependencies: BTreeMap<String, DependencySpec>,
    classification: Classification,
}

impl Crate {
    fn load(
        root: &Path,
        directory: &Path,
        workspace: &toml::Table,
        watched: &mut BTreeSet<PathBuf>,
    ) -> Result<Self, Vec<String>> {
        let label = directory
            .strip_prefix(root)
            .map(|relative| relative.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|_| directory.display().to_string());
        let label = if label.is_empty() {
            ".".to_string()
        } else {
            label
        };
        watched.insert(directory.join("Cargo.toml"));
        let classification_path = directory.join(CLASSIFICATION_FILE);
        watched.insert(classification_path.clone());

        let manifest = read_toml(&directory.join("Cargo.toml")).map_err(|problem| vec![problem])?;
        let package = manifest
            .get("package")
            .and_then(|package| package.get("name"))
            .and_then(toml::Value::as_str)
            .ok_or_else(|| vec![format!("{label}/Cargo.toml names no package")])?
            .to_string();
        let text = fs::read_to_string(&classification_path).map_err(|_| {
            vec![format!(
                "{label} has no {CLASSIFICATION_FILE}, and something it is reached through \
                 classifies it as deciding; add one naming every file under its sources and \
                 every dependency"
            )]
        })?;
        let classification = Classification::parse(&label, &text)?;
        let dependencies = manifest_dependencies(directory, &manifest, root, workspace);
        let mut problems = Vec::new();

        let at = format!("{label}/{CLASSIFICATION_FILE}");
        let mut present = BTreeSet::new();
        for root_entry in &classification.roots {
            let path = directory.join(root_entry);
            if path.is_dir() {
                collect(directory, &path, &mut present);
            } else if path.is_file() {
                present.insert(root_entry.clone());
            } else {
                problems.push(format!("{at}: root `{root_entry}` does not exist"));
            }
        }
        for file in &present {
            if !classification.files.contains_key(file) {
                problems.push(format!(
                    "{label}/{file} is in no group of {at}: add it under `[[decides]]` if it can \
                     change what Kin derives and stores, and when unsure; otherwise under \
                     `[[excluded]]` with the reason"
                ));
            }
        }
        for file in classification.files.keys() {
            if !present.contains(file) {
                let under_a_root = classification
                    .roots
                    .iter()
                    .any(|root| file == root || file.starts_with(&format!("{root}/")));
                if under_a_root {
                    problems.push(format!("{at} lists {file}, which does not exist"));
                } else {
                    problems.push(format!(
                        "{at} lists {file}, which is under none of its roots"
                    ));
                }
            }
        }
        for name in dependencies.keys() {
            if !classification.dependencies.contains_key(name) {
                problems.push(format!(
                    "{label}/Cargo.toml depends on `{name}`, which {at} does not classify: add \
                     `{name} = {{ decides = [...] }}` if it can change what Kin derives and \
                     stores, and when unsure; otherwise `{name} = {{ reason = \"...\" }}`"
                ));
            }
        }
        for file in classification.excluded_items.keys() {
            if !matches!(classification.files.get(file), Some(Class::Decides(_))) {
                problems.push(format!(
                    "{at} excludes items of {file}, which no `[[decides]]` group lists; items are \
                     left out only of a file that decides"
                ));
            }
        }
        for name in classification.dependencies.keys() {
            if !dependencies.contains_key(name) {
                problems.push(format!(
                    "{at} classifies dependency `{name}`, which {label}/Cargo.toml does not list"
                ));
            }
        }
        if problems.is_empty() {
            Ok(Self {
                directory: directory.to_path_buf(),
                label,
                package,
                dependencies,
                classification,
            })
        } else {
            Err(problems)
        }
    }
}

/// Every dependency `manifest` lists for building the crate itself, on any
/// target. Development and build dependencies decide no derived data.
fn manifest_dependencies(
    directory: &Path,
    manifest: &toml::Table,
    root: &Path,
    workspace: &toml::Table,
) -> BTreeMap<String, DependencySpec> {
    let mut tables: Vec<&toml::Table> = Vec::new();
    if let Some(table) = manifest.get("dependencies").and_then(toml::Value::as_table) {
        tables.push(table);
    }
    if let Some(targets) = manifest.get("target").and_then(toml::Value::as_table) {
        for target in targets.values() {
            if let Some(table) = target.get("dependencies").and_then(toml::Value::as_table) {
                tables.push(table);
            }
        }
    }
    let shared = workspace
        .get("workspace")
        .and_then(|workspace| workspace.get("dependencies"))
        .and_then(toml::Value::as_table);
    let mut dependencies = BTreeMap::new();
    for table in tables {
        for (name, spec) in table {
            let inherited = spec
                .get("workspace")
                .and_then(toml::Value::as_bool)
                .unwrap_or(false);
            let (base, source) = if inherited {
                (root, shared.and_then(|shared| shared.get(name)))
            } else {
                (directory, Some(spec))
            };
            let package = spec
                .get("package")
                .or_else(|| source.and_then(|source| source.get("package")))
                .and_then(toml::Value::as_str)
                .unwrap_or(name)
                .to_string();
            let path = source
                .and_then(|source| source.get("path"))
                .and_then(toml::Value::as_str)
                .map(|path| normalize(&base.join(path)));
            dependencies.insert(name.clone(), DependencySpec { package, path });
        }
    }
    dependencies
}

/// Every file under `directory`, as its path from `crate_dir` with `/`.
fn collect(crate_dir: &Path, directory: &Path, files: &mut BTreeSet<String>) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(crate_dir, &path, files);
        } else if IGNORED_NAMES.contains(&entry.file_name().to_string_lossy().as_ref()) {
            continue;
        } else if let Ok(relative) = path.strip_prefix(crate_dir) {
            files.insert(relative.to_string_lossy().replace('\\', "/"));
        }
    }
}

/// The packages `Cargo.lock` records.
#[derive(Debug)]
struct Lockfile {
    packages: Vec<LockPackage>,
}

#[derive(Debug)]
struct LockPackage {
    name: String,
    version: String,
    source: Option<String>,
    checksum: Option<String>,
    dependencies: Vec<String>,
}

impl LockPackage {
    /// What a key takes from a registry package: everything that names the
    /// exact bytes built.
    fn identity(&self) -> String {
        format!(
            "{} {} {} {}",
            self.name,
            self.version,
            self.source.as_deref().unwrap_or("-"),
            self.checksum.as_deref().unwrap_or("-")
        )
    }
}

impl Lockfile {
    fn read(path: &Path) -> Result<Self, String> {
        let table = read_toml(path)?;
        let text = |entry: &toml::Value, key: &str| {
            entry
                .get(key)
                .and_then(toml::Value::as_str)
                .map(str::to_string)
        };
        let packages = table
            .get("package")
            .and_then(toml::Value::as_array)
            .ok_or_else(|| format!("{} records no packages", path.display()))?
            .iter()
            .map(|entry| LockPackage {
                name: text(entry, "name").unwrap_or_default(),
                version: text(entry, "version").unwrap_or_default(),
                source: text(entry, "source"),
                checksum: text(entry, "checksum"),
                dependencies: entry
                    .get("dependencies")
                    .and_then(strings)
                    .unwrap_or_default(),
            })
            .collect();
        Ok(Self { packages })
    }

    /// The exact packages `member`, a crate of this workspace, builds as
    /// `package`, as their identities.
    fn resolve(&self, member: &str, package: &str) -> Result<Vec<String>, String> {
        let member_entry = self
            .packages
            .iter()
            .find(|entry| entry.name == member && entry.source.is_none())
            .ok_or_else(|| format!("Cargo.lock records no workspace crate `{member}`"))?;
        let mut identities = Vec::new();
        for written in &member_entry.dependencies {
            let mut parts = written.splitn(3, ' ');
            if parts.next() != Some(package) {
                continue;
            }
            let version = parts.next();
            let source = parts
                .next()
                .map(|source| source.trim_start_matches('(').trim_end_matches(')'));
            let matches: Vec<&LockPackage> = self
                .packages
                .iter()
                .filter(|entry| {
                    entry.name == package
                        && version.is_none_or(|version| entry.version == version)
                        && source.is_none_or(|source| entry.source.as_deref() == Some(source))
                })
                .collect();
            match matches.as_slice() {
                [only] => identities.push(only.identity()),
                [] => return Err(format!("Cargo.lock has no package for `{written}`")),
                _ => {
                    return Err(format!(
                        "Cargo.lock has more than one package that `{written}` could name"
                    ))
                }
            }
        }
        if identities.is_empty() {
            return Err(format!(
                "Cargo.lock records no `{package}` among `{member}`'s dependencies"
            ));
        }
        identities.sort();
        Ok(identities)
    }
}
