// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC
// Included inside mcp_commit::tests to reuse exact publication fixtures.
//
// Entity-grain lifecycle per language, through the commit route: what each
// language's adapter hooks let the planner do, and the one sentence it says
// where a language lacks a form.

/// Each language's unit: two top-level functions, the second of them led by
/// its doc comment where the language writes one outside the declaration,
/// and a function to create beside the first, led by its own.
const LIFECYCLE_UNITS: &[(&str, &str, &str)] = &[
    (
        "pkg/source.py",
        "def anchor():\n    return 1\n\n\ndef sibling():\n    return 2\n",
        "def added():\n    \"\"\"Three.\"\"\"\n    return 3",
    ),
    (
        "src/source.ts",
        "export function anchor(): number {\n  return 1;\n}\n\n/** Two. */\nexport function sibling(): number {\n  return 2;\n}\n",
        "/** Three. */\nexport function added(): number {\n  return 3;\n}",
    ),
    (
        "src/source.js",
        "export const anchor = () => 1;\n\n/** Two. */\nexport const sibling = () => 2;\n",
        "/** Three. */\nexport const added = () => 3;",
    ),
    (
        "pkg/source.go",
        "package sample\n\nfunc anchor() int { return 1 }\n\n// sibling returns two.\nfunc sibling() int { return 2 }\n",
        "// added returns three.\nfunc added() int { return 3 }",
    ),
    (
        "src/lib.rs",
        "pub fn anchor() -> u8 {\n    1\n}\n\n/// Two.\npub fn sibling() -> u8 {\n    2\n}\n",
        "/// Three.\npub fn added() -> u8 {\n    3\n}",
    ),
];

#[test]
fn a_function_is_created_beside_another_and_removed_with_its_doc_in_every_lifecycle_language() {
    use kin_mcp::entity_lifecycle::EntityPlacement::SiblingAfter;
    for (file, original, creation) in LIFECYCLE_UNITS {
        let (_dir, state) = test_state();
        let (anchor, _) = install_exact_source(&state, file, original.as_bytes(), "anchor");
        let sessions = test_sessions();
        let arguments = lifecycle_stage(
            &sessions,
            vec![lifecycle_create(&state, &anchor, SiblingAfter, creation)],
        );
        let result = commit_exact_transaction(&state, &sessions, &arguments, None);
        assert_ne!(
            result.is_error,
            Some(true),
            "{file}: {}",
            result_text(&result)
        );
        let written = std::fs::read_to_string(state.layout.working_dir().join(file)).unwrap();
        assert!(written.contains(creation), "{file}: {written}");
        // Written after the anchor, as far from it as the next declaration
        // stood.
        assert!(
            written.find("anchor").unwrap() < written.find(creation).unwrap()
                && written.find(creation).unwrap() < written.find("sibling").unwrap(),
            "{file}: {written}"
        );
        let added = lifecycle_entity(&state, "added", kin_model::EntityKind::Function)
            .unwrap_or_else(|| panic!("{file}"));
        assert_eq!(added.file_origin.as_ref().unwrap().0, *file);
        // Removing it takes its whole statement, its doc comment and the
        // space that set it apart, so the unit reads as it did.
        let arguments = lifecycle_stage(&sessions, vec![lifecycle_remove(&state, &added)]);
        let result = commit_exact_transaction(&state, &sessions, &arguments, None);
        assert_ne!(
            result.is_error,
            Some(true),
            "{file}: {}",
            result_text(&result)
        );
        assert!(
            state.graph.get_entity(&added.id).unwrap().is_none(),
            "{file}"
        );
        assert_eq!(
            std::fs::read_to_string(state.layout.working_dir().join(file)).unwrap(),
            *original,
            "{file}"
        );
        // The sibling goes the same way, its doc comment with it.
        let sibling = lifecycle_entity(&state, "sibling", kin_model::EntityKind::Function).unwrap();
        let arguments = lifecycle_stage(&sessions, vec![lifecycle_remove(&state, &sibling)]);
        let result = commit_exact_transaction(&state, &sessions, &arguments, None);
        assert_ne!(
            result.is_error,
            Some(true),
            "{file}: {}",
            result_text(&result)
        );
        let left = std::fs::read_to_string(state.layout.working_dir().join(file)).unwrap();
        assert!(
            !left.contains("sibling")
                && !left.to_lowercase().contains("two")
                && left.contains("anchor"),
            "{file}: {left}"
        );
    }
}

#[test]
fn a_function_starts_a_new_unit_beside_its_anchor_where_the_language_places_one() {
    use kin_mcp::entity_lifecycle::EntityPlacement::NewSourceUnit;
    for (file, original, creation) in LIFECYCLE_UNITS {
        let (_dir, state) = test_state();
        let (anchor, _) = install_exact_source(&state, file, original.as_bytes(), "anchor");
        let sessions = test_sessions();
        let arguments = lifecycle_stage(
            &sessions,
            vec![lifecycle_create(&state, &anchor, NewSourceUnit, creation)],
        );
        let result = commit_exact_transaction(&state, &sessions, &arguments, None);
        let extension = file.rsplit_once('.').unwrap().1;
        let unit = format!("{}/added.{extension}", file.rsplit_once('/').unwrap().0);
        if extension == "rs" {
            // A Rust unit belongs to its crate only through a `mod` line in
            // its parent module, which this lifecycle does not write.
            assert_eq!(result.is_error, Some(true));
            assert!(
                result_text(&result).contains(
                    "Rust new source units require a module-owner declaration; use \
                     sibling_after until module-owner creation is supported"
                ),
                "{}",
                result_text(&result)
            );
            assert!(!state.layout.working_dir().join(&unit).exists());
            continue;
        }
        assert_ne!(
            result.is_error,
            Some(true),
            "{file}: {}",
            result_text(&result)
        );
        let written = std::fs::read_to_string(state.layout.working_dir().join(&unit)).unwrap();
        let prelude = if extension == "go" {
            "package sample\n\n"
        } else {
            ""
        };
        assert_eq!(written, format!("{prelude}{creation}\n"), "{unit}");
        assert_eq!(
            lifecycle_entity(&state, "added", kin_model::EntityKind::Function)
                .unwrap()
                .file_origin
                .unwrap()
                .0,
            unit
        );
        assert_eq!(
            std::fs::read_to_string(state.layout.working_dir().join(file)).unwrap(),
            *original
        );
    }
}

#[test]
fn a_language_without_a_lifecycle_is_refused_in_one_sentence() {
    use kin_mcp::entity_lifecycle::EntityPlacement::SiblingAfter;
    let (_dir, state) = test_state();
    let original = "package app\n\nfun anchor(): Int = 1\n";
    let (anchor, _) = install_exact_source(&state, "src/app/Run.kt", original.as_bytes(), "anchor");
    let sessions = test_sessions();
    let arguments = lifecycle_stage(
        &sessions,
        vec![lifecycle_create(
            &state,
            &anchor,
            SiblingAfter,
            "fun added(): Int = 3",
        )],
    );
    let result = commit_exact_transaction(&state, &sessions, &arguments, None);
    assert_eq!(result.is_error, Some(true));
    assert!(
        result_text(&result).contains(
            "entity lifecycle currently supports top-level leaf functions in TypeScript, \
             JavaScript, Python, Go and Rust"
        ),
        "{}",
        result_text(&result)
    );
    assert_eq!(
        std::fs::read_to_string(state.layout.working_dir().join("src/app/Run.kt")).unwrap(),
        original
    );
}

/// Each member language's unit: a type with two methods, the method to
/// create beside the first, the method to add as the type's last member
/// (`None` where the language writes methods outside the type, with the
/// sentence it refuses with), and a type to create after the first.
struct MemberUnit {
    file: &'static str,
    original: &'static str,
    method: &'static str,
    beside: &'static str,
    added: &'static str,
    member: Result<(&'static str, &'static str), &'static str>,
    other: &'static str,
}

const MEMBER_UNITS: &[MemberUnit] = &[
    MemberUnit {
        file: "pkg/store.py",
        original: "class Store:\n    def get(self):\n        return 1\n\n    def put(self):\n        return 2\n",
        method: "Store.get",
        beside: "def added(self):\n    return 3",
        added: "Store.added",
        member: Ok(("def last(self):\n    return 4", "Store.last")),
        other: "class Other:\n    limit = 3",
    },
    MemberUnit {
        file: "src/store.ts",
        original: "export class Store {\n  get(): number {\n    return 1;\n  }\n\n  put(): number {\n    return 2;\n  }\n}\n",
        method: "Store.get",
        beside: "/** Three. */\nadded(): number {\n  return 3;\n}",
        added: "Store.added",
        member: Ok(("last(): number {\n  return 4;\n}", "Store.last")),
        other: "export class Other {\n  run(): void {}\n}",
    },
    MemberUnit {
        file: "src/store.js",
        original: "export class Store {\n  get() {\n    return 1;\n  }\n\n  put() {\n    return 2;\n  }\n}\n",
        method: "Store.get",
        beside: "added() {\n  return 3;\n}",
        added: "Store.added",
        member: Ok(("last() {\n  return `four\nlines`;\n}", "Store.last")),
        other: "export class Other {\n  run() {}\n}",
    },
    MemberUnit {
        file: "src/lib.rs",
        original: "pub struct Store;\n\nimpl Store {\n    pub fn get(&self) -> u8 {\n        1\n    }\n\n    pub fn put(&self) -> u8 {\n        2\n    }\n}\n",
        method: "Store::get",
        beside: "/// Three.\npub fn added(&self) -> u8 {\n    3\n}",
        added: "Store::added",
        member: Err("a Rust type declares its fields in its body, and its methods in impl blocks"),
        other: "pub struct Other {\n    value: u8,\n}",
    },
    MemberUnit {
        file: "pkg/store.go",
        original: "package store\n\ntype Store struct{}\n\nfunc (s Store) Get() int { return 1 }\n\nfunc (s Store) Put() int { return 2 }\n",
        method: "Store.Get",
        beside: "// Added returns three.\nfunc (s Store) Added() int { return 3 }",
        added: "Store.Added",
        member: Err("a Go type declares its fields in its body, and its methods beside it"),
        other: "type Other struct {\n\tvalue int\n}",
    },
];

/// The name a creation gives: the member's own identifier.
fn own_name(qualified: &str) -> &str {
    qualified.rsplit(['.', ':']).next().unwrap()
}

#[test]
fn a_method_is_created_beside_another_or_in_its_type_and_removed_in_every_member_language() {
    use kin_mcp::entity_lifecycle::EntityPlacement::{Member, SiblingAfter};
    use kin_mcp::source_unit::DeclarationKind;
    use kin_model::EntityKind;
    for unit in MEMBER_UNITS {
        let file = unit.file;
        let (_dir, state) = test_state();
        let (method, _) = install_exact_source(&state, file, unit.original.as_bytes(), unit.method);
        let sessions = test_sessions();
        // Beside a method of its type, as far from it as the next one.
        let result = lifecycle_commit(
            &state,
            &sessions,
            lifecycle_create_as(
                &state,
                &method,
                SiblingAfter,
                DeclarationKind::Method,
                own_name(unit.added),
                unit.beside,
            ),
        );
        assert_ne!(
            result.is_error,
            Some(true),
            "{file}: {}",
            result_text(&result)
        );
        let written = lifecycle_disk(&state, file);
        let added = lifecycle_entity(&state, unit.added, EntityKind::Method)
            .unwrap_or_else(|| panic!("{file}: {written}"));
        assert!(
            written
                .find(unit.method.rsplit(['.', ':']).next().unwrap())
                .unwrap()
                < written.find(own_name(unit.added)).unwrap(),
            "{file}: {written}"
        );
        // Removed with its doc comment, the unit reads as it did.
        let result = lifecycle_commit(&state, &sessions, lifecycle_remove(&state, &added));
        assert_ne!(
            result.is_error,
            Some(true),
            "{file}: {}",
            result_text(&result)
        );
        assert_eq!(lifecycle_disk(&state, file), unit.original, "{file}");
        // As the last member of its type, or refused in one sentence where
        // the language writes methods outside the type.
        let class = lifecycle_entity(&state, "Store", EntityKind::Class).unwrap();
        match unit.member {
            Ok((body, name)) => {
                let result = lifecycle_commit(
                    &state,
                    &sessions,
                    lifecycle_create_as(
                        &state,
                        &class,
                        Member,
                        DeclarationKind::Method,
                        own_name(name),
                        body,
                    ),
                );
                assert_ne!(
                    result.is_error,
                    Some(true),
                    "{file}: {}",
                    result_text(&result)
                );
                let written = lifecycle_disk(&state, file);
                let last = lifecycle_entity(&state, name, EntityKind::Method)
                    .unwrap_or_else(|| panic!("{file}: {written}"));
                assert!(
                    written.find(own_name(name)).unwrap() > written.rfind("return 2").unwrap(),
                    "{file}: {written}"
                );
                let result = lifecycle_commit(&state, &sessions, lifecycle_remove(&state, &last));
                assert_ne!(
                    result.is_error,
                    Some(true),
                    "{file}: {}",
                    result_text(&result)
                );
                assert_eq!(lifecycle_disk(&state, file), unit.original, "{file}");
            }
            Err(sentence) => {
                let result = lifecycle_commit(
                    &state,
                    &sessions,
                    lifecycle_create_as(
                        &state,
                        &class,
                        Member,
                        DeclarationKind::Method,
                        "last",
                        "fn last() {}",
                    ),
                );
                assert_eq!(result.is_error, Some(true), "{file}");
                assert!(
                    result_text(&result).contains(sentence),
                    "{file}: {}",
                    result_text(&result)
                );
                assert_eq!(lifecycle_disk(&state, file), unit.original, "{file}");
            }
        }
        // A type after the type.
        let class = lifecycle_entity(&state, "Store", EntityKind::Class).unwrap();
        let result = lifecycle_commit(
            &state,
            &sessions,
            lifecycle_create_as(
                &state,
                &class,
                SiblingAfter,
                DeclarationKind::Class,
                "Other",
                unit.other,
            ),
        );
        assert_ne!(
            result.is_error,
            Some(true),
            "{file}: {}",
            result_text(&result)
        );
        let written = lifecycle_disk(&state, file);
        assert!(
            lifecycle_entity(&state, "Other", EntityKind::Class).is_some()
                && written.contains(unit.other.lines().next().unwrap()),
            "{file}: {written}"
        );
    }
}

/// A unit of `language` addressed by identity.
fn unit_of(
    language: kin_model::LanguageId,
    package: &str,
    name: &str,
    role: kin_mcp::source_unit::UnitRole,
) -> kin_mcp::source_unit::SourceUnit {
    kin_mcp::source_unit::SourceUnit {
        language,
        package: package.into(),
        name: name.into(),
        role,
    }
}

/// Each unit language's empty-repository build: a unit, a function created
/// in it with an import, a type created after the function, the import
/// swapped for another, and the unit as it then reads.
struct GreenfieldUnit {
    unit: kin_mcp::source_unit::SourceUnit,
    path: &'static str,
    function: (&'static str, &'static str),
    import: &'static str,
    kind: kin_mcp::source_unit::DeclarationKind,
    type_: (&'static str, &'static str),
    swapped: &'static str,
    reads: &'static str,
}

#[test]
fn a_unit_is_built_from_an_empty_repository_in_every_unit_language() {
    use kin_mcp::source_unit::{DeclarationKind as Kind, UnitRole};
    use kin_model::LanguageId;
    let units = [
        GreenfieldUnit {
            unit: unit_of(LanguageId::TypeScript, "src/runs", "cache", UnitRole::Source),
            path: "src/runs/cache.ts",
            function: (
                "cacheKey",
                "export function cacheKey(id: string): string {\n  return `runs:${id}`;\n}",
            ),
            import: "node:path",
            kind: Kind::Class,
            type_: (
                "RunCache",
                "export class RunCache {\n  key(id: string): string {\n    return cacheKey(id);\n  }\n}",
            ),
            swapped: "node:fs",
            reads: "import \"node:fs\";\n\nexport function cacheKey(id: string): string {\n  return `runs:${id}`;\n}\n\nexport class RunCache {\n  key(id: string): string {\n    return cacheKey(id);\n  }\n}\n",
        },
        GreenfieldUnit {
            unit: unit_of(LanguageId::JavaScript, "src", "util", UnitRole::Source),
            path: "src/util.js",
            function: ("double", "export function double(value) {\n  return value * 2;\n}"),
            import: "node:path",
            kind: Kind::Class,
            type_: ("Doubler", "export class Doubler {\n  run(value) {\n    return double(value);\n  }\n}"),
            swapped: "node:fs",
            reads: "import \"node:fs\";\n\nexport function double(value) {\n  return value * 2;\n}\n\nexport class Doubler {\n  run(value) {\n    return double(value);\n  }\n}\n",
        },
        GreenfieldUnit {
            unit: unit_of(LanguageId::Python, "shop", "ledger", UnitRole::Source),
            path: "shop/ledger.py",
            function: ("total", "def total(values):\n    return sum(values)"),
            import: "decimal",
            kind: Kind::Class,
            type_: (
                "Account",
                "class Account:\n    def __init__(self):\n        self.balance = 0",
            ),
            swapped: "fractions",
            reads: "import fractions\n\n\ndef total(values):\n    return sum(values)\n\n\nclass Account:\n    def __init__(self):\n        self.balance = 0\n",
        },
        GreenfieldUnit {
            unit: unit_of(LanguageId::Rust, ".", "lib", UnitRole::Source),
            path: "src/lib.rs",
            function: (
                "total",
                "/// The sum.\npub fn total(values: &[u64]) -> u64 {\n    values.iter().sum()\n}",
            ),
            import: "std::collections::BTreeMap",
            kind: Kind::Struct,
            type_: ("Account", "pub struct Account {\n    pub balance: u64,\n}"),
            swapped: "std::collections::HashMap",
            reads: "use std::collections::HashMap;\n\n/// The sum.\npub fn total(values: &[u64]) -> u64 {\n    values.iter().sum()\n}\n\npub struct Account {\n    pub balance: u64,\n}\n",
        },
    ];
    for unit in units {
        let path = unit.path;
        let (_dir, state) = test_state();
        assert_eq!(state.graph.resolved_tree().artifacts().count(), 0, "{path}");
        let sessions = test_sessions();
        let (name, body) = unit.function;
        let result = commit_unit_ops(
            &state,
            &sessions,
            vec![unit_create_op(
                &current_base(&state),
                unit.unit.clone(),
                name,
                Kind::Function,
                body,
                &[unit.import],
            )],
        );
        assert_ne!(
            result.is_error,
            Some(true),
            "{path}: {}",
            result_text(&result)
        );
        assert!(project(&state, path).contains(unit.import), "{path}");
        assert!(
            lifecycle_entity(&state, name, kin_model::EntityKind::Function).is_some(),
            "{path}"
        );
        let (type_name, type_body) = unit.type_;
        let result = commit_unit_ops(
            &state,
            &sessions,
            vec![unit_create_op(
                &current_base(&state),
                unit.unit.clone(),
                type_name,
                unit.kind,
                type_body,
                &[],
            )],
        );
        assert_ne!(
            result.is_error,
            Some(true),
            "{path}: {}",
            result_text(&result)
        );
        let result = commit_unit_ops(
            &state,
            &sessions,
            vec![unit_imports_op(
                &current_base(&state),
                unit.unit.clone(),
                &[unit.swapped],
                &[unit.import],
            )],
        );
        assert_ne!(
            result.is_error,
            Some(true),
            "{path}: {}",
            result_text(&result)
        );
        assert_eq!(project(&state, path), unit.reads, "{path}");
        assert!(
            lifecycle_entity(&state, type_name, kin_model::EntityKind::Class).is_some(),
            "{path}"
        );
    }
}

#[test]
fn a_unit_a_language_cannot_place_is_refused_in_one_sentence() {
    use kin_mcp::source_unit::{DeclarationKind as Kind, UnitRole};
    use kin_model::LanguageId;
    for (unit, said) in [
        (
            unit_of(LanguageId::Rust, ".", "store", UnitRole::Source),
            "must be lib or main, the crate root",
        ),
        (
            unit_of(LanguageId::TypeScript, "src", "cache.ts", UnitRole::Source),
            "with no extension",
        ),
        (
            unit_of(
                LanguageId::Python,
                "shop/models",
                "ledger",
                UnitRole::Source,
            ),
            "dotted package path",
        ),
        (
            unit_of(LanguageId::Kotlin, ".", "app", UnitRole::Source),
            "Kotlin declarations are not created by unit",
        ),
    ] {
        let (_dir, state) = test_state();
        let operation = unit_create_op(
            &current_base(&state),
            unit,
            "run",
            Kind::Function,
            "fun run() {}",
            &[],
        );
        let refusal = kin_mcp::session::validate_semantic_operations(&[operation]).unwrap_err();
        assert!(refusal.contains(said), "{refusal}");
        assert_eq!(state.graph.resolved_tree().artifacts().count(), 0);
    }
}
