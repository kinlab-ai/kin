// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

use super::*;
use kin_model::{
    EntityMetadata, EntityRole, FileEscapeRecord, FingerprintAlgorithm, ModuleScope, ProtocolSite,
    ReflectiveSite, SemanticFingerprint, UnparsedRegion, ValueOccurrence, PROTOCOL_MEMBER_KEY,
};

const BLOB: [u8; 32] = [1; 32];

/// A census graph over files held in memory: each file's record rides on a
/// module entity, the way the parser pipeline places it.
#[derive(Default, Clone)]
struct Fixture {
    files: Vec<CensusFile>,
    entities: Vec<Entity>,
    imports: Vec<RelationEvidence>,
    unreadable: HashSet<String>,
    /// The stored rows into the focal, or `None` for a graph that cannot
    /// read them.
    uses: Option<Vec<StoredUse>>,
}

fn language_of(path: &str) -> LanguageId {
    language_of_path(path).expect("a source path")
}

fn entity(path: &str, name: &str, kind: EntityKind, line: u32) -> Entity {
    Entity {
        id: EntityId::from_content(path, name, &format!("{kind:?}"), line),
        kind,
        name: name.to_string(),
        language: language_of(path),
        fingerprint: SemanticFingerprint {
            algorithm: FingerprintAlgorithm::V1TreeSitter,
            ast_hash: Hash256::from_bytes([0; 32]),
            signature_hash: Hash256::from_bytes([0; 32]),
            behavior_hash: Hash256::from_bytes([5; 32]),
            equivalence_hash: Hash256::from_bytes([0; 32]),
            stability_score: 1.0,
        },
        file_origin: Some(FilePathId::new(path)),
        span: Some(SourceSpan {
            file: FilePathId::new(path),
            start_byte: line as usize * 10,
            end_byte: line as usize * 10 + 5,
            start_line: line,
            start_col: 0,
            end_line: line,
            end_col: 0,
        }),
        signature: if kind == EntityKind::Module {
            format!("module {path}")
        } else {
            String::new()
        },
        visibility: Visibility::Public,
        role: EntityRole::Source,
        doc_summary: None,
        metadata: EntityMetadata {
            extra: [(
                "blob_hash".to_string(),
                serde_json::json!(Hash256::from_bytes(BLOB).to_string()),
            )]
            .into_iter()
            .collect(),
        },
        lineage_parent: None,
        created_in: None,
        superseded_by: None,
    }
}

fn occurrence(
    line: u32,
    context: OccurrenceContext,
    binding: OccurrenceBinding,
) -> ValueOccurrence {
    ValueOccurrence {
        line,
        context,
        binding,
        receiver: None,
        receiver_kind: None,
        aliases: Vec::new(),
        module: None,
        import: None,
        imported: None,
        target_line: None,
        form: None,
    }
}

fn reflective(line: u32, form: ReflectiveForm, reach: ReflectiveReach) -> ReflectiveSite {
    ReflectiveSite {
        line,
        form,
        reach,
        key_prefix: None,
        key_suffix: None,
        called: false,
        excerpt: format!("site {line}"),
        key_parts: Vec::new(),
        receiver: None,
        enumerable_only: false,
    }
}

fn record(scope: ModuleScope) -> FileEscapeRecord {
    FileEscapeRecord::new(scope)
}

impl Fixture {
    /// A file whose module entity carries `record`.
    fn recorded(mut self, path: &str, record: FileEscapeRecord) -> Self {
        let mut module = entity(path, path, EntityKind::Module, 0);
        module
            .metadata
            .extra
            .insert(VALUE_ESCAPE_RECORD_KEY.into(), record.to_metadata());
        self.entities.push(module);
        self.files.push(CensusFile {
            path: FilePathId::new(path),
            language: language_of(path),
            blob: Hash256::from_bytes(BLOB),
            parsed: Some(true),
        });
        self
    }

    /// A file with no entity, parsed or not as `parsed` says.
    fn bare(mut self, path: &str, parsed: Option<bool>) -> Self {
        self.files.push(CensusFile {
            path: FilePathId::new(path),
            language: language_of(path),
            blob: Hash256::from_bytes(BLOB),
            parsed,
        });
        self
    }

    fn declares(
        mut self,
        path: &str,
        name: &str,
        kind: EntityKind,
        line: u32,
        visibility: Visibility,
    ) -> Self {
        let mut declared = entity(path, name, kind, line);
        declared.visibility = visibility;
        self.entities.push(declared);
        self
    }

    fn import_into(mut self, path: &str, start: usize, end: usize) -> Self {
        self.imports.push(RelationEvidence {
            source_span: Some(SourceSpan {
                file: FilePathId::new(path),
                start_byte: start,
                end_byte: end,
                start_line: 0,
                start_col: 0,
                end_line: 0,
                end_col: 0,
            }),
            ..Default::default()
        });
        self
    }

    fn import_as(self, path: &str, start: usize, end: usize, local: &str) -> Self {
        let mut fixture = self.import_into(path, start, end);
        fixture.imports.last_mut().unwrap().token = Some(local.into());
        fixture
    }

    fn named(&self, name: &str) -> &Entity {
        self.entities
            .iter()
            .find(|entity| entity.name == name)
            .expect("an entity of that name")
    }

    fn census(&self, name: &str) -> FocalEscape {
        let focal = self.named(name);
        focal_escape_evidence_in(self, focal, &kin_model::focal_call_names(focal))
    }
}

impl EscapeCensusGraph for Fixture {
    fn census_files(&self, languages: &[LanguageId]) -> Option<Vec<CensusFile>> {
        Some(
            self.files
                .iter()
                .filter(|file| languages.contains(&file.language))
                .cloned()
                .collect(),
        )
    }

    fn entities_in(&self, path: &FilePathId) -> Option<Vec<Entity>> {
        if self.unreadable.contains(&path.0) {
            return None;
        }
        Some(
            self.entities
                .iter()
                .filter(|entity| entity.file_origin.as_ref() == Some(path))
                .cloned()
                .collect(),
        )
    }

    fn import_evidence_into(&self, _target: EntityId) -> Option<Vec<RelationEvidence>> {
        Some(self.imports.clone())
    }

    fn uses_into(&self, _target: EntityId) -> Option<Vec<StoredUse>> {
        self.uses.clone()
    }
}

fn residue(escape: &FocalEscape) -> &EscapeResidue {
    escape.residue().expect("a census with a residue")
}

fn sites(sites: &[ResidueSite]) -> Vec<(String, u32)> {
    sites
        .iter()
        .map(|site| (site.file.clone(), site.line))
        .collect()
}

/// Every reflective site the census found could reach the focal, whether its
/// record names the focal's key (`reflective`) or not (`unkeyed_lookups`):
/// what a site can reach does not depend on how it is served.
fn reached(residue: &EscapeResidue) -> Vec<(String, u32)> {
    let mut all = sites(&residue.reflective);
    all.extend(sites(&residue.unkeyed_lookups));
    all.sort();
    all
}

#[test]
fn a_focal_no_file_uses_as_a_value_is_contained_with_nothing_listed() {
    let mut other = record(ModuleScope::Open);
    other.values.insert(
        "unrelated".into(),
        vec![occurrence(
            3,
            OccurrenceContext::Argument,
            OccurrenceBinding::Free,
        )],
    );
    let fixture = Fixture::default()
        .recorded("app.py", record(ModuleScope::Open))
        .declares(
            "app.py",
            "target",
            EntityKind::Function,
            4,
            Visibility::Public,
        )
        .recorded("use.py", other);
    let escape = fixture.census("target");
    assert!(
        matches!(escape, FocalEscape::Contained { .. }),
        "{escape:?}"
    );
    assert!(residue(&escape).is_empty());
    assert!(!escape.may_escape());
}

/// A class entity whose span covers `first..=last`.
fn class_entity(path: &str, name: &str, first: u32, last: u32) -> Entity {
    let mut class = entity(path, name, EntityKind::Class, first);
    let span = class.span.as_mut().expect("a spanned entity");
    span.end_line = last;
    span.end_byte = last as usize * 10 + 5;
    class
}

fn binding(line: u32, binding: OccurrenceBinding) -> ValueOccurrence {
    occurrence(line, OccurrenceContext::Binding, binding)
}

/// Flask at the benchmark commit: `expect_exception(flask.session.pop, "foo")`
/// passes a member named `pop`, so `AppContext.pop` escapes. `NullSession`
/// binds its own `pop` in its class body
/// (`__setitem__ = __delitem__ = clear = pop = popitem = ... = _fail`), and
/// `NullSession` shares no class with `AppContext`: that line declares
/// `NullSession.pop`, as a `def pop` there would, and is no site of this
/// focal.
#[test]
fn a_member_passed_as_an_argument_escapes_and_another_classes_binding_is_not_listed() {
    let mut tests = record(ModuleScope::Open);
    let mut passed = occurrence(392, OccurrenceContext::Argument, OccurrenceBinding::Member);
    passed.receiver = Some("flask.session".into());
    passed.receiver_kind = Some(ReceiverKind::Other);
    tests.values.insert("pop".into(), vec![passed]);
    let mut sessions = record(ModuleScope::Open);
    sessions
        .classes
        .push(class("SessionMixin", 20, &["MutableMapping"], false));
    sessions.classes.push(class(
        "SecureCookieSession",
        49,
        &["CallbackDict", "SessionMixin"],
        false,
    ));
    sessions
        .classes
        .push(class("NullSession", 82, &["SecureCookieSession"], false));
    sessions
        .values
        .insert("pop".into(), vec![binding(95, OccurrenceBinding::Class)]);
    let mut ctx = record(ModuleScope::Open);
    ctx.classes.push(class("AppContext", 18, &[], false));
    let mut fixture = Fixture::default()
        .recorded("src/flask/ctx.py", ctx)
        .declares(
            "src/flask/ctx.py",
            "AppContext.pop",
            EntityKind::Method,
            20,
            Visibility::Public,
        )
        .recorded("src/flask/sessions.py", sessions)
        .recorded("tests/test_basic.py", tests);
    fixture
        .entities
        .push(class_entity("src/flask/sessions.py", "NullSession", 82, 96));
    let escape = fixture.census("AppContext.pop");
    let FocalEscape::Escapes { residue, .. } = &escape else {
        panic!("{escape:?}");
    };
    assert_eq!(
        sites(&residue.values),
        vec![("tests/test_basic.py".into(), 393)]
    );
    assert_eq!(residue.values[0].detail.as_deref(), Some("flask.session"));
    assert!(residue.bindings.is_empty(), "{:?}", residue.bindings);
    assert!(residue.alias_names.is_empty());
}

/// A name bound in a class body is listed where its class stands in the
/// focal's hierarchy, on either side, and wherever the census cannot place
/// the class. Only a class it can place outside that hierarchy is left out.
#[test]
fn a_class_scope_binding_is_listed_only_in_the_focals_hierarchy() {
    let census = |classes: Vec<kin_model::ClassBases>, holder: Option<(&str, u32, u32)>| {
        let mut ctx = record(ModuleScope::Open);
        ctx.classes.push(class("Base", 2, &[], false));
        ctx.classes.push(class("AppContext", 18, &["Base"], false));
        let mut other = record(ModuleScope::Open);
        other.classes = classes;
        other
            .values
            .insert("pop".into(), vec![binding(45, OccurrenceBinding::Class)]);
        let mut fixture = Fixture::default()
            .recorded("ctx.py", ctx)
            .declares(
                "ctx.py",
                "AppContext.pop",
                EntityKind::Method,
                20,
                Visibility::Public,
            )
            .recorded("other.py", other);
        if let Some((name, first, last)) = holder {
            fixture
                .entities
                .push(class_entity("other.py", name, first, last));
        }
        sites(&residue_of(&fixture.census("AppContext.pop")).bindings)
    };
    let listed = vec![("other.py".to_string(), 46)];
    let none: Vec<(String, u32)> = Vec::new();

    // A class of its own: `pop` there is that class's member.
    assert_eq!(
        census(
            vec![class("Session", 40, &["dict"], false)],
            Some(("Session", 40, 50))
        ),
        none
    );
    // A subclass may replace the member it inherits.
    assert_eq!(
        census(
            vec![class("QuietContext", 40, &["AppContext"], false)],
            Some(("QuietContext", 40, 50))
        ),
        listed
    );
    // A base declares what the focal overrides.
    assert_eq!(
        census(vec![class("Base", 40, &[], false)], Some(("Base", 40, 50))),
        listed
    );
    // The focal's own class, declared under the same name elsewhere.
    assert_eq!(
        census(
            vec![class("AppContext", 40, &[], false)],
            Some(("AppContext", 40, 50))
        ),
        listed
    );
    // A base no declared name settles may be anything.
    assert_eq!(
        census(vec![class("Mixed", 40, &[], true)], Some(("Mixed", 40, 50))),
        listed
    );
    // No class entity covers the line, or the record names no such class:
    // the census cannot place the binding, so it stays.
    assert_eq!(
        census(vec![class("Session", 40, &["dict"], false)], None),
        listed
    );
    assert_eq!(census(Vec::new(), Some(("Session", 40, 50))), listed);
    // The innermost class decides, not the one around it.
    let nested = |inner: &[&str]| {
        let mut ctx = record(ModuleScope::Open);
        ctx.classes.push(class("AppContext", 18, &[], false));
        let mut other = record(ModuleScope::Open);
        other
            .classes
            .push(class("Outer", 30, &["AppContext"], false));
        other.classes.push(class("Inner", 40, inner, false));
        other
            .values
            .insert("pop".into(), vec![binding(45, OccurrenceBinding::Class)]);
        let mut fixture = Fixture::default()
            .recorded("ctx.py", ctx)
            .declares(
                "ctx.py",
                "AppContext.pop",
                EntityKind::Method,
                20,
                Visibility::Public,
            )
            .recorded("other.py", other);
        fixture
            .entities
            .push(class_entity("other.py", "Outer", 30, 60));
        fixture
            .entities
            .push(class_entity("other.py", "Outer.Inner", 40, 50));
        sites(&residue_of(&fixture.census("AppContext.pop")).bindings)
    };
    assert_eq!(nested(&[]), none);
    assert_eq!(nested(&["AppContext"]), listed);
}

/// React at the benchmark tree: `ReactFiberConfigDOM` declares
/// `measureInstance`, and `ReactFiberConfigWithNoViewTransition` and the noop
/// renderer each write `export const measureInstance = shim;`. Those lines
/// declare their own module's `measureInstance`. Only an assignment in the
/// focal's own module binds the focal's name again.
#[test]
fn a_module_scope_binding_is_listed_only_in_the_focals_own_module() {
    let dom = "packages/react-dom-bindings/src/client/ReactFiberConfigDOM.js";
    let shimmed = "packages/react-reconciler/src/ReactFiberConfigWithNoViewTransition.js";
    let mut other = record(ModuleScope::Open);
    other.values.insert(
        "measureInstance".into(),
        vec![binding(29, OccurrenceBinding::Module)],
    );
    let fixture = Fixture::default()
        .recorded(dom, record(ModuleScope::Open))
        .declares(
            dom,
            "measureInstance",
            EntityKind::Function,
            40,
            Visibility::Public,
        )
        .recorded(shimmed, other);
    let escape = fixture.census("measureInstance");
    assert!(
        escape
            .residue()
            .is_none_or(|residue| residue.bindings.is_empty()),
        "{escape:?}"
    );

    // `load = traced(load)` below `def load` rebinds the focal itself.
    let mut own = record(ModuleScope::Open);
    own.values
        .insert("load".into(), vec![binding(12, OccurrenceBinding::Module)]);
    let mut elsewhere = record(ModuleScope::Open);
    elsewhere
        .values
        .insert("load".into(), vec![binding(3, OccurrenceBinding::Module)]);
    let fixture = Fixture::default()
        .recorded("app/config.py", own)
        .declares(
            "app/config.py",
            "load",
            EntityKind::Function,
            4,
            Visibility::Public,
        )
        .recorded("app/other.py", elsewhere);
    assert_eq!(
        sites(&residue_of(&fixture.census("load")).bindings),
        vec![("app/config.py".to_string(), 13)]
    );

    // An attribute store names no scope, so it stays wherever it is.
    let mut patched = record(ModuleScope::Open);
    patched
        .values
        .insert("load".into(), vec![binding(7, OccurrenceBinding::Member)]);
    let fixture = Fixture::default()
        .recorded("app/config.py", record(ModuleScope::Open))
        .declares(
            "app/config.py",
            "load",
            EntityKind::Function,
            4,
            Visibility::Public,
        )
        .recorded("tests/test_config.py", patched);
    assert_eq!(
        sites(&residue_of(&fixture.census("load")).bindings),
        vec![("tests/test_config.py".to_string(), 8)]
    );
}

/// The residue of a census, or an empty one when it lists nothing.
fn residue_of(escape: &FocalEscape) -> EscapeResidue {
    escape.residue().cloned().unwrap_or_default()
}

/// `flask.app._make_timedelta`: `sansio/app.py` passes its own module-level
/// `_make_timedelta`, a different declaration, so the focal is contained.
#[test]
fn a_same_named_declaration_of_another_file_is_not_the_focal() {
    let mut sansio = record(ModuleScope::Open);
    let mut own = occurrence(223, OccurrenceContext::Argument, OccurrenceBinding::Module);
    own.target_line = Some(51);
    sansio.values.insert("_make_timedelta".into(), vec![own]);
    let fixture = Fixture::default()
        .recorded("src/flask/app.py", record(ModuleScope::Open))
        .declares(
            "src/flask/app.py",
            "_make_timedelta",
            EntityKind::Function,
            60,
            Visibility::Private,
        )
        .recorded("src/flask/sansio/app.py", sansio);
    let escape = fixture.census("_make_timedelta");
    assert!(
        matches!(escape, FocalEscape::Contained { .. }),
        "{escape:?}"
    );
    assert!(residue(&escape).is_empty(), "{escape:?}");
}

#[test]
fn a_use_bound_to_the_focals_own_declaration_escapes() {
    let mut app = record(ModuleScope::Open);
    let mut own = occurrence(80, OccurrenceContext::Argument, OccurrenceBinding::Module);
    own.target_line = Some(60);
    let mut other = occurrence(81, OccurrenceContext::Argument, OccurrenceBinding::Module);
    other.target_line = Some(10);
    app.values
        .insert("_make_timedelta".into(), vec![own, other]);
    let fixture = Fixture::default()
        .recorded("src/flask/app.py", app)
        .declares(
            "src/flask/app.py",
            "_make_timedelta",
            EntityKind::Function,
            60,
            Visibility::Private,
        );
    let escape = fixture.census("_make_timedelta");
    assert_eq!(
        sites(&residue(&escape).values),
        vec![("src/flask/app.py".into(), 81)]
    );
}

/// `setEnabled`: a renaming import that lands on the focal adds its local
/// name as a call name, and a value use of that name escapes.
#[test]
fn a_renaming_import_adds_a_call_name_and_its_uses_are_followed() {
    let mut config = record(ModuleScope::Closed);
    let mut import = occurrence(105, OccurrenceContext::Import, OccurrenceBinding::Import);
    import.aliases = vec!["EmitterSetEnabled".into()];
    import.module = Some("../events/Listener".into());
    import.import = Some((3342, 3494));
    config.values.insert("setEnabled".into(), vec![import]);
    let fixture = Fixture::default()
        .recorded("events/Listener.js", record(ModuleScope::Closed))
        .declares(
            "events/Listener.js",
            "setEnabled",
            EntityKind::Function,
            30,
            Visibility::Public,
        )
        .recorded("client/Config.js", config.clone())
        .import_as("client/Config.js", 3400, 3420, "EmitterSetEnabled");
    let escape = fixture.census("setEnabled");
    let FocalEscape::Contained { residue, .. } = &escape else {
        panic!("{escape:?}");
    };
    assert_eq!(residue.alias_names, vec!["EmitterSetEnabled"]);
    assert_eq!(
        sites(&residue.alias_sites),
        vec![("client/Config.js".into(), 106)]
    );
    let focal = fixture.named("setEnabled");
    assert_eq!(
        escape.call_names(focal),
        vec!["setEnabled", "EmitterSetEnabled"]
    );
    assert!(
        escape.may_escape(),
        "a reader that ignores aliases must widen"
    );
    assert!(!escape.may_escape_unlisted(&escape.call_names(focal)));

    let mut used = config;
    let mut passed = occurrence(200, OccurrenceContext::Argument, OccurrenceBinding::Import);
    passed.imported = Some("setEnabled".into());
    passed.import = Some((3342, 3494));
    used.values.insert("EmitterSetEnabled".into(), vec![passed]);
    let fixture = Fixture::default()
        .recorded("events/Listener.js", record(ModuleScope::Closed))
        .declares(
            "events/Listener.js",
            "setEnabled",
            EntityKind::Function,
            30,
            Visibility::Public,
        )
        .recorded("client/Config.js", used)
        .import_as("client/Config.js", 3400, 3420, "EmitterSetEnabled");
    let escape = fixture.census("setEnabled");
    let FocalEscape::Escapes { residue, .. } = &escape else {
        panic!("{escape:?}");
    };
    assert_eq!(
        sites(&residue.values),
        vec![("client/Config.js".into(), 201)]
    );
}

#[test]
fn alias_metadata_keeps_only_the_same_named_import_that_proves_the_focal() {
    let mut publisher = record(ModuleScope::Open);
    let mut selected = occurrence(1, OccurrenceContext::Import, OccurrenceBinding::Import);
    selected.aliases = vec!["normalize".into()];
    selected.module = Some("rate_engine".into());
    selected.import = Some((10, 70));
    let mut other = occurrence(2, OccurrenceContext::Import, OccurrenceBinding::Import);
    other.aliases = vec!["format_rate".into()];
    other.module = Some("other_engine".into());
    other.import = Some((71, 130));
    publisher
        .values
        .insert("normalize_rate".into(), vec![selected, other]);
    let fixture = Fixture::default()
        .recorded("rate_engine.py", record(ModuleScope::Open))
        .declares(
            "rate_engine.py",
            "normalize_rate",
            EntityKind::Function,
            0,
            Visibility::Public,
        )
        .recorded("other_engine.py", record(ModuleScope::Open))
        .declares(
            "other_engine.py",
            "normalize_rate",
            EntityKind::Function,
            0,
            Visibility::Public,
        )
        .recorded("publisher.py", publisher)
        .import_as("publisher.py", 45, 69, "normalize");
    let escape = fixture.census("normalize_rate");
    let listed = residue(&escape);
    assert_eq!(listed.alias_names, vec!["normalize", "format_rate"]);
    assert_eq!(sites(&listed.alias_sites), vec![("publisher.py".into(), 2)]);
    assert_eq!(listed.alias_sites[0].detail.as_deref(), Some("normalize"));

    // The same import text without identity evidence stays in the conservative
    // name closure, but is never asserted as a binding of this entity.
    let mut missing = fixture.clone();
    missing.imports.clear();
    let unproved = missing.census("normalize_rate");
    assert_eq!(residue(&unproved).alias_names, listed.alias_names);
    assert!(residue(&unproved).alias_sites.is_empty());

    let mut unnamed = fixture.clone();
    unnamed.imports[0].token = None;
    assert!(residue(&unnamed.census("normalize_rate"))
        .alias_sites
        .is_empty());

    // An edge in the right statement is not proof of a different specifier.
    let mut wrong_token = fixture;
    wrong_token.imports[0].token = Some("format_rate".into());
    assert!(residue(&wrong_token.census("normalize_rate"))
        .alias_sites
        .is_empty());
}

#[test]
fn alias_metadata_keeps_a_direct_export_and_withholds_an_unproved_binding() {
    let mut app = record(ModuleScope::Closed);
    let mut export = occurrence(6, OccurrenceContext::Export, OccurrenceBinding::Module);
    export.aliases = vec!["legacy".into(), "default".into()];
    export.target_line = Some(2);
    app.values.insert("target".into(), vec![export]);
    let fixture = Fixture::default().recorded("app.js", app).declares(
        "app.js",
        "target",
        EntityKind::Function,
        2,
        Visibility::Public,
    );
    let escape = fixture.census("target");
    assert_eq!(
        residue(&escape).alias_sites[0].detail.as_deref(),
        Some("legacy, default")
    );
    let ambiguous = fixture.clone().declares(
        "app.js",
        "Other.target",
        EntityKind::Method,
        2,
        Visibility::Public,
    );
    let ambiguous = ambiguous.census("target");
    assert_eq!(residue(&ambiguous).alias_names, vec!["legacy", "default"]);
    assert!(residue(&ambiguous).alias_sites.is_empty());
    let mut unproved = fixture;
    let carrier = unproved
        .entities
        .iter_mut()
        .find(|entity| entity.kind == EntityKind::Module)
        .unwrap();
    carrier
        .metadata
        .extra
        .get_mut(VALUE_ESCAPE_RECORD_KEY)
        .unwrap()["values"]["target"][0]
        .as_object_mut()
        .unwrap()
        .remove("t");
    let escape = unproved.census("target");
    assert_eq!(residue(&escape).alias_names, vec!["legacy", "default"]);
    assert!(residue(&escape).alias_sites.is_empty());
}

#[test]
fn a_module_scope_rebinding_is_followed_through_its_own_file() {
    let mut user = record(ModuleScope::Open);
    let mut rebound = occurrence(2, OccurrenceContext::Alias, OccurrenceBinding::Import);
    rebound.aliases = vec!["handler".into()];
    user.values.insert("target".into(), vec![rebound]);
    user.values.insert(
        "handler".into(),
        vec![occurrence(
            5,
            OccurrenceContext::Argument,
            OccurrenceBinding::Module,
        )],
    );
    let fixture = Fixture::default()
        .recorded("app.py", record(ModuleScope::Open))
        .declares(
            "app.py",
            "target",
            EntityKind::Function,
            1,
            Visibility::Public,
        )
        .recorded("user.py", user);
    let escape = fixture.census("target");
    assert!(matches!(escape, FocalEscape::Escapes { .. }), "{escape:?}");
    let residue = residue(&escape);
    assert_eq!(residue.alias_names, vec!["handler"]);
    assert_eq!(sites(&residue.values), vec![("user.py".into(), 6)]);
}

#[test]
fn a_member_read_off_an_instance_cannot_be_a_module_function() {
    let mut user = record(ModuleScope::Open);
    let mut on_self = occurrence(4, OccurrenceContext::Argument, OccurrenceBinding::Member);
    on_self.receiver_kind = Some(ReceiverKind::Instance);
    user.values.insert("target".into(), vec![on_self]);
    let fixture = Fixture::default()
        .recorded("app.py", record(ModuleScope::Open))
        .declares(
            "app.py",
            "target",
            EntityKind::Function,
            1,
            Visibility::Public,
        )
        .declares(
            "app.py",
            "Store.target",
            EntityKind::Method,
            8,
            Visibility::Public,
        )
        .recorded("user.py", user);
    assert!(matches!(
        fixture.census("target"),
        FocalEscape::Contained { .. }
    ));
    assert!(matches!(
        fixture.census("Store.target"),
        FocalEscape::Escapes { .. }
    ));
}

#[test]
fn reflective_sites_reach_by_their_kind_and_privacy() {
    let mut app = record(ModuleScope::Closed);
    app.reflective.push(reflective(
        2,
        ReflectiveForm::ScopeTable,
        ReflectiveReach::OwnModule,
    ));
    let mut other = record(ModuleScope::Closed);
    other.reflective.push(reflective(
        3,
        ReflectiveForm::ComputedMember,
        ReflectiveReach::Any,
    ));
    other.reflective.push(reflective(
        4,
        ReflectiveForm::ComputedMember,
        ReflectiveReach::Members,
    ));
    other.reflective.push(reflective(
        5,
        ReflectiveForm::Evaluate,
        ReflectiveReach::Any,
    ));
    other.reflective.push(reflective(
        6,
        ReflectiveForm::DynamicImport,
        ReflectiveReach::Nothing,
    ));
    let mut called = reflective(7, ReflectiveForm::ComputedMember, ReflectiveReach::Any);
    called.called = true;
    other.reflective.push(called);
    let mut prefixed = reflective(8, ReflectiveForm::ComputedMember, ReflectiveReach::Any);
    prefixed.key_prefix = Some("on".into());
    other.reflective.push(prefixed);
    let fixture = Fixture::default()
        .recorded("app.js", app)
        .declares(
            "app.js",
            "exported",
            EntityKind::Function,
            10,
            Visibility::Public,
        )
        .declares(
            "app.js",
            "hidden",
            EntityKind::Function,
            11,
            Visibility::Private,
        )
        .declares(
            "app.js",
            "Store.onClick",
            EntityKind::Method,
            12,
            Visibility::Public,
        )
        .recorded("other.js", other);
    let lines = |name: &str| reached(residue(&fixture.census(name)));
    // A lookup whose value is called on the spot (line 8) is listed like any
    // other: the call-site reading leaves it to the census.
    assert_eq!(
        lines("exported"),
        vec![
            ("app.js".into(), 3),
            ("other.js".into(), 4),
            ("other.js".into(), 6),
            ("other.js".into(), 8),
        ]
    );
    // A declaration its module does not export: only its own file's sites.
    assert_eq!(lines("hidden"), vec![("app.js".into(), 3)]);
    // A member: never its module's name table, and the `on` prefix admits it.
    assert_eq!(
        lines("Store.onClick"),
        vec![
            ("other.js".into(), 4),
            ("other.js".into(), 5),
            ("other.js".into(), 6),
            ("other.js".into(), 8),
            ("other.js".into(), 9),
        ]
    );
    let escape = fixture.census("exported");
    assert!(matches!(escape, FocalEscape::Contained { .. }));
    assert!(
        escape.may_escape(),
        "a listed residue is not a certification"
    );
    assert!(!escape.may_escape_unlisted(&["exported".into()]));
}

#[test]
fn protocol_members_list_their_dispatch_and_implicit_calls_are_listed() {
    let mut app = record(ModuleScope::Open);
    app.protocol.push(ProtocolSite {
        line: 9,
        form: ProtocolForm::With,
        name: None,
    });
    app.protocol.push(ProtocolSite {
        line: 11,
        form: ProtocolForm::Raise,
        name: Some("NotFound".into()),
    });
    let mut enter = entity("app.py", "Ctx.__enter__", EntityKind::Method, 2);
    enter.metadata.extra.insert(
        PROTOCOL_MEMBER_KEY.into(),
        ProtocolMember::Listed(vec![ProtocolForm::With]).to_metadata(),
    );
    let mut equals = entity("app.py", "Ctx.__eq__", EntityKind::Method, 4);
    equals.metadata.extra.insert(
        PROTOCOL_MEMBER_KEY.into(),
        ProtocolMember::Implicit("operators and builtins".into()).to_metadata(),
    );
    let mut fixture = Fixture::default().recorded("app.py", app).declares(
        "app.py",
        "NotFound",
        EntityKind::Class,
        1,
        Visibility::Public,
    );
    fixture.entities.push(enter);
    fixture.entities.push(equals);
    let escape = fixture.census("Ctx.__enter__");
    let protocol = residue(&escape)
        .protocol
        .clone()
        .expect("a protocol residue");
    assert_eq!(protocol.dispatch, "listed");
    assert_eq!(sites(&protocol.sites), vec![("app.py".into(), 10)]);
    let escape = fixture.census("Ctx.__eq__");
    let protocol = residue(&escape)
        .protocol
        .clone()
        .expect("a protocol residue");
    assert_eq!(protocol.dispatch, "implicit");
    assert_eq!(protocol.through.as_deref(), Some("operators and builtins"));
    let escape = fixture.census("NotFound");
    assert_eq!(
        sites(&residue(&escape).implicit_calls),
        vec![("app.py".into(), 12)]
    );
}

#[test]
fn what_no_record_speaks_for_is_listed_or_unknown() {
    let base = || {
        Fixture::default()
            .recorded("app.py", record(ModuleScope::Open))
            .declares(
                "app.py",
                "target",
                EntityKind::Function,
                1,
                Visibility::Public,
            )
    };
    // A parsed file with no entity holds nothing.
    let escape = base().bare("empty.py", Some(true)).census("target");
    assert!(residue(&escape).is_empty(), "{escape:?}");
    // A file with no parse on record is listed.
    let escape = base().bare("skipped.py", None).census("target");
    assert_eq!(
        sites(&residue(&escape).unrecorded),
        vec![("skipped.py".into(), 1)]
    );
    // An unparsed region is listed only when it spells a call name.
    let mut broken = record(ModuleScope::Open);
    broken.unparsed.push(UnparsedRegion {
        first: 4,
        last: 6,
        names: vec!["target".into()],
    });
    broken.unparsed.push(UnparsedRegion {
        first: 9,
        last: 9,
        names: vec!["other".into()],
    });
    let escape = base().recorded("broken.py", broken).census("target");
    assert_eq!(
        sites(&residue(&escape).unrecorded),
        vec![("broken.py".into(), 5)]
    );
    // Entities with no current record make the answer unknown.
    let mut stale = base().recorded("stale.py", record(ModuleScope::Open));
    stale.entities.last_mut().unwrap().metadata.extra.insert(
        "blob_hash".into(),
        serde_json::json!(Hash256::from_bytes([9; 32]).to_string()),
    );
    assert!(matches!(
        stale.census("target"),
        FocalEscape::Unknown { .. }
    ));
    let mut old = base();
    old.entities
        .push(entity("old.py", "legacy", EntityKind::Function, 1));
    old.files.push(CensusFile {
        path: FilePathId::new("old.py"),
        language: LanguageId::Python,
        blob: Hash256::from_bytes(BLOB),
        parsed: Some(true),
    });
    assert!(matches!(old.census("target"), FocalEscape::Unknown { .. }));
    let mut unreadable = base();
    unreadable.unreadable.insert("app.py".into());
    assert!(matches!(
        unreadable.census("target"),
        FocalEscape::Unknown { .. }
    ));
    // Another language's files are outside the domain.
    let escape = base().bare("script.js", None).census("target");
    assert!(residue(&escape).is_empty());
}

#[test]
fn a_focal_with_no_call_name_is_unknown() {
    let fixture = Fixture::default().recorded("app.py", record(ModuleScope::Open));
    let focal = fixture.named("app.py");
    assert!(matches!(
        focal_escape_evidence_in(&fixture, focal, &[]),
        FocalEscape::Unknown { .. }
    ));
}

#[test]
fn a_batch_answers_each_focal_in_order() {
    let mut user = record(ModuleScope::Open);
    user.values.insert(
        "target".into(),
        vec![occurrence(
            1,
            OccurrenceContext::Return,
            OccurrenceBinding::Free,
        )],
    );
    let fixture = Fixture::default()
        .recorded("app.py", record(ModuleScope::Open))
        .declares(
            "app.py",
            "target",
            EntityKind::Function,
            1,
            Visibility::Public,
        )
        .declares(
            "app.py",
            "quiet",
            EntityKind::Function,
            5,
            Visibility::Public,
        )
        .recorded("user.py", user);
    let target = fixture.named("target");
    let quiet = fixture.named("quiet");
    let answers = focal_escape_evidence_batch(
        &fixture,
        &[
            (target, kin_model::focal_call_names(target)),
            (quiet, kin_model::focal_call_names(quiet)),
        ],
    );
    assert!(matches!(answers[0], FocalEscape::Escapes { .. }));
    assert!(matches!(answers[1], FocalEscape::Contained { .. }));
}

#[test]
fn scalar_binding_requires_current_evidence_from_every_python_file() {
    let mut fixture = Fixture::default()
        .recorded("a.py", record(ModuleScope::Open))
        .declares("a.py", "VALUE", EntityKind::Constant, 0, Visibility::Public)
        .recorded("b.py", record(ModuleScope::Open));
    let value = fixture
        .entities
        .iter()
        .position(|entity| entity.name == "VALUE")
        .unwrap();
    fixture.entities[value].metadata.extra.insert(
        "scalar_binding_v1".into(),
        serde_json::json!({
            "version":1,"language":LanguageId::Python.to_string(),"name":"VALUE","immutable":false,
        }),
    );
    let modules: Vec<usize> = fixture
        .entities
        .iter()
        .enumerate()
        .filter(|(_, entity)| entity.kind == EntityKind::Module)
        .map(|(index, _)| index)
        .collect();
    for (index, writes) in [
        (modules[0], serde_json::json!({"VALUE":1})),
        (modules[1], serde_json::json!({"use":1})),
    ] {
        fixture.entities[index].metadata.extra.insert(
            "python_binding_census_v1".into(),
            serde_json::json!({"version":1,"blocked":false,"writes":writes}),
        );
    }
    assert!(matches!(
        fixture.census("VALUE"),
        FocalEscape::NonCallable { .. }
    ));
    fixture.entities[modules[1]]
        .metadata
        .extra
        .get_mut("python_binding_census_v1")
        .unwrap()["writes"]["VALUE"] = serde_json::json!(1);
    assert!(!matches!(
        fixture.census("VALUE"),
        FocalEscape::NonCallable { .. }
    ));
}

fn lookup_on(line: u32, receiver: Option<LookupReceiver>) -> ReflectiveSite {
    let mut site = reflective(line, ReflectiveForm::ComputedMember, ReflectiveReach::Any);
    site.receiver = receiver;
    site
}

fn declared(name: &str, line: u32) -> Option<LookupReceiver> {
    Some(LookupReceiver::Declared {
        name: name.into(),
        line,
    })
}

fn class(name: &str, line: u32, bases: &[&str], open: bool) -> kin_model::ClassBases {
    kin_model::ClassBases {
        name: name.into(),
        line,
        bases: bases.iter().map(|base| base.to_string()).collect(),
        open,
        typing_only: false,
        structural: false,
        dynamic: false,
    }
}

/// `flask.app._make_timedelta`: its only reflective neighbour,
/// `getattr(Flask, method.__name__)`, reads members of a class, never a
/// module-level function, so the focal is contained with nothing listed.
#[test]
fn a_lookup_on_a_declared_class_never_reaches_a_module_function() {
    let mut app = record(ModuleScope::Open);
    app.reflective.push(lookup_on(274, declared("Flask", 100)));
    app.classes.push(class("Flask", 100, &["App"], false));
    let fixture = Fixture::default()
        .recorded("src/flask/app.py", app)
        .declares(
            "src/flask/app.py",
            "_make_timedelta",
            EntityKind::Function,
            73,
            Visibility::Private,
        )
        .declares(
            "src/flask/app.py",
            "public_helper",
            EntityKind::Function,
            80,
            Visibility::Public,
        );
    for name in ["_make_timedelta", "public_helper"] {
        let escape = fixture.census(name);
        assert!(
            matches!(escape, FocalEscape::Contained { .. }),
            "{escape:?}"
        );
        assert!(residue(&escape).is_empty(), "{name}: {escape:?}");
        assert!(!escape.may_escape(), "{name} certifies");
    }
}

/// Soundness: a lookup by a computed name on a module object, on a receiver
/// that may be one, or through the module's own name table still reaches a
/// module function; a lookup on a module object never reaches a class member.
#[test]
fn a_lookup_on_a_module_object_or_globals_still_reaches_a_module_function() {
    let mut app = record(ModuleScope::Open);
    app.reflective.push(reflective(
        2,
        ReflectiveForm::ScopeTable,
        ReflectiveReach::OwnModule,
    ));
    let mut user = record(ModuleScope::Open);
    user.reflective
        .push(lookup_on(4, Some(LookupReceiver::Module)));
    user.reflective.push(lookup_on(6, None));
    let fixture = Fixture::default()
        .recorded("app.py", app)
        .declares(
            "app.py",
            "target",
            EntityKind::Function,
            10,
            Visibility::Public,
        )
        .declares(
            "app.py",
            "_hidden",
            EntityKind::Function,
            12,
            Visibility::Private,
        )
        .declares(
            "app.py",
            "Store.pop",
            EntityKind::Method,
            14,
            Visibility::Public,
        )
        .recorded("user.py", user);
    let lines = |name: &str| reached(residue(&fixture.census(name)));
    assert_eq!(
        lines("target"),
        vec![
            ("app.py".into(), 3),
            ("user.py".into(), 5),
            ("user.py".into(), 7)
        ]
    );
    // `globals()` in its own file reaches a module-private function.
    assert_eq!(lines("_hidden"), vec![("app.py".into(), 3)]);
    // A module object holds module-level bindings, never a class member.
    assert_eq!(lines("Store.pop"), vec![("user.py".into(), 7)]);
}

/// A lookup on a class reads its own members and its bases', so it reaches
/// a member of a class on that hierarchy and no other, and reaches
/// anything when a base on the way is not settled by a declared name.
#[test]
fn a_lookup_on_a_class_reaches_only_members_of_its_hierarchy() {
    let mut app = record(ModuleScope::Open);
    app.classes.push(class("Flask", 10, &["App"], false));
    app.classes.push(class("Mixed", 30, &[], true));
    app.reflective.push(lookup_on(40, declared("Flask", 10)));
    app.reflective.push(lookup_on(41, declared("Mixed", 30)));
    // A declaration the record names as no class: a function or unknown.
    app.reflective.push(lookup_on(42, declared("helper", 50)));
    let mut sansio = record(ModuleScope::Open);
    sansio
        .classes
        .push(class("App", 5, &["Scaffold", "object"], false));
    let mut scaffold = record(ModuleScope::Open);
    scaffold.classes.push(class("Scaffold", 3, &[], false));
    let mut ctx = record(ModuleScope::Open);
    ctx.classes.push(class("AppContext", 8, &[], false));
    let fixture = Fixture::default()
        .recorded("app.py", app)
        .recorded("sansio/app.py", sansio)
        .recorded("scaffold.py", scaffold)
        .recorded("ctx.py", ctx)
        .declares(
            "ctx.py",
            "AppContext.pop",
            EntityKind::Method,
            9,
            Visibility::Public,
        )
        .declares(
            "scaffold.py",
            "Scaffold.route",
            EntityKind::Method,
            4,
            Visibility::Public,
        )
        .declares(
            "app.py",
            "Flask.run",
            EntityKind::Method,
            11,
            Visibility::Public,
        );
    let lines = |name: &str| reached(residue(&fixture.census(name)));
    assert_eq!(
        lines("AppContext.pop"),
        vec![("app.py".into(), 42), ("app.py".into(), 43)]
    );
    assert_eq!(
        lines("Scaffold.route"),
        vec![
            ("app.py".into(), 41),
            ("app.py".into(), 42),
            ("app.py".into(), 43)
        ]
    );
    assert_eq!(
        lines("Flask.run"),
        vec![
            ("app.py".into(), 41),
            ("app.py".into(), 42),
            ("app.py".into(), 43)
        ]
    );
}

/// `Object.values(o)` and `o[k]` over `o`'s own keys yield enumerable
/// properties only, never a method a class body declares, unless an alias
/// stored the method where enumeration reads.
#[test]
fn an_enumerable_only_lookup_never_reaches_a_non_enumerable_member() {
    let mut user = record(ModuleScope::Closed);
    let mut own = lookup_on(3, None);
    own.enumerable_only = true;
    user.reflective.push(own);
    let mut values = reflective(5, ReflectiveForm::Enumerate, ReflectiveReach::Any);
    values.enumerable_only = true;
    user.reflective.push(values);
    let mut fixture = Fixture::default()
        .recorded("store.js", record(ModuleScope::Closed))
        .recorded("user.js", user.clone());
    let mut method = entity("store.js", "Store.containsElement", EntityKind::Method, 20);
    method
        .metadata
        .extra
        .insert(NON_ENUMERABLE_MEMBER_KEY.into(), serde_json::json!(true));
    fixture.entities.push(method.clone());
    fixture.entities.push(entity(
        "store.js",
        "literal.containsElement",
        EntityKind::Method,
        40,
    ));
    let escape = fixture.census("Store.containsElement");
    assert!(residue(&escape).is_empty(), "{escape:?}");
    // An object-literal method is enumerable.
    assert_eq!(
        reached(residue(&fixture.census("literal.containsElement"))),
        vec![("user.js".into(), 4), ("user.js".into(), 6)]
    );
    // Stored under an alias by assignment, the method is an enumerable
    // property of whatever holds it.
    let mut aliased = user;
    let mut stored = occurrence(9, OccurrenceContext::Alias, OccurrenceBinding::Member);
    stored.aliases = vec!["check".into()];
    aliased
        .values
        .insert("containsElement".into(), vec![stored]);
    let mut fixture = Fixture::default()
        .recorded("store.js", record(ModuleScope::Closed))
        .recorded("user.js", aliased);
    fixture.entities.push(method);
    let escape = fixture.census("Store.containsElement");
    assert_eq!(reached(residue(&escape)).len(), 2, "{escape:?}");
}

/// A module function stored as a class attribute under an alias is a member
/// a lookup on an instance reads.
#[test]
fn a_function_aliased_into_a_class_is_reached_by_a_member_lookup() {
    let mut app = record(ModuleScope::Open);
    let mut stored = occurrence(20, OccurrenceContext::Alias, OccurrenceBinding::Module);
    stored.aliases = vec!["handler".into()];
    stored.target_line = Some(1);
    app.values.insert("target".into(), vec![stored]);
    app.reflective.push(reflective(
        30,
        ReflectiveForm::ComputedMember,
        ReflectiveReach::Members,
    ));
    let fixture = Fixture::default().recorded("app.py", app).declares(
        "app.py",
        "target",
        EntityKind::Function,
        1,
        Visibility::Public,
    );
    let escape = fixture.census("target");
    assert_eq!(residue(&escape).alias_names, vec!["handler"]);
    assert_eq!(reached(residue(&escape)), vec![("app.py".into(), 31)]);
}

/// A key whose literal runs hold a character no name holds reaches nothing.
#[test]
fn a_key_that_holds_a_non_name_character_reaches_nothing() {
    let mut user = record(ModuleScope::Closed);
    let mut dashed = lookup_on(2, None);
    dashed.key_prefix = Some("on".into());
    dashed.key_parts = vec!["-".into()];
    user.reflective.push(dashed);
    let mut digits = lookup_on(3, None);
    digits.key_suffix = Some("1".into());
    user.reflective.push(digits);
    let fixture = Fixture::default()
        .recorded("app.js", record(ModuleScope::Closed))
        .declares(
            "app.js",
            "Store.onClick",
            EntityKind::Method,
            5,
            Visibility::Public,
        )
        .recorded("user.js", user);
    assert!(residue(&fixture.census("Store.onClick")).is_empty());
}

/// A lookup is served by what its record says about the focal's name. One
/// whose key literals admit a call name, whose key an adapter resolved from a
/// constant, or whose text spells a call name is a site a reader can check,
/// under `reflective`. One whose record says nothing about any name reaches
/// the focal only as it reaches anything its receiver holds, and is counted
/// under `unkeyed_lookups`: never listed, never dropped.
#[test]
fn a_lookup_is_listed_only_when_its_record_names_the_focal() {
    let mut user = record(ModuleScope::Open);
    // `getattr(module, attr_name)`: a name no record follows.
    user.reflective.push(lookup_on(2, None));
    // `getattr(helpers, "send_" + kind)`.
    let mut built = lookup_on(3, None);
    built.key_prefix = Some("send_".into());
    user.reflective.push(built);
    // `eval("helpers.send_file(path)")`: code that spells the name.
    let mut spelled = reflective(4, ReflectiveForm::Evaluate, ReflectiveReach::Any);
    spelled.excerpt = "eval(\"helpers.send_file(path)\")".into();
    user.reflective.push(spelled);
    // `exec(compile(source, path, "exec"))`: code nothing records.
    user.reflective.push(reflective(
        5,
        ReflectiveForm::Evaluate,
        ReflectiveReach::Any,
    ));
    // `helpers[SEND]` with `const SEND = "send_file"`, which an adapter that
    // resolves constants records as the key's literal.
    let mut constant = lookup_on(6, None);
    constant.key_prefix = Some("send_file".into());
    user.reflective.push(constant);
    let fixture = Fixture::default()
        .recorded("helpers.py", record(ModuleScope::Open))
        .declares(
            "helpers.py",
            "send_file",
            EntityKind::Function,
            1,
            Visibility::Public,
        )
        .declares(
            "helpers.py",
            "render_template",
            EntityKind::Function,
            9,
            Visibility::Public,
        )
        .recorded("user.py", user);

    let escape = fixture.census("send_file");
    let listed = residue(&escape);
    assert_eq!(
        sites(&listed.reflective),
        vec![
            ("user.py".into(), 4),
            ("user.py".into(), 5),
            ("user.py".into(), 7)
        ]
    );
    assert_eq!(
        sites(&listed.unkeyed_lookups),
        vec![("user.py".into(), 3), ("user.py".into(), 6)]
    );
    assert!(!listed.lists_nothing_to_check());
    assert_eq!(listed.unsettled_count(), 3);

    // Nothing here says anything about `render_template`: the prefix and the
    // constant rule it out, and the rest are unkeyed.
    let escape = fixture.census("render_template");
    let listed = residue(&escape);
    assert!(listed.reflective.is_empty(), "{escape:?}");
    assert_eq!(
        sites(&listed.unkeyed_lookups),
        vec![
            ("user.py".into(), 3),
            ("user.py".into(), 5),
            ("user.py".into(), 6)
        ]
    );
    assert!(listed.lists_nothing_to_check());
    assert_eq!(listed.unsettled_count(), 0);
    // Counted, not certified: the census never says nothing could reach it.
    assert!(escape.may_escape(), "{escape:?}");
    assert!(!listed.lists_nothing_unsettled());
}

/// `getattr(helpers, "send_file")` spells the focal's name as a literal key.
/// The adapter records it as a use of the name, so the census lists it with
/// the value uses: a dead-code answer can never count it away with the
/// unkeyed lookups beside it.
#[test]
fn a_literal_lookup_key_naming_the_focal_stays_a_listed_site() {
    let mut user = record(ModuleScope::Open);
    let mut literal = occurrence(8, OccurrenceContext::Assignment, OccurrenceBinding::Member);
    literal.receiver = Some("helpers".into());
    literal.receiver_kind = Some(ReceiverKind::Namespace);
    user.values.insert("send_file".into(), vec![literal]);
    user.reflective.push(lookup_on(2, None));
    let fixture = Fixture::default()
        .recorded("helpers.py", record(ModuleScope::Open))
        .declares(
            "helpers.py",
            "send_file",
            EntityKind::Function,
            1,
            Visibility::Public,
        )
        .recorded("user.py", user);
    let escape = fixture.census("send_file");
    assert!(matches!(escape, FocalEscape::Escapes { .. }), "{escape:?}");
    let listed = residue(&escape);
    assert_eq!(sites(&listed.values), vec![("user.py".into(), 9)]);
    assert_eq!(sites(&listed.unkeyed_lookups), vec![("user.py".into(), 3)]);
    assert!(!listed.lists_nothing_to_check());
}

// ── Mentions: non-call reads matched against stored rows ──────────────────

/// A record that lists its mentions, as a version 2 Python or JavaScript
/// record does.
fn listing(scope: ModuleScope) -> FileEscapeRecord {
    let mut record = FileEscapeRecord::new(scope);
    record.lists_mentions = true;
    record
}

fn mention(line: u32, binding: OccurrenceBinding, form: MentionForm) -> ValueOccurrence {
    let mut occurrence = occurrence(line, OccurrenceContext::Mention, binding);
    occurrence.form = Some(form);
    occurrence
}

fn span_at(path: &str, line: u32) -> SourceSpan {
    SourceSpan {
        file: FilePathId::new(path),
        start_byte: line as usize * 10,
        end_byte: line as usize * 10 + 5,
        start_line: line,
        start_col: 0,
        end_line: line,
        end_col: 0,
    }
}

fn stored(kind: RelationKind, path: &str, line: u32) -> StoredUse {
    StoredUse {
        kind,
        source: None,
        sites: vec![span_at(path, line)],
    }
}

fn mentions_of(escape: &FocalEscape) -> &MentionResidue {
    &residue(escape).mentions
}

/// `flask.app._make_timedelta`: no file reads it, and `sansio/app.py` reads
/// its own same-named declaration in a comparison and an annotation, so a
/// census over records that list their mentions measures none.
#[test]
fn an_unused_focal_measures_no_mention_and_a_same_named_one_elsewhere_is_not_it() {
    let mut sansio = listing(ModuleScope::Open);
    let mut compared = mention(230, OccurrenceBinding::Module, MentionForm::Comparison);
    compared.target_line = Some(51);
    let mut annotated = mention(231, OccurrenceBinding::Module, MentionForm::Annotation);
    annotated.target_line = Some(51);
    sansio
        .values
        .insert("_make_timedelta".into(), vec![compared, annotated]);
    let mut fixture = Fixture::default()
        .recorded("src/flask/app.py", listing(ModuleScope::Open))
        .declares(
            "src/flask/app.py",
            "_make_timedelta",
            EntityKind::Function,
            60,
            Visibility::Private,
        )
        .recorded("src/flask/sansio/app.py", sansio);
    fixture.uses = Some(Vec::new());
    let escape = fixture.census("_make_timedelta");
    assert!(
        matches!(escape, FocalEscape::Contained { .. }),
        "{escape:?}"
    );
    let mentions = mentions_of(&escape);
    assert!(mentions.measured, "{mentions:?}");
    assert_eq!(mentions.count(), 0, "{mentions:?}");
    assert!(
        residue(&escape).is_empty(),
        "mentions never unsettle a call census"
    );
    let summary = escape.summary_json(5);
    assert_eq!(
        summary["residue"]["mentions"],
        serde_json::json!({"measured": true, "count": 0, "matched": {}})
    );
}

/// A comparison and an import of the focal, one covered by a stored
/// `References` row and one by an `Imports` row, are matched; a decorator no
/// row covers is listed by file and line.
#[test]
fn mentions_a_stored_row_covers_are_matched_and_the_rest_are_listed() {
    let mut user = listing(ModuleScope::Open);
    let mut imported = mention(0, OccurrenceBinding::Import, MentionForm::Import);
    imported.import = Some((0, 30));
    imported.module = Some(".app".into());
    let mut compared = mention(5, OccurrenceBinding::Import, MentionForm::Comparison);
    compared.import = Some((0, 30));
    let mut decorated = mention(9, OccurrenceBinding::Import, MentionForm::Decorator);
    decorated.import = Some((0, 30));
    user.values
        .insert("helper".into(), vec![imported, compared, decorated]);
    let mut fixture = Fixture::default()
        .recorded("pkg/app.py", listing(ModuleScope::Open))
        .declares(
            "pkg/app.py",
            "helper",
            EntityKind::Function,
            3,
            Visibility::Public,
        )
        .recorded("pkg/use.py", user)
        .import_into("pkg/use.py", 0, 30);
    fixture.uses = Some(vec![
        stored(RelationKind::Imports, "pkg/use.py", 0),
        stored(RelationKind::References, "pkg/use.py", 5),
    ]);
    let escape = fixture.census("helper");
    assert!(
        matches!(escape, FocalEscape::Contained { .. }),
        "{escape:?}"
    );
    let mentions = mentions_of(&escape);
    assert!(mentions.measured);
    assert_eq!(mentions.matched.get("imports"), Some(&1));
    assert_eq!(mentions.matched.get("references"), Some(&1));
    assert_eq!(sites(&mentions.unmatched), vec![("pkg/use.py".into(), 10)]);
    assert_eq!(mentions.unmatched[0].kind, "decorator");
    assert_eq!(mentions.unmatched[0].name.as_deref(), Some("helper"));
    assert!(residue(&escape).lists_nothing_unsettled());
    let summary = escape.summary_json(5);
    assert_eq!(summary["residue"]["mentions"]["count"], 3);
    assert_eq!(
        summary["residue"]["mentions"]["unmatched"]["sites"][0]["line"],
        10
    );
}

/// A value use is a non-call read too: covered, it is matched beside the
/// escape it still is; an edge that records no site covers a read inside its
/// source entity.
#[test]
fn a_value_use_is_matched_and_a_siteless_row_covers_reads_in_its_source() {
    let mut user = listing(ModuleScope::Open);
    let mut passed = occurrence(12, OccurrenceContext::Argument, OccurrenceBinding::Free);
    passed.form = None;
    user.values.insert("handler".into(), vec![passed]);
    let mut fixture = Fixture::default()
        .recorded("pkg/app.py", listing(ModuleScope::Open))
        .declares(
            "pkg/app.py",
            "handler",
            EntityKind::Function,
            3,
            Visibility::Public,
        )
        .recorded("pkg/use.py", user);
    let mut source = span_at("pkg/use.py", 10);
    source.end_line = 14;
    fixture.uses = Some(vec![StoredUse {
        kind: RelationKind::References,
        source: Some(source),
        sites: Vec::new(),
    }]);
    let escape = fixture.census("handler");
    let FocalEscape::Escapes { residue, .. } = &escape else {
        panic!("{escape:?}");
    };
    assert_eq!(sites(&residue.values), vec![("pkg/use.py".into(), 13)]);
    assert_eq!(residue.mentions.matched.get("references"), Some(&1));
    assert!(residue.mentions.unmatched.is_empty());
}

/// A module is imported by the path of its file, which no occurrence of a
/// name records, so a reading over names that found nothing must not read as
/// whole: `export * from './config'` reads `config` without spelling it.
#[test]
fn a_module_focal_leaves_the_mention_reading_unmeasured() {
    let mut fixture = Fixture::default()
        .recorded("pkg/config.py", listing(ModuleScope::Open))
        .recorded("pkg/use.py", listing(ModuleScope::Open));
    fixture.uses = Some(Vec::new());
    let escape = fixture.census("pkg/config.py");
    let mentions = mentions_of(&escape);
    assert!(!mentions.measured, "{escape:?}");
    assert_eq!(mentions.reason, Some(MENTIONS_MODULE_PATH));
    assert_eq!(
        escape.summary_json(5)["residue"]["mentions"]["measured"],
        false
    );
}

/// A class is referenced by a use of a member through a value of its type,
/// which spells no name, so its reading measures imports and leaves
/// references unmeasured. A function's reading speaks for both.
#[test]
fn a_focal_with_members_leaves_references_unmeasured() {
    let fixture = |kind: EntityKind| {
        let mut fixture = Fixture::default()
            .recorded("pkg/app.py", listing(ModuleScope::Open))
            .declares("pkg/app.py", "Logs", kind, 3, Visibility::Public);
        fixture.uses = Some(Vec::new());
        fixture
    };
    let class = fixture(EntityKind::Class).census("Logs");
    let mentions = mentions_of(&class);
    assert!(mentions.measured, "{class:?}");
    assert_eq!(mentions.references_unmeasured, Some(MENTIONS_MEMBER_READS));
    assert_eq!(
        class.summary_json(5)["residue"]["mentions"]["references_unmeasured"],
        MENTIONS_MEMBER_READS
    );
    let function = fixture(EntityKind::Function).census("Logs");
    assert_eq!(mentions_of(&function).references_unmeasured, None);
}

/// Records written before mentions existed, or by a language that lists
/// none, leave the reading unmeasured and answer everything else exactly as
/// before: a version 1 record still reads.
#[test]
fn records_that_list_no_mentions_leave_the_reading_unmeasured_and_read_as_before() {
    let mut v1 = record(ModuleScope::Open);
    v1.version = 1;
    let mut sansio = record(ModuleScope::Open);
    sansio.version = 1;
    let mut own = occurrence(223, OccurrenceContext::Argument, OccurrenceBinding::Module);
    own.target_line = Some(51);
    sansio.values.insert("_make_timedelta".into(), vec![own]);
    let mut fixture = Fixture::default()
        .recorded("src/flask/app.py", v1)
        .declares(
            "src/flask/app.py",
            "_make_timedelta",
            EntityKind::Function,
            60,
            Visibility::Private,
        )
        .recorded("src/flask/sansio/app.py", sansio);
    fixture.uses = Some(Vec::new());
    let escape = fixture.census("_make_timedelta");
    assert!(
        matches!(escape, FocalEscape::Contained { .. }),
        "a version 1 record is read, not refused: {escape:?}"
    );
    assert!(residue(&escape).is_empty());
    let mentions = mentions_of(&escape);
    assert!(!mentions.measured);
    assert_eq!(mentions.reason, Some(MENTIONS_UNLISTED));
    // One listing record beside a current one that lists none is still
    // unmeasured, for a language whose reader records none: a re-derivation
    // would record none again, so it is not said to be stale. Python's reader
    // always records mentions, so the unit that lists none here is Go, in the
    // domain of a Go focal.
    let mut mixed = Fixture::default()
        .recorded("internal/flask/app.go", listing(ModuleScope::Open))
        .declares(
            "internal/flask/app.go",
            "makeTimedelta",
            EntityKind::Function,
            60,
            Visibility::Private,
        )
        .recorded("internal/flask/cli.go", record(ModuleScope::Open));
    mixed.uses = Some(Vec::new());
    assert_eq!(
        mentions_of(&mixed.census("makeTimedelta")).reason,
        Some(MENTIONS_NOT_READ_FOR_LANGUAGE)
    );
    let summary = mixed.census("makeTimedelta").summary_json(5);
    assert_eq!(summary["residue"]["mentions"]["measured"], false);
    assert!(summary["residue"]["mentions"].get("count").is_none());
}

/// A file with no parse on record, or a graph that cannot read the stored
/// rows, leaves the reading unmeasured.
#[test]
fn an_unrecorded_file_or_unreadable_rows_leave_the_reading_unmeasured() {
    let mut fixture = Fixture::default()
        .recorded("pkg/app.py", listing(ModuleScope::Open))
        .declares(
            "pkg/app.py",
            "helper",
            EntityKind::Function,
            3,
            Visibility::Public,
        )
        .bare("pkg/new.py", None);
    fixture.uses = Some(Vec::new());
    assert_eq!(
        mentions_of(&fixture.census("helper")).reason,
        Some(MENTIONS_UNRECORDED)
    );
    let unreadable = Fixture::default()
        .recorded("pkg/app.py", listing(ModuleScope::Open))
        .declares(
            "pkg/app.py",
            "helper",
            EntityKind::Function,
            3,
            Visibility::Public,
        );
    assert_eq!(
        mentions_of(&unreadable.census("helper")).reason,
        Some(MENTIONS_ROWS_UNREADABLE)
    );
}

/// A mention is never a value use or an alias, whatever it is named after:
/// a focal mentioned everywhere and passed nowhere stays contained.
#[test]
fn a_mention_never_escapes_or_aliases() {
    let mut user = listing(ModuleScope::Open);
    let mut store = mention(4, OccurrenceBinding::Member, MentionForm::Store);
    store.receiver = Some("obj".into());
    store.receiver_kind = Some(ReceiverKind::Other);
    let mut imported = mention(0, OccurrenceBinding::Import, MentionForm::Import);
    imported.import = Some((0, 20));
    user.values.insert("run".into(), vec![store, imported]);
    let mut fixture = Fixture::default()
        .recorded("pkg/app.py", listing(ModuleScope::Open))
        .declares(
            "pkg/app.py",
            "run",
            EntityKind::Function,
            3,
            Visibility::Public,
        )
        .recorded("pkg/use.py", user);
    fixture.uses = Some(Vec::new());
    let escape = fixture.census("run");
    assert!(
        matches!(escape, FocalEscape::Contained { .. }),
        "{escape:?}"
    );
    assert!(residue(&escape).is_empty());
    assert!(!escape.may_escape());
    assert_eq!(
        sites(&mentions_of(&escape).unmatched),
        vec![("pkg/use.py".into(), 1), ("pkg/use.py".into(), 5)]
    );
}

/// A file that says `import json` and reads `json.<name>` on `line`: the
/// member read as the adapter records it, in `context`, and the receiver read
/// it resolves to the one import statement of `module`.
fn reads_on_import(
    record: &mut FileEscapeRecord,
    receiver: &str,
    module: &str,
    name: &str,
    line: u32,
    context: OccurrenceContext,
) {
    let mut read = occurrence(line, context, OccurrenceBinding::Member);
    read.receiver = Some(receiver.into());
    read.receiver_kind = Some(ReceiverKind::Namespace);
    if context == OccurrenceContext::Mention {
        read.form = Some(MentionForm::Other);
    }
    record.values.entry(name.into()).or_default().push(read);
    let mut bound = mention(line, OccurrenceBinding::Import, MentionForm::Receiver);
    bound.module = Some(module.into());
    bound.import = Some((0, 11));
    record
        .values
        .entry(receiver.into())
        .or_default()
        .push(bound);
}

/// `flask.json.load` beside tests that pass the standard library's
/// `json.load`: the focal's own package directory is named `json`, and it
/// sits inside the package `flask`, so no import of `json` finds it.
fn flask_json(tests: FileEscapeRecord) -> Fixture {
    let mut fixture = Fixture::default()
        .bare("src/flask/__init__.py", Some(true))
        .recorded("src/flask/json/__init__.py", listing(ModuleScope::Open))
        .declares(
            "src/flask/json/__init__.py",
            "load",
            EntityKind::Function,
            40,
            Visibility::Public,
        )
        .recorded("tests/test_config.py", tests);
    fixture.uses = Some(Vec::new());
    fixture
}

#[test]
fn a_read_on_a_module_imported_from_outside_is_counted_and_never_holds_the_focal() {
    let mut tests = listing(ModuleScope::Open);
    reads_on_import(
        &mut tests,
        "json",
        "json",
        "load",
        30,
        OccurrenceContext::Argument,
    );
    reads_on_import(
        &mut tests,
        "json",
        "json",
        "load",
        44,
        OccurrenceContext::Argument,
    );
    reads_on_import(
        &mut tests,
        "json",
        "json",
        "load",
        50,
        OccurrenceContext::Mention,
    );
    let escape = flask_json(tests).census("load");
    assert!(
        matches!(escape, FocalEscape::Contained { .. }),
        "{escape:?}"
    );
    let residue = residue(&escape);
    assert!(residue.values.is_empty());
    assert!(mentions_of(&escape).unmatched.is_empty());
    assert_eq!(
        residue.outside_receivers,
        vec![OutsideReceiver {
            receiver: "json".into(),
            module: "json".into(),
            name: "load".into(),
            reads: 3,
        }]
    );
    assert!(residue.lists_nothing_to_check());
    assert_eq!(
        escape.summary_json(8)["residue"]["outside_receivers"],
        serde_json::json!([{"receiver": "json", "module": "json", "name": "load", "reads": 3}])
    );
}

#[test]
fn a_read_on_an_import_the_tree_could_supply_stays_a_use_to_check() {
    // `from flask import json`: the receiver comes from this repository.
    let mut from_flask = listing(ModuleScope::Open);
    reads_on_import(
        &mut from_flask,
        "json",
        "flask",
        "load",
        30,
        OccurrenceContext::Argument,
    );
    let escape = flask_json(from_flask).census("load");
    assert_eq!(
        sites(&residue(&escape).values),
        vec![("tests/test_config.py".into(), 31)]
    );
    assert!(residue(&escape).outside_receivers.is_empty());

    let passes_json = || {
        let mut tests = listing(ModuleScope::Open);
        reads_on_import(
            &mut tests,
            "json",
            "json",
            "load",
            30,
            OccurrenceContext::Argument,
        );
        tests
    };
    // A sibling `json.py` beside the test is what its `import json` finds
    // when the test's directory is on the path.
    let sibling = flask_json(passes_json()).bare("tests/json.py", Some(true));
    assert_eq!(sites(&residue(&sibling.census("load")).values).len(), 1);
    // So is a directory `json` under a directory that is no package.
    let namespace = flask_json(passes_json()).bare("tools/json/decode.py", Some(true));
    assert_eq!(sites(&residue(&namespace.census("load")).values).len(), 1);
    // A repository whose root is itself a package is imported under a name
    // its tree never writes, so nothing is judged outside it.
    let rooted = flask_json(passes_json()).bare("__init__.py", Some(true));
    assert_eq!(sites(&residue(&rooted.census("load")).values).len(), 1);
}

#[test]
fn a_read_the_record_does_not_bind_to_one_import_stays_a_use_to_check() {
    // The receiver is a parameter or a local: the adapter classes it as
    // anything else, and records no import binding for its read.
    let mut local = listing(ModuleScope::Open);
    let mut read = occurrence(30, OccurrenceContext::Argument, OccurrenceBinding::Member);
    read.receiver = Some("tomllib".into());
    read.receiver_kind = Some(ReceiverKind::Other);
    local.values.insert("load".into(), vec![read]);
    let escape = flask_json(local).census("load");
    assert_eq!(sites(&residue(&escape).values).len(), 1);
    assert!(residue(&escape).outside_receivers.is_empty());

    // The import binding is read on another line only: nothing says this
    // read resolves to it.
    let mut elsewhere = listing(ModuleScope::Open);
    reads_on_import(
        &mut elsewhere,
        "json",
        "json",
        "load",
        30,
        OccurrenceContext::Argument,
    );
    elsewhere.values.get_mut("json").unwrap()[0].line = 12;
    assert_eq!(
        sites(&residue(&flask_json(elsewhere).census("load")).values).len(),
        1
    );

    // A dotted receiver is a member of something, not the imported name.
    let mut dotted = listing(ModuleScope::Open);
    reads_on_import(
        &mut dotted,
        "json",
        "json",
        "load",
        30,
        OccurrenceContext::Argument,
    );
    dotted.values.get_mut("load").unwrap()[0].receiver = Some("json.decoder".into());
    assert_eq!(
        sites(&residue(&flask_json(dotted).census("load")).values).len(),
        1
    );
}

/// A method is read through a value of its class, and a module another
/// project supplies can hand such a value out, so a member focal keeps the
/// read.
#[test]
fn a_member_focal_keeps_a_read_on_an_outside_import() {
    let mut tests = listing(ModuleScope::Open);
    reads_on_import(
        &mut tests,
        "json",
        "json",
        "load",
        30,
        OccurrenceContext::Argument,
    );
    let fixture = Fixture::default()
        .recorded("src/flask/provider.py", listing(ModuleScope::Open))
        .declares(
            "src/flask/provider.py",
            "JSONProvider.load",
            EntityKind::Method,
            12,
            Visibility::Public,
        )
        .recorded("tests/test_config.py", tests);
    let escape = fixture.census("JSONProvider.load");
    assert_eq!(sites(&residue(&escape).values).len(), 1);
    assert!(residue(&escape).outside_receivers.is_empty());
}

#[test]
fn an_import_finds_a_name_only_under_a_root_that_is_no_package() {
    let roots = ImportRoots::of([
        "src/flask/__init__.py",
        "src/flask/json/__init__.py",
        "src/flask/json/tag.py",
        "src/flask/cli.py",
        "tests/test_json.py",
        "tests/helpers.py",
        "README.md",
    ]);
    // `json` exists only inside the package `flask`.
    assert!(roots.outside("tests/test_config.py", "json"));
    assert!(roots.outside("tests/test_config.py", "json.decoder"));
    assert!(roots.outside("src/flask/json/__init__.py", "json"));
    // A file run from inside the package finds its sibling first.
    assert!(!roots.outside("src/flask/cli.py", "json"));
    // `flask` under `src`, `helpers` beside the tests, `src` at the root.
    assert!(!roots.outside("tests/test_config.py", "flask.json"));
    assert!(!roots.outside("tests/test_config.py", "helpers"));
    assert!(!roots.outside("tests/test_config.py", "src.flask"));
    assert!(roots.outside("tests/test_config.py", "werkzeug.local"));
    // A relative import, a name that is no dotted name and a file of another
    // language are never judged.
    assert!(!roots.outside("tests/test_config.py", ".json"));
    assert!(!roots.outside("tests/test_config.py", "."));
    assert!(!roots.outside("tests/test_config.py", ""));
    assert!(!roots.outside("tests/test_config.py", "json-ld"));
    assert!(!roots.outside("web/app.ts", "json"));
}

#[test]
fn a_receiver_import_is_read_from_the_record_and_the_tree_on_demand() {
    let mut tests = listing(ModuleScope::Open);
    reads_on_import(
        &mut tests,
        "_json",
        "json",
        "load",
        7,
        OccurrenceContext::Argument,
    );
    reads_on_import(
        &mut tests,
        "flask",
        "flask",
        "json",
        9,
        OccurrenceContext::Mention,
    );
    let fixture = flask_json(tests);
    let mut imports = ReceiverImports::new(&fixture);
    let path = FilePathId::new("tests/test_config.py");
    assert_eq!(
        imports.of(&path, "_json", 7),
        Some(ReceiverImport {
            module: "json".into(),
            outside_repository: true,
        })
    );
    assert_eq!(
        imports.of(&path, "flask", 9),
        Some(ReceiverImport {
            module: "flask".into(),
            outside_repository: false,
        })
    );
    assert_eq!(imports.of(&path, "_json", 8), None);
    assert_eq!(imports.of(&path, "other", 7), None);
    assert_eq!(
        imports.of(&FilePathId::new("tests/missing.py"), "_json", 7),
        None
    );
}

/// A module's own imports, as far as its record holds them: every
/// `from m import f`, under the name the file reads it by, and an `import m`
/// the file reads. Each says whether the repository can supply its module.
#[test]
fn a_files_imported_names_are_read_from_its_record() {
    let mut tests = listing(ModuleScope::Open);
    // `import json`, read as a receiver.
    reads_on_import(
        &mut tests,
        "json",
        "json",
        "load",
        30,
        OccurrenceContext::Argument,
    );
    // `from flask import Flask`, the statement itself.
    let mut flask = mention(1, OccurrenceBinding::Import, MentionForm::Import);
    flask.module = Some("flask".into());
    flask.import = Some((12, 35));
    tests.values.insert("Flask".into(), vec![flask]);
    // `from werkzeug.local import LocalProxy as Proxy`, and a read of `Proxy`.
    let mut renamed = occurrence(2, OccurrenceContext::Import, OccurrenceBinding::Import);
    renamed.aliases = vec!["Proxy".into()];
    renamed.module = Some("werkzeug.local".into());
    renamed.import = Some((36, 80));
    tests.values.insert("LocalProxy".into(), vec![renamed]);
    let mut read = mention(9, OccurrenceBinding::Import, MentionForm::Other);
    read.module = Some("werkzeug.local".into());
    read.imported = Some("LocalProxy".into());
    tests.values.insert("Proxy".into(), vec![read]);
    // A member named like an import is no import.
    let mut member = mention(11, OccurrenceBinding::Member, MentionForm::Other);
    member.receiver = Some("app".into());
    tests.values.insert("config".into(), vec![member]);

    let fixture = flask_json(tests);
    let mut imports = ReceiverImports::new(&fixture);
    let bound = imports
        .bound_in(&FilePathId::new("tests/test_config.py"))
        .expect("the file's record");
    let rows: Vec<(&str, &str, Option<&str>, bool)> = bound
        .iter()
        .map(|row| {
            (
                row.name.as_str(),
                row.module.as_str(),
                row.imported.as_deref(),
                row.outside_repository,
            )
        })
        .collect();
    assert_eq!(
        rows,
        vec![
            ("Flask", "flask", None, false),
            ("Proxy", "werkzeug.local", Some("LocalProxy"), true),
            ("json", "json", None, true),
        ]
    );
    assert_eq!(imports.bound_in(&FilePathId::new("web/app.ts")), None);
    assert_eq!(imports.bound_in(&FilePathId::new("tests/missing.py")), None);
}

/// A census graph that counts how often the census asks it for each file's
/// member reads, and answers as the fixture it wraps does.
struct CountingReads<'f> {
    inner: &'f Fixture,
    asked: std::cell::RefCell<HashMap<String, usize>>,
}

impl EscapeCensusGraph for CountingReads<'_> {
    fn census_files(&self, languages: &[LanguageId]) -> Option<Vec<CensusFile>> {
        self.inner.census_files(languages)
    }

    fn entities_in(&self, path: &FilePathId) -> Option<Vec<Entity>> {
        self.inner.entities_in(path)
    }

    fn import_evidence_into(&self, target: EntityId) -> Option<Vec<RelationEvidence>> {
        self.inner.import_evidence_into(target)
    }

    fn uses_into(&self, target: EntityId) -> Option<Vec<StoredUse>> {
        self.inner.uses_into(target)
    }

    fn outside_member_reads(&self, file: &CensusFile) -> Vec<OutsideMemberRead> {
        *self
            .asked
            .borrow_mut()
            .entry(file.path.0.clone())
            .or_default() += 1;
        Vec::new()
    }
}

/// A batch census reads each file's member reads once, however many of its
/// focals have names read there.
///
/// What a graph holds for a file does not depend on the focal whose names led
/// the census to it. A guide of a change asks about every entity the change
/// reaches in one batch, and each focal used to read every such file again.
#[test]
fn a_batch_reads_each_files_member_reads_once_for_all_its_focals() {
    let mut user = record(ModuleScope::Open);
    for (name, line) in [("first", 3), ("second", 7)] {
        let mut read = occurrence(line, OccurrenceContext::Argument, OccurrenceBinding::Member);
        read.receiver = Some("obj".into());
        read.receiver_kind = Some(ReceiverKind::Other);
        user.values.insert(name.into(), vec![read]);
    }
    let mut owner = record(ModuleScope::Open);
    owner.classes.push(class("Owner", 1, &[], false));
    let fixture = Fixture::default()
        .recorded("app.py", owner)
        .declares(
            "app.py",
            "Owner.first",
            EntityKind::Method,
            2,
            Visibility::Public,
        )
        .declares(
            "app.py",
            "Owner.second",
            EntityKind::Method,
            4,
            Visibility::Public,
        )
        .recorded("user.py", user);
    let first = fixture.named("Owner.first");
    let second = fixture.named("Owner.second");
    let counting = CountingReads {
        inner: &fixture,
        asked: Default::default(),
    };
    let answers = focal_escape_evidence_batch(
        &counting,
        &[
            (first, kin_model::focal_call_names(first)),
            (second, kin_model::focal_call_names(second)),
        ],
    );
    assert_eq!(answers.len(), 2);
    assert_eq!(
        counting.asked.borrow().get("user.py").copied(),
        Some(1),
        "the file both focals' names are read in is read once: {:?}",
        counting.asked.borrow()
    );
}
