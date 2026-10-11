// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The language conformance suite, parser layer.
//!
//! Every language Kin parses is measured here the same way. Each one has a
//! fixture under `tests/language-conformance/<language>/` at the workspace
//! root: the same small project written in that language (a helper module with
//! `scale`, a module that imports it with a documented `total` holding a nested
//! `double` and calling `scale` and a same-module `clamp`, a type `Account`
//! whose `deposit` calls `validate` through its receiver, and one framework
//! test), plus an `expect.toml` declaring the outcome Kin has today for every
//! capability below.
//!
//! A capability is measured as a list of named checks. Its status follows from
//! them: `pass` when every check holds, `partial` when some fail, `fail` when
//! all fail, and `refused` when Kin does not offer it for the language (the
//! adapter states no hook for it, or the reading declines). `n/a` is declared,
//! with a reason, for a construct the language does not have.
//!
//! One test per language and capability compares what it measures with what
//! `expect.toml` declares, status and failing checks alike, and fails when
//! they differ in either direction. A language cannot claim a capability it
//! lacks, and a change that makes one work has to flip its declaration in the
//! same commit, which is how the improvement shows in review.
//! `scripts/language-scorecard.py` renders the declarations into the scorecard
//! in `docs/language-support.md`, and CI checks that the rendering is current,
//! so the published scorecard is what these tests hold true.
//!
//! One language runs alone with
//! `cargo nextest run -p kin-parser --lib -E 'test(/language_conformance_test::go::/)'`.
//!
//! This is the parser layer: what the adapter extracts and what the shared
//! parser readings make of it. The same fixtures are driven through the agent
//! surface by the route-layer suite in kin-daemon.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use kin_model::{EntityKind, FilePathId, LanguageId, ParseState};
use tree_sitter::Tree;

use crate::adapter::LanguageAdapter;
use crate::code_placement::{after_sibling, indentation_at, place_entity_code, removal_of};
use crate::declaration_census::{take_census, DeclarationKind, REASON_NESTED_IN_CALLABLE};
use crate::extract::{ExtractedEntity, ParseOutput};
use crate::languages::{AdapterRegistry, ALL_LANGUAGE_IDS};
use crate::nested::{is_local_scope_name, local_scope_name};
use crate::syntax_reading::read_syntax;

/// One test per language and capability. The languages are every
/// `LanguageId`, which `every_language_has_cells` holds this list to, and the
/// capabilities are every one `expect.toml` may declare.
macro_rules! conformance {
    (languages: [$($module:ident => $language:ident),* $(,)?], capabilities: $capabilities:tt) => {
        const LANGUAGES: &[LanguageId] = &[$(LanguageId::$language),*];
        conformance!(@names $capabilities);
        $(conformance!(@language $module $language $capabilities);)*
    };
    (@names [$($capability:ident),* $(,)?]) => {
        const CAPABILITIES: &[&str] = &[$(stringify!($capability)),*];
    };
    (@language $module:ident $language:ident [$($capability:ident),* $(,)?]) => {
        mod $module {
            $(
                #[test]
                fn $capability() {
                    super::cell(kin_model::LanguageId::$language, stringify!($capability));
                }
            )*
        }
    };
}

conformance! {
    languages: [
        python => Python,
        typescript => TypeScript,
        javascript => JavaScript,
        go => Go,
        rust => Rust,
        java => Java,
        kotlin => Kotlin,
        c => C,
        cpp => Cpp,
        csharp => CSharp,
        ruby => Ruby,
        php => Php,
        swift => Swift,
        hcl => Hcl,
    ],
    capabilities: [
        parse_clean,
        entities,
        module_name,
        module_contains,
        declaration_extent,
        census_clean,
        nested_definition,
        call_same_file,
        call_receiver,
        imports,
        tests_detected,
        refuse_unparseable,
        placement_three_forms,
        create_remove_spacing,
        body_edit_keeps_entities,
        form_generator,
        form_async,
        form_bound_lambda,
        form_default_export,
        form_overload,
        form_nested_type,
        form_decorated,
        form_field,
        form_flow_types,
    ]
}

/// The outcome classes a capability can have, as `expect.toml` spells them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Pass,
    Partial,
    Fail,
    Refused,
    NotApplicable,
}

impl Status {
    fn parse(word: &str) -> Option<Self> {
        Some(match word {
            "pass" => Self::Pass,
            "partial" => Self::Partial,
            "fail" => Self::Fail,
            "refused" => Self::Refused,
            "n/a" => Self::NotApplicable,
            _ => return None,
        })
    }

    fn word(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Partial => "partial",
            Self::Fail => "fail",
            Self::Refused => "refused",
            Self::NotApplicable => "n/a",
        }
    }
}

/// What one capability measured.
enum Measured {
    /// Each named check and whether it held.
    Checks(Vec<(String, bool)>),
    /// Kin does not offer the capability for this language, and why.
    Refused(String),
}

impl Measured {
    /// The status and the failing checks.
    fn outcome(&self) -> (Status, BTreeSet<String>) {
        match self {
            Self::Refused(_) => (Status::Refused, BTreeSet::new()),
            Self::Checks(checks) => {
                assert!(
                    !checks.is_empty(),
                    "a measured capability names at least one check"
                );
                let failing: BTreeSet<String> = checks
                    .iter()
                    .filter(|(_, held)| !held)
                    .map(|(name, _)| name.clone())
                    .collect();
                let status = if failing.is_empty() {
                    Status::Pass
                } else if failing.len() == checks.len() {
                    Status::Fail
                } else {
                    Status::Partial
                };
                (status, failing)
            }
        }
    }

    fn detail(&self) -> String {
        match self {
            Self::Refused(why) => format!("refused: {why}"),
            Self::Checks(checks) => checks
                .iter()
                .map(|(name, held)| format!("{} {name}", if *held { "ok" } else { "FAILED" }))
                .collect::<Vec<_>>()
                .join("; "),
        }
    }
}

/// The fixture root, `tests/language-conformance` at the workspace root.
fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/language-conformance")
        .canonicalize()
        .expect("the language conformance fixtures sit at the workspace root")
}

/// One language's fixture: its declarations and its sources.
struct Fixture {
    language: LanguageId,
    dir: PathBuf,
    expect: toml::Table,
}

/// One source of a fixture, parsed and extracted.
struct Parsed {
    path: String,
    source: Vec<u8>,
    tree: Tree,
    output: ParseOutput,
}

impl Parsed {
    /// Whether `entity` is the module surface: the module entity that spans
    /// the whole source, as against a namespace or module block inside it.
    fn is_surface(&self, entity: &ExtractedEntity) -> bool {
        let root = self.tree.root_node();
        matches!(entity.kind, EntityKind::Module | EntityKind::Package)
            && entity.span.start_byte <= root.start_byte()
            && entity.span.end_byte >= root.end_byte()
    }

    fn surface(&self) -> Option<&ExtractedEntity> {
        self.output
            .entities
            .iter()
            .find(|entity| self.is_surface(entity))
    }

    /// The entity stored under `name`, other than the module surface.
    fn entity(&self, name: &str) -> Option<&ExtractedEntity> {
        self.output
            .entities
            .iter()
            .find(|entity| entity.name == name && !self.is_surface(entity))
    }
}

impl Fixture {
    fn load(language: LanguageId) -> Self {
        let dir = fixture_root().join(language.to_string());
        let path = dir.join("expect.toml");
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        let expect: toml::Table = text
            .parse()
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        let named = expect.get("language").and_then(|value| value.as_str());
        assert_eq!(
            named,
            Some(language.to_string().as_str()),
            "{} names the language it describes",
            path.display()
        );
        Self {
            language,
            dir,
            expect,
        }
    }

    fn adapter(&self) -> &'static dyn LanguageAdapter {
        static REGISTRY: std::sync::OnceLock<AdapterRegistry> = std::sync::OnceLock::new();
        REGISTRY
            .get_or_init(AdapterRegistry::new)
            .get_by_language(self.language)
            .expect("every LanguageId has an adapter")
    }

    /// The fixture path a role names in `[sources]`.
    fn source_path(&self, role: &str) -> String {
        self.expect
            .get("sources")
            .and_then(|sources| sources.get(role))
            .and_then(|path| path.as_str())
            .unwrap_or_else(|| panic!("{}: [sources] names no {role}", self.language))
            .to_string()
    }

    /// Whether Kin parses `path` as this language: whether its adapter claims
    /// the extension. A source it does not claim is held as an unparsed entity.
    fn claims(&self, path: &str) -> bool {
        let name = path.rsplit('/').next().unwrap_or(path);
        name.rsplit_once('.')
            .is_some_and(|(_, extension)| self.adapter().file_extensions().contains(&extension))
    }

    /// Every source `[sources]` names that Kin parses as this language, each
    /// once, in the order it names them.
    fn parsed_paths(&self) -> Vec<String> {
        self.source_paths()
            .into_iter()
            .filter(|path| self.claims(path))
            .collect()
    }

    /// Every source `[sources]` names, each once, in the order it names them.
    fn source_paths(&self) -> Vec<String> {
        let mut paths: Vec<String> = Vec::new();
        for path in self
            .expect
            .get("sources")
            .and_then(|sources| sources.as_table())
            .expect("expect.toml has a [sources] table")
            .values()
        {
            let path = path.as_str().expect("a source is a path").to_string();
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
        paths
    }

    fn parse_path(&self, path: &str) -> Parsed {
        let full = self.dir.join(path);
        let source =
            std::fs::read(&full).unwrap_or_else(|error| panic!("{}: {error}", full.display()));
        self.parse_bytes(path, source)
    }

    fn parse_bytes(&self, path: &str, source: Vec<u8>) -> Parsed {
        let adapter = self.adapter();
        let tree = adapter
            .parse(&source)
            .unwrap_or_else(|error| panic!("{path}: the parser gave no tree: {error}"));
        let output = adapter
            .extract(&tree, &source, &FilePathId::new(path))
            .unwrap_or_else(|error| panic!("{path}: extraction failed: {error}"));
        Parsed {
            path: path.to_string(),
            source,
            tree,
            output,
        }
    }

    fn parse_role(&self, role: &str) -> Parsed {
        self.parse_path(&self.source_path(role))
    }

    fn capability(&self, capability: &str) -> &toml::Table {
        self.expect
            .get(capability)
            .and_then(|value| value.as_table())
            .unwrap_or_else(|| {
                panic!(
                    "tests/language-conformance/{}/expect.toml declares no [{capability}]",
                    self.language
                )
            })
    }
}

fn text<'a>(table: &'a toml::Table, key: &str) -> &'a str {
    table
        .get(key)
        .and_then(|value| value.as_str())
        .unwrap_or_else(|| panic!("the capability names no {key}"))
}

fn texts(table: &toml::Table, key: &str) -> Vec<String> {
    table
        .get(key)
        .and_then(|value| value.as_array())
        .map(|items| {
            items
                .iter()
                .map(|item| item.as_str().expect("a list of strings").to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// The last segment of a stored name: `validate` of `Account.validate` and of
/// `Account::validate`.
fn leaf(name: &str) -> &str {
    name.rsplit(['.', ':', '#', '\\']).next().unwrap_or(name)
}

fn starts_on_line(source: &[u8], byte: usize) -> usize {
    source[..byte.min(source.len())]
        .iter()
        .filter(|b| **b == b'\n')
        .count()
        + 1
}

/// Run one cell: measure the capability and hold it to the declaration.
fn cell(language: LanguageId, capability: &str) {
    let fixture = Fixture::load(language);
    let table = fixture.capability(capability);
    let declared = table
        .get("status")
        .and_then(|value| value.as_str())
        .and_then(Status::parse)
        .unwrap_or_else(|| {
            panic!("{language}/{capability}: status is one of pass, partial, fail, refused, n/a")
        });
    let declared_gaps: BTreeSet<String> = texts(table, "gaps").into_iter().collect();
    if declared == Status::NotApplicable {
        let reason = table.get("reason").and_then(|value| value.as_str());
        assert!(
            reason.is_some_and(|reason| !reason.trim().is_empty()),
            "{language}/{capability}: n/a says why the language has no such construct"
        );
        assert!(
            declared_gaps.is_empty(),
            "{language}/{capability}: n/a lists no gaps"
        );
        return;
    }
    if declared == Status::Refused {
        let reason = table.get("reason").and_then(|value| value.as_str());
        assert!(
            reason.is_some_and(|reason| !reason.trim().is_empty()),
            "{language}/{capability}: refused says what Kin declines"
        );
    }
    let measured = measure(&fixture, capability, table);
    let (status, failing) = measured.outcome();
    assert!(
        status == declared && failing == declared_gaps,
        "{language}/{capability}: tests/language-conformance/{language}/expect.toml declares \
         {} with gaps {declared_gaps:?}, and the suite measured {} with gaps {failing:?} ({}). \
         When Kin changed on purpose, update the declaration and rerun \
         scripts/language-scorecard.py --write.",
        declared.word(),
        status.word(),
        measured.detail(),
    );
}

fn measure(fixture: &Fixture, capability: &str, table: &toml::Table) -> Measured {
    match capability {
        "parse_clean" => parse_clean(fixture),
        "entities" => entities(fixture, table),
        "module_name" => module_name(fixture, table),
        "module_contains" => module_contains(fixture, table),
        "declaration_extent" => declaration_extent(fixture, table),
        "census_clean" => census_clean(fixture),
        "nested_definition" => nested_definition(fixture, table),
        "call_same_file" => call_same_file(fixture, table),
        "call_receiver" => call_receiver(fixture, table),
        "imports" => imports(fixture, table),
        "tests_detected" => tests_detected(fixture, table),
        "refuse_unparseable" => refuse_unparseable(fixture, table),
        "placement_three_forms" => placement_three_forms(fixture, table),
        "create_remove_spacing" => create_remove_spacing(fixture, table),
        "body_edit_keeps_entities" => body_edit_keeps_entities(fixture, table),
        form if form.starts_with("form_") => declaration_form(fixture, table),
        other => panic!("no measurement for capability {other}"),
    }
}

/// Every source parses with no error node, and extraction calls it valid.
fn parse_clean(fixture: &Fixture) -> Measured {
    Measured::Checks(
        fixture
            .parsed_paths()
            .iter()
            .map(|path| {
                let parsed = fixture.parse_path(path);
                let clean = !parsed.tree.root_node().has_error()
                    && matches!(parsed.output.parse_state, ParseState::Valid);
                (path.clone(), clean)
            })
            .collect(),
    )
}

/// Each expected entity is extracted with its kind and its stored name, which
/// carries the language's member rule (`Account.deposit`, `Account::deposit`),
/// and the module yields no entity beyond them. An expectation is
/// `"Kind Name"` for the module, or `"path: Kind Name"` for another source.
/// Definitions local to a function body are `nested_definition`'s to measure.
fn entities(fixture: &Fixture, table: &toml::Table) -> Measured {
    let module = fixture.source_path("module");
    let mut checks = Vec::new();
    let mut expected_in_module = BTreeSet::new();
    for expected in texts(table, "expect") {
        let (path, entity) = match expected.split_once(": ") {
            Some((path, entity)) => (path.to_string(), entity.to_string()),
            None => (module.clone(), expected.clone()),
        };
        let (kind, name) = entity
            .split_once(' ')
            .unwrap_or_else(|| panic!("{expected}: an entity is written `Kind Name`"));
        let parsed = fixture.parse_path(&path);
        let found = parsed
            .output
            .entities
            .iter()
            .any(|entity| format!("{:?}", entity.kind) == kind && entity.name == name);
        if path == module {
            expected_in_module.insert(entity.clone());
        }
        checks.push((expected, found));
    }
    let parsed = fixture.parse_path(&module);
    for entity in &parsed.output.entities {
        if parsed.is_surface(entity) || is_local_scope_name(&entity.name) {
            continue;
        }
        let written = format!("{:?} {}", entity.kind, entity.name);
        if !expected_in_module.contains(&written) {
            checks.push((format!("unexpected {written}"), false));
        }
    }
    Measured::Checks(checks)
}

/// The module carries the module entity its import edges are sourced at,
/// under the name the language's module rule gives it.
fn module_name(fixture: &Fixture, table: &toml::Table) -> Measured {
    let parsed = fixture.parse_role("module");
    let Some(surface) = parsed.surface() else {
        return Measured::Refused("the adapter mints no module entity".into());
    };
    Measured::Checks(vec![("named".into(), surface.name == text(table, "name"))])
}

/// The module entity contains each of its top-level entities, directly or
/// through a namespace it contains, so a reader can say where an entity is
/// without naming a path. `top_level` lists the module's own declarations:
/// functions, constants, classes and types, not their members.
fn module_contains(fixture: &Fixture, table: &toml::Table) -> Measured {
    let parsed = fixture.parse_role("module");
    let Some(module) = parsed.surface() else {
        return Measured::Refused("the adapter mints no module entity".into());
    };
    let mut reached: BTreeSet<&str> = BTreeSet::from([module.name.as_str()]);
    loop {
        let before = reached.len();
        for relation in &parsed.output.relations {
            if relation.kind == kin_model::RelationKind::Contains
                && reached.contains(relation.src_name.as_str())
            {
                reached.insert(relation.dst_name.as_str());
            }
        }
        if reached.len() == before {
            break;
        }
    }
    Measured::Checks(
        texts(table, "top_level")
            .into_iter()
            .map(|name| {
                let held = name != module.name && reached.contains(name.as_str());
                (name, held)
            })
            .collect(),
    )
}

/// An entity's span starts at the language's whole declaration: its doc
/// comment, attributes, decorators and export keywords are its own. `starts`
/// maps each entity to the text its span must start with.
fn declaration_extent(fixture: &Fixture, table: &toml::Table) -> Measured {
    let parsed = fixture.parse_role("module");
    let starts = table
        .get("starts")
        .and_then(|value| value.as_table())
        .expect("declaration_extent names `starts`");
    Measured::Checks(
        starts
            .iter()
            .map(|(name, prefix)| {
                let prefix = prefix.as_str().expect("a start is text");
                let held = parsed.entity(name).is_some_and(|entity| {
                    parsed.source[entity.span.start_byte.min(parsed.source.len())..]
                        .starts_with(prefix.as_bytes())
                });
                (name.clone(), held)
            })
            .collect(),
    )
}

/// The adapter takes a declaration census, and every callable, type and
/// namespace declaration it sees is admitted as an entity, or is local to a
/// callable (which `nested_definition` measures). Bindings stay out of it.
fn census_clean(fixture: &Fixture) -> Measured {
    let module = fixture.source_path("module");
    let mut checks = Vec::new();
    for path in fixture.parsed_paths() {
        let parsed = fixture.parse_path(&path);
        let Some(seen) = fixture
            .adapter()
            .declaration_census(&parsed.tree, &parsed.source)
        else {
            return Measured::Refused("the adapter takes no declaration census".into());
        };
        let census = take_census(seen.clone(), &parsed.output.entities);
        for declaration in seen.iter().filter(|declaration| {
            // Terraform's public declarations are variable/output blocks and
            // local bindings. Unlike a function-local variable, these are the
            // whole language's named surface and must be graded here.
            fixture.language == LanguageId::Hcl
                || !matches!(
                    declaration.kind,
                    DeclarationKind::Variable | DeclarationKind::Attribute
                )
        }) {
            let unexplained = census.unadmitted.iter().any(|unadmitted| {
                unadmitted.site == declaration.site
                    && unadmitted.reason != REASON_NESTED_IN_CALLABLE
            });
            let name = if path == module {
                declaration.name.clone()
            } else {
                format!("{path}: {}", declaration.name)
            };
            checks.push((name, !unexplained));
        }
    }
    if checks.is_empty() {
        return Measured::Refused("the census saw no declaration".into());
    }
    Measured::Checks(checks)
}

/// The definition nested in a function body is admitted under the shared
/// `<locals>` name, or at least recorded in the nested-definition census.
fn nested_definition(fixture: &Fixture, table: &toml::Table) -> Measured {
    let parsed = fixture.parse_role("module");
    let name = local_scope_name(text(table, "enclosing"), text(table, "name"));
    let admitted = parsed
        .output
        .entities
        .iter()
        .any(|entity| entity.name == name);
    let recorded = parsed
        .output
        .nested_definitions
        .as_ref()
        .is_some_and(|census| census.not_admitted.iter().any(|entry| entry.name == name));
    Measured::Checks(vec![
        ("accounted".into(), admitted || recorded),
        ("admitted".into(), admitted),
    ])
}

fn calls_from<'a>(
    parsed: &'a Parsed,
    from: &'a str,
    to: &'a str,
) -> impl Iterator<Item = &'a crate::extract::ExtractedRelation> {
    parsed.output.relations.iter().filter(move |relation| {
        relation.kind == kin_model::RelationKind::Calls
            && relation.src_name == from
            && leaf(&relation.dst_name) == leaf(to)
    })
}

/// A bare call to a function in the same module is a call edge from the
/// caller's entity.
fn call_same_file(fixture: &Fixture, table: &toml::Table) -> Measured {
    let parsed = fixture.parse_role("module");
    let (from, to) = (text(table, "from"), text(table, "to"));
    let edge = calls_from(&parsed, from, to).next().is_some();
    Measured::Checks(vec![("edge".into(), edge)])
}

/// A call through the receiver (`self`, `this`, a Go or Rust receiver) is a
/// call edge from the method, and it carries the owner: either the callee is
/// named with its type, or the receiver is recorded for the linker.
fn call_receiver(fixture: &Fixture, table: &toml::Table) -> Measured {
    let parsed = fixture.parse_role("module");
    let (from, to) = (text(table, "from"), text(table, "to"));
    let edges: Vec<_> = calls_from(&parsed, from, to).collect();
    let owned = edges
        .iter()
        .any(|relation| relation.dst_name == to || relation.receiver.is_some());
    Measured::Checks(vec![
        ("edge".into(), !edges.is_empty()),
        ("owner".into(), owned),
    ])
}

/// The import of the helper is read with its module path and the names it
/// binds.
fn imports(fixture: &Fixture, table: &toml::Table) -> Measured {
    let parsed = fixture.parse_role("module");
    let module_path = text(table, "module_path");
    let import = parsed
        .output
        .imports
        .iter()
        .find(|import| import.module_path == module_path);
    let mut checks = vec![("module".to_string(), import.is_some())];
    for name in texts(table, "names") {
        let bound = import.is_some_and(|import| {
            import
                .specifiers
                .iter()
                .any(|specifier| specifier.local_name == name)
        });
        checks.push((format!("name {name}"), bound));
    }
    Measured::Checks(checks)
}

/// The framework test is detected, with its runner. A test source Kin does
/// not parse as the language is refused.
fn tests_detected(fixture: &Fixture, table: &toml::Table) -> Measured {
    let path = fixture.source_path("test");
    if !fixture.claims(&path) {
        return Measured::Refused(format!("Kin does not parse {path} as {}", fixture.language));
    }
    let parsed = fixture.parse_path(&path);
    let (name, runner) = (text(table, "name"), text(table, "runner"));
    // A test is found by its own name, whether or not Kin qualifies it with
    // its class or suite.
    let found: Vec<_> = parsed
        .output
        .tests
        .iter()
        .filter(|test| leaf(&test.name) == leaf(name))
        .collect();
    Measured::Checks(vec![
        ("detected".into(), !found.is_empty()),
        (
            "runner".into(),
            found.iter().any(|test| test.runner == runner),
        ),
    ])
}

/// A broken copy of the module (the first `find` replaced by `replace`) is
/// read as broken, at the line the break is on.
fn refuse_unparseable(fixture: &Fixture, table: &toml::Table) -> Measured {
    let parsed = fixture.parse_role("module");
    let original = String::from_utf8(parsed.source.clone()).expect("fixtures are UTF-8");
    let (find, replace) = (text(table, "find"), text(table, "replace"));
    assert!(
        original.contains(find),
        "{}: refuse_unparseable finds `{find}` in the module",
        fixture.language
    );
    let broken = original.replacen(find, replace, 1);
    let line = table
        .get("line")
        .and_then(|value| value.as_integer())
        .expect("refuse_unparseable names the 1-based line of the break") as usize;
    let Some(reading) = read_syntax(fixture.language, broken.as_bytes()) else {
        return Measured::Refused("no grammar reads the language".into());
    };
    Measured::Checks(vec![
        ("detected".into(), !reading.problems.is_empty()),
        (
            "line".into(),
            reading
                .problems
                .iter()
                .any(|problem| starts_on_line(broken.as_bytes(), problem.start_byte) == line),
        ),
    ])
}

/// An entity's whole code sent back in each of the three forms a caller
/// writes it in (as read, at the margin, indented as a whole) is placed where
/// it stood and leaves the source exactly as it was.
fn placement_three_forms(fixture: &Fixture, table: &toml::Table) -> Measured {
    let parsed = fixture.parse_role("module");
    let name = text(table, "entity");
    let Some(entity) = parsed.entity(name) else {
        return Measured::Checks(vec![
            ("as_read".into(), false),
            ("margin".into(), false),
            ("indented".into(), false),
        ]);
    };
    let span = entity.span.start_byte..entity.span.end_byte;
    let source = &parsed.source;
    let code = String::from_utf8_lossy(&source[span.clone()]).to_string();
    let indent = indentation_at(source, span.start).to_string();
    let margin = code
        .split('\n')
        .enumerate()
        .map(|(at, line)| {
            if at == 0 {
                line
            } else {
                line.strip_prefix(indent.as_str()).unwrap_or(line)
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let indented = margin
        .split('\n')
        .map(|line| {
            if line.is_empty() {
                String::new()
            } else {
                format!("{indent}{line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let round_trips = |form: &str| {
        let placed = place_entity_code(fixture.language, source, span.clone(), form);
        let mut written = source[..span.start].to_vec();
        written.extend_from_slice(placed.as_bytes());
        written.extend_from_slice(&source[span.end..]);
        &written == source
    };
    Measured::Checks(vec![
        ("as_read".into(), round_trips(&code)),
        ("margin".into(), round_trips(&margin)),
        ("indented".into(), round_trips(&indented)),
    ])
}

/// Blank lines directly above the line holding `at`.
fn blank_lines_above(source: &[u8], at: usize) -> usize {
    let text = String::from_utf8_lossy(&source[..at]);
    let lines: Vec<&str> = text.split('\n').collect();
    // The last element is the partial line `at` sits on.
    lines[..lines.len().saturating_sub(1)]
        .iter()
        .rev()
        .take_while(|line| line.trim().is_empty())
        .count()
}

/// Blank lines directly below the line holding `at`.
fn blank_lines_below(source: &[u8], at: usize) -> usize {
    let text = String::from_utf8_lossy(&source[at.min(source.len())..]);
    text.split('\n')
        .skip(1)
        .take_while(|line| line.trim().is_empty())
        .count()
}

/// A new declaration written after a sibling stands as far from its
/// neighbours as they stood from each other, parses, and is extracted;
/// removing it gives the source back byte for byte; and removing the
/// documented declaration takes its whole declaration (documentation and
/// prefixes included) and leaves source that parses.
fn create_remove_spacing(fixture: &Fixture, table: &toml::Table) -> Measured {
    let parsed = fixture.parse_role("module");
    let source = &parsed.source;
    let anchor = text(table, "anchor");
    let code = text(table, "code");
    let created_name = text(table, "entity");
    let documented = text(table, "documented");
    let doc = text(table, "doc");
    assert!(
        String::from_utf8_lossy(source).contains(doc),
        "{}: create_remove_spacing finds `{doc}` in the module",
        fixture.language
    );
    let clean = |bytes: &[u8]| {
        read_syntax(fixture.language, bytes).is_some_and(|reading| reading.problems.is_empty())
    };

    let mut checks = Vec::new();
    match parsed.entity(anchor) {
        Some(anchor) => {
            let (at, lead) = after_sibling(source, anchor.span.start_byte..anchor.span.end_byte, 1);
            let mut created = source[..at].to_vec();
            created.extend_from_slice(lead.as_bytes());
            let start = created.len();
            created.extend_from_slice(code.as_bytes());
            let end = created.len();
            created.extend_from_slice(&source[at..]);
            let reparsed = fixture.parse_bytes(&parsed.path, created.clone());
            let new_entity = reparsed.entity(created_name);
            checks.push((
                "create_parses".to_string(),
                clean(&created) && new_entity.is_some(),
            ));
            checks.push((
                "create_spacing".to_string(),
                blank_lines_above(&created, start) == blank_lines_below(&created, end),
            ));
            let round_trip = new_entity.is_some_and(|entity| {
                let removal = removal_of(&created, entity.span.start_byte..entity.span.end_byte);
                let mut removed = created[..removal.start].to_vec();
                removed.extend_from_slice(&created[removal.end..]);
                &removed == source
            });
            checks.push(("remove_roundtrip".to_string(), round_trip));
        }
        None => {
            for check in ["create_parses", "create_spacing", "remove_roundtrip"] {
                checks.push((check.to_string(), false));
            }
        }
    }
    let documented_removed = parsed.entity(documented).is_some_and(|entity| {
        let removal = removal_of(source, entity.span.start_byte..entity.span.end_byte);
        let mut removed = source[..removal.start].to_vec();
        removed.extend_from_slice(&source[removal.end..]);
        !String::from_utf8_lossy(&removed).contains(doc) && clean(&removed)
    });
    checks.push(("remove_documented".to_string(), documented_removed));
    Measured::Checks(checks)
}

/// A statement added inside a function body, such as an assignment to a
/// member of a local object, is a change to that function and nothing else:
/// the source still parses and Kin extracts the same entities from it. `in`
/// names the source (the module by default), and the first `find` inside
/// `entity`'s span is replaced by `replace`.
fn body_edit_keeps_entities(fixture: &Fixture, table: &toml::Table) -> Measured {
    let role = table
        .get("in")
        .and_then(|value| value.as_str())
        .unwrap_or("module");
    let parsed = fixture.parse_role(role);
    let original = String::from_utf8(parsed.source.clone()).expect("fixtures are UTF-8");
    let (find, replace) = (text(table, "find"), text(table, "replace"));
    let owner = text(table, "entity");
    let span = parsed
        .entity(owner)
        .map(|entity| entity.span.start_byte..entity.span.end_byte)
        .unwrap_or_else(|| panic!("{}: no entity {owner} to edit", fixture.language));
    let at = original[span.clone()]
        .find(find)
        .map(|offset| span.start + offset)
        .unwrap_or_else(|| panic!("{}: `{find}` is not inside {owner}", fixture.language));
    let mut edited = original.clone();
    edited.replace_range(at..at + find.len(), replace);
    let held = |parsed: &Parsed| -> BTreeSet<String> {
        parsed
            .output
            .entities
            .iter()
            .filter(|entity| !parsed.is_surface(entity))
            .map(|entity| format!("{:?} {}", entity.kind, entity.name))
            .collect()
    };
    let before = held(&parsed);
    let after_parse = fixture.parse_bytes(&parsed.path, edited.clone().into_bytes());
    let after = held(&after_parse);
    let clean = read_syntax(fixture.language, edited.as_bytes())
        .is_some_and(|reading| reading.problems.is_empty());
    let mut checks = vec![("parses".to_string(), clean)];
    for created in after.difference(&before) {
        checks.push((format!("created {created}"), false));
    }
    for removed in before.difference(&after) {
        checks.push((format!("removed {removed}"), false));
    }
    if before == after {
        checks.push(("entities unchanged".to_string(), true));
    }
    Measured::Checks(checks)
}

/// A declaration written in one of the language's forms (a generator, an
/// async function, a lambda bound to a name, a default export, an overload, a
/// nested type, a decorated declaration, a field or property) is an entity
/// like any other. Each expectation is `"Kinds Name"` in the fixture's `forms`
/// source, where `Kinds` may list alternatives (`Field|Method`): the entity is
/// found under its name, and it has one of those kinds.
fn declaration_form(fixture: &Fixture, table: &toml::Table) -> Measured {
    // A form may name its own source, one the route layer does not commit
    // (`parse-only/`), when no grammar reads it whole.
    let parsed = match table.get("source").and_then(|value| value.as_str()) {
        Some(path) => fixture.parse_path(path),
        None => fixture.parse_role("forms"),
    };
    let mut checks = Vec::new();
    for expected in texts(table, "expect") {
        let (kinds, name) = expected
            .split_once(' ')
            .unwrap_or_else(|| panic!("{expected}: a form is written `Kinds Name`"));
        let named: Vec<&ExtractedEntity> = parsed
            .output
            .entities
            .iter()
            .filter(|entity| entity.name == name && !parsed.is_surface(entity))
            .collect();
        let kinded = named.iter().any(|entity| {
            let kind = format!("{:?}", entity.kind);
            kinds.split('|').any(|wanted| wanted == kind)
        });
        checks.push((name.to_string(), !named.is_empty()));
        checks.push((expected.clone(), kinded));
    }
    Measured::Checks(checks)
}

/// The capability catalog, `capabilities.toml`: every capability any layer
/// measures, in scorecard order, with its layer.
fn catalog() -> Vec<(String, String)> {
    let path = fixture_root().join("capabilities.toml");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    let table: toml::Table = text
        .parse()
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    table
        .get("capability")
        .and_then(|value| value.as_array())
        .expect("capabilities.toml lists [[capability]] entries")
        .iter()
        .map(|entry| {
            let field = |key: &str| {
                entry
                    .get(key)
                    .and_then(|value| value.as_str())
                    .unwrap_or_else(|| panic!("a capability has a {key}"))
                    .to_string()
            };
            (field("name"), field("layer"))
        })
        .collect()
}

/// This suite measures exactly the catalog's parser-layer capabilities, in
/// its order, so the scorecard cannot list a cell nothing measures.
#[test]
fn catalog_lists_the_parser_cells() {
    let parser: Vec<String> = catalog()
        .into_iter()
        .filter(|(_, layer)| layer == "parser")
        .map(|(name, _)| name)
        .collect();
    let cells: Vec<String> = CAPABILITIES.iter().map(ToString::to_string).collect();
    assert_eq!(
        parser, cells,
        "capabilities.toml lists the parser layer's cells, in the order this suite declares them"
    );
}

/// The cells cover every language Kin parses, and each language's fixture
/// declares every catalogued capability and nothing else.
#[test]
fn every_language_has_cells() {
    let mut listed: Vec<String> = LANGUAGES.iter().map(ToString::to_string).collect();
    let mut known: Vec<String> = ALL_LANGUAGE_IDS.iter().map(ToString::to_string).collect();
    listed.sort();
    known.sort();
    assert_eq!(
        listed, known,
        "the conformance cells cover exactly the languages Kin parses"
    );
    let capabilities: BTreeSet<String> = catalog().into_iter().map(|(name, _)| name).collect();
    for language in ALL_LANGUAGE_IDS {
        let fixture = Fixture::load(*language);
        let declared: BTreeSet<String> = fixture
            .expect
            .keys()
            .filter(|key| !matches!(key.as_str(), "language" | "sources"))
            .cloned()
            .collect();
        assert_eq!(
            declared, capabilities,
            "tests/language-conformance/{language}/expect.toml declares every catalogued \
             capability and only those"
        );
        for path in fixture.source_paths() {
            assert!(
                fixture.dir.join(&path).is_file(),
                "{language}: [sources] names {path}, which the fixture lacks"
            );
        }
    }
}
