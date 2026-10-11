// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The last cut of a plain agent reply: what a row or block only restates.
//!
//! Every reply an agent receives is re-sent to its model on every later turn,
//! so a field it cannot act on is paid for many times. Measured on the
//! head-to-head call corpus, a source reply spent a fifth of its tokens on
//! blocks that said nothing was limited, and a locate row spent more on the
//! owner record its own name already spelled than on the row itself.
//!
//! This runs on plain replies after paging, presentation and every byte
//! budget, whatever detail the call asked for, and only removes:
//!
//! - `source_qualification` and `binding_coverage` blocks that report no
//!   limitation. A block that reports one is kept whole.
//! - from each `semantic_owners` record, its relation id and the ordinary
//!   `Contains` / `type_resolved` labels, and the whole record when the row
//!   already spells it: a module owner its `module` names, or a class or
//!   function owner its dotted name starts with. An empty owner list beside a
//!   `semantic_owner_gap` goes; the gap stays.
//! - an empty `signature`, or one its row's `body` already holds (a source
//!   read and a context row), and a context row's empty `why_included` and
//!   the edge label a `why_included` group already implies.
//! - a references page's `_kin.page` accounting once nothing follows it, and
//!   an `overridden_by` block that proved no overrides over complete coverage,
//!   which is served as `{"count": 0, "coverage": "complete"}`.
//! - a reading's `limitation` that is its `clauses` joined word for word.
//! - from a reach reading, the question, statement and witness note that
//!   restate its meaning and rows. Its policy, which a meaning cites as the
//!   declared scope, and the definitions of its reason codes stay.
//!
//! Verdicts, meanings, gaps, places to check, counts, cursors and every id an
//! agent passes back are never touched.

use serde_json::{json, Map, Value};

/// Lean every JSON text block of a plain tool result in place.
pub(crate) fn lean_result(result: &mut Value) {
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        return;
    }
    let Some(blocks) = result.get_mut("content").and_then(Value::as_array_mut) else {
        return;
    };
    for block in blocks {
        let Some(text) = block.get("text").and_then(Value::as_str) else {
            continue;
        };
        let Ok(Value::Object(mut payload)) = serde_json::from_str::<Value>(text) else {
            continue;
        };
        // A fragment is a lossless record, never a reply to shorten.
        if payload.contains_key("record_fragment") || !payload.contains_key("_kin") {
            continue;
        }
        let before = text.len();
        lean_payload(&mut payload);
        if let Ok(text) = serde_json::to_string(&payload) {
            if text.len() < before {
                block["text"] = Value::String(text);
            }
        }
    }
}

/// Lean one plain payload in place.
pub(crate) fn lean_payload(payload: &mut Map<String, Value>) {
    drop_nominal_blocks(payload);
    lean_page(payload);
    lean_overrides(payload);
    lean_reach_readings(payload);
    if let Some(answers) = payload.get_mut("answers").and_then(Value::as_array_mut) {
        for answer in answers.iter_mut().filter_map(Value::as_object_mut) {
            drop_nominal_blocks(answer);
            lean_overrides(answer);
        }
    }
    let mut root = Value::Object(std::mem::take(payload));
    lean_rows(&mut root);
    // `lean_rows` reads the payload itself as a row too: a source read is one.
    if let Value::Object(map) = root {
        *payload = map;
    }
}

fn drop_nominal_blocks(payload: &mut Map<String, Value>) {
    if payload
        .get("source_qualification")
        .is_some_and(nominal_qualification)
    {
        payload.remove("source_qualification");
    }
    if payload.get("binding_coverage").is_some_and(nominal_binding) {
        payload.remove("binding_coverage");
    }
}

/// A source qualification that limits nothing on the live head.
fn nominal_qualification(block: &Value) -> bool {
    let Some(block) = block.as_object() else {
        return false;
    };
    block.get("scope").and_then(Value::as_str) == Some("live_head")
        && ["history", "relations", "source"]
            .iter()
            .all(|key| block.get(*key).and_then(Value::as_str) == Some("no_limitation"))
        && block.keys().all(|key| {
            matches!(
                key.as_str(),
                "history" | "relations" | "source" | "scope" | "tool" | "version"
            )
        })
}

/// A binding coverage that owes nothing: no local binding required, or one
/// required with no outstanding or recorded obligation.
fn nominal_binding(block: &Value) -> bool {
    let Some(block) = block.as_object() else {
        return false;
    };
    if block.get("scope").and_then(Value::as_str) != Some("live_head") {
        return false;
    }
    match block
        .get("local_binding_requirement")
        .and_then(Value::as_str)
    {
        Some("not_required") => block.keys().all(|key| {
            matches!(
                key.as_str(),
                "checked_scope" | "local_binding_requirement" | "scope"
            )
        }),
        Some("required") => {
            let Some(scope) = block.get("binding_scope").and_then(Value::as_object) else {
                return false;
            };
            scope
                .get("outstanding_local_binding_obligations")
                .and_then(Value::as_u64)
                == Some(0)
                && scope.get("prior_local_binding").and_then(Value::as_str)
                    == Some("no_recorded_debt")
                && scope.keys().all(|key| {
                    matches!(
                        key.as_str(),
                        "basis"
                            | "languages"
                            | "outstanding_local_binding_obligations"
                            | "prior_local_binding"
                    )
                })
                && block.keys().all(|key| {
                    matches!(
                        key.as_str(),
                        "binding_scope" | "checked_scope" | "local_binding_requirement" | "scope"
                    )
                })
        }
        _ => false,
    }
}

/// Page accounting on a references reply with nothing after it: the
/// delivered sentence already says so.
fn lean_page(payload: &mut Map<String, Value>) {
    let Some(envelope) = payload.get_mut("_kin").and_then(Value::as_object_mut) else {
        return;
    };
    let finished = envelope
        .get("page")
        .and_then(Value::as_object)
        .is_some_and(|page| {
            page.get("evidence_has_more").and_then(Value::as_bool) == Some(false)
                && page.get("recorded_rows_has_more").and_then(Value::as_bool) == Some(false)
        });
    if finished && envelope.contains_key("delivered") {
        envelope.remove("page");
    }
}

/// An override census that proved none over complete coverage.
fn lean_overrides(payload: &mut Map<String, Value>) {
    let Some(block) = payload.get("overridden_by").and_then(Value::as_object) else {
        return;
    };
    let zero = |key: &str| block.get(key).and_then(Value::as_u64) == Some(0);
    let empty = block
        .get("methods")
        .and_then(Value::as_array)
        .is_some_and(Vec::is_empty);
    let complete = block
        .get("coverage")
        .and_then(Value::as_object)
        .is_some_and(|coverage| {
            coverage.get("recorded_closure").and_then(Value::as_str) == Some("complete")
                && coverage
                    .get("repository_completeness")
                    .and_then(Value::as_str)
                    == Some("complete")
                && ["unadmitted_candidates", "unproven_relations"]
                    .iter()
                    .all(|key| coverage.get(*key).and_then(Value::as_u64) == Some(0))
        });
    let known = block.keys().all(|key| {
        matches!(
            key.as_str(),
            "count" | "coverage" | "methods" | "note" | "proven"
        )
    });
    if zero("count") && zero("proven") && empty && complete && known {
        payload.insert(
            "overridden_by".into(),
            json!({"count": 0, "coverage": "complete"}),
        );
    }
}

/// Fields of a reach reading that restate its meaning and rows in the same
/// words on every reply.
const STATIC_REACH_TEXT: [&str; 3] = ["question", "statement", "witness_paths"];

fn lean_reach_readings(payload: &mut Map<String, Value>) {
    let Some(readings) = payload.get_mut("readings").and_then(Value::as_array_mut) else {
        return;
    };
    for reading in readings {
        let reach = reading.get("key").and_then(Value::as_str) == Some("reach");
        let Some(value) = reading.get_mut("value").and_then(Value::as_object_mut) else {
            continue;
        };
        if reach {
            for key in STATIC_REACH_TEXT {
                value.remove(key);
            }
        }
        // A limitation that is its clauses joined, word for word.
        let joined = value
            .get("clauses")
            .and_then(Value::as_array)
            .map(|clauses| {
                clauses
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join("; ")
            });
        if joined.is_some() && value.get("limitation").and_then(Value::as_str) == joined.as_deref()
        {
            value.remove("limitation");
        }
    }
}

/// Row-level cuts, wherever a row sits in the payload.
fn lean_rows(value: &mut Value) {
    match value {
        Value::Array(items) => items.iter_mut().for_each(lean_rows),
        Value::Object(fields) => {
            lean_owners(fields);
            lean_context_row(fields);
            for (key, item) in fields.iter_mut() {
                // Code, readings and frozen records are data, never rows.
                if matches!(key.as_str(), "body" | "header" | "code" | "_kin") {
                    continue;
                }
                lean_rows(item);
            }
        }
        _ => {}
    }
}

fn lean_owners(row: &mut Map<String, Value>) {
    let Some(owners) = row.get("semantic_owners").and_then(Value::as_array) else {
        return;
    };
    if owners.is_empty() {
        if row.contains_key("semantic_owner_gap") {
            row.remove("semantic_owners");
        }
        return;
    }
    let name = row.get("name").and_then(Value::as_str).unwrap_or_default();
    let module = row.get("module").and_then(Value::as_str);
    let kept: Vec<Value> = owners
        .iter()
        .filter(|owner| !restated_owner(owner, name, module))
        .map(|owner| {
            let mut owner = owner.clone();
            if let Some(fields) = owner.as_object_mut() {
                fields.remove("relation_id");
                if fields.get("relationship").and_then(Value::as_str) == Some("Contains") {
                    fields.remove("relationship");
                }
                if fields.get("resolution").and_then(Value::as_str) == Some("type_resolved") {
                    fields.remove("resolution");
                }
            }
            owner
        })
        .collect();
    if kept.is_empty() {
        row.remove("semantic_owners");
    } else {
        row.insert("semantic_owners".into(), Value::Array(kept));
    }
}

/// An owner record the row already spells, proven by type: a module its
/// `module` names, or a class or function its dotted name starts with.
fn restated_owner(owner: &Value, name: &str, module: Option<&str>) -> bool {
    let Some(owner) = owner.as_object() else {
        return false;
    };
    let ordinary = owner
        .get("relationship")
        .and_then(Value::as_str)
        .is_none_or(|relationship| relationship == "Contains")
        && owner
            .get("resolution")
            .and_then(Value::as_str)
            .is_none_or(|resolution| resolution == "type_resolved");
    if !ordinary {
        return false;
    }
    let owner_name = owner
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if owner_name.is_empty() {
        return false;
    }
    match owner.get("kind").and_then(Value::as_str) {
        Some("module") => {
            let qualified = owner
                .get("qualified_name")
                .and_then(Value::as_str)
                .or_else(|| {
                    owner
                        .get("member_lookup_query")
                        .and_then(Value::as_str)
                        .and_then(|query| query.strip_suffix(&format!(".{name}")))
                });
            module.is_some() && qualified == module
        }
        Some(_) => name.starts_with(&format!("{owner_name}.")),
        None => false,
    }
}

/// Whether `body` holds `signature` once whitespace and the trailing commas a
/// formatter adds before a closing bracket are set aside: a source read's
/// signature is the body's own first lines, collapsed.
pub(crate) fn body_restates(body: &str, signature: &str) -> bool {
    fn squeeze(text: &str) -> String {
        let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
        compact.replace(",)", ")").replace(",]", "]")
    }
    let signature = squeeze(signature);
    !signature.is_empty() && squeeze(body).contains(&signature)
}

fn lean_context_row(row: &mut Map<String, Value>) {
    let restated = match (
        row.get("body").and_then(Value::as_str),
        row.get("signature").and_then(Value::as_str),
    ) {
        (_, Some("")) => true,
        (Some(body), Some(signature)) => body_restates(body, signature),
        _ => false,
    };
    if restated {
        row.remove("signature");
    }
    let Some(why) = row.get_mut("why_included").and_then(Value::as_array_mut) else {
        return;
    };
    for reason in why.iter_mut().filter_map(Value::as_object_mut) {
        let implied = matches!(
            (
                reason.get("dependency_relation").and_then(Value::as_str),
                reason.get("group").and_then(Value::as_str),
            ),
            (Some("dependency_edge"), Some("dependencies"))
                | (Some("dependent_edge"), Some("dependents"))
        );
        if implied {
            reason.remove("dependency_relation");
        }
    }
    if why.is_empty() {
        row.remove("why_included");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaned(payload: Value) -> Value {
        let mut result = json!({"content": [{"type": "text", "text": payload.to_string()}]});
        lean_result(&mut result);
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap()
    }

    #[test]
    fn nominal_blocks_go_and_a_limitation_stays() {
        let reply = leaned(json!({
            "_kin": {"verdict": "complete"},
            "source_qualification": {"history": "no_limitation", "relations": "no_limitation",
                "scope": "live_head", "source": "no_limitation", "tool": "source", "version": 1},
            "binding_coverage": {"checked_scope": "selected_source_bindings",
                "local_binding_requirement": "not_required", "scope": "live_head"},
        }));
        assert!(reply.get("source_qualification").is_none());
        assert!(reply.get("binding_coverage").is_none());
        let limited = json!({
            "_kin": {"verdict": "complete"},
            "source_qualification": {"history": "no_limitation", "relations": "local_binding_unrelated",
                "scope": "live_head", "source": "no_limitation", "tool": "context", "version": 1},
            "binding_coverage": {"binding_scope": {"basis": "answer_entities", "languages": ["Rust"],
                "outstanding_local_binding_obligations": 1, "prior_local_binding": "outstanding"},
                "checked_scope": "admitted_inventory", "local_binding_requirement": "required",
                "scope": "live_head"},
        });
        assert_eq!(leaned(limited.clone()), limited);
    }

    #[test]
    fn an_owner_the_row_spells_goes_and_an_owner_it_does_not_stays() {
        let reply = leaned(json!({
            "_kin": {"verdict": "complete"},
            "results": [
                {"entity_id": "a", "name": "Scaffold.route", "module": "flask.sansio.scaffold",
                 "semantic_owners": [{"entity_id": "b", "kind": "class", "name": "Scaffold",
                    "relation_id": "r", "relationship": "Contains", "resolution": "type_resolved"}]},
                {"entity_id": "c", "name": "add", "module": "js_example.views",
                 "semantic_owners": [{"entity_id": "d", "kind": "module", "name": "views",
                    "qualified_name": "js_example.views", "member_lookup_query": "js_example.views.add",
                    "relation_id": "r", "relationship": "Contains", "resolution": "type_resolved"}]},
                {"entity_id": "e", "name": "decorator", "module": "flask.sansio.scaffold",
                 "semantic_owners": [{"entity_id": "a", "kind": "method", "name": "Scaffold.route",
                    "relation_id": "r", "relationship": "Contains", "resolution": "name_only"}]},
                {"entity_id": "f", "name": "provider", "semantic_owners": [],
                 "semantic_owner_gap": "no_recorded_owner"},
            ],
        }));
        let rows = reply["results"].as_array().unwrap();
        assert!(rows[0].get("semantic_owners").is_none());
        assert!(rows[1].get("semantic_owners").is_none());
        assert_eq!(
            rows[2]["semantic_owners"],
            json!([{"entity_id": "a", "kind": "method", "name": "Scaffold.route",
                    "resolution": "name_only"}]),
            "an owner proven by name alone keeps its resolution and stays"
        );
        assert!(rows[3].get("semantic_owners").is_none());
        assert_eq!(rows[3]["semantic_owner_gap"], "no_recorded_owner");
    }

    #[test]
    fn no_overrides_over_complete_coverage_is_one_short_block() {
        let reply = leaned(json!({
            "_kin": {"verdict": "complete", "delivered": "No more rows.",
                "page": {"evidence_has_more": false, "recorded_rows_has_more": false,
                         "recorded_references_total": 7}},
            "overridden_by": {"count": 0, "proven": 0, "methods": [], "note": "n",
                "coverage": {"recorded_closure": "complete", "repository_completeness": "complete",
                    "unadmitted_candidates": 0, "unproven_relations": 0, "transitive": true}},
        }));
        assert_eq!(
            reply["overridden_by"],
            json!({"count": 0, "coverage": "complete"})
        );
        assert!(reply["_kin"].get("page").is_none());
        let open = json!({
            "_kin": {"verdict": "partial", "page": {"evidence_has_more": true,
                "recorded_rows_has_more": false}},
            "overridden_by": {"count": 0, "proven": 0, "methods": [],
                "coverage": {"recorded_closure": "partial", "repository_completeness": "complete",
                    "unadmitted_candidates": 2, "unproven_relations": 0}},
        });
        assert_eq!(leaned(open.clone()), open);
    }

    #[test]
    fn a_reach_reading_keeps_its_counts_policy_and_definitions() {
        let reading = json!({
            "members_total": 5, "frontier_total": 2,
            "frontier_folded": {"by_reason": {"constructor_dispatch": 5}, "count": 5, "why": "long"},
            "blind_spots": {"unkeyed_lookups": 13, "why": "long"},
            "policy": {"follows": ["Calls"], "not_followed": "calls through values"}});
        let mut restated = reading.clone();
        for key in ["question", "statement", "witness_paths"] {
            restated[key] = json!("restated");
        }
        let reply = leaned(json!({
            "_kin": {"verdict": "partial"},
            "readings": [{"key": "reach", "value": restated}],
        }));
        assert_eq!(reply["readings"][0]["value"], reading);
    }

    #[test]
    fn a_limitation_that_restates_its_clauses_goes() {
        let reply = leaned(json!({
            "_kin": {"verdict": "complete"},
            "readings": [{"key": "enrichment", "value": {"clauses": ["a: one", "b: two"],
                "limitation": "a: one; b: two", "pending_by_code": {"a": 1}}},
                {"key": "other", "value": {"clauses": ["a"], "limitation": "different"}}],
        }));
        assert!(reply["readings"][0]["value"].get("limitation").is_none());
        assert_eq!(
            reply["readings"][0]["value"]["clauses"],
            json!(["a: one", "b: two"])
        );
        assert_eq!(reply["readings"][1]["value"]["limitation"], "different");
    }

    #[test]
    fn context_rows_lose_only_empty_and_implied_fields() {
        let reply = leaned(json!({
            "_kin": {"verdict": "complete"},
            "entities": [{"entity_id": "a", "name": "t", "signature": "", "why_included": [
                {"dependency_relation": "dependency_edge", "focal_entity_id": "f", "group": "dependencies"},
                {"dependency_relation": "call_edge", "focal_entity_id": "f", "group": "dependencies"}]},
                {"entity_id": "b", "name": "u", "why_included": []}],
        }));
        let rows = reply["entities"].as_array().unwrap();
        assert!(rows[0].get("signature").is_none());
        assert_eq!(
            rows[0]["why_included"][0],
            json!({"focal_entity_id": "f", "group": "dependencies"})
        );
        assert_eq!(
            rows[0]["why_included"][1]["dependency_relation"],
            "call_edge"
        );
        assert!(rows[1].get("why_included").is_none());
    }

    #[test]
    fn a_signature_its_body_holds_goes() {
        let reply = leaned(json!({
            "_kin": {"verdict": "complete"},
            "entity_id": "a", "name": "Scaffold.route",
            "body": "@setupmethod\n    def route(\n        self, rule: str,\n    ) -> T:\n        pass",
            "signature": "@setupmethod def route(self, rule: str) -> T",
            "entities": [{"entity_id": "b", "name": "f", "signature": "def f(x)", "body": "def g(x): f(x)"}],
        }));
        assert!(reply.get("signature").is_none());
        assert_eq!(
            reply["entities"][0]["signature"], "def f(x)",
            "a body that does not hold it keeps it"
        );
    }

    #[test]
    fn fragments_and_errors_pass_unchanged() {
        let fragment = json!({"_kin": {"verdict": "complete"}, "record_fragment": {"text": "x"},
            "source_qualification": {"history": "no_limitation", "relations": "no_limitation",
                "scope": "live_head", "source": "no_limitation"}});
        assert_eq!(leaned(fragment.clone()), fragment);
        let mut error = json!({"isError": true, "content": [{"type": "text", "text": "{\"_kin\":{},\"source_qualification\":{}}"}]});
        let before = error.clone();
        lean_result(&mut error);
        assert_eq!(error, before);
    }
}
