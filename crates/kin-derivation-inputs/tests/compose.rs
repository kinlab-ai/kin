// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! A key moves when code that decides its product moves, and only then.
//!
//! Each test builds a small workspace on disk, with a composer crate `app`
//! that depends on a workspace crate `core` and two registry crates, and
//! changes one thing at a time.

use std::fs;
use std::path::{Path, PathBuf};

use kin_derivation_inputs::{compose, CLASSIFICATION_FILE};

const ROOT_MANIFEST: &str = r#"[workspace]
members = ["crates/app", "crates/core"]

[workspace.dependencies]
core = { path = "crates/core" }
"#;

const LOCK: &str = r#"version = 4

[[package]]
name = "app"
version = "0.1.0"
dependencies = [
 "core",
 "grammar",
 "serde",
]

[[package]]
name = "core"
version = "0.1.0"

[[package]]
name = "grammar"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "1111111111111111111111111111111111111111111111111111111111111111"

[[package]]
name = "serde"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "2222222222222222222222222222222222222222222222222222222222222222"
"#;

const APP_MANIFEST: &str = r#"[package]
name = "app"
version = "0.1.0"
edition = "2021"

[dependencies]
core = { workspace = true }
grammar = "1"
serde = "1"

[dev-dependencies]
tempfile = "3"
"#;

const APP_CLASSIFICATION: &str = r#"roots = ["src"]

[[decides]]
products = ["graph"]
files = ["src/lib.rs", "src/link.rs", "src/parse.rs"]

[[excluded]]
reason = "how long a question may wait; it decides no record"
files = ["src/pace.rs"]

[dependencies]
core = { decides = ["graph"] }
grammar = { decides = ["graph"] }
serde = { reason = "serialization; each record carries its own schema" }
"#;

const CORE_MANIFEST: &str = r#"[package]
name = "core"
version = "0.1.0"
edition = "2021"
"#;

const CORE_CLASSIFICATION: &str = r#"roots = ["src"]

[[decides]]
products = ["graph"]
files = ["src/lib.rs", "src/ids.rs"]

[[excluded]]
reason = "tests"
files = ["src/tests.rs"]
"#;

const DECIDING: &[&str] = &[
    "crates/app/src/lib.rs",
    "crates/app/src/link.rs",
    "crates/app/src/parse.rs",
    "crates/core/src/lib.rs",
    "crates/core/src/ids.rs",
];

struct Workspace {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Workspace {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let workspace = Self { _dir: dir, root };
        workspace.write("Cargo.toml", ROOT_MANIFEST);
        workspace.write("Cargo.lock", LOCK);
        workspace.write("crates/app/Cargo.toml", APP_MANIFEST);
        workspace.write(
            &format!("crates/app/{CLASSIFICATION_FILE}"),
            APP_CLASSIFICATION,
        );
        workspace.write(
            "crates/app/src/lib.rs",
            "pub mod link;\npub mod parse;\nmod pace;\n",
        );
        workspace.write(
            "crates/app/src/link.rs",
            "/// Binds a call to the declaration it reaches.\npub fn link(name: &str) -> usize {\n    name.len()\n}\n",
        );
        workspace.write(
            "crates/app/src/parse.rs",
            "pub fn parse(text: &str) -> Vec<&str> {\n    text.split(' ').collect()\n}\n",
        );
        workspace.write("crates/app/src/pace.rs", "pub const WAIT_MS: u64 = 30;\n");
        workspace.write("crates/core/Cargo.toml", CORE_MANIFEST);
        workspace.write(
            &format!("crates/core/{CLASSIFICATION_FILE}"),
            CORE_CLASSIFICATION,
        );
        workspace.write(
            "crates/core/src/lib.rs",
            "pub mod ids;\n#[cfg(test)]\nmod tests;\n",
        );
        workspace.write(
            "crates/core/src/ids.rs",
            "pub fn id(name: &str) -> u64 {\n    name.bytes().map(u64::from).sum()\n}\n",
        );
        workspace.write("crates/core/src/tests.rs", "#[test]\nfn ids() {}\n");
        workspace
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }

    fn write(&self, relative: &str, text: &str) {
        let path = self.path(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn read(&self, relative: &str) -> String {
        fs::read_to_string(self.path(relative)).unwrap()
    }

    fn append(&self, relative: &str, text: &str) {
        let mut current = self.read(relative);
        current.push_str(text);
        self.write(relative, &current);
    }

    fn replace(&self, relative: &str, from: &str, to: &str) {
        let current = self.read(relative);
        assert!(current.contains(from), "{relative} holds no `{from}`");
        self.write(relative, &current.replacen(from, to, 1));
    }

    fn app(&self) -> PathBuf {
        self.path("crates/app")
    }

    fn key(&self) -> String {
        compose(&self.app(), &["graph"])
            .unwrap_or_else(|problems| panic!("{problems:#?}"))
            .key("graph")
            .unwrap()
            .to_string()
    }

    fn problems(&self) -> Vec<String> {
        match compose(&self.app(), &["graph"]) {
            Ok(_) => panic!("the composition was accepted"),
            Err(problems) => problems,
        }
    }

    /// The bytes of every classification, to show a change left them alone.
    fn classifications(&self) -> Vec<String> {
        ["crates/app", "crates/core"]
            .iter()
            .map(|krate| self.read(&format!("{krate}/{CLASSIFICATION_FILE}")))
            .collect()
    }
}

fn mentions(problems: &[String], needle: &str) -> bool {
    problems.iter().any(|problem| problem.contains(needle))
}

#[test]
fn the_key_covers_every_deciding_file_and_dependency_and_nothing_else() {
    let workspace = Workspace::new();
    let composition = compose(&workspace.app(), &["graph"]).unwrap();
    let names: Vec<&str> = composition
        .inputs("graph")
        .iter()
        .map(|input| input.name.as_str())
        .collect();
    assert_eq!(
        names,
        [
            "crates/app/Cargo.toml grammar",
            "crates/app/src/lib.rs",
            "crates/app/src/link.rs",
            "crates/app/src/parse.rs",
            "crates/core/src/ids.rs",
            "crates/core/src/lib.rs",
        ]
    );
    let key = composition.key("graph").unwrap();
    assert!(key.starts_with("sha256:") && key.len() == 71, "{key}");
    assert_eq!(workspace.key(), key, "the same tree gives the same key");
}

#[test]
fn every_change_to_a_deciding_file_moves_the_key() {
    let workspace = Workspace::new();
    let base = workspace.key();
    for file in DECIDING {
        let before = workspace.read(file);
        workspace.append(file, "\npub fn added() {}\n");
        assert_ne!(workspace.key(), base, "{file} decides the graph");
        workspace.write(file, &before);
    }
    assert_eq!(workspace.key(), base);
}

#[test]
fn comments_and_formatting_leave_the_key_alone_and_string_literals_do_not() {
    let workspace = Workspace::new();
    let base = workspace.key();
    let file = "crates/app/src/link.rs";
    let before = workspace.read(file);

    workspace.append(file, "\n// a note\n/* a block\n   note */\n");
    assert_eq!(workspace.key(), base, "a comment");
    workspace.write(file, &before);

    workspace.replace(
        file,
        "/// Binds a call to the declaration it reaches.",
        "/// Binds a call to the one declaration it reaches, or to none.",
    );
    assert_eq!(workspace.key(), base, "a doc comment");
    workspace.write(file, &before);

    workspace.write(
        file,
        &before.replace(
            "pub fn link(name: &str) -> usize {\n    name.len()\n}",
            "pub fn link( name : &str )->usize{name.len()}",
        ),
    );
    assert_eq!(workspace.key(), base, "a formatting pass");
    workspace.write(file, &before);

    workspace.write(
        "crates/app/src/parse.rs",
        &workspace
            .read("crates/app/src/parse.rs")
            .replace("' '", "'\\t'"),
    );
    assert_ne!(workspace.key(), base, "a character literal is code");
}

const EXCLUDE_DESCRIBE: &str = r#"
[[excluded_items]]
file = "src/link.rs"
reason = "the text a reader quotes for a call; nothing at ingest calls it"
items = ["fn describe", "impl Hook for Linker / fn name_syntax", "trait Hook / fn name_syntax"]
"#;

/// A workspace whose link.rs holds a read-side function and hook beside the
/// linker, both left out of the key.
fn with_read_side_items() -> Workspace {
    let workspace = Workspace::new();
    workspace.append(
        "crates/app/src/link.rs",
        "\npub fn describe(name: &str) -> String {\n    format!(\"calls {name}\")\n}\n\n\
         pub trait Hook {\n    fn parse_order(&self) -> u8;\n    fn name_syntax(&self) -> &'static str {\n        \"\"\n    }\n}\n\n\
         pub struct Linker;\n\n\
         impl Hook for Linker {\n    fn parse_order(&self) -> u8 {\n        1\n    }\n    fn name_syntax(&self) -> &'static str {\n        \"a.b\"\n    }\n}\n",
    );
    let path = format!("crates/app/{CLASSIFICATION_FILE}");
    let classification = workspace.read(&path).replace(
        "[dependencies]",
        &format!("{}\n[dependencies]", EXCLUDE_DESCRIBE.trim()),
    );
    workspace.write(&path, &classification);
    workspace
}

#[test]
fn an_excluded_item_leaves_the_key_alone_and_the_rest_of_its_file_still_moves_it() {
    let workspace = with_read_side_items();
    let base = workspace.key();

    workspace.replace("crates/app/src/link.rs", "calls {name}", "a call to {name}");
    assert_eq!(workspace.key(), base, "an excluded function");
    workspace.replace("crates/app/src/link.rs", "\"a.b\"", "\"a::b\"");
    assert_eq!(workspace.key(), base, "an excluded impl member");

    workspace.replace("crates/app/src/link.rs", "        1\n", "        2\n");
    assert_ne!(workspace.key(), base, "a kept member of the same impl");

    let workspace = with_read_side_items();
    workspace.append(
        "crates/app/src/link.rs",
        "\npub fn resolve_alias(name: &str) -> &str {\n    name\n}\n",
    );
    assert_ne!(
        workspace.key(),
        base,
        "a new function decides until it is named"
    );
}

#[test]
fn an_excluded_item_that_deciding_code_names_fails() {
    let workspace = with_read_side_items();
    workspace.replace(
        "crates/app/src/parse.rs",
        "text.split(' ').collect()",
        "{ let _ = crate::link::describe(text); text.split(' ').collect() }",
    );
    let problems = workspace.problems();
    assert!(
        mentions(&problems, "leaves `fn describe` out of the `graph` key")
            && mentions(&problems, "names `describe` (crates/app/src/parse.rs)"),
        "{problems:#?}"
    );

    // A method call names it as surely as a path does, in another crate too.
    let workspace = with_read_side_items();
    workspace.append("crates/core/src/ids.rs", "\npub fn syntax_of(hook: &dyn std::any::Any) { let _ = hook; let name_syntax = 1; let _ = name_syntax; }\n");
    let problems = workspace.problems();
    assert!(
        mentions(&problems, "names `name_syntax` (crates/core/src/ids.rs)"),
        "{problems:#?}"
    );
}

#[test]
fn an_excluded_item_that_is_gone_or_in_an_excluded_file_fails() {
    let workspace = with_read_side_items();
    workspace.replace(
        "crates/app/src/link.rs",
        "pub fn describe(",
        "pub fn describe_call(",
    );
    let problems = workspace.problems();
    assert!(
        mentions(
            &problems,
            "crates/app/src/link.rs holds no item `fn describe`"
        ) && mentions(&problems, "fn describe_call"),
        "{problems:#?}"
    );

    let workspace = Workspace::new();
    let path = format!("crates/app/{CLASSIFICATION_FILE}");
    workspace.append(
        &path,
        "\n[[excluded_items]]\nfile = \"src/pace.rs\"\nreason = \"x\"\nitems = [\"const WAIT_MS\"]\n",
    );
    // Appended after [dependencies], an array of tables still starts its own.
    assert!(mentions(
        &workspace.problems(),
        "excludes items of src/pace.rs, which no `[[decides]]` group lists"
    ));
}

#[test]
fn excluded_files_tests_and_excluded_dependencies_leave_the_key_alone() {
    let workspace = Workspace::new();
    let base = workspace.key();

    workspace.replace("crates/app/src/pace.rs", "30", "45");
    assert_eq!(workspace.key(), base, "an excluded file");

    workspace.append("crates/core/src/tests.rs", "\n#[test]\nfn more() {}\n");
    assert_eq!(workspace.key(), base, "an excluded test file");

    workspace.append(
        "crates/app/src/link.rs",
        "\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn links() { assert_eq!(super::link(\"}\"), 1); }\n}\n",
    );
    assert_eq!(workspace.key(), base, "a test module in a deciding file");

    let lock = workspace.read("Cargo.lock").replace(
        "name = \"serde\"\nversion = \"1.0.0\"",
        "name = \"serde\"\nversion = \"1.0.1\"",
    );
    workspace.write(
        "Cargo.lock",
        &lock.replace(&"2".repeat(64), &"3".repeat(64)),
    );
    assert_eq!(workspace.key(), base, "a release of an excluded dependency");

    workspace.write("crates/app/src/.DS_Store", "litter");
    assert_eq!(workspace.key(), base, "desktop litter under a root");
}

#[test]
fn a_release_of_a_deciding_dependency_moves_the_key() {
    let workspace = Workspace::new();
    let base = workspace.key();
    let lock = workspace.read("Cargo.lock").replace(
        "name = \"grammar\"\nversion = \"1.0.0\"",
        "name = \"grammar\"\nversion = \"1.0.1\"",
    );
    workspace.write(
        "Cargo.lock",
        &lock.replace(&"1".repeat(64), &"4".repeat(64)),
    );
    assert_ne!(workspace.key(), base);
}

/// Two changes made from one base, each to code that decides the graph, are
/// each told apart from the base, and their merge is told apart from both,
/// with no number chosen and nothing beside the code to collide on. A store
/// either change derived alone therefore reads as another build's under the
/// merge.
#[test]
fn two_changes_made_side_by_side_merge_into_a_key_neither_chose() {
    let edit_a: fn(&Workspace) = |workspace| {
        workspace.replace(
            "crates/app/src/parse.rs",
            "text.split(' ')",
            "text.split_whitespace()",
        )
    };
    let edit_b: fn(&Workspace) = |workspace| {
        workspace.replace(
            "crates/app/src/link.rs",
            "name.len()",
            "name.chars().count()",
        )
    };
    // The same, inside one file: two branches change different items of it.
    let edit_c: fn(&Workspace) = |workspace| {
        workspace.append(
            "crates/app/src/link.rs",
            "\npub fn receiver(name: &str) -> &str {\n    name\n}\n",
        )
    };

    for (first, second) in [(edit_a, edit_b), (edit_b, edit_c)] {
        let base = Workspace::new();
        let one = Workspace::new();
        first(&one);
        let other = Workspace::new();
        second(&other);
        let merged = Workspace::new();
        first(&merged);
        second(&merged);

        let keys = [base.key(), one.key(), other.key(), merged.key()];
        for (index, key) in keys.iter().enumerate() {
            for later in &keys[index + 1..] {
                assert_ne!(key, later, "{keys:#?}");
            }
        }
        for workspace in [&one, &other, &merged] {
            assert_eq!(
                workspace.classifications(),
                base.classifications(),
                "a change to classified code changes no classification"
            );
        }
    }
}

#[test]
fn an_unclassified_file_fails_with_the_remedy() {
    let workspace = Workspace::new();
    workspace.write("crates/app/src/resolve.rs", "pub fn resolve() {}\n");
    let problems = workspace.problems();
    assert!(
        mentions(&problems, "crates/app/src/resolve.rs is in no group")
            && mentions(&problems, "[[decides]]"),
        "{problems:#?}"
    );
}

#[test]
fn a_listed_file_that_is_gone_or_outside_the_roots_fails() {
    let workspace = Workspace::new();
    fs::remove_file(workspace.path("crates/app/src/pace.rs")).unwrap();
    assert!(mentions(
        &workspace.problems(),
        "lists src/pace.rs, which does not exist"
    ));

    let workspace = Workspace::new();
    workspace.replace(
        &format!("crates/app/{CLASSIFICATION_FILE}"),
        "\"src/pace.rs\"",
        "\"src/pace.rs\", \"build.rs\"",
    );
    assert!(mentions(
        &workspace.problems(),
        "lists build.rs, which is under none of its roots"
    ));
}

#[test]
fn a_file_in_two_groups_fails() {
    let workspace = Workspace::new();
    workspace.replace(
        &format!("crates/app/{CLASSIFICATION_FILE}"),
        "\"src/parse.rs\"]",
        "\"src/parse.rs\", \"src/pace.rs\"]",
    );
    assert!(mentions(
        &workspace.problems(),
        "lists src/pace.rs more than once"
    ));
}

#[test]
fn an_exclusion_needs_a_reason_and_a_product_must_exist() {
    let workspace = Workspace::new();
    workspace.replace(
        &format!("crates/app/{CLASSIFICATION_FILE}"),
        "reason = \"how long a question may wait; it decides no record\"",
        "reason = \"  \"",
    );
    assert!(mentions(&workspace.problems(), "needs a `reason`"));

    let workspace = Workspace::new();
    workspace.replace(
        &format!("crates/app/{CLASSIFICATION_FILE}"),
        "products = [\"graph\"]",
        "products = [\"grpah\"]",
    );
    assert!(mentions(
        &workspace.problems(),
        "names `grpah`, which is not a product"
    ));

    let workspace = Workspace::new();
    assert!(compose(&workspace.app(), &["vectors"])
        .unwrap_err()
        .iter()
        .any(|problem| problem.contains("`vectors` is not a product")));
}

#[test]
fn unknown_keys_fail_rather_than_being_ignored() {
    let workspace = Workspace::new();
    let path = format!("crates/app/{CLASSIFICATION_FILE}");
    workspace.write(
        &path,
        &format!("language = \"go\"\n{}", workspace.read(&path)),
    );
    assert!(mentions(&workspace.problems(), "has `language`"));

    let workspace = Workspace::new();
    workspace.replace(
        &format!("crates/app/{CLASSIFICATION_FILE}"),
        "reason = \"how long",
        "why = \"x\"\nreason = \"how long",
    );
    assert!(mentions(&workspace.problems(), "has `why`"));
}

#[test]
fn every_dependency_is_classified_and_every_classified_one_exists() {
    let workspace = Workspace::new();
    workspace.append(
        "crates/app/Cargo.toml",
        "\n[target.'cfg(unix)'.dependencies]\nlibc = \"0.2\"\n",
    );
    let problems = workspace.problems();
    assert!(
        mentions(
            &problems,
            "depends on `libc`, which crates/app/derivation-inputs.toml does not classify"
        ),
        "{problems:#?}"
    );

    let workspace = Workspace::new();
    workspace.append(
        &format!("crates/app/{CLASSIFICATION_FILE}"),
        "ghost = { reason = \"gone\" }\n",
    );
    assert!(mentions(
        &workspace.problems(),
        "classifies dependency `ghost`"
    ));

    let workspace = Workspace::new();
    workspace.replace(
        &format!("crates/app/{CLASSIFICATION_FILE}"),
        "serde = { reason = \"serialization; each record carries its own schema\" }",
        "serde = { decides = [\"graph\"], reason = \"both\" }",
    );
    assert!(mentions(
        &workspace.problems(),
        "exactly one of `decides` or `reason`"
    ));
}

#[test]
fn a_deciding_workspace_dependency_must_carry_its_own_classification() {
    let workspace = Workspace::new();
    fs::remove_file(workspace.path(&format!("crates/core/{CLASSIFICATION_FILE}"))).unwrap();
    assert!(mentions(
        &workspace.problems(),
        "crates/core has no derivation-inputs.toml"
    ));

    // One excluded with a reason is never read.
    workspace.replace(
        &format!("crates/app/{CLASSIFICATION_FILE}"),
        "core = { decides = [\"graph\"] }",
        "core = { reason = \"stores bytes by content; derives nothing\" }",
    );
    let composition = compose(&workspace.app(), &["graph"]).unwrap();
    assert!(composition
        .inputs("graph")
        .iter()
        .all(|input| !input.name.starts_with("crates/core/")));
}

#[test]
fn a_new_file_in_a_reached_crate_also_forces_the_decision() {
    let workspace = Workspace::new();
    workspace.write("crates/core/src/kinds.rs", "pub enum Kind { Function }\n");
    assert!(mentions(
        &workspace.problems(),
        "crates/core/src/kinds.rs is in no group"
    ));
}

#[test]
fn the_generated_source_and_watched_paths_are_what_a_build_needs() {
    let workspace = Workspace::new();
    let composition = compose(&workspace.app(), &["graph"]).unwrap();
    let source = composition.generated_source();
    let key = composition.key("graph").unwrap();
    assert!(
        source.contains(&format!("pub const GRAPH: &str = \"{key}\";")),
        "{source}"
    );
    assert!(source.contains("pub const GRAPH_INPUTS: &[(&str, &str)] = &["));
    assert!(source.contains("(\"crates/core/src/ids.rs\", \"sha256:"));

    let watched: Vec<&Path> = composition.watched().collect();
    for path in &watched {
        assert!(
            path.exists(),
            "{} is watched and does not exist",
            path.display()
        );
    }
    for expected in [
        "Cargo.lock",
        "Cargo.toml",
        "crates/app/Cargo.toml",
        "crates/app/derivation-inputs.toml",
        "crates/app/src/link.rs",
        "crates/core/Cargo.toml",
        "crates/core/derivation-inputs.toml",
        "crates/core/src/ids.rs",
    ] {
        assert!(
            watched.contains(&workspace.path(expected).as_path()),
            "{expected} is not watched: {watched:#?}"
        );
    }
    // Neither a directory nor a file the keys never read: an edit to a test
    // or an excluded file reruns nothing downstream of the composer.
    for unwatched in [
        "crates/app/src",
        "crates/app/src/pace.rs",
        "crates/core/src",
        "crates/core/src/tests.rs",
    ] {
        assert!(
            !watched.contains(&workspace.path(unwatched).as_path()),
            "{unwatched} is watched: {watched:#?}"
        );
    }
}

#[test]
fn a_root_may_be_one_file_beside_the_sources() {
    let workspace = Workspace::new();
    workspace.write("crates/app/build.rs", "fn main() {}\n");
    workspace.replace(
        &format!("crates/app/{CLASSIFICATION_FILE}"),
        "roots = [\"src\"]",
        "roots = [\"src\", \"build.rs\"]",
    );
    assert!(mentions(
        &workspace.problems(),
        "crates/app/build.rs is in no group"
    ));
    workspace.replace(
        &format!("crates/app/{CLASSIFICATION_FILE}"),
        "\"src/parse.rs\"]",
        "\"src/parse.rs\", \"build.rs\"]",
    );
    let base = workspace.key();
    workspace.write(
        "crates/app/build.rs",
        "fn main() { println!(\"cargo:rustc-cfg=fast\"); }\n",
    );
    assert_ne!(workspace.key(), base);
}

/// A trait method can run without its name being written (a sort calls
/// `cmp`, a format calls `fmt`, a serializer calls `serialize`), so the name
/// check alone cannot clear a member of a trait implementation.
#[test]
fn a_trait_implementation_member_is_left_out_only_with_its_own_trait_member() {
    let path = format!("crates/app/{CLASSIFICATION_FILE}");

    let workspace = with_read_side_items();
    workspace.replace(&path, ", \"trait Hook / fn name_syntax\"", "");
    let problems = workspace.problems();
    assert!(
        mentions(&problems, "but not `trait Hook / fn name_syntax`"),
        "{problems:#?}"
    );

    let workspace = with_read_side_items();
    workspace.append(
        "crates/app/src/link.rs",
        "\nimpl std::fmt::Display for Linker {\n    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {\n        f.write_str(\"linker\")\n    }\n}\n",
    );
    workspace.replace(
        &path,
        "\"fn describe\",",
        "\"fn describe\", \"impl std::fmt::Display for Linker / fn fmt\",",
    );
    let problems = workspace.problems();
    assert!(
        mentions(
            &problems,
            "an implementation of `Display`, which no code that decides `graph` declares"
        ),
        "{problems:#?}"
    );

    let workspace = with_read_side_items();
    workspace.replace(
        &path,
        "\"impl Hook for Linker / fn name_syntax\"",
        "\"impl Hook for Linker\"",
    );
    let problems = workspace.problems();
    assert!(
        mentions(&problems, "leave out its members one by one instead"),
        "{problems:#?}"
    );
}
