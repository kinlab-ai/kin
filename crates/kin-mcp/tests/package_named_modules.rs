// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! A JavaScript package is named by the manifest its tree holds, through a
//! real admission.
//!
//! The rule reads one fact from the graph: whether the tree it holds has a
//! `package.json` directly in a directory. This test admits a Git repository
//! the way `kin init` does and checks that the manifest is an artifact of
//! that tree, that a file of a package with no `index` file at its root is
//! placed in the package, and that the package is a scope a callers walk
//! resolves.

use std::fs;
use std::path::Path;
use std::process::Command;

use kin_db::InMemoryGraph;
use kin_mcp::server::{process_message, McpServerConfig, SessionAuthorityMode};
use kin_mcp::SessionRegistry;
use kin_model::graph::{EntityFilter, EntityStore};
use kin_model::{Entity, EntityKind, RepoPath};
use kin_ranking::module_qualifier::{module_path, ModuleIndex};
use serde_json::{json, Value};

const BINDINGS_MANIFEST: &str = "packages/wire-bindings/package.json";
const BINDINGS_FILE: &str = "packages/wire-bindings/src/client/Component.js";
const CORE_FILE: &str = "packages/core/src/Loop.js";

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

/// Two packages under `packages/`. `core` has an `index` file at its root;
/// `wire-bindings` keeps every source file under `src/` and declares a name
/// in its manifest that its directory does not have.
fn seed(repo: &Path) {
    let write = |path: &str, content: &str| {
        let path = repo.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    };
    run_git(repo, &["init", "--initial-branch=main"]);
    run_git(repo, &["config", "user.email", "kin@example.invalid"]);
    run_git(repo, &["config", "user.name", "Kin"]);
    write(
        "package.json",
        "{\"private\": true, \"workspaces\": [\"packages/*\"]}\n",
    );
    write(
        BINDINGS_MANIFEST,
        "{\"name\": \"@acme/wire\", \"version\": \"1.0.0\"}\n",
    );
    write(
        BINDINGS_FILE,
        "export function styles(element) {\n  return element.style;\n}\n\n\
         export function mount(element) {\n  return styles(element);\n}\n",
    );
    write(
        "packages/core/package.json",
        "{\"name\": \"core\", \"version\": \"1.0.0\"}\n",
    );
    write(
        "packages/core/index.js",
        "export {commit} from './src/Loop';\n",
    );
    write(
        CORE_FILE,
        "import {styles} from '../../wire-bindings/src/client/Component';\n\n\
         export function commit(element) {\n  return styles(element);\n}\n",
    );
    run_git(repo, &["add", "--all"]);
    run_git(repo, &["commit", "-m", "two packages"]);
}

/// The graph a query is answered from after a real admission of `repo`.
fn admitted_graph(repo: &Path) -> InMemoryGraph {
    let result = kin_core::init_from_git(repo).expect("admit the repository");
    let binding = kin_core::LocalRepositoryAuthorityBinding::from_layout(&result.layout)
        .expect("bind the published repository");
    let manager = binding.open_manager().expect("open the authority");
    let lease = manager.read_authority();
    let snapshot = lease
        .workspace_graph_snapshot(&binding.workspace_id())
        .expect("materialize the workspace graph")
        .expect("the authority carries the workspace");
    InMemoryGraph::from_snapshot(snapshot).expect("load the workspace graph")
}

fn declared(graph: &InMemoryGraph, kind: EntityKind, name: &str, file: &str) -> Entity {
    graph
        .query_entities(&EntityFilter {
            kinds: Some(vec![kind]),
            name_pattern: Some(name.to_string()),
            ..Default::default()
        })
        .unwrap()
        .into_iter()
        .find(|entity| {
            entity.name == name
                && entity
                    .file_origin
                    .as_ref()
                    .is_some_and(|path| path.0 == file)
        })
        .unwrap_or_else(|| panic!("{kind:?} {name} in {file}"))
}

async fn asked(graph: &InMemoryGraph, tool: &str, arguments: Value) -> Value {
    let config = McpServerConfig {
        session_authority_mode: SessionAuthorityMode::OfflineFallback,
        ..Default::default()
    };
    let message = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": tool, "arguments": arguments}})
    .to_string();
    let response = process_message(&message, graph, &config, &SessionRegistry::new())
        .await
        .expect("response");
    assert!(response.error.is_none(), "{tool}: {:?}", response.error);
    let text = response.result.expect("result")["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    serde_json::from_str(&text).unwrap_or_else(|_| panic!("{tool}: {text}"))
}

#[tokio::test]
async fn an_admitted_package_manifest_names_the_modules_of_its_package() {
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    seed(&repo);
    let graph = admitted_graph(&repo);

    // The manifest is an artifact of the tree the graph holds, like every
    // other tracked file, and the directory beside it holds none.
    let at = |path: &str| graph.artifact_id_at_path(&RepoPath::from_utf8(path).unwrap());
    assert!(at(BINDINGS_MANIFEST).is_some());
    assert!(at("packages/core/package.json").is_some());
    assert!(at("packages/wire-bindings/src/package.json").is_none());

    let styles = declared(&graph, EntityKind::Function, "styles", BINDINGS_FILE);
    let mount = declared(&graph, EntityKind::Function, "mount", BINDINGS_FILE);
    let commit = declared(&graph, EntityKind::Function, "commit", CORE_FILE);
    let index = ModuleIndex::load(&graph).unwrap();
    // The package names the level by its directory, not by the name its
    // manifest declares, and the folders under it stay out of the name.
    assert_eq!(index.path(&styles), vec!["wire-bindings", "Component"]);
    assert_eq!(index.path(&mount), vec!["wire-bindings", "Component"]);
    // A package its `index` file already names is named once.
    assert_eq!(index.path(&commit), vec!["core", "Loop"]);
    let surface = declared(&graph, EntityKind::Module, "Component", BINDINGS_FILE);
    assert_eq!(
        module_path(&graph, &surface).unwrap().as_deref(),
        Some("wire-bindings.Component")
    );

    // The label reads back, as a reply prints it now and as one printed it
    // before the package was a level, and the package is a scope.
    for spelling in ["wire-bindings.Component :: styles", "Component :: styles"] {
        let scoped = asked(
            &graph,
            "trace",
            json!({"focal": spelling, "direction": "callers", "scope": "wire-bindings"}),
        )
        .await;
        assert_eq!(scoped["focal_id"], styles.id.to_string(), "{spelling}");
        assert_eq!(scoped["scope"]["module"], "wire-bindings", "{scoped}");
        let members: Vec<(Option<&str>, Option<&str>)> = scoped["reach"]["members"]
            .as_array()
            .unwrap_or_else(|| panic!("{scoped}"))
            .iter()
            .map(|member| (member["module"].as_str(), member["name"].as_str()))
            .collect();
        assert_eq!(
            members,
            vec![(Some("wire-bindings.Component"), Some("mount"))],
            "{scoped}"
        );
    }
}
