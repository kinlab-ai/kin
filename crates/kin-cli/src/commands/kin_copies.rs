// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Every copy of Kin this machine can run, and what each one is.
//!
//! A machine has one Kin: the managed pair in `KIN_HOME/bin`, which the
//! installer and `kin update` replace. Anything else named `kin`, `kin-daemon`
//! or `kin-mcp` that a shell, an agent or a launcher can reach is found here,
//! named plainly, and paired with the uninstall its own channel provides.
//!
//! Finding and naming copies is read-only. Removal runs only a channel's own
//! uninstall (npm, cargo, Homebrew), or deletes a standalone release file a
//! person placed by hand, and only for a copy a person chose or an explicit
//! flag asked for. A development build and anything inside the managed Kin
//! home are never offered for removal, whatever they are.
//!
//! This is install boundary code: it reads directory entries, package
//! manifests and executable headers, and runs package managers when asked.

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use semver::Version;

/// The programs a Kin install puts on PATH.
pub(crate) const PROGRAMS: [&str; 3] = ["kin", "kin-daemon", "kin-mcp"];

/// The first release whose `kin` hands every command it is given to the
/// managed pair, so a copy of it on PATH cannot run a second Kin.
const FIRST_ROUTING_RELEASE: (u64, u64, u64) = (0, 8, 7);

/// The npm packages that install Kin launchers.
const NPM_PACKAGES: [&str; 2] = ["@kinlab/kin", "@kinlab/kin-mcp"];

/// What a copy is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CopyKind {
    /// The managed pair itself, in `KIN_HOME/bin`.
    Managed,
    /// A link, or the same file under another name, that is the managed pair.
    LinkToManaged,
    /// An npm launcher, which runs the managed pair rather than a Kin of its
    /// own. One older than the managed Kin can refuse to run, and launchers
    /// from before September 2026 trusted the version stamp and could
    /// reinstall their own release over a newer one.
    NpmLauncher { package: String },
    /// A Kin release from 0.8.7 on, which hands every command to the managed
    /// pair.
    RoutesToManaged,
    /// A Kin release that runs by itself, with its own daemons.
    IndependentRelease,
    /// A build from a source tree.
    DevelopmentBuild,
    /// A link whose target is gone. It names a Kin that cannot run.
    BrokenLink,
    /// Named like Kin, but nothing here says what it is.
    Unknown(String),
}

/// The command a copy's own channel removes it with.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Removal {
    /// `npm uninstall --global --prefix <prefix> <package>`.
    Npm { prefix: PathBuf, package: String },
    /// `cargo uninstall --root <root> <package>`.
    Cargo { root: PathBuf, package: String },
    /// `brew uninstall <formula>`.
    Homebrew { formula: String },
    /// A standalone release file placed by hand, removed by deleting it.
    DeleteFile { path: PathBuf },
}

impl Removal {
    /// The command, as a person would type it.
    pub(crate) fn command_line(&self) -> String {
        match self {
            Self::Npm { prefix, package } => format!(
                "npm uninstall --global --prefix {} {package}",
                shell_word(prefix)
            ),
            Self::Cargo { root, package } => {
                format!("cargo uninstall --root {} {package}", shell_word(root))
            }
            Self::Homebrew { formula } => format!("brew uninstall {formula}"),
            Self::DeleteFile { path } => format!("rm {}", shell_word(path)),
        }
    }
}

fn shell_word(path: &Path) -> String {
    let text = path.display().to_string();
    if text
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "/._-~+@:".contains(c))
    {
        text
    } else {
        format!("'{}'", text.replace('\'', r"'\''"))
    }
}

/// One file named like Kin, and what it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct KinCopy {
    /// Where it was found, as a shell would name it.
    pub(crate) path: PathBuf,
    pub(crate) program: &'static str,
    /// What it resolves to, when that is another path.
    pub(crate) target: Option<PathBuf>,
    pub(crate) kind: CopyKind,
    pub(crate) version: Option<String>,
    /// How it got here, when that is known: `cargo install kin-cli`, an npm
    /// prefix, a Homebrew formula.
    pub(crate) channel: Option<String>,
    /// Its channel's uninstall, when removing it is something Kin offers.
    pub(crate) removal: Option<Removal>,
    /// Whether a new shell runs this one for its program name.
    pub(crate) first_on_path: bool,
    /// Whether it sits inside the managed Kin home.
    pub(crate) in_kin_home: bool,
}

impl KinCopy {
    /// One plain sentence naming the copy.
    pub(crate) fn describe(&self, managed_version: Option<&str>) -> String {
        let version = self
            .version
            .as_deref()
            .map(|version| format!(" {version}"))
            .unwrap_or_default();
        let what = match &self.kind {
            CopyKind::Managed => format!("the managed Kin{version}"),
            CopyKind::LinkToManaged => "a link to the managed Kin".to_string(),
            CopyKind::NpmLauncher { package } => {
                let relation = match (self.version.as_deref(), managed_version) {
                    (Some(own), Some(managed)) if version_lt(own, managed) => {
                        format!(", older than the managed Kin {managed}")
                    }
                    _ => ", which runs the managed Kin".to_string(),
                };
                format!("npm launcher {package}{version}{relation}")
            }
            CopyKind::RoutesToManaged => {
                format!("Kin{version}, which hands its commands to the managed Kin")
            }
            CopyKind::IndependentRelease => {
                let older = match (self.version.as_deref(), managed_version) {
                    (Some(own), Some(managed)) if version_lt(own, managed) => {
                        format!(", older than the managed Kin {managed},")
                    }
                    _ => ",".to_string(),
                };
                format!("Kin{version}{older} a separate copy that runs its own daemons")
            }
            CopyKind::DevelopmentBuild => format!("a development build{version}"),
            CopyKind::BrokenLink => {
                return match &self.target {
                    Some(target) => format!(
                        "{}: a link to {}, which no longer exists",
                        display_path(&self.path),
                        display_path(target)
                    ),
                    None => format!(
                        "{}: a link to something that no longer exists",
                        display_path(&self.path)
                    ),
                };
            }
            CopyKind::Unknown(reason) => format!("unidentified ({reason})"),
        };
        let mut sentence = format!("{}: {what}", display_path(&self.path));
        if let Some(channel) = &self.channel {
            sentence.push_str(&format!(" ({channel})"));
        }
        if let Some(target) = &self.target {
            if !matches!(self.kind, CopyKind::LinkToManaged) {
                sentence.push_str(&format!(", linked to {}", display_path(target)));
            }
        }
        if self.first_on_path {
            sentence.push_str(&format!("; a new shell runs this `{}`", self.program));
        }
        sentence
    }

    /// Whether this copy is a second Kin rather than the one, or a way in to
    /// the one.
    pub(crate) fn is_concern(&self, managed_version: Option<&str>) -> bool {
        match &self.kind {
            CopyKind::Managed
            | CopyKind::LinkToManaged
            | CopyKind::RoutesToManaged
            | CopyKind::DevelopmentBuild
            | CopyKind::BrokenLink => false,
            CopyKind::NpmLauncher { .. } => {
                matches!((self.version.as_deref(), managed_version),
                    (Some(own), Some(managed)) if version_lt(own, managed))
            }
            CopyKind::IndependentRelease | CopyKind::Unknown(_) => true,
        }
    }
}

/// Everything one scan found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CopyScan {
    pub(crate) kin_home: PathBuf,
    /// The managed `kin`'s release, from its own bytes.
    pub(crate) managed_version: Option<String>,
    pub(crate) copies: Vec<KinCopy>,
}

/// One removal Kin can offer, which may cover several programs of one package.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemovalOffer {
    pub(crate) removal: Removal,
    /// What is removed, in a few words: `npm launcher @kinlab/kin 0.6.6`.
    pub(crate) what: String,
    /// The paths it removes.
    pub(crate) paths: Vec<PathBuf>,
}

impl CopyScan {
    /// Every copy that is not the managed pair itself.
    pub(crate) fn others(&self) -> impl Iterator<Item = &KinCopy> {
        self.copies
            .iter()
            .filter(|copy| !matches!(copy.kind, CopyKind::Managed))
    }

    /// The copies that are a second Kin.
    pub(crate) fn concerns(&self) -> Vec<&KinCopy> {
        self.copies
            .iter()
            .filter(|copy| copy.is_concern(self.managed_version.as_deref()))
            .collect()
    }

    /// The removals Kin can offer, one per package or file.
    pub(crate) fn removal_offers(&self) -> Vec<RemovalOffer> {
        let mut offers: Vec<RemovalOffer> = Vec::new();
        for copy in &self.copies {
            let Some(removal) = &copy.removal else {
                continue;
            };
            if let Some(offer) = offers.iter_mut().find(|offer| &offer.removal == removal) {
                if !offer.paths.contains(&copy.path) {
                    offer.paths.push(copy.path.clone());
                }
                continue;
            }
            let version = copy
                .version
                .as_deref()
                .map(|version| format!(" {version}"))
                .unwrap_or_default();
            let what = match (&copy.kind, removal) {
                (CopyKind::NpmLauncher { package }, _) => {
                    format!("npm launcher {package}{version}")
                }
                (_, Removal::Cargo { package, .. }) => format!("cargo package {package}{version}"),
                (_, Removal::Homebrew { formula }) => {
                    format!("Homebrew formula {formula}{version}")
                }
                _ => format!("Kin{version} at {}", display_path(&copy.path)),
            };
            offers.push(RemovalOffer {
                removal: removal.clone(),
                what,
                paths: vec![copy.path.clone()],
            });
        }
        offers
    }
}

/// Where a scan looks.
#[derive(Clone, Debug, Default)]
pub(crate) struct ScanRoots {
    pub(crate) kin_home: PathBuf,
    /// The directories of `PATH`, in order.
    pub(crate) path_dirs: Vec<PathBuf>,
    pub(crate) cargo_home: Option<PathBuf>,
    /// npm global prefixes beyond the ones `PATH` implies.
    pub(crate) npm_prefixes: Vec<PathBuf>,
    /// Further directories installs have used, such as `/usr/local/bin`.
    pub(crate) extra_dirs: Vec<PathBuf>,
    /// Whether an executable with no build identity may be asked for its
    /// version. Never in unit tests, where it would run whatever the host
    /// has installed.
    pub(crate) probe_versions: bool,
}

impl ScanRoots {
    /// The roots of this machine and this process's environment.
    pub(crate) fn from_environment() -> Result<Self> {
        let kin_home = crate::commands::setup::kin_dir()?;
        let home = crate::commands::setup::home_dir().ok();
        let path_dirs = std::env::var_os("PATH")
            .map(|path| std::env::split_paths(&path).collect())
            .unwrap_or_default();
        let cargo_home = std::env::var_os("CARGO_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|home| home.join(".cargo")));
        let mut npm_prefixes = Vec::new();
        for key in ["NPM_CONFIG_PREFIX", "npm_config_prefix"] {
            if let Some(value) = std::env::var_os(key).filter(|value| !value.is_empty()) {
                npm_prefixes.push(PathBuf::from(value));
            }
        }
        if let Some(home) = &home {
            npm_prefixes.push(home.join(".npm-global"));
            let nvm = std::env::var_os("NVM_DIR")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".nvm"));
            if let Ok(entries) = fs::read_dir(nvm.join("versions").join("node")) {
                let mut versions: Vec<PathBuf> =
                    entries.flatten().map(|entry| entry.path()).collect();
                versions.sort();
                npm_prefixes.extend(versions);
            }
        }
        let mut extra_dirs = Vec::new();
        if cfg!(unix) {
            for prefix in ["/opt/homebrew", "/usr/local", "/home/linuxbrew/.linuxbrew"] {
                npm_prefixes.push(PathBuf::from(prefix));
                extra_dirs.push(Path::new(prefix).join("bin"));
            }
        }
        if let Some(appdata) = std::env::var_os("APPDATA").filter(|value| !value.is_empty()) {
            npm_prefixes.push(PathBuf::from(appdata).join("npm"));
        }
        Ok(Self {
            kin_home,
            path_dirs,
            cargo_home,
            npm_prefixes,
            extra_dirs,
            probe_versions: true,
        })
    }
}

/// The file names a program can have on this platform.
fn program_file_names(program: &str) -> Vec<String> {
    if cfg!(windows) {
        ["exe", "cmd", "ps1", "bat"]
            .iter()
            .map(|extension| format!("{program}.{extension}"))
            .chain(std::iter::once(program.to_string()))
            .collect()
    } else {
        vec![program.to_string()]
    }
}

fn managed_component_name(program: &str) -> String {
    format!("{program}{}", std::env::consts::EXE_SUFFIX)
}

/// The program a file name is, if it is one of Kin's.
fn program_for_file_name(name: &OsStr) -> Option<&'static str> {
    let name = name.to_str()?;
    let stem = if cfg!(windows) {
        let lower = name.to_ascii_lowercase();
        ["exe", "cmd", "ps1", "bat"]
            .iter()
            .find_map(|extension| {
                lower
                    .strip_suffix(&format!(".{extension}"))
                    .map(str::to_string)
            })
            .unwrap_or(lower)
    } else {
        name.to_string()
    };
    PROGRAMS.iter().copied().find(|program| *program == stem)
}

fn display_path(path: &Path) -> String {
    crate::screen::home_relative(path)
}

fn version_triple(version: &str) -> Option<(u64, u64, u64)> {
    let parsed = Version::parse(version.trim().trim_start_matches('v')).ok()?;
    Some((parsed.major, parsed.minor, parsed.patch))
}

fn version_lt(left: &str, right: &str) -> bool {
    match (
        Version::parse(left.trim().trim_start_matches('v')),
        Version::parse(right.trim().trim_start_matches('v')),
    ) {
        (Ok(left), Ok(right)) => left < right,
        _ => false,
    }
}

fn routes_to_managed(version: &str) -> bool {
    version_triple(version).is_some_and(|triple| triple >= FIRST_ROUTING_RELEASE)
}

fn canonical(path: &Path) -> Option<PathBuf> {
    fs::canonicalize(path).ok()
}

/// Whether two paths name one file.
fn same_file(left: &Path, right: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match (fs::metadata(left), fs::metadata(right)) {
            (Ok(left), Ok(right)) => left.dev() == right.dev() && left.ino() == right.ino(),
            _ => false,
        }
    }
    #[cfg(not(unix))]
    {
        matches!((canonical(left), canonical(right)), (Some(left), Some(right)) if left == right)
    }
}

/// Whether `resolved` lives in a Cargo target directory, wherever
/// `CARGO_TARGET_DIR` put it: Cargo marks the root of every target directory
/// with both of these files.
fn is_development_path(resolved: &Path) -> bool {
    resolved.ancestors().skip(1).take(8).any(|directory| {
        directory.join("CACHEDIR.TAG").is_file() && directory.join(".rustc_info.json").is_file()
    })
}

/// The npm package a resolved path belongs to: its name, version and global
/// prefix.
fn npm_package_of(resolved: &Path) -> Option<(String, Option<String>, PathBuf)> {
    let mut package_dir = None;
    for ancestor in resolved.ancestors().skip(1) {
        let parent = ancestor.parent()?;
        let in_node_modules = parent.file_name() == Some(OsStr::new("node_modules"))
            || (parent
                .file_name()
                .and_then(OsStr::to_str)
                .is_some_and(|name| name.starts_with('@'))
                && parent.parent().and_then(Path::file_name) == Some(OsStr::new("node_modules")));
        if in_node_modules && ancestor.join("package.json").is_file() {
            package_dir = Some(ancestor.to_path_buf());
            break;
        }
    }
    let package_dir = package_dir?;
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(package_dir.join("package.json")).ok()?).ok()?;
    let name = manifest.get("name")?.as_str()?.to_string();
    let version = manifest
        .get("version")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let node_modules = package_dir
        .ancestors()
        .find(|ancestor| ancestor.file_name() == Some(OsStr::new("node_modules")))?;
    let above = node_modules.parent()?;
    // A Unix prefix keeps packages in `<prefix>/lib/node_modules`; Windows
    // keeps them in `<prefix>/node_modules`.
    let prefix = if above.file_name() == Some(OsStr::new("lib")) {
        above.parent()?.to_path_buf()
    } else {
        above.to_path_buf()
    };
    Some((name, version, prefix))
}

/// The npm launcher package beside a Windows command shim, which is a script
/// rather than a link into the package.
fn npm_shim_package(path: &Path) -> Option<(String, Option<String>, PathBuf)> {
    let directory = path.parent()?;
    for package in NPM_PACKAGES {
        let manifest_path = directory
            .join("node_modules")
            .join(package)
            .join("package.json");
        let Ok(bytes) = fs::read(&manifest_path) else {
            continue;
        };
        let manifest: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
        let version = manifest
            .get("version")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        return Some((package.to_string(), version, directory.to_path_buf()));
    }
    None
}

/// What `cargo install` recorded for each binary in a Cargo home: the package
/// that installed it and that package's version.
fn cargo_install_index(cargo_home: &Path) -> HashMap<String, (String, String)> {
    fn record(index: &mut HashMap<String, (String, String)>, key: &str, bins: Vec<String>) {
        let mut parts = key.splitn(3, ' ');
        let (Some(package), Some(version)) = (parts.next(), parts.next()) else {
            return;
        };
        for bin in bins {
            let bin = bin.strip_suffix(".exe").unwrap_or(&bin).to_string();
            index.insert(bin, (package.to_string(), version.to_string()));
        }
    }
    let mut index = HashMap::new();
    if let Ok(bytes) = fs::read(cargo_home.join(".crates2.json")) {
        if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) {
            if let Some(installs) = value.get("installs").and_then(serde_json::Value::as_object) {
                for (key, install) in installs {
                    let bins = install
                        .get("bins")
                        .and_then(serde_json::Value::as_array)
                        .map(|bins| {
                            bins.iter()
                                .filter_map(serde_json::Value::as_str)
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default();
                    record(&mut index, key, bins);
                }
            }
        }
    }
    if index.is_empty() {
        if let Ok(text) = fs::read_to_string(cargo_home.join(".crates.toml")) {
            if let Ok(value) = toml::from_str::<toml::Value>(&text) {
                if let Some(table) = value.get("v1").and_then(toml::Value::as_table) {
                    for (key, bins) in table {
                        let bins = bins
                            .as_array()
                            .map(|bins| {
                                bins.iter()
                                    .filter_map(toml::Value::as_str)
                                    .map(str::to_string)
                                    .collect()
                            })
                            .unwrap_or_default();
                        record(&mut index, key, bins);
                    }
                }
            }
        }
    }
    index
}

/// What every classification needs to know, computed once per scan.
struct ScanContext<'a> {
    roots: &'a ScanRoots,
    managed_bin: Option<PathBuf>,
    /// The managed `kin`'s release, read once per scan.
    managed_version: Option<String>,
    kin_home: Option<PathBuf>,
    cargo_bin: Option<PathBuf>,
    cargo_index: HashMap<String, (String, String)>,
}

impl ScanContext<'_> {
    fn in_kin_home(&self, path: &Path) -> bool {
        let resolved_parent = path.parent().and_then(canonical);
        self.kin_home
            .as_ref()
            .is_some_and(|home| resolved_parent.is_some_and(|parent| parent.starts_with(home)))
    }
}

fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink())
}

/// Classify one file named like a Kin program.
fn classify(path: &Path, program: &'static str, context: &ScanContext<'_>) -> KinCopy {
    let in_kin_home = context.in_kin_home(path);
    let mut copy = KinCopy {
        path: path.to_path_buf(),
        program,
        target: None,
        kind: CopyKind::Unknown(String::new()),
        version: None,
        channel: None,
        removal: None,
        first_on_path: false,
        in_kin_home,
    };
    let Some(resolved) = canonical(path) else {
        copy.kind = CopyKind::BrokenLink;
        copy.target = fs::read_link(path).ok();
        return copy;
    };
    if resolved != path {
        copy.target = Some(resolved.clone());
    }
    let link = is_symlink(path);

    // The managed pair, or something that is it under another name.
    if let Some(managed_bin) = &context.managed_bin {
        let managed = managed_bin.join(managed_component_name(program));
        let at_managed_location = path.parent().and_then(canonical).as_deref()
            == Some(managed_bin.as_path())
            && path.file_name() == Some(OsStr::new(&managed_component_name(program)));
        if at_managed_location && !link {
            copy.kind = CopyKind::Managed;
            copy.target = None;
            return copy;
        }
        if !at_managed_location && !is_symlink(&managed) && same_file(&resolved, &managed) {
            copy.kind = CopyKind::LinkToManaged;
            return copy;
        }
    }

    // An npm launcher, by link into its package or by a Windows shim beside it.
    if let Some((package, version, prefix)) = npm_package_of(&resolved)
        .or_else(|| npm_shim_package(path).filter(|_| cfg!(windows) && !link))
    {
        if NPM_PACKAGES.contains(&package.as_str()) {
            copy.version = version;
            copy.channel = Some(format!("npm prefix {}", display_path(&prefix)));
            copy.kind = CopyKind::NpmLauncher {
                package: package.clone(),
            };
            if !in_kin_home && copy.is_concern(context.managed_version.as_deref()) {
                copy.removal = Some(Removal::Npm { prefix, package });
            }
        } else {
            copy.kind = CopyKind::Unknown(format!("a file of the npm package {package}"));
        }
        return copy;
    }

    if is_development_path(&resolved) {
        copy.kind = CopyKind::DevelopmentBuild;
        return copy;
    }

    // A Homebrew formula: Cellar/<formula>/<version>/...
    let components: Vec<&OsStr> = resolved.iter().collect();
    if let Some(index) = components
        .iter()
        .position(|component| *component == OsStr::new("Cellar"))
    {
        if let (Some(formula), Some(version)) = (
            components.get(index + 1).and_then(|c| c.to_str()),
            components.get(index + 2).and_then(|c| c.to_str()),
        ) {
            copy.version = Some(version.to_string());
            copy.channel = Some(format!("Homebrew formula {formula}"));
            copy.kind = if routes_to_managed(version) {
                CopyKind::RoutesToManaged
            } else {
                CopyKind::IndependentRelease
            };
            if !in_kin_home {
                copy.removal = Some(Removal::Homebrew {
                    formula: formula.to_string(),
                });
            }
            return copy;
        }
    }

    // A `cargo install`, which records the package that put it there.
    let in_cargo_bin = context
        .cargo_bin
        .as_deref()
        .is_some_and(|cargo_bin| path.parent().and_then(canonical).as_deref() == Some(cargo_bin));
    if in_cargo_bin {
        if let Some((package, version)) = context.cargo_index.get(program) {
            copy.version = Some(version.clone());
            copy.channel = Some(format!("cargo install {package}"));
            copy.kind = if routes_to_managed(version) {
                CopyKind::RoutesToManaged
            } else {
                CopyKind::IndependentRelease
            };
            if let Some(cargo_home) = &context.roots.cargo_home {
                copy.removal = Some(Removal::Cargo {
                    root: cargo_home.clone(),
                    package: package.clone(),
                });
            }
            return copy;
        }
    }

    // A standalone executable: its own bytes say what it is.
    match crate::commands::update::executable_build_identity(&resolved) {
        Ok(identity) => {
            copy.version = Some(identity.version.clone());
            copy.kind = if !identity.clean || !identity.source_known {
                CopyKind::DevelopmentBuild
            } else if routes_to_managed(&identity.version) {
                CopyKind::RoutesToManaged
            } else {
                CopyKind::IndependentRelease
            };
        }
        Err(_) => match probe_version(&resolved, program, context.roots.probe_versions) {
            Some(version) => {
                copy.kind = if routes_to_managed(&version) {
                    CopyKind::RoutesToManaged
                } else {
                    CopyKind::IndependentRelease
                };
                copy.version = Some(version);
            }
            None => {
                copy.kind = CopyKind::Unknown(if starts_with_script_marker(&resolved) {
                    "a script".to_string()
                } else {
                    "it carries no Kin build identity".to_string()
                });
                return copy;
            }
        },
    }
    if in_cargo_bin {
        copy.channel = Some("in Cargo's bin directory".to_string());
    }
    if !link
        && !in_kin_home
        && matches!(
            copy.kind,
            CopyKind::IndependentRelease | CopyKind::RoutesToManaged
        )
    {
        copy.channel
            .get_or_insert_with(|| "placed by hand".to_string());
        copy.removal = Some(Removal::DeleteFile {
            path: path.to_path_buf(),
        });
    }
    copy
}

/// The managed `kin`'s release, from its own bytes.
fn managed_cli_version(managed_bin: &Path) -> Option<String> {
    crate::commands::update::executable_build_identity(
        &managed_bin.join(managed_component_name("kin")),
    )
    .ok()
    .map(|identity| identity.version)
}

fn starts_with_script_marker(path: &Path) -> bool {
    use std::io::Read;
    let mut head = [0_u8; 2];
    fs::File::open(path)
        .and_then(|mut file| file.read_exact(&mut head))
        .is_ok()
        && head == *b"#!"
}

/// Ask an executable with no build identity for its version. Only `kin`
/// answers `--version` without side effects, and only a native executable is
/// run, never a script, which could be a launcher that downloads.
fn probe_version(resolved: &Path, program: &str, allowed: bool) -> Option<String> {
    let native_name = resolved
        .extension()
        .is_none_or(|extension| extension.eq_ignore_ascii_case("exe"));
    if !allowed || program != "kin" || !native_name || starts_with_script_marker(resolved) {
        return None;
    }
    crate::commands::runtime::probe_legacy_cli_version(resolved)
        .ok()
        .map(|version| version.to_string())
}

/// Find and classify every copy under `roots`.
pub(crate) fn scan(roots: &ScanRoots) -> CopyScan {
    let kin_home = canonical(&roots.kin_home);
    let managed_bin = kin_home.as_ref().map(|home| home.join("bin"));
    let cargo_bin = roots
        .cargo_home
        .as_ref()
        .and_then(|home| canonical(&home.join("bin")));
    let managed_bin = managed_bin.filter(|bin| bin.is_dir());
    let managed_version = managed_bin.as_deref().and_then(managed_cli_version);
    let context = ScanContext {
        roots,
        managed_bin,
        managed_version: managed_version.clone(),
        kin_home,
        cargo_bin,
        cargo_index: roots
            .cargo_home
            .as_deref()
            .map(cargo_install_index)
            .unwrap_or_default(),
    };

    let mut copies: Vec<KinCopy> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut push = |copy: KinCopy, copies: &mut Vec<KinCopy>| {
        if seen.insert(copy.path.clone()) {
            copies.push(copy);
        }
    };

    // The managed home first: the pair itself, and anything else in its `bin`
    // that is or links to a Kin program.
    if let Some(bin) = &context.managed_bin {
        let mut entries: Vec<(OsString, PathBuf)> = fs::read_dir(bin)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| (entry.file_name(), entry.path()))
                    .collect()
            })
            .unwrap_or_default();
        entries.sort();
        for (name, path) in entries {
            if name.to_str().is_some_and(|name| name.starts_with('.')) {
                continue;
            }
            let program = program_for_file_name(&name).or_else(|| {
                fs::read_link(&path)
                    .ok()
                    .and_then(|target| target.file_name().and_then(program_for_file_name))
            });
            if let Some(program) = program {
                push(classify(&path, program, &context), &mut copies);
            }
        }
    }

    // Every other directory, PATH first so its order decides what a shell runs.
    let mut directories: Vec<(PathBuf, bool)> = Vec::new();
    let mut seen_directories: HashSet<PathBuf> = HashSet::new();
    if let Some(bin) = &context.managed_bin {
        seen_directories.insert(bin.clone());
    }
    let mut add_directory = |directory: PathBuf, on_path: bool| {
        if let Some(resolved) = canonical(&directory) {
            if resolved.is_dir() && seen_directories.insert(resolved) {
                directories.push((directory, on_path));
            }
        }
    };
    for directory in &roots.path_dirs {
        add_directory(directory.clone(), true);
    }
    if let Some(cargo_home) = &roots.cargo_home {
        add_directory(cargo_home.join("bin"), false);
    }
    for prefix in &roots.npm_prefixes {
        add_directory(prefix.join("bin"), false);
        if cfg!(windows) {
            add_directory(prefix.clone(), false);
        }
    }
    for directory in &roots.extra_dirs {
        add_directory(directory.clone(), false);
    }
    for (directory, _) in &directories {
        for program in PROGRAMS {
            for name in program_file_names(program) {
                let path = directory.join(&name);
                if fs::symlink_metadata(&path).is_ok() {
                    push(classify(&path, program, &context), &mut copies);
                }
            }
        }
    }

    // What a new shell runs for each program: the first match in PATH order,
    // which may be the managed pair itself.
    for program in PROGRAMS {
        let first = roots.path_dirs.iter().find_map(|directory| {
            program_file_names(program)
                .into_iter()
                .map(|name| directory.join(name))
                .find(|path| fs::symlink_metadata(path).is_ok())
        });
        let Some(first) = first else {
            continue;
        };
        let first_resolved = canonical(&first);
        if let Some(copy) = copies.iter_mut().find(|copy| {
            copy.path == first
                || (first.parent().and_then(canonical) == copy.path.parent().and_then(canonical)
                    && first.file_name() == copy.path.file_name())
        }) {
            copy.first_on_path = true;
        } else if first_resolved.is_some() {
            // Found through a directory spelled differently from the one the
            // scan recorded; it names the same file.
            if let Some(copy) = copies
                .iter_mut()
                .find(|copy| canonical(&copy.path) == first_resolved && copy.program == program)
            {
                copy.first_on_path = true;
            }
        }
    }

    CopyScan {
        kin_home: roots.kin_home.clone(),
        managed_version,
        copies,
    }
}

/// Run a removal Kin offered, then prove the paths it covers are gone.
///
/// The safety rules hold here as well as where offers are made: nothing inside
/// the managed Kin home and no development build is removed, whatever asked.
pub(crate) fn remove(offer: &RemovalOffer, roots: &ScanRoots) -> Result<()> {
    let kin_home = canonical(&roots.kin_home);
    for path in &offer.paths {
        let parent = path.parent().and_then(canonical);
        if kin_home.as_ref().is_some_and(|home| {
            parent
                .as_ref()
                .is_some_and(|parent| parent.starts_with(home))
        }) {
            anyhow::bail!(
                "{} is inside the managed Kin home, which removal never touches",
                path.display()
            );
        }
        if canonical(path).is_some_and(|resolved| is_development_path(&resolved)) {
            anyhow::bail!(
                "{} is a development build, which removal never touches",
                path.display()
            );
        }
    }
    match &offer.removal {
        Removal::DeleteFile { path } => {
            let metadata = fs::symlink_metadata(path)
                .with_context(|| format!("failed to inspect {}", path.display()))?;
            if !metadata.is_file() {
                anyhow::bail!("{} is no longer a regular file", path.display());
            }
            fs::remove_file(path)
                .with_context(|| format!("failed to remove {}", path.display()))?;
        }
        removal => {
            let (program, args) = removal_invocation(removal)?;
            let output = std::process::Command::new(&program)
                .args(&args)
                .stdin(std::process::Stdio::null())
                .output()
                .with_context(|| format!("failed to run `{}`", removal.command_line()))?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let tail = stderr.lines().rev().find(|line| !line.trim().is_empty());
                anyhow::bail!(
                    "`{}` exited with {}{}",
                    removal.command_line(),
                    output.status,
                    tail.map(|line| format!(": {}", line.trim()))
                        .unwrap_or_default()
                );
            }
        }
    }
    let remaining: Vec<String> = offer
        .paths
        .iter()
        .filter(|path| fs::symlink_metadata(path).is_ok())
        .map(|path| display_path(path))
        .collect();
    if !remaining.is_empty() {
        anyhow::bail!(
            "`{}` finished, but {} {} still there",
            offer.removal.command_line(),
            remaining.join(", "),
            if remaining.len() == 1 { "is" } else { "are" }
        );
    }
    Ok(())
}

/// The program and arguments a channel removal runs, preferring the tool that
/// belongs to the prefix or home it removes from.
fn removal_invocation(removal: &Removal) -> Result<(PathBuf, Vec<OsString>)> {
    let own_or_path = |own: PathBuf, name: &str| -> Result<PathBuf> {
        if own.is_file() {
            return Ok(own);
        }
        which::which(name).with_context(|| format!("`{name}` is not installed on this machine"))
    };
    match removal {
        Removal::Npm { prefix, package } => {
            let own = if cfg!(windows) {
                prefix.join("npm.cmd")
            } else {
                prefix.join("bin").join("npm")
            };
            Ok((
                own_or_path(own, "npm")?,
                vec![
                    "uninstall".into(),
                    "--global".into(),
                    "--prefix".into(),
                    prefix.into(),
                    package.into(),
                ],
            ))
        }
        Removal::Cargo { root, package } => Ok((
            own_or_path(
                root.join("bin").join(managed_component_name("cargo")),
                "cargo",
            )?,
            vec![
                "uninstall".into(),
                "--root".into(),
                root.into(),
                package.into(),
            ],
        )),
        Removal::Homebrew { formula } => Ok((
            which::which("brew").context("`brew` is not installed on this machine")?,
            vec!["uninstall".into(), formula.into()],
        )),
        Removal::DeleteFile { .. } => anyhow::bail!("a file removal runs no program"),
    }
}

/// One running `kin-daemon`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LiveDaemon {
    pub(crate) pid: u32,
    pub(crate) executable: PathBuf,
    /// The repository it serves, from its `--repo` argument.
    pub(crate) repo: Option<PathBuf>,
    /// Whether it runs as the machine's supervisor rather than for one
    /// repository.
    pub(crate) supervisor: bool,
    /// When it started, in seconds since the Unix epoch.
    pub(crate) started_at: u64,
}

/// Every running `kin-daemon` on this machine, from the process table.
pub(crate) fn live_daemons() -> Vec<LiveDaemon> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing()
            .with_exe(UpdateKind::OnlyIfNotSet)
            .with_cmd(UpdateKind::OnlyIfNotSet),
    );
    let mut daemons = Vec::new();
    for (pid, process) in system.processes() {
        if process.thread_kind().is_some() {
            continue;
        }
        let Some(executable) = process.exe() else {
            continue;
        };
        let name = executable
            .file_name()
            .and_then(OsStr::to_str)
            .map(|name| name.trim_end_matches(" (deleted)"));
        if name != Some(managed_component_name("kin-daemon").as_str()) {
            continue;
        }
        let args: Vec<String> = process
            .cmd()
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        let repo = args
            .iter()
            .position(|arg| arg == "--repo")
            .and_then(|index| args.get(index + 1))
            .map(PathBuf::from)
            .or_else(|| {
                args.iter()
                    .find_map(|arg| arg.strip_prefix("--repo="))
                    .map(PathBuf::from)
            });
        daemons.push(LiveDaemon {
            pid: pid.as_u32(),
            executable: executable.to_path_buf(),
            repo,
            supervisor: args.iter().any(|arg| arg == "--supervisor"),
            started_at: process.start_time(),
        });
    }
    daemons.sort_by_key(|daemon| daemon.pid);
    daemons
}

/// The daemons that run one release.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DaemonGroup {
    /// The release, in a few words.
    pub(crate) release: String,
    /// Whether this release is a second Kin.
    pub(crate) concern: bool,
    pub(crate) daemons: Vec<LiveDaemon>,
}

/// Group running daemons by the release their executable is.
///
/// A daemon running from the managed path but started before the file there
/// was last replaced is still running the release that was replaced, and on
/// Linux the kernel says so outright with a ` (deleted)` executable.
pub(crate) fn group_live_daemons(daemons: &[LiveDaemon], scan: &CopyScan) -> Vec<DaemonGroup> {
    let managed_daemon = scan
        .kin_home
        .join("bin")
        .join(managed_component_name("kin-daemon"));
    let managed_resolved = canonical(&managed_daemon);
    let managed_modified = fs::metadata(&managed_daemon)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|elapsed| elapsed.as_secs());
    let kin_home = canonical(&scan.kin_home);
    let mut identities: HashMap<PathBuf, Option<crate::commands::update::ExecutableBuildIdentity>> =
        HashMap::new();
    let mut groups: Vec<DaemonGroup> = Vec::new();
    for daemon in daemons {
        let text = daemon.executable.to_string_lossy();
        let deleted = text.ends_with(" (deleted)");
        let executable = PathBuf::from(text.trim_end_matches(" (deleted)"));
        let resolved = canonical(&executable).unwrap_or_else(|| executable.clone());
        let in_update_backup = kin_home.as_ref().is_some_and(|home| {
            resolved.starts_with(home)
                && resolved.iter().any(|component| {
                    component
                        .to_str()
                        .is_some_and(|name| name.starts_with(".update-backup-"))
                })
        });
        let at_managed =
            managed_resolved.as_ref() == Some(&resolved) || executable == managed_daemon;
        let (release, concern) = if in_update_backup
            || (at_managed
                && (deleted
                    || managed_modified.is_some_and(|modified| daemon.started_at < modified)))
        {
            (
                "a managed release that has since been replaced".to_string(),
                true,
            )
        } else if at_managed {
            (
                format!(
                    "the managed Kin{}",
                    scan.managed_version
                        .as_deref()
                        .map(|version| format!(" {version}"))
                        .unwrap_or_default()
                ),
                false,
            )
        } else {
            match scan.copies.iter().find(|copy| {
                canonical(&copy.path).as_ref() == Some(&resolved) || copy.path == executable
            }) {
                // A daemon is a second Kin whatever its CLI would do: only
                // the managed pair's daemon and a development build's are not.
                Some(copy) => (
                    copy.describe(scan.managed_version.as_deref()),
                    !matches!(
                        copy.kind,
                        CopyKind::Managed | CopyKind::LinkToManaged | CopyKind::DevelopmentBuild
                    ),
                ),
                // Not on any path the scan reads, such as a build copied
                // somewhere by hand: its own bytes say what it is.
                None => {
                    let identity = identities
                        .entry(resolved.clone())
                        .or_insert_with(|| {
                            crate::commands::update::executable_build_identity(&resolved).ok()
                        })
                        .clone();
                    let development = is_development_path(&resolved)
                        || identity
                            .as_ref()
                            .is_some_and(|identity| !identity.clean || !identity.source_known);
                    let release = match (&identity, development) {
                        (_, true) => format!(
                            "a development build{} at {}",
                            identity
                                .as_ref()
                                .map(|identity| format!(" {}", identity.version))
                                .unwrap_or_default(),
                            display_path(&executable)
                        ),
                        (Some(identity), false) => format!(
                            "Kin {} outside the managed home at {}",
                            identity.version,
                            display_path(&executable)
                        ),
                        // Too large to be a release, or older than the
                        // identity releases carry: say so rather than guess.
                        (None, false) => format!(
                            "an unidentified kin-daemon at {}, which carries no build \
                             identity Kin can read",
                            display_path(&executable)
                        ),
                    };
                    (release, !development)
                }
            }
        };
        match groups.iter_mut().find(|group| group.release == release) {
            Some(group) => group.daemons.push(daemon.clone()),
            None => groups.push(DaemonGroup {
                release,
                concern,
                daemons: vec![daemon.clone()],
            }),
        }
    }
    groups
}

// The fixtures are symlinks and Unix permission bits.
#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn executable(path: &Path, bytes: &[u8]) {
        use std::os::unix::fs::PermissionsExt;
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn link(target: &Path, at: &Path) {
        fs::create_dir_all(at.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(target, at).unwrap();
    }

    struct Fixture {
        _tmp: tempfile::TempDir,
        root: PathBuf,
        roots: ScanRoots,
    }

    /// A machine with one of every kind of copy: the managed pair, a link to
    /// it, an old npm launcher and a current one, a cargo install, a Homebrew
    /// formula, a standalone old release, a standalone routing release, a
    /// development build linked from the managed home, and a script.
    fn machine() -> Fixture {
        use crate::commands::update::test_binary_with_build_identity as kin_bytes;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let kin_home = root.join("home/.kin");
        executable(&kin_home.join("bin/kin"), &kin_bytes("0.8.7", true));
        executable(&kin_home.join("bin/kin-daemon"), &kin_bytes("0.8.7", true));
        fs::write(kin_home.join("bin/.kinlab-kin-version"), "0.8.7\n").unwrap();

        // A link to the managed pair, as /usr/local/bin/kin often is.
        link(&kin_home.join("bin/kin"), &root.join("usr-local/bin/kin"));

        // An npm prefix with an old launcher, and one with a current launcher.
        for (prefix, version) in [("old-node", "0.6.6"), ("new-node", "0.8.7")] {
            let package = root.join(prefix).join("lib/node_modules/@kinlab/kin");
            fs::create_dir_all(package.join("bin")).unwrap();
            fs::write(
                package.join("package.json"),
                format!(r#"{{"name":"@kinlab/kin","version":"{version}"}}"#),
            )
            .unwrap();
            executable(&package.join("bin/kin.mjs"), b"#!/usr/bin/env node\n");
            link(
                Path::new("../lib/node_modules/@kinlab/kin/bin/kin.mjs"),
                &root.join(prefix).join("bin/kin"),
            );
        }

        // A cargo install of an old release.
        let cargo_home = root.join("home/.cargo");
        executable(&cargo_home.join("bin/kin"), &kin_bytes("0.7.18", true));
        executable(
            &cargo_home.join("bin/kin-daemon"),
            &kin_bytes("0.7.18", true),
        );
        fs::write(
            cargo_home.join(".crates2.json"),
            r#"{"installs":{"kin-cli 0.7.18 (path+file:///src/kin/crates/kin-cli)":{"bins":["kin","kin-daemon"]}}}"#,
        )
        .unwrap();

        // A Homebrew formula.
        let cellar = root.join("brew/Cellar/kin/0.8.1/bin/kin");
        executable(&cellar, &kin_bytes("0.8.1", true));
        link(&cellar, &root.join("brew/bin/kin"));

        // Standalone releases placed by hand: an old one and a routing one.
        executable(&root.join("opt-old/kin"), &kin_bytes("0.6.6", true));
        executable(&root.join("opt-new/kin-mcp"), &kin_bytes("0.8.9", true));

        // A development build, linked from inside the managed home.
        let target = root.join("src/kin/target");
        fs::create_dir_all(target.join("release")).unwrap();
        fs::write(
            target.join("CACHEDIR.TAG"),
            "Signature: 8a477f597d28d172789f06886806bc55\n",
        )
        .unwrap();
        fs::write(target.join(".rustc_info.json"), "{}").unwrap();
        executable(
            &target.join("release/kin-daemon"),
            &kin_bytes("0.8.7", false),
        );
        link(
            &target.join("release/kin-daemon"),
            &kin_home.join("bin/kin-daemon.bak"),
        );
        // A link left behind after its target was deleted.
        link(
            &root.join("gone/release/kin"),
            &kin_home.join("bin/kin-real"),
        );
        // A development build on PATH too.
        executable(&target.join("release/kin"), &kin_bytes("0.8.7", false));

        // Something named kin that is none of the above.
        executable(&root.join("scripts/kin"), b"#!/bin/sh\necho hi\n");

        let roots = ScanRoots {
            kin_home,
            path_dirs: vec![
                root.join("old-node/bin"),
                root.join("usr-local/bin"),
                root.join("home/.kin/bin"),
                root.join("opt-old"),
                root.join("opt-new"),
                root.join("scripts"),
                target.join("release"),
            ],
            cargo_home: Some(cargo_home),
            npm_prefixes: vec![root.join("new-node")],
            extra_dirs: vec![root.join("brew/bin")],
            probe_versions: false,
        };
        Fixture {
            _tmp: tmp,
            root,
            roots,
        }
    }

    fn find<'a>(scan: &'a CopyScan, path: &Path) -> &'a KinCopy {
        scan.copies
            .iter()
            .find(|copy| copy.path == path)
            .unwrap_or_else(|| panic!("no copy at {}: {:#?}", path.display(), scan.copies))
    }

    #[test]
    fn every_copy_on_a_machine_is_found_and_named_by_what_it_is() {
        let fixture = machine();
        let root = &fixture.root;
        let scan = scan(&fixture.roots);
        assert_eq!(scan.managed_version.as_deref(), Some("0.8.7"));

        let managed = find(&scan, &root.join("home/.kin/bin/kin"));
        assert_eq!(managed.kind, CopyKind::Managed);
        assert!(managed.removal.is_none());
        assert_eq!(
            find(&scan, &root.join("home/.kin/bin/kin-daemon")).kind,
            CopyKind::Managed
        );

        let linked = find(&scan, &root.join("usr-local/bin/kin"));
        assert_eq!(linked.kind, CopyKind::LinkToManaged);
        assert!(linked.removal.is_none());

        let old_npm = find(&scan, &root.join("old-node/bin/kin"));
        assert_eq!(
            old_npm.kind,
            CopyKind::NpmLauncher {
                package: "@kinlab/kin".to_string()
            }
        );
        assert_eq!(old_npm.version.as_deref(), Some("0.6.6"));
        assert_eq!(
            old_npm.removal,
            Some(Removal::Npm {
                prefix: root.join("old-node"),
                package: "@kinlab/kin".to_string()
            })
        );
        assert!(
            old_npm.first_on_path,
            "PATH order puts the old launcher first"
        );

        let new_npm = find(&scan, &root.join("new-node/bin/kin"));
        assert_eq!(new_npm.version.as_deref(), Some("0.8.7"));
        assert!(
            new_npm.removal.is_none(),
            "a launcher as new as the managed Kin is a way in to it"
        );

        let cargo = find(&scan, &root.join("home/.cargo/bin/kin"));
        assert_eq!(cargo.kind, CopyKind::IndependentRelease);
        assert_eq!(cargo.version.as_deref(), Some("0.7.18"));
        assert_eq!(cargo.channel.as_deref(), Some("cargo install kin-cli"));
        assert_eq!(
            cargo.removal,
            Some(Removal::Cargo {
                root: root.join("home/.cargo"),
                package: "kin-cli".to_string()
            })
        );

        let brew = find(&scan, &root.join("brew/bin/kin"));
        assert_eq!(brew.kind, CopyKind::IndependentRelease);
        assert_eq!(brew.version.as_deref(), Some("0.8.1"));
        assert_eq!(
            brew.removal,
            Some(Removal::Homebrew {
                formula: "kin".to_string()
            })
        );

        let old_release = find(&scan, &root.join("opt-old/kin"));
        assert_eq!(old_release.kind, CopyKind::IndependentRelease);
        assert_eq!(
            old_release.removal,
            Some(Removal::DeleteFile {
                path: root.join("opt-old/kin")
            })
        );

        let routing = find(&scan, &root.join("opt-new/kin-mcp"));
        assert_eq!(routing.kind, CopyKind::RoutesToManaged);
        assert_eq!(routing.version.as_deref(), Some("0.8.9"));

        let bak = find(&scan, &root.join("home/.kin/bin/kin-daemon.bak"));
        assert_eq!(bak.kind, CopyKind::DevelopmentBuild);
        assert!(bak.in_kin_home);
        assert!(
            bak.removal.is_none(),
            "nothing in the managed home is offered"
        );

        let dev = find(&scan, &root.join("src/kin/target/release/kin"));
        assert_eq!(dev.kind, CopyKind::DevelopmentBuild);
        assert!(
            dev.removal.is_none(),
            "a development build is never offered"
        );

        let broken = find(&scan, &root.join("home/.kin/bin/kin-real"));
        assert_eq!(broken.kind, CopyKind::BrokenLink);
        assert!(broken.removal.is_none());
        assert!(!broken.is_concern(scan.managed_version.as_deref()));
        assert!(
            broken
                .describe(scan.managed_version.as_deref())
                .ends_with("gone/release/kin, which no longer exists"),
            "{}",
            broken.describe(scan.managed_version.as_deref())
        );

        let script = find(&scan, &root.join("scripts/kin"));
        assert_eq!(script.kind, CopyKind::Unknown("a script".to_string()));
        assert!(script.removal.is_none());

        let concerns: Vec<&Path> = scan
            .concerns()
            .into_iter()
            .map(|copy| copy.path.as_path())
            .collect();
        for expected in [
            root.join("old-node/bin/kin"),
            root.join("home/.cargo/bin/kin"),
            root.join("home/.cargo/bin/kin-daemon"),
            root.join("brew/bin/kin"),
            root.join("opt-old/kin"),
            root.join("scripts/kin"),
        ] {
            assert!(
                concerns.contains(&expected.as_path()),
                "{}",
                expected.display()
            );
        }
        for harmless in [
            root.join("home/.kin/bin/kin"),
            root.join("usr-local/bin/kin"),
            root.join("new-node/bin/kin"),
            root.join("opt-new/kin-mcp"),
            root.join("src/kin/target/release/kin"),
        ] {
            assert!(
                !concerns.contains(&harmless.as_path()),
                "{}",
                harmless.display()
            );
        }
    }

    #[test]
    fn one_package_is_offered_once_with_every_path_it_removes() {
        let fixture = machine();
        let scan = scan(&fixture.roots);
        let offers = scan.removal_offers();
        let cargo = offers
            .iter()
            .find(|offer| matches!(offer.removal, Removal::Cargo { .. }))
            .unwrap();
        assert_eq!(cargo.what, "cargo package kin-cli 0.7.18");
        assert_eq!(cargo.paths.len(), 2);
        assert_eq!(
            cargo.removal.command_line(),
            format!(
                "cargo uninstall --root {} kin-cli",
                fixture.root.join("home/.cargo").display()
            )
        );
        assert!(offers.iter().all(|offer| offer
            .paths
            .iter()
            .all(|path| !path.starts_with(&fixture.roots.kin_home))));
    }

    #[test]
    fn removal_refuses_the_managed_home_and_development_builds() {
        let fixture = machine();
        for path in [
            fixture.roots.kin_home.join("bin/kin"),
            fixture.root.join("src/kin/target/release/kin"),
        ] {
            let offer = RemovalOffer {
                removal: Removal::DeleteFile { path: path.clone() },
                what: "test".to_string(),
                paths: vec![path.clone()],
            };
            let error = remove(&offer, &fixture.roots).unwrap_err();
            assert!(format!("{error:#}").contains("never touches"), "{error:#}");
            assert!(path.exists(), "{} was removed", path.display());
        }
    }

    #[test]
    fn a_standalone_old_release_is_removed_by_deleting_it_and_proven_gone() {
        let fixture = machine();
        let scan = scan(&fixture.roots);
        let offer = scan
            .removal_offers()
            .into_iter()
            .find(|offer| matches!(offer.removal, Removal::DeleteFile { .. }))
            .unwrap();
        remove(&offer, &fixture.roots).unwrap();
        assert!(!fixture.root.join("opt-old/kin").exists());
        assert!(fixture.roots.kin_home.join("bin/kin").exists());
    }

    #[test]
    fn live_daemons_are_grouped_by_the_release_they_run() {
        let fixture = machine();
        let scan = scan(&fixture.roots);
        let managed = fixture.roots.kin_home.join("bin/kin-daemon");
        let modified = fs::metadata(&managed)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let daemon = |pid, executable: PathBuf, started_at| LiveDaemon {
            pid,
            executable,
            repo: Some(PathBuf::from(format!("/repo/{pid}"))),
            supervisor: false,
            started_at,
        };
        // Builds copied somewhere no scan reads are named by their own bytes.
        let copied_release = fixture.root.join("proofs/before/kin-daemon");
        executable(
            &copied_release,
            &crate::commands::update::test_binary_with_build_identity("0.8.5", true),
        );
        let copied_build = fixture.root.join("proofs/after/kin-daemon");
        executable(
            &copied_build,
            &crate::commands::update::test_binary_with_build_identity("0.8.7", false),
        );
        let groups = group_live_daemons(
            &[
                daemon(1, managed.clone(), modified + 60),
                daemon(2, managed.clone(), modified + 61),
                daemon(3, managed.clone(), modified.saturating_sub(3600)),
                daemon(4, fixture.root.join("home/.cargo/bin/kin-daemon"), modified),
                daemon(
                    5,
                    fixture.root.join("src/kin/target/release/kin-daemon"),
                    modified,
                ),
                daemon(6, copied_release.clone(), modified),
                daemon(7, copied_build.clone(), modified),
            ],
            &scan,
        );
        let current = groups
            .iter()
            .find(|group| group.release == "the managed Kin 0.8.7")
            .unwrap();
        assert_eq!(current.daemons.len(), 2);
        assert!(!current.concern);
        let replaced = groups
            .iter()
            .find(|group| group.release.contains("since been replaced"))
            .unwrap();
        assert_eq!(replaced.daemons[0].pid, 3);
        assert!(replaced.concern);
        let cargo = groups
            .iter()
            .find(|group| group.daemons.iter().any(|daemon| daemon.pid == 4))
            .unwrap();
        assert!(cargo.concern);
        assert!(cargo.release.contains("0.7.18"), "{}", cargo.release);
        let dev = groups
            .iter()
            .find(|group| group.daemons.iter().any(|daemon| daemon.pid == 5))
            .unwrap();
        assert!(!dev.concern);
        assert!(dev.release.contains("development build"), "{}", dev.release);
        let release_of = |pid: u32| {
            groups
                .iter()
                .find(|group| group.daemons.iter().any(|daemon| daemon.pid == pid))
                .unwrap()
        };
        assert!(release_of(6).concern);
        assert!(
            release_of(6)
                .release
                .starts_with("Kin 0.8.5 outside the managed home at "),
            "{}",
            release_of(6).release
        );
        assert!(!release_of(7).concern);
        assert!(
            release_of(7)
                .release
                .starts_with("a development build 0.8.7 at "),
            "{}",
            release_of(7).release
        );
    }
}
