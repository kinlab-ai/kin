// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! One answer the front door asked for, as the client of that verb reads it.
//!
//! The front door runs Kin's own verbs through the same path a client's call
//! takes and reads each answer in its plain form: the one verdict word under
//! `_kin.verdict`, the rows with their `module` and `name`, the declarations
//! Kin lists as not held, the count, and the cursor. Nothing here reads a
//! reply's prose.

use serde_json::{Map, Value};

use crate::plain_reply::{COMPLETE_EXCEPT, COMPLETE_EXCEPT_INDIRECT};

/// The plain verdict word of a complete answer.
pub const COMPLETE: &str = "complete";
/// The plain verdict word of an answer that is not established as whole.
pub const PARTIAL: &str = "partial";

/// One declaration a name lookup returned: a held entity or one Kin lists as
/// not held.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declaration {
    /// The id a later call names the entity by, as this connection serves it.
    /// Empty for a declaration Kin holds no entity for.
    pub entity_id: String,
    pub module: String,
    pub name: String,
    /// Why Kin holds no entity for it, for a not-held declaration.
    pub why: Option<String>,
}

impl Declaration {
    /// The row as every answer writes it: `module :: name`.
    pub fn handle(&self) -> String {
        if self.module.is_empty() {
            self.name.clone()
        } else {
            format!("{} :: {}", self.module, self.name)
        }
    }

    /// The name's own last segment, through any separator a language
    /// writes between an owner and its member.
    pub fn own_name(&self) -> &str {
        last_segment(&self.name)
    }

    fn from_row(row: &Value) -> Option<Self> {
        let name = row.get("name")?.as_str()?.to_string();
        let text = |key: &str| row.get(key).and_then(Value::as_str).map(str::to_string);
        Some(Self {
            entity_id: text("entity_id").unwrap_or_default(),
            module: text("module").unwrap_or_default(),
            name,
            why: text("why").or_else(|| text("reason")),
        })
    }
}

/// `name`'s last segment through any separator a stated language writes.
pub fn last_segment(name: &str) -> &str {
    let mut last = name;
    for separator in kin_parser::name_syntax::written_separators() {
        if let Some((_, after)) = last.rsplit_once(separator) {
            last = after;
        }
    }
    last
}

/// One sub-call's answer.
#[derive(Debug, Clone)]
pub struct SubAnswer {
    /// The verb that ran.
    pub verb: String,
    /// The arguments it ran with.
    pub arguments: Map<String, Value>,
    /// Its plain answer, when the answer is a JSON object.
    pub payload: Option<Map<String, Value>>,
    /// Its text, when it is not.
    pub text: String,
    pub is_error: bool,
    pub seconds: f64,
    pub bytes: usize,
}

impl SubAnswer {
    /// The one plain verdict word, when the answer carries one.
    pub fn verdict(&self) -> Option<&str> {
        self.payload
            .as_ref()?
            .get(crate::envelope::ENVELOPE_KEY)?
            .get("verdict")?
            .as_str()
    }

    /// What the verdict means, in Kin's own sentence.
    pub fn meaning(&self) -> Option<&str> {
        self.payload
            .as_ref()?
            .get(crate::envelope::ENVELOPE_KEY)?
            .get("meaning")?
            .as_str()
    }

    /// Whether the answer says it left rows out of this reply.
    pub fn withheld(&self) -> bool {
        self.payload
            .as_ref()
            .and_then(|payload| payload.get(crate::envelope::ENVELOPE_KEY))
            .is_some_and(|envelope| envelope.get("delivered").is_some())
    }

    /// The cursor to the next page, when more rows follow.
    pub fn next_cursor(&self) -> Option<&str> {
        self.payload
            .as_ref()?
            .get("next_cursor")?
            .as_str()
            .filter(|cursor| !cursor.is_empty())
    }

    pub fn is_partial(&self) -> bool {
        self.verdict() == Some(PARTIAL)
    }

    /// The held rows of a name lookup.
    pub fn declarations(&self) -> Vec<Declaration> {
        self.rows("results")
    }

    /// The declarations a name lookup lists as not held.
    pub fn not_held(&self) -> Vec<Declaration> {
        self.rows("not_held")
    }

    /// The count the answer states for its held rows.
    pub fn total(&self) -> Option<u64> {
        self.payload.as_ref()?.get("total")?.as_u64()
    }

    fn rows(&self, key: &str) -> Vec<Declaration> {
        self.payload
            .as_ref()
            .and_then(|payload| payload.get(key))
            .and_then(Value::as_array)
            .map(|rows| rows.iter().filter_map(Declaration::from_row).collect())
            .unwrap_or_default()
    }

    /// The answer without its cursor and its "more rows" line, for a reply
    /// that fetched the rest or says in its own words what it left out.
    pub fn without_continuation(&self) -> Self {
        let mut trimmed = self.clone();
        if let Some(payload) = trimmed.payload.as_mut() {
            payload.remove("next_cursor");
            if let Some(envelope) = payload
                .get_mut(crate::envelope::ENVELOPE_KEY)
                .and_then(Value::as_object_mut)
            {
                envelope.remove("delivered");
            }
        }
        trimmed
    }

    /// The answer as a client on a text connection reads it.
    pub fn rendered(&self, handles: &crate::session_handles::SessionHandles) -> String {
        match &self.payload {
            Some(payload) => {
                let arguments: std::collections::HashMap<String, Value> =
                    self.arguments.clone().into_iter().collect();
                let tool = crate::verbs::handler_name(&self.verb, &arguments);
                crate::text_reply::render(payload, &tool, handles)
            }
            None => self.text.clone(),
        }
    }
}

/// How far an answer can be relied on, from the plain verdict word: a
/// complete answer, one complete except what it lists, or one that is not
/// established as whole. A missing or unknown word reads as the last.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Standing {
    Complete,
    CompleteExcept,
    NotEstablished,
}

impl Standing {
    pub fn of(word: Option<&str>) -> Self {
        match word {
            Some(COMPLETE) => Self::Complete,
            Some(word) if word == COMPLETE_EXCEPT || word == COMPLETE_EXCEPT_INDIRECT => {
                Self::CompleteExcept
            }
            _ => Self::NotEstablished,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn answer(payload: Value) -> SubAnswer {
        SubAnswer {
            verb: "locate".into(),
            arguments: Map::new(),
            payload: payload.as_object().cloned(),
            text: String::new(),
            is_error: false,
            seconds: 0.0,
            bytes: 0,
        }
    }

    #[test]
    fn a_name_lookup_is_read_from_its_plain_fields() {
        let found = answer(json!({
            "_kin": {"verdict": "complete", "meaning": "m"},
            "results": [{"entity_id": "a1", "module": "flask.app", "name": "Flask.run", "kind": "method"}],
            "not_held": [{"module": "flask.cli", "name": "run", "why": "nested in a callable"}],
            "total": 1,
            "next_cursor": "c1"
        }));
        assert_eq!(found.verdict(), Some("complete"));
        assert_eq!(found.declarations()[0].handle(), "flask.app :: Flask.run");
        assert_eq!(found.declarations()[0].own_name(), "run");
        assert_eq!(
            found.not_held()[0].why.as_deref(),
            Some("nested in a callable")
        );
        assert_eq!(found.total(), Some(1));
        assert_eq!(found.next_cursor(), Some("c1"));
        assert!(found.without_continuation().next_cursor().is_none());
    }

    #[test]
    fn standing_orders_the_plain_words_and_reads_anything_else_as_not_established() {
        assert!(Standing::of(Some("complete")) < Standing::of(Some(COMPLETE_EXCEPT)));
        assert!(Standing::of(Some(COMPLETE_EXCEPT_INDIRECT)) < Standing::of(Some("partial")));
        assert_eq!(Standing::of(None), Standing::NotEstablished);
        assert_eq!(last_segment("store::Store::get"), "get");
        assert_eq!(
            last_segment("f.<locals>.obj.commitTextUpdate"),
            "commitTextUpdate"
        );
    }
}
