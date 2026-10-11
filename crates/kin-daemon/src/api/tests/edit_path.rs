// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

// The edit path as an agent drives it: `source` and `mutate` over the served
// route, each reply presented the way the default agent profile reads it, as
// compact text.

const EDIT_PATH_SHAPES: &str = "\"\"\"Shapes.\"\"\"\nimport math\n\n\ndef area(radius):\n    return math.pi * radius * radius\n\n\nclass Circle:\n    \"\"\"A circle.\"\"\"\n\n    sides = 0\n\n    def __init__(self, radius):\n        self.radius = radius\n\n    def area(self):\n        if self.radius < 0:\n            return 0\n        return area(self.radius)\n\n    def name(self):\n        return \"circle\"\n\n    def twice(self):\n        if self.radius:\n            pass\n        if self.radius:\n            pass\n        return 2\n";

const EDIT_PATH_SHAPES_RS: &str = "pub fn area(radius: f64) -> f64 {\n    3.14 * radius * radius\n}\n\npub struct Circle {\n    pub radius: f64,\n}\n\nimpl Circle {\n    pub fn area(&self) -> f64 {\n        if self.radius < 0.0 {\n            return 0.0;\n        }\n        area(self.radius)\n    }\n\n    pub fn name(&self) -> &'static str {\n        \"circle\"\n    }\n}\n";

const EDIT_PATH_SHAPES_TS: &str = "export function area(radius: number): number {\n  return Math.PI * radius * radius;\n}\n\nexport class Circle {\n  radius: number;\n\n  constructor(radius: number) {\n    this.radius = radius;\n  }\n\n  area(): number {\n    if (this.radius < 0) {\n      return 0;\n    }\n    return area(this.radius);\n  }\n\n  name(): string {\n    return \"circle\";\n  }\n}\n";

/// A store holding one Python, one TypeScript and one Rust module.
async fn edit_path_fixture() -> (tempfile::TempDir, Arc<DaemonState>) {
    let (dir, state) = mcp_lifecycle_fixture();
    for (path, body) in [
        ("pkg/__init__.py", "\"\"\"The package.\"\"\"\n"),
        ("pkg/shapes.py", EDIT_PATH_SHAPES),
        ("web/shapes.ts", EDIT_PATH_SHAPES_TS),
        ("src/shapes.rs", EDIT_PATH_SHAPES_RS),
    ] {
        source_tree_conversion_fixture(
            &state,
            serde_json::json!({
                "verb": "create", "target": path, "body": body,
                "description": "edit path fixture"
            }),
        )
        .await;
    }
    (dir, state)
}

/// What an agent on the default profile reads for one finished call: the
/// route's finalized result, presented in plain words and written as text.
fn edit_path_agent_text(
    tool: &str,
    arguments: &serde_json::Value,
    result: &kin_mcp::ToolCallResult,
) -> String {
    let config = kin_mcp::McpServerConfig {
        plain_replies: true,
        agent_belt: true,
        reply_format: kin_mcp::text_reply::ReplyFormat::Text,
        session_handles: Some(Arc::new(kin_mcp::session_handles::SessionHandles::default())),
        ..Default::default()
    };
    let request = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": tool, "arguments": arguments}})
    .to_string();
    let response = kin_mcp::JsonRpcResponse::success(
        Some(serde_json::json!(1)),
        serde_json::to_value(result).unwrap(),
    );
    kin_mcp::present_for_client(response, &request, &config)
        .result
        .expect("a tool call is answered with a result")["content"][0]["text"]
        .as_str()
        .expect("a text block")
        .to_string()
}

/// One call of `name` over the served route, with room for a large reply.
async fn edit_path_call(
    state: &Arc<DaemonState>,
    name: &str,
    arguments: serde_json::Value,
) -> kin_mcp::ToolCallResult {
    let response = router(Arc::clone(state))
        .oneshot(
            Request::post("/mcp/tools/call")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "name": name, "arguments": arguments }).to_string(),
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

/// One `mutate` of `operations` the way an agent's call reaches the daemon,
/// and the text the agent reads for it.
///
/// A `mutate` with no request id of its own is a transaction begun and
/// committed with its operations inline, and aborted when the commit is
/// refused (`kin_mcp::handlers::sessions::mutate_through`). The same three
/// calls are made here over the served route.
async fn edit_path_mutate(
    state: &Arc<DaemonState>,
    session: &str,
    operations: Vec<serde_json::Value>,
) -> (kin_mcp::ToolCallResult, String) {
    let transaction = mcp_lifecycle_begin(state, session).await;
    let arguments = serde_json::json!({
        "session_id": session,
        "operations": operations,
        "summary": "edit path",
    });
    let result = edit_path_call(
        state,
        "kin_transaction_commit",
        serde_json::json!({
            "transaction_id": transaction,
            "session_id": session,
            "operations": arguments["operations"],
            "message": "edit path",
        }),
    )
    .await;
    if result.is_error == Some(true) {
        edit_path_call(
            state,
            "kin_transaction_abort",
            serde_json::json!({"transaction_id": transaction, "session_id": session}),
        )
        .await;
    }
    let text = edit_path_agent_text("mutate", &arguments, &result);
    (result, text)
}

/// The machine `source` reply for `entity_id`, with the ticket a write
/// carries back.
async fn edit_path_read(
    state: &Arc<DaemonState>,
    entity_id: &serde_json::Value,
) -> serde_json::Value {
    let result = edit_path_call(
        state,
        "get_entity_source",
        serde_json::json!({"entity_id": entity_id}),
    )
    .await;
    assert_ne!(result.is_error, Some(true), "{}", mcp_result_text(&result));
    tool_result_payload(&result)
}

/// A guarded whole-code `update` of the entity a `source` read returned.
fn edit_path_update(read: &serde_json::Value, code: &str) -> serde_json::Value {
    serde_json::json!({
        "verb": "update", "target": read["id"], "body": code,
        "payload": {"EntitySourceBase": read["source_base"]},
        "description": "rewrite the entity"
    })
}

/// A guarded `patch` of the entity a `source` read returned.
fn edit_path_patch(read: &serde_json::Value, edits: &[(&str, &str)]) -> serde_json::Value {
    serde_json::json!({
        "verb": "patch", "target": read["id"], "description": "change the entity",
        "payload": {"EntitySourcePatch": {
            "source_base": read["source_base"],
            "edits": edits.iter().map(|(old, new)| serde_json::json!({"old_text": old, "new_text": new})).collect::<Vec<_>>()
        }}
    })
}

/// A `create` of `kind` `name` beside the entity a `source` read returned.
fn edit_path_create(
    read: &serde_json::Value,
    placement: &str,
    kind: &str,
    name: &str,
    code: &str,
) -> serde_json::Value {
    serde_json::json!({
        "verb": "create", "target": read["id"], "description": "add a declaration",
        "payload": {"EntityCreate": {
            "source_base": read["source_base"], "placement": placement,
            "name": name, "kind": kind, "body": code
        }}
    })
}

fn edit_path_disk(dir: &tempfile::TempDir, path: &str) -> String {
    std::fs::read_to_string(dir.path().join(path)).unwrap()
}

/// A method's whole code lands at the method's indentation whichever of the
/// three ways the agent wrote it: at the margin, indented as a whole, or as
/// `source` served it. Python, where a body at its header's indentation is
/// not a body, and TypeScript and Rust, where it only looks wrong.
#[tokio::test]
#[serial_test::serial]
async fn edit_path_whole_code_lands_at_the_entity_indentation_however_it_was_written() {
    let (dir, state) = edit_path_fixture().await;
    let session = mcp_test_session(&state);
    for (path, name, label, original, held, forms) in [
        (
            "pkg/shapes.py",
            "Circle.area",
            "pkg.shapes :: Circle.area",
            EDIT_PATH_SHAPES,
            "    def area(self):\n        if self.radius < 0:\n            return 0\n        return area(self.radius)\n",
            [
                ("at the margin", "def area(self):\n    if self.radius <= 0:\n        return 0.5\n    return area(self.radius)", "0.5"),
                ("indented as a whole", "    def area(self):\n        if self.radius <= 0:\n            return 0.25\n        return area(self.radius)\n", "0.25"),
                ("as source served it", "def area(self):\n        if self.radius <= 0:\n            return 0.125\n        return area(self.radius)", "0.125"),
            ],
        ),
        (
            "web/shapes.ts",
            "Circle.area",
            "shapes :: Circle.area",
            EDIT_PATH_SHAPES_TS,
            "  area(): number {\n    if (this.radius < 0) {\n      return 0;\n    }\n    return area(this.radius);\n  }\n",
            [
                ("at the margin", "area(): number {\n  if (this.radius <= 0) {\n    return 0.5;\n  }\n  return area(this.radius);\n}", "0.5"),
                ("indented as a whole", "  area(): number {\n    if (this.radius <= 0) {\n      return 0.25;\n    }\n    return area(this.radius);\n  }\n", "0.25"),
                ("as source served it", "area(): number {\n    if (this.radius <= 0) {\n      return 0.125;\n    }\n    return area(this.radius);\n  }", "0.125"),
            ],
        ),
        (
            "src/shapes.rs",
            "Circle::area",
            "shapes :: Circle::area",
            EDIT_PATH_SHAPES_RS,
            "    pub fn area(&self) -> f64 {\n        if self.radius < 0.0 {\n            return 0.0;\n        }\n        area(self.radius)\n    }\n",
            [
                ("at the margin", "pub fn area(&self) -> f64 {\n    if self.radius <= 0.0 {\n        return 0.5;\n    }\n    area(self.radius)\n}", "0.5"),
                ("indented as a whole", "    pub fn area(&self) -> f64 {\n        if self.radius <= 0.0 {\n            return 0.25;\n        }\n        area(self.radius)\n    }\n", "0.25"),
                ("as source served it", "pub fn area(&self) -> f64 {\n        if self.radius <= 0.0 {\n            return 0.125;\n        }\n        area(self.radius)\n    }", "0.125"),
            ],
        ),
    ] {
        assert!(original.contains(held), "{path}: the fixture holds the method");
        let entity = module_code_entity(&state, path, name);
        for (how, code, value) in forms {
            let read = edit_path_read(&state, &serde_json::json!(entity.id)).await;
            let (result, text) =
                edit_path_mutate(&state, &session, vec![edit_path_update(&read, code)]).await;
            assert_ne!(result.is_error, Some(true), "{path} {how}: {text}");
            println!("{path} {how}: {text}");
            assert_eq!(
                text,
                format!("Changed {label}; 1 source unit written; the graph now holds it."),
                "{path} {how}"
            );
            // The method as the source holds it when its lines are placed.
            let placed = held
                .replace("< 0", "<= 0")
                .replace("return 0;", &format!("return {value};"))
                .replace("return 0.0;", &format!("return {value};"))
                .replace("return 0\n", &format!("return {value}\n"));
            assert_eq!(
                edit_path_disk(&dir, path),
                original.replace(held, &placed),
                "{path} {how}"
            );
            // And the graph serves the same code back, from its first character.
            let after = edit_path_read(&state, &serde_json::json!(entity.id)).await;
            assert_eq!(
                after["body"].as_str().unwrap(),
                placed.trim_end().trim_start(),
                "{path} {how}"
            );
        }
    }
}

/// Source that does not parse is never committed. Each refusal is one
/// sentence that says nothing was written, names the problem and the line of
/// it in what the agent sent, and leaves the working source and repository
/// authority as they were.
#[tokio::test]
#[serial_test::serial]
async fn edit_path_code_that_does_not_parse_is_refused_in_one_sentence_and_nothing_is_written() {
    let (dir, state) = edit_path_fixture().await;
    let session = mcp_test_session(&state);
    let roots = source_base_roots(&state);
    for (path, name, original, operation, expected) in [
        (
            "pkg/shapes.py",
            "Circle.area",
            EDIT_PATH_SHAPES,
            (
                "update",
                "def area(self):\n    value = area(self.radius)\n        return value",
                "",
            ),
            "Nothing was written: the code sent for Circle.area is not valid Python at line 3, \
             `return value`: unexpected indent.",
        ),
        (
            "pkg/shapes.py",
            "Circle.area",
            EDIT_PATH_SHAPES,
            (
                "update",
                "def area(self):\n    return (area(self.radius)",
                "",
            ),
            "Nothing was written: the code sent for Circle.area is not valid Python at line 2, \
             `return (area(self.radius)`: this does not parse.",
        ),
        (
            "pkg/shapes.py",
            "Circle.area",
            EDIT_PATH_SHAPES,
            (
                "patch",
                "return area(self.radius)",
                "return area(self.radius",
            ),
            "Nothing was written: Circle.area as patched is not valid Python at line 4, `return \
             area(self.radius`: this does not parse.",
        ),
        (
            "pkg/shapes.py",
            "Circle.area",
            EDIT_PATH_SHAPES,
            (
                "patch",
                "            return 0\n",
                "        return 0\n            pass\n",
            ),
            "Nothing was written: Circle.area as patched is not valid Python at line 3, `return \
             0`: expected an indented block.",
        ),
        (
            "web/shapes.ts",
            "Circle.area",
            EDIT_PATH_SHAPES_TS,
            (
                "update",
                "area(): number {\n  return area(this.radius;\n}",
                "",
            ),
            "Nothing was written: the code sent for Circle.area is not valid TypeScript at line \
             2, `return area(this.radius;`: `)` is missing.",
        ),
        (
            "web/shapes.ts",
            "Circle.area",
            EDIT_PATH_SHAPES_TS,
            (
                "patch",
                "return area(this.radius);",
                "return area(this.radius;",
            ),
            "Nothing was written: Circle.area as patched is not valid TypeScript at line 5, \
             `return area(this.radius;`: `)` is missing.",
        ),
        (
            "src/shapes.rs",
            "Circle::area",
            EDIT_PATH_SHAPES_RS,
            (
                "update",
                "pub fn area(&self) -> f64 {\n    area(self.radius\n}",
                "",
            ),
            "Nothing was written: the code sent for Circle::area is not valid Rust at line 2, \
             `area(self.radius`: `)` is missing.",
        ),
        // A statement dedented out of a method is valid Python where it
        // lands, and would be left where no entity holds it.
        (
            "pkg/shapes.py",
            "Circle.area",
            EDIT_PATH_SHAPES,
            (
                "update",
                "def area(self):\n    value = area(self.radius)\nreturn value",
                "",
            ),
            "Nothing was written: line 3 of the code sent for Circle.area, `return value`, \
             stands outside Circle.area, and an edit holds the one declaration it names, so \
             check that line's indentation and the brackets before it.",
        ),
        (
            "pkg/shapes.py",
            "Circle.area",
            EDIT_PATH_SHAPES,
            ("patch", "        return area(self.radius)", "    total = 1"),
            "Nothing was written: this edit of Circle.area would also add field Circle.total \
             beside it, and an edit changes only the declaration it names. Send the code of \
             Circle.area alone, and add Circle.total with the create verb (payload EntityCreate).",
        ),
        // A new method, a new function beside a function, in Python and Rust.
        (
            "pkg/shapes.py",
            "Circle.area",
            EDIT_PATH_SHAPES,
            (
                "create method perimeter",
                "def perimeter(self):\n    value = 2\n        return value",
                "",
            ),
            "Nothing was written: the code sent for perimeter is not valid Python at line 3, \
             `return value`: unexpected indent.",
        ),
        (
            "pkg/shapes.py",
            "area",
            EDIT_PATH_SHAPES,
            (
                "create function double",
                "def double(radius):\n    return (radius * 2",
                "",
            ),
            "Nothing was written: the code sent for double is not valid Python at line 2, \
             `return (radius * 2`: this does not parse.",
        ),
        (
            "pkg/shapes.py",
            "area",
            EDIT_PATH_SHAPES,
            (
                "create function double",
                "def double(radius):\nreturn radius * 2",
                "",
            ),
            "Nothing was written: the code sent for double is not valid Python at line 2, \
             `return radius * 2`: expected an indented block.",
        ),
        (
            "src/shapes.rs",
            "area",
            EDIT_PATH_SHAPES_RS,
            (
                "create function double",
                "pub fn double(radius: f64) -> f64 {\n    (radius * 2.0\n}",
                "",
            ),
            "Nothing was written: the code sent for double is not valid Rust at line 2, `(radius \
             * 2.0`: `)` is missing.",
        ),
    ] {
        let entity = module_code_entity(&state, path, name);
        let read = edit_path_read(&state, &serde_json::json!(entity.id)).await;
        let operation = match operation {
            ("update", code, _) => edit_path_update(&read, code),
            ("patch", old, new) => edit_path_patch(&read, &[(old, new)]),
            (create, code, _) => {
                let mut words = create.split(' ').skip(1);
                let (kind, name) = (words.next().unwrap(), words.next().unwrap());
                edit_path_create(&read, "sibling_after", kind, name, code)
            }
        };
        let (result, text) = edit_path_mutate(&state, &session, vec![operation]).await;
        println!("{path}: {text}");
        assert_eq!(result.is_error, Some(true), "{path}: {text}");
        assert_eq!(text, expected, "{path}");
        assert!(!text.contains('\n'), "one sentence: {text}");
        assert_eq!(edit_path_disk(&dir, path), original, "{path}");
        assert_eq!(source_base_roots(&state), roots, "{path}");
        // The entity is still the entity it was, with the code it had.
        let after = edit_path_read(&state, &serde_json::json!(entity.id)).await;
        assert_eq!(after["body"], read["body"], "{path}");
    }
}

/// A patch whose anchor was written at another indentation than the code
/// `source` served: at the margin, the way a display that takes a method's own
/// indentation off shows it, or one indentation too deep, the way it reads
/// when that indentation is put back twice. Each fits one place once its
/// lines are moved by the method's indentation, and its replacement lands
/// there with it. An anchor that then fits two places, or none, is refused,
/// and the one that fits none is answered with the code nearest it.
#[tokio::test]
#[serial_test::serial]
async fn edit_path_a_patch_anchor_at_another_indentation_lands_or_is_refused_with_the_code() {
    let (dir, state) = edit_path_fixture().await;
    let session = mcp_test_session(&state);
    let area = module_code_entity(&state, "pkg/shapes.py", "Circle.area");
    let held = "        if self.radius < 0:\n            return 0\n";
    assert!(EDIT_PATH_SHAPES.contains(held));

    let mut expected = EDIT_PATH_SHAPES.to_string();
    for (how, old, new, now) in [
        (
            "at the margin",
            "    if self.radius < 0:\n        return 0\n".to_string(),
            "    if self.radius <= 0:\n        return 0.5\n",
            "        if self.radius <= 0:\n            return 0.5\n",
        ),
        (
            "one indentation too deep",
            "            if self.radius <= 0:\n                return 0.5\n".to_string(),
            "            if self.radius <= 0:\n                value = 0.25\n                return value\n",
            "        if self.radius <= 0:\n            value = 0.25\n            return value\n",
        ),
        (
            "with the indentation ahead of the method",
            "    def area(self):\n        if self.radius <= 0:\n".to_string(),
            "    def area(self):\n        if self.radius < 1:\n",
            "        if self.radius < 1:\n            value = 0.25\n            return value\n",
        ),
    ] {
        let read = edit_path_read(&state, &serde_json::json!(area.id)).await;
        assert!(
            !read["body"].as_str().unwrap().contains(&old),
            "{how}: the anchor is not in the code as source serves it"
        );
        let (result, text) =
            edit_path_mutate(&state, &session, vec![edit_path_patch(&read, &[(&old, new)])]).await;
        assert_ne!(result.is_error, Some(true), "{how}: {text}");
        assert_eq!(
            text,
            "Changed pkg.shapes :: Circle.area; 1 source unit written; the graph now holds it.",
            "{how}"
        );
        let before = expected.clone();
        let from = before.find("        if self.radius").unwrap();
        let to = before.find("        return area(self.radius)").unwrap();
        expected.replace_range(from..to, now);
        assert_eq!(edit_path_disk(&dir, "pkg/shapes.py"), expected, "{how}");
    }

    // Moved to the method's indentation it fits two places: refused, and
    // nothing is written.
    let roots = source_base_roots(&state);
    let twice = module_code_entity(&state, "pkg/shapes.py", "Circle.twice");
    let read = edit_path_read(&state, &serde_json::json!(twice.id)).await;
    let (result, text) = edit_path_mutate(
        &state,
        &session,
        vec![edit_path_patch(
            &read,
            &[("    if self.radius:\n        pass", "    pass")],
        )],
    )
    .await;
    assert_eq!(result.is_error, Some(true), "{text}");
    assert_eq!(
        text,
        "Nothing was written: patch edit #0: old_text is ambiguous in Circle.twice: it is not \
         there as written, and with its lines moved to the indentation Circle.twice stands at \
         it fits more than one place, so copy the lines you mean exactly as source returned \
         them."
    );
    assert_eq!(edit_path_disk(&dir, "pkg/shapes.py"), expected);
    assert_eq!(source_base_roots(&state), roots);

    // It fits nowhere: the refusal quotes the code nearest it, exactly as the
    // method holds it, so the next anchor can be copied from that.
    let read = edit_path_read(&state, &serde_json::json!(twice.id)).await;
    let (result, text) = edit_path_mutate(
        &state,
        &session,
        vec![edit_path_patch(
            &read,
            &[(
                "    if self.radius > 1:\n        pass\n    return 2",
                "    return 3",
            )],
        )],
    )
    .await;
    assert_eq!(result.is_error, Some(true), "{text}");
    assert_eq!(
        text,
        "Nothing was written: patch edit #0: old_text is absent from Circle.twice as its code \
         stands. The code nearest it there reads:\n        if self.radius:\n            \
         pass\n        if self.radius:\nCopy old_text from those lines exactly, with their \
         indentation, or read Circle.twice again with source and copy old_text from what it \
         returns."
    );
    assert_eq!(edit_path_disk(&dir, "pkg/shapes.py"), expected);
    assert_eq!(source_base_roots(&state), roots);
    // The quoted lines are an anchor that fits as written.
    let (result, text) = edit_path_mutate(
        &state,
        &session,
        vec![edit_path_patch(
            &read,
            &[(
                "def twice(self):\n        if self.radius:\n            pass\n        if self.radius:",
                "def twice(self):\n        if self.radius:",
            )],
        )],
    )
    .await;
    assert_ne!(result.is_error, Some(true), "{text}");
}

/// A Python module holding a class of more than 16 KB: a header, a docstring
/// and class-level statements before, between and after its 60 methods.
fn edit_path_big_class_source() -> String {
    edit_path_class_source("Big", 60)
}

/// The same module with a class named `name` of `methods` methods.
fn edit_path_class_source(name: &str, methods: usize) -> String {
    let mut source = String::from(
        "\"\"\"A module with one large class.\"\"\"\nimport os\n\nLIMIT = 3\n\n\ndef helper():\n    return LIMIT\n\n\nclass Big:\n    \"\"\"A large class.\n\n    It holds settings and many methods.\n    \"\"\"\n\n    #: The default configuration.\n    default_config = {\n        \"DEBUG\": False,\n        \"LIMIT\": LIMIT,\n    }\n    name = \"big\"\n",
    );
    source = source.replace("class Big:", &format!("class {name}:"));
    for index in 0..methods {
        source.push_str(&format!(
            "\n    def method_{index:03}(self, value):\n        \"\"\"Method {index} of the large class, which only adds to a value.\n\n        It is long enough that sixty of them are more than a reply holds.\n        \"\"\"\n        total = value + {index}\n        if total > LIMIT:\n            total = total - LIMIT\n        return total + len(os.sep) + helper()\n"
        ));
        if index == 29 {
            source.push_str("\n    #: Set between two methods.\n    between = (\n        \"one\",\n        \"two\",\n    )\n");
        }
    }
    source.push_str(&format!(
        "\n    last = \"the last statement\"\n\n\ndef after():\n    return {name}.name\n"
    ));
    source
}

async fn edit_path_big_class_fixture() -> (tempfile::TempDir, Arc<DaemonState>, String) {
    let (dir, state) = mcp_lifecycle_fixture();
    let source = edit_path_big_class_source();
    for (path, body) in [
        ("pkg/__init__.py", "\"\"\"The package.\"\"\"\n"),
        ("pkg/big.py", source.as_str()),
        ("pkg/mid.py", edit_path_class_source("Mid", 20).as_str()),
        ("pkg/shapes.py", EDIT_PATH_SHAPES),
    ] {
        source_tree_conversion_fixture(
            &state,
            serde_json::json!({
                "verb": "create", "target": path, "body": body,
                "description": "large class fixture"
            }),
        )
        .await;
    }
    (dir, state, source)
}

/// `source` on `entity_id` the way an agent's call reaches the daemon: the
/// machine reply, and the text the agent reads.
async fn edit_path_source(
    state: &Arc<DaemonState>,
    entity_id: &serde_json::Value,
) -> (serde_json::Value, String) {
    let arguments = serde_json::json!({"entity_id": entity_id});
    let result = edit_path_call(state, "get_entity_source", arguments.clone()).await;
    let text = edit_path_agent_text("source", &arguments, &result);
    assert_ne!(result.is_error, Some(true), "{text}");
    (tool_result_payload(&result), text)
}

/// A large class serves markers for its fields as well as its methods. Each
/// field is read and changed by its own ticket; the class's ticket survives
/// those member edits and only changes when its own code changes.
#[tokio::test]
#[serial_test::serial]
async fn edit_path_a_large_class_is_read_and_patched_as_its_own_code() {
    let (dir, state, original) = edit_path_big_class_fixture().await;
    let session = mcp_test_session(&state);
    let class = module_code_entity(&state, "pkg/big.py", "Big");
    let span = class.span.as_ref().unwrap();
    assert!(span.end_byte - span.start_byte > 16 * 1024);
    let member = |name: &str| module_code_entity(&state, "pkg/big.py", name);
    let (read, text) = edit_path_source(&state, &serde_json::json!(class.id)).await;
    let body = read["body"].as_str().unwrap();
    assert!(
        text.starts_with("this is the class-level code of Big"),
        "{text}"
    );
    assert!(body.len() < 8 * 1024);
    assert!(body.contains("It holds settings and many methods."));
    for name in ["default_config", "name", "between", "last"] {
        assert!(
            body.contains(&format!("kin:entity field Big.{name} #")),
            "{body}"
        );
    }
    assert!(
        !body.contains("\"DEBUG\": False") && !body.contains("def method_"),
        "{body}"
    );
    assert_eq!(body.matches("kin:entity").count(), 64);
    assert_eq!(read["class_code"]["members"], 64);
    assert!(kin_mcp::source_ticket::is_ticket(&read["source_base"]));
    let first = member("Big.method_000").id;

    // The configuration key and the fields before, between and after methods
    // are addressable source entities. Each edit leaves every other member.
    let mut patched = original;
    for (name, old, new) in [
        ("Big.default_config.DEBUG", "False", "True"),
        ("Big.name", "\"big\"", "\"renamed\""),
        ("Big.between", "\"two\"", "\"three\""),
        ("Big.last", "\"the last statement\"", "\"changed\""),
    ] {
        let field = member(name);
        assert_eq!(field.kind, kin_model::EntityKind::Field);
        let (field_read, text) = edit_path_source(&state, &serde_json::json!(field.id)).await;
        assert!(field_read["body"].as_str().unwrap().contains(old), "{text}");
        let (result, text) = edit_path_mutate(
            &state,
            &session,
            vec![edit_path_patch(&field_read, &[(old, new)])],
        )
        .await;
        assert_ne!(result.is_error, Some(true), "{name}: {text}");
        patched = patched.replacen(old, new, 1);
        assert_eq!(edit_path_disk(&dir, "pkg/big.py"), patched);
        assert_eq!(member(name).id, field.id);
    }
    assert_eq!(member("Big.method_000").id, first);

    // The class ticket was read before the field edits. Its own code did not
    // change, so it still admits a patch to the docstring.
    let (result, text) = edit_path_mutate(
        &state,
        &session,
        vec![edit_path_patch(
            &read,
            &[(
                "It holds settings and many methods.",
                "It holds fields and many methods.",
            )],
        )],
    )
    .await;
    assert_ne!(result.is_error, Some(true), "{text}");
    patched = patched.replace(
        "It holds settings and many methods.",
        "It holds fields and many methods.",
    );
    assert_eq!(edit_path_disk(&dir, "pkg/big.py"), patched);

    // The same independence holds for a method body's edit.
    let (read, _) = edit_path_source(&state, &serde_json::json!(class.id)).await;
    let method = member("Big.method_003");
    let method_read = edit_path_read(&state, &serde_json::json!(method.id)).await;
    let (result, text) = edit_path_mutate(
        &state,
        &session,
        vec![edit_path_patch(
            &method_read,
            &[("total = value + 3", "total = value + 33")],
        )],
    )
    .await;
    assert_ne!(result.is_error, Some(true), "{text}");
    let (result, text) = edit_path_mutate(
        &state,
        &session,
        vec![edit_path_patch(
            &read,
            &[(
                "It holds fields and many methods.",
                "It holds independent fields and methods.",
            )],
        )],
    )
    .await;
    assert_ne!(result.is_error, Some(true), "{text}");
    patched = patched
        .replace("total = value + 3\n", "total = value + 33\n")
        .replace(
            "It holds fields and many methods.",
            "It holds independent fields and methods.",
        );
    assert_eq!(edit_path_disk(&dir, "pkg/big.py"), patched);

    // A change to the class's own text, unlike a member edit, stales its ticket.
    let roots = source_base_roots(&state);
    let (result, text) = edit_path_mutate(
        &state,
        &session,
        vec![edit_path_patch(
            &read,
            &[(
                "It holds independent fields and methods.",
                "A stale change.",
            )],
        )],
    )
    .await;
    assert_eq!(result.is_error, Some(true), "{text}");
    assert_eq!(
        text,
        "That ticket is from before Big last changed. Read Big again with source and resend \
         your change with the source_base that read returns. Nothing was written."
    );
    assert_eq!(edit_path_disk(&dir, "pkg/big.py"), patched);
    assert_eq!(source_base_roots(&state), roots);
}

/// A patch to a large class's own code changes that code and nothing else.
/// What it may not do is said in one sentence with the call that does it: a
/// member is changed by its own id, a method is created, and the class is
/// never replaced whole.
#[tokio::test]
#[serial_test::serial]
async fn edit_path_a_large_class_patch_never_reaches_a_member_or_past_the_class() {
    let (dir, state, original) = edit_path_big_class_fixture().await;
    let session = mcp_test_session(&state);
    let class = module_code_entity(&state, "pkg/big.py", "Big");
    let member = |name: &str| module_code_entity(&state, "pkg/big.py", name);
    let (read, _) = edit_path_source(&state, &serde_json::json!(class.id)).await;
    let body = read["body"].as_str().unwrap().to_string();
    let marker = body
        .lines()
        .find(|line| line.contains("kin:entity method Big.method_002 "))
        .unwrap()
        .to_string();
    let last_marker = body
        .lines()
        .find(|line| line.contains("kin:entity field Big.last "))
        .unwrap()
        .to_string();
    let roots = source_base_roots(&state);

    for (operation, expected) in [
        (
            edit_path_update(&read, "class Big:\n    pass\n"),
            "Nothing was written: class Big is too large to replace whole, and Kin holds it as \
             its own code and its members: change the class-level code with a patch against \
             what source returns for Big, and each member by its own id."
                .to_string(),
        ),
        (
            edit_path_patch(&read, &[("total = value + 1\n", "total = value + 100\n")]),
            format!(
                "Nothing was written: patch edit #0: old_text is inside method Big.method_001, \
                 which the class-level code of Big shows only as a kin:entity line, so read \
                 Big.method_001 with source (entity_id {}) and patch it with the source_base \
                 that read returns.",
                member("Big.method_001").id
            ),
        ),
        (
            edit_path_patch(&read, &[(&format!("{marker}\n"), "")]),
            format!(
                "Nothing was written: the patch changes or removes the kin:entity line that \
                 stands for method Big.method_002, which is not code of Big, so keep that line \
                 exactly as source returned it and change Big.method_002 by its own id \
                 (entity_id {}): source it and patch it, or remove it with EntityRemove.",
                member("Big.method_002").id
            ),
        ),
        (
            edit_path_patch(
                &read,
                &[(
                    "    #: The default configuration.\n",
                    "    def extra(self):\n        return 1\n\n    #: The default configuration.\n",
                )],
            ),
            "Nothing was written: the patch to class Big would declare method Big.extra in its \
             class-level code, and a class patch adds statements, never a method or a class, \
             so create Big.extra with EntityCreate and placement member."
                .to_string(),
        ),
        (
            edit_path_patch(
                &read,
                &[(
                    last_marker.as_str(),
                    &format!("{last_marker}\nprint(\"loose\")"),
                )],
            ),
            "Nothing was written: the patch would leave code outside class Big: every line of \
             a class's own code stays inside the class, indented under its header, so check \
             the indentation of new_text."
                .to_string(),
        ),
        (
            edit_path_patch(
                &read,
                &[(
                    "    #: The default configuration.\n",
                    "    name = (\"big\"\n",
                )],
            ),
            "Nothing was written: the class-level code of Big as patched is not valid Python, \
             `name = (\"big\"`: this does not parse."
                .to_string(),
        ),
    ] {
        let (result, text) = edit_path_mutate(&state, &session, vec![operation]).await;
        println!("{text}");
        assert_eq!(result.is_error, Some(true), "{text}");
        assert_eq!(text, expected);
        assert_eq!(edit_path_disk(&dir, "pkg/big.py"), original);
        assert_eq!(source_base_roots(&state), roots);
    }

    // A method is created with the class's ticket, as a member.
    let (result, text) = edit_path_mutate(
        &state,
        &session,
        vec![edit_path_create(
            &read,
            "member",
            "method",
            "added",
            "def added(self):\n    return self.name",
        )],
    )
    .await;
    assert_ne!(result.is_error, Some(true), "{text}");
    assert_eq!(
        text,
        "Created pkg.big :: Big.added; 1 source unit written; the graph now holds it."
    );
    let disk = edit_path_disk(&dir, "pkg/big.py");
    assert!(
        disk.contains("\n    def added(self):\n        return self.name\n"),
        "{disk}"
    );
    // The class holds one member more, so its code shows one marker more and
    // the ticket of the read before it is from before the class changed.
    let (after, _) = edit_path_source(&state, &serde_json::json!(class.id)).await;
    assert_eq!(after["class_code"]["members"], 65, "{after}");
    let (result, text) = edit_path_mutate(
        &state,
        &session,
        vec![edit_path_patch(
            &read,
            &[(
                "It holds settings and many methods.",
                "It holds fields and many methods.",
            )],
        )],
    )
    .await;
    assert_eq!(result.is_error, Some(true), "{text}");
    assert!(
        text.starts_with("That ticket is from before Big last changed."),
        "{text}"
    );
    let (result, text) = edit_path_mutate(
        &state,
        &session,
        vec![edit_path_patch(
            &after,
            &[(
                "It holds settings and many methods.",
                "It holds fields and many methods.",
            )],
        )],
    )
    .await;
    assert_ne!(result.is_error, Some(true), "{text}");
    assert_eq!(
        edit_path_disk(&dir, "pkg/big.py"),
        disk.replace(
            "It holds settings and many methods.",
            "It holds fields and many methods."
        )
    );
}

/// A class that is served whole stays one entity: a small one is read whole
/// by default, and one of 4 to 16 KB as its outline, or whole when its whole
/// body is asked for, and either is patched with the ticket of that read.
/// Only a class no reply holds whole is served as its own code.
#[tokio::test]
#[serial_test::serial]
async fn edit_path_a_class_that_fits_whole_is_still_one_entity() {
    let (dir, state, _) = edit_path_big_class_fixture().await;
    let session = mcp_test_session(&state);

    let circle = module_code_entity(&state, "pkg/shapes.py", "Circle");
    let (read, text) = edit_path_source(&state, &serde_json::json!(circle.id)).await;
    assert!(read.get("class_code").is_none(), "{read}");
    assert!(
        text.starts_with("this is the exact current source of Circle, the whole body."),
        "{text}"
    );
    assert!(read["body"]
        .as_str()
        .unwrap()
        .contains("return area(self.radius)"));

    let mid = module_code_entity(&state, "pkg/mid.py", "Mid");
    let mid_bytes = {
        let span = mid.span.as_ref().unwrap();
        span.end_byte - span.start_byte
    };
    assert!((4 * 1024..16 * 1024).contains(&mid_bytes), "{mid_bytes}");
    let (outline, text) = edit_path_source(&state, &serde_json::json!(mid.id)).await;
    assert!(
        outline.get("class_code").is_none() && outline.get("body").is_none(),
        "{outline}"
    );
    assert!(text.starts_with("this is the outline of Mid:"), "{text}");
    let arguments = serde_json::json!({"entity_id": mid.id, "full_body": true});
    let result = edit_path_call(&state, "get_entity_source", arguments).await;
    let whole = tool_result_payload(&result);
    assert!(whole.get("class_code").is_none(), "{whole}");
    assert!(
        whole["body"]
            .as_str()
            .unwrap()
            .contains("total = value + 19"),
        "{whole}"
    );
    // The whole class is patched as one entity, a member's body included.
    let (result, text) = edit_path_mutate(
        &state,
        &session,
        vec![edit_path_patch(
            &whole,
            &[("total = value + 19", "total = value + 190")],
        )],
    )
    .await;
    assert_ne!(result.is_error, Some(true), "{text}");
    assert!(edit_path_disk(&dir, "pkg/mid.py").contains("total = value + 190"));
}

const EDIT_PATH_GAUGE: &str = "\"\"\"A gauge.\"\"\"\nfrom typing import overload\n\n\n@overload\ndef scale(value: int) -> int: ...\n\n\n@overload\ndef scale(value: str) -> str: ...\n\n\ndef scale(value):\n    return value\n\n\nclass Gauge:\n    \"\"\"Holds a level.\"\"\"\n\n    def __init__(self):\n        self._level = 0\n\n    @property\n    def level(self):\n        return self._level\n\n    @level.setter\n    def level(self, value):\n        self._level = value\n\n    def reset(self):\n        self._level = 0\n";

/// Every entity named `name` declared in the file at `path`, in source order.
fn edit_path_twins(state: &DaemonState, path: &str, name: &str) -> Vec<kin_model::Entity> {
    let mut twins: Vec<kin_model::Entity> = state
        .graph
        .query_entities(&kin_model::EntityFilter {
            file_path: Some(kin_model::FilePathId::new(path)),
            ..Default::default()
        })
        .unwrap()
        .into_iter()
        .filter(|entity| entity.name == name && !kin_model::is_file_module_surface(entity))
        .collect();
    twins.sort_by_key(|entity| entity.span.as_ref().map(|span| span.start_byte));
    twins
}

/// One `source` call with `entity_id` as an agent wrote it: the route's
/// result and the text the agent reads.
async fn edit_path_source_as(
    state: &Arc<DaemonState>,
    entity_id: &str,
) -> (kin_mcp::ToolCallResult, String) {
    let arguments = serde_json::json!({"entity_id": entity_id});
    let result = edit_path_call(state, "get_entity_source", arguments.clone()).await;
    let text = edit_path_agent_text("source", &arguments, &result);
    (result, text)
}

/// A name declared more than once in one scope, a property and its setter or
/// the overloads of a function, is answered with every declaration and the
/// address of each, never as a name that names nothing. Each address reads
/// that declaration, and a write whose target is any spelling of the entity
/// its ticket was read from lands on it.
#[tokio::test]
#[serial_test::serial]
async fn edit_path_a_name_declared_twice_is_listed_by_address_and_read_and_written_by_it() {
    let (dir, state) = mcp_lifecycle_fixture();
    for (path, body) in [
        ("pkg/__init__.py", "\"\"\"The package.\"\"\"\n"),
        ("pkg/gauge.py", EDIT_PATH_GAUGE),
    ] {
        source_tree_conversion_fixture(
            &state,
            serde_json::json!({
                "verb": "create", "target": path, "body": body,
                "description": "twins fixture"
            }),
        )
        .await;
    }
    let session = mcp_test_session(&state);
    let short = |entity: &kin_model::Entity| entity.id.to_string()[..8].to_string();
    let level = edit_path_twins(&state, "pkg/gauge.py", "Gauge.level");
    let [getter, setter] = level.as_slice() else {
        panic!("a property and its setter are two entities: {level:?}");
    };
    let scale = edit_path_twins(&state, "pkg/gauge.py", "scale");
    assert_eq!(scale.len(), 3, "two overloads and the function: {scale:?}");

    // The name alone, or its label, lists both with their addresses.
    let listed = format!(
        "names 2 entities, and this argument takes one. Pass one of: `pkg.gauge :: Gauge.level \
         #{}` (`@property def level(self)`), `pkg.gauge :: Gauge.level #{}` (`@setter def \
         level(self, value)`).",
        short(getter),
        short(setter)
    );
    for spelling in ["pkg.gauge :: Gauge.level", "Gauge.level"] {
        let (result, text) = edit_path_source_as(&state, spelling).await;
        assert_eq!(result.is_error, Some(true), "{text}");
        assert_eq!(text, format!("entity_id `{spelling}` {listed}"));
    }
    let (result, text) = edit_path_source_as(&state, "pkg.gauge :: scale").await;
    assert_eq!(result.is_error, Some(true), "{text}");
    assert_eq!(
        text,
        format!(
            "entity_id `pkg.gauge :: scale` names 3 entities, and this argument takes one. \
             Pass one of: `pkg.gauge :: scale #{}` (`@overload def scale(value: int) -> int`), \
             `pkg.gauge :: scale #{}` (`@overload def scale(value: str) -> str`), `pkg.gauge \
             :: scale #{}` (`def scale(value)`).",
            short(&scale[0]),
            short(&scale[1]),
            short(&scale[2])
        )
    );

    // Each address reads its own declaration, on a connection that was never
    // shown it.
    for (twin, line) in [(getter, "@property"), (setter, "@level.setter")] {
        for spelling in [
            format!("pkg.gauge :: Gauge.level #{}", short(twin)),
            format!("Gauge.level #{}", short(twin)),
            format!("#{}", short(twin)),
        ] {
            let (result, text) = edit_path_source_as(&state, &spelling).await;
            assert_ne!(result.is_error, Some(true), "{spelling}: {text}");
            assert!(
                text.starts_with(&format!(
                    "this is the exact current source of Gauge.level, the whole body.\n\
                     pkg.gauge :: Gauge.level #{}\nbody:\n  {line}\n",
                    short(twin)
                )),
                "{spelling}: {text}"
            );
        }
    }

    // A write: the ticket says which declaration, and the target agrees with
    // it in any spelling, the shared name included.
    let mut expected = EDIT_PATH_GAUGE.to_string();
    for (target, verb, from, to) in [
        (
            format!("pkg.gauge :: Gauge.level #{}", short(setter)),
            "patch",
            "self._level = value",
            "self._level = int(value)",
        ),
        (
            format!("#{}", short(setter)),
            "patch",
            "self._level = int(value)",
            "self._level = abs(int(value))",
        ),
        (
            "pkg.gauge :: Gauge.level".to_string(),
            "patch",
            "abs(int(value))",
            "max(0, int(value))",
        ),
        (
            "Gauge.level".to_string(),
            "update",
            "self._level = max(0, int(value))",
            "self._level = value",
        ),
    ] {
        let read = edit_path_read(&state, &serde_json::json!(setter.id)).await;
        let mut operation = if verb == "patch" {
            edit_path_patch(&read, &[(from, to)])
        } else {
            edit_path_update(&read, &read["body"].as_str().unwrap().replace(from, to))
        };
        operation["target"] = serde_json::json!(target);
        let (result, text) = edit_path_mutate(&state, &session, vec![operation]).await;
        assert_ne!(result.is_error, Some(true), "{target}: {text}");
        assert_eq!(
            text,
            format!(
                "Changed pkg.gauge :: Gauge.level #{}; 1 source unit written; the graph now \
                 holds it.",
                short(setter)
            ),
            "{target}"
        );
        expected = expected.replace(from, to);
        assert_eq!(edit_path_disk(&dir, "pkg/gauge.py"), expected, "{target}");
    }
    // A method is created beside the setter, named by its address.
    let read = edit_path_read(&state, &serde_json::json!(setter.id)).await;
    let mut create = edit_path_create(
        &read,
        "sibling_after",
        "method",
        "half",
        "def half(self):\n    return self._level / 2",
    );
    create["target"] = serde_json::json!(format!("Gauge.level #{}", short(setter)));
    let (result, text) = edit_path_mutate(&state, &session, vec![create]).await;
    assert_ne!(result.is_error, Some(true), "{text}");
    assert_eq!(
        text,
        "Created pkg.gauge :: Gauge.half; 1 source unit written; the graph now holds it."
    );

    // A target that is some other entity is refused with the address of the
    // one the ticket was read from, and nothing is written.
    let roots = source_base_roots(&state);
    let disk = edit_path_disk(&dir, "pkg/gauge.py");
    let read = edit_path_read(&state, &serde_json::json!(setter.id)).await;
    for target in [
        format!("pkg.gauge :: Gauge.level #{}", short(getter)),
        "pkg.gauge :: Gauge.reset".to_string(),
    ] {
        let mut operation = edit_path_patch(&read, &[("self._level = value", "pass")]);
        operation["target"] = serde_json::json!(target);
        let (result, text) = edit_path_mutate(&state, &session, vec![operation]).await;
        assert_eq!(result.is_error, Some(true), "{target}: {text}");
        assert_eq!(
            text,
            format!(
                "target `{target}` is not the entity this source_base was read from, which is \
                 `pkg.gauge :: Gauge.level #{}`. Pass that as target to change it, or read \
                 `{target}` with source and send the source_base that read returns. Nothing was \
                 written.",
                short(setter)
            )
        );
        assert_eq!(edit_path_disk(&dir, "pkg/gauge.py"), disk);
        assert_eq!(source_base_roots(&state), roots);
    }
}

/// A function declared inside the function being edited is part of that
/// function's code: adding one, changing it and taking it out again are
/// ordinary edits. A declaration beside the edited one is not, and the
/// refusal names the verb that adds it.
#[tokio::test]
#[serial_test::serial]
async fn edit_path_an_edit_may_declare_inside_the_entity_it_names_and_never_beside_it() {
    let (dir, state) = edit_path_fixture().await;
    let session = mcp_test_session(&state);
    let declared = |path: &str, name: &str| {
        state
            .graph
            .query_entities(&kin_model::EntityFilter {
                file_path: Some(kin_model::FilePathId::new(path)),
                ..Default::default()
            })
            .unwrap()
            .into_iter()
            .any(|entity| entity.name == name)
    };

    // A method gains a nested function, in an update written at the margin.
    let area = module_code_entity(&state, "pkg/shapes.py", "Circle.area");
    let read = edit_path_read(&state, &serde_json::json!(area.id)).await;
    let (result, text) = edit_path_mutate(
        &state,
        &session,
        vec![edit_path_update(
            &read,
            "def area(self):\n    def squared(value):\n        return value * value\n\n    if self.radius < 0:\n        return 0\n    return math.pi * squared(self.radius)",
        )],
    )
    .await;
    assert_ne!(result.is_error, Some(true), "{text}");
    assert_eq!(
        text,
        "Changed pkg.shapes :: Circle.area; 1 source unit written; the graph now holds it."
    );
    let nested = "    def area(self):\n        def squared(value):\n            return value * value\n\n        if self.radius < 0:\n            return 0\n        return math.pi * squared(self.radius)\n";
    let held = "    def area(self):\n        if self.radius < 0:\n            return 0\n        return area(self.radius)\n";
    let mut expected = EDIT_PATH_SHAPES.replace(held, nested);
    assert_eq!(edit_path_disk(&dir, "pkg/shapes.py"), expected);
    assert!(declared("pkg/shapes.py", "Circle.area.<locals>.squared"));

    // The nested function is changed, and a second one added, by a patch.
    let read = edit_path_read(&state, &serde_json::json!(area.id)).await;
    let (result, text) = edit_path_mutate(
        &state,
        &session,
        vec![edit_path_patch(
            &read,
            &[(
                "            return value * value\n",
                "            return value**2\n\n        def halved(value):\n            return value / 2\n",
            )],
        )],
    )
    .await;
    assert_ne!(result.is_error, Some(true), "{text}");
    expected = expected.replace(
        "            return value * value\n",
        "            return value**2\n\n        def halved(value):\n            return value / 2\n",
    );
    assert_eq!(edit_path_disk(&dir, "pkg/shapes.py"), expected);
    assert!(declared("pkg/shapes.py", "Circle.area.<locals>.halved"));

    // Both are taken out again by an update of the method.
    let read = edit_path_read(&state, &serde_json::json!(area.id)).await;
    let (result, text) = edit_path_mutate(
        &state,
        &session,
        vec![edit_path_update(
            &read,
            "def area(self):\n    if self.radius < 0:\n        return 0\n    return area(self.radius)",
        )],
    )
    .await;
    assert_ne!(result.is_error, Some(true), "{text}");
    assert_eq!(edit_path_disk(&dir, "pkg/shapes.py"), EDIT_PATH_SHAPES);
    assert!(!declared("pkg/shapes.py", "Circle.area.<locals>.squared"));
    assert!(!declared("pkg/shapes.py", "Circle.area.<locals>.halved"));

    // The same in a function at the top of a module, and in Rust and
    // TypeScript, where a function may be declared in a function too.
    for (path, name, code, inside) in [
        (
            "pkg/shapes.py",
            "area",
            "def area(radius):\n    def squared(value):\n        return value * value\n\n    return math.pi * squared(radius)",
            "def squared(value):",
        ),
        (
            "src/shapes.rs",
            "area",
            "pub fn area(radius: f64) -> f64 {\n    fn squared(value: f64) -> f64 {\n        value * value\n    }\n    3.14 * squared(radius)\n}",
            "fn squared(value: f64) -> f64 {",
        ),
        (
            "web/shapes.ts",
            "area",
            "function area(radius: number): number {\n  function squared(value: number): number {\n    return value * value;\n  }\n  return Math.PI * squared(radius);\n}",
            "function squared(value: number): number {",
        ),
    ] {
        let entity = module_code_entity(&state, path, name);
        let read = edit_path_read(&state, &serde_json::json!(entity.id)).await;
        let (result, text) =
            edit_path_mutate(&state, &session, vec![edit_path_update(&read, code)]).await;
        assert_ne!(result.is_error, Some(true), "{path}: {text}");
        assert!(edit_path_disk(&dir, path).contains(inside), "{path}");
    }

    // A declaration beside the edited one is what create is for.
    let roots = source_base_roots(&state);
    for (path, name, code, expected) in [
        (
            "pkg/shapes.py",
            "Circle.name",
            "def name(self):\n    return \"circle\"\n\ndef label(self):\n    return \"Circle\"",
            "Nothing was written: this edit of Circle.name would also add method Circle.label \
             beside it, and an edit changes only the declaration it names. Send the code of \
             Circle.name alone, and add Circle.label with the create verb (payload EntityCreate).",
        ),
        (
            "src/shapes.rs",
            "Circle::name",
            "pub fn name(&self) -> &'static str {\n    \"circle\"\n}\n\npub fn label(&self) -> &'static str {\n    \"Circle\"\n}",
            "Nothing was written: this edit of Circle::name would also add method Circle::label \
             beside it, and an edit changes only the declaration it names. Send the code of \
             Circle::name alone, and add Circle::label with the create verb (payload \
             EntityCreate).",
        ),
        // Code that declares another name is not an edit of this one.
        (
            "pkg/shapes.py",
            "Circle.name",
            "def title(self):\n    return \"circle\"",
            "Nothing was written: this edit of Circle.name no longer declares Circle.name, and an \
             edit keeps the name and kind of the declaration it changes. To put another \
             declaration in its place, add that one with the create verb (payload EntityCreate) \
             and take this one out with the remove verb (payload EntityRemove).",
        ),
    ] {
        let before = edit_path_disk(&dir, path);
        let entity = module_code_entity(&state, path, name);
        let read = edit_path_read(&state, &serde_json::json!(entity.id)).await;
        let (result, text) =
            edit_path_mutate(&state, &session, vec![edit_path_update(&read, code)]).await;
        println!("{text}");
        assert_eq!(result.is_error, Some(true), "{path}: {text}");
        assert_eq!(text, expected, "{path}");
        assert_eq!(edit_path_disk(&dir, path), before, "{path}");
        assert_eq!(source_base_roots(&state), roots, "{path}");
    }
}

// ---- separation of created and removed declarations

const EDIT_PATH_SPACED: &str = "\"\"\"Spaced.\"\"\"\n\n\ndef first():\n    return 1\n\n\ndef second():\n    return 2\n\n\nclass Holder:\n    \"\"\"Holds.\"\"\"\n\n    def one(self):\n        return 1\n\n    def two(self):\n        return 2\n";

const EDIT_PATH_SPACED_RS: &str =
    "pub fn first() -> u8 {\n    1\n}\n\npub fn second() -> u8 {\n    2\n}\n";

/// A `remove` of the entity a `source` read returned.
fn edit_path_remove(read: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "verb": "remove", "target": read["id"], "description": "take the declaration out",
        "payload": {"EntityRemove": {"source_base": read["source_base"]}}
    })
}

/// A created declaration stands as far from its neighbours as they stand
/// from one another, and a removed one leaves them as far apart as they were
/// from it: two blank lines between Python's top-level definitions, one
/// between its methods, and in Rust whatever the functions around it use.
/// The new code is placed at the margin however it was indented.
#[tokio::test]
#[serial_test::serial]
async fn edit_path_create_and_remove_keep_the_separation_the_neighbours_use() {
    let (dir, state) = mcp_lifecycle_fixture();
    for (path, body) in [
        ("pkg/__init__.py", "\"\"\"The package.\"\"\"\n"),
        ("pkg/spaced.py", EDIT_PATH_SPACED),
        ("src/spaced.rs", EDIT_PATH_SPACED_RS),
    ] {
        source_tree_conversion_fixture(
            &state,
            serde_json::json!({
                "verb": "create", "target": path, "body": body,
                "description": "spacing fixture"
            }),
        )
        .await;
    }
    let session = mcp_test_session(&state);

    for (path, label, anchor, kind, name, code, original, created) in [
        // Between two top-level Python functions, sent indented as a whole
        // and with a line break after it.
        (
            "pkg/spaced.py",
            "pkg.spaced :: added",
            "first",
            "function",
            "added",
            "    def added():\n        return 3\n",
            EDIT_PATH_SPACED,
            EDIT_PATH_SPACED.replace(
                "def first():\n    return 1\n\n\n",
                "def first():\n    return 1\n\n\ndef added():\n    return 3\n\n\n",
            ),
        ),
        // Before a class, which is a neighbour like any other.
        (
            "pkg/spaced.py",
            "pkg.spaced :: added",
            "second",
            "function",
            "added",
            "def added():\n    return 3",
            EDIT_PATH_SPACED,
            EDIT_PATH_SPACED.replace(
                "def second():\n    return 2\n\n\n",
                "def second():\n    return 2\n\n\ndef added():\n    return 3\n\n\n",
            ),
        ),
        // Between two methods, and after the last one.
        (
            "pkg/spaced.py",
            "pkg.spaced :: Holder.added",
            "Holder.one",
            "method",
            "added",
            "def added(self):\n    return 3",
            EDIT_PATH_SPACED,
            EDIT_PATH_SPACED.replace(
                "    def one(self):\n        return 1\n\n",
                "    def one(self):\n        return 1\n\n    def added(self):\n        return 3\n\n",
            ),
        ),
        (
            "pkg/spaced.py",
            "pkg.spaced :: Holder.added",
            "Holder.two",
            "method",
            "added",
            "def added(self):\n    return 3",
            EDIT_PATH_SPACED,
            format!("{EDIT_PATH_SPACED}\n    def added(self):\n        return 3\n"),
        ),
        // Rust functions one blank line apart: between them, and at the end.
        (
            "src/spaced.rs",
            "spaced :: added",
            "first",
            "function",
            "added",
            "  pub fn added() -> u8 {\n      3\n  }\n\n",
            EDIT_PATH_SPACED_RS,
            EDIT_PATH_SPACED_RS.replace(
                "pub fn first() -> u8 {\n    1\n}\n\n",
                "pub fn first() -> u8 {\n    1\n}\n\npub fn added() -> u8 {\n    3\n}\n\n",
            ),
        ),
        (
            "src/spaced.rs",
            "spaced :: added",
            "second",
            "function",
            "added",
            "pub fn added() -> u8 {\n    3\n}",
            EDIT_PATH_SPACED_RS,
            format!("{EDIT_PATH_SPACED_RS}\npub fn added() -> u8 {{\n    3\n}}\n"),
        ),
    ] {
        let beside = module_code_entity(&state, path, anchor);
        let read = edit_path_read(&state, &serde_json::json!(beside.id)).await;
        let (result, text) = edit_path_mutate(
            &state,
            &session,
            vec![edit_path_create(&read, "sibling_after", kind, name, code)],
        )
        .await;
        assert_ne!(result.is_error, Some(true), "{path} after {anchor}: {text}");
        assert_eq!(
            text,
            format!("Created {label}; 1 source unit written; the graph now holds it."),
            "{path} after {anchor}"
        );
        assert_eq!(edit_path_disk(&dir, path), created, "{path} after {anchor}");

        // Taking it out again leaves the source as it was, byte for byte.
        let added = label.rsplit(" :: ").next().unwrap();
        let entity = module_code_entity(&state, path, added);
        let read = edit_path_read(&state, &serde_json::json!(entity.id)).await;
        let (result, text) =
            edit_path_mutate(&state, &session, vec![edit_path_remove(&read)]).await;
        assert_ne!(result.is_error, Some(true), "{path} remove after {anchor}: {text}");
        assert_eq!(
            text,
            format!("Removed {label}; 1 source unit written; the graph now holds it."),
            "{path} after {anchor}"
        );
        assert_eq!(edit_path_disk(&dir, path), original, "{path} after {anchor}");
    }

    // A function between two others is removed with the space after it.
    let second = module_code_entity(&state, "src/spaced.rs", "first");
    let read = edit_path_read(&state, &serde_json::json!(second.id)).await;
    let (result, text) = edit_path_mutate(&state, &session, vec![edit_path_remove(&read)]).await;
    assert_ne!(result.is_error, Some(true), "{text}");
    assert_eq!(
        edit_path_disk(&dir, "src/spaced.rs"),
        "pub fn second() -> u8 {\n    2\n}\n"
    );
    let first = module_code_entity(&state, "pkg/spaced.py", "first");
    let read = edit_path_read(&state, &serde_json::json!(first.id)).await;
    let (result, text) = edit_path_mutate(&state, &session, vec![edit_path_remove(&read)]).await;
    assert_ne!(result.is_error, Some(true), "{text}");
    assert_eq!(
        edit_path_disk(&dir, "pkg/spaced.py"),
        EDIT_PATH_SPACED.replace("def first():\n    return 1\n\n\n", "")
    );
}

// ---- a shared name is never one entity's address

/// One agent connection that lives across calls: what it was served it reads
/// back, the way a stdio connection does, and its replies are the text an
/// agent reads.
struct EditPathConnection {
    config: kin_mcp::McpServerConfig,
}

impl EditPathConnection {
    fn open() -> Self {
        Self {
            config: kin_mcp::McpServerConfig {
                plain_replies: true,
                agent_belt: true,
                reply_format: kin_mcp::text_reply::ReplyFormat::Text,
                session_handles: Some(
                    Arc::new(kin_mcp::session_handles::SessionHandles::default()),
                ),
                ..Default::default()
            },
        }
    }

    /// One call of the agent's `tool`, served by the route's `registered`
    /// tool: every handle and label this connection served is read back
    /// first, then the call is dispatched and its reply presented.
    async fn call(
        &self,
        state: &Arc<DaemonState>,
        tool: &str,
        registered: &str,
        arguments: serde_json::Value,
    ) -> (kin_mcp::ToolCallResult, String) {
        let request = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": tool, "arguments": arguments}})
        .to_string();
        let read_back: serde_json::Value =
            serde_json::from_str(&self.config.expand_handles(request.clone())).unwrap();
        let result =
            edit_path_call(state, registered, read_back["params"]["arguments"].clone()).await;
        let response = kin_mcp::JsonRpcResponse::success(
            Some(serde_json::json!(1)),
            serde_json::to_value(&result).unwrap(),
        );
        let text = kin_mcp::present_for_client(response, &request, &self.config)
            .result
            .expect("a tool call is answered with a result")["content"][0]["text"]
            .as_str()
            .expect("a text block")
            .to_string();
        (result, text)
    }
}

/// A name its source unit declares more than once is no entity's address. A
/// reply that holds only one of the declarations still writes its row with
/// its id, so the connection never learns the bare label as that one, and a
/// read by the bare label afterwards is answered with every declaration, not
/// with the one that happened to be shown. A name declared once is read back
/// by its label as before.
#[tokio::test]
#[serial_test::serial]
async fn edit_path_a_shared_name_is_always_written_with_its_id_and_never_read_back_as_one_twin() {
    let (dir, state) = mcp_lifecycle_fixture();
    for (path, body) in [
        ("pkg/__init__.py", "\"\"\"The package.\"\"\"\n"),
        ("pkg/gauge.py", EDIT_PATH_GAUGE),
    ] {
        source_tree_conversion_fixture(
            &state,
            serde_json::json!({
                "verb": "create", "target": path, "body": body,
                "description": "twins fixture"
            }),
        )
        .await;
    }
    let session = mcp_test_session(&state);
    let short = |entity: &kin_model::Entity| entity.id.to_string()[..8].to_string();
    let level = edit_path_twins(&state, "pkg/gauge.py", "Gauge.level");
    let [getter, setter] = level.as_slice() else {
        panic!("a property and its setter are two entities: {level:?}");
    };
    let connection = EditPathConnection::open();

    // A search that returns the getter and not the setter still says which.
    let (result, listing) = connection
        .call(
            &state,
            "locate",
            "semantic_locate",
            serde_json::json!({"query": "level", "details": true, "limit": 10}),
        )
        .await;
    assert_ne!(result.is_error, Some(true), "{listing}");
    println!("{listing}");
    assert!(
        listing.contains(&format!(
            "\n  pkg.gauge :: Gauge.level #{}  ",
            short(getter)
        )),
        "{listing}"
    );
    assert!(!listing.contains(&short(setter)), "{listing}");
    // A name declared once is written bare.
    assert!(
        listing.contains("\n  pkg.gauge :: Gauge.reset  "),
        "{listing}"
    );

    // The bare label on the same connection: both declarations, no body.
    let listed = format!(
        "entity_id `pkg.gauge :: Gauge.level` names 2 entities, and this argument takes one. \
         Pass one of: `pkg.gauge :: Gauge.level #{}` (`@property def level(self)`), `pkg.gauge \
         :: Gauge.level #{}` (`@setter def level(self, value)`).",
        short(getter),
        short(setter)
    );
    let bare = serde_json::json!({"entity_id": "pkg.gauge :: Gauge.level"});
    let (result, text) = connection
        .call(&state, "source", "get_entity_source", bare.clone())
        .await;
    assert_eq!(result.is_error, Some(true), "{text}");
    assert_eq!(text, listed);

    // Reading the getter by the address the row printed serves it under that
    // address, and still teaches the connection nothing about the bare label.
    let (result, text) = connection
        .call(
            &state,
            "source",
            "get_entity_source",
            serde_json::json!({"entity_id": format!("pkg.gauge :: Gauge.level #{}", short(getter))}),
        )
        .await;
    assert_ne!(result.is_error, Some(true), "{text}");
    assert!(
        text.starts_with(&format!(
            "this is the exact current source of Gauge.level, the whole body.\npkg.gauge :: \
             Gauge.level #{}\nbody:\n  @property\n",
            short(getter)
        )),
        "{text}"
    );
    let (result, text) = connection
        .call(&state, "source", "get_entity_source", bare)
        .await;
    assert_eq!(result.is_error, Some(true), "{text}");
    assert_eq!(text, listed);

    // A write with the bare label is the write of the entity its ticket was
    // read from, and its sentence says which of the two that was.
    let read = edit_path_read(&state, &serde_json::json!(getter.id)).await;
    let mut operation =
        edit_path_patch(&read, &[("return self._level", "return int(self._level)")]);
    operation["target"] = serde_json::json!("pkg.gauge :: Gauge.level");
    let (result, text) = edit_path_mutate(&state, &session, vec![operation]).await;
    assert_ne!(result.is_error, Some(true), "{text}");
    assert_eq!(
        text,
        format!(
            "Changed pkg.gauge :: Gauge.level #{}; 1 source unit written; the graph now holds it.",
            short(getter)
        )
    );
    assert_eq!(
        edit_path_disk(&dir, "pkg/gauge.py"),
        EDIT_PATH_GAUGE.replacen("return self._level", "return int(self._level)", 1)
    );

    // A name declared once is read back by its bare label on the connection.
    let (result, text) = connection
        .call(
            &state,
            "source",
            "get_entity_source",
            serde_json::json!({"entity_id": "pkg.gauge :: Gauge.reset"}),
        )
        .await;
    assert_ne!(result.is_error, Some(true), "{text}");
    assert!(
        text.starts_with(
            "this is the exact current source of Gauge.reset, the whole body.\npkg.gauge :: \
             Gauge.reset\nbody:\n  def reset(self):\n"
        ),
        "{text}"
    );
}

include!("edited_module_evidence.rs");

// ---- every line sent for an entity lands inside it

const EDIT_PATH_OUTSIDE_TS: &str =
    "function helper(x: number): number {\n  return x;\n}\n\nexport function user(): number {\n  return helper(1);\n}\n";

const EDIT_PATH_OUTSIDE_RS: &str =
    "pub fn helper(x: u8) -> u8 {\n    x\n}\n\npub fn user() -> u8 {\n    helper(1)\n}\n";

const EDIT_PATH_OUTSIDE_PY: &str =
    "\"\"\"Outside.\"\"\"\n\n\ndef helper(x):\n    return x\n\n\ndef user():\n    return helper(1)\n";

/// An edit holds the one declaration it names. Code sent for it that the
/// parse places before or after that declaration, an import or a `use` line
/// among it, would be written where no entity holds it, so the write is
/// refused whatever the language, and nothing is written.
#[tokio::test]
#[serial_test::serial]
async fn edit_path_code_sent_for_an_entity_never_lands_outside_it() {
    for (path, original, case, code) in [
        (
            "web/outside.ts",
            EDIT_PATH_OUTSIDE_TS,
            "an import before a function no export marks",
            "import { other } from \"./other\";\nfunction helper(x: number): number {\n  return other(x);\n}",
        ),
        (
            "web/outside.ts",
            EDIT_PATH_OUTSIDE_TS,
            "a statement after the function",
            "function helper(x: number): number {\n  return x + 1;\n}\nconsole.log(helper(2));",
        ),
        (
            "src/outside.rs",
            EDIT_PATH_OUTSIDE_RS,
            "a use line after the function",
            "pub fn helper(x: u8) -> u8 {\n    x + 1\n}\nuse std::fmt;",
        ),
        (
            "src/outside.rs",
            EDIT_PATH_OUTSIDE_RS,
            "a use line before the function",
            "use std::fmt;\npub fn helper(x: u8) -> u8 {\n    x + 1\n}",
        ),
        (
            "web/outside.ts",
            EDIT_PATH_OUTSIDE_TS,
            "a statement on the function's last line",
            "function helper(x: number): number { return x + 1; } console.log(helper(2));",
        ),
        (
            "src/outside.rs",
            EDIT_PATH_OUTSIDE_RS,
            "a use on the function's last line",
            "pub fn helper(x: u8) -> u8 { x + 1 } use std::fmt;",
        ),
        (
            "pkg/outside.py",
            EDIT_PATH_OUTSIDE_PY,
            "an import before the function",
            "import os\ndef helper(x):\n    return os.sep + x",
        ),
    ] {
        for verb in ["update", "patch"] {
            // Each case starts from its own store, so an escaped write cannot
            // change what the next case reads.
            let (dir, state) = mcp_lifecycle_fixture();
            for (unit, body) in [("pkg/__init__.py", "\"\"\"The package.\"\"\"\n"), (path, original)] {
                source_tree_conversion_fixture(
                    &state,
                    serde_json::json!({
                        "verb": "create", "target": unit, "body": body,
                        "description": "outside fixture"
                    }),
                )
                .await;
            }
            let session = mcp_test_session(&state);
            let entity = module_code_entity(&state, path, "helper");
            let read = edit_path_read(&state, &serde_json::json!(entity.id)).await;
            let roots = source_base_roots(&state);
            let operation = if verb == "update" {
                edit_path_update(&read, code)
            } else {
                edit_path_patch(&read, &[(read["body"].as_str().unwrap(), code)])
            };
            let (result, text) = edit_path_mutate(&state, &session, vec![operation]).await;
            assert_eq!(result.is_error, Some(true), "{verb}, {path}, {case}: {text}");
            assert!(text.starts_with("Nothing was written: line "), "{verb}, {case}: {text}");
            assert!(text.contains("stands outside helper"), "{verb}, {case}: {text}");
            assert!(!text.contains('\n'), "one sentence: {text}");
            assert_eq!(edit_path_disk(&dir, path), original, "{verb}, {case}: {text}");
            assert_eq!(source_base_roots(&state), roots, "{verb}, {case}");
            let after = edit_path_read(&state, &serde_json::json!(entity.id)).await;
            assert_eq!(after["body"], read["body"], "{verb}, {case}");
        }
    }
}

/// Existing wrappers remain outside the splice, while decorators, attributes,
/// comments and nested declarations stay inside the adapter's edit boundary.
#[tokio::test]
#[serial_test::serial]
async fn edit_path_boundary_keeps_owned_syntax_and_maps_multiple_edits() {
    for (path, original, edits, expected) in [
        (
            "web/outside.ts",
            EDIT_PATH_OUTSIDE_TS,
            vec![
                ("helper", "/* before */ function helper(x: number): number {\n  function nested(value: number): number { return value + 1; }\n  return nested(x);\n} // after"),
                ("user", "export function user(): number { return helper(2); }"),
            ],
            "/* before */ function helper(x: number): number {\n  function nested(value: number): number { return value + 1; }\n  return nested(x);\n} // after\n\nexport function user(): number { return helper(2); }\n",
        ),
        (
            "src/outside.rs",
            EDIT_PATH_OUTSIDE_RS,
            vec![("helper", "#[inline]\npub fn helper(x: u8) -> u8 {\n    x + 1\n} /* after */")],
            "#[inline]\npub fn helper(x: u8) -> u8 {\n    x + 1\n} /* after */\n\npub fn user() -> u8 {\n    helper(1)\n}\n",
        ),
        (
            "pkg/outside.py",
            EDIT_PATH_OUTSIDE_PY,
            vec![("helper", "@decorator\ndef helper(x):\n    return x + 1\n# after")],
            "\"\"\"Outside.\"\"\"\n\n\n@decorator\ndef helper(x):\n    return x + 1\n# after\n\n\ndef user():\n    return helper(1)\n",
        ),
    ] {
        let (dir, state) = mcp_lifecycle_fixture();
        source_tree_conversion_fixture(
            &state,
            serde_json::json!({
                "verb": "create", "target": path, "body": original,
                "description": "owned syntax fixture"
            }),
        ).await;
        let session = mcp_test_session(&state);
        let mut operations = Vec::new();
        for (name, code) in edits {
            let entity = module_code_entity(&state, path, name);
            let read = edit_path_read(&state, &serde_json::json!(entity.id)).await;
            operations.push(edit_path_update(&read, code));
        }
        let (result, text) = edit_path_mutate(&state, &session, operations).await;
        assert_ne!(result.is_error, Some(true), "{path}: {text}");
        assert_eq!(edit_path_disk(&dir, path), expected, "{path}");
    }
}
