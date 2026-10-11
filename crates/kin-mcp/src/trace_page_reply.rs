// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Page-local trace presentation for the explicit compact query profiles.
//! The daemon owns the frozen cache; a client renders only the complete
//! readings and transport metadata that actually arrived on this page.

use std::collections::{HashMap, HashSet};

use serde_json::{json, Map, Value};

pub(crate) fn present(result: &mut Value, arguments: &HashMap<String, Value>) {
    if result.get("structuredContent").is_some() {
        return;
    }
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        crate::plain_reply::present(result, "trace_data_flow");
        return;
    }
    let original = result.clone();
    crate::plain_reply::present(result, "trace_data_flow");
    crate::trace_compact::present(result);
    let (Some(before), Some(after)) = (
        original.get("content").and_then(Value::as_array),
        result.get_mut("content").and_then(Value::as_array_mut),
    ) else {
        return;
    };
    for (original_block, block) in before.iter().zip(after) {
        if original_block.get("type").and_then(Value::as_str) != Some("text") {
            *block = original_block.clone();
            continue;
        }
        let Some(text) = original_block.get("text").and_then(Value::as_str) else {
            continue;
        };
        let Ok(Value::Object(machine)) = serde_json::from_str::<Value>(text) else {
            *block = original_block.clone();
            continue;
        };
        // Already-plain replies retain the existing exactly expandable codec.
        // Only raw machine pages need the new summary/fragment validation.
        if machine
            .get("_kin")
            .is_some_and(|kin| kin.get("meaning").is_some())
        {
            continue;
        }
        if machine.contains_key("record_fragment") {
            *block = original_block.clone();
            continue;
        }
        if machine
            .get("_kin")
            .and_then(|kin| kin.get("page"))
            .is_none()
        {
            continue;
        }
        // A fragment, incomplete wrapper, conflict or missing summary stays
        // exactly as the bounded transport wrote it, including its proof.
        let Some((semantic, page, cap)) = observe(&machine, arguments) else {
            *block = original_block.clone();
            continue;
        };
        let mut plain = crate::plain_reply::plain_trace_page(machine.clone(), &semantic);
        let Some(envelope) = plain.get_mut("_kin").and_then(Value::as_object_mut) else {
            *block = original_block.clone();
            continue;
        };
        envelope.insert(
            "page".into(),
            crate::plain_reply::plain_delivery_page(page.clone()),
        );
        // These observations belong to the current page. A retained original
        // envelope reading may not replace its verdict, identity or paging.
        for key in [
            "graph_as_of",
            "repository",
            "answered_by",
            "model_slots",
            "model_slots_invalid",
        ] {
            if let Some(value) = machine.get("_kin").and_then(|kin| kin.get(key)) {
                envelope.insert(key.into(), value.clone());
            }
        }
        if let Some(warning) = machine
            .get("_kin")
            .and_then(|kin| kin.pointer("/repository/warning"))
        {
            envelope.insert("warning".into(), warning.clone());
        }
        envelope.remove("delivered");
        if let Some(delivered) = delivered(&page) {
            envelope.insert("delivered".into(), json!(delivered));
        }
        // Normalization is a view for the lead and gaps, never a rewrite of
        // the complete readings or a projection of retained evidence.
        if let Some(readings) = machine.get("readings") {
            plain["readings"] = readings.clone();
        }
        retain_proof_fields(&machine, &mut plain);
        let mut rendered = json!({"content":[{"type":"text", "text":plain.to_string()}]});
        crate::trace_compact::present(&mut rendered);
        let Some(encoded) = rendered["content"][0]["text"].as_str() else {
            *block = original_block.clone();
            continue;
        };
        // UTF-8 bytes after both renderers are the emitted text budget. Do
        // not cut a proof or consume a cursor to make presentation fit.
        if encoded.len() <= cap {
            block["text"] = json!(encoded);
        } else {
            *block = original_block.clone();
        }
    }
}

fn observe(
    machine: &Map<String, Value>,
    arguments: &HashMap<String, Value>,
) -> Option<(Map<String, Value>, Map<String, Value>, usize)> {
    if machine.contains_key("record_fragment") {
        return None;
    }
    let kin = machine.get("_kin")?.as_object()?;
    let mut page = kin.get("page")?.as_object()?.clone();
    if page.get("version")?.as_u64()? != 1 || !kin.get("verdict")?.is_object() {
        return None;
    }
    let complete = page.get("complete")?.as_bool()?;
    let more = page.get("has_more")?.as_bool()?;
    let cursor = machine.get("next_cursor")?;
    if more != cursor.is_string() || (!more && !cursor.is_null()) || (complete && more) {
        return None;
    }
    let first = if complete {
        true
    } else {
        let start = page.get("record_offset")?.as_u64()?;
        let next = page.get("next_record")?.as_u64()?;
        let total = page.get("total_records")?.as_u64()?;
        if start >= total || next < start || next > total || more != (next < total) {
            return None;
        }
        for key in ["field_offset", "byte_offset", "next_field", "next_byte"] {
            if page.get(key)?.as_u64()? != 0 {
                return None;
            }
        }
        start == 0
    };
    machine.get("chain")?.as_array()?;
    page.get("total_steps")?.as_u64()?;
    let mut semantic = machine.clone();
    if let Some(question) = kin.get("trace_question").and_then(Value::as_object) {
        for key in ["direction", "depth", "limit_per_step", "focal_name"] {
            if let Some(value) = question.get(key) {
                merge(&mut semantic, key, value)?;
            }
        }
    }
    if let Some(readings) = machine.get("readings") {
        let mut keys = HashSet::new();
        for reading in readings.as_array()? {
            let wrapper = reading.as_object()?;
            if wrapper.len() != 2 {
                return None;
            }
            let key = wrapper.get("key")?.as_str()?;
            let value = wrapper.get("value")?;
            if key.is_empty() || !keys.insert(key) {
                return None;
            }
            match key {
                "focal_name"
                | "focal_id"
                | "direction"
                | "depth"
                | "focal_entity"
                | "terminal_bound_steps"
                | "terminal_coverage_gap_steps"
                | "focal_terminal"
                | "limit_per_step"
                | "scope"
                | "reach" => merge(&mut semantic, key, value)?,
                "_kin" => {
                    let held = value.as_object()?;
                    for identity in ["repository", "graph_as_of", "answered_by"] {
                        if let (Some(current), Some(recorded)) =
                            (kin.get(identity), held.get(identity))
                        {
                            if current != recorded {
                                return None;
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
    // Initial requests can supply the scope before its complete reading
    // arrives. Bare cursors use only this page's frozen readings.
    if first
        && arguments
            .get("cursor")
            .is_none_or(|cursor| cursor.as_str().is_none_or(|cursor| cursor.is_empty()))
    {
        for key in ["direction", "depth"] {
            if let Some(value) = arguments.get(key) {
                merge(&mut semantic, key, value)?;
            }
        }
    }
    let direction = semantic
        .get("direction")
        .and_then(Value::as_str)
        .or_else(|| semantic.get("reach").map(|_| "callers"))?
        .to_owned();
    if !matches!(direction.as_str(), "callers" | "calls" | "both") {
        return None;
    }
    if !semantic
        .get("depth")?
        .as_u64()
        .is_some_and(|depth| (1..=8).contains(&depth))
    {
        return None;
    }
    let focal = semantic
        .get("focal_name")
        .and_then(Value::as_str)
        .or_else(|| semantic.get("focal_entity")?.get("entity_name")?.as_str())
        .or_else(|| focal_from_reach(&semantic))?
        .to_owned();
    if focal.is_empty() {
        return None;
    }
    if !semantic.contains_key("focal_name") {
        semantic.insert("focal_name".into(), json!(focal));
    }
    for key in ["focal_name", "focal_id"] {
        if semantic.get(key).is_some_and(|value| !value.is_string()) {
            return None;
        }
    }
    for key in [
        "terminal_bound_steps",
        "terminal_coverage_gap_steps",
        "limit_per_step",
    ] {
        if semantic.get(key).is_some_and(|value| !value.is_u64()) {
            return None;
        }
    }
    if semantic
        .get("focal_terminal")
        .is_some_and(|value| !value.is_string())
    {
        return None;
    }
    let domain = if direction == "callers" {
        "reach.members"
    } else {
        "chain"
    };
    let (total, returned) = if domain == "reach.members" {
        let reach = match semantic.get("reach") {
            Some(reach) => Some(reach.as_object()?),
            None => None,
        };
        for key in ["members_total", "frontier_total"] {
            if reach
                .and_then(|reach| reach.get(key))
                .is_some_and(|value| !value.is_u64())
            {
                return None;
            }
        }
        let total = reach
            .and_then(|reach| reach.get("members_total"))
            .and_then(Value::as_u64);
        // A current page's members are not the frozen answer's total.
        // The legacy lead's row-count fallback cannot establish that total.
        if total.is_none() && reach.is_some_and(|reach| reach.contains_key("members")) {
            return None;
        }
        let returned = match machine.get("reach") {
            Some(reach) => match reach.as_object()?.get("members") {
                Some(members) => members.as_array()?.len() as u64,
                None => 0,
            },
            None => 0,
        };
        (total, returned)
    } else {
        (
            page.get("total_steps").and_then(Value::as_u64),
            machine.get("chain")?.as_array()?.len() as u64,
        )
    };
    if total.is_some_and(|total| returned > total) {
        return None;
    }
    // A frozen callers walk says on every page where its reach rows end, so
    // a later page knows whether any answer row remains.
    let published = (domain == "reach.members"
        && page
            .get("total_answer_records")
            .and_then(Value::as_u64)
            .is_some_and(|end| end > 0))
    .then(|| page.get("answer_rows_complete").and_then(Value::as_bool))
    .flatten();
    let rows_complete = if !more {
        Some(true)
    } else if let Some(complete) = published {
        Some(complete)
    } else if first {
        total.map(|total| returned == total)
    } else {
        None
    };
    page.insert("answer_domain".into(), json!(domain));
    page.insert("recorded_answer_rows_total".into(), json!(total));
    page.insert("returned_answer_rows".into(), json!(returned));
    page.insert("answer_rows_complete".into(), json!(rows_complete));
    page.insert(
        "answer_rows_has_more".into(),
        json!(rows_complete.map(|complete| !complete)),
    );
    page.insert(
        "evidence_has_more".into(),
        json!(if !more {
            Some(false)
        } else if rows_complete == Some(true) {
            Some(true)
        } else {
            None
        }),
    );
    let cap = kin.get("response")?.get("max_chars")?.as_u64()?;
    for key in ["max_output_bytes", "max_response_chars", "max_chars"] {
        if arguments.get(key).is_some_and(|value| !value.is_u64()) {
            return None;
        }
    }
    let requested = ["max_output_bytes", "max_response_chars", "max_chars"]
        .into_iter()
        .filter_map(|key| arguments.get(key).and_then(Value::as_u64))
        .min()
        .unwrap_or(cap);
    let cap = usize::try_from(cap.min(requested)).ok()?;
    (cap > 0).then_some((semantic, page, cap))
}

fn merge(semantic: &mut Map<String, Value>, key: &str, held: &Value) -> Option<()> {
    if let Some(current) = semantic.get_mut(key) {
        if let (Some(current), Some(held)) = (current.as_object_mut(), held.as_object()) {
            for (key, value) in held {
                if current.get(key).is_some_and(|current| current != value) {
                    return None;
                }
                current.entry(key.clone()).or_insert_with(|| value.clone());
            }
        } else if current != held {
            return None;
        }
    } else {
        semantic.insert(key.into(), held.clone());
    }
    Some(())
}

/// Plain row names remain compatible, while recorded proof predicates and
/// site evidence retain their original values and associations.
fn retain_proof_fields(machine: &Map<String, Value>, plain: &mut Value) {
    let (Some(original), Some(rows)) = (
        machine.get("chain").and_then(Value::as_array),
        plain.get_mut("chain").and_then(Value::as_array_mut),
    ) else {
        return;
    };
    for (original, row) in original.iter().zip(rows) {
        let (Some(original), Some(row)) = (original.as_object(), row.as_object_mut()) else {
            continue;
        };
        for key in [
            "proof",
            "resolution",
            "relation",
            "terminal",
            "fanout_truncated",
            "fanout_dropped",
            "fanout_dropped_count",
            "span_coherence",
            "site_state",
            "crossing",
        ] {
            if let Some(value) = original.get(key) {
                row.insert(key.into(), value.clone());
            }
        }
        for (key, value) in original.iter().filter(|(key, _)| key.ends_with("_reason")) {
            row.insert(key.clone(), value.clone());
        }
        if let Some(sites) = original.get(crate::site_expression::TRACE_REFERENCE_SITES_KEY) {
            row.insert(
                crate::site_expression::TRACE_REFERENCE_SITES_KEY.into(),
                sites.clone(),
            );
        }
    }
}

fn focal_from_reach(machine: &Map<String, Value>) -> Option<&str> {
    let reach = machine.get("reach")?;
    let focal = reach
        .get("members")?
        .as_array()?
        .iter()
        .find(|member| member.get("distance").and_then(Value::as_u64) == Some(1))?
        .get("via")?
        .as_str()?;
    reach
        .get("frontier")?
        .as_array()?
        .iter()
        .find(|row| row.get("to").and_then(Value::as_str) == Some(focal))?
        .get("to_name")?
        .as_str()
}

fn delivered(page: &Map<String, Value>) -> Option<&'static str> {
    if page.get("has_more").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    match (page.get("answer_domain")?.as_str()?, page.get("answer_rows_complete")?.as_bool()) {
        ("reach.members", Some(true)) => Some("All recorded trace members have been delivered. Walk evidence and readings remain; send next_cursor as cursor only when that evidence is needed. Caller coverage remains subject to the stated gaps."),
        ("reach.members", Some(false)) => Some("More recorded trace members remain; send next_cursor as cursor. The cursor also continues the frozen walk evidence and readings."),
        ("chain", Some(true)) => Some("All recorded chain rows have been delivered. Additional evidence and readings remain; send next_cursor as cursor to inspect them."),
        ("chain", Some(false)) => Some("More recorded chain rows remain; send next_cursor as cursor."),
        _ => Some("More frozen trace records remain; send next_cursor as cursor to inspect them. This page does not establish whether recorded answer rows remain."),
    }
}
