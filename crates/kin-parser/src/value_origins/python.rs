// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Python value origins: single-binding locals, declared returns, declared
//! instance attributes, and pytest fixture injection.
//!
//! pytest passes a test the value of the fixture whose name its parameter
//! spells. The fixture is found by a fixed rule: one the requesting class
//! defines, then one a class it derives from defines, then one the
//! requesting module defines, then one a `conftest.py` defines in the
//! module's directory or the nearest ancestor holding one, then one a module
//! a `pytest_plugins` list on that walk names. A fixture's own parameters are
//! fixtures too, injected wherever the fixture is declared, a plugin module
//! included, and pytest finds each of them for the test that made the
//! request, from where that test sits (the linker anchors the walk there).
//! Nothing else can supply a repository fixture's value to a plain parameter
//! except the cases this module refuses to record: a parameter with a
//! default, one `@pytest.mark.parametrize` names, a module whose `pytestmark`
//! could parametrize it, and a lookup that walks past a
//! `pytest_generate_tests` or `pytest_fixture_setup` hook (the linker checks
//! those, since they can sit in another file). A name a test parametrizes is
//! also recorded as withheld ([`ScopeFacts::withheld_names`]), because it is
//! the parameter's value for every fixture the test's request reaches.
//!
//! A local is recorded only when the function binds it exactly once anywhere
//! in its body, nested scopes included. In a function scope a name bound once
//! holds that one value at every read that succeeds, so no flow analysis is
//! needed, and counting nested scopes too means a call the adapter attributes
//! to the outer function can never read a shadowing inner binding. A name a
//! `global` or `nonlocal` statement declares is never single; every other
//! name keeps its count.
//!
//! A plain module-level function that declares no return type returns the
//! one value every `return` of its body agrees on ([`ScopeFacts::returned`]):
//! `create_app()` returns the `app` it builds. A generator, a coroutine and a
//! decorated function return something else and record nothing.
//!
//! A class records what its instances' attributes are declared to hold
//! ([`ScopeFacts::members`]): an annotated name in its body, an annotated
//! `self.<attribute>` assignment in one of its instance methods, or a
//! property's declared return, so `app.json.dumps(...)` follows
//! `self.json: JSONProvider` to the method that annotation types.
//!
//! Values of types the repository does not declare are recorded as external
//! ([`ValueOrigin::External`]), so the linker can prove a call through them
//! reaches nothing here:
//!
//! - a literal or comprehension (`{}`, `[x for x in y]`, `"a"`) and a call to
//!   a built-in type (`dict(...)`, `sorted(...)`) or to a `collections` class
//!   the file imports, when nothing in the module or the function rebinds the
//!   name and the module has no wildcard import: the value's class is exactly
//!   that type;
//! - an annotation naming a built-in container (`dict[str, Any]`,
//!   `t.Optional[list]`), its `typing` alias, or a public pytest type: the
//!   value is that type or a subclass, which the linker checks the repository
//!   for;
//! - a pytest built-in fixture (`recwarn`, `monkeypatch`, `tmp_path` and the
//!   rest), by the type pytest documents for it, only when the conftest walk
//!   finds no fixture of that name in the repository first and the test's
//!   class names no base that could define one. A file pytest reads fixtures
//!   from that can bring in fixtures the walk cannot name (a wildcard import,
//!   a `pytest_plugins` value the reader cannot list) stops the walk. A name
//!   it imports is the fixture the import reaches, when it is one
//!   ([`ValueOrigin::ImportedProvider`]).
//!
//! Two framework rules bind values by declaration as well. A FastAPI parameter
//! whose default is `Depends(provider)` (or `Security`) holds what FastAPI
//! supplies from `provider`: an instance of a class, or what a function yields
//! or returns ([`ScopeFacts::supplies`]). A unittest attribute `self.x`
//! assigned exactly once in its class, as a top-level statement of `setUp`
//! with no call through `self` or `super()` after it, holds that value in
//! every `test*` method of the class, provided the class has no repository
//! base and no repository subclass (the linker checks both).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use tree_sitter::{Node, Tree};

use super::{
    DeclaredReturn, ExternalReach, FileFacts, ProviderLookup, ProviderModule, ScopeFacts,
    ValueOrigin, ValueProvider,
};
use crate::languages::python::python_base_class_node;
use crate::languages::python_escape::{names_protocol_base, under_type_checking};

/// The namespace pytest fixtures are provided in.
pub(crate) const PYTEST_FIXTURE_KIND: &str = "pytest_fixture";

/// The file pytest reads directory-scoped fixtures from.
const CONFTEST: &str = "conftest.py";

/// Hooks that can replace a fixture's value or its parameters, so a lookup
/// that walks past one of them proves nothing.
const FIXTURE_BLOCKERS: &[&str] = &["pytest_generate_tests", "pytest_fixture_setup"];

/// The spellings of `typing.Self` a return annotation can use.
const SELF_TYPES: &[&str] = &[
    "Self",
    "typing.Self",
    "typing_extensions.Self",
    "te.Self",
    "t.Self",
];

/// The namespace unittest `setUp` attributes are provided in.
pub(crate) const UNITTEST_SETUP_KIND: &str = "unittest_setup";

/// pytest's built-in fixtures, by the class pytest documents for the value
/// each supplies. pytest constructs every one of them itself, so the value's
/// class is outside the repository unless the repository is pytest, which the
/// linker sees as a repository declaration of the member.
const PYTEST_BUILTIN_FIXTURES: &[(&str, &str)] = &[
    ("cache", "_pytest.cacheprovider.Cache"),
    ("capfd", "_pytest.capture.CaptureFixture"),
    ("capfdbinary", "_pytest.capture.CaptureFixture"),
    ("caplog", "_pytest.logging.LogCaptureFixture"),
    ("capsys", "_pytest.capture.CaptureFixture"),
    ("capsysbinary", "_pytest.capture.CaptureFixture"),
    ("capteesys", "_pytest.capture.CaptureFixture"),
    ("doctest_namespace", "dict"),
    ("monkeypatch", "_pytest.monkeypatch.MonkeyPatch"),
    ("pytestconfig", "_pytest.config.Config"),
    ("pytester", "_pytest.pytester.Pytester"),
    ("recwarn", "_pytest.recwarn.WarningsRecorder"),
    ("request", "_pytest.fixtures.FixtureRequest"),
    ("testdir", "_pytest.legacypath.Testdir"),
    ("tmp_path", "pathlib.Path"),
    ("tmp_path_factory", "_pytest.tmpdir.TempPathFactory"),
    ("tmpdir", "_pytest._py.path.LocalPath"),
    ("tmpdir_factory", "_pytest.legacypath.TempdirFactory"),
];

/// pytest's public classes, as `pytest` exports them, for an annotation that
/// names one.
const PYTEST_PUBLIC_TYPES: &[(&str, &str)] = &[
    ("Cache", "_pytest.cacheprovider.Cache"),
    ("CaptureFixture", "_pytest.capture.CaptureFixture"),
    ("Config", "_pytest.config.Config"),
    ("FixtureRequest", "_pytest.fixtures.FixtureRequest"),
    ("LogCaptureFixture", "_pytest.logging.LogCaptureFixture"),
    ("MonkeyPatch", "_pytest.monkeypatch.MonkeyPatch"),
    ("Pytester", "_pytest.pytester.Pytester"),
    ("TempPathFactory", "_pytest.tmpdir.TempPathFactory"),
    ("WarningsRecorder", "_pytest.recwarn.WarningsRecorder"),
];

/// Built-in types a literal, a call or an annotation can name, by the name
/// the builtins module binds.
const BUILTIN_TYPES: &[&str] = &[
    "bytearray",
    "bytes",
    "dict",
    "frozenset",
    "list",
    "set",
    "str",
    "tuple",
];

/// Built-in functions whose result is a built-in type, by what they return.
const BUILTIN_RESULTS: &[(&str, &str)] = &[("sorted", "list")];

/// `typing` aliases of built-in and `collections` types.
const TYPING_ALIASES: &[(&str, &str)] = &[
    ("Counter", "collections.Counter"),
    ("DefaultDict", "collections.defaultdict"),
    ("Deque", "collections.deque"),
    ("Dict", "dict"),
    ("FrozenSet", "frozenset"),
    ("List", "list"),
    ("OrderedDict", "collections.OrderedDict"),
    ("Set", "set"),
    ("Text", "str"),
    ("Tuple", "tuple"),
];

/// The modules [`TYPING_ALIASES`] are read from.
const TYPING_MODULES: &[&str] = &["typing", "typing_extensions"];

/// Concrete `collections` classes a call constructs.
const COLLECTIONS_CLASSES: &[&str] = &["Counter", "OrderedDict", "defaultdict", "deque"];

/// The base a Python class names to derive from nothing but the universal
/// root, which makes it a subtype of no external type.
const PYTHON_ROOTS: &[&str] = &["object"];

/// The FastAPI markers whose argument is the dependency that supplies the
/// parameter's value.
const FASTAPI_DEPENDENCY_MARKERS: &[&str] = &["Depends", "Security"];

/// The generator wrappers whose first argument is what a generator fixture
/// yields.
const GENERATOR_WRAPPERS: &[&str] = &[
    "Iterator",
    "Iterable",
    "Generator",
    "AsyncIterator",
    "AsyncIterable",
    "AsyncGenerator",
];

pub(super) fn read_file(tree: &Tree, source: &[u8], file_path: &str) -> FileFacts {
    let root = tree.root_node();
    let mut file = FileContext::read(root, source, file_path);
    let mut facts = FileFacts::default();
    // What the module itself binds once, for a scope that reads one of those
    // names without binding it and for a reader in another module.
    facts.module.bindings = module_origins(root, source, &file);
    file.module_origin_names = facts.module.bindings.keys().cloned().collect();
    let file = file;
    let mut cursor = root.walk();
    let no_class = ClassContext::default();
    for child in root.named_children(&mut cursor) {
        match definition_of(child) {
            Some((def, wrapper)) if def.kind() == "function_definition" => {
                read_function(def, wrapper, None, &no_class, &file, source, &mut facts);
            }
            Some((def, wrapper)) if def.kind() == "class_definition" => {
                read_class(def, wrapper, &file, source, &mut facts);
            }
            _ => {
                if let Some(provider) = assigned_fixture(child, None, &file, source) {
                    facts.module.provides.push(provider);
                }
            }
        }
    }
    read_inner_classes(root, &file, source, &mut facts);
    let declares_fixtures = facts
        .by_node
        .iter()
        .flat_map(|(_, _, scope)| &scope.provides)
        .chain(&facts.module.provides)
        .any(|provider| provider.kind == PYTEST_FIXTURE_KIND);
    facts
        .module
        .provides
        .extend(file.fixture_shadows(declares_fixtures));
    facts.module.provider_modules = file
        .plugin_modules
        .iter()
        .map(|module| ProviderModule {
            kind: PYTEST_FIXTURE_KIND.into(),
            module: module.clone(),
        })
        .collect();
    facts
}

/// What a module binds each of its own names to, for a name its top level
/// binds exactly once and no `global` statement or wildcard import can
/// rebind: the declared type of an annotated assignment (`session:
/// SessionMixinProxy = LocalProxy(...)`), the member a `from ... import`
/// brings in, the module an `import` names. A scope that reads such a name
/// without binding it reads this ([`ValueOrigin::Global`]).
fn module_origins(
    root: Node<'_>,
    source: &[u8],
    file: &FileContext,
) -> BTreeMap<String, ValueOrigin> {
    let mut origins = BTreeMap::new();
    if file.star_import {
        return origins;
    }
    let counts = module_binding_counts(root, source);
    let once = |name: &str| counts.get(name) == Some(&1) && !file.global_names.contains(name);
    for (local, (module, original)) in &file.from_imports {
        if once(local) {
            origins.insert(
                local.clone(),
                ValueOrigin::ModuleMember {
                    module: module.clone(),
                    name: original.clone(),
                },
            );
        }
    }
    for (local, module) in &file.module_of_alias {
        if once(local) {
            origins.insert(
                local.clone(),
                ValueOrigin::Module {
                    module: module.clone(),
                },
            );
        }
    }
    let no_names = HashSet::new();
    let mut cursor = root.walk();
    for statement in root.named_children(&mut cursor) {
        let Some(assignment) = Some(statement)
            .filter(|statement| statement.kind() == "expression_statement")
            .and_then(|statement| statement.named_child(0))
            .filter(|assignment| assignment.kind() == "assignment")
        else {
            continue;
        };
        let (Some(left), Some(annotation)) = (
            assignment.child_by_field_name("left"),
            assignment.child_by_field_name("type"),
        ) else {
            continue;
        };
        if left.kind() != "identifier" || !once(text(left, source)) {
            continue;
        }
        if let Some(origin) = annotation_origin(annotation, file, &no_names, source) {
            origins.insert(text(left, source).to_string(), origin);
        }
    }
    origins
}

/// Record, for each module-level name `body` reads an attribute off without
/// the function binding it, that the function reads the module's binding
/// ([`ValueOrigin::Global`]): `flask` in `flask.session.pop`, `session` in
/// `session.pop`. Nested functions, classes and lambdas are other scopes.
fn collect_global_roots(
    body: Node<'_>,
    census: &Census,
    file: &FileContext,
    source: &[u8],
    bindings: &mut BTreeMap<String, ValueOrigin>,
) {
    let mut stack = vec![body];
    while let Some(node) = stack.pop() {
        match node.kind() {
            "function_definition" | "class_definition" | "lambda" => continue,
            "attribute" => {
                let mut root = node;
                while root.kind() == "attribute" {
                    match root.child_by_field_name("object") {
                        Some(object) => root = object,
                        None => break,
                    }
                }
                if root.kind() == "identifier" {
                    let name = text(root, source);
                    if census.count(name) == 0
                        && file.module_origin_names.contains(name)
                        && !bindings.contains_key(name)
                    {
                        bindings.insert(
                            name.to_string(),
                            ValueOrigin::Global {
                                name: name.to_string(),
                            },
                        );
                    }
                }
            }
            _ => {}
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
}

/// What every class the top-level pass does not read says about itself: a
/// class in a function body, in another class, or under a module-level block.
/// A reader that walks a hierarchy has to hear from every class of it, and a
/// test often derives one where it uses it. Such a class says the names it
/// binds, whether the file hands it over, and whether it is a class for the
/// type checker alone or a structural type; what its bases are is left to
/// the scope around it, which this does not read.
fn read_inner_classes(root: Node<'_>, file: &FileContext, source: &[u8], facts: &mut FileFacts) {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push(child);
            // A decorated class is read with its wrapper, one level up, and
            // the module's own classes by the top-level pass.
            if matches!(node.kind(), "module" | "decorated_definition") {
                continue;
            }
            let Some((class, wrapper)) = definition_of(child)
                .filter(|(definition, _)| definition.kind() == "class_definition")
            else {
                continue;
            };
            let (Some(name), Some(body)) = (
                class
                    .child_by_field_name("name")
                    .map(|name| text(name, source)),
                class.child_by_field_name("body"),
            ) else {
                continue;
            };
            let bound_names =
                class_bound_names(class, wrapper, body, &class_bindings(body, source), source);
            let handed_over = !decorators_of(wrapper).is_empty()
                || names_keyword_base(class)
                || file.passed_names.contains(name);
            // A class under `if TYPE_CHECKING:` is always one of these: the
            // top-level pass reads only the module's own statements.
            let typing_only = under_type_checking(class, source);
            let structural = names_protocol_base(class, source);
            if bound_names.is_none() && !handed_over && !typing_only && !structural {
                continue;
            }
            let mut ranges = vec![(class.start_byte(), class.end_byte())];
            if let Some(wrapper) = wrapper {
                ranges.push((wrapper.start_byte(), wrapper.end_byte()));
            }
            facts.by_node.push((
                ranges,
                name.to_string(),
                ScopeFacts {
                    bound_names,
                    handed_over,
                    typing_only,
                    structural,
                    ..ScopeFacts::default()
                },
            ));
        }
    }
}

/// The modules a `pytest_plugins` value lists: one string, or a list or
/// tuple of strings, each a dotted module name.
fn plugin_list(value: Node<'_>, source: &[u8]) -> Option<Vec<String>> {
    let module = |node: Node<'_>| {
        let name = string_literal(node, source)?;
        let name = name.trim();
        (!name.is_empty() && name.split('.').all(is_identifier)).then(|| name.to_string())
    };
    match value.kind() {
        "string" => Some(vec![module(value)?]),
        "list" | "tuple" => {
            let mut modules = Vec::new();
            let mut cursor = value.walk();
            for item in value.named_children(&mut cursor) {
                modules.push(module(item)?);
            }
            Some(modules)
        }
        "parenthesized_expression" => plugin_list(value.named_child(0)?, source),
        _ => None,
    }
}

/// What the file says about every scope in it.
struct FileContext {
    /// Whether pytest collects tests from this module under its default
    /// `python_files` patterns, or reads fixtures from it as a conftest.
    collects: bool,
    /// Whether unittest's default discovery pattern `test*.py` matches the
    /// module, or pytest collects it.
    collects_unittest: bool,
    /// Whether a module-level `pytestmark` could parametrize any argument.
    pytestmark: bool,
    /// Whether the module binds `pytest_plugins` in a way the reader cannot
    /// list, which loads fixtures from modules nothing here names.
    pytest_plugins: bool,
    /// The modules a module-level `pytest_plugins` assignment lists, when it
    /// is the one binding of the name and spells a string or a list or tuple
    /// of them.
    plugin_modules: Vec<String>,
    /// Whether the module holds a wildcard import, which can bind any name.
    star_import: bool,
    /// Names a module-level `import` binds to a module.
    module_aliases: HashSet<String>,
    /// The module each module-level `import` binds a local name to.
    module_of_alias: HashMap<String, String>,
    /// (module, original name) each module-level `from ... import` binds a
    /// local name to.
    from_imports: HashMap<String, (String, String)>,
    /// Every name the module scope binds, anywhere outside a function or class
    /// body: a definition, an import or an assignment target.
    module_bindings: HashSet<String>,
    /// The dotted name each local name stands for, where the module binds
    /// the name by one top-level absolute `from ... import` and by nothing
    /// else: `CliRunner` -> `click.testing.CliRunner`. Empty in a module
    /// with a wildcard import.
    imported_names: HashMap<String, String>,
    /// The module each local name stands for, where the module binds the
    /// name by one top-level `import` and by nothing else.
    imported_modules: HashMap<String, String>,
    /// Every plain name the module passes as an argument of a call, other
    /// than to a class test (`isinstance`, `issubclass`) or `super`.
    passed_names: HashSet<String>,
    /// Every name a `global` statement anywhere in the module declares, which
    /// a function can bind at module scope.
    global_names: HashSet<String>,
    /// The module-level names [`module_origins`] records an origin for, which
    /// a function that does not bind one of them reads as the module's.
    module_origin_names: HashSet<String>,
    /// Local names `from pytest import fixture [as x]` binds.
    fixture_aliases: HashSet<String>,
    /// Local names `import pytest [as x]` binds.
    pytest_aliases: HashSet<String>,
}

impl FileContext {
    fn read(root: Node<'_>, source: &[u8], file_path: &str) -> Self {
        let base = file_path.rsplit('/').next().unwrap_or(file_path);
        let collects = base == CONFTEST
            || (base.ends_with(".py") && (base.starts_with("test_") || base.ends_with("_test.py")));
        let collects_unittest =
            collects || (base.ends_with(".py") && base.starts_with("test") && base != CONFTEST);
        let mut context = Self {
            collects,
            collects_unittest,
            pytestmark: false,
            pytest_plugins: false,
            plugin_modules: Vec::new(),
            star_import: false,
            module_aliases: HashSet::new(),
            module_of_alias: HashMap::new(),
            from_imports: HashMap::new(),
            module_bindings: HashSet::new(),
            imported_names: HashMap::new(),
            imported_modules: HashMap::new(),
            passed_names: passed_names(root, source),
            global_names: global_names(root, source),
            module_origin_names: HashSet::new(),
            fixture_aliases: HashSet::new(),
            pytest_aliases: HashSet::new(),
        };
        module_bindings(
            root,
            source,
            &mut context.module_bindings,
            &mut context.star_import,
        );
        let mut plugin_assignments = 0usize;
        let mut cursor = root.walk();
        for child in root.named_children(&mut cursor) {
            match child.kind() {
                "import_statement" => {
                    let mut names = child.walk();
                    for name in child.children_by_field_name("name", &mut names) {
                        let (module, local) = match name.kind() {
                            "aliased_import" => (
                                name.child_by_field_name("name").map(|n| text(n, source)),
                                name.child_by_field_name("alias").map(|n| text(n, source)),
                            ),
                            _ => {
                                let full = text(name, source);
                                (full.split('.').next(), full.split('.').next())
                            }
                        };
                        if let Some(local) = local {
                            context.module_aliases.insert(local.to_string());
                            if let Some(module) = module {
                                context
                                    .module_of_alias
                                    .insert(local.to_string(), module.to_string());
                            }
                            if module == Some("pytest") {
                                context.pytest_aliases.insert(local.to_string());
                            }
                        }
                    }
                }
                "import_from_statement" => {
                    let Some(module) = child
                        .child_by_field_name("module_name")
                        .map(|n| text(n, source))
                    else {
                        continue;
                    };
                    let mut names = child.walk();
                    for name in child.children_by_field_name("name", &mut names) {
                        let (original, local) = match name.kind() {
                            "aliased_import" => (
                                name.child_by_field_name("name").map(|n| text(n, source)),
                                name.child_by_field_name("alias").map(|n| text(n, source)),
                            ),
                            _ => (Some(text(name, source)), Some(text(name, source))),
                        };
                        let (Some(original), Some(local)) = (original, local) else {
                            continue;
                        };
                        if module == "pytest" && original == "fixture" {
                            context.fixture_aliases.insert(local.to_string());
                        }
                        context.from_imports.insert(
                            local.to_string(),
                            (module.to_string(), original.to_string()),
                        );
                    }
                }
                "expression_statement" => {
                    if let Some(assignment) = child.named_child(0) {
                        if assignment.kind() == "assignment" {
                            match assignment
                                .child_by_field_name("left")
                                .map(|left| text(left, source))
                            {
                                Some("pytestmark") => context.pytestmark = true,
                                Some("pytest_plugins") => {
                                    plugin_assignments += 1;
                                    match assignment
                                        .child_by_field_name("right")
                                        .and_then(|right| plugin_list(right, source))
                                    {
                                        Some(modules) => context.plugin_modules = modules,
                                        None => context.pytest_plugins = true,
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        // Bound anywhere else as well (augmented, in a block, imported), the
        // list the module ends up with is not one the reader can name.
        if context.module_bindings.contains("pytest_plugins")
            && (plugin_assignments != 1
                || module_binding_count(root, source, "pytest_plugins") != 1)
        {
            context.pytest_plugins = true;
        }
        if context.pytest_plugins {
            context.plugin_modules.clear();
        }
        // A name one import binds and nothing rebinds is that import's, at
        // every read of it the module scope answers. A `global` statement in
        // a function can rebind any name it declares.
        if !context.star_import {
            let counts = module_binding_counts(root, source);
            let rebound = &context.global_names;
            let once = |local: &String| counts.get(local) == Some(&1) && !rebound.contains(local);
            for (local, (module, original)) in &context.from_imports {
                if !module.starts_with('.') && once(local) {
                    context
                        .imported_names
                        .insert(local.clone(), format!("{module}.{original}"));
                }
            }
            for (local, module) in &context.module_of_alias {
                if once(local) {
                    context
                        .imported_modules
                        .insert(local.clone(), module.clone());
                }
            }
        }
        context
    }

    /// The dotted name a callee or a base written as `spelled` stands for,
    /// when its first name is one the module binds by one import and by
    /// nothing else: a plain imported name, or a path through an imported
    /// module.
    fn imported_name(&self, spelled: &str) -> Option<String> {
        match spelled.split_once('.') {
            None => self.imported_names.get(spelled).cloned(),
            Some((root, rest)) => self
                .imported_modules
                .get(root)
                .map(|module| format!("{module}.{rest}")),
        }
    }

    /// The fixtures a module registers beside the ones it declares: pytest
    /// registers every fixture function a module's namespace holds, so a
    /// name a `from` import binds may be a fixture defined elsewhere, whose
    /// value is that fixture's, and a wildcard import or a `pytest_plugins`
    /// list the reader cannot name may bring in any. Read for a module pytest
    /// collects or reads fixtures from: a test module, a conftest, and a
    /// module that declares a fixture or plugins itself, as a plugin module
    /// does.
    fn fixture_shadows(&self, declares_fixtures: bool) -> Vec<ValueProvider> {
        if !self.collects && !declares_fixtures && self.plugin_modules.is_empty() {
            return Vec::new();
        }
        let mut shadows: Vec<ValueProvider> = self
            .from_imports
            .keys()
            .map(|local| ValueProvider {
                kind: PYTEST_FIXTURE_KIND.into(),
                name: local.clone(),
                owner: None,
                value: Some(ValueOrigin::ImportedProvider {
                    kind: PYTEST_FIXTURE_KIND.into(),
                    name: local.clone(),
                }),
                every_name: false,
            })
            .collect();
        shadows.sort_by(|a, b| a.name.cmp(&b.name));
        if self.star_import || self.pytest_plugins {
            shadows.push(ValueProvider {
                kind: PYTEST_FIXTURE_KIND.into(),
                name: String::new(),
                owner: None,
                value: None,
                every_name: true,
            });
        }
        shadows
    }

    /// Whether `name` still means the builtin of that name at run time in a
    /// function whose census is `census`.
    fn builtin_unshadowed(&self, name: &str, census: &Census) -> bool {
        !self.star_import && !self.module_bindings.contains(name) && census.count(name) == 0
    }

    /// The external type a dotted or plain name written as a type or a
    /// callee names, when it is one this adapter knows: a built-in type, its
    /// `typing` alias, a `collections` class, or a public pytest class.
    /// `shadowed` holds the names a nearer scope rebinds.
    fn external_type(&self, spelled: &str, shadowed: &HashSet<String>) -> Option<String> {
        if self.star_import {
            return None;
        }
        let (root, rest) = match spelled.split_once('.') {
            Some((root, rest)) => (root, Some(rest)),
            None => (spelled, None),
        };
        if shadowed.contains(root) {
            return None;
        }
        match rest {
            None => {
                if BUILTIN_TYPES.contains(&root) && !self.module_bindings.contains(root) {
                    return Some(root.to_string());
                }
                let (module, original) = self.from_imports.get(root)?;
                self.member_type(module, original)
            }
            Some(member) if !member.contains('.') => {
                let module = self.module_of_alias.get(root)?;
                self.member_type(module, member)
            }
            Some(_) => None,
        }
    }

    fn member_type(&self, module: &str, member: &str) -> Option<String> {
        if TYPING_MODULES.contains(&module) {
            return TYPING_ALIASES
                .iter()
                .find(|(alias, _)| *alias == member)
                .map(|(_, canonical)| canonical.to_string());
        }
        if module == "collections" && COLLECTIONS_CLASSES.contains(&member) {
            return Some(format!("collections.{member}"));
        }
        if module == "pytest" {
            return PYTEST_PUBLIC_TYPES
                .iter()
                .find(|(name, _)| *name == member)
                .map(|(_, canonical)| canonical.to_string());
        }
        if module == "pathlib" && member == "Path" {
            return Some("pathlib.Path".into());
        }
        None
    }

    /// Whether `callee` names FastAPI's `Depends` or `Security` marker.
    fn names_dependency_marker(&self, callee: Node<'_>, source: &[u8]) -> bool {
        match callee.kind() {
            "identifier" => {
                self.from_imports
                    .get(text(callee, source))
                    .is_some_and(|(module, original)| {
                        (module == "fastapi" || module == "fastapi.params")
                            && FASTAPI_DEPENDENCY_MARKERS.contains(&original.as_str())
                    })
            }
            "attribute" => {
                let object = callee.child_by_field_name("object");
                let attribute = callee.child_by_field_name("attribute");
                matches!((object, attribute), (Some(object), Some(attribute))
                    if object.kind() == "identifier"
                        && self.module_of_alias.get(text(object, source)).map(String::as_str)
                            == Some("fastapi")
                        && FASTAPI_DEPENDENCY_MARKERS.contains(&text(attribute, source)))
            }
            _ => false,
        }
    }

    /// Whether the module imports FastAPI in any form.
    fn imports_fastapi(&self) -> bool {
        self.module_of_alias
            .values()
            .any(|module| module == "fastapi")
            || self
                .from_imports
                .values()
                .any(|(module, _)| module == "fastapi" || module.starts_with("fastapi."))
    }

    /// Whether `decorator` registers its function as a pytest fixture, and
    /// under which name when it overrides the function's own.
    fn fixture_decorator(&self, decorator: Node<'_>, source: &[u8]) -> Option<Option<String>> {
        let expression = decorator.named_child(0)?;
        let (callee, call) = match expression.kind() {
            "call" => (
                expression.child_by_field_name("function")?,
                Some(expression),
            ),
            _ => (expression, None),
        };
        if !self.names_fixture(callee, source) {
            return None;
        }
        let name = call
            .and_then(|call| keyword_string(call, "name", source))
            .filter(|name| !name.is_empty());
        Some(name)
    }

    fn names_fixture(&self, callee: Node<'_>, source: &[u8]) -> bool {
        match callee.kind() {
            "identifier" => self.fixture_aliases.contains(text(callee, source)),
            "attribute" => {
                let object = callee.child_by_field_name("object");
                let attribute = callee.child_by_field_name("attribute");
                matches!((object, attribute), (Some(object), Some(attribute))
                    if object.kind() == "identifier"
                        && self.pytest_aliases.contains(text(object, source))
                        && text(attribute, source) == "fixture")
            }
            _ => false,
        }
    }

    /// Whether `decorator` is `@pytest.mark.parametrize(...)`.
    fn parametrize_call<'tree>(
        &self,
        decorator: Node<'tree>,
        source: &[u8],
    ) -> Option<Node<'tree>> {
        let call = decorator.named_child(0).filter(|n| n.kind() == "call")?;
        let function = call.child_by_field_name("function")?;
        let spelled = text(function, source);
        let parametrize = self
            .pytest_aliases
            .iter()
            .any(|alias| spelled == format!("{alias}.mark.parametrize"))
            || spelled == "mark.parametrize";
        parametrize.then_some(call)
    }
}

/// The definition a module- or class-level statement declares, with the
/// decorated wrapper around it when there is one.
fn definition_of(node: Node<'_>) -> Option<(Node<'_>, Option<Node<'_>>)> {
    match node.kind() {
        "function_definition" | "class_definition" => Some((node, None)),
        "decorated_definition" => {
            let definition = node.child_by_field_name("definition")?;
            Some((definition, Some(node)))
        }
        _ => None,
    }
}

fn decorators_of<'tree>(wrapper: Option<Node<'tree>>) -> Vec<Node<'tree>> {
    let Some(wrapper) = wrapper else {
        return Vec::new();
    };
    let mut cursor = wrapper.walk();
    wrapper
        .named_children(&mut cursor)
        .filter(|child| child.kind() == "decorator")
        .collect()
}

/// What a class body says about the members read inside it.
#[derive(Default)]
struct ClassContext<'tree> {
    /// Decorators on the class, which parametrize every test in it.
    decorators: Vec<Node<'tree>>,
    /// Names the class body itself binds. An annotation in a method signature
    /// is evaluated in the class body, where these shadow the module's.
    bindings: HashSet<String>,
    /// Attributes `setUp` binds once for every `test*` method.
    setup_attributes: BTreeSet<String>,
    /// Whether the class names a base other than `object`, which can define a
    /// fixture its test methods see and the conftest walk does not.
    has_bases: bool,
}

fn read_class(
    class: Node<'_>,
    wrapper: Option<Node<'_>>,
    file: &FileContext,
    source: &[u8],
    facts: &mut FileFacts,
) {
    let Some(name) = class.child_by_field_name("name").map(|n| text(n, source)) else {
        return;
    };
    let class_decorators = decorators_of(wrapper);
    let Some(body) = class.child_by_field_name("body") else {
        return;
    };
    let has_bases = class
        .child_by_field_name("superclasses")
        .is_some_and(|bases| {
            let mut cursor = bases.walk();
            let named = bases
                .named_children(&mut cursor)
                .any(|base| text(base, source) != "object");
            named
        });
    let mut context = ClassContext {
        decorators: class_decorators,
        bindings: class_bindings(body, source),
        setup_attributes: BTreeSet::new(),
        has_bases,
    };
    let setup = if file.collects_unittest && !file.star_import {
        setup_attributes(body, source, &context.bindings)
    } else {
        BTreeMap::new()
    };
    context.setup_attributes = setup.keys().cloned().collect();
    // What the class's instances hold under each attribute it declares a
    // type for, for a call through `instance.attribute` elsewhere.
    let members = if file.star_import {
        BTreeMap::new()
    } else {
        class_members(body, file, &context.bindings, source)
    };
    // Every name the class binds, for a reader that has to know a name is
    // declared nowhere in a hierarchy.
    let bound_names = class_bound_names(class, wrapper, body, &context.bindings, source);
    let base_origins = base_origins(class, wrapper, file, source);
    let handed_over = !decorators_of(wrapper).is_empty()
        || names_keyword_base(class)
        || file.passed_names.contains(name);
    let typing_only = under_type_checking(class, source);
    let structural = names_protocol_base(class, source);
    if !members.is_empty()
        || bound_names.is_some()
        || !base_origins.is_empty()
        || handed_over
        || typing_only
        || structural
    {
        let mut ranges = vec![(class.start_byte(), class.end_byte())];
        if let Some(wrapper) = wrapper {
            ranges.push((wrapper.start_byte(), wrapper.end_byte()));
        }
        facts.by_node.push((
            ranges,
            name.to_string(),
            ScopeFacts {
                members,
                bound_names,
                base_origins,
                handed_over,
                typing_only,
                structural,
                ..ScopeFacts::default()
            },
        ));
    }
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        match definition_of(child) {
            Some((def, method_wrapper)) if def.kind() == "function_definition" => {
                let at = facts.by_node.len();
                read_function(
                    def,
                    method_wrapper,
                    Some(name),
                    &context,
                    file,
                    source,
                    facts,
                );
                // `setUp` supplies each attribute it binds once to the rest of
                // the class, from its own scope.
                let is_setup = def
                    .child_by_field_name("name")
                    .is_some_and(|method| text(method, source) == "setUp");
                if is_setup {
                    if let Some((_, _, scope)) = facts.by_node.get_mut(at) {
                        let census = Census::of(def, source);
                        for (attribute, value) in &setup {
                            let value = if census.blocked {
                                None
                            } else {
                                expression_origin(*value, &census, file, source)
                            };
                            scope.provides.push(ValueProvider {
                                kind: UNITTEST_SETUP_KIND.into(),
                                name: attribute.clone(),
                                owner: Some(name.to_string()),
                                value,
                                every_name: false,
                            });
                        }
                    }
                }
            }
            _ => {
                if let Some(provider) = assigned_fixture(child, Some(name), file, source) {
                    facts.module.provides.push(provider);
                }
            }
        }
    }
}

/// The decorators that make a method an attribute holding what the method
/// returns: `@property`, and `@cached_property` under any import of it.
fn is_property_decorator(decorator: Node<'_>, file: &FileContext, source: &[u8]) -> bool {
    let Some(expression) = decorator.named_child(0) else {
        return false;
    };
    let spelled = text(expression, source);
    match expression.kind() {
        "identifier" => {
            (spelled == "property" && !file.module_bindings.contains("property"))
                || spelled == "cached_property"
        }
        "attribute" => spelled.ends_with(".cached_property"),
        _ => false,
    }
}

/// Whether a decorator makes a method something other than an instance
/// method whose first parameter is the instance, or the accessor of another
/// attribute (`@json.setter`).
fn is_not_instance_method(decorator: Node<'_>, source: &[u8]) -> bool {
    let Some(expression) = decorator.named_child(0) else {
        return true;
    };
    let spelled = text(expression, source);
    matches!(spelled, "staticmethod" | "classmethod")
        || spelled.ends_with(".setter")
        || spelled.ends_with(".deleter")
        || spelled.ends_with(".getter")
}

/// What each attribute of the class's instances is declared to hold: an
/// annotated name in the class body, an annotated assignment to
/// `self.<attribute>` in one of its instance methods, or a property's
/// declared return. An attribute two declarations disagree about, or one
/// declared without a type the reader can name, maps to `None`, which still
/// shadows a base's declaration of the name.
fn class_members(
    body: Node<'_>,
    file: &FileContext,
    class_bound: &HashSet<String>,
    source: &[u8],
) -> BTreeMap<String, Option<ValueOrigin>> {
    let mut members: BTreeMap<String, Option<ValueOrigin>> = BTreeMap::new();
    let mut declare = |name: &str, origin: Option<ValueOrigin>| match members.get_mut(name) {
        Some(existing) => {
            if *existing != origin {
                *existing = None;
            }
        }
        None => {
            members.insert(name.to_string(), origin);
        }
    };
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        if child.kind() == "expression_statement" {
            let annotated = child
                .named_child(0)
                .filter(|node| node.kind() == "assignment")
                .and_then(|assignment| {
                    Some((
                        assignment.child_by_field_name("left")?,
                        assignment.child_by_field_name("type")?,
                    ))
                });
            if let Some((left, annotation)) = annotated {
                if left.kind() == "identifier" {
                    declare(
                        text(left, source),
                        annotation_origin(annotation, file, class_bound, source),
                    );
                }
            }
            continue;
        }
        let Some((def, wrapper)) = definition_of(child) else {
            continue;
        };
        if def.kind() != "function_definition" {
            continue;
        }
        let Some(method) = def.child_by_field_name("name").map(|n| text(n, source)) else {
            continue;
        };
        let decorators = decorators_of(wrapper);
        if decorators
            .iter()
            .any(|decorator| is_property_decorator(*decorator, file, source))
        {
            let origin = def
                .child_by_field_name("return_type")
                .and_then(|annotation| annotation_origin(annotation, file, class_bound, source));
            declare(method, origin);
            continue;
        }
        if decorators
            .iter()
            .any(|decorator| is_not_instance_method(*decorator, source))
            || !first_parameter_is_self(def, source)
        {
            continue;
        }
        let census = Census::of(def, source);
        // `self` rebound anywhere in the method is not the instance there.
        if census.blocked || census.count("self") != 1 {
            continue;
        }
        let local_names = census.names();
        let Some(method_body) = def.child_by_field_name("body") else {
            continue;
        };
        let mut stack = vec![method_body];
        while let Some(node) = stack.pop() {
            match node.kind() {
                "function_definition" | "class_definition" | "lambda" => continue,
                "assignment" => {
                    let annotated = node
                        .child_by_field_name("left")
                        .zip(node.child_by_field_name("type"));
                    if let Some((left, annotation)) = annotated {
                        if let Some(attribute) = self_attribute(left, source) {
                            declare(
                                attribute,
                                annotation_origin(annotation, file, &local_names, source),
                            );
                        }
                    }
                }
                _ => {}
            }
            let mut cursor = node.walk();
            stack.extend(node.named_children(&mut cursor));
        }
    }
    members
}

/// Text that lets a class hold a name its body never spells as a binding:
/// attributes written, removed or served by a computed name.
const COMPUTED_ATTRIBUTE_MARKS: [&str; 7] = [
    "setattr(",
    "delattr(",
    "__dict__",
    "__setattr__",
    "__getattr__",
    "__getattribute__",
    "__set_name__",
];

/// Every name the class binds for itself or its instances (see
/// [`ScopeFacts::bound_names`]), or `None` when its text does not say them
/// all: a decorator or a metaclass may add members, the body holds a wildcard
/// import, or an attribute is reached by a computed name.
fn class_bound_names(
    class: Node<'_>,
    wrapper: Option<Node<'_>>,
    body: Node<'_>,
    class_bound: &HashSet<String>,
    source: &[u8],
) -> Option<BTreeSet<String>> {
    if !decorators_of(wrapper).is_empty() || class_bound.contains("*") {
        return None;
    }
    let class_text = text(body, source);
    if names_keyword_base(class)
        || COMPUTED_ATTRIBUTE_MARKS
            .iter()
            .any(|mark| class_text.contains(mark))
    {
        return None;
    }
    let mut names: BTreeSet<String> = class_bound.iter().cloned().collect();
    attribute_writes(body, source, &mut names);
    Some(names)
}

/// Whether a class's base list holds a keyword: `metaclass=...`, or any
/// other a metaclass or `__init_subclass__` acts on.
fn names_keyword_base(class: Node<'_>) -> bool {
    class
        .child_by_field_name("superclasses")
        .is_some_and(|bases| {
            let mut cursor = bases.walk();
            let keyword = bases
                .named_children(&mut cursor)
                .any(|base| matches!(base.kind(), "keyword_argument" | "dictionary_splat"));
            keyword
        })
}

/// Bases that stand for no nominal class a value's own class must derive
/// from: a structural protocol, and an abstract base other classes register
/// with. A class that names one says nothing about its bases here.
const OPEN_BASES: [&str; 3] = ["typing.Protocol", "typing_extensions.Protocol", "abc.ABC"];

/// Bases that give a class no member a call spells by name.
const ROOT_BASES: [&str; 3] = [
    "builtins.object",
    "typing.Generic",
    "typing_extensions.Generic",
];

/// What each base of a class written at module level is (see
/// [`ScopeFacts::base_origins`]), by the base as the class's `Extends`
/// relation names it. A base list is evaluated in the scope around the class,
/// so only a module-level class is read: there a plain name is the module's,
/// or the language's own when the module never binds it. A subscripted base
/// (`Mapping[str, int]`) is the class it subscripts, and is keyed by that
/// class's name (`Mapping`), the name the recorded base carries.
fn base_origins(
    class: Node<'_>,
    wrapper: Option<Node<'_>>,
    file: &FileContext,
    source: &[u8],
) -> BTreeMap<String, Option<String>> {
    let mut origins = BTreeMap::new();
    let at_module_level = wrapper
        .unwrap_or(class)
        .parent()
        .is_some_and(|parent| parent.kind() == "module");
    let Some(bases) = class
        .child_by_field_name("superclasses")
        .filter(|_| at_module_level && !file.star_import)
    else {
        return origins;
    };
    let mut cursor = bases.walk();
    for base in bases.named_children(&mut cursor) {
        let named = python_base_class_node(base);
        if !matches!(named.kind(), "identifier" | "attribute") {
            continue;
        }
        let spelled = text(named, source);
        let qualified = match file.imported_name(spelled) {
            Some(qualified) => qualified,
            // A plain name the module never binds is the language's own.
            None if named.kind() == "identifier"
                && !file.module_bindings.contains(spelled)
                && !file.global_names.contains(spelled) =>
            {
                format!("builtins.{spelled}")
            }
            None => continue,
        };
        if OPEN_BASES.contains(&qualified.as_str()) {
            continue;
        }
        let origin = (!ROOT_BASES.contains(&qualified.as_str())).then_some(qualified);
        origins.insert(spelled.to_string(), origin);
    }
    origins
}

/// The attribute of every attribute write anywhere under `node`, whatever it
/// is written on: `self.x = ...`, `cls.x += ...`, `for other.x in ...`,
/// `del self.x`.
fn attribute_writes(node: Node<'_>, source: &[u8], names: &mut BTreeSet<String>) {
    let targets: Vec<Node<'_>> = match node.kind() {
        "assignment" | "augmented_assignment" | "for_statement" | "for_in_clause" => {
            node.child_by_field_name("left").into_iter().collect()
        }
        "as_pattern" => node.child_by_field_name("alias").into_iter().collect(),
        "named_expression" => node.child_by_field_name("name").into_iter().collect(),
        "delete_statement" => {
            let mut cursor = node.walk();
            node.named_children(&mut cursor).collect()
        }
        _ => Vec::new(),
    };
    for target in targets {
        let mut stack = vec![target];
        while let Some(target) = stack.pop() {
            if target.kind() == "attribute" {
                if let Some(attribute) = target.child_by_field_name("attribute") {
                    names.insert(text(attribute, source).to_string());
                }
                continue;
            }
            if matches!(
                target.kind(),
                "pattern_list"
                    | "tuple_pattern"
                    | "list_pattern"
                    | "tuple"
                    | "list"
                    | "expression_list"
                    | "as_pattern_target"
                    | "parenthesized_expression"
                    | "list_splat_pattern"
            ) {
                let mut cursor = target.walk();
                stack.extend(target.named_children(&mut cursor));
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        attribute_writes(child, source, names);
    }
}

/// Every name a class body binds directly: a method, a nested class, an
/// assignment or annotated target, an import.
fn class_bindings(body: Node<'_>, source: &[u8]) -> HashSet<String> {
    let mut bound = HashSet::new();
    let mut star = false;
    module_bindings(body, source, &mut bound, &mut star);
    if star {
        // A wildcard import in a class body binds any name.
        bound.insert("*".into());
    }
    bound
}

/// The attributes `setUp` binds for every `test*` method of its class, with
/// the expression each holds: `self.x = value` as a top-level statement of
/// `setUp`, the one write to `self.x` anywhere in the class, with nothing
/// after it that calls through `self` or `super()` (which could write it
/// again), in a class that binds no member of that name and never writes
/// attributes by name (`setattr`, `__dict__`).
fn setup_attributes<'tree>(
    body: Node<'tree>,
    source: &[u8],
    class_bound: &HashSet<String>,
) -> BTreeMap<String, Node<'tree>> {
    let mut found = BTreeMap::new();
    let class_text = text(body, source);
    if class_text.contains("setattr(")
        || class_text.contains("__dict__")
        || class_text.contains("delattr(")
        || class_text.contains("__setattr__")
        || class_bound.contains("*")
    {
        return found;
    }
    let mut writes: HashMap<String, usize> = HashMap::new();
    count_self_writes(body, source, &mut writes);
    let mut cursor = body.walk();
    let Some(setup) = body.named_children(&mut cursor).find_map(|child| {
        let (def, _) = definition_of(child)?;
        (def.kind() == "function_definition"
            && def
                .child_by_field_name("name")
                .is_some_and(|name| text(name, source) == "setUp"))
        .then_some(def)
    }) else {
        return found;
    };
    // `setUp(self)` and nothing else, so `self` is the instance.
    let takes_only_self = setup
        .child_by_field_name("parameters")
        .is_some_and(|params| {
            let mut cursor = params.walk();
            let names: Vec<&str> = params
                .named_children(&mut cursor)
                .map(|param| text(param, source))
                .collect();
            names == ["self"]
        });
    let Some(setup_body) = setup.child_by_field_name("body") else {
        return found;
    };
    if !takes_only_self {
        return found;
    }
    let statements: Vec<Node<'tree>> = {
        let mut cursor = setup_body.walk();
        setup_body.named_children(&mut cursor).collect()
    };
    for (at, statement) in statements.iter().enumerate() {
        let Some(assignment) = statement.named_child(0).filter(|node| {
            statement.kind() == "expression_statement" && node.kind() == "assignment"
        }) else {
            continue;
        };
        let (Some(left), Some(right)) = (
            assignment.child_by_field_name("left"),
            assignment.child_by_field_name("right"),
        ) else {
            continue;
        };
        if assignment.child_by_field_name("type").is_some() || right.kind() == "assignment" {
            continue;
        }
        let Some(attribute) = self_attribute(left, source) else {
            continue;
        };
        if writes.get(attribute) != Some(&1) || class_bound.contains(attribute) {
            continue;
        }
        let later_calls_through_self = statements[at + 1..]
            .iter()
            .any(|later| calls_through_self(*later, source));
        if later_calls_through_self {
            continue;
        }
        found.insert(attribute.to_string(), right);
    }
    found
}

/// `x` of a target written `self.x`.
fn self_attribute<'s>(target: Node<'_>, source: &'s [u8]) -> Option<&'s str> {
    if target.kind() != "attribute" {
        return None;
    }
    let object = target.child_by_field_name("object")?;
    if object.kind() != "identifier" || text(object, source) != "self" {
        return None;
    }
    Some(text(target.child_by_field_name("attribute")?, source))
}

/// How many times each `self.x` is a write target anywhere under `node`:
/// assignments, augmented assignments, loop and `with` targets, `del`.
fn count_self_writes(node: Node<'_>, source: &[u8], writes: &mut HashMap<String, usize>) {
    let targets: Vec<Node<'_>> = match node.kind() {
        "assignment" | "augmented_assignment" | "for_statement" | "for_in_clause" => {
            node.child_by_field_name("left").into_iter().collect()
        }
        "as_pattern" => node.child_by_field_name("alias").into_iter().collect(),
        "named_expression" => node.child_by_field_name("name").into_iter().collect(),
        "delete_statement" => {
            let mut cursor = node.walk();
            node.named_children(&mut cursor).collect()
        }
        _ => Vec::new(),
    };
    for target in targets {
        let mut stack = vec![target];
        while let Some(target) = stack.pop() {
            if let Some(attribute) = self_attribute(target, source) {
                *writes.entry(attribute.to_string()).or_default() += 1;
                continue;
            }
            if matches!(
                target.kind(),
                "pattern_list"
                    | "tuple_pattern"
                    | "list_pattern"
                    | "tuple"
                    | "list"
                    | "expression_list"
                    | "as_pattern_target"
                    | "parenthesized_expression"
                    | "list_splat_pattern"
            ) {
                let mut cursor = target.walk();
                stack.extend(target.named_children(&mut cursor));
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        count_self_writes(child, source, writes);
    }
}

/// Whether `node` holds a call through `self` or through `super()`.
fn calls_through_self(node: Node<'_>, source: &[u8]) -> bool {
    let mut stack = vec![node];
    while let Some(node) = stack.pop() {
        if node.kind() == "call" {
            let mut root = node.child_by_field_name("function");
            while let Some(current) = root {
                root = match current.kind() {
                    "attribute" => current.child_by_field_name("object"),
                    "call" => current.child_by_field_name("function"),
                    "identifier" => {
                        if matches!(text(current, source), "self" | "super") {
                            return true;
                        }
                        None
                    }
                    _ => None,
                };
            }
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    false
}

/// Every name a module or class scope binds outside nested function and class
/// bodies, and whether it holds a wildcard import.
fn module_bindings(scope: Node<'_>, source: &[u8], bound: &mut HashSet<String>, star: &mut bool) {
    let mut stack = vec![scope];
    while let Some(node) = stack.pop() {
        match node.kind() {
            "function_definition" | "class_definition" => {
                if let Some(name) = node.child_by_field_name("name") {
                    bound.insert(text(name, source).to_string());
                }
                continue;
            }
            "lambda" => continue,
            "wildcard_import" => *star = true,
            "import_statement" | "import_from_statement" => {
                let mut cursor = node.walk();
                for name in node.children_by_field_name("name", &mut cursor) {
                    let local = match name.kind() {
                        "aliased_import" => name.child_by_field_name("alias"),
                        _ => Some(name),
                    };
                    if let Some(local) = local {
                        let spelled = text(local, source);
                        bound.insert(spelled.split('.').next().unwrap_or(spelled).to_string());
                    }
                }
            }
            "assignment" | "augmented_assignment" | "for_statement" | "for_in_clause" => {
                if let Some(left) = node.child_by_field_name("left") {
                    bind_targets(left, source, bound);
                }
            }
            "as_pattern" => {
                if let Some(alias) = node.child_by_field_name("alias") {
                    bind_targets(alias, source, bound);
                }
            }
            "named_expression" => {
                if let Some(name) = node.child_by_field_name("name") {
                    bind_targets(name, source, bound);
                }
            }
            "global_statement" | "nonlocal_statement" => {
                let mut cursor = node.walk();
                for name in node.named_children(&mut cursor) {
                    bound.insert(text(name, source).to_string());
                }
            }
            _ => {}
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
}

/// How many times the module scope binds `name` outside nested function and
/// class bodies, by every binding form [`module_bindings`] reads.
fn module_binding_count(scope: Node<'_>, source: &[u8], name: &str) -> usize {
    module_binding_counts(scope, source)
        .get(name)
        .copied()
        .unwrap_or(0)
}

/// How many times the module scope binds each name outside nested function
/// and class bodies, by every binding form [`module_bindings`] reads.
fn module_binding_counts(scope: Node<'_>, source: &[u8]) -> HashMap<String, usize> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    let mut stack = vec![scope];
    while let Some(node) = stack.pop() {
        let mut bound = HashSet::new();
        match node.kind() {
            "function_definition" | "class_definition" => {
                if let Some(defined) = node.child_by_field_name("name") {
                    *counts.entry(text(defined, source).to_string()).or_default() += 1;
                }
                continue;
            }
            "lambda" => continue,
            "import_statement" | "import_from_statement" => {
                let mut star = false;
                module_bindings(node, source, &mut bound, &mut star);
                for name in bound {
                    *counts.entry(name).or_default() += 1;
                }
                continue;
            }
            "assignment" | "augmented_assignment" | "for_statement" | "for_in_clause" => {
                if let Some(left) = node.child_by_field_name("left") {
                    bind_targets(left, source, &mut bound);
                }
            }
            "as_pattern" => {
                if let Some(alias) = node.child_by_field_name("alias") {
                    bind_targets(alias, source, &mut bound);
                }
            }
            "named_expression" => {
                if let Some(target) = node.child_by_field_name("name") {
                    bind_targets(target, source, &mut bound);
                }
            }
            "global_statement" | "nonlocal_statement" => {
                let mut cursor = node.walk();
                for declared in node.named_children(&mut cursor) {
                    bound.insert(text(declared, source).to_string());
                }
            }
            _ => {}
        }
        for name in bound {
            *counts.entry(name).or_default() += 1;
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    counts
}

/// Every name a `global` statement anywhere in the module declares.
fn global_names(root: Node<'_>, source: &[u8]) -> HashSet<String> {
    let mut names = HashSet::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        let mut cursor = node.walk();
        if node.kind() == "global_statement" {
            names.extend(
                node.named_children(&mut cursor)
                    .map(|declared| text(declared, source).to_string()),
            );
            continue;
        }
        stack.extend(node.named_children(&mut cursor));
    }
    names
}

/// Callees that read a class they are passed and keep nothing of it.
const CLASS_READERS: [&str; 3] = ["isinstance", "issubclass", "super"];

/// Every plain name the module passes as an argument of a call, positional
/// or by keyword, other than to one of [`CLASS_READERS`].
fn passed_names(root: Node<'_>, source: &[u8]) -> HashSet<String> {
    let mut names = HashSet::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        let mut cursor = node.walk();
        if node.kind() == "call" {
            let reader = node
                .child_by_field_name("function")
                .is_some_and(|callee| CLASS_READERS.contains(&text(callee, source)));
            if let Some(arguments) = node.child_by_field_name("arguments").filter(|_| !reader) {
                let mut held = arguments.walk();
                for argument in arguments.named_children(&mut held) {
                    let value = match argument.kind() {
                        "keyword_argument" => argument.child_by_field_name("value"),
                        _ => Some(argument),
                    };
                    if let Some(value) = value.filter(|value| value.kind() == "identifier") {
                        names.insert(text(value, source).to_string());
                    }
                }
            }
        }
        stack.extend(node.named_children(&mut cursor));
    }
    names
}

fn bind_targets(target: Node<'_>, source: &[u8], bound: &mut HashSet<String>) {
    match target.kind() {
        "identifier" => {
            bound.insert(text(target, source).to_string());
        }
        "attribute" | "subscript" => {}
        _ => {
            let mut cursor = target.walk();
            for child in target.named_children(&mut cursor) {
                bind_targets(child, source, bound);
            }
        }
    }
}

/// `app = pytest.fixture(make_app)` registers a fixture too. Its value is not
/// read, but it must still shadow a farther fixture of the same name, so it
/// is recorded with no value.
fn assigned_fixture(
    statement: Node<'_>,
    owner: Option<&str>,
    file: &FileContext,
    source: &[u8],
) -> Option<ValueProvider> {
    if !file.collects || statement.kind() != "expression_statement" {
        return None;
    }
    let assignment = statement
        .named_child(0)
        .filter(|n| n.kind() == "assignment")?;
    let left = assignment.child_by_field_name("left")?;
    let right = assignment.child_by_field_name("right")?;
    if left.kind() != "identifier" || right.kind() != "call" {
        return None;
    }
    let mut callee = right.child_by_field_name("function")?;
    if callee.kind() == "call" {
        callee = callee.child_by_field_name("function")?;
    }
    if !file.names_fixture(callee, source) {
        return None;
    }
    let name = keyword_string(right, "name", source).unwrap_or_else(|| text(left, source).into());
    Some(ValueProvider {
        kind: PYTEST_FIXTURE_KIND.into(),
        name,
        owner: owner.map(str::to_string),
        value: None,
        every_name: false,
    })
}

#[allow(clippy::too_many_arguments)]
fn read_function(
    function: Node<'_>,
    wrapper: Option<Node<'_>>,
    class: Option<&str>,
    class_context: &ClassContext<'_>,
    file: &FileContext,
    source: &[u8],
    facts: &mut FileFacts,
) {
    let Some(name) = function
        .child_by_field_name("name")
        .map(|n| text(n, source))
    else {
        return;
    };
    let decorators = decorators_of(wrapper);
    let fixture = decorators
        .iter()
        .find_map(|decorator| file.fixture_decorator(*decorator, source));
    let is_test = name.starts_with("test") && class.is_none_or(|class| class.starts_with("Test"));
    // pytest injects a fixture's own parameters wherever the fixture is
    // declared, a plugin module included, and a test's only where it collects
    // the test.
    let injectable = !file.pytestmark && (fixture.is_some() || (file.collects && is_test));
    let mut parametrized = BTreeSet::new();
    let mut parametrize_unknown = false;
    for decorator in decorators.iter().chain(&class_context.decorators) {
        if let Some(call) = file.parametrize_call(*decorator, source) {
            match parametrize_names(call, source) {
                Some(names) => parametrized.extend(names),
                None => parametrize_unknown = true,
            }
        }
    }

    let census = Census::of(function, source);
    let mut scope = ScopeFacts::default();
    // A name a test parametrizes directly is the parameter's value for every
    // fixture the test's request reaches, not only for the test's own
    // parameter, so a lookup made on the test's behalf must not answer it
    // from a fixture.
    if is_test && injectable && !parametrize_unknown {
        scope.withheld_names = parametrized.clone();
    }
    // A signature's annotations are evaluated where the definition sits: in
    // the class body for a method, whose own names shadow the module's.
    let signature_shadowed = &class_context.bindings;
    if !census.blocked {
        if let Some(params) = function.child_by_field_name("parameters") {
            let mut cursor = params.walk();
            for param in params.named_children(&mut cursor) {
                let Some((param_name, origin)) =
                    parameter_origin(param, file, signature_shadowed, source)
                else {
                    continue;
                };
                if census.count(&param_name) != 1 {
                    continue;
                }
                let origin = match origin {
                    Some(origin) => Some(origin),
                    None if injectable
                        && !parametrize_unknown
                        && !parametrized.contains(&param_name)
                        && param_name != "self"
                        && param_name != "cls" =>
                    {
                        Some(ValueOrigin::Provided {
                            lookup: ProviderLookup::DirectoryChain {
                                kind: PYTEST_FIXTURE_KIND.into(),
                                file_name: CONFTEST.into(),
                                blockers: FIXTURE_BLOCKERS.iter().map(|s| s.to_string()).collect(),
                                // A base class can define a fixture of the
                                // name that the walk never reads.
                                otherwise: (!class_context.has_bases)
                                    .then(|| builtin_fixture(&param_name))
                                    .flatten()
                                    .map(Box::new),
                            },
                            name: param_name.clone(),
                        })
                    }
                    None => None,
                };
                if let Some(origin) = origin {
                    scope.bindings.insert(param_name, origin);
                }
            }
        }
        if let Some(body) = function.child_by_field_name("body") {
            collect_local_origins(body, &census, file, source, &mut scope.bindings);
            collect_global_roots(body, &census, file, source, &mut scope.bindings);
        }
        // Every `test*` method of a unittest class reads what `setUp` bound.
        if class.is_some()
            && name.starts_with("test")
            && census.count("self") == 1
            && first_parameter_is_self(function, source)
        {
            for attribute in &class_context.setup_attributes {
                scope.bindings.insert(
                    format!("self.{attribute}"),
                    ValueOrigin::Provided {
                        lookup: ProviderLookup::Owner {
                            kind: UNITTEST_SETUP_KIND.into(),
                        },
                        name: attribute.clone(),
                    },
                );
            }
        }
    }
    scope.returns = function
        .child_by_field_name("return_type")
        .and_then(|annotation| declared_return(annotation, file, signature_shadowed, source));
    let generator = function
        .child_by_field_name("body")
        .is_some_and(|body| holds_yield(body));
    let is_fixture = fixture.is_some();
    if let Some(override_name) = fixture {
        let value = fixture_value(function, generator, &census, file, source);
        scope.provides.push(ValueProvider {
            kind: PYTEST_FIXTURE_KIND.into(),
            name: override_name.unwrap_or_else(|| name.to_string()),
            owner: class.map(str::to_string),
            // A wildcard import may rebind the fixture's name after it.
            value: value.filter(|_| !file.star_import),
            every_name: false,
        });
    } else if class.is_none()
        && !census.blocked
        && (generator
            || (file.imports_fastapi() && function.child_by_field_name("return_type").is_none()))
    {
        // What a framework that calls this function for a value receives.
        scope.supplies = fixture_value(function, generator, &census, file, source);
    }
    // What a call of a plain function returns, when it declares nothing. A
    // decorator may hand callers another function, a generator's or a
    // coroutine's call returns no value its `return` writes, and a wildcard
    // import may rebind any name the value is read through.
    if !is_fixture
        && class.is_none()
        && !census.blocked
        && !generator
        && decorators.is_empty()
        && !file.star_import
        && !is_async(function)
        && function.child_by_field_name("return_type").is_none()
    {
        scope.returned = fixture_value(function, false, &census, file, source);
    }
    // A function nested in this one reads what this one binds.
    if !census.blocked && !file.star_import {
        if let Some(body) = function.child_by_field_name("body") {
            read_nested_functions(body, &scope.bindings, &census.names(), file, source, facts);
        }
    }
    let mut ranges = vec![(function.start_byte(), function.end_byte())];
    if let Some(wrapper) = wrapper {
        ranges.push((wrapper.start_byte(), wrapper.end_byte()));
    }
    facts.by_node.push((ranges, name.to_string(), scope));
}

/// Read every function defined directly in `body`, an enclosing function's
/// body, and every function nested in those in turn.
///
/// A nested function reads a name it does not bind from the function around
/// it (Python's enclosing scope), and its decorators are evaluated there too:
/// `@app.route("/")` on a view a test declares reads the test's `app`. Such
/// a name is recorded as [`ValueOrigin::Enclosing`] when the enclosing
/// function records a binding for it (`outer`), which it does only for a
/// name bound once anywhere in it, nested scopes included. A name an
/// enclosing function binds more than once (`outer_bound`) holds no single
/// value, and is never read as the module's. A decorator's name the nested
/// function binds itself is read in two scopes at once, so the nested
/// function records no origin for it. A function inside a class nested here
/// is left alone: the class body is a scope of its own.
fn read_nested_functions(
    body: Node<'_>,
    outer: &BTreeMap<String, ValueOrigin>,
    outer_bound: &HashSet<String>,
    file: &FileContext,
    source: &[u8],
    facts: &mut FileFacts,
) {
    let mut stack = vec![body];
    while let Some(node) = stack.pop() {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            match definition_of(child) {
                Some((function, wrapper)) if function.kind() == "function_definition" => {
                    read_nested_function(
                        function,
                        wrapper,
                        outer,
                        outer_bound,
                        file,
                        source,
                        facts,
                    );
                }
                Some(_) => {}
                None if matches!(child.kind(), "class_definition" | "lambda") => {}
                None => stack.push(child),
            }
        }
    }
}

fn read_nested_function(
    function: Node<'_>,
    wrapper: Option<Node<'_>>,
    outer: &BTreeMap<String, ValueOrigin>,
    outer_bound: &HashSet<String>,
    file: &FileContext,
    source: &[u8],
    facts: &mut FileFacts,
) {
    let (Some(name), Some(body)) = (
        function
            .child_by_field_name("name")
            .map(|name| text(name, source)),
        function.child_by_field_name("body"),
    ) else {
        return;
    };
    let census = Census::of(function, source);
    if census.blocked {
        return;
    }
    let decorators = decorators_of(wrapper);
    // Names read here or in a function nested here: a read two levels down
    // reaches the test through this function's record of it.
    let mut read_names: BTreeSet<String> = attribute_roots(body, source, true);
    let decorator_names: BTreeSet<String> = decorators
        .iter()
        .flat_map(|decorator| attribute_roots(*decorator, source, false))
        .collect();
    read_names.extend(decorator_names.iter().cloned());
    let mut scope = ScopeFacts::default();
    for read in &read_names {
        if census.count(read) == 0 && outer.contains_key(read) {
            scope
                .bindings
                .insert(read.clone(), ValueOrigin::Enclosing { name: read.clone() });
        }
    }
    collect_local_origins(body, &census, file, source, &mut scope.bindings);
    collect_global_roots(body, &census, file, source, &mut scope.bindings);
    // A decorator is evaluated where the definition sits, so a name it reads
    // off the module counts as the module's only when the nested function
    // does not bind it either.
    for read in &decorator_names {
        if census.count(read) == 0
            && file.module_origin_names.contains(read.as_str())
            && !scope.bindings.contains_key(read)
        {
            scope
                .bindings
                .insert(read.clone(), ValueOrigin::Global { name: read.clone() });
        }
    }
    scope.bindings.retain(|bound, origin| {
        // An enclosing function's name is never the module's.
        let shadowed_global =
            matches!(origin, ValueOrigin::Global { .. }) && outer_bound.contains(bound);
        // A decorator reads it in the enclosing scope, the body in this one.
        let two_scopes = decorator_names.contains(bound) && census.count(bound) > 0;
        !shadowed_global && !two_scopes
    });
    let mut inner_bound = outer_bound.clone();
    inner_bound.extend(census.names());
    read_nested_functions(body, &scope.bindings, &inner_bound, file, source, facts);
    if scope.bindings.is_empty() {
        return;
    }
    let mut ranges = vec![(function.start_byte(), function.end_byte())];
    if let Some(wrapper) = wrapper {
        ranges.push((wrapper.start_byte(), wrapper.end_byte()));
    }
    facts.by_node.push((ranges, name.to_string(), scope));
}

/// The name at the root of every attribute read in `node`: `app` in
/// `app.route("/")`. `nested` says whether functions nested in it are read
/// too; a class body and a lambda never are.
fn attribute_roots(node: Node<'_>, source: &[u8], nested: bool) -> BTreeSet<String> {
    let mut roots = BTreeSet::new();
    let mut stack = vec![node];
    while let Some(node) = stack.pop() {
        match node.kind() {
            "function_definition" if !nested => continue,
            "class_definition" | "lambda" => continue,
            "attribute" => {
                let mut root = node;
                while root.kind() == "attribute" {
                    match root.child_by_field_name("object") {
                        Some(object) => root = object,
                        None => break,
                    }
                }
                if root.kind() == "identifier" {
                    roots.insert(text(root, source).to_string());
                }
            }
            _ => {}
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    roots
}

/// Whether a function is written `async def`.
fn is_async(function: Node<'_>) -> bool {
    let mut cursor = function.walk();
    let found = function
        .children(&mut cursor)
        .take_while(|child| child.kind() != "def")
        .any(|child| child.kind() == "async");
    found
}

/// What pytest's built-in fixture of this name supplies.
fn builtin_fixture(name: &str) -> Option<ValueOrigin> {
    let (_, type_name) = PYTEST_BUILTIN_FIXTURES
        .iter()
        .find(|(fixture, _)| *fixture == name)?;
    Some(exact(type_name))
}

/// A value whose class is exactly `type_name`, which the repository does not
/// declare.
fn exact(type_name: &str) -> ValueOrigin {
    ValueOrigin::External {
        type_name: type_name.to_string(),
        reach: ExternalReach::Exact,
        own_members: Vec::new(),
    }
}

/// Whether the function's first parameter is a plain `self`.
fn first_parameter_is_self(function: Node<'_>, source: &[u8]) -> bool {
    let Some(params) = function.child_by_field_name("parameters") else {
        return false;
    };
    let mut cursor = params.walk();
    let first = params.named_children(&mut cursor).next();
    first.is_some_and(|param| param.kind() == "identifier" && text(param, source) == "self")
}

/// The argument names one `parametrize` call names, or `None` when its first
/// argument is not a literal the reader can list.
fn parametrize_names(call: Node<'_>, source: &[u8]) -> Option<Vec<String>> {
    let arguments = call.child_by_field_name("arguments")?;
    let mut cursor = arguments.walk();
    let first = arguments.named_children(&mut cursor).next()?;
    let first = if first.kind() == "keyword_argument" {
        if first
            .child_by_field_name("name")
            .is_some_and(|name| text(name, source) == "argnames")
        {
            first.child_by_field_name("value")?
        } else {
            return None;
        }
    } else {
        first
    };
    match first.kind() {
        "string" => Some(
            string_literal(first, source)?
                .split(',')
                .map(|name| name.trim().to_string())
                .filter(|name| !name.is_empty())
                .collect(),
        ),
        "list" | "tuple" => {
            let mut names = Vec::new();
            let mut items = first.walk();
            for item in first.named_children(&mut items) {
                names.push(string_literal(item, source)?.trim().to_string());
            }
            Some(names)
        }
        _ => None,
    }
}

/// The name a parameter binds, with its declared origin when it declares a
/// single type, or the dependency FastAPI supplies it from. `None` for a
/// splat, and for an untyped parameter with any other default, whose value may
/// be the default.
fn parameter_origin(
    param: Node<'_>,
    file: &FileContext,
    shadowed: &HashSet<String>,
    source: &[u8],
) -> Option<(String, Option<ValueOrigin>)> {
    match param.kind() {
        "identifier" => Some((text(param, source).to_string(), None)),
        "typed_parameter" => {
            // pytest injects a fixture whatever the parameter's annotation
            // says, so an annotation naming no single type leaves the
            // parameter exactly as an untyped one.
            let name = param.named_child(0).filter(|n| n.kind() == "identifier")?;
            let annotation = param.child_by_field_name("type");
            let origin = annotation
                .and_then(|annotation| annotation_origin(annotation, file, shadowed, source))
                .or_else(|| {
                    // `Annotated[T, Depends(provider)]` with a `T` no fact reads.
                    annotation
                        .and_then(|annotation| dependency_in(annotation, file, source))
                        .map(|callee| ValueOrigin::Supplied { callee })
                });
            Some((text(name, source).to_string(), origin))
        }
        "typed_default_parameter" => {
            let name = param.child_by_field_name("name")?;
            let annotation = param.child_by_field_name("type")?;
            let origin = annotation_origin(annotation, file, shadowed, source).or_else(|| {
                param
                    .child_by_field_name("value")
                    .and_then(|default| dependency_marker(default, file, source))
                    .map(|callee| ValueOrigin::Supplied { callee })
            })?;
            Some((text(name, source).to_string(), Some(origin)))
        }
        "default_parameter" => {
            let name = param
                .child_by_field_name("name")
                .filter(|name| name.kind() == "identifier")?;
            let callee = dependency_marker(param.child_by_field_name("value")?, file, source)?;
            Some((
                text(name, source).to_string(),
                Some(ValueOrigin::Supplied { callee }),
            ))
        }
        _ => None,
    }
}

/// The dependency a FastAPI `Depends(provider)` or `Security(provider)`
/// default names, as a plain or dotted name.
fn dependency_marker(default: Node<'_>, file: &FileContext, source: &[u8]) -> Option<String> {
    if default.kind() != "call" {
        return None;
    }
    let function = default.child_by_field_name("function")?;
    if !file.names_dependency_marker(function, source) {
        return None;
    }
    let arguments = default.child_by_field_name("arguments")?;
    let mut cursor = arguments.walk();
    let dependency =
        arguments
            .named_children(&mut cursor)
            .find_map(|argument| match argument.kind() {
                "keyword_argument" => argument
                    .child_by_field_name("name")
                    .filter(|name| text(*name, source) == "dependency")
                    .and_then(|_| argument.child_by_field_name("value")),
                _ => Some(argument),
            })?;
    let spelled = text(dependency, source);
    (matches!(dependency.kind(), "identifier" | "attribute")
        && spelled.split('.').all(is_identifier))
    .then(|| spelled.to_string())
}

/// The dependency a `Depends(...)` inside an annotation names.
fn dependency_in(annotation: Node<'_>, file: &FileContext, source: &[u8]) -> Option<String> {
    let mut stack = vec![annotation];
    let mut found = None;
    while let Some(node) = stack.pop() {
        if let Some(callee) = dependency_marker(node, file, source) {
            if found.is_some() {
                return None;
            }
            found = Some(callee);
            continue;
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    found
}

/// Record every local the body binds exactly once to a value whose origin
/// the reader can name.
fn collect_local_origins(
    node: Node<'_>,
    census: &Census,
    file: &FileContext,
    source: &[u8],
    bindings: &mut BTreeMap<String, ValueOrigin>,
) {
    // A local annotation is read in the function's own scope.
    let local_names = census.names();
    let mut stack = vec![node];
    while let Some(node) = stack.pop() {
        match node.kind() {
            // Another scope's statements bind that scope's names.
            "function_definition" | "class_definition" | "lambda" => continue,
            "assignment" => {
                if let (Some(left), Some(right)) = (
                    node.child_by_field_name("left"),
                    node.child_by_field_name("right"),
                ) {
                    if left.kind() == "identifier" {
                        let name = text(left, source);
                        if census.count(name) == 1 {
                            let declared =
                                node.child_by_field_name("type").and_then(|annotation| {
                                    annotation_origin(annotation, file, &local_names, source)
                                });
                            if let Some(origin) =
                                declared.or_else(|| expression_origin(right, census, file, source))
                            {
                                bindings.insert(name.to_string(), origin);
                            }
                        }
                    }
                }
            }
            "with_item" => {
                if let Some((value, alias)) = with_item_parts(node) {
                    if alias.kind() == "identifier" {
                        let name = text(alias, source);
                        if census.count(name) == 1 {
                            if let Some(value) = expression_origin(value, census, file, source) {
                                bindings.insert(
                                    name.to_string(),
                                    ValueOrigin::Entered {
                                        value: Box::new(value),
                                        method: "__enter__".into(),
                                    },
                                );
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
}

/// The context expression and the single name a `with` item binds.
fn with_item_parts(item: Node<'_>) -> Option<(Node<'_>, Node<'_>)> {
    let value = item.child_by_field_name("value")?;
    if value.kind() != "as_pattern" {
        return None;
    }
    let expression = value.named_child(0)?;
    let alias = value.child_by_field_name("alias")?;
    let target = if alias.kind() == "as_pattern_target" {
        alias.named_child(0)?
    } else {
        alias
    };
    Some((expression, target))
}

/// The origin of the value an expression evaluates to, when it is one the
/// linker can follow: a name, a call to a name, a method call on one, or a
/// literal or built-in construction whose class is exactly a built-in type.
fn expression_origin(
    expression: Node<'_>,
    census: &Census,
    file: &FileContext,
    source: &[u8],
) -> Option<ValueOrigin> {
    match expression.kind() {
        "parenthesized_expression" => {
            let inner = expression.named_child(0)?;
            expression_origin(inner, census, file, source)
        }
        "identifier" => {
            let name = text(expression, source);
            if name == "self" || name == "cls" {
                return None;
            }
            Some(ValueOrigin::Local {
                name: name.to_string(),
            })
        }
        "dictionary" | "dictionary_comprehension" => Some(exact("dict")),
        "list" | "list_comprehension" => Some(exact("list")),
        "set" | "set_comprehension" => Some(exact("set")),
        "tuple" => Some(exact("tuple")),
        "string" | "concatenated_string" => {
            let first = if expression.kind() == "string" {
                expression
            } else {
                expression.named_child(0)?
            };
            let prefix: String = text(first, source)
                .chars()
                .take_while(|ch| ch.is_ascii_alphabetic())
                .collect();
            Some(exact(if prefix.contains(['b', 'B']) {
                "bytes"
            } else {
                "str"
            }))
        }
        "call" => {
            let function = expression.child_by_field_name("function")?;
            match function.kind() {
                "identifier" => {
                    let callee = text(function, source);
                    // A callee the function binds itself is a local value,
                    // not the module-level name the linker would resolve.
                    if census.count(callee) > 0 {
                        return None;
                    }
                    if let Some(built) = constructed_type(callee, file, census) {
                        return Some(exact(&built));
                    }
                    Some(called(callee, file))
                }
                "attribute" => {
                    let object = function.child_by_field_name("object")?;
                    let method = text(function.child_by_field_name("attribute")?, source);
                    if let Some(path) = module_path(function, file, census, source) {
                        if let Some(built) = constructed_type(&path, file, census) {
                            return Some(exact(&built));
                        }
                        return Some(called(&path, file));
                    }
                    let receiver = expression_origin(object, census, file, source)?;
                    Some(ValueOrigin::MethodCall {
                        receiver: Box::new(receiver),
                        method: method.to_string(),
                    })
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// What calling `callee` returns, a name no nearer scope binds: the callee
/// with the dotted name it is imported under, when the module binds its
/// first name by one import and by nothing else.
fn called(callee: &str, file: &FileContext) -> ValueOrigin {
    match file.imported_name(callee) {
        Some(qualified) => ValueOrigin::ImportedCall {
            callee: callee.to_string(),
            qualified,
        },
        None => ValueOrigin::Call {
            callee: callee.to_string(),
        },
    }
}

/// The class a call to `callee` constructs when it is exactly a type the
/// repository does not declare: a built-in type or a built-in function that
/// returns one, a `collections` class, `pathlib.Path`. Never a `typing` alias,
/// which is not a constructor.
fn constructed_type(callee: &str, file: &FileContext, census: &Census) -> Option<String> {
    if !callee.contains('.') {
        if file.builtin_unshadowed(callee, census) {
            if BUILTIN_TYPES.contains(&callee) {
                return Some(callee.to_string());
            }
            if let Some((_, result)) = BUILTIN_RESULTS.iter().find(|(name, _)| *name == callee) {
                return Some(result.to_string());
            }
        }
        if census.count(callee) > 0 {
            return None;
        }
    }
    let root = callee.split('.').next()?;
    if census.count(root) > 0 {
        return None;
    }
    let named = file.external_type(callee, &HashSet::new())?;
    (named.starts_with("collections.") || named == "pathlib.Path").then_some(named)
}

/// `flask.Flask` when the attribute chain starts at a module the file
/// imports and names nothing but identifiers: a qualified callee rather than a
/// method call on a value.
fn module_path(
    attribute: Node<'_>,
    file: &FileContext,
    census: &Census,
    source: &[u8],
) -> Option<String> {
    let spelled = text(attribute, source);
    let root = spelled.split('.').next()?;
    let plain = spelled
        .split('.')
        .all(|segment| !segment.is_empty() && is_identifier(segment));
    (plain && file.module_aliases.contains(root) && census.count(root) == 0)
        .then(|| spelled.to_string())
}

/// What a fixture supplies: its declared return (the yielded type of a
/// declared generator), else the one origin every `return` or `yield` in its
/// own body agrees on.
fn fixture_value(
    function: Node<'_>,
    generator: bool,
    census: &Census,
    file: &FileContext,
    source: &[u8],
) -> Option<ValueOrigin> {
    if let Some(annotation) = function.child_by_field_name("return_type") {
        let no_names = HashSet::new();
        return if generator {
            let spelled = annotation_text(annotation, source)?;
            let inner = unwrap_generic(&spelled, GENERATOR_WRAPPERS)?;
            let first = split_top_level(inner, ',').into_iter().next()?;
            spelled_origin(first, file, &no_names)
        } else {
            annotation_origin(annotation, file, &no_names, source)
        };
    }
    let body = function.child_by_field_name("body")?;
    let mut values = Vec::new();
    let mut stack = vec![body];
    while let Some(node) = stack.pop() {
        match node.kind() {
            "function_definition" | "class_definition" | "lambda" => continue,
            "yield" if generator => {
                let value = node.named_child(0)?;
                values.push(expression_origin(value, census, file, source)?);
                continue;
            }
            "return_statement" if !generator => {
                let value = node.named_child(0)?;
                values.push(expression_origin(value, census, file, source)?);
                continue;
            }
            _ => {}
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    let first = values.first()?.clone();
    values.iter().all(|value| *value == first).then_some(first)
}

fn holds_yield(node: Node<'_>) -> bool {
    let mut stack = vec![node];
    while let Some(node) = stack.pop() {
        match node.kind() {
            "function_definition" | "class_definition" | "lambda" => continue,
            "yield" => return true,
            _ => {}
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    false
}

/// A callable's declared return.
fn declared_return(
    annotation: Node<'_>,
    file: &FileContext,
    shadowed: &HashSet<String>,
    source: &[u8],
) -> Option<DeclaredReturn> {
    let spelled = annotation_text(annotation, source)?;
    if SELF_TYPES.contains(&spelled.as_str()) {
        return Some(DeclaredReturn::Receiver);
    }
    match spelled_origin(&spelled, file, shadowed)? {
        ValueOrigin::Declared { type_name } => Some(DeclaredReturn::Type { type_name }),
        ValueOrigin::External {
            type_name, reach, ..
        } => Some(DeclaredReturn::External { type_name, reach }),
        _ => None,
    }
}

/// What an annotation declares a value to be: one class the linker locates,
/// or a type the repository does not declare. `shadowed` holds the names the
/// scope the annotation is read in rebinds.
fn annotation_origin(
    annotation: Node<'_>,
    file: &FileContext,
    shadowed: &HashSet<String>,
    source: &[u8],
) -> Option<ValueOrigin> {
    let spelled = annotation_text(annotation, source)?;
    spelled_origin(&spelled, file, shadowed)
}

/// [`annotation_origin`] over the annotation's text: a name or dotted path,
/// `Optional[T]`, `T | None` or `Annotated[T, ...]` around one, or a built-in,
/// `typing` or pytest type with or without type arguments. `None` for every
/// other shape, a union of two types above all, because the value's type is
/// then undecided.
fn spelled_origin(
    spelled: &str,
    file: &FileContext,
    shadowed: &HashSet<String>,
) -> Option<ValueOrigin> {
    let spelled = spelled.trim();
    let parts = split_top_level(spelled, '|');
    if parts.len() > 1 {
        let named: Vec<&str> = parts.into_iter().filter(|part| *part != "None").collect();
        return match named.as_slice() {
            [one] => spelled_origin(one, file, shadowed),
            _ => None,
        };
    }
    if let Some(inner) = unwrap_generic(spelled, &["Optional"]) {
        return spelled_origin(inner, file, shadowed);
    }
    if let Some(inner) = unwrap_generic(spelled, &["Annotated"]) {
        let first = split_top_level(inner, ',').into_iter().next()?;
        return spelled_origin(first, file, shadowed);
    }
    let base = spelled
        .split_once('[')
        .map_or(spelled, |(base, _)| base)
        .trim();
    if let Some(type_name) = file.external_type(base, shadowed) {
        return Some(ValueOrigin::External {
            type_name,
            reach: ExternalReach::Nominal {
                roots: PYTHON_ROOTS.iter().map(|root| root.to_string()).collect(),
            },
            own_members: Vec::new(),
        });
    }
    normalize_type(spelled).map(|type_name| ValueOrigin::Declared { type_name })
}

fn annotation_text(annotation: Node<'_>, source: &[u8]) -> Option<String> {
    let raw = text(annotation, source).trim();
    let unquoted = raw
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| raw.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
        .unwrap_or(raw);
    let collapsed: String = unquoted.split_whitespace().collect::<Vec<_>>().join(" ");
    (!collapsed.is_empty()).then_some(collapsed)
}

/// A plain name or dotted path naming a class the linker may locate.
fn normalize_type(spelled: &str) -> Option<String> {
    let spelled = spelled.trim();
    let plain = spelled
        .split('.')
        .all(|segment| !segment.is_empty() && is_identifier(segment));
    let builtin = matches!(
        spelled,
        "Any"
            | "t.Any"
            | "typing.Any"
            | "object"
            | "None"
            | "str"
            | "int"
            | "float"
            | "bool"
            | "bytes"
            | "dict"
            | "list"
            | "tuple"
            | "set"
    );
    (plain && !builtin).then(|| spelled.to_string())
}

/// The argument list of `Wrapper[...]` when the wrapper's last segment is one
/// of `wrappers`.
fn unwrap_generic<'a>(spelled: &'a str, wrappers: &[&str]) -> Option<&'a str> {
    let open = spelled.find('[')?;
    let inner = spelled.strip_suffix(']')?.get(open + 1..)?;
    let wrapper = &spelled[..open];
    let leaf = wrapper.rsplit('.').next()?;
    wrappers.contains(&leaf).then_some(inner)
}

/// `inner` split at every `separator` outside brackets and parentheses.
fn split_top_level(inner: &str, separator: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0;
    for (at, ch) in inner.char_indices() {
        match ch {
            '[' | '(' => depth += 1,
            ']' | ')' => depth = depth.saturating_sub(1),
            ch if ch == separator && depth == 0 => {
                parts.push(inner[start..at].trim());
                start = at + ch.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(inner[start..].trim());
    parts
}

fn is_identifier(text: &str) -> bool {
    let mut chars = text.chars();
    chars
        .next()
        .is_some_and(|first| first.is_alphabetic() || first == '_')
        && chars.all(|ch| ch.is_alphanumeric() || ch == '_')
}

fn text<'a>(node: Node<'_>, source: &'a [u8]) -> &'a str {
    node.utf8_text(source).unwrap_or("")
}

/// The value of a string literal with no interpolation or escape.
fn string_literal(node: Node<'_>, source: &[u8]) -> Option<String> {
    if node.kind() != "string" {
        return None;
    }
    let raw = text(node, source);
    let body = raw
        .trim_start_matches(|c: char| c.is_ascii_alphabetic())
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| {
            raw.trim_start_matches(|c: char| c.is_ascii_alphabetic())
                .strip_prefix('\'')
                .and_then(|s| s.strip_suffix('\''))
        })?;
    (!body.contains('\\') && !body.contains('{')).then(|| body.to_string())
}

/// The string literal passed to `call` as keyword `keyword`.
fn keyword_string(call: Node<'_>, keyword: &str, source: &[u8]) -> Option<String> {
    let arguments = call.child_by_field_name("arguments")?;
    let mut cursor = arguments.walk();
    let found = arguments
        .named_children(&mut cursor)
        .filter(|arg| arg.kind() == "keyword_argument")
        .find(|arg| {
            arg.child_by_field_name("name")
                .is_some_and(|name| text(name, source) == keyword)
        })?;
    string_literal(found.child_by_field_name("value")?, source)
}

/// How many times each name is bound anywhere in one function: its
/// parameters and every binding form in its body, nested scopes included.
struct Census {
    counts: BTreeMap<String, usize>,
    /// A form this reader does not count exactly (`match`, a star import)
    /// makes every count untrustworthy.
    blocked: bool,
}

impl Census {
    fn of(function: Node<'_>, source: &[u8]) -> Self {
        let mut census = Self {
            counts: BTreeMap::new(),
            blocked: false,
        };
        if let Some(params) = function.child_by_field_name("parameters") {
            census.count_parameters(params, source);
        }
        if let Some(body) = function.child_by_field_name("body") {
            census.walk(body, source);
        }
        census
    }

    fn count(&self, name: &str) -> usize {
        self.counts.get(name).copied().unwrap_or(0)
    }

    /// Every name the function binds.
    fn names(&self) -> HashSet<String> {
        self.counts.keys().cloned().collect()
    }

    fn bind(&mut self, name: &str) {
        *self.counts.entry(name.to_string()).or_default() += 1;
    }

    /// Count `name` as bound more than once, so nothing reads it as single.
    fn withdraw(&mut self, name: &str) {
        *self.counts.entry(name.to_string()).or_default() += 2;
    }

    fn count_parameters(&mut self, params: Node<'_>, source: &[u8]) {
        let mut cursor = params.walk();
        for param in params.named_children(&mut cursor) {
            let name = match param.kind() {
                "identifier" => Some(param),
                "typed_parameter" | "list_splat_pattern" | "dictionary_splat_pattern" => {
                    param.named_child(0).and_then(|n| {
                        if n.kind() == "identifier" {
                            Some(n)
                        } else {
                            n.named_child(0).filter(|n| n.kind() == "identifier")
                        }
                    })
                }
                "default_parameter" | "typed_default_parameter" => {
                    param.child_by_field_name("name")
                }
                _ => {
                    self.targets(param, source);
                    None
                }
            };
            if let Some(name) = name {
                self.bind(text(name, source));
            }
        }
    }

    /// Every identifier a binding target holds, counted once each.
    fn targets(&mut self, target: Node<'_>, source: &[u8]) {
        match target.kind() {
            "identifier" => self.bind(text(target, source)),
            // `obj.x = ...` and `items[0] = ...` bind no name.
            "attribute" | "subscript" => {}
            _ => {
                let mut cursor = target.walk();
                for child in target.named_children(&mut cursor) {
                    self.targets(child, source);
                }
            }
        }
    }

    fn walk(&mut self, node: Node<'_>, source: &[u8]) {
        match node.kind() {
            "match_statement" | "wildcard_import" => {
                self.blocked = true;
            }
            // A name a `global` or `nonlocal` statement declares, anywhere in
            // the function or a scope nested in it, is a variable some other
            // scope can rebind, so no read of it holds one value. Only that
            // name is withdrawn: every other count stays exact.
            "global_statement" | "nonlocal_statement" => {
                let mut cursor = node.walk();
                for name in node.named_children(&mut cursor) {
                    if name.kind() == "identifier" {
                        self.withdraw(text(name, source));
                    }
                }
            }
            "assignment" | "augmented_assignment" => {
                // An annotation with no value declares and binds nothing.
                if node.kind() == "augmented_assignment"
                    || node.child_by_field_name("right").is_some()
                {
                    if let Some(left) = node.child_by_field_name("left") {
                        self.targets(left, source);
                    }
                }
            }
            "for_statement" | "for_in_clause" => {
                if let Some(left) = node.child_by_field_name("left") {
                    self.targets(left, source);
                }
            }
            "named_expression" => {
                if let Some(name) = node.child_by_field_name("name") {
                    self.targets(name, source);
                }
            }
            "as_pattern" => {
                if let Some(alias) = node.child_by_field_name("alias") {
                    self.targets(alias, source);
                }
            }
            "delete_statement" => {
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    self.targets(child, source);
                }
            }
            "import_statement" | "import_from_statement" => {
                let mut cursor = node.walk();
                for name in node.children_by_field_name("name", &mut cursor) {
                    let local = match name.kind() {
                        "aliased_import" => name.child_by_field_name("alias"),
                        _ => Some(name),
                    };
                    if let Some(local) = local {
                        let spelled = text(local, source);
                        self.bind(spelled.split('.').next().unwrap_or(spelled));
                    }
                }
            }
            "function_definition" | "class_definition" => {
                if let Some(name) = node.child_by_field_name("name") {
                    self.bind(text(name, source));
                }
                if let Some(params) = node.child_by_field_name("parameters") {
                    self.count_parameters(params, source);
                }
            }
            "lambda" => {
                if let Some(params) = node.child_by_field_name("parameters") {
                    self.count_parameters(params, source);
                }
            }
            _ => {}
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            // A nested definition's parameters were counted above; its body
            // is walked like any other statement so its bindings count too.
            if matches!(node.kind(), "function_definition" | "lambda")
                && Some(child) == node.child_by_field_name("parameters")
            {
                continue;
            }
            self.walk(child, source);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(source: &str, path: &str) -> FileFacts {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_python::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(source, None).unwrap();
        read_file(&tree, source.as_bytes(), path)
    }

    fn scope<'a>(facts: &'a FileFacts, name: &str) -> &'a ScopeFacts {
        &facts
            .by_node
            .iter()
            .find(|(_, leaf, _)| leaf == name)
            .unwrap_or_else(|| panic!("no facts for {name}"))
            .2
    }

    fn fixture_lookup(name: &str) -> ValueOrigin {
        ValueOrigin::Provided {
            lookup: ProviderLookup::DirectoryChain {
                kind: PYTEST_FIXTURE_KIND.into(),
                file_name: CONFTEST.into(),
                blockers: FIXTURE_BLOCKERS.iter().map(|s| s.to_string()).collect(),
                otherwise: builtin_fixture(name).map(Box::new),
            },
            name: name.into(),
        }
    }

    /// A function nested in a test reads the test's names: the view a test
    /// declares under `@app.route("/")` reads `app` in the decorator and in
    /// its body, and a function two levels down reads through the one in
    /// between.
    #[test]
    fn a_nested_function_reads_what_its_enclosing_function_binds() {
        let facts = facts(
            "def test_route(app, client):\n    @app.route(\"/\")\n    def index():\n        return client.get(\"/\")\n\n    def deeper():\n        def inner():\n            return client.open(\"/\")\n        return inner\n\n    return index\n",
            "tests/test_route.py",
        );
        let enclosing = |name: &str| ValueOrigin::Enclosing { name: name.into() };
        let index = scope(&facts, "index");
        assert_eq!(index.bindings.get("app"), Some(&enclosing("app")));
        assert_eq!(index.bindings.get("client"), Some(&enclosing("client")));
        assert_eq!(
            scope(&facts, "inner").bindings.get("client"),
            Some(&enclosing("client"))
        );
    }

    /// The binding of `name` a nested function's facts record, if any.
    fn nested_binding<'a>(facts: &'a FileFacts, leaf: &str, name: &str) -> Option<&'a ValueOrigin> {
        facts
            .by_node
            .iter()
            .find(|(_, found, _)| found == leaf)
            .and_then(|(_, _, scope)| scope.bindings.get(name))
    }

    /// A nested function that binds a name reads its own, and the test's
    /// count of it then is two, so nothing in the test reads it as one
    /// value. A name the enclosing function binds more than once holds no
    /// single value, and is not the module's either.
    #[test]
    fn a_name_bound_twice_around_a_nested_function_says_nothing() {
        let facts = facts(
            "import flask\n\ndef test_shadow(app):\n    @app.route(\"/\")\n    def index():\n        return app.name\n\n    def other(app):\n        return app.name\n\n    return index\n\ndef test_twice():\n    flask = make()\n    flask = other()\n\n    def inner():\n        return flask.session.pop\n\n    return inner\n",
            "tests/test_basic.py",
        );
        assert_eq!(nested_binding(&facts, "index", "app"), None);
        assert_eq!(nested_binding(&facts, "other", "app"), None);
        assert_eq!(nested_binding(&facts, "inner", "flask"), None);
    }

    /// Flask's `globals.py` declares `session` once, with a type; its
    /// `__init__.py` re-exports it; a test reads it off the package.
    #[test]
    fn a_module_says_what_it_binds_once_and_a_function_reads_it_as_the_modules() {
        let globals = facts(
            "import typing as t\nfrom werkzeug.local import LocalProxy\n\nsession: SessionMixinProxy = LocalProxy(_cv_app, \"session\")\n",
            "src/flask/globals.py",
        );
        assert_eq!(
            globals.module.bindings.get("session"),
            Some(&ValueOrigin::Declared {
                type_name: "SessionMixinProxy".into(),
            })
        );
        assert_eq!(
            globals.module.bindings.get("LocalProxy"),
            Some(&ValueOrigin::ModuleMember {
                module: "werkzeug.local".into(),
                name: "LocalProxy".into(),
            })
        );

        let init = facts(
            "from .globals import session as session\n",
            "src/flask/__init__.py",
        );
        assert_eq!(
            init.module.bindings.get("session"),
            // As spelled: a reader resolves a relative module against the
            // importing file.
            Some(&ValueOrigin::ModuleMember {
                module: ".globals".into(),
                name: "session".into(),
            })
        );

        let test = facts(
            "import flask\n\ndef test_missing_session(app):\n    expect_exception(flask.session.pop, \"foo\")\n",
            "tests/test_basic.py",
        );
        assert_eq!(
            test.module.bindings.get("flask"),
            Some(&ValueOrigin::Module {
                module: "flask".into(),
            })
        );
        assert_eq!(
            scope(&test, "test_missing_session").bindings.get("flask"),
            Some(&ValueOrigin::Global {
                name: "flask".into(),
            })
        );
    }

    /// A module-level name says nothing when the module may bind it twice,
    /// a `global` statement may rebind it, or a wildcard import may; and a
    /// function that binds the name itself does not read the module's.
    #[test]
    fn a_module_name_that_may_be_rebound_says_nothing() {
        let twice = facts(
            "session: Session = make()\nsession = other()\n\ndef read():\n    return session.pop\n",
            "src/app/state.py",
        );
        assert_eq!(twice.module.bindings.get("session"), None);
        assert_eq!(scope(&twice, "read").bindings.get("session"), None);

        let rebound = facts(
            "session: Session = make()\n\ndef reset():\n    global session\n    session = other()\n\ndef read():\n    return session.pop\n",
            "src/app/state.py",
        );
        assert_eq!(rebound.module.bindings.get("session"), None);
        assert_eq!(scope(&rebound, "read").bindings.get("session"), None);

        let starred = facts(
            "from .other import *\n\nsession: Session = make()\n",
            "src/app/state.py",
        );
        assert!(starred.module.bindings.is_empty());

        let shadowed = facts(
            "import flask\n\ndef read(flask):\n    return flask.session.pop\n\ndef local():\n    flask = make()\n    return flask.session.pop\n",
            "tests/test_basic.py",
        );
        assert_ne!(
            scope(&shadowed, "read").bindings.get("flask"),
            Some(&ValueOrigin::Global {
                name: "flask".into(),
            })
        );
        assert_ne!(
            scope(&shadowed, "local").bindings.get("flask"),
            Some(&ValueOrigin::Global {
                name: "flask".into(),
            })
        );
    }

    #[test]
    fn a_test_parameter_is_the_fixture_and_a_local_is_what_its_method_returns() {
        let facts = facts(
            "import pytest\n\ndef test_teardown_on_pop(app):\n    ctx = app.test_request_context()\n    ctx.push()\n    ctx.pop()\n",
            "tests/test_reqctx.py",
        );
        let test = scope(&facts, "test_teardown_on_pop");
        assert_eq!(test.bindings.get("app"), Some(&fixture_lookup("app")));
        assert_eq!(
            test.bindings.get("ctx"),
            Some(&ValueOrigin::MethodCall {
                receiver: Box::new(ValueOrigin::Local { name: "app".into() }),
                method: "test_request_context".into(),
            })
        );
    }

    #[test]
    fn a_fixture_provides_the_class_its_single_local_is_built_from() {
        let facts = facts(
            "import pytest\nfrom flask import Flask\n\n@pytest.fixture\ndef app():\n    app = Flask('x')\n    app.config.update(TESTING=True)\n    return app\n\n@pytest.fixture\ndef req_ctx(app):\n    with app.test_request_context() as ctx:\n        yield ctx\n",
            "tests/conftest.py",
        );
        let app = scope(&facts, "app");
        assert_eq!(app.provides.len(), 1);
        assert_eq!(app.provides[0].name, "app");
        assert_eq!(
            app.provides[0].value,
            Some(ValueOrigin::Local { name: "app".into() })
        );
        assert_eq!(
            app.bindings.get("app"),
            Some(&ValueOrigin::ImportedCall {
                callee: "Flask".into(),
                qualified: "flask.Flask".into(),
            })
        );
        let req_ctx = scope(&facts, "req_ctx");
        assert_eq!(
            req_ctx.provides[0].value,
            Some(ValueOrigin::Local { name: "ctx".into() })
        );
        assert_eq!(
            req_ctx.bindings.get("ctx"),
            Some(&ValueOrigin::Entered {
                value: Box::new(ValueOrigin::MethodCall {
                    receiver: Box::new(ValueOrigin::Local { name: "app".into() }),
                    method: "test_request_context".into(),
                }),
                method: "__enter__".into(),
            })
        );
    }

    #[test]
    fn a_name_bound_twice_or_parametrized_or_defaulted_carries_no_fact() {
        let facts = facts(
            "import pytest\n\n@pytest.mark.parametrize('app', [1, 2])\ndef test_a(app, client=None):\n    ctx = app.make()\n    ctx = other()\n    ctx.pop()\n\ndef test_b(app):\n    for app in []:\n        pass\n",
            "tests/test_x.py",
        );
        assert!(scope(&facts, "test_a").bindings.is_empty());
        assert!(scope(&facts, "test_b").bindings.is_empty());
    }

    #[test]
    fn a_non_test_module_or_non_test_function_is_not_injected() {
        let facts = facts(
            "def test_a(app):\n    app.run()\n\ndef helper(app):\n    app.run()\n",
            "src/pkg/module.py",
        );
        assert!(facts
            .by_node
            .iter()
            .all(|(_, _, scope)| !scope.bindings.contains_key("app")));
        let facts = super::tests::facts("def helper(app):\n    app.run()\n", "tests/test_mod.py");
        assert!(scope(&facts, "helper").bindings.is_empty());
    }

    #[test]
    fn a_shadowing_nested_scope_withdraws_the_outer_binding() {
        let facts = facts(
            "def test_a(app):\n    ctx = app.make()\n    def inner(ctx):\n        return ctx\n    ctx.pop()\n",
            "tests/test_x.py",
        );
        let test = scope(&facts, "test_a");
        assert!(test.bindings.contains_key("app"));
        assert!(!test.bindings.contains_key("ctx"));
    }

    #[test]
    fn declared_returns_read_self_and_optional() {
        let facts = facts(
            "import typing as t\nimport typing_extensions as te\n\nclass AppContext:\n    def __enter__(self) -> te.Self:\n        return self\n\nclass Flask:\n    def test_request_context(self, *args: t.Any) -> AppContext:\n        pass\n    def maybe(self) -> t.Optional[AppContext]:\n        pass\n    def either(self) -> AppContext | Flask:\n        pass\n",
            "src/flask/app.py",
        );
        assert_eq!(
            scope(&facts, "__enter__").returns,
            Some(DeclaredReturn::Receiver)
        );
        assert_eq!(
            scope(&facts, "test_request_context").returns,
            Some(DeclaredReturn::Type {
                type_name: "AppContext".into()
            })
        );
        assert_eq!(
            scope(&facts, "maybe").returns,
            Some(DeclaredReturn::Type {
                type_name: "AppContext".into()
            })
        );
        assert_eq!(scope(&facts, "either").returns, None);
    }

    #[test]
    fn a_named_fixture_and_a_class_scoped_fixture_record_their_names_and_owner() {
        let facts = facts(
            "from pytest import fixture\n\n@fixture(name='app')\ndef make_app():\n    return Flask('x')\n\nclass TestThing:\n    @fixture\n    def app(self):\n        return Other()\n\n    def test_it(self, app):\n        app.pop()\n",
            "tests/test_named.py",
        );
        let make_app = scope(&facts, "make_app");
        assert_eq!(make_app.provides[0].name, "app");
        assert_eq!(make_app.provides[0].owner, None);
        let class_fixture = facts
            .by_node
            .iter()
            .find(|(_, leaf, scope)| leaf == "app" && !scope.provides.is_empty())
            .unwrap();
        assert_eq!(
            class_fixture.2.provides[0].owner.as_deref(),
            Some("TestThing")
        );
        assert_eq!(
            scope(&facts, "test_it").bindings.get("app"),
            Some(&fixture_lookup("app"))
        );
    }

    fn nominal(type_name: &str) -> ValueOrigin {
        ValueOrigin::External {
            type_name: type_name.into(),
            reach: ExternalReach::Nominal {
                roots: vec!["object".into()],
            },
            own_members: Vec::new(),
        }
    }

    #[test]
    fn literals_and_builtin_constructions_hold_exactly_a_builtin_type() {
        let facts = facts(
            "from collections import defaultdict\n\n\
             def work(items):\n    seen = {}\n    stack = [x for x in items]\n    \
             tags = {1, 2}\n    pair = (1, 2)\n    word = 'a'\n    raw = b'a'\n    \
             copy = dict(a=1)\n    order = sorted(items)\n    groups = defaultdict(list)\n",
            "src/pkg/work.py",
        );
        let work = scope(&facts, "work");
        for (name, type_name) in [
            ("seen", "dict"),
            ("stack", "list"),
            ("tags", "set"),
            ("pair", "tuple"),
            ("word", "str"),
            ("raw", "bytes"),
            ("copy", "dict"),
            ("order", "list"),
            ("groups", "collections.defaultdict"),
        ] {
            assert_eq!(work.bindings.get(name), Some(&exact(type_name)), "{name}");
        }
    }

    #[test]
    fn a_rebound_builtin_name_is_not_the_builtin() {
        let facts = facts(
            "from mylib import MyDict as dict\n\n\
             def rebound_at_module():\n    values = dict()\n\n\
             def rebound_locally(list):\n    values = list()\n",
            "src/pkg/work.py",
        );
        assert_eq!(
            scope(&facts, "rebound_at_module").bindings.get("values"),
            Some(&ValueOrigin::ImportedCall {
                callee: "dict".into(),
                qualified: "mylib.MyDict".into(),
            })
        );
        assert_eq!(
            scope(&facts, "rebound_locally").bindings.get("values"),
            None
        );
        let facts = super::tests::facts(
            "from helpers import *\n\ndef starred():\n    values = dict()\n",
            "src/pkg/work.py",
        );
        assert_eq!(
            scope(&facts, "starred").bindings.get("values"),
            Some(&ValueOrigin::Call {
                callee: "dict".into()
            }),
            "a wildcard import may bind `dict` to anything"
        );
    }

    #[test]
    fn builtin_typing_and_pytest_annotations_are_external_types() {
        let facts = facts(
            "import typing as t\nfrom typing import List\nimport pytest\n\n\
             def pull(endpoint: str | None, values: dict[str, t.Any] | None) -> None:\n    pass\n\n\
             def more(a: t.Dict[str, int], b: List[int], mp: pytest.MonkeyPatch, c: 'dict') -> dict:\n    pass\n\n\
             def either(a: dict | list) -> t.Optional[t.List[str]]:\n    pass\n",
            "src/pkg/work.py",
        );
        let pull = scope(&facts, "pull");
        assert_eq!(pull.bindings.get("values"), Some(&nominal("dict")));
        assert_eq!(pull.bindings.get("endpoint"), Some(&nominal("str")));
        let more = scope(&facts, "more");
        assert_eq!(more.bindings.get("a"), Some(&nominal("dict")));
        assert_eq!(more.bindings.get("b"), Some(&nominal("list")));
        assert_eq!(
            more.bindings.get("mp"),
            Some(&nominal("_pytest.monkeypatch.MonkeyPatch"))
        );
        assert_eq!(more.bindings.get("c"), Some(&nominal("dict")));
        assert_eq!(
            more.returns,
            Some(DeclaredReturn::External {
                type_name: "dict".into(),
                reach: ExternalReach::Nominal {
                    roots: vec!["object".into()]
                },
            })
        );
        let either = scope(&facts, "either");
        assert_eq!(either.bindings.get("a"), None, "a union decides nothing");
        assert!(matches!(
            either.returns,
            Some(DeclaredReturn::External { ref type_name, .. }) if type_name == "list"
        ));
    }

    #[test]
    fn a_class_body_binding_shadows_a_builtin_in_its_method_signatures() {
        let facts = facts(
            "class Registry:\n    dict = OrderedStore\n\n    \
             def register(self, values: dict) -> None:\n        values.pop('x')\n",
            "src/pkg/registry.py",
        );
        let register = scope(&facts, "register");
        assert!(
            !matches!(
                register.bindings.get("values"),
                Some(ValueOrigin::External { .. })
            ),
            "{register:?}"
        );
    }

    #[test]
    fn a_builtin_fixture_falls_back_to_its_documented_type() {
        let facts = facts(
            "def test_warns(app, recwarn, monkeypatch, tmp_path):\n    recwarn.pop()\n",
            "tests/test_x.py",
        );
        let test = scope(&facts, "test_warns");
        let otherwise = |name: &str| match test.bindings.get(name) {
            Some(ValueOrigin::Provided {
                lookup: ProviderLookup::DirectoryChain { otherwise, .. },
                ..
            }) => otherwise.as_deref().cloned(),
            other => panic!("{name}: {other:?}"),
        };
        assert_eq!(otherwise("app"), None);
        assert_eq!(
            otherwise("recwarn"),
            Some(exact("_pytest.recwarn.WarningsRecorder"))
        );
        assert_eq!(
            otherwise("monkeypatch"),
            Some(exact("_pytest.monkeypatch.MonkeyPatch"))
        );
        assert_eq!(otherwise("tmp_path"), Some(exact("pathlib.Path")));
    }

    #[test]
    fn a_test_class_with_a_base_takes_no_builtin_fixture_fallback() {
        let facts = facts(
            "class TestChild(TestBase):\n    def test_warns(self, recwarn):\n        recwarn.pop()\n\n\
             class TestPlain:\n    def test_warns_too(self, recwarn):\n        recwarn.pop()\n",
            "tests/test_x.py",
        );
        let otherwise = |test: &str| match scope(&facts, test).bindings.get("recwarn") {
            Some(ValueOrigin::Provided {
                lookup: ProviderLookup::DirectoryChain { otherwise, .. },
                ..
            }) => otherwise.is_some(),
            other => panic!("{test}: {other:?}"),
        };
        assert!(!otherwise("test_warns"), "TestBase may define recwarn");
        assert!(otherwise("test_warns_too"));
    }

    #[test]
    fn a_collected_module_shadows_the_fixtures_it_cannot_name() {
        let facts = facts(
            "import pytest\nfrom .helpers import app\nfrom .more import *\n\n\
             @pytest.fixture\ndef client():\n    return {}\n",
            "tests/conftest.py",
        );
        let module = &facts.module.provides;
        // An imported name is the fixture the import reaches, if it is one.
        assert!(module.iter().any(|provider| provider.name == "app"
            && provider.value
                == Some(ValueOrigin::ImportedProvider {
                    kind: PYTEST_FIXTURE_KIND.into(),
                    name: "app".into(),
                })
            && !provider.every_name));
        assert!(module.iter().any(|provider| provider.every_name));
        // A wildcard import can rebind the fixture's own name after it.
        let client = scope(&facts, "client");
        assert_eq!(client.provides[0].value, None);
        // A plugin list the reader can name registers its modules and opens
        // nothing; one it cannot opens every name.
        let facts = super::tests::facts("pytest_plugins = ['x', \"pkg.y\"]\n", "tests/conftest.py");
        assert!(!facts
            .module
            .provides
            .iter()
            .any(|provider| provider.every_name));
        let modules: Vec<&str> = facts
            .module
            .provider_modules
            .iter()
            .map(|module| module.module.as_str())
            .collect();
        assert_eq!(modules, ["x", "pkg.y"]);
        for unreadable in [
            "pytest_plugins = make_plugins()\n",
            "pytest_plugins = ['x']\npytest_plugins += ['y']\n",
            "if True:\n    pytest_plugins = ['x']\n",
            "pytest_plugins = ['x']\npytest_plugins = ['y']\n",
        ] {
            let facts = super::tests::facts(unreadable, "tests/conftest.py");
            assert!(
                facts
                    .module
                    .provides
                    .iter()
                    .any(|provider| provider.every_name),
                "{unreadable}"
            );
            assert!(facts.module.provider_modules.is_empty(), "{unreadable}");
        }
        let facts = super::tests::facts("from .helpers import app\n", "src/pkg/not_collected.py");
        assert!(facts.module.provides.is_empty());
    }

    #[test]
    fn a_plugin_module_injects_its_fixtures_parameters_and_shadows_its_imports() {
        let facts = facts(
            "import pytest\nfrom .other import client\n\n@pytest.fixture\ndef runner(app):\n    return app.test_cli_runner()\n",
            "src/pkg/testing/fixtures.py",
        );
        let runner = scope(&facts, "runner");
        assert_eq!(runner.bindings.get("app"), Some(&fixture_lookup("app")));
        assert_eq!(
            runner.provides[0].value,
            Some(ValueOrigin::MethodCall {
                receiver: Box::new(ValueOrigin::Local { name: "app".into() }),
                method: "test_cli_runner".into(),
            })
        );
        assert!(facts
            .module
            .provides
            .iter()
            .any(|provider| provider.name == "client"));
        // A plain function there is still no fixture and gets nothing.
        let facts = super::tests::facts(
            "def helper(app):\n    return app.run()\n",
            "src/pkg/testing/helpers.py",
        );
        assert!(scope(&facts, "helper").bindings.is_empty());
    }

    #[test]
    fn a_test_withholds_the_names_it_parametrizes() {
        let facts = facts(
            "import pytest\n\n@pytest.mark.parametrize('app', [1])\ndef test_a(runner):\n    runner.invoke()\n\n\
             def test_b(runner):\n    runner.invoke()\n\n\
             @pytest.mark.parametrize(names(), [1])\ndef test_c(runner):\n    runner.invoke()\n",
            "tests/test_x.py",
        );
        let a = scope(&facts, "test_a");
        assert_eq!(
            a.withheld_names
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["app"]
        );
        assert_eq!(a.bindings.get("runner"), Some(&fixture_lookup("runner")));
        assert!(scope(&facts, "test_b").withheld_names.is_empty());
        // Names the reader cannot list leave nothing to look up at all.
        assert!(scope(&facts, "test_c").bindings.is_empty());
    }

    #[test]
    fn a_plain_function_returns_what_its_returns_agree_on() {
        let facts = facts(
            "from flask import Flask\n\n\
             def create_app(test_config=None):\n    app = Flask(__name__)\n    if test_config:\n        app.config.update(test_config)\n\n    \
             @app.route('/hello')\n    def hello():\n        return 'hi'\n\n    return app\n\n\
             def declared() -> Flask:\n    return make()\n\n\
             def generated():\n    yield Flask(__name__)\n\n\
             async def awaited():\n    return Flask(__name__)\n\n\
             @cache\ndef decorated():\n    return Flask(__name__)\n\n\
             def two(flag):\n    if flag:\n        return Flask(__name__)\n    return other()\n\n\
             def bare(flag):\n    if flag:\n        return\n    return Flask(__name__)\n",
            "examples/tutorial/flaskr/__init__.py",
        );
        let create_app = scope(&facts, "create_app");
        assert_eq!(
            create_app.returned,
            Some(ValueOrigin::Local { name: "app".into() })
        );
        assert_eq!(
            create_app.bindings.get("app"),
            Some(&ValueOrigin::ImportedCall {
                callee: "Flask".into(),
                qualified: "flask.Flask".into(),
            })
        );
        // A declared return is the declaration, never the body's guess.
        assert_eq!(scope(&facts, "declared").returned, None);
        for name in ["generated", "awaited", "decorated", "two", "bare"] {
            let returned = facts
                .by_node
                .iter()
                .find(|(_, leaf, _)| leaf == name)
                .and_then(|(_, _, scope)| scope.returned.clone());
            assert_eq!(returned, None, "{name}");
        }
    }

    /// Flask's test client at the benchmark commit: `FlaskClient` binds
    /// `open`, `session_transaction` and a handful of attributes, and nothing
    /// named `get`, which it inherits from werkzeug's `Client`.
    #[test]
    fn a_class_says_every_name_it_binds_or_that_it_cannot() {
        let facts = facts(
            "from werkzeug.test import Client\nfrom .sessions import SessionMixin\n\n\
             class FlaskClient(Client):\n    application: Flask\n    preserve = False\n\n    \
             def __init__(self, *args, **kwargs):\n        super().__init__(*args, **kwargs)\n        \
             self.preserve_context = False\n        self._new_contexts: list = []\n        \
             self.environ_base = {\"REMOTE_ADDR\": \"127.0.0.1\"}\n\n    \
             def session_transaction(self, *args, **kwargs):\n        pass\n\n    \
             def open(self, *args, **kwargs):\n        a, self.last = args, kwargs\n        \
             return super().open(*args, **kwargs)\n\n\
             class NullSession(SessionMixin):\n    def _fail(self, *args, **kwargs):\n        \
             raise RuntimeError()\n\n    __setitem__ = clear = pop = _fail\n    del _fail\n\n\
             class Proxy:\n    def __getattr__(self, name):\n        return name\n\n\
             @dataclass\nclass Decorated:\n    name: str\n\n\
             class Tagged(Base, metaclass=Meta):\n    pass\n\n\
             class Patched:\n    def patch(self, name, value):\n        setattr(self, name, value)\n",
            "src/flask/testing.py",
        );
        // A class with nothing to say carries no facts at all.
        let bound = |class: &str| {
            facts
                .by_node
                .iter()
                .find(|(_, leaf, _)| leaf == class)
                .and_then(|(_, _, scope)| scope.bound_names.clone())
        };
        let names = |names: &[&str]| -> Option<BTreeSet<String>> {
            Some(names.iter().map(|name| name.to_string()).collect())
        };
        assert_eq!(
            bound("FlaskClient"),
            names(&[
                "__init__",
                "_new_contexts",
                "application",
                "environ_base",
                "last",
                "open",
                "preserve",
                "preserve_context",
                "session_transaction",
            ])
        );
        assert!(!bound("FlaskClient").unwrap().contains("get"));
        // A name bound by assignment in the class body is bound, entity or not.
        assert_eq!(
            bound("NullSession"),
            names(&["__setitem__", "_fail", "clear", "pop"])
        );
        // Attribute hooks, a decorator, a metaclass or a write by a computed
        // name: the text does not say every name the class holds.
        for class in ["Proxy", "Decorated", "Tagged", "Patched"] {
            assert_eq!(bound(class), None, "{class}");
        }
    }

    /// `src/flask/testing.py` derives `FlaskCliRunner` from click's
    /// `CliRunner` and `tests/test_cli.py` constructs click's own: both name
    /// the class by the one import that binds it.
    #[test]
    fn an_imported_callee_and_an_imported_base_say_what_they_import() {
        let facts = facts(
            "import typing as t\nimport click\nfrom click.testing import CliRunner\n\
             from werkzeug.test import Client as Base\nfrom .app import Flask\n\
             from registry import register, Plugin\n\
             from collections.abc import MutableMapping\n\
             try:\n    from fast import Loader\nexcept ImportError:\n    from slow import Loader\n\n\
             class FlaskCliRunner(CliRunner):\n    pass\n\n\
             class FlaskClient(Base):\n    pass\n\n\
             class AppGroup(click.Group):\n    pass\n\n\
             class Config(dict):\n    pass\n\n\
             class Plain(object, t.Generic[T]):\n    pass\n\n\
             class SessionMixin(MutableMapping[str, t.Any]):\n    pass\n\n\
             class Shape(t.Protocol):\n    pass\n\n\
             class Local(Flask, Loader):\n    pass\n\n\
             class Handed(Plugin):\n    pass\n\n\
             class Checked(Plugin):\n    pass\n\n\
             register(Handed)\nisinstance(thing, Checked)\n\n\
             def build():\n    runner = CliRunner()\n    group = click.Group()\n    \
             app = Flask()\n    loader = Loader()\n    return runner\n\n\
             def outer():\n    class Inner(CliRunner):\n        pass\n",
            "src/flask/testing.py",
        );
        let class = |class: &str| {
            facts
                .by_node
                .iter()
                .find(|(_, leaf, _)| leaf == class)
                .map(|(_, _, scope)| scope.clone())
                .unwrap_or_default()
        };
        let origins = |pairs: &[(&str, Option<&str>)]| -> BTreeMap<String, Option<String>> {
            pairs
                .iter()
                .map(|(base, origin)| (base.to_string(), origin.map(str::to_string)))
                .collect()
        };
        assert_eq!(
            class("FlaskCliRunner").base_origins,
            origins(&[("CliRunner", Some("click.testing.CliRunner"))])
        );
        assert_eq!(
            class("FlaskClient").base_origins,
            origins(&[("Base", Some("werkzeug.test.Client"))])
        );
        assert_eq!(
            class("AppGroup").base_origins,
            origins(&[("click.Group", Some("click.Group"))])
        );
        assert_eq!(
            class("Config").base_origins,
            origins(&[("dict", Some("builtins.dict"))])
        );
        // Roots that hold no member a call spells are named as such.
        assert_eq!(
            class("Plain").base_origins,
            origins(&[("object", None), ("t.Generic", None)])
        );
        // A subscripted base is keyed by the class it subscripts, the name
        // the class's `Extends` relation records, so a lookup by the recorded
        // base finds it.
        assert_eq!(
            class("SessionMixin").base_origins,
            origins(&[("MutableMapping", Some("collections.abc.MutableMapping"))])
        );
        // A protocol, a relative import, a name two imports bind and a class
        // in a function body: the text does not decide the base.
        assert_eq!(class("Shape").base_origins, origins(&[]));
        assert_eq!(class("Local").base_origins, origins(&[]));
        assert_eq!(class("Inner").base_origins, origins(&[]));
        // A class in a function body still says what it binds.
        assert_eq!(class("Inner").bound_names, Some(BTreeSet::new()));
        // Passed to a call, the class is in other code's hands. A class test
        // keeps nothing.
        assert!(class("Handed").handed_over);
        assert!(!class("Checked").handed_over);
        assert!(!class("FlaskCliRunner").handed_over);

        let build = scope(&facts, "build");
        assert_eq!(
            build.bindings.get("runner"),
            Some(&ValueOrigin::ImportedCall {
                callee: "CliRunner".into(),
                qualified: "click.testing.CliRunner".into(),
            })
        );
        assert_eq!(
            build.bindings.get("group"),
            Some(&ValueOrigin::ImportedCall {
                callee: "click.Group".into(),
                qualified: "click.Group".into(),
            })
        );
        // A relative import and a name bound twice stay plain calls.
        assert_eq!(
            build.bindings.get("app"),
            Some(&ValueOrigin::Call {
                callee: "Flask".into()
            })
        );
        assert_eq!(
            build.bindings.get("loader"),
            Some(&ValueOrigin::Call {
                callee: "Loader".into()
            })
        );
    }

    #[test]
    fn a_class_declares_what_its_instances_attributes_hold() {
        let facts = facts(
            "import typing as t\nfrom ..json.provider import JSONProvider\nfrom .config import Config\n\n\
             class App:\n    json_provider_class: type[JSONProvider] = None\n    name: str\n    \
             config_class: Config\n\n    \
             def __init__(self, import_name):\n        self.import_name = import_name\n        \
             self.json: JSONProvider = self.json_provider_class(self)\n        \
             self.extensions: dict[str, t.Any] = {}\n        self.both: Config = None\n\n    \
             def reset(self):\n        self.both: JSONProvider = None\n\n    \
             @property\n    def debug(self) -> bool:\n        return False\n\n    \
             @property\n    def provider(self) -> JSONProvider:\n        return self.json\n\n    \
             @provider.setter\n    def provider(self, value: Config) -> None:\n        pass\n\n    \
             @staticmethod\n    def make(self):\n        self.static: Config = None\n\n    \
             def rebinds(self):\n        self = other()\n        self.rebound: Config = None\n",
            "src/flask/sansio/app.py",
        );
        let app = scope(&facts, "App");
        let declared = |type_name: &str| {
            Some(Some(ValueOrigin::Declared {
                type_name: type_name.into(),
            }))
        };
        assert_eq!(app.members.get("json").cloned(), declared("JSONProvider"));
        assert_eq!(app.members.get("config_class").cloned(), declared("Config"));
        assert_eq!(
            app.members.get("provider").cloned(),
            declared("JSONProvider")
        );
        // Declared, but not as one type a call can be bound through.
        assert_eq!(app.members.get("json_provider_class").cloned(), Some(None));
        assert_eq!(app.members.get("debug").cloned(), Some(None));
        // Two declarations that disagree.
        assert_eq!(app.members.get("both").cloned(), Some(None));
        assert!(matches!(
            app.members.get("extensions"),
            Some(Some(ValueOrigin::External { .. }))
        ));
        assert!(matches!(
            app.members.get("name"),
            Some(Some(ValueOrigin::External { .. }))
        ));
        // Not an instance attribute, or `self` is not the instance there.
        for absent in ["import_name", "static", "rebound"] {
            assert!(!app.members.contains_key(absent), "{absent}");
        }
        assert!(app.bindings.is_empty() && app.provides.is_empty());
    }

    #[test]
    fn a_global_or_nonlocal_name_withdraws_only_itself() {
        let facts = facts(
            "def test_init_once(app, client):\n    n = 0\n\n    class CountInit:\n        def __init__(self):\n            nonlocal n\n            n += 1\n\n    \
             app.add_url_rule('/')\n    client.get('/')\n\n\
             def test_global(app):\n    global app_state\n    app_state = app.make()\n    ctx = app.make()\n",
            "tests/test_views.py",
        );
        let once = scope(&facts, "test_init_once");
        assert_eq!(once.bindings.get("app"), Some(&fixture_lookup("app")));
        assert_eq!(once.bindings.get("client"), Some(&fixture_lookup("client")));
        assert!(!once.bindings.contains_key("n"));
        let global = scope(&facts, "test_global");
        assert_eq!(global.bindings.get("app"), Some(&fixture_lookup("app")));
        assert!(global.bindings.contains_key("ctx"));
        assert!(!global.bindings.contains_key("app_state"));
    }

    #[test]
    fn a_fastapi_dependency_parameter_is_what_its_provider_supplies() {
        let facts = facts(
            "from typing import Annotated\nfrom fastapi import Depends, FastAPI\n\
             from .db import SessionLocal\nfrom .services import Service, get_service\n\n\
             app = FastAPI()\n\n\
             def get_db():\n    db = SessionLocal()\n    try:\n        yield db\n    finally:\n        db.close()\n\n\
             def get_settings():\n    return Settings()\n\n\
             @app.get('/items')\n\
             def read_items(db = Depends(get_db), svc: Annotated[Service, Depends(get_service)] = None, \
             other = Depends(dependency=get_settings), plain = 3):\n    return db.query()\n",
            "src/app/main.py",
        );
        let read = scope(&facts, "read_items");
        assert_eq!(
            read.bindings.get("db"),
            Some(&ValueOrigin::Supplied {
                callee: "get_db".into()
            })
        );
        assert_eq!(
            read.bindings.get("svc"),
            Some(&ValueOrigin::Declared {
                type_name: "Service".into()
            })
        );
        assert_eq!(
            read.bindings.get("other"),
            Some(&ValueOrigin::Supplied {
                callee: "get_settings".into()
            })
        );
        assert_eq!(read.bindings.get("plain"), None);
        assert_eq!(
            scope(&facts, "get_db").supplies,
            Some(ValueOrigin::Local { name: "db".into() })
        );
        assert_eq!(
            scope(&facts, "get_settings").supplies,
            Some(ValueOrigin::Call {
                callee: "Settings".into()
            })
        );
    }

    #[test]
    fn a_depends_default_from_elsewhere_is_only_a_default() {
        let facts = facts(
            "from mylib import Depends\n\ndef read(db = Depends(get_db)):\n    return db\n",
            "src/app/main.py",
        );
        assert!(scope(&facts, "read").bindings.is_empty());
    }

    #[test]
    fn a_unittest_setup_attribute_is_bound_in_every_test_method() {
        let facts = facts(
            "import unittest\nfrom app import Client, Other\n\n\
             class TestViews(unittest.TestCase):\n    def setUp(self):\n        \
             self.client = Client()\n        self.other = Other()\n        self.twice = Other()\n\n    \
             def test_get(self):\n        self.client.get('/')\n\n    \
             def test_rewrites(self):\n        self.twice = Client()\n\n    \
             def helper(self):\n        self.client.get('/')\n",
            "tests/test_views.py",
        );
        let test = scope(&facts, "test_get");
        let owner = |name: &str| ValueOrigin::Provided {
            lookup: ProviderLookup::Owner {
                kind: UNITTEST_SETUP_KIND.into(),
            },
            name: name.into(),
        };
        assert_eq!(test.bindings.get("self.client"), Some(&owner("client")));
        assert_eq!(test.bindings.get("self.other"), Some(&owner("other")));
        assert_eq!(
            test.bindings.get("self.twice"),
            None,
            "an attribute written twice in the class holds no one value"
        );
        assert!(scope(&facts, "helper").bindings.is_empty());
        let setup = scope(&facts, "setUp");
        let client = setup
            .provides
            .iter()
            .find(|provider| provider.name == "client")
            .unwrap();
        assert_eq!(client.kind, UNITTEST_SETUP_KIND);
        assert_eq!(client.owner.as_deref(), Some("TestViews"));
        assert_eq!(
            client.value,
            Some(ValueOrigin::ImportedCall {
                callee: "Client".into(),
                qualified: "app.Client".into(),
            })
        );
    }

    #[test]
    fn a_call_through_super_after_the_assignment_withdraws_it() {
        let facts = facts(
            "import unittest\n\n\
             class TestViews(unittest.TestCase):\n    def setUp(self):\n        \
             self.client = Client()\n        super().setUp()\n\n    \
             def test_get(self):\n        self.client.get('/')\n",
            "tests/test_views.py",
        );
        assert!(scope(&facts, "test_get").bindings.is_empty());
        let facts = super::tests::facts(
            "import unittest\n\n\
             class TestViews(unittest.TestCase):\n    client = None\n\n    def setUp(self):\n        \
             self.client = Client()\n\n    def test_get(self):\n        self.client.get('/')\n",
            "tests/test_views.py",
        );
        assert!(
            scope(&facts, "test_get").bindings.is_empty(),
            "a class attribute of the name can be what a read finds"
        );
    }

    #[test]
    fn a_module_pytestmark_withdraws_every_injection() {
        let facts = facts(
            "import pytest\npytestmark = pytest.mark.parametrize('app', [1])\n\ndef test_a(app):\n    app.pop()\n",
            "tests/test_x.py",
        );
        assert!(scope(&facts, "test_a").bindings.is_empty());
    }
}
