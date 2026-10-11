// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Page-local, exactly expandable trace aliases for explicit query profiles.
//! The graph, machine answer and frozen cursor remain unchanged. Every row and
//! site stays here; only repeated strings receive an explicit reference.

use serde_json::Value;

pub(crate) const EXPAND: &str = "Trace encoding v1: replace frontier why_ref with reach.frontier_why[why_ref] as why; replace chain code_from_site with that step sites[code_from_site].code as code. Remove the reference keys, legend and trace_encoding to recover every original field.";

/// Compact JSON text blocks only when both decoded and JSON-quoted bytes fall.
/// Errors, prose, collisions and nongaining pages pass through byte for byte.
pub(crate) fn present(result: &mut Value) {
    if result.get("isError").and_then(Value::as_bool) == Some(true)
        || result.get("structuredContent").is_some()
    {
        return;
    }
    let Some(blocks) = result.get_mut("content").and_then(Value::as_array_mut) else {
        return;
    };
    for block in blocks {
        if block.get("type").and_then(Value::as_str) != Some("text") {
            continue;
        }
        let Some(original) = block.get("text").and_then(Value::as_str) else {
            continue;
        };
        let Ok(mut payload) = serde_json::from_str::<Value>(original) else {
            continue;
        };
        if !alias(&mut payload) {
            continue;
        }
        let Ok(encoded) = serde_json::to_string(&payload) else {
            continue;
        };
        // JSON-RPC wraps the text as a JSON string. Both actual representations
        // must shrink; UTF-8 chars and escaped wire bytes are different units.
        if encoded.len() < original.len()
            && serde_json::to_string(&encoded).unwrap().len()
                < serde_json::to_string(original).unwrap().len()
        {
            block["text"] = Value::String(encoded);
        }
    }
}

fn alias(payload: &mut Value) -> bool {
    let Some(object) = payload.as_object() else {
        return false;
    };
    if object.contains_key("trace_encoding")
        || payload
            .pointer("/reach")
            .and_then(Value::as_object)
            .is_some_and(|reach| reach.contains_key("frontier_why"))
        || payload
            .pointer("/reach/frontier")
            .and_then(Value::as_array)
            .is_some_and(|rows| {
                rows.iter()
                    .any(|row| !row.is_object() || row.get("why_ref").is_some())
            })
        || payload
            .get("chain")
            .and_then(Value::as_array)
            .is_some_and(|steps| {
                steps
                    .iter()
                    .any(|step| !step.is_object() || step.get("code_from_site").is_some())
            })
    {
        return false;
    }
    let mut changed = false;
    if let Some(frontier) = payload
        .pointer_mut("/reach/frontier")
        .and_then(Value::as_array_mut)
    {
        let mut counts = std::collections::HashMap::<String, usize>::new();
        for row in frontier.iter() {
            if let Some(why) = row.get("why").and_then(Value::as_str) {
                *counts.entry(why.to_owned()).or_default() += 1;
            }
        }
        let mut legend = Vec::<String>::new();
        for row in frontier.iter_mut() {
            let Some(why) = row.get("why").and_then(Value::as_str) else {
                continue;
            };
            if why.len() < 80 || counts.get(why).copied().unwrap_or(0) < 2 {
                continue;
            }
            let index = match legend.iter().position(|reason| reason == why) {
                Some(index) => index,
                None => {
                    legend.push(why.to_owned());
                    legend.len() - 1
                }
            };
            let row = row.as_object_mut().unwrap();
            row.remove("why");
            row.insert("why_ref".into(), serde_json::json!(index));
        }
        if !legend.is_empty() {
            payload["reach"]["frontier_why"] = serde_json::json!(legend);
            changed = true;
        }
    }
    if let Some(chain) = payload.get_mut("chain").and_then(Value::as_array_mut) {
        for step in chain {
            let Some(code) = step.get("code").and_then(Value::as_str) else {
                continue;
            };
            if step.pointer("/sites/0/code").and_then(Value::as_str) != Some(code)
                || serde_json::to_string(code).unwrap().len() + "\"code\":".len()
                    <= "\"code_from_site\":0".len()
            {
                continue;
            }
            let step = step.as_object_mut().unwrap();
            step.remove("code");
            step.insert("code_from_site".into(), serde_json::json!(0));
            changed = true;
        }
    }
    if changed {
        payload["trace_encoding"] = serde_json::json!({"version":1, "expand":EXPAND});
    }
    changed
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    pub(crate) fn fixture() -> Value {
        let why = "This receiver may dispatch to more than one implementation. This frontier row records a possible call and its source site; it does not prove which implementation runs for every runtime value.";
        let code = format!(
            "send_packet(payload, destination=\"{}\")",
            "public_synthetic_destination_".repeat(5)
        );
        let frontier: Vec<_> = (0..60)
            .map(|i| {
                json!({
                    "entity_id": format!("00000000-0000-0000-0000-{i:012}"),
                    "kind": "dispatch", "file": "public_fixture.py", "line": i+10,
                    "proof": "possible_dispatch", "why": why
                })
            })
            .collect();
        let chain: Vec<_> = (0..12).map(|i| json!({
            "hop": i, "parent_hop": if i == 0 { Value::Null } else { json!(i-1) },
            "entity_id": format!("10000000-0000-0000-0000-{i:012}"),
            "code": code, "sites": [{"file":"public_fixture.py", "line":i+100, "code":code}],
            "via":"calls", "proven_path":true
        })).collect();
        json!({
            "_kin":{"verdict":"partial", "meaning":"Proven paths and possible dispatch are separate.",
                "delivered":"Send next_cursor as cursor for the next frozen page."},
            "chain":chain,
            "reach":{"members":[{"entity_id":chain[0]["entity_id"], "proven_path":true}],
                "frontier":frontier, "withheld":{"dispatch":7}},
            "next_cursor":"public-synthetic-frozen-page-token", "total_steps":19,
            "record_fragment":{"record":"public-fragment-id", "offset":0, "utf8":"δ"}
        })
    }

    pub(crate) fn result(value: &Value) -> Value {
        json!({"content":[{"type":"text", "text":serde_json::to_string(value).unwrap()}]})
    }

    pub(crate) fn body(result: &Value) -> Value {
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap()
    }

    /// Test-side independent decoder follows only the served contract.
    pub(crate) fn expand(mut value: Value) -> Value {
        if value.get("trace_encoding").is_none() {
            return value;
        }
        assert_eq!(
            value["trace_encoding"],
            json!({"version":1, "expand":EXPAND})
        );
        value.as_object_mut().unwrap().remove("trace_encoding");
        let legend = value
            .get_mut("reach")
            .and_then(Value::as_object_mut)
            .and_then(|reach| reach.remove("frontier_why"))
            .unwrap_or(json!([]));
        if let Some(rows) = value
            .pointer_mut("/reach/frontier")
            .and_then(Value::as_array_mut)
        {
            for row in rows {
                let row = row.as_object_mut().unwrap();
                if let Some(index) = row.remove("why_ref") {
                    row.insert(
                        "why".into(),
                        legend[index.as_u64().unwrap() as usize].clone(),
                    );
                }
            }
        }
        if let Some(steps) = value.get_mut("chain").and_then(Value::as_array_mut) {
            for step in steps {
                let step = step.as_object_mut().unwrap();
                if let Some(index) = step.remove("code_from_site") {
                    let code = step["sites"][index.as_u64().unwrap() as usize]["code"].clone();
                    step.insert("code".into(), code);
                }
            }
        }
        value
    }

    #[test]
    fn fresh_public_fixture_is_smaller_and_expands_every_proof_and_fragment() {
        let original = fixture();
        let mut response = result(&original);
        let before_wire = serde_json::to_vec(&response).unwrap().len();
        assert_eq!(
            response["content"][0]["text"].as_str().unwrap().len(),
            27_148
        );
        present(&mut response);
        let encoded = body(&response);
        assert_eq!(
            response["content"][0]["text"].as_str().unwrap().len(),
            14_284
        );
        assert!(serde_json::to_vec(&response).unwrap().len() < before_wire);
        assert_eq!(encoded["reach"]["frontier"].as_array().unwrap().len(), 60);
        assert_eq!(encoded["chain"].as_array().unwrap().len(), 12);
        assert_eq!(expand(encoded), original);
    }

    #[test]
    fn eligible_reasons_use_utf8_bytes_and_unique_or_short_reasons_stay_inline() {
        let mut original = fixture();
        original["chain"] = json!([]);
        let rows = original["reach"]["frontier"].as_array_mut().unwrap();
        for row in rows.iter_mut().take(20) {
            row["why"] = json!("é".repeat(40));
        }
        for row in rows.iter_mut().skip(20).take(20) {
            row["why"] = json!("a".repeat(79));
        }
        for (i, row) in rows.iter_mut().enumerate().skip(40) {
            row["why"] = json!(format!("{i}:{}", "distinct".repeat(20)));
        }
        let mut response = result(&original);
        present(&mut response);
        let encoded = body(&response);
        assert!(encoded["reach"]["frontier"][0].get("why_ref").is_some());
        assert_eq!(
            encoded["reach"]["frontier"][20]["why"],
            original["reach"]["frontier"][20]["why"]
        );
        assert_eq!(
            encoded["reach"]["frontier"][40]["why"],
            original["reach"]["frontier"][40]["why"]
        );
        assert_eq!(expand(encoded), original);
    }

    #[test]
    fn matching_code_alias_keeps_all_sites_and_unequal_code() {
        let mut original = fixture();
        original["chain"][0]["code"] = json!("distinct top-level code");
        original["chain"][1]["sites"]
            .as_array_mut()
            .unwrap()
            .push(json!({"code":"second site", "line":777}));
        original["chain"][2]["code"] = json!("quoted \"δ\"\\n code".repeat(20));
        original["chain"][2]["sites"][0]["code"] = original["chain"][2]["code"].clone();
        let mut response = result(&original);
        present(&mut response);
        let encoded = body(&response);
        assert_eq!(encoded["chain"][0]["code"], original["chain"][0]["code"]);
        assert_eq!(encoded["chain"][1]["code_from_site"], 0);
        assert_eq!(encoded["chain"][1]["sites"], original["chain"][1]["sites"]);
        assert_eq!(expand(encoded), original);
    }

    #[test]
    fn reserved_fields_and_errors_are_byte_identical() {
        for pointer in [
            "/trace_encoding",
            "/reach/frontier_why",
            "/reach/frontier/0/why_ref",
            "/chain/0/code_from_site",
        ] {
            let mut original = fixture();
            let (parent, field) = pointer.rsplit_once('/').unwrap();
            let parent = if parent.is_empty() {
                &mut original
            } else {
                original.pointer_mut(parent).unwrap()
            };
            parent
                .as_object_mut()
                .unwrap()
                .insert(field.into(), Value::Null);
            let mut response = result(&original);
            let before = response.clone();
            present(&mut response);
            assert_eq!(response, before, "{pointer}");
        }
        let mut error = result(&fixture());
        error["isError"] = json!(true);
        let before = error.clone();
        present(&mut error);
        assert_eq!(error, before);
    }

    #[test]
    fn nongaining_small_page_and_prose_are_byte_identical() {
        let tiny = json!({"chain":[{"code":"x", "sites":[{"code":"x"}]}],
            "reach":{"frontier":[{"why":"z".repeat(80)},{"why":"z".repeat(80)}]},
            "next_cursor":null});
        for mut response in [
            result(&tiny),
            json!({"content":[{"type":"text", "text":"A plain error note."}]}),
        ] {
            let before = response.clone();
            present(&mut response);
            assert_eq!(response, before);
        }
    }

    #[test]
    fn repeated_presentation_is_idempotent_and_second_page_has_its_own_legend() {
        let original = fixture();
        let mut first = result(&original);
        present(&mut first);
        assert!(body(&first).get("trace_encoding").is_some());
        let before = first.clone();
        present(&mut first);
        assert_eq!(first, before);
        let mut page = original.clone();
        page["next_cursor"] = Value::Null;
        for row in page["reach"]["frontier"].as_array_mut().unwrap() {
            row["why"] = json!("page two separate reason ".repeat(8));
        }
        let mut second = result(&page);
        present(&mut second);
        assert_ne!(
            body(&first)["reach"]["frontier_why"],
            body(&second)["reach"]["frontier_why"]
        );
        assert_eq!(expand(body(&second)), page);
    }

    #[test]
    fn semantic_record_fragment_bytes_and_identity_are_untouched() {
        let mut original = fixture();
        original["record_fragment"] = json!({"collection":"chain", "entity_id":"public-fragment-entity",
            "hop":19, "parent_hop":7, "field":"code", "encoding":"json_utf8", "byte_offset":8,
            "total_bytes":4000, "field_complete":false, "record_complete":false, "text":"\\\"δ\\n"});
        let mut response = result(&original);
        present(&mut response);
        let encoded = body(&response);
        assert!(encoded.get("trace_encoding").is_some());
        assert_eq!(encoded["record_fragment"], original["record_fragment"]);
        assert_eq!(encoded["next_cursor"], original["next_cursor"]);
        assert_eq!(expand(encoded), original);
    }

    #[test]
    fn structured_results_and_nontext_blocks_are_byte_identical() {
        for structured in [Value::Null, json!({"machine":"content"})] {
            let mut response = result(&fixture());
            response["structuredContent"] = structured;
            let before = response.clone();
            present(&mut response);
            assert_eq!(response, before);
        }
        let mut response = result(&fixture());
        response["content"][0]["type"] = json!("resource");
        let before = response.clone();
        present(&mut response);
        assert_eq!(response, before);
    }
}
