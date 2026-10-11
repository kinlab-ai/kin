// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! `ask` on a repository admitted the way `kin init` admits one.
//!
//! One question per kind the front door routes: who calls, what can end up
//! calling within a package, what a change affects through an override, a
//! name declared in two modules, whether a function passed as a value is
//! used, which modules declare a name, where a name is, its code, test code
//! left out, and a path the question writes, which is named as not applied.
//! Each answer goes through the same message path a client's call takes.

use std::fs;
use std::path::Path;
use std::process::Command;

use kin_db::InMemoryGraph;
use kin_mcp::handlers::RequestRepositoryAuthority;
use kin_mcp::server::{process_message, McpServerConfig, SessionAuthorityMode};
use kin_mcp::SessionRegistry;
use serde_json::{json, Value};

fn run_git(repo: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", kin_git::empty_global_git_config())
        .current_dir(repo)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?} failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A small Python package: a call chain, an override, a function passed as a
/// value, one name declared in two modules, an unused helper, and a test.
fn seed(repo: &Path) {
    let write = |path: &str, content: &str| {
        let path = repo.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    };
    run_git(repo, &["init", "--initial-branch=main"]);
    run_git(repo, &["config", "user.email", "kin@example.invalid"]);
    run_git(repo, &["config", "user.name", "Kin"]);
    write("shop/__init__.py", "");
    write(
        "shop/helpers.py",
        "def make_timedelta(value):\n    return value\n\n\n\
         def render(request):\n    return str(request)\n\n\n\
         def unused_helper():\n    return 1\n",
    );
    write(
        "shop/app.py",
        "from shop.helpers import make_timedelta, render\n\n\n\
         class Session:\n    def open_session(self, app):\n        return {}\n\n\n\
         class SecureSession(Session):\n    def open_session(self, app):\n        return {\"secure\": True}\n\n\n\
         def dispatch(request):\n    return render(request)\n\n\n\
         def handle(request):\n    return dispatch(request)\n\n\n\
         def boot(app):\n    session = SecureSession().open_session(app)\n    return handle(session)\n\n\n\
         def store_callback(app):\n    app.callback = make_timedelta\n    return app\n",
    );
    write(
        "shop/render_html.py",
        "def commit_update(node):\n    return node\n",
    );
    write(
        "shop/render_text.py",
        "def commit_update(node):\n    return str(node)\n",
    );
    write(
        "shop/work.py",
        "from shop import render_html, render_text\n\n\n\
         def flush_html(node):\n    return render_html.commit_update(node)\n\n\n\
         def flush_text(node):\n    return render_text.commit_update(node)\n",
    );
    write(
        "tests/test_app.py",
        "from shop.app import handle\n\n\n\
         def test_handle():\n    assert handle(\"x\")\n",
    );
    run_git(repo, &["add", "--all"]);
    run_git(repo, &["commit", "-m", "a small shop"]);
}

fn admitted_graph(repo: &Path) -> (InMemoryGraph, kin_core::LocalRepositoryAuthorityBinding) {
    let result = kin_core::init_from_git(repo).expect("admit the repository");
    let binding = kin_core::LocalRepositoryAuthorityBinding::from_layout(&result.layout)
        .expect("bind the published repository");
    let manager = binding.open_manager().expect("open the authority");
    let lease = manager.read_authority();
    let snapshot = lease
        .workspace_graph_snapshot(&binding.workspace_id())
        .expect("materialize the workspace graph")
        .expect("the authority carries the workspace");
    let graph = InMemoryGraph::from_snapshot(snapshot).expect("load the workspace graph");
    (graph, binding)
}

async fn ask(
    graph: &InMemoryGraph,
    binding: &kin_core::LocalRepositoryAuthorityBinding,
    arguments: Value,
) -> String {
    let config = McpServerConfig {
        session_authority_mode: SessionAuthorityMode::OfflineFallback,
        repository_authority: Some(RequestRepositoryAuthority::pinned(binding.clone())),
        ..Default::default()
    };
    let message = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "ask", "arguments": arguments}})
    .to_string();
    let response = process_message(&message, graph, &config, &SessionRegistry::new())
        .await
        .expect("response");
    assert!(response.error.is_none(), "{:?}", response.error);
    let result = response.result.expect("result");
    result["content"][0]["text"]
        .as_str()
        .expect("a text reply")
        .to_string()
}

async fn ask_with(
    graph: &InMemoryGraph,
    config: &McpServerConfig,
    arguments: Value,
) -> (String, bool) {
    let message = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "ask", "arguments": arguments}})
    .to_string();
    let response = process_message(&message, graph, config, &SessionRegistry::new())
        .await
        .expect("response");
    assert!(response.error.is_none(), "{:?}", response.error);
    let result = response.result.expect("result");
    (
        result["content"][0]["text"]
            .as_str()
            .expect("a text reply")
            .to_string(),
        result["isError"].as_bool().unwrap_or(false),
    )
}

/// `reply` holds every one of `wanted`, and none of `unwanted`.
fn holds(reply: &str, wanted: &[&str], unwanted: &[&str]) {
    for text in wanted {
        assert!(reply.contains(text), "missing {text:?} in:\n{reply}");
    }
    for text in unwanted {
        assert!(!reply.contains(text), "unexpected {text:?} in:\n{reply}");
    }
}

#[tokio::test]
async fn every_routing_kind_answers_through_the_real_route() {
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    seed(&repo);
    let (graph, binding) = admitted_graph(&repo);
    let asked = |question: &'static str| {
        let graph = &graph;
        let binding = &binding;
        async move { ask(graph, binding, json!({ "question": question })).await }
    };

    // Who calls: the operation, the resolved target and the words it was
    // read from come first, then the answer in the verb's own words.
    let reply = asked("What calls `dispatch`?").await;
    assert!(
        reply.starts_with(
            "Kin ran: direct callers of shop.app :: dispatch, test code included \
             [read as \"who calls it\" from \"calls\"].\nAnswer ("
        ),
        "{reply}"
    );
    holds(
        &reply,
        &["shop.app :: handle  dispatch(request)"],
        &["Not applied"],
    );

    // What can end up calling, limited by a package named in words, tests
    // left out: the package becomes the module scope.
    let reply =
        asked("What inside the shop package can end up calling `render`? Skip tests.").await;
    holds(
        &reply,
        &[
            "every direct and indirect caller of shop.helpers :: render",
            "test code left out, within modules under shop",
            "[read as \"what can end up calling it\" from \"end up\"]",
            "shop.app :: dispatch",
            "shop.app :: handle",
            "shop.app :: boot",
        ],
        &["test_handle", "Not applied"],
    );

    // What a change affects reaches the override.
    let reply =
        asked("I want to add a required parameter to `Session.open_session`. What has to change?")
            .await;
    holds(
        &reply,
        &[
            "what a change to shop.app :: Session.open_session affects",
            "shop.app :: SecureSession.open_session",
        ],
        &[],
    );

    // A name declared in two modules: every declaration, then the callers
    // of each.
    let reply = asked("What has to change if I add a parameter to `commit_update`?").await;
    holds(
        &reply,
        &[
            "Kin found 2 declarations named commit_update.",
            "shop.render_html :: commit_update",
            "shop.render_text :: commit_update",
            "Kin then ran the direct callers of each, test code included",
            "shop.work :: flush_html",
            "shop.work :: flush_text",
        ],
        &[],
    );

    // Is it used: a function stored as a value is a use.
    let reply = asked("Is `make_timedelta` used anywhere? Can I delete it?").await;
    holds(
        &reply,
        &[
            "every use of shop.helpers :: make_timedelta (calls, imports, references)",
            "shop.app :: store_callback",
        ],
        &[],
    );

    // Which modules declare it: yes or no, then every declaration, then
    // whether that list is all of them.
    let reply = asked("Does the module render_text define a `commit_update` function?").await;
    holds(
        &reply,
        &[
            "Yes. Kin lists 2 declarations named commit_update",
            "Declarations named commit_update that Kin lists (2 held as entities, 0 not held):",
            "Kin ran: locate by name for commit_update [read as \"which modules declare it\" from \"define\"].",
        ],
        &["Beyond none named"],
    );
    assert!(
        reply.contains("These are every declaration named commit_update")
            || reply
                .contains("This list is not established as every declaration named commit_update"),
        "{reply}"
    );

    // Where is it, and its code.
    let reply = asked("Where is `make_timedelta` declared?").await;
    holds(
        &reply,
        &[
            "Kin ran: locate by name for make_timedelta [read as \"where it is declared\"",
            "shop.helpers",
        ],
        &[],
    );
    let reply = asked("Show the source code of `render`.").await;
    holds(
        &reply,
        &[
            "Kin ran: the code of shop.helpers :: render",
            "return str(request)",
        ],
        &[],
    );

    // Test code left out, and kept.
    let reply = asked("What calls `handle`? Skip tests.").await;
    holds(
        &reply,
        &["test code left out", "shop.app :: boot"],
        &["test_handle"],
    );
    let reply = asked("What calls `handle`? Check everything, tests included.").await;
    holds(
        &reply,
        &[
            "test code included",
            "shop.app :: boot",
            "tests.test_app :: test_handle",
        ],
        &[],
    );

    // A path is never an address: it is named as not applied, and the
    // answer stands on the name.
    let reply = asked("Who calls `dispatch` in shop/app.py?").await;
    holds(
        &reply,
        &[
            "Not applied: the path shop/app.py (Kin addresses code by module and name, never by path).",
            "shop.app :: handle",
        ],
        &[],
    );

    // A cue word inside a name does not route: "unused" in `unused_helper`
    // is the name, and "Where" is the question.
    let reply = asked("Where is `unused_helper` declared?").await;
    holds(
        &reply,
        &["Kin ran: locate by name for unused_helper [read as \"where it is declared\" from \"Where\"]."],
        &["whether it is used"],
    );

    // A misroute stays visible: the first line names what the question was
    // read as and the words it was read from, so the caller can ask again.
    let reply = asked("Which callers does `render` have, and what calls those?").await;
    assert!(reply.starts_with("Kin ran: "), "{reply}");
    assert!(reply.contains("[read as \""), "{reply}");
}

#[tokio::test]
async fn explain_adds_the_plan_and_a_profile_without_ask_refuses_it() {
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    seed(&repo);
    let (graph, binding) = admitted_graph(&repo);
    let reply = ask(
        &graph,
        &binding,
        json!({"question": "What calls `dispatch`?", "explain": true}),
    )
    .await;
    let plan_line = reply
        .lines()
        .find(|line| line.starts_with("Plan: "))
        .unwrap_or_else(|| panic!("{reply}"));
    let plan: Value = serde_json::from_str(&plan_line["Plan: ".len()..]).unwrap();
    assert_eq!(plan["plan"]["intent"], "callers_direct", "{plan}");
    assert_eq!(plan["plan"]["cue"], "calls", "{plan}");
    let verbs: Vec<&str> = plan["plan"]["calls"]
        .as_array()
        .unwrap()
        .iter()
        .map(|call| call["verb"].as_str().unwrap())
        .collect();
    assert_eq!(verbs, vec!["locate", "refs"], "{plan}");
    assert!(plan["verdict"].is_string(), "{plan}");

    let question = json!({"question": "What calls `dispatch`?"});
    let (refused, is_error) = ask_with(
        &graph,
        &McpServerConfig {
            session_authority_mode: SessionAuthorityMode::OfflineFallback,
            repository_authority: Some(RequestRepositoryAuthority::pinned(binding.clone())),
            allowed_tools: Some(["locate".to_string()].into_iter().collect()),
            ..Default::default()
        },
        question,
    )
    .await;
    assert!(is_error, "{refused}");
    assert!(!refused.contains("Kin ran:"), "{refused}");

    let (refused, is_error) = ask_with(
        &graph,
        &McpServerConfig {
            session_authority_mode: SessionAuthorityMode::OfflineFallback,
            ..Default::default()
        },
        json!({"question": "  "}),
    )
    .await;
    assert!(is_error, "{refused}");
    assert!(
        refused.starts_with("ask needs a question in plain words"),
        "{refused}"
    );
}

#[tokio::test]
async fn ask_is_reached_as_a_routed_command_and_alone_on_agent_ask() {
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    seed(&repo);
    let (graph, binding) = admitted_graph(&repo);
    let pinned = || Some(RequestRepositoryAuthority::pinned(binding.clone()));

    // The routed tool's `ask` command is the named `ask` call.
    let routed = McpServerConfig {
        session_authority_mode: SessionAuthorityMode::OfflineFallback,
        repository_authority: pinned(),
        allowed_tools: Some(kin_mcp::tool_name_set(kin_mcp::agent_routed_tool_names())),
        agent_belt: true,
        routed: Some(kin_mcp::routed::RoutedSurface::READ_ONLY),
        ..Default::default()
    };
    let message = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "kin", "arguments": {"command": "ask",
            "args": {"question": "What calls `dispatch`?"}}}})
    .to_string();
    let response = process_message(&message, &graph, &routed, &SessionRegistry::new())
        .await
        .expect("response");
    let reply = response.result.expect("result")["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    holds(
        &reply,
        &[
            "Kin ran: direct callers of shop.app :: dispatch",
            "shop.app :: handle",
        ],
        &[],
    );

    // `agent-ask` serves the front door alone, and every verb answers
    // through it.
    let alone = McpServerConfig {
        session_authority_mode: SessionAuthorityMode::OfflineFallback,
        repository_authority: pinned(),
        allowed_tools: Some(kin_mcp::tool_name_set(kin_mcp::agent_ask_tool_names())),
        agent_belt: true,
        plain_replies: true,
        ..Default::default()
    };
    let (reply, is_error) = ask_with(
        &graph,
        &alone,
        json!({"question": "Is `make_timedelta` used anywhere?"}),
    )
    .await;
    assert!(!is_error, "{reply}");
    holds(&reply, &["shop.app :: store_callback"], &[]);
}

#[tokio::test]
async fn the_ask_prompt_sends_kins_answer_as_the_persons_message() {
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    seed(&repo);
    let (graph, binding) = admitted_graph(&repo);
    let config = |allowed: Option<&[&str]>| McpServerConfig {
        session_authority_mode: SessionAuthorityMode::OfflineFallback,
        repository_authority: Some(RequestRepositoryAuthority::pinned(binding.clone())),
        allowed_tools: allowed.map(kin_mcp::tool_name_set),
        ..Default::default()
    };
    let send = |config: McpServerConfig, method: &'static str, params: Value| {
        let graph = &graph;
        async move {
            let message =
                json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).to_string();
            process_message(&message, graph, &config, &SessionRegistry::new())
                .await
                .expect("response")
        }
    };

    let serving = config(Some(kin_mcp::agent_ask_tool_names()));
    let initialized = send(serving.clone(), "initialize", json!({})).await;
    assert_eq!(
        initialized.result.unwrap()["capabilities"]["prompts"]["listChanged"],
        false
    );
    let listed = send(serving.clone(), "prompts/list", json!({})).await;
    assert_eq!(listed.result.unwrap()["prompts"][0]["name"], "ask");
    let got = send(
        serving.clone(),
        "prompts/get",
        json!({"name": "ask", "arguments": {"question": "What calls `dispatch`?"}}),
    )
    .await;
    let result = got.result.expect("a prompt result");
    let text = result["messages"][0]["content"]["text"].as_str().unwrap();
    assert_eq!(result["messages"][0]["role"], "user");
    holds(
        text,
        &[
            "What calls `dispatch`?\n\nKin ran this question against the repository graph",
            "Kin ran: direct callers of shop.app :: dispatch",
            "shop.app :: handle",
        ],
        &[],
    );
    let refused = send(
        serving,
        "prompts/get",
        json!({"name": "ask", "arguments": {}}),
    )
    .await;
    assert_eq!(refused.error.expect("refused").code, -32602);

    // A profile that does not serve `ask` serves no prompt and says none.
    let other = config(Some(&["locate"]));
    let initialized = send(other.clone(), "initialize", json!({})).await;
    assert!(initialized.result.unwrap()["capabilities"]
        .get("prompts")
        .is_none());
    let listed = send(other, "prompts/list", json!({})).await;
    assert_eq!(listed.result.unwrap()["prompts"], json!([]));
}
