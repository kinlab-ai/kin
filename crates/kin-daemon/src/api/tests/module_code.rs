// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

const MODULE_CODE_UTIL: &str = "\"\"\"Utilities.\"\"\"\n\n\ndef helper(value):\n    return value * 2\n\n\ndef other(value):\n    return value + 1\n";

const MODULE_CODE_APP: &str = "\"\"\"Application entry.\"\"\"\nimport os\n\nfrom pkg.util import other\n\nLIMIT = 3\n__all__ = [\"run\"]\n\n\ndef run(value):\n    return other(value) + LIMIT\n\n\nclass Runner:\n    \"\"\"Runs.\"\"\"\n\n    def go(self):\n        return run(os.getpid())\n";

/// A store holding a small Python package: `pkg/util.py` and `pkg/app.py`.
async fn module_code_fixture() -> (tempfile::TempDir, Arc<DaemonState>) {
    let (dir, state) = mcp_lifecycle_fixture();
    for (path, body) in [
        ("pkg/__init__.py", "\"\"\"The package.\"\"\"\n"),
        ("pkg/util.py", MODULE_CODE_UTIL),
        ("pkg/app.py", MODULE_CODE_APP),
    ] {
        source_tree_conversion_fixture(
            &state,
            serde_json::json!({
                "verb": "create", "target": path, "body": body,
                "description": "python module fixture"
            }),
        )
        .await;
    }
    (dir, state)
}

/// The module surface of the file at `path`.
fn module_code_surface(state: &DaemonState, path: &str) -> kin_model::Entity {
    state
        .graph
        .query_entities(&kin_model::EntityFilter {
            file_path: Some(kin_model::FilePathId::new(path)),
            ..Default::default()
        })
        .unwrap()
        .into_iter()
        .find(kin_model::is_file_module_surface)
        .unwrap_or_else(|| panic!("{path} has no module surface"))
}

/// The one entity named `name` declared in the file at `path`.
fn module_code_entity(state: &DaemonState, path: &str, name: &str) -> kin_model::Entity {
    state
        .graph
        .query_entities(&kin_model::EntityFilter {
            file_path: Some(kin_model::FilePathId::new(path)),
            ..Default::default()
        })
        .unwrap()
        .into_iter()
        .find(|entity| entity.name == name && !kin_model::is_file_module_surface(entity))
        .unwrap_or_else(|| panic!("{path} declares no {name}"))
}

/// One public `source` read over the served route, with its arguments.
async fn module_code_read_with(
    state: &Arc<DaemonState>,
    arguments: serde_json::Value,
) -> kin_mcp::ToolCallResult {
    let response = router(Arc::clone(state))
        .oneshot(
            Request::post("/mcp/tools/call")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "name": "get_entity_source", "arguments": arguments })
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&body).unwrap()
}

/// The public `source` reply for the module `module`: its own code and the
/// ticket a patch carries back.
async fn module_code_read(
    state: &Arc<DaemonState>,
    module: &kin_model::Entity,
) -> serde_json::Value {
    let result = module_code_read_with(state, serde_json::json!({"entity_id": module.id})).await;
    assert_ne!(result.is_error, Some(true), "{}", mcp_result_text(&result));
    let reply = tool_result_payload(&result);
    assert!(
        kin_mcp::source_ticket::is_ticket(&reply["source_base"]),
        "a module-code read serves the ticket a patch carries: {reply}"
    );
    reply
}

fn module_code_patch(read: &serde_json::Value, edits: &[(&str, &str)]) -> serde_json::Value {
    serde_json::json!({
        "verb": "patch", "target": read["id"], "description": "change module-level code",
        "payload": {"EntitySourcePatch": {
            "source_base": read["source_base"],
            "edits": edits.iter().map(|(old, new)| serde_json::json!({"old_text": old, "new_text": new})).collect::<Vec<_>>()
        }}
    })
}

/// One `mutate` over the served route. It carries a request id of its own,
/// which is what takes a mutate to the daemon's commit path on this route: an
/// unkeyed one is answered by the in-process handler, which writes no source.
async fn module_code_mutate(
    state: &Arc<DaemonState>,
    session: &str,
    operations: Vec<serde_json::Value>,
    summary: &str,
) -> kin_mcp::ToolCallResult {
    mutate_http(
        state,
        "kin_mutate",
        serde_json::json!({
            "session_id": session,
            "request_id": format!("module-code-{}", Uuid::new_v4()),
            "operations": operations,
            "summary": summary,
        }),
        session,
    )
    .await
}

/// Whether the graph holds the import of `target` by the module `module` in
/// the form the linker stores a Python `from x import name`: an `Imports`
/// relation from the module entity to the imported entity, whose evidence is
/// the specifier binding.
fn module_code_imports(
    state: &DaemonState,
    module: &kin_model::Entity,
    target: &kin_model::Entity,
) -> bool {
    state
        .graph
        .get_relations(&module.id, &[kin_model::RelationKind::Imports])
        .unwrap()
        .iter()
        .any(|relation| {
            relation.src == kin_model::GraphNodeId::Entity(module.id)
                && relation.dst == kin_model::GraphNodeId::Entity(target.id)
                && relation.evidence.iter().any(|evidence| {
                    evidence.parser_rule.as_deref() == Some("import_specifier_binding")
                        && evidence.token.as_deref() == Some(target.name.as_str())
                })
        })
}

fn module_code_disk(dir: &tempfile::TempDir, path: &str) -> String {
    std::fs::read_to_string(dir.path().join(path)).unwrap()
}

/// The whole path an agent takes: read a module's own code with `source`,
/// then add an import, remove one and change a constant with `mutate`, each
/// against the ticket of a fresh read. Every change reaches the working file
/// byte for byte and the graph in the same commit.
#[tokio::test]
#[serial_test::serial]
async fn module_code_adds_and_removes_imports_and_changes_a_constant_on_disk_and_in_the_graph() {
    let (dir, state) = module_code_fixture().await;
    let module = module_code_surface(&state, "pkg/app.py");
    let helper = module_code_entity(&state, "pkg/util.py", "helper");
    let other = module_code_entity(&state, "pkg/util.py", "other");
    let run = module_code_entity(&state, "pkg/app.py", "run");
    let runner = module_code_entity(&state, "pkg/app.py", "Runner");
    let session = mcp_test_session(&state);

    // The read is the module's own code, with each function and class as one
    // marker and no line of their bodies.
    let read = module_code_read(&state, &module).await;
    let short = |entity: &kin_model::Entity| entity.id.to_string()[..8].to_string();
    assert_eq!(
        read["body"],
        format!(
            "\"\"\"Application entry.\"\"\"\nimport os\n\nfrom pkg.util import other\n\nLIMIT = 3\n\
             __all__ = [\"run\"]\n\n\n\
             # kin:entity function run #{}\n\n\n\
             # kin:entity class Runner #{}\n",
            short(&run),
            short(&runner)
        ),
        "{read}"
    );
    assert_eq!(read["kind"], "module", "{read}");
    assert_eq!(read["module_code"]["entities"], 2, "{read}");
    assert!(module_code_imports(&state, &module, &other));
    assert!(!module_code_imports(&state, &module, &helper));

    // Add an import.
    let added = module_code_mutate(
        &state,
        &session,
        vec![module_code_patch(
            &read,
            &[(
                "from pkg.util import other\n",
                "from pkg.util import helper\nfrom pkg.util import other\n",
            )],
        )],
        "Import helper into the app module",
    )
    .await;
    assert_ne!(added.is_error, Some(true), "{}", mcp_result_text(&added));
    let with_helper = MODULE_CODE_APP.replace(
        "from pkg.util import other\n",
        "from pkg.util import helper\nfrom pkg.util import other\n",
    );
    assert_eq!(module_code_disk(&dir, "pkg/app.py"), with_helper);
    assert!(
        module_code_imports(&state, &module, &helper),
        "the commit that wrote the import holds its import relation"
    );
    assert!(module_code_imports(&state, &module, &other));

    // The ticket of the first read names source that has since changed.
    let stale = module_code_mutate(
        &state,
        &session,
        vec![module_code_patch(&read, &[("LIMIT = 3", "LIMIT = 4")])],
        "A patch against a stale read",
    )
    .await;
    assert_eq!(stale.is_error, Some(true), "{}", mcp_result_text(&stale));
    assert!(
        mcp_result_text(&stale).contains("source_base_changed"),
        "{}",
        mcp_result_text(&stale)
    );
    assert_eq!(module_code_disk(&dir, "pkg/app.py"), with_helper);

    // Remove an import, against a fresh read.
    let read = module_code_read(&state, &module).await;
    let removed = module_code_mutate(
        &state,
        &session,
        vec![module_code_patch(
            &read,
            &[("from pkg.util import helper\n", "")],
        )],
        "Drop the helper import",
    )
    .await;
    assert_ne!(
        removed.is_error,
        Some(true),
        "{}",
        mcp_result_text(&removed)
    );
    assert_eq!(module_code_disk(&dir, "pkg/app.py"), MODULE_CODE_APP);
    assert!(
        !module_code_imports(&state, &module, &helper),
        "the commit that removed the import retired its import relation"
    );
    assert!(module_code_imports(&state, &module, &other));

    // Change a module-level constant and `__all__`, in one patch.
    let limit = module_code_entity(&state, "pkg/app.py", "LIMIT");
    let read = module_code_read(&state, &module).await;
    let changed = module_code_mutate(
        &state,
        &session,
        vec![module_code_patch(
            &read,
            &[
                ("LIMIT = 3", "LIMIT = 5"),
                ("__all__ = [\"run\"]", "__all__ = [\"run\", \"Runner\"]"),
            ],
        )],
        "Raise the limit and export Runner",
    )
    .await;
    assert_ne!(
        changed.is_error,
        Some(true),
        "{}",
        mcp_result_text(&changed)
    );
    let expected = MODULE_CODE_APP
        .replace("LIMIT = 3", "LIMIT = 5")
        .replace("[\"run\"]", "[\"run\", \"Runner\"]");
    assert_eq!(module_code_disk(&dir, "pkg/app.py"), expected);
    let reparsed = module_code_entity(&state, "pkg/app.py", "LIMIT");
    assert_eq!(reparsed.id, limit.id, "the constant keeps its identity");
    assert_ne!(reparsed.fingerprint, limit.fingerprint);
    let constant = entity_patch_read(&state, &serde_json::json!(limit.id)).await;
    assert_eq!(constant["body"], "LIMIT = 5");
    // Every entity the module holds is the entity it was, byte for byte.
    for (entity, body) in [
        (&run, "def run(value):\n    return other(value) + LIMIT"),
        (
            &runner,
            "class Runner:\n    \"\"\"Runs.\"\"\"\n\n    def go(self):\n        return run(os.getpid())",
        ),
    ] {
        assert_eq!(
            entity_patch_read(&state, &serde_json::json!(entity.id)).await["body"],
            body
        );
    }
    // Repository authority holds exactly the bytes the working file does.
    let reopened = module_code_read(&state, &module).await;
    assert!(reopened["body"]
        .as_str()
        .unwrap()
        .contains("LIMIT = 5\n__all__ = [\"run\", \"Runner\"]\n"));
}

/// Every refusal leaves the working file and repository authority as they
/// were, and says in one sentence what to send instead.
#[tokio::test]
#[serial_test::serial]
async fn module_code_refusals_write_nothing_and_say_what_to_send() {
    let (dir, state) = module_code_fixture().await;
    let module = module_code_surface(&state, "pkg/app.py");
    let run = module_code_entity(&state, "pkg/app.py", "run");
    let session = mcp_test_session(&state);
    let read = module_code_read(&state, &module).await;
    let roots = source_base_roots(&state);
    let marker = read["body"]
        .as_str()
        .unwrap()
        .lines()
        .find(|line| line.starts_with("# kin:entity function run "))
        .unwrap()
        .to_string();

    for (edits, expected) in [
        (
            vec![("return other(value) + LIMIT", "return other(value)")],
            format!(
                "patch edit #0: old_text is inside function run, which the module-level code of \
                 app shows only as a kin:entity line, so read run with source (entity_id {}) and \
                 patch it with the source_base that read returns",
                run.id
            ),
        ),
        (
            vec![("import sys", "import re")],
            "patch edit #0: old_text is not in the module-level code of app, so copy it exactly \
             from the body source returned for app, or read app again with source"
                .to_string(),
        ),
        (
            vec![("import", "from")],
            "patch edit #0: old_text occurs 2 times in the module-level code of app, so extend \
             it with the text beside the one you mean until it occurs once"
                .to_string(),
        ),
        (
            vec![(marker.as_str(), "def run(value):\n    return 0")],
            format!(
                "the patch changes or removes the kin:entity line that stands for function run, \
                 which is not code of app, so keep that line exactly as source returned it and \
                 change run by its own id (entity_id {}): source it and patch it, or remove it \
                 with EntityRemove",
                run.id
            ),
        ),
        (
            vec![("LIMIT = 3", "LIMIT = 3\n\n\ndef extra():\n    return 1")],
            "the patch to module app would declare function extra in its module-level code, \
             and a module patch adds statements, never a function or class, so create extra \
             with EntityCreate"
                .to_string(),
        ),
        (
            vec![("LIMIT = 3", "LIMIT = (3")],
            "the module-level code of app as patched is not valid Python, `LIMIT = (3`: this \
             does not parse"
                .to_string(),
        ),
    ] {
        let refused = module_code_mutate(
            &state,
            &session,
            vec![module_code_patch(&read, &edits)],
            "A refused module patch",
        )
        .await;
        let text = mcp_result_text(&refused);
        assert_eq!(refused.is_error, Some(true), "{edits:?}: {text}");
        assert!(
            text.contains(&expected),
            "{edits:?}\nwanted: {expected}\n   got: {text}"
        );
        assert_eq!(module_code_disk(&dir, "pkg/app.py"), MODULE_CODE_APP);
        assert_eq!(
            source_base_roots(&state),
            roots,
            "{edits:?} moved authority"
        );
    }

    // A module is never replaced whole, with the base its own read served.
    let replaced = module_code_mutate(
        &state,
        &session,
        vec![serde_json::json!({
            "verb": "update", "target": read["id"], "body": "import os\n",
            "payload": {"EntitySourceBase": read["source_base"]},
            "description": "replace the module"
        })],
        "A whole-module overwrite",
    )
    .await;
    let text = mcp_result_text(&replaced);
    assert_eq!(replaced.is_error, Some(true), "{text}");
    assert!(text.contains("module_body_unavailable"), "{text}");
    assert!(text.contains("change that code with a patch"), "{text}");
    assert_eq!(module_code_disk(&dir, "pkg/app.py"), MODULE_CODE_APP);
    assert_eq!(source_base_roots(&state), roots);

    // The same read still patches: nothing above consumed or moved its base.
    let landed = module_code_mutate(
        &state,
        &session,
        vec![module_code_patch(&read, &[("LIMIT = 3", "LIMIT = 4")])],
        "Raise the limit",
    )
    .await;
    assert_ne!(landed.is_error, Some(true), "{}", mcp_result_text(&landed));
    assert_eq!(
        module_code_disk(&dir, "pkg/app.py"),
        MODULE_CODE_APP.replace("LIMIT = 3", "LIMIT = 4")
    );
}

/// A module whose file is larger than one reply serves, and holds many
/// entities, is read as its own code alone: no reply holds an entity's body.
/// Its code is patched like any other.
#[tokio::test]
#[serial_test::serial]
async fn module_code_of_a_large_module_never_serves_an_entity_body() {
    let (dir, state) = mcp_lifecycle_fixture();
    let mut source =
        String::from("\"\"\"Generated handlers.\"\"\"\nimport os\nimport sys\n\nREGISTRY = {}\n\n");
    for index in 0..200 {
        source.push_str(&format!(
            "\ndef handler_{index:03}(request):\n    payload = request.json()\n    if payload is \
             None:\n        raise ValueError(\"handler {index:03} needs a body, and this \
             sentence pads the function to a realistic length for a handler\")\n    for attempt \
             in range(3):\n        os.write(1, b\"BODY-{index:03}\")\n    return {{\"handler\": \
             {index}, \"ok\": True, \"payload\": payload}}\n\n\n\
             REGISTRY[\"{index:03}\"] = handler_{index:03}\n"
        ));
    }
    source.push_str(
        "\n\nclass Dispatcher:\n    def dispatch(self, name, request):\n        return \
         REGISTRY[name](request)\n",
    );
    assert!(source.len() > 60 * 1024, "{} bytes", source.len());
    source_tree_conversion_fixture(
        &state,
        serde_json::json!({
            "verb": "create", "target": "pkg/handlers.py", "body": source,
            "description": "large python module fixture"
        }),
    )
    .await;
    let module = module_code_surface(&state, "pkg/handlers.py");
    let bodies = [
        "request.json()",
        "BODY-",
        "raise ValueError",
        "REGISTRY[name](request)",
        "os.write",
    ];

    // The daemon answers with the module-level code alone. (The MCP server
    // pages a body over 12 KB on its way to a client, as it does any body.)
    let whole = module_code_read_with(&state, serde_json::json!({"entity_id": module.id})).await;
    assert_ne!(whole.is_error, Some(true), "{}", mcp_result_text(&whole));
    let text = mcp_result_text(&whole);
    for body in bodies {
        assert!(
            !text.contains(body),
            "the read holds an entity body: {body}"
        );
    }
    let whole = tool_result_payload(&whole);
    let code = whole["body"].as_str().unwrap();
    assert_eq!(whole["module_code"]["entities"], 201, "{whole}");
    assert_eq!(code.matches("# kin:entity ").count(), 201);
    assert_eq!(code.matches("REGISTRY[\"").count(), 200);
    assert!(code.starts_with(
        "\"\"\"Generated handlers.\"\"\"\nimport os\nimport sys\n\nREGISTRY = {}\n\n\n# kin:entity function handler_000 #"
    ));
    assert!(
        code.len() < source.len() / 2,
        "{} of {}",
        code.len(),
        source.len()
    );
    assert!(
        kin_mcp::source_ticket::is_ticket(&whole["source_base"]),
        "{whole}"
    );

    // A change at the top and one between two functions land byte for byte.
    let session = mcp_test_session(&state);
    let patched = module_code_mutate(
        &state,
        &session,
        vec![module_code_patch(
            &whole,
            &[
                ("import sys\n", "import sys\nimport json\n"),
                (
                    "REGISTRY[\"074\"] = handler_074",
                    "REGISTRY[\"seventy-four\"] = handler_074",
                ),
            ],
        )],
        "Import json and rename one registration",
    )
    .await;
    assert_ne!(
        patched.is_error,
        Some(true),
        "{}",
        mcp_result_text(&patched)
    );
    assert_eq!(
        module_code_disk(&dir, "pkg/handlers.py"),
        source
            .replace("import sys\n", "import sys\nimport json\n")
            .replace(
                "REGISTRY[\"074\"] = handler_074",
                "REGISTRY[\"seventy-four\"] = handler_074"
            )
    );
}

/// An entity edit and a patch to its module's own code commit in one
/// transaction: the function gains a call and the module gains its import.
#[tokio::test]
#[serial_test::serial]
async fn module_code_patch_and_entity_patch_of_one_module_commit_together() {
    let (dir, state) = module_code_fixture().await;
    let module = module_code_surface(&state, "pkg/app.py");
    let helper = module_code_entity(&state, "pkg/util.py", "helper");
    let run = module_code_entity(&state, "pkg/app.py", "run");
    let session = mcp_test_session(&state);
    let read = module_code_read(&state, &module).await;
    let function = entity_patch_read(&state, &serde_json::json!(run.id)).await;
    let result = module_code_mutate(
        &state,
        &session,
        vec![
            entity_patch_operation_public(&function, &[("other(value)", "helper(other(value))")]),
            module_code_patch(
                &read,
                &[(
                    "from pkg.util import other\n",
                    "from pkg.util import helper, other\n",
                )],
            ),
        ],
        "Call helper from run and import it",
    )
    .await;
    assert_ne!(result.is_error, Some(true), "{}", mcp_result_text(&result));
    assert_eq!(
        module_code_disk(&dir, "pkg/app.py"),
        MODULE_CODE_APP
            .replace(
                "from pkg.util import other\n",
                "from pkg.util import helper, other\n"
            )
            .replace(
                "return other(value) + LIMIT",
                "return helper(other(value)) + LIMIT"
            )
    );
    assert!(module_code_imports(&state, &module, &helper));
}

/// A patch built from a public read, ticket and all.
fn entity_patch_operation_public(
    source: &serde_json::Value,
    edits: &[(&str, &str)],
) -> serde_json::Value {
    serde_json::json!({
        "verb": "patch", "target": source["id"], "description": "exact anchored entity edit",
        "payload": {"EntitySourcePatch": {
            "source_base": source["source_base"],
            "edits": edits.iter().map(|(old, new)| serde_json::json!({"old_text": old, "new_text": new})).collect::<Vec<_>>()
        }}
    })
}

/// A module with no code yet reads as empty code with a ticket, and one edit
/// with an empty anchor gives it its first code. Once it holds code, an
/// empty anchor is refused and the code is anchored on like any other.
#[tokio::test]
#[serial_test::serial]
async fn module_code_gives_an_empty_module_its_first_code() {
    let (dir, state) = module_code_fixture().await;
    // An empty body is not a conversion operation, so the file is installed
    // the way the commit tests install source.
    let (module, _) =
        crate::mcp_commit::tests::install_exact_source(&state, "pkg/empty.py", b"", "empty");
    assert!(kin_model::is_file_module_surface(&module), "{module:?}");
    let helper = module_code_entity(&state, "pkg/util.py", "helper");
    let session = mcp_test_session(&state);

    let read = module_code_read(&state, &module).await;
    assert_eq!(read["body"], "", "{read}");
    assert_eq!(read["module_code"]["empty"], true, "{read}");
    assert_eq!(read["module_code"]["entities"], 0, "{read}");

    let landed = module_code_mutate(
        &state,
        &session,
        vec![module_code_patch(
            &read,
            &[("", "from pkg.util import helper\n")],
        )],
        "Give the empty module its first import",
    )
    .await;
    assert_ne!(landed.is_error, Some(true), "{}", mcp_result_text(&landed));
    assert_eq!(
        module_code_disk(&dir, "pkg/empty.py"),
        "from pkg.util import helper\n"
    );
    let module = module_code_surface(&state, "pkg/empty.py");
    assert!(module_code_imports(&state, &module, &helper));

    // It now holds code: the read serves it, and an empty anchor is refused.
    let read = module_code_read(&state, &module).await;
    assert_eq!(read["body"], "from pkg.util import helper\n", "{read}");
    assert!(read["module_code"].get("empty").is_none(), "{read}");
    let refused = module_code_mutate(
        &state,
        &session,
        vec![module_code_patch(&read, &[("", "import os\n")])],
        "An empty anchor on a module that holds code",
    )
    .await;
    let text = mcp_result_text(&refused);
    assert_eq!(refused.is_error, Some(true), "{text}");
    assert!(
        text.contains(
            "patch edit #0: old_text must be nonempty, since an empty old_text is only the first \
             code of a module that holds none, so anchor it on text copied from the source read"
        ),
        "{text}"
    );
    assert_eq!(
        module_code_disk(&dir, "pkg/empty.py"),
        "from pkg.util import helper\n"
    );

    // An entity body never takes an empty anchor.
    let function = entity_patch_read(&state, &serde_json::json!(helper.id)).await;
    let refused = module_code_mutate(
        &state,
        &session,
        vec![entity_patch_operation_public(&function, &[("", "x = 1\n")])],
        "An empty anchor on a function",
    )
    .await;
    assert_eq!(
        refused.is_error,
        Some(true),
        "{}",
        mcp_result_text(&refused)
    );
    assert!(
        mcp_result_text(&refused).contains("old_text must be nonempty"),
        "{}",
        mcp_result_text(&refused)
    );
    assert_eq!(module_code_disk(&dir, "pkg/util.py"), MODULE_CODE_UTIL);
}

/// A module's ticket guards its own code and nothing wider. It holds through
/// an edit to the body of a function in the module, and through a commit to
/// another module, and goes stale once the module's own code has changed.
#[tokio::test]
#[serial_test::serial]
async fn module_code_ticket_holds_until_the_module_s_own_code_changes() {
    let (dir, state) = module_code_fixture().await;
    let module = module_code_surface(&state, "pkg/app.py");
    let run = module_code_entity(&state, "pkg/app.py", "run");
    let helper = module_code_entity(&state, "pkg/util.py", "helper");
    let session = mcp_test_session(&state);
    let first = module_code_read(&state, &module).await;

    // A function in the module grows by a line, and one in another module
    // changes: neither is the module's own code.
    for (entity, old, new) in [
        (
            &run,
            "return other(value) + LIMIT",
            "total = other(value)\n    return total + LIMIT",
        ),
        (&helper, "value * 2", "value * 3"),
    ] {
        let function = entity_patch_read(&state, &serde_json::json!(entity.id)).await;
        let edited = module_code_mutate(
            &state,
            &session,
            vec![entity_patch_operation_public(&function, &[(old, new)])],
            "Edit a function body",
        )
        .await;
        assert_ne!(edited.is_error, Some(true), "{}", mcp_result_text(&edited));
    }
    let after_edits = module_code_read(&state, &module).await;
    assert_eq!(after_edits["body"], first["body"]);

    // The ticket read before both edits still patches the module's code.
    let landed = module_code_mutate(
        &state,
        &session,
        vec![module_code_patch(&first, &[("LIMIT = 3", "LIMIT = 7")])],
        "Raise the limit with the first read's ticket",
    )
    .await;
    assert_ne!(landed.is_error, Some(true), "{}", mcp_result_text(&landed));
    let expected = MODULE_CODE_APP.replace("LIMIT = 3", "LIMIT = 7").replace(
        "return other(value) + LIMIT",
        "total = other(value)\n    return total + LIMIT",
    );
    assert_eq!(module_code_disk(&dir, "pkg/app.py"), expected);

    // Now the module's own code has changed, and that ticket is stale.
    let stale = module_code_mutate(
        &state,
        &session,
        vec![module_code_patch(
            &first,
            &[("__all__ = [\"run\"]", "__all__ = []")],
        )],
        "A patch against code that has changed",
    )
    .await;
    assert_eq!(stale.is_error, Some(true), "{}", mcp_result_text(&stale));
    assert!(
        mcp_result_text(&stale).contains("source_base_changed"),
        "{}",
        mcp_result_text(&stale)
    );
    assert_eq!(module_code_disk(&dir, "pkg/app.py"), expected);
}

include!("edit_path.rs");
include!("language_conformance.rs");
