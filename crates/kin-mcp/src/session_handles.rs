// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Short handles for the long opaque values an agent passes back.
//!
//! An agent profile's replies name entities by UUID, pages by cursor and
//! source reads by ticket, and every one of them is re-sent to the model on
//! every later turn. A UUID costs a tokenizer about 32 tokens, a cursor about
//! 86 and a ticket about 65, and a reference row carries several. This
//! connection serves each such value under a short handle instead, and reads
//! the handle back as the value on every call it receives:
//!
//! - a UUID as its first eight hex digits (`cfaa9527`), longer when two
//!   served UUIDs share them;
//! - a change id (64 hex digits) as its first twelve;
//! - a cursor (any `*cursor` field) as `p` and seven hex digits;
//! - a source ticket as `t` and seven hex digits.
//!
//! Handles are only ever minted for values this connection served, and only
//! those are read back, so a mistyped handle names nothing rather than a
//! neighbour. A full value is always accepted unchanged. The table lives as
//! long as the connection; a value served earlier keeps its handle.
//!
//! This is presentation only: membership, verdicts and gaps are computed
//! before it, and it maps strings one to one. Prose is never rewritten.

use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::Value;

/// Argument fields that carry code or free text; a handle-shaped string in
/// them is the caller's text, never a handle.
const FREE_TEXT_KEYS: [&str; 9] = [
    "body",
    "new_text",
    "old_text",
    "description",
    "summary",
    "message",
    "code",
    "literal",
    "question",
];

/// The digits a UUID handle starts with.
const UUID_HANDLE_HEX: usize = 8;
/// The digits a change-id handle starts with.
const CHANGE_HANDLE_HEX: usize = 12;
/// The digits a cursor or ticket handle carries after its letter.
const OPAQUE_HANDLE_HEX: usize = 7;

#[derive(Debug, Default)]
pub struct SessionHandles {
    table: Mutex<Table>,
}

#[derive(Clone, Debug, Default)]
struct Table {
    short_of: HashMap<String, String>,
    full_of: HashMap<String, String>,
    /// Display names served for more than one entity: none reads back.
    ambiguous: std::collections::HashSet<String>,
}

#[derive(Clone, Copy)]
enum Kind {
    Uuid,
    Change,
    Cursor,
    Ticket,
}

impl Table {
    fn handle(&mut self, full: &str, kind: Kind) -> String {
        if let Some(short) = self.short_of.get(full) {
            return short.clone();
        }
        let candidates: Vec<String> = match kind {
            Kind::Uuid => {
                let hex: String = full.chars().filter(|c| *c != '-').collect();
                [UUID_HANDLE_HEX, 12, 16, 20]
                    .iter()
                    .map(|n| hex[..*n].to_string())
                    .collect()
            }
            Kind::Change => [CHANGE_HANDLE_HEX, 16, 24]
                .iter()
                .map(|n| full[..*n].to_string())
                .collect(),
            Kind::Cursor | Kind::Ticket => {
                let letter = if matches!(kind, Kind::Cursor) {
                    'p'
                } else {
                    't'
                };
                let digest = format!("{:016x}", fnv1a(full));
                [OPAQUE_HANDLE_HEX, 10, 13, 16]
                    .iter()
                    .map(|n| format!("{letter}{}", &digest[..*n]))
                    .collect()
            }
        };
        // A handle names exactly one value; on a clash the longer form is
        // used, and the full value itself when every form is taken.
        let short = candidates
            .into_iter()
            .find(|short| !self.full_of.contains_key(short))
            .unwrap_or_else(|| full.to_string());
        if short != full {
            self.full_of.insert(short.clone(), full.to_string());
        }
        self.short_of.insert(full.to_string(), short.clone());
        short
    }

    fn shorten(&mut self, value: &mut Value, key: Option<&str>) {
        match value {
            Value::String(text) => {
                if let Some(kind) = kind_of(text, key) {
                    *text = self.handle(text, kind);
                }
            }
            Value::Array(items) => {
                for item in items {
                    self.shorten(item, key);
                }
            }
            Value::Object(fields) => {
                for (field, item) in fields.iter_mut() {
                    self.shorten(item, Some(field));
                }
            }
            _ => {}
        }
    }

    fn expand(&self, value: &mut Value, key: Option<&str>) -> bool {
        if key.is_some_and(|key| FREE_TEXT_KEYS.contains(&key)) {
            return false;
        }
        match value {
            Value::String(text) => match self.lookup(text) {
                Some(full) => {
                    *text = full.to_string();
                    true
                }
                None => false,
            },
            Value::Array(items) => items
                .iter_mut()
                .fold(false, |changed, item| self.expand(item, key) | changed),
            Value::Object(fields) => fields.iter_mut().fold(false, |changed, (field, item)| {
                self.expand(item, Some(field)) | changed
            }),
            _ => false,
        }
    }
}

impl Table {
    /// The value a served handle or label stands for, also when a text reply's
    /// `name #handle` or `#handle` spelling is passed back whole.
    fn lookup(&self, text: &str) -> Option<&str> {
        let text = text.trim();
        if let Some(full) = self.full_of.get(text) {
            return Some(full);
        }
        let handle = text
            .rsplit_once('#')
            .map_or(text, |(_, handle)| handle)
            .trim();
        (handle != text)
            .then(|| self.full_of.get(handle))
            .flatten()
            .map(String::as_str)
    }
}

fn kind_of(text: &str, key: Option<&str>) -> Option<Kind> {
    if is_uuid(text) {
        return Some(Kind::Uuid);
    }
    if text.len() == 64 && text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Some(Kind::Change);
    }
    let key = key?;
    if key.ends_with("cursor") && text.len() > 24 && !text.contains(' ') {
        return Some(Kind::Cursor);
    }
    if key == crate::source_ticket::TICKET_KEY && text.len() > 24 && !text.contains(' ') {
        return Some(Kind::Ticket);
    }
    None
}

/// A hyphenated UUID, `8-4-4-4-12` hex digits.
pub fn is_uuid(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(at, byte)| match at {
            8 | 13 | 18 | 23 => *byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
}

fn fnv1a(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

impl SessionHandles {
    /// The handle this connection serves for `full`, minting it on first use.
    /// A copy of this table, for rendering a reply that may not ship.
    ///
    /// A reply is sized as its text, and its text depends on the handles it
    /// is written with. What a trial rendering mints lands in the copy, so a
    /// value the connection never served never reads back on it.
    pub(crate) fn scratch(&self) -> Self {
        let table = self.table.lock().unwrap_or_else(|error| error.into_inner());
        Self {
            table: Mutex::new(table.clone()),
        }
    }

    pub fn handle_for_uuid(&self, full: &str) -> String {
        let mut table = self.table.lock().unwrap_or_else(|e| e.into_inner());
        if is_uuid(full) {
            table.handle(full, Kind::Uuid)
        } else {
            full.to_string()
        }
    }

    /// Record that this connection served `alias` for `full` (a display name
    /// that stands for exactly one entity), so the alias reads back as it.
    /// An alias already standing for another value is withdrawn: it would no
    /// longer name one thing.
    pub fn alias(&self, alias: &str, full: &str) -> bool {
        let mut table = self.table.lock().unwrap_or_else(|e| e.into_inner());
        if table.ambiguous.contains(alias) {
            return false;
        }
        match table.full_of.get(alias) {
            Some(existing) if existing == full => true,
            Some(_) => {
                table.full_of.remove(alias);
                table.ambiguous.insert(alias.to_string());
                false
            }
            None => {
                table.full_of.insert(alias.to_string(), full.to_string());
                true
            }
        }
    }

    /// Replace every UUID, change id, cursor and ticket in a reply value with
    /// its handle.
    pub fn shorten_value(&self, value: &mut Value) {
        let mut table = self.table.lock().unwrap_or_else(|e| e.into_inner());
        table.shorten(value, None);
    }

    /// Replace every handle this connection served in a call's arguments
    /// with the value it stands for. Returns whether anything changed.
    pub fn expand_arguments(&self, arguments: &mut Value) -> bool {
        let table = self.table.lock().unwrap_or_else(|e| e.into_inner());
        table.expand(arguments, None)
    }

    /// A `tools/call` request with its argument handles read back, or `None`
    /// when the message is not one or names no handle.
    pub fn expand_request(&self, message: &str) -> Option<String> {
        let mut request: Value = serde_json::from_str(message).ok()?;
        if request.get("method").and_then(Value::as_str) != Some("tools/call") {
            return None;
        }
        let arguments = request.pointer_mut("/params/arguments")?;
        if !self.expand_arguments(arguments) {
            return None;
        }
        serde_json::to_string(&request).ok()
    }

    /// Shorten every JSON text block and the structured content of a tool
    /// result in place. Text that is not JSON is left as it is.
    pub fn shorten_result(&self, result: &mut Value) {
        if let Some(blocks) = result.get_mut("content").and_then(Value::as_array_mut) {
            for block in blocks {
                let Some(text) = block.get("text").and_then(Value::as_str) else {
                    continue;
                };
                let Ok(mut payload) = serde_json::from_str::<Value>(text) else {
                    continue;
                };
                if !payload.is_object() {
                    continue;
                }
                self.shorten_value(&mut payload);
                if let Ok(text) = serde_json::to_string(&payload) {
                    block["text"] = Value::String(text);
                }
            }
        }
        if let Some(structured) = result.get_mut("structuredContent") {
            self.shorten_value(structured);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ID: &str = "cfaa9527-8673-4daa-8d27-d8a5ac89d37c";
    const TWIN: &str = "cfaa9527-0000-4daa-8d27-d8a5ac89d37c";
    const CURSOR: &str =
        "eyJ2IjoxLCJzbmFwc2hvdCI6IjJiOWRmZTJlLWE1NWItNDg1YS1iNTgxLWRiZDJiYWYyZWM2ZCJ9";

    #[test]
    fn served_values_read_back_and_nothing_else_does() {
        let handles = SessionHandles::default();
        let mut reply = json!({
            "entity_id": ID,
            "rows": [{"id": TWIN, "name": "route"}],
            "next_cursor": CURSOR,
            "source_base": {"ticket": format!("sb1.{ID}.8.1c976b444644a10a5ab5f51ccee2ec30")},
            "_kin": {"meaning": format!("prose naming {ID} stays whole")},
        });
        handles.shorten_value(&mut reply);
        assert_eq!(reply["entity_id"], "cfaa9527");
        assert_eq!(
            reply["rows"][0]["id"], "cfaa95270000",
            "a clash takes the longer form"
        );
        let page = reply["next_cursor"].as_str().unwrap().to_string();
        let ticket = reply["source_base"]["ticket"].as_str().unwrap().to_string();
        assert!(page.starts_with('p') && page.len() == 8);
        assert!(ticket.starts_with('t') && ticket.len() == 8);
        assert!(reply["_kin"]["meaning"].as_str().unwrap().contains(ID));

        let mut call = json!({
            "entity_id": "cfaa9527",
            "entities": ["cfaa95270000", "Blueprint"],
            "cursor": page,
            "operations": [{"payload": {"EntityRemove": {"source_base": {"ticket": ticket}}},
                            "description": "cfaa9527"}],
            "query": "deadbeef",
        });
        assert!(handles.expand_arguments(&mut call));
        assert_eq!(call["entity_id"], ID);
        assert_eq!(call["entities"], json!([TWIN, "Blueprint"]));
        assert_eq!(call["cursor"], CURSOR);
        assert!(
            call["operations"][0]["payload"]["EntityRemove"]["source_base"]["ticket"]
                .as_str()
                .unwrap()
                .starts_with("sb1.")
        );
        assert_eq!(
            call["operations"][0]["description"], "cfaa9527",
            "free text is kept"
        );
        assert_eq!(
            call["query"], "deadbeef",
            "an unserved handle names nothing"
        );
    }

    #[test]
    fn a_handle_is_stable_for_the_connection() {
        let handles = SessionHandles::default();
        let mut first = json!({"entity_id": ID});
        let mut second = json!({"members": [{"id": ID}]});
        handles.shorten_value(&mut first);
        handles.shorten_value(&mut second);
        assert_eq!(first["entity_id"], second["members"][0]["id"]);
        let mut full = json!({"entity_id": ID});
        assert!(
            !handles.expand_arguments(&mut full),
            "a full id passes unchanged"
        );
        assert_eq!(full["entity_id"], ID);
    }

    #[test]
    fn an_alias_names_one_entity_or_none() {
        let handles = SessionHandles::default();
        assert!(handles.alias("flask.app :: Flask.route", ID));
        assert!(handles.alias("flask.app :: Flask.route", ID));
        let mut call = json!({"entity_id": "flask.app :: Flask.route"});
        assert!(handles.expand_arguments(&mut call));
        assert_eq!(call["entity_id"], ID);
        handles.shorten_value(&mut json!({"id": TWIN}));
        let mut spelled =
            json!({"a": "flask.app :: other #cfaa9527", "b": "#cfaa9527", "c": "x #nothing"});
        assert!(handles.expand_arguments(&mut spelled));
        assert_eq!(spelled, json!({"a": TWIN, "b": TWIN, "c": "x #nothing"}));
        assert!(!handles.alias("flask.app :: Flask.route", TWIN));
        assert!(
            !handles.alias("flask.app :: Flask.route", ID),
            "withdrawn for good"
        );
        let mut call = json!({"entity_id": "flask.app :: Flask.route"});
        assert!(
            !handles.expand_arguments(&mut call),
            "an ambiguous name reads back as itself"
        );
    }
}
