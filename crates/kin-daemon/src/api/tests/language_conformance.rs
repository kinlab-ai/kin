// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

// The language conformance suite, route layer: the same fixture project the
// parser layer measures (`tests/language-conformance/<language>/`), committed
// into a store and driven through the agent surface over the served route.
// Every capability `capabilities.toml` lists with layer `route` is measured
// here for every language, and each language's test compares what it measures
// with what its `expect.toml` declares, status and failing checks alike, in
// either direction. The scorecard renders the same declarations.
//
// A language's store is seeded once per test run from its fixture and every
// capability works on a fresh copy, so an edit one capability makes never
// reaches another. One language runs alone with
// `cargo nextest run -p kin-daemon --lib -E 'test(/language_conformance_route_go/)'`.

/// `tests/language-conformance` at the workspace root.
fn language_conformance_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/language-conformance")
}

fn language_conformance_expect(language: &str) -> toml::Table {
    let path = language_conformance_root()
        .join(language)
        .join("expect.toml");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()))
        .parse()
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

/// The route-layer capabilities, in scorecard order.
fn language_conformance_route_capabilities() -> Vec<String> {
    let path = language_conformance_root().join("capabilities.toml");
    let catalog: toml::Table = std::fs::read_to_string(&path)
        .unwrap()
        .parse()
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    catalog["capability"]
        .as_array()
        .expect("capabilities.toml lists [[capability]] entries")
        .iter()
        .filter(|entry| entry["layer"].as_str() == Some("route"))
        .map(|entry| entry["name"].as_str().unwrap().to_string())
        .collect()
}

/// Every source of a language's fixture project, scaffolding included, as
/// (path, body), in path order. Sources under `parse-only/` are measured by
/// the parser layer alone: no grammar reads them whole, so they are not
/// committed.
fn language_conformance_sources(language: &str) -> Vec<(String, String)> {
    let root = language_conformance_root().join(language);
    let mut sources = Vec::new();
    let mut pending = vec![root.clone()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else if path.file_name().and_then(|name| name.to_str()) != Some("expect.toml")
                && !path.starts_with(root.join("parse-only"))
            {
                let relative = path
                    .strip_prefix(&root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                sources.push((relative, std::fs::read_to_string(&path).unwrap()));
            }
        }
    }
    sources.sort();
    sources
}

/// The language's fixture project committed into a fresh store.
async fn language_conformance_seeded(language: String) -> (tempfile::TempDir, Arc<DaemonState>) {
    let (dir, state) = mcp_lifecycle_fixture();
    for (path, body) in language_conformance_sources(&language) {
        source_tree_conversion_fixture(
            &state,
            serde_json::json!({
                "verb": "create", "target": path, "body": body,
                "description": "language conformance fixture"
            }),
        )
        .await;
    }
    (dir, state)
}

/// What one route capability measured.
enum RouteMeasured {
    /// Each named check and whether it held.
    Checks(Vec<(String, bool)>),
    /// Kin declined the operation for this language, with its words.
    Refused(String),
}

fn route_text<'a>(table: &'a toml::Table, key: &str) -> &'a str {
    table
        .get(key)
        .and_then(|value| value.as_str())
        .unwrap_or_else(|| panic!("the capability names no {key}"))
}

fn route_texts(table: &toml::Table, key: &str) -> Vec<String> {
    table
        .get(key)
        .and_then(|value| value.as_array())
        .map(|items| {
            items
                .iter()
                .map(|item| item.as_str().unwrap().to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// The fixture path `[sources]` gives a role.
fn route_source_path(expect: &toml::Table, role: &str) -> String {
    expect["sources"][role].as_str().unwrap().to_string()
}

/// The entity `name` in the source `role` names, when the store holds it.
fn route_entity(
    state: &DaemonState,
    expect: &toml::Table,
    role: &str,
    name: &str,
) -> Option<kin_model::Entity> {
    let path = route_source_path(expect, role);
    state
        .graph
        .query_entities(&kin_model::EntityFilter {
            file_path: Some(kin_model::FilePathId::new(&path)),
            ..Default::default()
        })
        .unwrap()
        .into_iter()
        .find(|entity| entity.name == name && !kin_model::is_file_module_surface(entity))
}

/// One read tool over the served route: its text, or `None` when it erred.
async fn route_read(
    state: &Arc<DaemonState>,
    tool: &str,
    arguments: serde_json::Value,
) -> Option<String> {
    let result = edit_path_call(state, tool, arguments).await;
    (result.is_error != Some(true)).then(|| mcp_result_text(&result))
}

/// One mutation over the served route: `Ok` with nothing when it landed,
/// `Err` with Kin's words when it was refused.
async fn route_mutate(
    state: &Arc<DaemonState>,
    operation: serde_json::Value,
) -> Result<(), String> {
    let session = mcp_test_session(state);
    let (result, text) = edit_path_mutate(state, &session, vec![operation]).await;
    if result.is_error == Some(true) {
        Err(text)
    } else {
        Ok(())
    }
}

/// The `source` read of an entity, or the refusal it drew.
async fn route_source_read(
    state: &Arc<DaemonState>,
    entity: &kin_model::Entity,
) -> Result<serde_json::Value, String> {
    let result = edit_path_call(
        state,
        "get_entity_source",
        serde_json::json!({"entity_id": entity.id}),
    )
    .await;
    if result.is_error == Some(true) {
        Err(mcp_result_text(&result))
    } else {
        Ok(tool_result_payload(&result))
    }
}

fn route_working(dir: &tempfile::TempDir, expect: &toml::Table, role: &str) -> String {
    edit_path_disk(dir, &route_source_path(expect, role))
}

/// Measure `capability` for the language of `expect` on a fresh copy of its
/// seeded store.
async fn route_measure(
    template: &SeededStore,
    expect: &toml::Table,
    capability: &str,
    table: &toml::Table,
) -> RouteMeasured {
    let (dir, state) = template.open_copy();
    let role = table
        .get("in")
        .and_then(|value| value.as_str())
        .unwrap_or("module");
    match capability {
        // The whole declaration is served: the body starts where the parser's
        // span does, with the declaration's documentation and prefixes.
        "route_source" => {
            let Some(entity) = route_entity(&state, expect, role, route_text(table, "entity"))
            else {
                return RouteMeasured::Checks(vec![
                    ("entity".into(), false),
                    ("starts".into(), false),
                ]);
            };
            let read = route_source_read(&state, &entity).await;
            let starts = read.as_ref().is_ok_and(|payload| {
                payload["body"]
                    .as_str()
                    .is_some_and(|body| body.starts_with(route_text(table, "starts")))
            });
            RouteMeasured::Checks(vec![
                ("entity".into(), read.is_ok()),
                ("starts".into(), starts),
            ])
        }
        // A search by the entity's name finds it: the answer carries its id.
        "route_locate" => {
            let Some(entity) = route_entity(&state, expect, role, route_text(table, "entity"))
            else {
                return RouteMeasured::Checks(vec![("found".into(), false)]);
            };
            let found = route_read(
                &state,
                "semantic_locate",
                serde_json::json!({"query": route_text(table, "query")}),
            )
            .await
            .is_some_and(|text| text.contains(&entity.id.to_string()));
            RouteMeasured::Checks(vec![("found".into(), found)])
        }
        // The references of a callee carry its caller's id.
        "route_refs" => {
            let Some(entity) = route_entity(&state, expect, role, route_text(table, "entity"))
            else {
                return RouteMeasured::Checks(vec![("caller".into(), false)]);
            };
            let caller_id = route_entity(&state, expect, "module", route_text(table, "caller"))
                .map(|caller| caller.id.to_string());
            let caller = route_read(
                &state,
                "find_references",
                serde_json::json!({"entity_id": entity.id}),
            )
            .await
            .is_some_and(|text| caller_id.is_some_and(|id| text.contains(&id)));
            RouteMeasured::Checks(vec![("caller".into(), caller)])
        }
        // The impact of a helper reaches its caller in another module, by id.
        "route_impact" => {
            let Some(entity) = route_entity(&state, expect, role, route_text(table, "entity"))
            else {
                return RouteMeasured::Checks(vec![("affected".into(), false)]);
            };
            let affected = route_read(
                &state,
                "impact_analysis",
                serde_json::json!({"entity_ids": [entity.id.to_string()], "include_traffic": false}),
            )
            .await
            .is_some_and(|text| {
                route_entity(&state, expect, "module", route_text(table, "affected"))
                    .is_some_and(|affected| text.contains(&affected.id.to_string()))
            });
            RouteMeasured::Checks(vec![("affected".into(), affected)])
        }
        // The context of a type carries its methods' ids.
        "route_context" => {
            let Some(entity) = route_entity(&state, expect, role, route_text(table, "entity"))
            else {
                return RouteMeasured::Checks(
                    route_texts(table, "mentions")
                        .into_iter()
                        .map(|name| (name, false))
                        .collect(),
                );
            };
            let text = route_read(
                &state,
                "get_context_pack",
                serde_json::json!({"entity_id": entity.id}),
            )
            .await
            .unwrap_or_default();
            RouteMeasured::Checks(
                route_texts(table, "mentions")
                    .into_iter()
                    .map(|name| {
                        let held = route_entity(&state, expect, role, &name)
                            .is_some_and(|member| text.contains(&member.id.to_string()));
                        (name, held)
                    })
                    .collect(),
            )
        }
        // A whole-code update of the code `source` served, with one token
        // changed, lands and writes exactly that change.
        "route_update" | "route_body_edit" => {
            let Some(entity) = route_entity(&state, expect, role, route_text(table, "entity"))
            else {
                return RouteMeasured::Checks(vec![
                    ("accepted".into(), false),
                    ("written".into(), false),
                ]);
            };
            let read = match route_source_read(&state, &entity).await {
                Ok(read) => read,
                Err(refusal) => return RouteMeasured::Refused(refusal),
            };
            let body = read["body"].as_str().unwrap_or_default().to_string();
            let (find, replace) = (route_text(table, "find"), route_text(table, "replace"));
            if !body.contains(find) {
                return RouteMeasured::Checks(vec![
                    ("accepted".into(), false),
                    ("written".into(), false),
                ]);
            }
            let before = route_working(&dir, expect, role);
            match route_mutate(
                &state,
                edit_path_update(&read, &body.replacen(find, replace, 1)),
            )
            .await
            {
                Err(refusal) => RouteMeasured::Refused(refusal),
                Ok(()) => {
                    let after = route_working(&dir, expect, role);
                    let written =
                        after == before.replacen(&body, &body.replacen(find, replace, 1), 1);
                    RouteMeasured::Checks(vec![
                        ("accepted".into(), true),
                        ("written".into(), written),
                    ])
                }
            }
        }
        // An anchored patch of one span of text lands and writes it.
        "route_patch" => {
            let Some(entity) = route_entity(&state, expect, role, route_text(table, "entity"))
            else {
                return RouteMeasured::Checks(vec![
                    ("accepted".into(), false),
                    ("written".into(), false),
                ]);
            };
            let read = match route_source_read(&state, &entity).await {
                Ok(read) => read,
                Err(refusal) => return RouteMeasured::Refused(refusal),
            };
            let (old, new) = (route_text(table, "old_text"), route_text(table, "new_text"));
            let before = route_working(&dir, expect, role);
            match route_mutate(&state, edit_path_patch(&read, &[(old, new)])).await {
                Err(refusal) => RouteMeasured::Refused(refusal),
                Ok(()) => {
                    let after = route_working(&dir, expect, role);
                    RouteMeasured::Checks(vec![
                        ("accepted".into(), true),
                        ("written".into(), after == before.replacen(old, new, 1)),
                    ])
                }
            }
        }
        // A declaration created after a sibling lands, and the graph holds it.
        "route_create_sibling" => {
            let Some(anchor) = route_entity(&state, expect, role, route_text(table, "anchor"))
            else {
                return RouteMeasured::Checks(vec![
                    ("accepted".into(), false),
                    ("created".into(), false),
                ]);
            };
            let read = match route_source_read(&state, &anchor).await {
                Ok(read) => read,
                Err(refusal) => return RouteMeasured::Refused(refusal),
            };
            let created = route_text(table, "entity");
            let leaf = created.rsplit(['.', ':']).next().unwrap_or(created);
            let kind = if anchor.kind == kin_model::EntityKind::Method {
                "method"
            } else {
                "function"
            };
            let operation = edit_path_create(
                &read,
                "sibling_after",
                kind,
                leaf,
                route_text(table, "code"),
            );
            match route_mutate(&state, operation).await {
                Err(refusal) => RouteMeasured::Refused(refusal),
                Ok(()) => RouteMeasured::Checks(vec![
                    ("accepted".into(), true),
                    (
                        "created".into(),
                        route_entity(&state, expect, role, created).is_some(),
                    ),
                ]),
            }
        }
        // A function removed takes its whole declaration out.
        "route_remove" => {
            let name = route_text(table, "entity");
            let Some(entity) = route_entity(&state, expect, role, name) else {
                return RouteMeasured::Checks(vec![
                    ("accepted".into(), false),
                    ("removed".into(), false),
                ]);
            };
            let read = match route_source_read(&state, &entity).await {
                Ok(read) => read,
                Err(refusal) => return RouteMeasured::Refused(refusal),
            };
            let body = read["body"].as_str().unwrap_or_default().to_string();
            match route_mutate(&state, edit_path_remove(&read)).await {
                Err(refusal) => RouteMeasured::Refused(refusal),
                Ok(()) => RouteMeasured::Checks(vec![
                    ("accepted".into(), true),
                    (
                        "removed".into(),
                        route_entity(&state, expect, role, name).is_none()
                            && !route_working(&dir, expect, role).contains(&body),
                    ),
                ]),
            }
        }
        other => panic!("no route measurement for capability {other}"),
    }
}

/// Measure every route capability for `language` and hold each to its
/// declaration, reporting every cell that differs.
async fn language_conformance_route(language: &str) {
    let expect = language_conformance_expect(language);
    let capabilities = language_conformance_route_capabilities();
    let owned = language.to_string();
    let template = SeededStore::cached(&format!("language-conformance-{language}"), move || {
        language_conformance_seeded(owned)
    })
    .await;
    let mut mismatches = Vec::new();
    for capability in &capabilities {
        let table = expect
            .get(capability.as_str())
            .and_then(|value| value.as_table())
            .unwrap_or_else(|| panic!("{language}/expect.toml declares no [{capability}]"));
        let declared = table["status"].as_str().unwrap();
        let declared_gaps: std::collections::BTreeSet<String> =
            route_texts(table, "gaps").into_iter().collect();
        if declared == "n/a" {
            continue;
        }
        let measured = route_measure(&template, &expect, capability, table).await;
        let (status, failing, detail) = match &measured {
            RouteMeasured::Refused(why) => (
                "refused",
                std::collections::BTreeSet::new(),
                format!("refused: {}", why.lines().next().unwrap_or_default()),
            ),
            RouteMeasured::Checks(checks) => {
                let failing: std::collections::BTreeSet<String> = checks
                    .iter()
                    .filter(|(_, held)| !held)
                    .map(|(name, _)| name.clone())
                    .collect();
                let status = if failing.is_empty() {
                    "pass"
                } else if failing.len() == checks.len() {
                    "fail"
                } else {
                    "partial"
                };
                let detail = checks
                    .iter()
                    .map(|(name, held)| format!("{} {name}", if *held { "ok" } else { "FAILED" }))
                    .collect::<Vec<_>>()
                    .join("; ");
                (status, failing, detail)
            }
        };
        if status != declared || failing != declared_gaps {
            mismatches.push(format!(
                "{language}/{capability}: tests/language-conformance/{language}/expect.toml \
                 declares {declared} with gaps {declared_gaps:?}, and the suite measured {status} \
                 with gaps {failing:?} ({detail}). When Kin changed on purpose, update the \
                 declaration and rerun scripts/language-scorecard.py --write."
            ));
        }
    }
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
}

macro_rules! language_conformance_route_tests {
    ($($test:ident => $language:literal),* $(,)?) => {
        $(
            #[tokio::test]
            #[serial_test::serial]
            async fn $test() {
                language_conformance_route($language).await;
            }
        )*
    };
}

language_conformance_route_tests! {
    language_conformance_route_python => "python",
    language_conformance_route_typescript => "typescript",
    language_conformance_route_javascript => "javascript",
    language_conformance_route_go => "go",
    language_conformance_route_rust => "rust",
    language_conformance_route_java => "java",
    language_conformance_route_kotlin => "kotlin",
    language_conformance_route_c => "c",
    language_conformance_route_cpp => "cpp",
    language_conformance_route_csharp => "csharp",
    language_conformance_route_ruby => "ruby",
    language_conformance_route_php => "php",
    language_conformance_route_swift => "swift",
    language_conformance_route_hcl => "hcl",
}
