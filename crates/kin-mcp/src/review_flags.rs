// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Review flags, as an agent's answer carries them.
//!
//! A reviewer flags an entity with a note, and the flag is held in the
//! repository. The daemon that answers a tool call knows which entities the
//! answer returned, so it is the one that says which open flags the answer
//! reaches: it hands them back beside the answer, under [`META_KEY`] of the
//! result's `_meta`, and never inside the answer's own payload, which every
//! budget and verdict has already been computed over.
//!
//! The stdio server collects what the daemon handed back for one `tools/call`
//! ([`collecting`]) and writes it into the reply the agent reads, last, after
//! every budget ([`stamp`]): `_kin.flags` lists each flag, and `_kin.advice`
//! says it in a sentence. `status` answers with every open flag in the
//! repository, so an agent can start from them. While no flag is open nothing
//! is written, and every answer is the bytes it always was.
//!
//! The flags are written after the response budget, so an answer that
//! carries them can exceed its `max_chars` by their size: at most
//! [`ANSWER_FLAGS_MAX`] on an answer and [`LISTING_FLAGS_MAX`] on `status`,
//! each note at most 400 characters. Cutting a reviewer's note to fit a budget
//! would hand the agent half of what it was asked to address.
//!
//! A flag is advisory. Nothing here refuses a call or a commit.

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::session_handles::SessionHandles;
use crate::types::JsonRpcResponse;

/// Where a daemon's tool result carries the flags its answer reaches, inside
/// the result's `_meta`.
pub const META_KEY: &str = "kin/review_flags";

/// Flags one answer lists when it returned flagged entities.
pub const ANSWER_FLAGS_MAX: usize = 5;

/// Flags `status` lists.
pub const LISTING_FLAGS_MAX: usize = 25;

/// Flags the advice names one by one before it counts the rest.
const SPOKEN_FLAGS_MAX: usize = 3;

/// One open flag, as an answer carries it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlagNotice {
    /// The flagged entity's id.
    pub entity: String,
    /// The entity's name.
    pub name: String,
    /// The reviewer's note. Empty when the reviewer left none.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
    /// Who set the flag.
    pub by: String,
    /// When the flag was set.
    pub at: String,
    /// The entity is no longer the version the reviewer flagged.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub changed_since: bool,
    /// Kin could not read the current recorded source to compare it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub source_unavailable: bool,
}

/// The open flags one answer reaches.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delivered {
    pub flags: Vec<FlagNotice>,
    /// Open flags the answer reaches beyond the ones listed.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub more: usize,
    /// Every currently open flag, including earlier status pages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<usize>,
    /// Continue a status flag listing independently of its enrichment page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// The answer is the repository's whole list of open flags, not the flags
    /// on entities it returned.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub listing: bool,
}

fn is_zero(count: &usize) -> bool {
    *count == 0
}

impl Delivered {
    /// Whether there is anything to write into a reply. With no flag to
    /// list there is not, so while no flag is open every answer is served
    /// byte for byte as it was.
    pub fn is_silent(&self) -> bool {
        self.flags.is_empty()
    }

    /// Fold what another call of the same `tools/call` reached into this.
    fn absorb(&mut self, other: Delivered) {
        self.listing |= other.listing;
        self.more += other.more;
        if other.next_cursor.is_some() {
            self.next_cursor = other.next_cursor;
        }
        if other.total.is_some() {
            self.total = other.total;
        }
        for flag in other.flags {
            if !self.flags.iter().any(|held| held.entity == flag.entity) {
                self.flags.push(flag);
            }
        }
    }
}

/// Put what an answer reaches beside a daemon's tool result. A result that
/// reaches no flag is left exactly as it was.
pub fn attach(result: &mut Value, delivered: &Delivered) {
    if delivered.is_silent() {
        return;
    }
    let Some(result) = result.as_object_mut() else {
        return;
    };
    let Ok(value) = serde_json::to_value(delivered) else {
        return;
    };
    let meta = result
        .entry("_meta")
        .or_insert_with(|| Value::Object(Map::new()));
    if let Some(meta) = meta.as_object_mut() {
        meta.insert(META_KEY.to_string(), value);
    }
}

/// Take what a daemon's tool result says its answer reaches, leaving the
/// result as a daemon that knows no flags would have sent it.
pub fn take(result: &mut Value) -> Option<Delivered> {
    let fields = result.as_object_mut()?;
    let meta = fields.get_mut("_meta")?.as_object_mut()?;
    let delivered = meta.remove(META_KEY)?;
    if meta.is_empty() {
        fields.remove("_meta");
    }
    serde_json::from_value(delivered).ok()
}

tokio::task_local! {
    static REACHED: Arc<Mutex<Delivered>>;
}

/// Run one `tools/call`, and hand back with it the flags its daemon answers
/// reached. A call can ask the daemon more than once, a write that begins,
/// stages and commits for one, and what each answer reached is kept once.
pub async fn collecting<F: std::future::Future>(call: F) -> (F::Output, Delivered) {
    let reached = Arc::new(Mutex::new(Delivered::default()));
    let output = REACHED.scope(Arc::clone(&reached), call).await;
    let delivered = reached
        .lock()
        .map(|held| held.clone())
        .unwrap_or_else(|poisoned| poisoned.into_inner().clone());
    (output, delivered)
}

/// Keep what one daemon answer reached for the `tools/call` being run.
/// Outside [`collecting`] there is no reply to write it into, and it is
/// dropped.
pub(crate) fn note_reached(delivered: Delivered) {
    let _ = REACHED.try_with(|reached| {
        reached
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .absorb(delivered);
    });
}

/// A reviewer's note as the middle of a sentence: one line, with no closing
/// punctuation of its own.
fn spoken_note(note: &str) -> String {
    let line = note.split_whitespace().collect::<Vec<_>>().join(" ");
    line.trim_end_matches(['.', '!', '?', ';', ':', ','])
        .to_string()
}

/// One flag, in words.
fn sentence(flag: &FlagNotice) -> String {
    let note = spoken_note(&flag.note);
    let said = if note.is_empty() {
        format!("A reviewer flagged {} and left no note.", flag.name)
    } else {
        format!("A reviewer flagged {}: {note}.", flag.name)
    };
    if flag.source_unavailable {
        format!(
            "{said} Kin could not confirm its current source; the flag is still open. Address \
             it before committing."
        )
    } else if flag.changed_since {
        format!(
            "{said} It has changed since the flag was set, and the flag is still open; check \
             that the change answers it before committing."
        )
    } else if note.is_empty() {
        format!("{said} Look at it again before committing.")
    } else {
        format!("{said} Address it before committing.")
    }
}

fn entities(count: usize) -> String {
    if count == 1 {
        "1 entity".to_string()
    } else {
        format!("{count} entities")
    }
}

/// What an answer's flags say, as the sentences an agent acts on. `None` when
/// there is nothing to say.
pub fn advice(delivered: &Delivered) -> Option<String> {
    let total = delivered
        .total
        .unwrap_or(delivered.flags.len() + delivered.more);
    if delivered.listing {
        if total == 0 {
            return None;
        }
        let shown = delivered.flags.len();
        let listed = if delivered.more == 0 && total == shown {
            "Each is in _kin.flags with the reviewer's note.".to_string()
        } else {
            format!("This page lists {shown} in _kin.flags with the reviewer's note.")
        };
        let continuation = if delivered.next_cursor.is_some() {
            " Continue status with flags_cursor from _kin.flags_next_cursor."
        } else {
            ""
        };
        return Some(format!(
            "A reviewer flagged {} in this repository. {listed} Address them before committing.{continuation}",
            entities(total)
        ));
    }
    if delivered.flags.is_empty() {
        return None;
    }
    let mut said: Vec<String> = delivered
        .flags
        .iter()
        .take(SPOKEN_FLAGS_MAX)
        .map(sentence)
        .collect();
    let unspoken = total.saturating_sub(said.len());
    if unspoken > 0 {
        said.push(if unspoken == 1 {
            "1 more entity in this answer is flagged; status lists every open flag.".to_string()
        } else {
            format!(
                "{unspoken} more entities in this answer are flagged; status lists every open \
                 flag."
            )
        });
    }
    Some(said.join(" "))
}

fn presented_cursor(delivered: &Delivered, handles: Option<&SessionHandles>) -> Option<String> {
    let cursor = delivered.next_cursor.as_ref()?;
    let mut value = json!({"flags_next_cursor": cursor});
    if let Some(handles) = handles {
        handles.shorten_value(&mut value);
    }
    value["flags_next_cursor"].as_str().map(str::to_string)
}

/// The flags as `_kin.flags` lists them, each entity under the handle this
/// connection serves it by.
fn listed(delivered: &Delivered, handles: Option<&SessionHandles>) -> Value {
    Value::Array(
        delivered
            .flags
            .iter()
            .map(|flag| {
                let mut flag = flag.clone();
                if let Some(handles) = handles {
                    flag.entity = handles.handle_for_uuid(&flag.entity);
                }
                serde_json::to_value(flag).unwrap_or(Value::Null)
            })
            .collect(),
    )
}

/// Text answers have no `_kin.flags` block, so every handed note must be
/// written here even when the JSON advice would only summarize it.
fn text_notices(delivered: &Delivered, handles: Option<&SessionHandles>) -> String {
    let mut notices = Vec::new();
    if delivered.listing {
        notices.push(format!(
            "A reviewer flagged {} in this repository.",
            entities(
                delivered
                    .total
                    .unwrap_or(delivered.flags.len() + delivered.more)
            )
        ));
    }
    for flag in &delivered.flags {
        let entity = handles
            .map(|handles| handles.handle_for_uuid(&flag.entity))
            .unwrap_or_else(|| flag.entity.clone());
        let note = if flag.note.trim().is_empty() {
            "The reviewer left no note."
        } else {
            &flag.note
        };
        let action = if flag.source_unavailable {
            "Kin could not confirm its current source; the flag is still open. Address it \
             before committing."
        } else if flag.changed_since {
            "It has changed since the flag was set, and the flag is still open; check that the \
             change answers it before committing."
        } else {
            "Address it before committing."
        };
        notices.push(format!(
            "Reviewer flag on {} ({entity}), by {}:\n{note}\n{action}",
            flag.name, flag.by
        ));
    }
    if delivered.more > 0 {
        notices.push(format!(
            "{} more flagged {} not listed here; status lists open flags.",
            delivered.more,
            if delivered.more == 1 {
                "entity is"
            } else {
                "entities are"
            }
        ));
    }
    if let Some(cursor) = presented_cursor(delivered, handles) {
        notices.push(format!(
            "Continue status with flags_cursor: {cursor}. This is separate from the enrichment cursor."
        ));
    }
    notices.join("\n\n")
}

/// Write the flags into one JSON answer's `_kin` block, after whatever advice
/// it already carries.
fn stamp_payload(
    payload: &mut Map<String, Value>,
    delivered: &Delivered,
    handles: Option<&SessionHandles>,
) {
    let key = crate::envelope::ENVELOPE_KEY;
    let envelope = payload
        .entry(key)
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(envelope) = envelope.as_object_mut() else {
        return;
    };
    envelope.insert("flags".to_string(), listed(delivered, handles));
    if let Some(cursor) = presented_cursor(delivered, handles) {
        envelope.insert("flags_next_cursor".to_string(), json!(cursor));
    }
    if let Some(flagged) = advice(delivered) {
        let said = match envelope.get("advice").and_then(Value::as_str) {
            Some(existing) if !existing.trim().is_empty() => format!("{existing} {flagged}"),
            _ => flagged,
        };
        envelope.insert("advice".to_string(), json!(said));
    }
}

/// Write the flags an answer reached into the reply the agent reads.
///
/// Runs last, on the reply as it ships. A JSON answer gets `_kin.flags` and
/// the sentences in `_kin.advice`; an answer in words gets every handed note
/// as its first lines. Prompt messages and refusals carry the same notes. A
/// reply that reached no flag is left byte for byte as it was.
pub fn stamp(
    response: &mut JsonRpcResponse,
    delivered: &Delivered,
    handles: Option<&SessionHandles>,
) {
    if delivered.is_silent() {
        return;
    }
    if let Some(error) = response.error.as_mut() {
        error.message = format!("{}\n\n{}", text_notices(delivered, handles), error.message);
    }
    let Some(result) = response.result.as_mut() else {
        return;
    };
    if let Some(structured) = result
        .get_mut("structuredContent")
        .and_then(Value::as_object_mut)
    {
        stamp_payload(structured, delivered, handles);
    }
    if let Some(text) = result
        .pointer_mut("/messages/0/content/text")
        .filter(|text| text.is_string())
    {
        *text = Value::String(format!(
            "{}\n\n{}",
            text_notices(delivered, handles),
            text.as_str().unwrap_or_default()
        ));
        return;
    }
    let Some(block) = result
        .get_mut("content")
        .and_then(Value::as_array_mut)
        .and_then(|content| content.first_mut())
    else {
        return;
    };
    let Some(text) = block.get("text").and_then(Value::as_str) else {
        return;
    };
    let stamped = match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(mut payload)) => {
            stamp_payload(&mut payload, delivered, handles);
            match crate::budget::render_in_format_of(&Value::Object(payload), text) {
                Ok(rendered) => rendered,
                Err(_) => return,
            }
        }
        _ => format!("{}\n\n{text}", text_notices(delivered, handles)),
    };
    block["text"] = Value::String(stamped);
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEND: &str = "50ba786c-1111-4222-8333-444455556666";
    const NOT_FOUND: &str = "7c0ffee0-1111-4222-8333-444455556666";

    fn flag(entity: &str, name: &str, note: &str) -> FlagNotice {
        FlagNotice {
            entity: entity.to_string(),
            name: name.to_string(),
            note: note.to_string(),
            by: "Ada Reviewer".to_string(),
            at: "2026-10-13T23:40:00Z".to_string(),
            changed_since: false,
            source_unavailable: false,
        }
    }

    fn reached(flags: Vec<FlagNotice>) -> Delivered {
        Delivered {
            flags,
            more: 0,
            total: None,
            next_cursor: None,
            listing: false,
        }
    }

    fn reply(text: &str) -> JsonRpcResponse {
        JsonRpcResponse::success(
            Some(json!(1)),
            json!({"content": [{"type": "text", "text": text}]}),
        )
    }

    fn text_of(response: &JsonRpcResponse) -> String {
        response.result.as_ref().unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[test]
    fn a_flag_is_said_in_one_sentence_that_names_the_entity_and_the_note() {
        let noted = flag(SEND, "ServerReply.send", "a second send must fail loudly.");
        assert_eq!(
            sentence(&noted),
            "A reviewer flagged ServerReply.send: a second send must fail loudly. \
             Address it before committing."
        );
        let bare = flag(SEND, "ServerReply.send", "  ");
        assert_eq!(
            sentence(&bare),
            "A reviewer flagged ServerReply.send and left no note. Look at it again before \
             committing."
        );
        let mut moved = flag(SEND, "ServerReply.send", "throw\non a second send");
        moved.changed_since = true;
        let said = sentence(&moved);
        assert!(
            said.starts_with("A reviewer flagged ServerReply.send: throw on a second send. "),
            "{said}"
        );
        assert!(said.contains("changed since the flag was set"), "{said}");
        assert!(said.contains("still open"), "{said}");

        moved.source_unavailable = true;
        let said = sentence(&moved);
        assert!(
            said.contains("could not confirm its current source"),
            "{said}"
        );
        assert!(!said.contains("has changed"), "{said}");
    }

    #[test]
    fn a_json_answer_carries_the_flags_in_its_envelope_and_its_advice() {
        let mut response = reply(
            &json!({
                "_kin": {"meaning": "Answer: 1 declaration.", "verdict": "complete"},
                "results": [{"entity_id": SEND, "name": "ServerReply.send"}],
            })
            .to_string(),
        );
        let delivered = reached(vec![flag(
            SEND,
            "ServerReply.send",
            "throw on a second send",
        )]);
        stamp(&mut response, &delivered, None);
        let text = text_of(&response);
        // The advice is the first thing the answer says.
        assert!(
            text.starts_with("{\"_kin\":{\"advice\":\"A reviewer flagged ServerReply.send: "),
            "{text}"
        );
        let payload: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            payload["_kin"]["flags"],
            json!([{
                "entity": SEND, "name": "ServerReply.send", "note": "throw on a second send",
                "by": "Ada Reviewer", "at": "2026-10-13T23:40:00Z",
            }])
        );
        assert_eq!(payload["_kin"]["verdict"], "complete");
        assert_eq!(payload["_kin"]["meaning"], "Answer: 1 declaration.");
        assert_eq!(payload["results"][0]["entity_id"], SEND);
    }

    #[test]
    fn a_flag_names_its_entity_by_the_handle_the_connection_serves() {
        let handles = SessionHandles::default();
        let short = handles.handle_for_uuid(SEND);
        assert_ne!(short, SEND);
        let mut response = reply(&json!({"_kin": {"meaning": "m"}}).to_string());
        stamp(
            &mut response,
            &reached(vec![flag(SEND, "ServerReply.send", "n")]),
            Some(&handles),
        );
        let payload: Value = serde_json::from_str(&text_of(&response)).unwrap();
        assert_eq!(payload["_kin"]["flags"][0]["entity"], json!(short));
    }

    #[test]
    fn advice_an_answer_already_gives_stays_first() {
        let mut response = reply(
            &json!({"_kin": {"advice": "This answer is from another repository."}}).to_string(),
        );
        stamp(
            &mut response,
            &reached(vec![flag(SEND, "ServerReply.send", "n")]),
            None,
        );
        let payload: Value = serde_json::from_str(&text_of(&response)).unwrap();
        assert_eq!(
            payload["_kin"]["advice"],
            "This answer is from another repository. A reviewer flagged ServerReply.send: n. \
             Address it before committing."
        );
    }

    #[test]
    fn an_answer_in_words_says_the_flag_first_and_structured_content_agrees() {
        let mut response = JsonRpcResponse::success(
            Some(json!(1)),
            json!({
                "content": [{"type": "text", "text": "ServerReply.send  method"}],
                "structuredContent": {"_kin": {"verdict": "complete"}},
            }),
        );
        stamp(
            &mut response,
            &reached(vec![flag(SEND, "ServerReply.send", "n")]),
            None,
        );
        assert_eq!(
            text_of(&response),
            format!(
                "Reviewer flag on ServerReply.send ({SEND}), by Ada Reviewer:\nn\n\
                 Address it before committing.\n\nServerReply.send  method"
            )
        );
        let structured = &response.result.as_ref().unwrap()["structuredContent"];
        assert_eq!(structured["_kin"]["flags"][0]["name"], "ServerReply.send");
        assert_eq!(structured["_kin"]["verdict"], "complete");
    }

    #[test]
    fn a_text_answer_writes_every_handed_note_with_its_entity_handle() {
        let handles = SessionHandles::default();
        let flags: Vec<FlagNotice> = (0..ANSWER_FLAGS_MAX)
            .map(|index| {
                flag(
                    &format!("0000000{index}-1111-4222-8333-444455556666"),
                    &format!("entity_{index}"),
                    &format!("note_{index}\nKeep this second line."),
                )
            })
            .collect();
        let mut response = reply("Answer (complete): five declarations.");
        stamp(&mut response, &reached(flags.clone()), Some(&handles));
        let text = text_of(&response);
        for flag in flags {
            assert!(text.contains(&flag.note), "{text}");
            assert!(
                text.contains(&handles.handle_for_uuid(&flag.entity)),
                "{text}"
            );
            assert!(!text.contains(&flag.entity), "{text}");
        }
        assert!(text.ends_with("Answer (complete): five declarations."));
    }

    #[test]
    fn a_text_listing_writes_notes_instead_of_pointing_at_an_absent_json_block() {
        let mut listing = reached(vec![
            flag(SEND, "ServerReply.send", "throw on a second send"),
            flag(NOT_FOUND, "notFound", "check the fallback"),
        ]);
        listing.listing = true;
        listing.more = 1;
        let mut response = reply("Graph health is settled.");
        stamp(&mut response, &listing, None);
        let text = text_of(&response);
        assert!(
            text.starts_with("A reviewer flagged 3 entities in this repository."),
            "{text}"
        );
        assert!(text.contains("throw on a second send"), "{text}");
        assert!(text.contains("check the fallback"), "{text}");
        assert!(
            text.contains("1 more flagged entity is not listed here"),
            "{text}"
        );
        assert!(!text.contains("_kin.flags"), "{text}");
    }

    #[test]
    fn a_status_flag_cursor_is_separate_from_enrichment_and_reads_back_under_handles() {
        let handles = SessionHandles::default();
        let mut listing = reached(vec![flag(SEND, "ServerReply.send", "n")]);
        listing.listing = true;
        listing.more = 1;
        listing.total = Some(27);
        listing.next_cursor = Some(format!("review-flags:{NOT_FOUND}"));
        let mut response = reply(
            &json!({
                "_kin": {"meaning": "Health report."},
                "enrichment": {"page": {"next_cursor": "metadata-page"}},
            })
            .to_string(),
        );
        stamp(&mut response, &listing, Some(&handles));
        let payload: Value = serde_json::from_str(&text_of(&response)).unwrap();
        let cursor = payload["_kin"]["flags_next_cursor"].as_str().unwrap();
        assert_ne!(cursor, listing.next_cursor.as_deref().unwrap());
        assert_eq!(
            payload["enrichment"]["page"]["next_cursor"],
            "metadata-page"
        );
        assert!(payload["_kin"]["advice"]
            .as_str()
            .unwrap()
            .contains("27 entities"));
        let mut arguments = json!({"flags_cursor": cursor});
        assert!(handles.expand_arguments(&mut arguments));
        assert_eq!(arguments["flags_cursor"], listing.next_cursor.unwrap());
    }

    #[test]
    fn a_prompt_message_carries_every_handed_note() {
        let delivered = reached(vec![flag(
            SEND,
            "ServerReply.send",
            "throw on a second send",
        )]);
        let mut response = JsonRpcResponse::success(
            Some(json!(1)),
            json!({
                "description": "Kin's answer",
                "messages": [{"role": "user", "content": {
                    "type": "text", "text": "Who calls send?\n\nKin's answer."
                }}],
            }),
        );
        stamp(&mut response, &delivered, None);
        let result = response.result.unwrap();
        let text = result
            .pointer("/messages/0/content/text")
            .unwrap()
            .as_str()
            .unwrap();
        assert!(text.contains("throw on a second send"), "{text}");
        assert!(text.contains(SEND), "{text}");
        assert!(text.ends_with("Who calls send?\n\nKin's answer."), "{text}");
        assert_eq!(result["messages"][0]["role"], "user");
        assert_eq!(result["description"], "Kin's answer");
    }

    #[test]
    fn a_refused_composite_call_still_carries_the_flags_its_subcalls_handed_back() {
        let mut response = JsonRpcResponse::error(Some(json!(1)), -32602, "ask failed".into());
        stamp(
            &mut response,
            &reached(vec![flag(
                SEND,
                "ServerReply.send",
                "throw on a second send",
            )]),
            None,
        );
        let error = response.error.unwrap();
        assert_eq!(error.code, -32602);
        assert!(
            error.message.contains("throw on a second send"),
            "{}",
            error.message
        );
        assert!(error.message.ends_with("ask failed"), "{}", error.message);
        assert!(response.result.is_none());
    }

    #[test]
    fn an_answer_that_reached_no_flag_is_left_as_it_was() {
        let original = json!({"_kin": {"meaning": "m"}, "results": []}).to_string();
        let mut response = reply(&original);
        stamp(&mut response, &Delivered::default(), None);
        assert_eq!(text_of(&response), original);
    }

    #[test]
    fn the_listing_counts_every_open_flag_and_says_so_when_there_are_none() {
        let mut listing = reached(vec![
            flag(SEND, "ServerReply.send", "n"),
            flag(NOT_FOUND, "notFound", ""),
        ]);
        listing.listing = true;
        assert_eq!(
            advice(&listing).unwrap(),
            "A reviewer flagged 2 entities in this repository. Each is in _kin.flags with the \
             reviewer's note. Address them before committing."
        );
        listing.more = 30;
        assert!(advice(&listing)
            .unwrap()
            .starts_with("A reviewer flagged 32 entities in this repository. This page lists 2"));

        // With none open there is nothing to list, and the answer is left as
        // it was.
        let none = Delivered {
            listing: true,
            ..Default::default()
        };
        assert!(none.is_silent());
        assert_eq!(advice(&none), None);
        let original = json!({"_kin": {"meaning": "Health report."}}).to_string();
        let mut response = reply(&original);
        stamp(&mut response, &none, None);
        assert_eq!(text_of(&response), original);
    }

    #[test]
    fn flags_past_the_spoken_ones_are_counted_not_dropped() {
        let flags: Vec<FlagNotice> = (0..5)
            .map(|index| {
                flag(
                    &format!("0000000{index}-1111-4222-8333-444455556666"),
                    &format!("entity_{index}"),
                    "n",
                )
            })
            .collect();
        let mut delivered = reached(flags);
        delivered.more = 2;
        let said = advice(&delivered).unwrap();
        assert!(
            said.contains("entity_0") && said.contains("entity_2"),
            "{said}"
        );
        assert!(!said.contains("entity_3"), "{said}");
        assert!(
            said.ends_with(
                "4 more entities in this answer are flagged; status lists every open flag."
            ),
            "{said}"
        );
    }

    #[test]
    fn the_daemon_hands_flags_back_beside_the_answer_and_the_server_takes_them() {
        let answer = json!({"content": [{"type": "text", "text": "{}"}]});
        let mut sent = answer.clone();
        attach(&mut sent, &Delivered::default());
        assert_eq!(sent, answer, "an answer that reached no flag is unchanged");

        let delivered = reached(vec![flag(SEND, "ServerReply.send", "n")]);
        attach(&mut sent, &delivered);
        assert_eq!(sent["content"], answer["content"]);
        assert!(sent["_meta"][META_KEY].is_object());
        // An older server reads the same result and ignores what it does not know.
        let read: crate::ToolCallResult = serde_json::from_value(sent.clone()).unwrap();
        assert_eq!(read.content.len(), 1);

        assert_eq!(take(&mut sent), Some(delivered));
        assert_eq!(sent, answer);
        assert_eq!(take(&mut sent), None);
    }

    #[tokio::test]
    async fn one_call_keeps_what_each_daemon_answer_reached_once() {
        let ((), delivered) = collecting(async {
            note_reached(reached(vec![flag(SEND, "ServerReply.send", "n")]));
            note_reached(reached(vec![
                flag(SEND, "ServerReply.send", "n"),
                flag(NOT_FOUND, "notFound", ""),
            ]));
        })
        .await;
        assert_eq!(delivered.flags.len(), 2);
        // Outside a call there is nothing to write into, and nothing is kept.
        note_reached(reached(vec![flag(SEND, "ServerReply.send", "n")]));
        let ((), empty) = collecting(async {}).await;
        assert!(empty.is_silent());
    }
}
