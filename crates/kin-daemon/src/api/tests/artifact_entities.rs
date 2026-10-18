// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

// A tracked thing Kin does not parse, as an agent finds, reads and changes
// it: `locate`, `source` and `mutate` over the served route, each reply
// presented the way the default agent profile reads it, as compact text.
//
// Every case starts from the records the non-entity coverage pass derives,
// and only a build with embedding support carries that pass, so the cases and
// their fixtures share its gate. The call and row helpers also serve a
// Unix-only crate-name case, so they follow the union of those consumers.
// The read helper serves every build.

#[cfg(any(feature = "embeddings", feature = "external-embeddings"))]
const ARTIFACT_LICENSE: &str = "MIT License\n\nCopyright (c) 2026 Example Authors\n\nPermission is hereby granted, free of charge, to any person obtaining a copy\nof this software and associated documentation files (the \"Software\"), to deal\nin the Software without restriction.\n";

#[cfg(any(feature = "embeddings", feature = "external-embeddings"))]
const ARTIFACT_MANIFEST: &str = "[project]\nname = \"Shop\"\nversion = \"0.1.0\"\ndescription = \"A tiny storefront\"\n\n[tool.setuptools.package-data]\nshop = [\"schema.sql\", \"static/*.png\"]\n";

#[cfg(any(feature = "embeddings", feature = "external-embeddings"))]
const ARTIFACT_SCHEMA: &str = "CREATE TABLE item (\n    id INTEGER PRIMARY KEY,\n    name TEXT NOT NULL,\n    price_cents INTEGER NOT NULL\n);\n";

#[cfg(any(feature = "embeddings", feature = "external-embeddings"))]
const ARTIFACT_README: &str = "# Shop\n\nA tiny storefront.\n\n## Install\n\nRun `pip install .` and load `schema.sql` into the database.\n\n## Licence\n\nSee [LICENSE](LICENSE).\n";

#[cfg(any(feature = "embeddings", feature = "external-embeddings"))]
const ARTIFACT_DB: &str = "\"\"\"Database setup for the shop.\"\"\"\nimport sqlite3\nfrom importlib import resources\n\n\ndef schema_version():\n    \"\"\"The version of the bundled schema.\"\"\"\n    return 1\n\n\ndef init_db(path):\n    \"\"\"Create the tables from the bundled schema.\"\"\"\n    connection = sqlite3.connect(path)\n    with resources.files(\"shop\").joinpath(\"schema.sql\").open() as handle:\n        connection.executescript(handle.read())\n    return connection\n";

#[cfg(any(feature = "embeddings", feature = "external-embeddings"))]
const ARTIFACT_TEMPLATE: &str = "<!doctype html>\n<title>Shop</title>\n<img src=\"{{ url_for('static', filename='logo.png') }}\" alt=\"Shop logo\">\n";

#[cfg(any(feature = "embeddings", feature = "external-embeddings"))]
const ARTIFACT_GUIDE: &str = "# Guide\n\nThe shop shows ![its logo](static/logo.png) on every page.\n\nThe old mark lived at `assets/logo.png` and is gone.\n";

/// A changelog held in sections: two versions under one title, the older
/// one long enough that the whole is more than a class is served whole.
#[cfg(any(feature = "embeddings", feature = "external-embeddings"))]
fn artifact_history() -> String {
    let mut text = String::from(
        "History\n=======\n\nVersion 1.1.0\n-------------\n\nUnreleased\n\n- Read the \
         schema version with :func:`schema_version` before ``init_db`` runs.\n\n\
         Version 1.0.0\n-------------\n\nReleased 2026-01-01\n\n",
    );
    for entry in 0..120 {
        text.push_str(&format!(
            "- Entry {entry:03}: one line of the first release.\n"
        ));
    }
    text
}

/// A changelog longer than one reply carries whole: 400 numbered entries.
#[cfg(any(feature = "embeddings", feature = "external-embeddings"))]
fn artifact_changelog() -> String {
    let mut text = String::from("Changes\n=======\n\nUnreleased\n----------\n\n");
    for entry in 0..400 {
        text.push_str(&format!(
            "- Entry {entry:03}: a change that is described in one line of the changelog.\n"
        ));
    }
    text
}

/// A store holding a small Python package and the things beside it that Kin
/// does not parse: a licence, two readmes, a manifest, a schema and a long
/// changelog.
#[cfg(any(feature = "embeddings", feature = "external-embeddings"))]
async fn artifact_fixture() -> (tempfile::TempDir, Arc<DaemonState>) {
    let (dir, state) = mcp_lifecycle_fixture();
    let changelog = artifact_changelog();
    let history = artifact_history();
    for (path, body) in [
        ("pyproject.toml", ARTIFACT_MANIFEST),
        ("HISTORY.rst", history.as_str()),
        ("LICENSE", ARTIFACT_LICENSE),
        ("README.md", ARTIFACT_README),
        ("CHANGES.txt", changelog.as_str()),
        ("src/shop/__init__.py", "\"\"\"The shop package.\"\"\"\n"),
        ("src/shop/db.py", ARTIFACT_DB),
        ("src/shop/schema.sql", ARTIFACT_SCHEMA),
        (
            "docs/conf.py",
            "\"\"\"Documentation settings.\"\"\"\nproject = \"shop\"\n",
        ),
        ("docs/README.md", "# Documentation\n"),
    ] {
        source_tree_conversion_fixture(
            &state,
            serde_json::json!({
                "verb": "create", "target": path, "body": body,
                "description": "artifact fixture"
            }),
        )
        .await;
    }
    // What a daemon derives when it opens a store: the retrieval record of
    // each thing no parser reads, which a ranking finds an artifact by.
    crate::loop_runner::ensure_non_entity_enrichment_coverage(&state).unwrap();
    (dir, state)
}

/// What an agent on the default profile reads for one call of `verb`: the
/// verb resolved to the tool that answers it, the call made over the served
/// route, and the answer paged, enveloped and written as text the way the MCP
/// server presents a daemon's answer to its client.
#[cfg(any(unix, feature = "embeddings", feature = "external-embeddings"))]
async fn artifact_call(
    state: &Arc<DaemonState>,
    verb: &str,
    arguments: serde_json::Value,
) -> (kin_mcp::ToolCallResult, String) {
    let mut tool = verb.to_string();
    let mut resolved: HashMap<String, serde_json::Value> =
        serde_json::from_value(arguments.clone()).unwrap();
    kin_mcp::verbs::resolve(&mut tool, &mut resolved).unwrap();
    let mut result = edit_path_call(state, &tool, serde_json::json!(resolved)).await;
    kin_mcp::source_pages::present(&tool, &resolved, &mut result);
    let result = kin_mcp::envelope::finalize(result, kin_mcp::Envelope::daemon(), &tool);
    let text = edit_path_agent_text(verb, &arguments, &result);
    (result, text)
}

/// The lines of a text reply under the heading `heading`, which is written
/// `heading (count):`, each without its indentation.
#[cfg(any(unix, feature = "embeddings", feature = "external-embeddings"))]
fn artifact_rows(text: &str, heading: &str) -> Vec<String> {
    text.lines()
        .skip_while(|line| !line.starts_with(&format!("{heading} (")))
        .skip(1)
        .take_while(|line| line.starts_with("  "))
        .map(|line| line.trim().to_string())
        .collect()
}

/// The machine `source` reply for `address`, as the route answers it.
async fn artifact_read(state: &Arc<DaemonState>, address: &str) -> serde_json::Value {
    let result = edit_path_call(
        state,
        "get_entity_source",
        serde_json::json!({"entity_id": address}),
    )
    .await;
    assert_ne!(result.is_error, Some(true), "{}", mcp_result_text(&result));
    tool_result_payload(&result)
}

async fn artifact_assert_source_refused(state: &Arc<DaemonState>, address: &str) {
    let roots = source_base_roots(state);
    let (result, text) =
        artifact_call(state, "source", serde_json::json!({"entity_id": address})).await;
    assert_eq!(result.is_error, Some(true), "{text}");
    assert!(
        text.contains("unparsed_source_requires_semantic_coverage"),
        "{text}"
    );
    let delivered = serde_json::to_string(&result).unwrap();
    assert!(!delivered.contains("source_base"), "{delivered}");
    assert!(!delivered.contains("\"body\":"), "{delivered}");
    assert!(!delivered.contains(ARTIFACT_LICENSE), "{delivered}");
    assert_eq!(source_base_roots(state), roots);
}

/// A tracked thing no parser reads is found by its name, exactly, without
/// its extension, and beside a declaration that shares the word, and by a
/// description. Its row names it the way a row names code, with its kind and
/// the level Kin holds it at, and says nothing of where it is projected. No
/// reply says a name Kin holds is absent.
#[cfg(any(feature = "embeddings", feature = "external-embeddings"))]
#[tokio::test]
#[serial_test::serial]
async fn artifact_text_is_found_by_name_and_by_description() {
    let (_dir, state) = artifact_fixture().await;

    let (result, text) =
        artifact_call(&state, "locate", serde_json::json!({"query": "LICENSE"})).await;
    assert_ne!(result.is_error, Some(true), "{text}");
    assert!(
        text.contains("this is the 1 unparsed entity Kin holds whose name matches LICENSE"),
        "{text}"
    );
    assert_eq!(
        artifact_rows(&text, "results"),
        ["shop :: LICENSE  (licence)  held_as=text"],
        "{text}"
    );
    assert!(!text.contains("no declaration Kin holds"), "{text}");
    assert!(
        text.contains(
            "searched: Searched recorded declarations and their semantic names, and the names \
             of the entities Kin holds without parsing them."
        ),
        "{text}"
    );

    // Without its extension, and beside the declaration that shares the word.
    let (_, text) = artifact_call(&state, "locate", serde_json::json!({"query": "schema"})).await;
    assert!(
        text.contains(
            "these are the 2 entities Kin holds whose name matches schema: 1 declaration and 1 \
             unparsed entity"
        ),
        "{text}"
    );
    assert_eq!(
        artifact_rows(&text, "results"),
        [
            "shop.db :: schema_version",
            "shop :: schema.sql  (SQL)  held_as=text"
        ],
        "{text}"
    );

    // Two artifacts of one name are told apart by where code places them.
    let (_, text) = artifact_call(&state, "locate", serde_json::json!({"query": "readme"})).await;
    assert_eq!(
        artifact_rows(&text, "results"),
        [
            "shop :: README.md  (Markdown)  held_as=parts",
            "docs :: README.md  (Markdown)  held_as=parts"
        ],
        "{text}"
    );

    // A search narrowed to a kind of code asked about code.
    let (_, text) = artifact_call(
        &state,
        "locate",
        serde_json::json!({"query": "schema", "kind": "function"}),
    )
    .await;
    assert_eq!(
        artifact_rows(&text, "results"),
        ["shop.db :: schema_version"],
        "{text}"
    );

    // A name nothing carries is still reported as one.
    let (_, text) = artifact_call(
        &state,
        "locate",
        serde_json::json!({"query": "nothing_named_this"}),
    )
    .await;
    assert!(
        text.contains("no declaration Kin holds is named nothing_named_this"),
        "{text}"
    );

    // By description: the ranking's own candidates, kept as the artifacts
    // they are.
    let (result, text) = artifact_call(
        &state,
        "locate",
        serde_json::json!({
            "query": "database schema tables",
            "mode": "description",
            "surface": "compact"
        }),
    )
    .await;
    assert_ne!(result.is_error, Some(true), "{text}");
    let rows = artifact_rows(&text, "results");
    assert!(
        rows.iter().any(|row| row.starts_with("shop.db :: init_db")),
        "{text}"
    );
    assert!(
        rows.contains(&"shop :: schema.sql  (SQL)  held_as=text".to_string()),
        "the schema is ranked, as the artifact it is: {text}"
    );
    let payload = tool_result_payload(&result);
    let ranked = payload["entities"]
        .as_array()
        .or_else(|| payload["results"].as_array())
        .unwrap_or_else(|| panic!("{payload}"))
        .iter()
        .find(|row| row["name"] == "schema.sql")
        .unwrap_or_else(|| panic!("{payload}"))
        .clone();
    // Discovery metadata does not grant a source body or edit base.
    let id = ranked["entity_id"]
        .as_str()
        .or_else(|| ranked["id"].as_str())
        .unwrap_or_else(|| panic!("a ranked row carries its id: {ranked}"));
    artifact_assert_source_refused(&state, id).await;

    // No row says where anything is projected.
    for query in ["LICENSE", "schema", "readme"] {
        let (_, text) = artifact_call(&state, "locate", serde_json::json!({"query": query})).await;
        assert!(!text.contains("src/"), "{text}");
        assert!(!text.contains("projected_at"), "{text}");
    }
}

/// Unparsed source is refused by name and UUID without issuing an edit base.
#[cfg(any(feature = "embeddings", feature = "external-embeddings"))]
#[tokio::test]
#[serial_test::serial]
async fn artifact_text_source_refuses_names_and_ids_without_an_edit_ticket() {
    let (_dir, state) = artifact_fixture().await;
    for address in [
        "LICENSE",
        "shop :: LICENSE",
        "schema.sql",
        "shop :: README.md",
    ] {
        artifact_assert_source_refused(&state, address).await;
    }
    let artifact = state
        .graph
        .resolved_tree()
        .artifact_at_path(&kin_model::RepoPath::from_utf8("LICENSE").unwrap())
        .unwrap()
        .artifact_id;
    artifact_assert_source_refused(&state, &artifact.0.to_string()).await;
}

/// Paging is not a whole-file fallback for unparsed source.
#[cfg(any(feature = "embeddings", feature = "external-embeddings"))]
#[tokio::test]
#[serial_test::serial]
async fn artifact_long_text_cannot_be_reconstructed_through_source_excerpts() {
    let (dir, state) = artifact_fixture().await;
    let changelog = artifact_changelog();
    assert!(changelog.len() > kin_mcp::source_pages::FULL_BODY_MAX_BYTES);
    for address in ["CHANGES.txt", "shop :: CHANGES.txt"] {
        artifact_assert_source_refused(&state, address).await;
    }
    assert_eq!(edit_path_disk(&dir, "CHANGES.txt"), changelog);
}

/// Code is still read as its entities. Asked for whole by its own name it is
/// refused with the calls that read it, where it is projected is still no
/// address, and its module answers as it did.
#[cfg(any(feature = "embeddings", feature = "external-embeddings"))]
#[tokio::test]
#[serial_test::serial]
async fn artifact_surface_still_refuses_a_whole_read_of_parsed_code() {
    let (_dir, state) = artifact_fixture().await;
    let (result, text) =
        artifact_call(&state, "source", serde_json::json!({"entity_id": "db.py"})).await;
    assert_eq!(result.is_error, Some(true), "{text}");
    assert_eq!(
        text,
        "message: db.py is held as code: Kin serves its functions and classes one at a time \
         and never the whole source. Its module is shop.db; call source on shop.db for what it \
         holds, or find a function or class with locate and read it with source."
    );
    assert!(!text.contains("sqlite3"), "{text}");

    for place in ["src/shop/db.py", "src/shop/schema.sql", "./LICENSE"] {
        let (result, text) =
            artifact_call(&state, "source", serde_json::json!({"entity_id": place})).await;
        assert_eq!(result.is_error, Some(true), "{place}: {text}");
        assert_eq!(
            text,
            "message: Select semantic entities by name or returned UUID. This selector is not \
             supported; nothing ran.",
            "{place}"
        );
    }

    // The module is read as its own code, never as the text of its source.
    let (result, text) = artifact_call(
        &state,
        "source",
        serde_json::json!({"entity_id": "shop.db"}),
    )
    .await;
    assert_ne!(result.is_error, Some(true), "{text}");
    assert!(
        text.contains("this is the module-level code of shop.db"),
        "{text}"
    );
    assert!(text.contains("# kin:entity function init_db #"), "{text}");
    assert!(!text.contains("connection.executescript"), "{text}");
    assert!(!text.contains("held_as"), "{text}");

    // And a name search does not list source Kin parses as an artifact.
    let (_, text) = artifact_call(&state, "locate", serde_json::json!({"query": "db.py"})).await;
    assert!(!text.contains("held_as"), "{text}");
}

/// Unparsed reads leave authority unchanged; parsed edits remain usable.
#[cfg(any(feature = "embeddings", feature = "external-embeddings"))]
#[tokio::test]
#[serial_test::serial]
async fn unparsed_text_cannot_issue_a_patch_ticket_or_change_authority() {
    let (dir, state) = artifact_fixture().await;
    artifact_assert_source_refused(&state, "shop :: LICENSE").await;
    assert_eq!(edit_path_disk(&dir, "LICENSE"), ARTIFACT_LICENSE);
    // Supported parsed-entity edits remain usable beside the refused artifact.
    let session = mcp_test_session(&state);
    let function = edit_path_read(&state, &serde_json::json!("shop.db::schema_version")).await;
    let (result, text) = edit_path_mutate(
        &state,
        &session,
        vec![edit_path_patch(&function, &[("return 1", "return 2")])],
    )
    .await;
    assert_ne!(result.is_error, Some(true), "{text}");
    assert_eq!(edit_path_disk(&dir, "LICENSE"), ARTIFACT_LICENSE);
    assert!(edit_path_disk(&dir, "src/shop/db.py").contains("return 2"));
}

/// Long unparsed artifacts yield neither excerpts nor edit bases.
#[cfg(any(feature = "embeddings", feature = "external-embeddings"))]
#[tokio::test]
#[serial_test::serial]
async fn unparsed_long_text_has_no_excerpt_edit_base() {
    let (dir, state) = artifact_fixture().await;
    let roots = source_base_roots(&state);
    artifact_assert_source_refused(&state, "shop :: CHANGES.txt").await;
    assert_eq!(edit_path_disk(&dir, "CHANGES.txt"), artifact_changelog());
    assert_eq!(source_base_roots(&state), roots);
}

/// Duplicate parsed-entity operations fail atomically without blocking normal edits.
#[cfg(any(feature = "embeddings", feature = "external-embeddings"))]
#[tokio::test]
#[serial_test::serial]
async fn parsed_entity_writes_refuse_duplicate_operations_atomically() {
    let (dir, state) = artifact_fixture().await;
    let session = mcp_test_session(&state);
    let function = edit_path_read(&state, &serde_json::json!("shop.db::schema_version")).await;
    let roots = source_base_roots(&state);
    let before = edit_path_disk(&dir, "src/shop/db.py");
    let (result, text) = edit_path_mutate(
        &state,
        &session,
        vec![
            edit_path_patch(&function, &[("return 1", "return 2")]),
            edit_path_patch(&function, &[("return 1", "return 3")]),
        ],
    )
    .await;
    assert_eq!(result.is_error, Some(true), "{text}");
    assert!(
        text.contains("edited more than once in one transaction"),
        "{text}"
    );
    assert_eq!(source_base_roots(&state), roots);
    assert_eq!(edit_path_disk(&dir, "src/shop/db.py"), before);
    let (result, text) = edit_path_mutate(
        &state,
        &session,
        vec![edit_path_patch(&function, &[("return 1", "return 2")])],
    )
    .await;
    assert_ne!(result.is_error, Some(true), "{text}");
    assert!(edit_path_disk(&dir, "src/shop/db.py").contains("return 2"));
}

/// A PNG image 16 by 8 pixels: its signature, its header and its end. Only
/// the header is read, so no pixel data is carried.
#[cfg(any(feature = "embeddings", feature = "external-embeddings"))]
fn artifact_png() -> Vec<u8> {
    let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
    png.extend_from_slice(&16u32.to_be_bytes());
    png.extend_from_slice(&8u32.to_be_bytes());
    png.extend_from_slice(&[8, 2, 0, 0, 0, 0x5c, 0x6d, 0x2f, 0x3b]);
    png.extend_from_slice(b"\0\0\0\0IEND\xae\x42\x60\x82");
    png
}

/// A GIF image 32 by 16 pixels: its signature, its screen and its end.
#[cfg(any(feature = "embeddings", feature = "external-embeddings"))]
fn artifact_gif() -> Vec<u8> {
    let mut gif = b"GIF89a".to_vec();
    gif.extend_from_slice(&[32, 0, 16, 0, 0, 0, 0]);
    gif.extend_from_slice(&[0x3b]);
    gif
}

/// The artifact fixture with a logo and a banner beside the package's code,
/// admitted and committed the way a change made on disk reaches Kin.
#[cfg(any(feature = "embeddings", feature = "external-embeddings"))]
async fn artifact_fixture_with_logo() -> (tempfile::TempDir, Arc<DaemonState>) {
    let (dir, state) = artifact_fixture().await;
    let place = dir.path().join("src/shop/static");
    std::fs::create_dir_all(&place).unwrap();
    std::fs::write(place.join("logo.png"), artifact_png()).unwrap();
    std::fs::write(place.join("banner.gif"), artifact_gif()).unwrap();
    // A template that names the logo, and a guide that names it by the end of
    // its place and names another logo elsewhere.
    let templates = dir.path().join("src/shop/templates");
    std::fs::create_dir_all(&templates).unwrap();
    std::fs::write(templates.join("index.html"), ARTIFACT_TEMPLATE).unwrap();
    std::fs::write(dir.path().join("docs/guide.md"), ARTIFACT_GUIDE).unwrap();
    waiting_admit(&state, "logo").await;
    waiting_commit(&state, "Add the logo").await;
    crate::loop_runner::ensure_non_entity_enrichment_coverage(&state).unwrap();
    (dir, state)
}

/// An entity held as bytes is found by its name and by a description, and
/// read as a description: its kind and what its header states, its size, its
/// content id, its history and the level Kin holds it at, with the ticket a
/// replacement carries. No reply holds its bytes, and asking for them whole
/// is refused in a sentence that says what to call instead.
#[cfg(any(feature = "embeddings", feature = "external-embeddings"))]
#[tokio::test]
#[serial_test::serial]
async fn artifact_bytes_are_found_and_described_and_never_returned() {
    let (_dir, state) = artifact_fixture_with_logo().await;
    let png = artifact_png();

    for query in ["logo", "logo.png", "png"] {
        let (result, text) =
            artifact_call(&state, "locate", serde_json::json!({"query": query})).await;
        assert_ne!(result.is_error, Some(true), "{text}");
        assert!(
            text.contains(&format!(
                "this is the 1 unparsed entity Kin holds whose name matches {query}"
            )),
            "{text}"
        );
        assert_eq!(
            artifact_rows(&text, "results"),
            ["shop.static :: logo.png  (PNG image)  held_as=bytes"],
            "{text}"
        );
    }
    let (_, text) = artifact_call(
        &state,
        "locate",
        serde_json::json!({"query": "logo image", "mode": "description", "surface": "compact"}),
    )
    .await;
    assert!(
        artifact_rows(&text, "results")
            .contains(&"shop.static :: logo.png  (PNG image)  held_as=bytes".to_string()),
        "{text}"
    );

    let (result, text) = artifact_call(
        &state,
        "source",
        serde_json::json!({"entity_id": "logo.png"}),
    )
    .await;
    assert_ne!(result.is_error, Some(true), "{text}");
    let field = |name: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(&format!("{name}: ")))
            .unwrap_or_else(|| panic!("the description names its {name}: {text}"))
            .to_string()
    };
    let revision = field("revision");
    // What names it: the template by its name, the guide by the end of its
    // place.
    let mentions = "matched_by: name\n\
         named_in (2):\n  \
         docs.guide :: Guide  (section)  The shop shows ![its logo](static/logo.png) on every \
         page.  held_as=text\n  \
         shop.templates :: index.html  (HTML)  <img src=\"{{ url_for('static', \
         filename='logo.png') }}\" alt=\"Shop logo\">  held_as=text\n\
         named_in_total: 2\n";
    let change = revision.strip_prefix("change=").unwrap();
    let digest: String = kin_blobs::digest(&png)
        .to_string()
        .chars()
        .take(12)
        .collect();
    assert_eq!(
        text,
        format!(
            "Answer (complete): this is the description of logo.png, a PNG image of 16 by 8 \
             pixels and {} bytes, held as bytes (Kin knows its name, type, size, history and \
             what refers to it, and never returns its bytes in a reply).\n\
             shop.static :: logo.png  (PNG image)\n\
             content: sha256={digest}\n\
             held_as: bytes\n\
             history: changes=1 latest={change}\n\
             {mentions}\
             projected_at: src/shop/static/logo.png\n\
             revision: {revision}",
            png.len()
        )
    );
    assert!(!text.contains("source_base:"), "{text}");

    // Neither reply holds the bytes, nor any spelling of them.
    let machine = artifact_read(&state, "shop.static :: logo.png").await;
    let artifact_id = state
        .graph
        .resolved_tree()
        .artifact_at_path(
            &kin_model::RepoPath::from_utf8("src/shop/static/logo.png".to_string()).unwrap(),
        )
        .unwrap()
        .artifact_id;
    assert_eq!(machine["id"], serde_json::json!(artifact_id.0));
    assert!(machine.get("source_base").is_none(), "{machine}");
    assert!(machine.get("body").is_none(), "{machine}");
    assert!(machine.get("editor_uri").is_none(), "{machine}");
    let served = format!("{machine}{text}");
    for spelling in ["PNG\\r", "IHDR", "iVBOR", "\u{89}", "content_base64"] {
        assert!(!served.contains(spelling), "{spelling}: {served}");
    }

    // Asked for whole, the bytes are refused.
    let (result, text) = artifact_call(
        &state,
        "source",
        serde_json::json!({"entity_id": "shop.static :: logo.png", "full_body": true}),
    )
    .await;
    assert_eq!(result.is_error, Some(true), "{text}");
    assert_eq!(
        text,
        "message: logo.png is held as bytes, and Kin does not return bytes in a reply. Call \
         source without full_body for its description: its type, size, history and what \
         refers to it."
    );
}

/// One operation on an unparsed entity, the way an agent writes it.
#[cfg(any(feature = "embeddings", feature = "external-embeddings"))]
fn artifact_operation(
    verb: &str,
    read: &serde_json::Value,
    payload: serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({
        "verb": verb, "target": read["id"], "description": "change an unparsed entity",
        "payload": payload
    })
}

/// Byte descriptions issue no edit base and cannot authorize mutation.
#[cfg(any(feature = "embeddings", feature = "external-embeddings"))]
#[tokio::test]
#[serial_test::serial]
async fn artifact_byte_metadata_does_not_authorize_replacement_or_removal() {
    let (dir, state) = artifact_fixture_with_logo().await;
    let session = mcp_test_session(&state);
    let read = artifact_read(&state, "shop.static :: logo.png").await;
    assert!(read.get("source_base").is_none(), "{read}");
    let roots = source_base_roots(&state);
    for (verb, payload) in [
        (
            "replace",
            serde_json::json!({"EntityContent": {
                "source_base": read["source_base"], "content_of": "banner.gif"
            }}),
        ),
        (
            "remove",
            serde_json::json!({"EntityRemove": {"source_base": read["source_base"]}}),
        ),
    ] {
        let (result, text) = edit_path_mutate(
            &state,
            &session,
            vec![artifact_operation(verb, &read, payload)],
        )
        .await;
        assert_eq!(result.is_error, Some(true), "{text}");
        assert_eq!(source_base_roots(&state), roots);
        assert_eq!(
            std::fs::read(dir.path().join("src/shop/static/logo.png")).unwrap(),
            artifact_png()
        );
    }
}

/// Unparsed metadata cannot supply the base for a new artifact mutation.
#[cfg(any(feature = "embeddings", feature = "external-embeddings"))]
#[tokio::test]
#[serial_test::serial]
async fn unparsed_text_metadata_does_not_supply_an_artifact_mutation_base() {
    let (dir, state) = artifact_fixture().await;
    let roots = source_base_roots(&state);
    artifact_assert_source_refused(&state, "shop :: LICENSE").await;
    artifact_assert_source_refused(&state, "shop :: README.md").await;
    assert_eq!(source_base_roots(&state), roots);
    assert_eq!(edit_path_disk(&dir, "LICENSE"), ARTIFACT_LICENSE);
    assert!(!dir.path().join("NOTICE").exists());
}

/// The rows of a text reply under `heading`, which is written
/// `heading (count):` or `heading: none`.
#[cfg(any(feature = "embeddings", feature = "external-embeddings"))]
fn artifact_named_in(text: &str) -> Vec<String> {
    artifact_rows(text, "named_in")
}

/// What names an unparsed entity is found in the text of code and of other
/// unparsed entities, by its name or by a path-shaped name that ends where it
/// is placed, and is answered as matches by name: each row the line it was
/// found on, never a resolved use. An entity's own text, a name that ends
/// another place, and a pattern do not count. `refs` takes the entity by its
/// label, its id and its name.
#[cfg(any(feature = "embeddings", feature = "external-embeddings"))]
#[tokio::test]
#[serial_test::serial]
async fn artifact_mentions_by_name_answer_refs_on_an_unparsed_entity() {
    let (_dir, state) = artifact_fixture_with_logo().await;

    let (result, text) = artifact_call(
        &state,
        "refs",
        serde_json::json!({"entity_id": "shop :: schema.sql"}),
    )
    .await;
    assert_ne!(result.is_error, Some(true), "{text}");
    assert!(text.starts_with("Answer (partial): "), "{text}");
    assert!(
        text.contains("\nentity:\n  shop :: schema.sql  (SQL)  held_as=text\n"),
        "{text}"
    );
    assert!(text.contains("\nmatched_by: name\n"), "{text}");
    let mut rows = artifact_named_in(&text);
    rows.sort();
    assert_eq!(
        rows,
        [
            "shop :: pyproject.toml  (package manifest)  shop = [\"schema.sql\", \"static/*.png\"]  held_as=text",
            "shop.README :: Install  (section)  Run `pip install .` and load `schema.sql` into the database.  held_as=text",
            "shop.db :: init_db  with resources.files(\"shop\").joinpath(\"schema.sql\").open() as handle:",
        ],
        "{text}"
    );
    assert!(text.contains("\nnamed_in_total: 3\n"), "{text}");

    // A name, and a path-shaped name that ends where it is placed; not a
    // name that ends another place, nor a pattern.
    for asked in [
        serde_json::json!({"query": "logo.png"}),
        serde_json::json!({"entity_id": "shop.static :: logo.png"}),
    ] {
        let (result, text) = artifact_call(&state, "refs", asked.clone()).await;
        assert_ne!(result.is_error, Some(true), "{asked}: {text}");
        let mut rows = artifact_named_in(&text);
        rows.sort();
        assert_eq!(
            rows,
            [
                "docs.guide :: Guide  (section)  The shop shows ![its logo](static/logo.png) on every page.  held_as=text",
                "shop.templates :: index.html  (HTML)  <img src=\"{{ url_for('static', filename='logo.png') }}\" alt=\"Shop logo\">  held_as=text",
            ],
            "{asked}: {text}"
        );
        assert!(!text.contains("assets/logo.png"), "{text}");
    }

    // Named twice in one entity, it is one row that says so.
    let (_, text) =
        artifact_call(&state, "refs", serde_json::json!({"entity_id": "LICENSE"})).await;
    assert_eq!(
        artifact_named_in(&text),
        ["shop.README :: Licence  (section)  See [LICENSE](LICENSE).  held_as=text  times=2"],
        "{text}"
    );

    // Named nowhere: none, and still not an absence Kin certifies.
    let (result, text) = artifact_call(
        &state,
        "refs",
        serde_json::json!({"entity_id": "banner.gif"}),
    )
    .await;
    assert_ne!(result.is_error, Some(true), "{text}");
    assert!(text.contains("\nnamed_in: none\n"), "{text}");
    assert!(!text.starts_with("Answer (complete)"), "{text}");

    // The description of an entity held as bytes lists what names it.
    let (_, text) = artifact_call(
        &state,
        "source",
        serde_json::json!({"entity_id": "logo.png"}),
    )
    .await;
    assert_eq!(artifact_named_in(&text).len(), 2, "{text}");
    assert!(text.contains("\nmatched_by: name\n"), "{text}");

    // An entity of code is answered as it always was.
    let (_, text) = artifact_call(
        &state,
        "refs",
        serde_json::json!({"entity_id": "shop.db :: init_db"}),
    )
    .await;
    assert!(!text.contains("named_in"), "{text}");
    assert!(
        text.contains("\nrelation_kinds: calls, imports, references"),
        "{text}"
    );
}

/// Artifact outlines are metadata, not permission to read unparsed source.
#[cfg(any(feature = "embeddings", feature = "external-embeddings"))]
#[tokio::test]
#[serial_test::serial]
async fn artifact_outlines_do_not_bypass_the_parsed_source_boundary() {
    let (dir, state) = artifact_fixture().await;
    artifact_assert_source_refused(&state, "HISTORY.rst").await;
    artifact_assert_source_refused(&state, "shop :: README.md").await;
    // Named sections remain discoverable metadata, never a whole-file fallback.
    let (_, text) = artifact_call(
        &state,
        "locate",
        serde_json::json!({
            "query": "Version 1.0.0", "mode": "name"
        }),
    )
    .await;
    assert!(
        artifact_rows(&text, "results")
            .iter()
            .any(|row| row.starts_with("shop.HISTORY :: Version 1.0.0  (section)")),
        "{text}"
    );
    assert_eq!(edit_path_disk(&dir, "HISTORY.rst"), artifact_history());
    assert_eq!(edit_path_disk(&dir, "README.md"), ARTIFACT_README);
}

/// The retrieval record of an entity held as bytes carries the media type of
/// the format Kin recognizes it as, read from its content, and the record of
/// bytes no format names says so. The record is derived where a daemon
/// derives it when it opens a store.
#[cfg(any(feature = "embeddings", feature = "external-embeddings"))]
#[tokio::test]
#[serial_test::serial]
async fn artifact_bytes_records_carry_the_media_type_of_their_format() {
    let (dir, state) = artifact_fixture_with_logo().await;
    std::fs::write(
        dir.path().join("src/shop/static/data.blob"),
        [0u8, 0xFF, 1, 2],
    )
    .unwrap();
    waiting_admit(&state, "data").await;
    waiting_commit(&state, "Add data").await;
    crate::loop_runner::ensure_non_entity_enrichment_coverage(&state).unwrap();
    for (place, media_type) in [
        ("src/shop/static/logo.png", "image/png"),
        ("src/shop/static/banner.gif", "image/gif"),
        ("src/shop/static/data.blob", "application/octet-stream"),
    ] {
        let record = state
            .graph
            .get_opaque_artifact(&kin_model::FilePathId::new(place))
            .unwrap()
            .unwrap_or_else(|| panic!("{place} has a retrieval record"));
        assert_eq!(record.mime_type.as_deref(), Some(media_type), "{place}");
    }
}
