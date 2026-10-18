// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Whole-row pages of a recaptured, exact-authority status observation.

use crate::{types::ContentBlock, ToolCallResult};
use base64::Engine as _;
use kin_model::RepoPath;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;

const MAX_CURSOR: usize = 512;
const ENVELOPE_RESERVE: usize = 8_192;

/// The shared verdict label for a bounded selected-graph metadata scan.
pub fn metadata_limit_clause(reason: &str) -> String {
    format!("enrichment_metadata_unavailable: {reason}")
}

#[cfg(test)]
#[path = "status_pages/tests/unavailable.rs"]
mod unavailable_tests;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusRequest {
    #[serde(default)]
    pub dependencies: Vec<String>,
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default)]
    pub max_chars: Option<usize>,
}

impl StatusRequest {
    pub fn from_arguments(arguments: &HashMap<String, Value>) -> Result<Self, String> {
        let mut selected = serde_json::Map::new();
        for key in ["dependencies", "cursor"] {
            if let Some(value) = arguments.get(key) {
                selected.insert(key.into(), value.clone());
            }
        }
        for key in [
            crate::request_schema::OUTPUT_BUDGET,
            "max_chars",
            "max_response_chars",
        ] {
            if let Some(value) = arguments.get(key) {
                selected.insert("max_chars".into(), value.clone());
                break;
            }
        }
        let request: Self =
            serde_json::from_value(Value::Object(selected)).map_err(|e| e.to_string())?;
        request.paths()?;
        if request
            .cursor
            .as_ref()
            .is_some_and(|cursor| cursor.len() > MAX_CURSOR)
        {
            return Err("status cursor exceeds its bound".into());
        }
        Ok(request)
    }

    pub fn paths(&self) -> Result<Vec<RepoPath>, String> {
        if self.dependencies.len() > 1024 {
            return Err("at most 1024 dependencies may be requested".into());
        }
        let mut paths = self
            .dependencies
            .iter()
            .map(|path| RepoPath::from_utf8(path.clone()).map_err(|error| error.to_string()))
            .collect::<Result<Vec<_>, _>>()?;
        kin_core::validate_source_paths(paths.iter()).map_err(|error| error.to_string())?;
        paths.sort();
        paths.dedup();
        Ok(paths)
    }

    pub fn ceiling(&self) -> usize {
        self.max_chars
            .unwrap_or(crate::budget::RESPONSE_DEFAULT_MAX_CHARS)
            .clamp(
                crate::budget::RESPONSE_MIN_MAX_CHARS,
                crate::budget::RESPONSE_MAX_MAX_CHARS,
            )
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    version: u8,
    snapshot: String,
    query: String,
    offset: usize,
}

// HMAC-SHA256, using the repository's SHA256 primitive. The daemon supplies
// its existing random per-instance cursor key; restart invalidates every token.
fn mac(key: &[u8; 32], payload: &[u8]) -> [u8; 32] {
    let mut inner = vec![0x36; 64];
    let mut outer = vec![0x5c; 64];
    for (index, byte) in key.iter().enumerate() {
        inner[index] ^= byte;
        outer[index] ^= byte;
    }
    inner.extend_from_slice(payload);
    outer.extend_from_slice(&kin_blobs::digest(&inner).0);
    kin_blobs::digest(&outer).0
}

fn encode(cursor: &Cursor, key: &[u8; 32]) -> String {
    let bytes = serde_json::to_vec(cursor).expect("status cursor");
    format!(
        "{}.{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&bytes),
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(mac(key, &bytes))
    )
}

fn decode(token: &str, key: &[u8; 32]) -> Result<Cursor, String> {
    let invalid = || "invalid status cursor; restart without cursor".to_string();
    if token.len() > MAX_CURSOR {
        return Err(invalid());
    }
    let (body, tag) = token.split_once('.').ok_or_else(invalid)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(body)
        .map_err(|_| invalid())?;
    let tag = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(tag)
        .map_err(|_| invalid())?;
    let expected = mac(key, &bytes);
    if tag.len() != expected.len()
        || tag.iter().zip(expected).fold(0u8, |d, (a, b)| d | (a ^ b)) != 0
    {
        return Err(invalid());
    }
    let cursor: Cursor = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
    if cursor.version != 1 {
        return Err(invalid());
    }
    Ok(cursor)
}

/// Validate the selected-observation contract at both producer and consumer.
/// A page describes metadata coverage, never semantic completeness.
/// Check a selected enrichment observation. `status_page_continues` says the
/// observation rides in a mixed status page, whose `status_page.next_cursor`
/// is the one continuation, so its own page must not repeat it; on an
/// enrichment-only page, its own page carries it.
pub fn validate_observation(
    value: &Value,
    current: bool,
    scope: crate::handlers::entities::GraphStatusScope,
    status_page_continues: bool,
) -> Result<(), String> {
    if value["schema"] != "kin.enrichment-status.v1"
        || value["proof_scope"] != "recorded_call_site_census"
        || value["all_relationships_attested"] != false
        || value["current"].as_bool() != Some(current)
        || value["truth_epoch"].as_u64().is_none()
        || !value["snapshot_id"]
            .as_str()
            .is_some_and(|id| id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err("invalid selected enrichment observation identity or scope".into());
    }
    let expected = match scope {
        crate::handlers::entities::GraphStatusScope::Head => "workspace_head",
        crate::handlers::entities::GraphStatusScope::TemporalSession => "committed_graph",
    };
    if value["scope"]["kind"] != expected
        || (expected == "committed_graph"
            && value["scope"]["change"].as_str().is_none_or(str::is_empty))
    {
        return Err("enrichment source scope differs from selected graph status".into());
    }
    let rows = value["files"]
        .as_array()
        .ok_or("enrichment rows are missing")?;
    if let Some(unavailable) = value.get("unavailable") {
        let limit = match unavailable["limit_kind"].as_str() {
            Some("bytes" | "path_bytes") => 8 * 1024 * 1024,
            Some("records") => 1_000_000,
            _ => return Err("invalid enrichment metadata limit kind".into()),
        };
        if unavailable["limit"].as_u64() != Some(limit)
            || unavailable["reason"].as_str().is_none_or(str::is_empty)
            || value["status"] != "bounded"
            || !value["limitation"]
                .as_str()
                .is_some_and(|text| text.starts_with("enrichment_metadata_unavailable: "))
            || !rows.is_empty()
            || value.get("page").is_some()
        {
            return Err("unavailable enrichment metadata cannot certify inventory coverage".into());
        }
        return Ok(());
    }
    if let Some(page) = value.get("page") {
        let start = page["start"]
            .as_u64()
            .ok_or("invalid enrichment page start")?;
        let returned = page["returned"]
            .as_u64()
            .ok_or("invalid enrichment page count")?;
        let total = page["total"]
            .as_u64()
            .ok_or("invalid enrichment page total")?;
        let end = start
            .checked_add(returned)
            .filter(|end| *end <= total)
            .ok_or("invalid enrichment page range")?;
        let continuation_described = if status_page_continues {
            page.get("next_cursor").is_none()
        } else {
            page["next_cursor"].is_string() == (end < total)
        };
        if returned != rows.len() as u64
            || page["complete"].as_bool() != Some(start == 0 && end == total)
            || !continuation_described
        {
            return Err("enrichment page does not describe its rows".into());
        }
    }
    Ok(())
}

/// Check independent operational metadata and the mixed page's exact row count.
pub fn validate_collections(
    enrichment: Option<&Value>,
    transactions: Option<&Value>,
    page: Option<&Value>,
) -> Result<(), String> {
    let tx_count = if let Some(transactions) = transactions {
        let observation: crate::session::OpenTransactionObservation =
            serde_json::from_value(transactions.clone()).map_err(|error| error.to_string())?;
        if transactions["scope"] != "live_staged_session_work"
            || !transactions["snapshot_id"]
                .as_str()
                .is_some_and(|id| id.len() == 64 && id.bytes().all(|byte| byte.is_ascii_hexdigit()))
            || observation.items.iter().any(|item| {
                item.staged_count == 0
                    || !crate::session::is_unfinished_transaction_state(&item.state)
                    || item.transaction_id.is_empty()
                    || item.session_id.is_empty()
                    || item.created_at.is_some() != item.age_seconds.is_some()
            })
        {
            return Err("invalid live staged transaction observation".into());
        }
        observation.items.len()
    } else {
        0
    };
    if let Some(page) = page {
        let start = page["start"].as_u64().ok_or("invalid status page start")?;
        let returned = page["returned"]
            .as_u64()
            .ok_or("invalid status page count")?;
        let total = page["total"].as_u64().ok_or("invalid status page total")?;
        let end = start
            .checked_add(returned)
            .filter(|end| *end <= total)
            .ok_or("invalid status page range")?;
        let files = enrichment
            .and_then(|value| value["files"].as_array())
            .map_or(0, Vec::len);
        if returned != (files + tx_count) as u64
            || page["complete"].as_bool() != Some(start == 0 && end == total)
            || page["next_cursor"].is_string() != (end < total)
            || !page["snapshot_id"]
                .as_str()
                .is_some_and(|id| id.len() == 64 && id.bytes().all(|byte| byte.is_ascii_hexdigit()))
        {
            return Err("status page does not describe its metadata rows".into());
        }
    }
    Ok(())
}

/// Add a fresh operational transaction observation without changing the graph
/// reading or placing this data in its settled cache. The identity excludes
/// elapsed time, but includes every staged payload digest and ownership field.
pub fn with_open_transactions(
    mut result: ToolCallResult,
    observation: &crate::session::OpenTransactionObservation,
    binding: &Value,
) -> Result<ToolCallResult, String> {
    let Some(ContentBlock::Text { text }) = result.content.first_mut() else {
        return Err("status response is not text".into());
    };
    let mut value: Value = if result.is_error == Some(true) {
        json!({"graph_status":"unavailable", "message":text})
    } else {
        serde_json::from_str(text).map_err(|error| error.to_string())?
    };
    let stable: Vec<_> = observation
        .items
        .iter()
        .map(|item| {
            (
                &item.transaction_id,
                &item.session_id,
                &item.scope,
                &item.state,
                item.staged_count,
                &item.staged_digest,
                &item.created_at,
            )
        })
        .collect();
    let snapshot = kin_blobs::digest(
        &serde_json::to_vec(&(binding, stable)).map_err(|error| error.to_string())?,
    )
    .to_string();
    let mut transactions = serde_json::to_value(observation).map_err(|error| error.to_string())?;
    transactions["snapshot_id"] = json!(snapshot);
    transactions["scope"] = json!("live_staged_session_work");
    value["open_transactions"] = transactions;
    *text = serde_json::to_string(&value).map_err(|error| error.to_string())?;
    Ok(result)
}

/// The offline registry is process-local. Restart changes the key and every
/// token also binds the specific registry/store observation supplied above.
pub fn offline_cursor_key() -> &'static [u8; 32] {
    static KEY: std::sync::LazyLock<[u8; 32]> = std::sync::LazyLock::new(|| {
        let mut bytes = [0; 32];
        bytes[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        bytes[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        bytes
    });
    &KEY
}

/// Page graph metadata and fresh operational transaction rows together. The
/// collections remain separate and retain their own scope and counts.
pub fn page(
    result: ToolCallResult,
    request: &StatusRequest,
    key: &[u8; 32],
) -> Result<ToolCallResult, String> {
    let value = match result.content.first() {
        Some(ContentBlock::Text { text }) => serde_json::from_str::<Value>(text).ok(),
        None => None,
    };
    match value.filter(|value| value.get("open_transactions").is_some()) {
        Some(value) => page_collections(result, value, request, key),
        None => page_enrichment(result, request, key),
    }
}

fn page_collections(
    mut result: ToolCallResult,
    mut value: Value,
    request: &StatusRequest,
    key: &[u8; 32],
) -> Result<ToolCallResult, String> {
    if request.cursor.is_some() && value.get("stale").is_some_and(|stale| !stale.is_null()) {
        return Err(
            "status cannot resume while the selected graph is changing; restart without cursor"
                .into(),
        );
    }
    let paths = request.paths()?;
    let query =
        kin_blobs::digest(&serde_json::to_vec(&paths).map_err(|e| e.to_string())?).to_string();
    let has_enrichment = value.get("enrichment").is_some();
    let enrichment_unavailable = value["enrichment"].get("unavailable").is_some();
    let enrichment_available = has_enrichment && !enrichment_unavailable;
    let mut files = if has_enrichment {
        value["enrichment"]["files"]
            .take()
            .as_array()
            .ok_or("status rows are missing")?
            .clone()
    } else {
        Vec::new()
    };
    // An unavailable graph status still pages operational work. Its original
    // error is not an incomplete dependency observation.
    if !enrichment_unavailable
        && !paths.is_empty()
        && (has_enrichment || result.is_error != Some(true))
    {
        let selected = paths
            .iter()
            .map(serde_json::to_value)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        files.retain(|row| selected.contains(&row["projection_path"]));
        if files.len() != paths.len() {
            return Err("status did not account for every requested dependency".into());
        }
    }
    let transactions = value["open_transactions"]["items"]
        .take()
        .as_array()
        .ok_or("status transactions are missing")?
        .clone();
    let snapshot = kin_blobs::digest(
        &serde_json::to_vec(&(
            &value["enrichment"]["snapshot_id"],
            &value["open_transactions"]["snapshot_id"],
        ))
        .map_err(|e| e.to_string())?,
    )
    .to_string();
    let total = transactions
        .len()
        .checked_add(files.len())
        .ok_or("status row count overflow")?;
    let start = if let Some(token) = &request.cursor {
        let cursor = decode(token, key)?;
        if cursor.snapshot != snapshot || cursor.query != query {
            return Err("status snapshot or dependencies changed; restart without cursor".into());
        }
        if cursor.offset >= total {
            return Err("status cursor is outside the observation".into());
        }
        cursor.offset
    } else {
        0
    };
    let ceiling = request.ceiling();
    let target = ceiling.saturating_sub(ENVELOPE_RESERVE).max(ceiling / 2);
    let mut end = start;
    let mut previous = None;
    loop {
        let next = (end < total).then(|| {
            encode(
                &Cursor {
                    version: 1,
                    snapshot: snapshot.clone(),
                    query: query.clone(),
                    offset: end,
                },
                key,
            )
        });
        // Operational work comes first so open staged changes are visible even
        // when many source-observation pages remain. No record is split/dropped.
        let tx_start = start.min(transactions.len());
        let tx_end = end.min(transactions.len());
        let file_start = start.saturating_sub(transactions.len()).min(files.len());
        let file_end = end.saturating_sub(transactions.len()).min(files.len());
        // Each collection says which of its rows this page holds; the one
        // continuation is `status_page.next_cursor`, which covers both, so no
        // collection repeats it.
        value["open_transactions"]["items"] = json!(&transactions[tx_start..tx_end]);
        value["open_transactions"]["page"] = json!({
            "start":tx_start,"returned":tx_end-tx_start,"total":transactions.len(),
            "complete":tx_start==0 && tx_end==transactions.len(),
        });
        if has_enrichment {
            value["enrichment"]["files"] = json!(&files[file_start..file_end]);
            value["enrichment"]["requested_dependencies"] = json!(paths);
            if enrichment_available {
                value["enrichment"]["page"] = json!({
                "start":file_start,"returned":file_end-file_start,"total":files.len(),
                "complete":file_start==0 && file_end==files.len(),"max_chars":ceiling,
                });
            }
        }
        value["status_page"] = json!({
            "snapshot_id":snapshot,"start":start,"returned":end-start,"total":total,
            "complete":start==0 && end==total,"next_cursor":next,"max_chars":ceiling,
        });
        let encoded = serde_json::to_string(&value).map_err(|e| e.to_string())?;
        if encoded.len() > target {
            let Some(encoded) = previous else {
                return Err("status metadata row or envelope exceeds the response budget; increase max_output_bytes".into());
            };
            let Some(ContentBlock::Text { text }) = result.content.first_mut() else {
                unreachable!()
            };
            *text = encoded;
            return Ok(result);
        }
        if end == total {
            let Some(ContentBlock::Text { text }) = result.content.first_mut() else {
                unreachable!()
            };
            *text = encoded;
            return Ok(result);
        }
        // An empty page is never a successful continuation.
        if end > start {
            previous = Some(encoded);
        }
        end += 1;
    }
}

/// Page only after the complete observation passed its source/truth fence.
/// This never changes the underlying proof or repository-wide limitations.
fn page_enrichment(
    result: ToolCallResult,
    request: &StatusRequest,
    key: &[u8; 32],
) -> Result<ToolCallResult, String> {
    // Leave room for the stdio standing envelope. Its final boundary checks
    // actual serialized bytes too, rather than assuming this reserve suffices.
    let ceiling = request.ceiling();
    let target = ceiling.saturating_sub(ENVELOPE_RESERVE).max(ceiling / 2);
    page_enrichment_measured(result, request, key, target, |value| {
        serde_json::to_vec(value)
            .map(|encoded| encoded.len())
            .map_err(|error| error.to_string())
    })
}

/// Admit whole enrichment rows against the caller's actual delivered envelope.
/// The measurement includes transport decorations; it never changes the
/// requested ceiling, observation identity, proof rows, or cursor contract.
pub fn page_enrichment_for_envelope(
    result: ToolCallResult,
    request: &StatusRequest,
    key: &[u8; 32],
    measure: impl Fn(&Value) -> Result<usize, String>,
) -> Result<ToolCallResult, String> {
    page_enrichment_measured(result, request, key, request.ceiling(), measure)
}

fn page_enrichment_measured(
    mut result: ToolCallResult,
    request: &StatusRequest,
    key: &[u8; 32],
    target: usize,
    measure: impl Fn(&Value) -> Result<usize, String>,
) -> Result<ToolCallResult, String> {
    if result.is_error == Some(true) {
        return Ok(result);
    }
    let Some(ContentBlock::Text { text }) = result.content.first_mut() else {
        return Err("status response is not text".into());
    };
    let mut value: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    if request.cursor.is_some() && value.get("stale").is_some_and(|stale| !stale.is_null()) {
        return Err(
            "status cannot resume while the selected graph is changing; restart without cursor"
                .into(),
        );
    }
    let current = value.get("stale").is_none_or(Value::is_null);
    let paths = request.paths()?;
    let query =
        kin_blobs::digest(&serde_json::to_vec(&paths).map_err(|e| e.to_string())?).to_string();
    let Some(enrichment) = value.get_mut("enrichment") else {
        return if request.cursor.is_some() || !paths.is_empty() {
            Err("selected graph enrichment observation is unavailable".into())
        } else {
            if measure(&value)? > target {
                Err("status envelope cannot fit within the response budget".into())
            } else {
                Ok(result)
            }
        };
    };
    enrichment["current"] = json!(current);
    if enrichment.get("unavailable").is_some() {
        if request.cursor.is_some() {
            return Err("enrichment detail is unavailable; restart without cursor".into());
        }
        enrichment["requested_dependencies"] = json!(paths);
        if measure(&value)? > target {
            return Err("status envelope cannot fit within the response budget".into());
        }
        *text = serde_json::to_string(&value).map_err(|error| error.to_string())?;
        return Ok(result);
    }
    let snapshot = enrichment["snapshot_id"]
        .as_str()
        .ok_or("status snapshot identity is missing")?
        .to_string();
    let mut rows = enrichment["files"]
        .take()
        .as_array()
        .ok_or("status rows are missing")?
        .clone();
    if !paths.is_empty() {
        let selected = paths
            .iter()
            .map(serde_json::to_value)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        rows.retain(|row| selected.contains(&row["projection_path"]));
        if rows.len() != paths.len() {
            return Err("status did not account for every requested dependency".into());
        }
    }
    let start = if let Some(token) = &request.cursor {
        let cursor = decode(token, key)?;
        if cursor.snapshot != snapshot || cursor.query != query {
            return Err("status snapshot or dependencies changed; restart without cursor".into());
        }
        if cursor.offset >= rows.len() {
            return Err("status cursor is outside the observation".into());
        }
        cursor.offset
    } else {
        0
    };
    let total = rows.len();
    let ceiling = request.ceiling();
    // A terminal page omits the cursor. It can fit even when a smaller prefix
    // cannot, so measure all remaining whole rows before assuming size grows.
    value["enrichment"]["files"] = json!(&rows[start..]);
    value["enrichment"]["page"] = json!({
        "start": start, "returned": total - start, "total": total,
        "complete": start == 0, "next_cursor": null, "max_chars": ceiling,
    });
    value["enrichment"]["requested_dependencies"] = json!(paths);
    if measure(&value)? <= target {
        *text = serde_json::to_string(&value).map_err(|error| error.to_string())?;
        return Ok(result);
    }
    // Measure the first deliverable whole row, or the empty inventory. A
    // zero-row preview's cursor can be larger than a terminal one-row page.
    let mut end = start.saturating_add(1).min(total);
    let mut previous = None;
    loop {
        let next = (end < total).then(|| {
            encode(
                &Cursor {
                    version: 1,
                    snapshot: snapshot.clone(),
                    query: query.clone(),
                    offset: end,
                },
                key,
            )
        });
        value["enrichment"]["files"] = json!(&rows[start..end]);
        value["enrichment"]["page"] = json!({
            "start": start, "returned": end - start, "total": total,
            "complete": start == 0 && end == total, "next_cursor": next,
            "max_chars": ceiling,
        });
        value["enrichment"]["requested_dependencies"] = json!(paths);
        let encoded = serde_json::to_string(&value).map_err(|e| e.to_string())?;
        if measure(&value)? > target {
            if end <= start + 1 {
                return Err(
                    "status metadata row or envelope cannot fit within the response budget".into(),
                );
            }
            *text = previous.ok_or("status page has no fitting metadata row")?;
            return Ok(result);
        }
        if end == total {
            *text = encoded;
            return Ok(result);
        }
        previous = Some(encoded);
        end += 1;
    }
}

/// Final transport check. An unexpectedly large standing envelope is refused,
/// never silently clipped or allowed to discard source/proof rows.
pub fn enforce_ceiling(result: ToolCallResult, max_chars: usize) -> ToolCallResult {
    if result
        .content
        .iter()
        .map(|block| match block {
            ContentBlock::Text { text } => text.len(),
        })
        .sum::<usize>()
        <= max_chars
    {
        result
    } else {
        ToolCallResult::error(
            "status envelope exceeds the response budget; increase max_output_bytes",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(count: usize) -> Value {
        json!({
            "enrichment": {
                "snapshot_id":"a".repeat(64),
                "all_relationships_attested":false,
                "proof_scope":"recorded_call_site_census",
                "files":(0..count).map(|index|json!({
                    "projection_path":RepoPath::from_utf8(format!("src/{index:03}.py")).unwrap(),
                    "artifact_id":format!("artifact-{index}"),
                    "proof":"unverified", "current_completion":"recorded",
                    "contexts":[{"state":"unverified","reason":"observed uncertainty λ".repeat(20)}],
                })).collect::<Vec<_>>()
            },
            "_kin":{"verdict":{"state":"inconclusive","safe_to_conclude_absent":false}}
        })
    }

    fn text(result: &ToolCallResult) -> &str {
        let ContentBlock::Text { text } = &result.content[0];
        text
    }

    #[test]
    fn status_pages_reconstruct_every_semantic_row_within_full_payload_budget() {
        let full = fixture(90);
        let mut request = StatusRequest {
            max_chars: Some(12_000),
            ..Default::default()
        };
        let mut all = Vec::new();
        loop {
            let result = page(ToolCallResult::text(full.to_string()), &request, &[7; 32]).unwrap();
            assert!(text(&result).len() <= request.ceiling());
            let value: Value = serde_json::from_str(text(&result)).unwrap();
            assert_eq!(value["_kin"], full["_kin"]);
            assert_eq!(value["enrichment"]["page"]["start"], json!(all.len()));
            let rows = value["enrichment"]["files"].as_array().unwrap();
            assert!(!rows.is_empty());
            all.extend(rows.iter().cloned());
            request.cursor = value["enrichment"]["page"]["next_cursor"]
                .as_str()
                .map(str::to_owned);
            if request.cursor.is_none() {
                break;
            }
        }
        assert_eq!(json!(all), full["enrichment"]["files"]);
    }

    #[test]
    fn status_pages_reject_changed_authority_dependencies_forgery_restart_and_stale_replay() {
        let full = fixture(50);
        let mut request = StatusRequest {
            max_chars: Some(12_000),
            ..Default::default()
        };
        let first = page(ToolCallResult::text(full.to_string()), &request, &[7; 32]).unwrap();
        let value: Value = serde_json::from_str(text(&first)).unwrap();
        request.cursor = Some(
            value["enrichment"]["page"]["next_cursor"]
                .as_str()
                .unwrap()
                .into(),
        );
        let mut changed = full.clone();
        changed["enrichment"]["snapshot_id"] = json!("b".repeat(64));
        assert!(page(
            ToolCallResult::text(changed.to_string()),
            &request,
            &[7; 32]
        )
        .unwrap_err()
        .contains("changed"));
        assert!(page(ToolCallResult::text(full.to_string()), &request, &[8; 32]).is_err());
        let mut changed = request.clone();
        changed.dependencies = vec!["src/000.py".into()];
        assert!(
            page(ToolCallResult::text(full.to_string()), &changed, &[7; 32])
                .unwrap_err()
                .contains("changed")
        );
        let mut changed = request.clone();
        changed.cursor = Some("x".repeat(MAX_CURSOR + 1));
        assert!(page(ToolCallResult::text(full.to_string()), &changed, &[7; 32]).is_err());
        let mut replay = full.clone();
        replay["stale"] = json!({"reason":"selected_graph_changing"});
        assert!(page(ToolCallResult::text(replay.to_string()), &request, &[7; 32]).is_err());
        let replay = page(
            ToolCallResult::text(replay.to_string()),
            &StatusRequest::default(),
            &[7; 32],
        )
        .unwrap();
        let replay: Value = serde_json::from_str(text(&replay)).unwrap();
        assert_eq!(replay["enrichment"]["current"], false);
    }

    #[test]
    fn status_pages_dependency_subset_never_hides_a_missing_required_row_or_global_gap() {
        let full = fixture(5);
        let request = StatusRequest {
            dependencies: vec!["src/002.py".into()],
            ..Default::default()
        };
        let result = page(ToolCallResult::text(full.to_string()), &request, &[1; 32]).unwrap();
        let selected: Value = serde_json::from_str(text(&result)).unwrap();
        assert_eq!(selected["enrichment"]["page"]["total"], 1);
        assert_eq!(
            selected["enrichment"]["files"][0],
            full["enrichment"]["files"][2]
        );
        assert_eq!(selected["_kin"], full["_kin"]);
        let request = StatusRequest {
            dependencies: vec!["missing.py".into()],
            ..Default::default()
        };
        assert!(page(ToolCallResult::text(full.to_string()), &request, &[1; 32]).is_err());
    }

    fn transaction_fixture(count: usize) -> crate::session::OpenTransactionObservation {
        let created = kin_model::Timestamp::now();
        crate::session::OpenTransactionObservation {
            observed_at: created.clone(),
            items: (0..count)
                .map(|index| crate::session::OpenStagedTransaction {
                    transaction_id: format!("transaction-{index:03}"),
                    session_id: "owner".into(),
                    scope: "selected-entity-λ".repeat(15),
                    state: "active".into(),
                    staged_count: 1,
                    staged_digest: format!("{index:064x}"),
                    created_at: Some(created.clone()),
                    age_seconds: Some(0),
                })
                .collect(),
        }
    }

    #[test]
    fn status_dependency_queries_preserve_unavailable_errors_and_transaction_pages() {
        let original = "kin_graph_status could not sample the selected graph: authority moved; no settled reading. \"λ\"\nWait for graph mutation to quiesce.";
        let standing = json!({
            "runtime":"repo-daemon",
            "answered_by":{"pid":7,"repo_id":"00000000-0000-0000-0000-000000000007"},
            "verdict":{"state":"inconclusive","safe_to_conclude_absent":false}
        });
        for count in [0, 30] {
            let observation = transaction_fixture(count);
            let wrapped = with_open_transactions(
                ToolCallResult::error(original),
                &observation,
                &json!("selected-repository"),
            )
            .unwrap();
            let mut full: Value = serde_json::from_str(text(&wrapped)).unwrap();
            full["_kin"] = standing.clone();
            let make = || {
                let mut result = ToolCallResult::text(full.to_string());
                result.is_error = Some(true);
                result
            };
            let mut request = StatusRequest {
                dependencies: (0..8).map(|index| format!("src/{index}.py")).collect(),
                max_chars: Some(6000),
                ..Default::default()
            };
            let mut reconstructed = Vec::new();
            let mut pages = 0;
            loop {
                let result = page(make(), &request, &[4; 32]).unwrap();
                let result = enforce_ceiling(result, request.ceiling());
                assert_eq!(result.is_error, Some(true));
                assert!(text(&result).len() <= request.ceiling());
                let value: Value = serde_json::from_str(text(&result)).unwrap();
                assert_eq!(value["message"], original);
                assert_eq!(value["graph_status"], "unavailable");
                assert_eq!(value["_kin"], standing);
                assert!(value.get("enrichment").is_none());
                assert!(value.get("entity_count").is_none());
                assert_eq!(value["status_page"]["start"], reconstructed.len());
                assert_eq!(value["status_page"]["total"], count);
                reconstructed.extend(
                    value["open_transactions"]["items"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .cloned(),
                );
                request.cursor = value["status_page"]["next_cursor"]
                    .as_str()
                    .map(str::to_owned);
                pages += 1;
                if request.cursor.is_none() {
                    break;
                }
                let mut changed = request.clone();
                changed.dependencies.push("src/another.py".into());
                assert!(page(make(), &changed, &[4; 32])
                    .unwrap_err()
                    .contains("changed"));
            }
            assert_eq!(
                json!(reconstructed),
                serde_json::to_value(&observation.items).unwrap()
            );
            assert_eq!(pages > 1, count > 0);
        }
    }

    #[test]
    fn status_dependency_queries_still_refuse_missing_successful_rows() {
        let request = StatusRequest {
            dependencies: vec!["src/000.py".into(), "missing.py".into()],
            max_chars: Some(6000),
            ..Default::default()
        };
        for transactions in [false, true] {
            let mut result = ToolCallResult::text(fixture(1).to_string());
            if transactions {
                result =
                    with_open_transactions(result, &transaction_fixture(2), &json!("repository"))
                        .unwrap();
            }
            assert_eq!(
                page(result, &request, &[4; 32]).unwrap_err(),
                "status did not account for every requested dependency"
            );
        }
    }

    #[test]
    fn status_pages_preserve_every_transaction_and_source_row_across_age_changes() {
        let mut full = fixture(25);
        for (key, value) in [
            ("schema", json!("kin.enrichment-status.v1")),
            ("current", json!(true)),
            ("truth_epoch", json!(0)),
            ("scope", json!({"kind": "workspace_head"})),
        ] {
            full["enrichment"][key] = value;
        }
        let mut observation = transaction_fixture(30);
        let original = serde_json::to_value(&observation.items).unwrap();
        let mut request = StatusRequest {
            max_chars: Some(12_000),
            ..Default::default()
        };
        let mut transactions = Vec::new();
        let mut files = Vec::new();
        let mut pages = 0u64;
        loop {
            observation.observed_at = kin_model::Timestamp::now();
            for row in &mut observation.items {
                row.age_seconds = Some(pages);
            }
            let result = with_open_transactions(
                ToolCallResult::text(full.to_string()),
                &observation,
                &json!({"daemon":"one"}),
            )
            .unwrap();
            let result = page(result, &request, &[3; 32]).unwrap();
            assert!(text(&result).len() <= request.ceiling());
            let value: Value = serde_json::from_str(text(&result)).unwrap();
            validate_collections(
                value.get("enrichment"),
                value.get("open_transactions"),
                value.get("status_page"),
            )
            .unwrap();
            assert_eq!(
                value["status_page"]["start"],
                json!(transactions.len() + files.len())
            );
            assert_eq!(value["_kin"], full["_kin"]);
            for row in value["open_transactions"]["items"].as_array().unwrap() {
                assert_eq!(row["age_seconds"], pages);
                let mut row = row.clone();
                row["age_seconds"] = json!(0);
                transactions.push(row);
            }
            files.extend(
                value["enrichment"]["files"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .cloned(),
            );
            // One continuation for the whole page: neither collection
            // repeats it, and a page that did would not validate.
            for collection in ["open_transactions", "enrichment"] {
                assert!(
                    value[collection]["page"].get("next_cursor").is_none(),
                    "{collection} repeats the continuation: {}",
                    value[collection]["page"]
                );
            }
            let head = crate::handlers::entities::GraphStatusScope::Head;
            validate_observation(&value["enrichment"], true, head, true).unwrap();
            if value["status_page"]["next_cursor"].is_string() {
                let mut repeated = value["enrichment"].clone();
                repeated["page"]["next_cursor"] = value["status_page"]["next_cursor"].clone();
                assert!(validate_observation(&repeated, true, head, true).is_err());
            }
            request.cursor = value["status_page"]["next_cursor"]
                .as_str()
                .map(str::to_owned);
            pages += 1;
            if request.cursor.is_none() {
                break;
            }
        }
        assert!(pages > 1);
        assert_eq!(json!(transactions), original);
        assert_eq!(json!(files), full["enrichment"]["files"]);
    }

    #[test]
    fn status_pages_reject_same_count_staged_edits_owner_changes_and_offline_rebinding() {
        let observation = transaction_fixture(30);
        let mut request = StatusRequest {
            max_chars: Some(6000),
            ..Default::default()
        };
        let make = |observation: &crate::session::OpenTransactionObservation, binding: &Value| {
            with_open_transactions(
                ToolCallResult::error("graph status requires a daemon"),
                observation,
                binding,
            )
            .unwrap()
        };
        let first = page(make(&observation, &json!("registry-a")), &request, &[4; 32]).unwrap();
        assert_eq!(first.is_error, Some(true));
        let value: Value = serde_json::from_str(text(&first)).unwrap();
        assert_eq!(value["graph_status"], "unavailable");
        assert!(value.get("entity_count").is_none());
        request.cursor = value["status_page"]["next_cursor"]
            .as_str()
            .map(str::to_owned);
        assert!(request.cursor.is_some());
        let mut changed = observation.clone();
        changed.items[0].staged_digest = "f".repeat(64);
        assert!(
            page(make(&changed, &json!("registry-a")), &request, &[4; 32])
                .unwrap_err()
                .contains("changed")
        );
        let mut changed = observation.clone();
        changed.items[0].session_id = "new-owner".into();
        assert!(page(make(&changed, &json!("registry-a")), &request, &[4; 32]).is_err());
        assert!(page(make(&observation, &json!("registry-b")), &request, &[4; 32]).is_err());
        assert!(page(make(&observation, &json!("registry-a")), &request, &[5; 32]).is_err());
    }

    #[test]
    fn status_budget_honors_canonical_bytes_and_legacy_aliases() {
        for key in [
            crate::request_schema::OUTPUT_BUDGET,
            "max_chars",
            "max_response_chars",
        ] {
            let args = HashMap::from([(key.to_string(), json!(60_000))]);
            assert_eq!(
                StatusRequest::from_arguments(&args).unwrap().ceiling(),
                60_000
            );
        }
    }

    #[test]
    fn status_budget_uses_the_shared_advertised_range() {
        for requested in [
            None,
            Some(0),
            Some(2000),
            Some(2048),
            Some(12_000),
            Some(60_000),
            Some(usize::MAX),
        ] {
            let request = StatusRequest {
                max_chars: requested,
                ..Default::default()
            };
            let mut arguments = HashMap::new();
            if let Some(value) = requested {
                arguments.insert("max_chars".into(), json!(value));
            }
            assert_eq!(
                request.ceiling(),
                crate::budget::ResponseBudget::from_arguments(&arguments).max_chars
            );
        }
    }

    #[test]
    fn status_pages_refuse_oversized_records_and_envelopes_without_trimming_proof() {
        let mut full = fixture(1);
        full["enrichment"]["files"][0]["reason"] = json!("x".repeat(70_000));
        assert!(page(
            ToolCallResult::text(full.to_string()),
            &StatusRequest::default(),
            &[1; 32]
        )
        .is_err());
        let result = enforce_ceiling(ToolCallResult::text(full.to_string()), 45_000);
        assert_eq!(result.is_error, Some(true));
        assert!(text(&result).len() < 2048);
    }
}
