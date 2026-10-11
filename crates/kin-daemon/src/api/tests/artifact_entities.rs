// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

// A tracked thing Kin does not parse, as an agent finds, reads and changes
// it: `locate`, `source` and `mutate` over the served route, each reply
// presented the way the default agent profile reads it, as compact text.
//
// Every case starts from the records the non-entity coverage pass derives,
// and only a build with embedding support carries that pass, so the cases and
// their fixtures share its gate. The reply helpers serve other cases too and
// are not gated.

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
        text.push_str(&format!("- Entry {entry:03}: one line of the first release.\n"));
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
    // Its id reads it.
    let id = ranked["entity_id"]
        .as_str()
        .or_else(|| ranked["id"].as_str())
        .unwrap_or_else(|| panic!("a ranked row carries its id: {ranked}"));
    let read = artifact_read(&state, id).await;
    assert_eq!(read["body"], ARTIFACT_SCHEMA);

    // No row says where anything is projected.
    for query in ["LICENSE", "schema", "readme"] {
        let (_, text) = artifact_call(&state, "locate", serde_json::json!({"query": query})).await;
        assert!(!text.contains("src/"), "{text}");
        assert!(!text.contains("projected_at"), "{text}");
    }
}

/// A small text is read whole, with the ticket a write carries back, under
/// every spelling a reply serves for it. The reply states the level Kin holds
/// it at and, in one line, where it is projected.
#[cfg(any(feature = "embeddings", feature = "external-embeddings"))]
#[tokio::test]
#[serial_test::serial]
async fn artifact_text_is_read_whole_with_a_ticket_and_its_level() {
    let (_dir, state) = artifact_fixture().await;
    let binding = state.local_repository_authority_binding().unwrap();
    let repository_id = binding.repository_id().as_str();
    let workspace_id = binding.workspace_id().to_string();
    let (result, text) = artifact_call(
        &state,
        "source",
        serde_json::json!({"entity_id": "shop :: LICENSE"}),
    )
    .await;
    assert_ne!(result.is_error, Some(true), "{text}");
    let ticket = text
        .lines()
        .find_map(|line| line.strip_prefix("source_base: ticket="))
        .unwrap_or_else(|| panic!("a read carries a ticket: {text}"));
    let revision = text
        .lines()
        .find(|line| line.starts_with("revision: "))
        .unwrap_or_else(|| panic!("a read names its revision: {text}"));
    let body: String = ARTIFACT_LICENSE
        .lines()
        .map(|line| format!("  {line}\n"))
        .collect();
    assert_eq!(
        text,
        format!(
            "Answer (complete): this is the text of LICENSE, held as text (Kin knows its name, \
             its history and what refers to it, not its structure), the whole body.\n\
             shop :: LICENSE  (licence)\n\
             body:\n\
             {body}\
             held_as: text\n\
             projected_at: LICENSE\n\
             {revision}\n\
             source_base: ticket={ticket}\n\
             source_identity:\n  \
             shop :: LICENSE  repository_id={repository_id:.8}  workspace_id={workspace_id:.8}"
        )
    );

    // The machine reply holds the exact text and the base that guards it.
    let read = artifact_read(&state, "shop :: LICENSE").await;
    assert_eq!(read["body"], ARTIFACT_LICENSE);
    assert_eq!(read["held_as"], "text");
    assert!(read.get("editor_uri").is_none(), "{read}");
    let artifact_id = state
        .graph
        .resolved_tree()
        .artifact_at_path(&kin_model::RepoPath::from_utf8("LICENSE".to_string()).unwrap())
        .unwrap()
        .artifact_id;
    assert_eq!(read["id"], serde_json::json!(artifact_id.0));
    assert_eq!(
        read["source_identity"],
        serde_json::json!({
            "repository_id": repository_id,
            "workspace_id": workspace_id,
            "entity_id": artifact_id.0,
        })
    );
    assert!(
        kin_mcp::source_ticket::is_ticket(&read["source_base"]),
        "{read}"
    );
    let id = read["id"].as_str().unwrap().to_string();
    let short: String = id.chars().filter(|c| *c != '-').take(8).collect();
    for address in [
        id.clone(),
        short.clone(),
        format!("#{short}"),
        format!("LICENSE #{short}"),
        "LICENSE".to_string(),
        "shop::LICENSE".to_string(),
    ] {
        let again = artifact_read(&state, &address).await;
        assert_eq!(again["id"], read["id"], "{address}");
        assert_eq!(again["body"], ARTIFACT_LICENSE, "{address}");
    }

    // One below the package, placed like the code it sits with.
    let (_, text) = artifact_call(
        &state,
        "source",
        serde_json::json!({"entity_id": "schema.sql"}),
    )
    .await;
    assert!(text.contains("\nshop :: schema.sql  (SQL)\n"), "{text}");
    assert!(
        text.contains("\nprojected_at: src/shop/schema.sql\n"),
        "{text}"
    );
    // That one line is the only place the reply says where it is projected.
    assert_eq!(text.matches("src/shop").count(), 1, "{text}");

    // A name two artifacts share names neither, and each is listed by an
    // address that reads it.
    let (result, text) = artifact_call(
        &state,
        "source",
        serde_json::json!({"entity_id": "README.md"}),
    )
    .await;
    assert_eq!(result.is_error, Some(true), "{text}");
    assert_eq!(
        text,
        "message: `README.md` names 2 unparsed entities, and this call takes one. Pass one of: \
         `shop :: README.md`, `docs :: README.md`."
    );
    let docs = artifact_read(&state, "docs :: README.md").await;
    assert_eq!(docs["body"], "# Documentation\n");
}

/// A text longer than one reply carries is read in excerpts that join to its
/// exact bytes. Each excerpt carries the ticket a patch needs, and the whole
/// text asked for at once pages the same way, from its first excerpt.
#[cfg(any(feature = "embeddings", feature = "external-embeddings"))]
#[tokio::test]
#[serial_test::serial]
async fn artifact_long_text_is_read_in_excerpts_that_join_to_the_exact_text() {
    let (_dir, state) = artifact_fixture().await;
    let changelog = artifact_changelog();
    assert!(changelog.len() > kin_mcp::source_pages::FULL_BODY_MAX_BYTES);

    let (result, text) = artifact_call(
        &state,
        "source",
        serde_json::json!({"entity_id": "CHANGES.txt"}),
    )
    .await;
    assert_ne!(result.is_error, Some(true), "{text}");
    let first = tool_result_payload(&result);
    let id = first["entity_id"]
        .as_str()
        .or_else(|| first["id"].as_str())
        .unwrap_or_else(|| panic!("{first}"))
        .to_string();
    assert!(
        text.starts_with(
            "Answer (complete): Excerpt: this is the text of CHANGES.txt, held as text (Kin \
             knows its name, its history and what refers to it, not its structure), lines +0 \
             to +"
        ),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "of {} bytes), not the whole body.",
            changelog.len()
        )),
        "{text}"
    );
    assert!(
        text.contains(
            "This excerpt's source_base carries a patch whose old_text is copied from these \
             lines, and never a whole replacement."
        ),
        "{text}"
    );
    // Its whole text is not offered: it is longer than one reply carries.
    assert!(!text.contains("full_body"), "{text}");
    assert!(text.contains("\nsource_base: ticket="), "{text}");

    // Followed to the end, the excerpts are the text, byte for byte.
    let mut joined = String::new();
    let mut cursor: Option<String> = None;
    let mut excerpts = 0;
    loop {
        let mut arguments = serde_json::json!({"entity_id": id});
        if let Some(cursor) = &cursor {
            arguments["cursor"] = serde_json::json!(cursor);
        }
        let mut result = edit_path_call(&state, "get_entity_source", arguments.clone()).await;
        let sent: HashMap<String, serde_json::Value> = serde_json::from_value(arguments).unwrap();
        kin_mcp::source_pages::present("get_entity_source", &sent, &mut result);
        assert_ne!(result.is_error, Some(true), "{}", mcp_result_text(&result));
        let page = tool_result_payload(&result);
        assert!(
            page["body"].as_str().unwrap().len() <= kin_mcp::source_pages::SOURCE_PAGE_BYTES,
            "{page}"
        );
        assert!(
            page["source_base"].is_object(),
            "every excerpt carries its base: {page}"
        );
        joined.push_str(page["body"].as_str().unwrap());
        excerpts += 1;
        match page["next_cursor"].as_str() {
            Some(next) => cursor = Some(next.to_string()),
            None => break,
        }
        assert!(excerpts < 16, "the excerpts end");
    }
    assert!(excerpts > 1);
    assert_eq!(joined, changelog);

    // Asked for whole, it is served its first excerpt with its ticket, and
    // nothing past it.
    let (result, text) = artifact_call(
        &state,
        "source",
        serde_json::json!({"entity_id": "CHANGES.txt", "full_body": true}),
    )
    .await;
    assert_ne!(result.is_error, Some(true), "{text}");
    assert_eq!(tool_result_payload(&result)["body"], first["body"]);
    assert!(
        text.starts_with(
            "Answer (complete): Excerpt: this is the text of CHANGES.txt, held as text"
        ),
        "{text}"
    );
    assert!(
        text.contains("; a body over 16 KB is never served whole."),
        "{text}"
    );
    assert!(text.contains("\nsource_base: ticket="), "{text}");
    assert!(!text.contains("Entry 399"), "{text}");
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

/// The first excerpt of `address` as the MCP server pages it: the machine
/// reply, with the base a patch against it carries.
#[cfg(any(feature = "embeddings", feature = "external-embeddings"))]
async fn artifact_first_excerpt(state: &Arc<DaemonState>, address: &str) -> serde_json::Value {
    let arguments = serde_json::json!({"entity_id": address});
    let mut result = edit_path_call(state, "get_entity_source", arguments.clone()).await;
    let sent: HashMap<String, serde_json::Value> = serde_json::from_value(arguments).unwrap();
    kin_mcp::source_pages::present("get_entity_source", &sent, &mut result);
    assert_ne!(result.is_error, Some(true), "{}", mcp_result_text(&result));
    tool_result_payload(&result)
}

/// A text artifact is changed the way an entity is: an anchored patch with
/// the ticket of its read lands in the graph and in the projected tree, at
/// the one place its anchor names, and the artifact keeps its id. A ticket
/// from before the change, and an anchor that names no one place, are refused
/// in a sentence, with nothing written.
#[cfg(any(feature = "embeddings", feature = "external-embeddings"))]
#[tokio::test]
#[serial_test::serial]
async fn artifact_text_is_patched_with_the_ticket_of_its_read() {
    let (dir, state) = artifact_fixture().await;
    let session = mcp_test_session(&state);
    let read = artifact_read(&state, "shop :: LICENSE").await;

    let (result, text) = edit_path_mutate(
        &state,
        &session,
        vec![edit_path_patch(
            &read,
            &[("Example Authors", "Example Authors and contributors")],
        )],
    )
    .await;
    assert_ne!(result.is_error, Some(true), "{text}");
    assert_eq!(text, "Changed shop :: LICENSE; the graph now holds it.");
    let patched = ARTIFACT_LICENSE.replace("Example Authors", "Example Authors and contributors");
    assert_eq!(edit_path_disk(&dir, "LICENSE"), patched);
    // The graph serves the same text back, and it is the same artifact.
    let after = artifact_read(&state, "shop :: LICENSE").await;
    assert_eq!(after["body"].as_str().unwrap(), patched);
    assert_eq!(after["id"], read["id"]);
    assert_ne!(after["source_base"], read["source_base"]);
    // Nothing else moved.
    assert_eq!(edit_path_disk(&dir, "README.md"), ARTIFACT_README);
    assert_eq!(edit_path_disk(&dir, "src/shop/db.py"), ARTIFACT_DB);
    let roots = source_base_roots(&state);

    // The ticket of the first read names text that has since changed.
    let (result, text) = edit_path_mutate(
        &state,
        &session,
        vec![edit_path_patch(
            &read,
            &[("MIT License", "The MIT License")],
        )],
    )
    .await;
    assert_eq!(result.is_error, Some(true), "{text}");
    assert_eq!(
        text,
        "That ticket is from before LICENSE last changed. Read LICENSE again with source and \
         resend your change with the source_base that read returns. Nothing was written."
    );

    // An anchor that stands twice names no one place, and one that stands
    // nowhere names none.
    for (anchor, refusal) in [
        (
            "of ",
            "Nothing was written: patch edit #0: old_text occurs 2 times in the text of LICENSE, \
             so extend it with the text beside the one you mean until it occurs once.",
        ),
        (
            "Apache License",
            "Nothing was written: patch edit #0: old_text is not in the text of LICENSE, so copy \
             it exactly from what source returned for LICENSE, or read LICENSE again with \
             source.",
        ),
    ] {
        let (result, text) = edit_path_mutate(
            &state,
            &session,
            vec![edit_path_patch(&after, &[(anchor, "x")])],
        )
        .await;
        assert_eq!(result.is_error, Some(true), "{text}");
        assert_eq!(text, refusal);
    }
    assert_eq!(edit_path_disk(&dir, "LICENSE"), patched);
    assert_eq!(source_base_roots(&state), roots);

    // A text short enough to be read whole is also replaced whole.
    let (result, text) = edit_path_mutate(
        &state,
        &session,
        vec![edit_path_update(&after, "Public domain.\n")],
    )
    .await;
    assert_ne!(result.is_error, Some(true), "{text}");
    assert_eq!(text, "Changed shop :: LICENSE; the graph now holds it.");
    assert_eq!(edit_path_disk(&dir, "LICENSE"), "Public domain.\n");
    let replaced = artifact_read(&state, "shop :: LICENSE").await;
    assert_eq!(replaced["body"], "Public domain.\n");
    assert_eq!(replaced["id"], read["id"]);
}

/// A long text is patched from an excerpt: the excerpt's ticket carries a
/// patch whose anchor stands once in the whole text, so a changelog gains an
/// entry without being read whole. An anchor that also stands in a part the
/// reader did not see is refused, and so is a whole replacement of a text no
/// reply carried whole. A search then finds the artifact by what was written.
#[cfg(any(feature = "embeddings", feature = "external-embeddings"))]
#[tokio::test]
#[serial_test::serial]
async fn artifact_long_text_is_patched_from_an_excerpt_and_never_replaced_whole() {
    let (dir, state) = artifact_fixture().await;
    let session = mcp_test_session(&state);
    let changelog = artifact_changelog();
    let page = artifact_first_excerpt(&state, "CHANGES.txt").await;
    assert_eq!(page["body_complete"], false, "{page}");
    assert!(page["body"].as_str().unwrap().len() < changelog.len());

    let entry = "- Entry new: a line a reader of one excerpt added.\n";
    let (result, text) = edit_path_mutate(
        &state,
        &session,
        vec![edit_path_patch(
            &page,
            &[(
                "Unreleased\n----------\n\n",
                &format!("Unreleased\n----------\n\n{entry}"),
            )],
        )],
    )
    .await;
    assert_ne!(result.is_error, Some(true), "{text}");
    assert_eq!(text, "Changed shop :: CHANGES.txt; the graph now holds it.");
    let patched = changelog.replace(
        "Unreleased\n----------\n\n",
        &format!("Unreleased\n----------\n\n{entry}"),
    );
    assert_eq!(edit_path_disk(&dir, "CHANGES.txt"), patched);
    let roots = source_base_roots(&state);

    // An anchor every entry holds stands many times in the whole text.
    let page = artifact_first_excerpt(&state, "CHANGES.txt").await;
    let (result, text) = edit_path_mutate(
        &state,
        &session,
        vec![edit_path_patch(
            &page,
            &[("a change that is described", "a change described")],
        )],
    )
    .await;
    assert_eq!(result.is_error, Some(true), "{text}");
    assert_eq!(
        text,
        "Nothing was written: patch edit #0: old_text occurs 400 times in the text of \
         CHANGES.txt, so extend it with the text beside the one you mean until it occurs once."
    );

    // The whole text was never in one reply, so it is never replaced whole.
    let (result, text) =
        edit_path_mutate(&state, &session, vec![edit_path_update(&page, "Changes\n")]).await;
    assert_eq!(result.is_error, Some(true), "{text}");
    assert_eq!(
        text,
        "Nothing was written: CHANGES.txt holds more text than one reply carries, so it is \
         changed by patch and never replaced whole. Send a patch whose old_text is copied from \
         an excerpt source returned."
    );
    assert_eq!(edit_path_disk(&dir, "CHANGES.txt"), patched);
    assert_eq!(source_base_roots(&state), roots);

    // What was written is found by a description of it.
    let (_, text) = artifact_call(
        &state,
        "locate",
        serde_json::json!({
            "query": "a line a reader of one excerpt added",
            "mode": "description",
            "surface": "compact"
        }),
    )
    .await;
    assert!(
        artifact_rows(&text, "results")
            .contains(&"shop :: CHANGES.txt  (plain text)  held_as=text".to_string()),
        "{text}"
    );
}

/// A change to an artifact is held to what it is. Source Kin parses is
/// changed through its entities, and a base that names it as an artifact is
/// refused; one transaction changes one artifact once.
#[cfg(any(feature = "embeddings", feature = "external-embeddings"))]
#[tokio::test]
#[serial_test::serial]
async fn artifact_writes_refuse_parsed_code_and_two_changes_to_one_artifact() {
    let (dir, state) = artifact_fixture().await;
    let session = mcp_test_session(&state);
    let read = artifact_read(&state, "shop :: LICENSE").await;

    let (result, text) = edit_path_mutate(
        &state,
        &session,
        vec![
            edit_path_patch(&read, &[("MIT License", "The MIT License")]),
            edit_path_patch(&read, &[("Example Authors", "Example Writers")]),
        ],
    )
    .await;
    assert_eq!(result.is_error, Some(true), "{text}");
    assert_eq!(
        text,
        "Nothing was written: one entity is changed more than once in one transaction, so \
         send every edit to it in one patch's edits."
    );
    assert_eq!(edit_path_disk(&dir, "LICENSE"), ARTIFACT_LICENSE);

    // Two edits in one patch are the way to make two changes.
    let (result, text) = edit_path_mutate(
        &state,
        &session,
        vec![edit_path_patch(
            &read,
            &[
                ("MIT License", "The MIT License"),
                ("Example Authors", "Example Writers"),
            ],
        )],
    )
    .await;
    assert_ne!(result.is_error, Some(true), "{text}");
    assert_eq!(
        edit_path_disk(&dir, "LICENSE"),
        ARTIFACT_LICENSE
            .replace("MIT License", "The MIT License")
            .replace("Example Authors", "Example Writers")
    );

    // An artifact and a function change in one transaction, or neither does.
    let license = artifact_read(&state, "shop :: LICENSE").await;
    let function = edit_path_read(&state, &serde_json::json!("shop.db::schema_version")).await;
    let (result, text) = edit_path_mutate(
        &state,
        &session,
        vec![
            edit_path_patch(&license, &[("Example Writers", "Example Authors")]),
            edit_path_patch(&function, &[("return 1", "return 2")]),
        ],
    )
    .await;
    assert_ne!(result.is_error, Some(true), "{text}");
    assert_eq!(
        text,
        "Changed shop.db :: schema_version and shop :: LICENSE; 1 source unit written; the \
         graph now holds the change."
    );
    assert!(edit_path_disk(&dir, "LICENSE").contains("Example Authors"));
    assert!(edit_path_disk(&dir, "src/shop/db.py").contains("return 2"));

    // Source Kin parses is not an artifact to patch: a ticket that names it
    // as one resolves to nothing, and the write is refused before it stages.
    let code = state
        .graph
        .resolved_tree()
        .artifact_at_path(&kin_model::RepoPath::from_utf8("src/shop/db.py".to_string()).unwrap())
        .unwrap()
        .artifact_id;
    let license = artifact_read(&state, "shop :: LICENSE").await;
    let ticket = license["source_base"]["ticket"].as_str().unwrap();
    let forged = ticket.replacen(license["id"].as_str().unwrap(), &code.0.to_string(), 1);
    assert_ne!(forged, ticket);
    let before = edit_path_disk(&dir, "src/shop/db.py");
    let (result, text) = edit_path_mutate(
        &state,
        &session,
        vec![serde_json::json!({
            "verb": "patch", "target": code.0, "description": "patch code as text",
            "payload": {"EntitySourcePatch": {
                "source_base": {"ticket": forged},
                "edits": [{"old_text": "import sqlite3", "new_text": "import sqlite4"}]
            }}
        })],
    )
    .await;
    assert_eq!(result.is_error, Some(true), "{text}");
    assert!(text.contains("db.py is held as code"), "{text}");
    assert!(text.ends_with("Nothing was written."), "{text}");
    assert_eq!(edit_path_disk(&dir, "src/shop/db.py"), before);
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
    let binding = state.local_repository_authority_binding().unwrap();
    let repository_id = binding.repository_id().as_str();
    let workspace_id = binding.workspace_id().to_string();
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
    let (revision, ticket) = (field("revision"), field("source_base"));
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
             revision: {revision}\n\
             source_base: {ticket}\n\
             source_identity:\n  \
             shop.static :: logo.png  repository_id={repository_id:.8}  workspace_id={workspace_id:.8}",
            png.len()
        )
    );
    assert!(ticket.starts_with("ticket="), "{text}");

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
    assert_eq!(
        machine["source_identity"],
        serde_json::json!({
            "repository_id": repository_id,
            "workspace_id": workspace_id,
            "entity_id": artifact_id.0,
        })
    );
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

/// An entity held as bytes is replaced by content Kin already holds: another
/// image's, or its own as it stood at an earlier change. It keeps its id, its
/// history grows, and its description follows its new content. A copy is
/// created beside it, and it is removed. Each refusal says what to send
/// instead, with nothing written.
#[cfg(any(feature = "embeddings", feature = "external-embeddings"))]
#[tokio::test]
#[serial_test::serial]
async fn artifact_bytes_are_replaced_by_content_kin_holds_and_removed() {
    let (dir, state) = artifact_fixture_with_logo().await;
    let session = mcp_test_session(&state);
    let disk = |place: &str| std::fs::read(dir.path().join(place)).ok();
    let read = artifact_read(&state, "shop.static :: logo.png").await;
    assert_eq!(read["history"]["changes"], 1, "{read}");
    let first = read["history"]["latest"].as_str().unwrap().to_string();

    // Another unparsed entity's content.
    let (result, text) = edit_path_mutate(
        &state,
        &session,
        vec![artifact_operation(
            "replace",
            &read,
            serde_json::json!({"EntityContent": {
                "source_base": read["source_base"], "content_of": "banner.gif"
            }}),
        )],
    )
    .await;
    assert_ne!(result.is_error, Some(true), "{text}");
    assert_eq!(
        text,
        "Changed shop.static :: logo.png; the graph now holds it."
    );
    assert_eq!(disk("src/shop/static/logo.png"), Some(artifact_gif()));
    let (_, text) = artifact_call(
        &state,
        "source",
        serde_json::json!({"entity_id": "logo.png"}),
    )
    .await;
    assert!(
        text.starts_with(
            "Answer (complete): this is the description of logo.png, a GIF image of 32 by 16 \
             pixels and 14 bytes, held as bytes"
        ),
        "{text}"
    );
    let replaced = artifact_read(&state, "shop.static :: logo.png").await;
    assert_eq!(replaced["id"], read["id"]);
    assert_eq!(replaced["history"]["changes"], 2, "{replaced}");
    assert_eq!(replaced["history"]["first"], first.as_str(), "{replaced}");

    // The ticket of the first read is from before that change.
    let roots = source_base_roots(&state);
    let (result, text) = edit_path_mutate(
        &state,
        &session,
        vec![artifact_operation(
            "replace",
            &read,
            serde_json::json!({"EntityContent": {
                "source_base": read["source_base"], "content_of": "banner.gif"
            }}),
        )],
    )
    .await;
    assert_eq!(result.is_error, Some(true), "{text}");
    assert_eq!(
        text,
        "That ticket is from before logo.png last changed. Read logo.png again with source and \
         resend your change with the source_base that read returns. Nothing was written."
    );

    // Refused in a sentence each, with nothing written.
    for (content, refusal) in [
        (
            serde_json::json!({"content_of": "banner.gif"}),
            "Nothing was written: the replacement leaves logo.png as it is, so nothing was \
             written.",
        ),
        (
            serde_json::json!({"content_of": "db.py"}),
            "Nothing was written: db.py is held as code, and its source is not given to an \
             unparsed entity.",
        ),
        (
            serde_json::json!({"content_of": "missing.png"}),
            "Nothing was written: content_of `missing.png` names no unparsed entity Kin holds, \
             so find it with locate and pass the id or the module :: name its row shows.",
        ),
        (
            serde_json::json!({"content_of": "logo.png", "at": "deadbeefdeadbeef"}),
            "Nothing was written: at `deadbeefdeadbeef` names no change in the history Kin \
             holds for this branch; pass a change id as a history line shows it.",
        ),
    ] {
        let mut payload = content.clone();
        payload["source_base"] = replaced["source_base"].clone();
        let (result, text) = edit_path_mutate(
            &state,
            &session,
            vec![artifact_operation(
                "replace",
                &replaced,
                serde_json::json!({"EntityContent": payload}),
            )],
        )
        .await;
        assert_eq!(result.is_error, Some(true), "{content}: {text}");
        assert_eq!(text, refusal, "{content}");
    }
    assert_eq!(source_base_roots(&state), roots);
    assert_eq!(disk("src/shop/static/logo.png"), Some(artifact_gif()));

    // Its own content as it stood at the change that first held it.
    let (result, text) = edit_path_mutate(
        &state,
        &session,
        vec![artifact_operation(
            "replace",
            &replaced,
            serde_json::json!({"EntityContent": {
                "source_base": replaced["source_base"], "content_of": "logo.png", "at": first
            }}),
        )],
    )
    .await;
    assert_ne!(result.is_error, Some(true), "{text}");
    assert_eq!(disk("src/shop/static/logo.png"), Some(artifact_png()));
    let restored = artifact_read(&state, "shop.static :: logo.png").await;
    assert_eq!(restored["history"]["changes"], 3, "{restored}");
    assert_eq!(
        restored["artifact"]["detail"], "16 by 8 pixels",
        "{restored}"
    );

    // A copy beside it, under a name of its own.
    let banner = artifact_read(&state, "banner.gif").await;
    let (result, text) = edit_path_mutate(
        &state,
        &session,
        vec![artifact_operation(
            "create",
            &banner,
            serde_json::json!({"EntityContent": {
                "source_base": banner["source_base"], "name": "logo-copy.png",
                "content_of": "shop.static :: logo.png"
            }}),
        )],
    )
    .await;
    assert_ne!(result.is_error, Some(true), "{text}");
    assert_eq!(
        text,
        "Created shop.static :: logo-copy.png; the graph now holds it."
    );
    assert_eq!(disk("src/shop/static/logo-copy.png"), Some(artifact_png()));
    let copy = artifact_read(&state, "logo-copy.png").await;
    assert_ne!(copy["id"], restored["id"]);
    assert_eq!(copy["held_as"], "bytes");

    // Removed: gone from the projected tree and from every answer.
    let (result, text) = edit_path_mutate(
        &state,
        &session,
        vec![artifact_operation(
            "remove",
            &restored,
            serde_json::json!({"EntityRemove": {"source_base": restored["source_base"]}}),
        )],
    )
    .await;
    assert_ne!(result.is_error, Some(true), "{text}");
    assert_eq!(
        text,
        "Removed shop.static :: logo.png; the graph now holds it."
    );
    assert_eq!(disk("src/shop/static/logo.png"), None);
    let (_, text) = artifact_call(&state, "locate", serde_json::json!({"query": "logo"})).await;
    assert_eq!(
        artifact_rows(&text, "results"),
        ["shop.static :: logo-copy.png  (PNG image)  held_as=bytes"],
        "{text}"
    );
    let (result, _) = artifact_call(
        &state,
        "source",
        serde_json::json!({"entity_id": restored["id"]}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
}

/// A new unparsed entity is created with its text beside an entity Kin
/// holds, unparsed or code, placed where that entity is, and removed. A name
/// that is a place, that Kin would parse as code, or that is already held
/// there is refused.
#[cfg(any(feature = "embeddings", feature = "external-embeddings"))]
#[tokio::test]
#[serial_test::serial]
async fn artifact_text_is_created_beside_an_entity_and_removed() {
    let (dir, state) = artifact_fixture().await;
    let session = mcp_test_session(&state);
    let license = artifact_read(&state, "shop :: LICENSE").await;

    let (result, text) = edit_path_mutate(
        &state,
        &session,
        vec![artifact_operation(
            "create",
            &license,
            serde_json::json!({"EntityContent": {
                "source_base": license["source_base"], "name": "NOTICE",
                "text": "Shop includes no third-party work.\n"
            }}),
        )],
    )
    .await;
    assert_ne!(result.is_error, Some(true), "{text}");
    assert_eq!(text, "Created shop :: NOTICE; the graph now holds it.");
    assert_eq!(
        edit_path_disk(&dir, "NOTICE"),
        "Shop includes no third-party work.\n"
    );
    let (_, text) =
        artifact_call(&state, "source", serde_json::json!({"entity_id": "NOTICE"})).await;
    assert!(
        text.starts_with("Answer (complete): this is the text of NOTICE, held as text"),
        "{text}"
    );

    // Beside a function: where its source is.
    let function = edit_path_read(&state, &serde_json::json!("shop.db::init_db")).await;
    let (result, text) = edit_path_mutate(
        &state,
        &session,
        vec![serde_json::json!({
            "verb": "create", "target": function["id"], "description": "add seed data",
            "payload": {"EntityContent": {
                "source_base": function["source_base"], "name": "seed.sql",
                "text": "INSERT INTO item VALUES (1, 'pen', 120);\n"
            }}
        })],
    )
    .await;
    assert_ne!(result.is_error, Some(true), "{text}");
    assert_eq!(text, "Created shop :: seed.sql; the graph now holds it.");
    assert_eq!(
        edit_path_disk(&dir, "src/shop/seed.sql"),
        "INSERT INTO item VALUES (1, 'pen', 120);\n"
    );

    let license = artifact_read(&state, "shop :: LICENSE").await;
    let roots = source_base_roots(&state);
    for (name, refusal) in [
        (
            "docs/guide.md",
            "operation #0 ('create'): the name given is not one name: an entity's name holds \
             no separator, so create it beside an entity Kin holds in the place you mean",
        ),
        (
            "setup.py",
            "Nothing was written: setup.py names source Kin parses, which is created through \
             its entities with EntityCreate, not as an unparsed entity.",
        ),
        (
            "README.md",
            "Nothing was written: `README.md` already stands beside it, so replace that entity, \
             or create under another name.",
        ),
    ] {
        let (result, text) = edit_path_mutate(
            &state,
            &session,
            vec![artifact_operation(
                "create",
                &license,
                serde_json::json!({"EntityContent": {
                    "source_base": license["source_base"], "name": name, "text": "x\n"
                }}),
            )],
        )
        .await;
        assert_eq!(result.is_error, Some(true), "{name}: {text}");
        assert_eq!(text, refusal, "{name}");
    }
    assert_eq!(source_base_roots(&state), roots);

    let notice = artifact_read(&state, "NOTICE").await;
    let (result, text) = edit_path_mutate(
        &state,
        &session,
        vec![artifact_operation(
            "remove",
            &notice,
            serde_json::json!({"EntityRemove": {"source_base": notice["source_base"]}}),
        )],
    )
    .await;
    assert_ne!(result.is_error, Some(true), "{text}");
    assert_eq!(text, "Removed shop :: NOTICE; the graph now holds it.");
    assert!(!dir.path().join("NOTICE").exists());
    let (_, text) = artifact_call(&state, "locate", serde_json::json!({"query": "NOTICE"})).await;
    assert!(!text.contains("held_as"), "{text}");
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
    let (_, text) = artifact_call(&state, "refs", serde_json::json!({"entity_id": "LICENSE"})).await;
    assert_eq!(
        artifact_named_in(&text),
        ["shop.README :: Licence  (section)  See [LICENSE](LICENSE).  held_as=text  times=2"],
        "{text}"
    );

    // Named nowhere: none, and still not an absence Kin certifies.
    let (result, text) =
        artifact_call(&state, "refs", serde_json::json!({"entity_id": "banner.gif"})).await;
    assert_ne!(result.is_error, Some(true), "{text}");
    assert!(text.contains("\nnamed_in: none\n"), "{text}");
    assert!(!text.starts_with("Answer (complete)"), "{text}");

    // The description of an entity held as bytes lists what names it.
    let (_, text) = artifact_call(&state, "source", serde_json::json!({"entity_id": "logo.png"})).await;
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
    assert!(text.contains("\nrelation_kinds: calls, imports, references"), "{text}");
}

/// A text held with parts and longer than a class is served whole is read as
/// its outline: the text before its first section, and its sections by id.
/// A section is read by its address, with the ticket a patch needs and the
/// code it names. A patch to one section lands without the rest of the text
/// being read, the section keeps its id, and a ticket of another section
/// still holds. locate finds a section by its title, and a short text with
/// parts is read whole.
#[cfg(any(feature = "embeddings", feature = "external-embeddings"))]
#[tokio::test]
#[serial_test::serial]
async fn artifact_sections_are_read_found_and_patched_by_themselves() {
    let (dir, state) = artifact_fixture().await;
    let session = mcp_test_session(&state);
    let history = artifact_history();
    assert!(history.len() > kin_mcp::container_shape::CONTAINER_INLINE_MAX_BYTES);

    // The outline, and none of the text past its title.
    let (result, text) =
        artifact_call(&state, "source", serde_json::json!({"entity_id": "HISTORY.rst"})).await;
    assert_ne!(result.is_error, Some(true), "{text}");
    let outline = tool_result_payload(&result);
    assert!(outline.get("body").is_none(), "{outline}");
    assert!(outline.get("source_base").is_none(), "{outline}");
    assert_eq!(outline["held_as"], "parts", "{outline}");
    assert_eq!(outline["header"], "History\n=======", "{outline}");
    let members: Vec<&str> = outline["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|member| member["name"].as_str().unwrap())
        .collect();
    assert_eq!(members, ["Version 1.1.0", "Version 1.0.0"], "{outline}");
    assert!(text.contains("this is the outline of HISTORY.rst"), "{text}");
    assert!(!text.contains("Entry 000"), "{text}");

    // A section by its address: its text, its ticket and the code it names.
    let (result, text) = artifact_call(
        &state,
        "source",
        serde_json::json!({"entity_id": "shop.HISTORY :: Version 1.1.0"}),
    )
    .await;
    assert_ne!(result.is_error, Some(true), "{text}");
    let section = tool_result_payload(&result);
    assert_eq!(section["name"], "Version 1.1.0", "{section}");
    assert_eq!(section["kind"], "section");
    assert_eq!(section["module"], "shop.HISTORY");
    assert_eq!(section["part_of"], "shop :: HISTORY.rst");
    assert_eq!(section["id"], outline["members"][0]["id"]);
    let body = section["body"].as_str().unwrap();
    assert!(body.starts_with("Version 1.1.0\n-------------\n"), "{body}");
    assert!(!body.contains("Version 1.0.0"), "{body}");
    assert!(
        text.contains("this is the text of the section Version 1.1.0 of shop :: HISTORY.rst"),
        "{text}"
    );
    assert!(text.contains("\nsource_base: ticket="), "{text}");
    let named: Vec<&str> = section["names"]
        .as_array()
        .unwrap_or_else(|| panic!("{section}"))
        .iter()
        .map(|row| row["name"].as_str().unwrap())
        .collect();
    assert_eq!(named, ["init_db", "schema_version"], "{section}");
    let older = artifact_read(&state, "shop.HISTORY :: Version 1.0.0").await;

    // A patch to it lands, and nothing else of the text changes.
    let (result, text) = edit_path_mutate(
        &state,
        &session,
        vec![edit_path_patch(
            &section,
            &[("Unreleased\n", "Released 2026-02-01\n")],
        )],
    )
    .await;
    assert_ne!(result.is_error, Some(true), "{text}");
    assert_eq!(
        text,
        "Changed shop.HISTORY :: Version 1.1.0; the graph now holds it."
    );
    let patched = history.replacen("Unreleased\n", "Released 2026-02-01\n", 1);
    assert_eq!(edit_path_disk(&dir, "HISTORY.rst"), patched);
    let again = artifact_read(&state, "shop.HISTORY :: Version 1.1.0").await;
    assert_eq!(again["id"], section["id"]);

    // The other section's ticket, from before that change, still holds.
    let (result, text) = edit_path_mutate(
        &state,
        &session,
        vec![edit_path_patch(
            &older,
            &[("- Entry 079: one line", "- Entry 079: the last line")],
        )],
    )
    .await;
    assert_ne!(result.is_error, Some(true), "{text}");
    let patched = patched.replace("- Entry 079: one line", "- Entry 079: the last line");
    assert_eq!(edit_path_disk(&dir, "HISTORY.rst"), patched);

    // A ticket of a section that changed since is refused.
    let (result, text) = edit_path_mutate(
        &state,
        &session,
        vec![edit_path_patch(&section, &[("Version 1.1.0\n", "Version 1.2.0\n")])],
    )
    .await;
    assert_eq!(result.is_error, Some(true), "{text}");
    assert_eq!(edit_path_disk(&dir, "HISTORY.rst"), patched);

    // locate finds a section by its title.
    let (_, text) = artifact_call(
        &state,
        "locate",
        serde_json::json!({"query": "Version 1.0.0", "mode": "name"}),
    )
    .await;
    assert!(
        artifact_rows(&text, "results")
            .iter()
            .any(|row| row.starts_with("shop.HISTORY :: Version 1.0.0  (section)")),
        "{text}"
    );

    // A text with parts that fits in a class's bound is read whole.
    let (result, text) = artifact_call(
        &state,
        "source",
        serde_json::json!({"entity_id": "shop :: README.md"}),
    )
    .await;
    assert_ne!(result.is_error, Some(true), "{text}");
    let readme = tool_result_payload(&result);
    assert_eq!(readme["body"], ARTIFACT_README, "{readme}");
    assert_eq!(readme["held_as"], "parts", "{readme}");
    assert!(readme.get("parts").is_none(), "{readme}");
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
    std::fs::write(dir.path().join("src/shop/static/data.blob"), [0u8, 0xFF, 1, 2]).unwrap();
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
