// A Rust crate root follows admitted manifest metadata. Unparsed manifests
// cannot be read or patched through the semantic MCP surface.

/// The name the module of `file` carries in the graph a later commit plans
/// from, and in the daemon's live graph.
fn crate_names_module(state: &Arc<DaemonState>, file: &str) -> (String, String) {
    let surface = |graph: &kin_db::InMemoryGraph| {
        graph
            .query_entities(&kin_model::EntityFilter {
                file_path: Some(FilePathId::new(file)),
                ..Default::default()
            })
            .unwrap()
            .into_iter()
            .find(kin_model::is_file_module_surface)
            .map(|entity| entity.name)
            .unwrap_or_default()
    };
    let base = crate::repository_commit::load_native_commit_base(
        &crate::local_repository_authority::LocalRepositoryAuthorityContext::from_state(state)
            .unwrap(),
    )
    .unwrap();
    (surface(&base.graph), surface(state.graph.as_ref()))
}

/// Brownfield admission of an externally renamed manifest updates the crate root. The
/// semantic API refuses the unparsed manifest, while later parsed edits work.
#[tokio::test]
#[serial_test::serial]
async fn admitted_package_rename_preserves_later_semantic_edits_without_manifest_source() {
    let (_dir, state) = mcp_lifecycle_fixture();
    for (path, body) in [
        (
            "Cargo.toml",
            "[package]\nname = \"first-name\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        ),
        ("src/lib.rs", "pub mod store;\npub use store::open;\n"),
        ("src/store.rs", "pub fn open() -> u8 {\n    1\n}\n"),
    ] {
        source_tree_conversion_fixture(
            &state,
            serde_json::json!({
                "verb": "create", "target": path, "body": body,
                "description": "crate fixture"
            }),
        )
        .await;
    }
    let first = "first_name".to_string();
    assert_eq!(
        crate_names_module(&state, "src/lib.rs"),
        (first.clone(), first)
    );

    let session = mcp_test_session(&state);
    artifact_assert_source_refused(&state, "first-name :: Cargo.toml").await;
    // External fixture mutation enters through real brownfield admission,
    // never a semantic source or unguarded whole-entity replacement.
    std::fs::write(
        state.layout.working_dir().join("Cargo.toml"),
        "[package]\nname = \"second-name\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    waiting_admit(&state, "an externally renamed package").await;
    waiting_commit(&state, "an externally renamed package").await;
    let second = "second_name".to_string();
    assert_eq!(
        crate_names_module(&state, "src/lib.rs"),
        (second.clone(), second.clone())
    );

    let open = state
        .graph
        .query_entities(&kin_model::EntityFilter {
            file_path: Some(FilePathId::new("src/store.rs")),
            ..Default::default()
        })
        .unwrap()
        .into_iter()
        .find(|entity| entity.name == "open")
        .expect("the fixture holds open");
    let read = edit_path_read(&state, &serde_json::json!(open.id)).await;
    let (result, text) = edit_path_mutate(
        &state,
        &session,
        vec![edit_path_update(&read, "pub fn open() -> u8 {\n    2\n}")],
    )
    .await;
    assert_ne!(result.is_error, Some(true), "{text}");
    assert_eq!(
        crate_names_module(&state, "src/lib.rs"),
        (second.clone(), second)
    );
}

/// A Cargo manifest held as a link, as Bazel and cargo-raze lay them out, is
/// followed to its target inside the tree the way Cargo follows it, whatever
/// the target is named: `kin init` from Git names the crate's root by the
/// target's package, a live admission batch of a source beneath it admits and
/// commits, and so does one after `kin upgrade` re-derives the store. Renaming
/// the package in the target renames the root of every crate linked to it,
/// and a later edit beneath such a crate commits. A link whose target leaves
/// the tree names nothing, so its crate's root is named from its path and
/// nothing is refused for it.
#[cfg(unix)]
#[tokio::test]
#[serial_test::serial]
async fn a_symlinked_manifest_names_its_crate_through_init_upgrade_and_live_batches() {
    install_test_registry_override();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    run_test_git(root, ["init", "--initial-branch=main"]);
    run_test_git(root, ["config", "user.email", "crates@example.invalid"]);
    run_test_git(root, ["config", "user.name", "Crate Names Test"]);
    for directory in [
        "shared",
        "crates/app/src",
        "crates/tool/src",
        "crates/out/src",
    ] {
        std::fs::create_dir_all(root.join(directory)).unwrap();
    }
    let package = |name: &str| {
        format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n")
    };
    std::fs::write(root.join("shared/Cargo.toml"), package("shared-crate")).unwrap();
    std::fs::write(root.join("shared/tool.toml"), package("shared-tool")).unwrap();
    for (link, target) in [
        ("crates/app/Cargo.toml", "../../shared/Cargo.toml"),
        ("crates/tool/Cargo.toml", "../../shared/tool.toml"),
        ("crates/out/Cargo.toml", "../../../outside/Cargo.toml"),
    ] {
        std::os::unix::fs::symlink(target, root.join(link)).unwrap();
    }
    std::fs::write(
        root.join("crates/app/src/lib.rs"),
        "pub mod store;\npub use store::open;\n",
    )
    .unwrap();
    std::fs::write(
        root.join("crates/app/src/store.rs"),
        "pub fn open() -> u8 {\n    1\n}\n",
    )
    .unwrap();
    std::fs::write(root.join("crates/tool/src/lib.rs"), "pub fn tool() {}\n").unwrap();
    std::fs::write(root.join("crates/out/src/lib.rs"), "pub fn out() {}\n").unwrap();
    run_test_git(root, ["add", "--all"]);
    run_test_git(root, ["commit", "-m", "linked manifests"]);

    let named = |app: &str, tool: &str| {
        [
            ("crates/app/src/lib.rs", app.to_string()),
            ("crates/tool/src/lib.rs", tool.to_string()),
            ("crates/out/src/lib.rs", "out".to_string()),
        ]
    };
    let assert_named = |state: &Arc<DaemonState>, expected: [(&str, String); 3], when: &str| {
        for (file, name) in expected {
            assert_eq!(
                crate_names_module(state, file),
                (name.clone(), name),
                "{file} {when}"
            );
        }
    };
    // A live batch editing a source beneath each crate, admitted and committed.
    async fn edit_beneath_each_crate(state: &Arc<DaemonState>, value: u8, when: &str) {
        let working = state.layout.working_dir();
        for (path, body) in [
            (
                "crates/app/src/store.rs",
                format!("pub fn open() -> u8 {{\n    {value}\n}}\n"),
            ),
            (
                "crates/tool/src/lib.rs",
                format!("pub fn tool() -> u8 {{\n    {value}\n}}\n"),
            ),
            (
                "crates/out/src/lib.rs",
                format!("pub fn out() -> u8 {{\n    {value}\n}}\n"),
            ),
        ] {
            std::fs::write(working.join(path), body).unwrap();
        }
        waiting_admit(state, when).await;
        waiting_commit(state, when).await;
    }

    let layout = kin_core::init_from_git(root).unwrap().layout;
    let (dir, state) = mcp_lifecycle_open(dir, layout);
    assert_named(&state, named("shared_crate", "shared_tool"), "after init");
    edit_beneath_each_crate(&state, 2, "edits beneath linked manifests").await;
    assert_named(
        &state,
        named("shared_crate", "shared_tool"),
        "after a live batch",
    );

    // `kin upgrade` re-derives the store from repository CAS, staging each
    // link and the body it leads to, and the next live batch commits.
    let layout = state.layout.clone();
    drop(state);
    std::fs::remove_file(layout.kindb_hydration_semantics_path()).unwrap();
    let upgraded = kin_cli::commands::upgrade::upgrade_store(
        &layout,
        kin_model::AuthorId::new("crate-names-upgrade"),
        &kin_cli::commands::upgrade::UpgradeHooks::default(),
        &|_| {},
    )
    .unwrap();
    assert_ne!(
        upgraded.state,
        kin_cli::commands::upgrade::UpgradeState::AlreadyCurrent,
        "the upgrade re-derives the store"
    );
    let (_dir, state) = mcp_lifecycle_open(dir, layout);
    assert_named(
        &state,
        named("shared_crate", "shared_tool"),
        "after upgrade",
    );
    edit_beneath_each_crate(&state, 3, "edits after an upgrade").await;
    assert_named(
        &state,
        named("shared_crate", "shared_tool"),
        "after an upgrade and a live batch",
    );

    // Renaming the package in a link's target renames the linked crate's
    // root, whose own bytes and link did not change: one batch for a target
    // named otherwise than a Cargo manifest, and one for a target that is one.
    let working = state.layout.working_dir();
    std::fs::write(working.join("shared/tool.toml"), package("renamed-tool")).unwrap();
    waiting_admit(&state, "a renamed target named otherwise").await;
    waiting_commit(&state, "a renamed target named otherwise").await;
    assert_named(
        &state,
        named("shared_crate", "renamed_tool"),
        "after the rename of a target named otherwise",
    );
    std::fs::write(working.join("shared/Cargo.toml"), package("renamed-crate")).unwrap();
    waiting_admit(&state, "a renamed Cargo manifest target").await;
    waiting_commit(&state, "a renamed Cargo manifest target").await;
    assert_named(
        &state,
        named("renamed_crate", "renamed_tool"),
        "after the rename of a Cargo manifest target",
    );
    edit_beneath_each_crate(&state, 4, "edits after the targets' rename").await;
    assert_named(
        &state,
        named("renamed_crate", "renamed_tool"),
        "after a later edit",
    );
}

/// A linked manifest remains unavailable as semantic source. Explicit
/// brownfield admission of its target still updates dependent crate names,
/// and a later parsed-entity edit commits.
#[cfg(unix)]
#[tokio::test]
#[serial_test::serial]
async fn admitted_link_target_rename_preserves_semantic_edits_without_manifest_source() {
    install_test_registry_override();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    run_test_git(root, ["init", "--initial-branch=main"]);
    run_test_git(root, ["config", "user.email", "crates@example.invalid"]);
    run_test_git(root, ["config", "user.name", "Crate Names Test"]);
    for directory in ["shared", "crates/tool/src"] {
        std::fs::create_dir_all(root.join(directory)).unwrap();
    }
    std::fs::write(
        root.join("shared/tool.toml"),
        "[package]\nname = \"shared-tool\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::os::unix::fs::symlink(
        "../../shared/tool.toml",
        root.join("crates/tool/Cargo.toml"),
    )
    .unwrap();
    std::fs::write(root.join("crates/tool/src/lib.rs"), "pub fn tool() {}\n").unwrap();
    run_test_git(root, ["add", "--all"]);
    run_test_git(root, ["commit", "-m", "linked manifest"]);
    let layout = kin_core::init_from_git(root).unwrap().layout;
    let (_dir, state) = mcp_lifecycle_open(dir, layout);
    let shared = "shared_tool".to_string();
    assert_eq!(
        crate_names_module(&state, "crates/tool/src/lib.rs"),
        (shared.clone(), shared)
    );

    // Discovery exposes metadata, never a whole manifest or edit ticket.
    let (result, text) =
        artifact_call(&state, "locate", serde_json::json!({"query": "tool.toml"})).await;
    assert_ne!(result.is_error, Some(true), "{text}");
    let rows = artifact_rows(&text, "results");
    assert!(!rows.is_empty(), "{text}");
    for row in rows {
        artifact_assert_source_refused(&state, row.split("  ").next().unwrap()).await;
    }
    std::fs::write(
        state.layout.working_dir().join("shared/tool.toml"),
        "[package]\nname = \"written-tool\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    waiting_admit(&state, "an externally renamed linked manifest").await;
    waiting_commit(&state, "an externally renamed linked manifest").await;
    let session = mcp_test_session(&state);
    let written = "written_tool".to_string();
    assert_eq!(
        crate_names_module(&state, "crates/tool/src/lib.rs"),
        (written.clone(), written.clone())
    );

    let tool = state
        .graph
        .query_entities(&kin_model::EntityFilter {
            file_path: Some(FilePathId::new("crates/tool/src/lib.rs")),
            ..Default::default()
        })
        .unwrap()
        .into_iter()
        .find(|entity| entity.name == "tool")
        .expect("the tool crate holds tool");
    let read = edit_path_read(&state, &serde_json::json!(tool.id)).await;
    let (result, text) = edit_path_mutate(
        &state,
        &session,
        vec![edit_path_update(&read, "pub fn tool() -> u8 {\n    5\n}")],
    )
    .await;
    assert_ne!(result.is_error, Some(true), "{text}");
    assert_eq!(
        crate_names_module(&state, "crates/tool/src/lib.rs"),
        (written.clone(), written)
    );
}
