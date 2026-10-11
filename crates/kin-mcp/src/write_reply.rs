// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! A write's reply in plain words.
//!
//! A read answers in a sentence and rows. A write answered with a receipt:
//! a transaction id, a state, two delta counts, a root hash, an accounting
//! block and the paths of the files written, with no line that says which
//! entity changed. A refusal answered with an error code, a JSON conflict
//! record, or a planner's fragment, and the reader was left to work out the
//! next call. Measured in a fix run, a small model sent the same refused
//! write dozens of times.
//!
//! So a committed write says what it did, entity by entity, how many source
//! units it wrote and that the graph holds the result, naming each entity by
//! the address every read tool takes. A refusal says in one or two sentences
//! that nothing was written, why, and the call that fixes it. The machine
//! receipt and refusal are unchanged on the profiles that serve them.
//!
//! The sentence counts source units and never names one: a reply on the
//! semantic surface holds no storage coordinate, and the receipt's list of
//! written paths is withheld before this module reads it.

use serde_json::{json, Map, Value};

/// Where a commit receipt lists what its operations did to which entity:
/// rows of `entity_id`, `name`, `kind`, `module` when the graph places it,
/// and `change`, one of [`CHANGED`], [`CREATED`] and [`REMOVED`].
pub const ENTITY_CHANGES_KEY: &str = "entity_changes";

/// An entity whose body an operation replaced or patched.
pub const CHANGED: &str = "changed";
/// A declaration an operation created.
pub const CREATED: &str = "created";
/// A declaration an operation removed.
pub const REMOVED: &str = "removed";

/// Where a commit receipt counts the source units it wrote.
pub const UNITS_WRITTEN_KEY: &str = "source_units_written";

/// Set on a commit receipt when an operation carried a repository base, so
/// the caller's next one needs the base this commit left behind.
pub const REPOSITORY_BASE_SENT_KEY: &str = "repository_base_sent";

/// The three kinds of change, in the order a reply says them.
const CHANGE_KINDS: [&str; 3] = [CHANGED, CREATED, REMOVED];

/// Entities one sentence names for one kind of change before it counts the
/// rest. A kind with more is also listed, so every entity is named once.
const NAMED_MAX: usize = 3;

/// The tools whose reply is a write's: the one-call write, by its registered
/// name or its verb, and the commit of a staged transaction.
pub(crate) fn is_write_tool(tool: &str) -> bool {
    matches!(tool, "kin_mutate" | "kin_transaction_commit")
        || tool == crate::verbs::MUTATE
        || tool == crate::handlers::sessions::DURABLE_MUTATE_TOOL
}

fn label(row: &Value) -> String {
    if let Some(label) = crate::module_code::change_label(row) {
        return label;
    }
    let name = row
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("an entity");
    let label = match row.get("module").and_then(Value::as_str) {
        Some(module) => format!("{module} :: {name}"),
        None => name.to_string(),
    };
    // A name its source unit declares more than once is written with the
    // id that says which of them the write touched.
    let shared = row.get(crate::semantic_owners::NAME_SHARED_KEY) == Some(&Value::Bool(true));
    match row.get("entity_id").and_then(Value::as_str) {
        Some(id) if shared => {
            let short: String = id.chars().filter(|c| *c != '-').take(8).collect();
            format!("{label} #{short}")
        }
        _ => label,
    }
}

fn listed(labels: &[String]) -> String {
    let shown: Vec<&str> = labels.iter().take(NAMED_MAX).map(String::as_str).collect();
    let more = labels.len().saturating_sub(shown.len());
    let mut parts: Vec<String> = shown.iter().map(|label| label.to_string()).collect();
    if more > 0 {
        parts.push(format!("{more} more"));
    }
    match parts.as_slice() {
        [] => String::new(),
        [one] => one.clone(),
        [first, second] => format!("{first} and {second}"),
        [head @ .., last] => format!("{} and {last}", head.join(", ")),
    }
}

fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// A committed write, plainly: one sentence that says what changed, how many
/// source units were written and that the graph holds it, each entity
/// addressed the way a read addresses it. A kind of change with more
/// entities than the sentence names lists them all. `None` for anything that
/// is not a commit receipt, which is left to its own shape.
pub(crate) fn plain_receipt(machine: &Map<String, Value>) -> Option<Value> {
    if machine.get("state").and_then(Value::as_str) != Some("committed") {
        return None;
    }
    let rows: Vec<&Value> = machine
        .get(ENTITY_CHANGES_KEY)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .collect();
    let written = machine
        .get(UNITS_WRITTEN_KEY)
        .and_then(Value::as_u64)
        .map(|units| match units {
            0 => "no source unit written; ".to_string(),
            1 => "1 source unit written; ".to_string(),
            units => format!("{units} source units written; "),
        })
        .unwrap_or_default();
    let mut plain = Map::new();
    let mut said: Vec<String> = Vec::new();
    for kind in CHANGE_KINDS {
        let of_kind: Vec<&Value> = rows
            .iter()
            .copied()
            .filter(|row| row.get("change").and_then(Value::as_str) == Some(kind))
            .collect();
        if of_kind.is_empty() {
            continue;
        }
        let labels: Vec<String> = of_kind.iter().map(|row| label(row)).collect();
        said.push(format!("{kind} {}", listed(&labels)));
        if of_kind.len() <= NAMED_MAX {
            continue;
        }
        plain.insert(
            kind.to_string(),
            Value::Array(
                of_kind
                    .iter()
                    .map(|row| {
                        let mut entity = Map::new();
                        for key in [
                            "entity_id",
                            "name",
                            "module",
                            "kind",
                            crate::semantic_owners::NAME_SHARED_KEY,
                        ] {
                            if let Some(value) = row.get(key).filter(|value| !value.is_null()) {
                                entity.insert(key.to_string(), value.clone());
                            }
                        }
                        Value::Object(entity)
                    })
                    .collect(),
            ),
        );
    }
    let holds = if rows.len() == 1 { "it" } else { "the change" };
    let did = if said.is_empty() {
        // A receipt that names no entity: a relationship or a unit's
        // imports, or a runtime that lists none.
        let operations = machine
            .get("ops_applied")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        match operations {
            1 => "Applied 1 operation".to_string(),
            operations => format!("Applied {operations} operations"),
        }
    } else {
        capitalized(&said.join(", "))
    };
    let meaning = if machine.get("already_applied").and_then(Value::as_bool) == Some(true) {
        format!(
            "{did}. This write was already applied by an earlier call, and nothing was written \
             again; the graph holds {holds}."
        )
    } else {
        format!("{did}; {written}the graph now holds {holds}.")
    };
    plain.insert(
        crate::envelope::ENVELOPE_KEY.to_string(),
        json!({ "meaning": meaning }),
    );
    // What a later call takes: the base the next unit-addressed operation
    // carries, for a caller that sent one, and anything the commit warned of.
    if machine
        .get(REPOSITORY_BASE_SENT_KEY)
        .and_then(Value::as_bool)
        == Some(true)
    {
        if let Some(base) = machine
            .get("repository_base")
            .filter(|base| !base.is_null())
        {
            plain.insert("repository_base".to_string(), base.clone());
        }
    }
    for key in ["conflicts", "collision_warnings"] {
        if let Some(value) = machine
            .get(key)
            .filter(|value| value.as_array().is_some_and(|rows| !rows.is_empty()))
        {
            plain.insert(key.to_string(), value.clone());
        }
    }
    Some(Value::Object(plain))
}

const NOTHING_WRITTEN: &str = "Nothing was written.";

/// The line a refusal carries when the commit path discarded a plan before
/// it published anything. The daemon writes it only there, so it is the
/// proof that repository authority did not move.
pub(crate) const PLAN_DISCARDED: &str = "\nrepository authority did not move";

/// What a commit refusal says, after its reason, when the daemon's graph has
/// lost something the repository holds. Nothing a running daemon does brings
/// it back, so the same commit meets the same refusal however often it is
/// sent, and the refusal says so instead of asking for it again.
pub const RESEND_WILL_NOT_HELP: &str = "Sending this commit again will not help";

/// How that refusal names a failing publication of the daemon's own
/// background work, when there is one, and how the sentence ends.
pub const PUBLICATION_FAILING: &str = "Its background work also could not publish what it derived";
pub const PUBLICATION_FAILING_END: &str = "until that is repaired.";

/// A commit refused because the daemon's graph has lost something the
/// repository holds, plainly: nothing was written, sending it again will not
/// help, and what ends it.
fn lost_authority(text: &str) -> Option<String> {
    let (reason, after) = text.split_once(RESEND_WILL_NOT_HELP)?;
    // The reason names what went missing in parentheses: `entity <id> is
    // missing` or `relation <id> is missing`.
    let missing = reason
        .rsplit_once(" is missing)")
        .and_then(|(before, _)| before.rsplit_once('('))
        .map_or("part of the graph", |(_, what)| what);
    let publication = after
        .find(PUBLICATION_FAILING)
        .and_then(|start| {
            let end = after[start..].find(PUBLICATION_FAILING_END)?;
            Some(&after[start..start + end + PUBLICATION_FAILING_END.len()])
        })
        .map(|said| format!(" {said}"))
        .unwrap_or_default();
    Some(format!(
        "Nothing was written, and sending this change again will not help: the Kin daemon's \
         graph has lost {missing} that the repository holds, and a running daemon does not get \
         it back.{publication} Stop the daemon with `kin daemon stop`; the next call starts one \
         that reads the repository again."
    ))
}

fn sentence(text: &str) -> String {
    let text = text.trim();
    if text.ends_with(['.', '!', '?']) {
        text.to_string()
    } else {
        format!("{text}.")
    }
}

/// The name a `source_base_changed` refusal gives the entity it is about.
fn changed_entity(refusal: &str) -> Option<&str> {
    refusal
        .strip_prefix("source_base_changed:")?
        .trim_start()
        .split(" is not the source")
        .next()
        .filter(|name| !name.is_empty())
}

/// A refused write, plainly: that nothing was written, why, and the call
/// that fixes it. `text` is the refusal as the write path worded it: a code
/// and a sentence, a conflict record, or a planner's reason.
///
/// `None` for anything this does not recognise, which keeps its own words.
/// A failure after publication is one of those, and it must never be told as
/// "nothing was written": each form below is one the write path produces
/// only before repository authority moves.
pub(crate) fn plain_refusal(text: &str, tool: &str) -> Option<String> {
    let text = text.trim();
    if let Ok(Value::Object(record)) = serde_json::from_str::<Value>(text) {
        // A connection hands a refusal on inside its envelope: a record
        // with the envelope added to it, or prose as a `message` beside it.
        return refused_record(&record).or_else(|| {
            record
                .get("message")
                .and_then(Value::as_str)
                .and_then(|message| refused_prose(message.trim(), tool))
        });
    }
    refused_prose(text, tool)
}

/// A refusal the write path worded in prose, plainly.
fn refused_prose(text: &str, tool: &str) -> Option<String> {
    if let Some(name) = changed_entity(text) {
        return Some(format!(
            "That ticket is from before {name} last changed. Read {name} again with source and \
             resend your change with the source_base that read returns. {NOTHING_WRITTEN}"
        ));
    }
    if text.starts_with("source_base_ticket_invalid:") {
        return Some(format!(
            "That source_base ticket was not copied exactly. Send the source_base a source read \
             returned, unchanged, with that entity's id as the target, or read the entity again \
             with source and send what it returns. {NOTHING_WRITTEN}"
        ));
    }
    // A commit refusal: a sentence, the refused operations, the remedy, and
    // the same record as one line of JSON, which says nothing was applied.
    if let Some(record) = text
        .lines()
        .last()
        .and_then(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|record| {
            record["schema"] == crate::session::CommitRefusal::SCHEMA && record["applied"] == false
        })
    {
        let reasons: Vec<&str> = record["operations"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        let remedy = record["remedy"].as_str().unwrap_or_default();
        return Some(
            format!(
                "Nothing was written: {} {}",
                sentence(&reasons.join("; ")),
                sentence(remedy)
            )
            .trim_end()
            .to_string(),
        );
    }
    if let Some(said) = lost_authority(text) {
        return Some(said);
    }
    // A plan the commit path discarded: its reason, then what it did with
    // the staged operations. One call that wrote nothing is sent again, so
    // only a transaction's commit is told what became of its staging.
    if let Some((reason, _)) = text.split_once(PLAN_DISCARDED) {
        let reason = reason.trim().trim_end_matches('.');
        let staged = if tool == "kin_transaction_commit" {
            " Its staged operations were cleared: stage the corrected ones on the same \
             transaction and commit again."
        } else {
            ""
        };
        return Some(format!("Nothing was written: {reason}.{staged}"));
    }
    None
}

/// A conflict record, plainly, or `None` for a record this does not know.
fn refused_record(record: &Map<String, Value>) -> Option<String> {
    let reason = record
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("it no longer matches what was read");
    let kept = match (
        record
            .get("staged_operations_retained")
            .and_then(Value::as_bool),
        record.get("transaction_id").and_then(Value::as_str),
    ) {
        (Some(true), Some(transaction)) => format!(
            " Your operations are still staged in transaction {transaction}: abort it, or leave \
             it and send the change again in a new call."
        ),
        _ => String::new(),
    };
    match record.get("code").and_then(Value::as_str)? {
        "source_base_conflict" => Some(format!(
            "{NOTHING_WRITTEN} An entity is not as it was read: {}. Read it again with source, \
             then send your change again with the source_base that read returns.{kept}",
            reason.trim_end_matches('.')
        )),
        "repository_base_conflict" => {
            let reads = record
                .get("source_reads_required")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            let reread = match reads {
                0 => String::new(),
                1 => " One operation also carries a source_base that must be read again with \
                      source."
                    .to_string(),
                reads => format!(
                    " {reads} operations also carry a source_base that must be read again with \
                     source."
                ),
            };
            let current = record
                .get("current_repository_base")
                .filter(|base| !base.is_null())
                .map(|base| format!(" current_repository_base: {base}"))
                .unwrap_or_default();
            Some(format!(
                "{NOTHING_WRITTEN} The repository changed since that repository_base was read: \
                 {}. Send the operations again with the current repository_base.{reread}{kept}\
                 {current}",
                reason.trim_end_matches('.')
            ))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn receipt(changes: Value, units: usize) -> Map<String, Value> {
        json!({
            "transaction_id": "6d8a4a52-0000-4000-8000-000000000001",
            "state": "committed", "status": "committed", "already_applied": false,
            "ops_applied": 1, "entity_deltas": 3, "relation_deltas": 2, "empty": false,
            "change_id": "c".repeat(64), "repository_generation": 9,
            "new_root_hash": "f".repeat(64),
            UNITS_WRITTEN_KEY: units,
            "collision_warnings": [], "conflicts": [],
            "semantic_authority": "reparsed_exact_repository_bytes", "coordination": null,
            "repository_base": {"schema": "kin.repository.base.v1", "context": {}},
            ENTITY_CHANGES_KEY: changes,
        })
        .as_object()
        .unwrap()
        .clone()
    }

    fn row(name: &str, module: Option<&str>, change: &str) -> Value {
        let mut row = json!({"entity_id": "00000000-0000-4000-8000-000000000001",
            "name": name, "kind": "method", "change": change});
        if let Some(module) = module {
            row["module"] = json!(module);
        }
        row
    }

    #[test]
    fn a_committed_write_says_what_changed_and_names_it_by_address() {
        let plain = plain_receipt(&receipt(
            json!([row("Flask.make_response", Some("flask.app"), CHANGED)]),
            1,
        ))
        .unwrap();
        assert_eq!(
            plain["_kin"]["meaning"],
            "Changed flask.app :: Flask.make_response; 1 source unit written; the graph now \
             holds it."
        );
        // The sentence is the whole reply: the receipt's working is not
        // something a reader acts on, and the entity is named once.
        assert_eq!(
            plain.as_object().unwrap().keys().collect::<Vec<_>>(),
            ["_kin"]
        );

        let several = plain_receipt(&receipt(
            json!([
                row("App.first", Some("pkg.app"), CHANGED),
                row("App.second", Some("pkg.app"), CHANGED),
                row("App.added", Some("pkg.app"), CREATED),
                row("old", None, REMOVED),
            ]),
            2,
        ))
        .unwrap();
        assert_eq!(
            several["_kin"]["meaning"],
            "Changed pkg.app :: App.first and pkg.app :: App.second, created pkg.app :: \
             App.added, removed old; 2 source units written; the graph now holds the change."
        );
        assert!(several.get(CHANGED).is_none() && several.get(CREATED).is_none());

        // More than the sentence names: counted there, and each one listed.
        let many: Vec<Value> = (0..5)
            .map(|index| row(&format!("f{index}"), Some("m"), CHANGED))
            .collect();
        let plain = plain_receipt(&receipt(json!(many), 1)).unwrap();
        assert_eq!(
            plain["_kin"]["meaning"],
            "Changed m :: f0, m :: f1, m :: f2 and 2 more; 1 source unit written; the graph now \
             holds the change."
        );
        assert_eq!(plain[CHANGED].as_array().unwrap().len(), 5);
        assert_eq!(plain[CHANGED][4]["name"], "f4");
        assert!(plain[CHANGED][0].get("change").is_none());
    }

    #[test]
    fn a_receipt_that_names_no_entity_or_was_already_applied_still_reads_plainly() {
        let mut unnamed = receipt(json!([]), 0);
        unnamed.insert("ops_applied".into(), json!(2));
        assert_eq!(
            plain_receipt(&unnamed).unwrap()["_kin"]["meaning"],
            "Applied 2 operations; no source unit written; the graph now holds the change."
        );
        // A receipt from a runtime that counts no unit says nothing of them.
        unnamed.remove(UNITS_WRITTEN_KEY);
        assert_eq!(
            plain_receipt(&unnamed).unwrap()["_kin"]["meaning"],
            "Applied 2 operations; the graph now holds the change."
        );
        let mut replay = receipt(json!([row("f", Some("m"), CHANGED)]), 1);
        replay.insert("already_applied".into(), json!(true));
        assert_eq!(
            plain_receipt(&replay).unwrap()["_kin"]["meaning"],
            "Changed m :: f. This write was already applied by an earlier call, and nothing \
             was written again; the graph holds it."
        );
        // The base the commit left behind goes to a caller that sent one.
        let mut based = receipt(json!([row("f", Some("m"), CREATED)]), 1);
        assert!(plain_receipt(&based)
            .unwrap()
            .get("repository_base")
            .is_none());
        based.insert(REPOSITORY_BASE_SENT_KEY.into(), json!(true));
        assert_eq!(
            plain_receipt(&based).unwrap()["repository_base"]["schema"],
            "kin.repository.base.v1"
        );
        // Anything that is not a commit receipt keeps its own shape.
        let staged = json!({"state": "active", "staged_operations": 1});
        assert!(plain_receipt(staged.as_object().unwrap()).is_none());
    }

    /// A commit refused because the daemon's graph lost something the
    /// repository holds is told in two sentences: nothing was written and
    /// sending it again will not help, then what ends it. A failing
    /// publication is named between them when the refusal names one.
    #[test]
    fn a_refusal_that_repeating_cannot_clear_says_so_and_what_ends_it() {
        let refusal = |unpublished: &str| {
            format!(
                "Cannot commit transaction 816017e2-8e40-41ee-94a2-05fc607db4ba: daemon graph no \
                 longer holds the repository workspace authority it was planned against \
                 (relation 06e97818-a9c2-52b4-ad21-cd301e631c72 is missing); reopen before \
                 committing an MCP transaction. No repository authority moved and the staged \
                 operations are untouched. {RESEND_WILL_NOT_HELP}: the running daemon does not \
                 get back what its graph lost.{unpublished} Stop the daemon with `kin daemon \
                 stop`; the next call starts one that reads the repository again, and the \
                 commit can be sent then."
            )
        };
        assert_eq!(
            plain_refusal(&refusal(""), "kin_transaction_commit").unwrap(),
            "Nothing was written, and sending this change again will not help: the Kin daemon's \
             graph has lost relation 06e97818-a9c2-52b4-ad21-cd301e631c72 that the repository \
             holds, and a running daemon does not get it back. Stop the daemon with `kin daemon \
             stop`; the next call starts one that reads the repository again."
        );
        let failing = format!(
            " {PUBLICATION_FAILING} (binding withdrawal planning: prior obligation identity \
             collides), which a new daemon will meet again {PUBLICATION_FAILING_END}"
        );
        assert_eq!(
            plain_refusal(&refusal(&failing), "kin_mutate").unwrap(),
            "Nothing was written, and sending this change again will not help: the Kin daemon's \
             graph has lost relation 06e97818-a9c2-52b4-ad21-cd301e631c72 that the repository \
             holds, and a running daemon does not get it back. Its background work also could \
             not publish what it derived (binding withdrawal planning: prior obligation identity \
             collides), which a new daemon will meet again until that is repaired. Stop the \
             daemon with `kin daemon stop`; the next call starts one that reads the repository \
             again."
        );
        // A refusal the daemon can clear by itself keeps its own words.
        assert_eq!(
            plain_refusal(
                "Cannot commit transaction 816017e2: daemon repository cursor is at generation \
                 3, but the authority for committing an MCP transaction is at generation 4; \
                 reopen from repository authority before mutating. No repository authority \
                 moved and the staged operations are untouched, so re-send this commit \
                 unchanged once the daemon is reading current repository authority.",
                "kin_mutate"
            ),
            None
        );
    }

    #[test]
    fn a_refusal_says_nothing_was_written_and_what_to_do_next() {
        let mutate = "kin_mutate";
        assert_eq!(
            plain_refusal(
                "source_base_changed: App.first is not the source this ticket was read from. \
                 Its body changed since that read, so an edit written against it could \
                 overwrite work it never saw; read App.first again with get_entity_source and \
                 send the source_base it returns. Nothing was written.",
                mutate
            )
            .unwrap(),
            "That ticket is from before App.first last changed. Read App.first again with \
             source and resend your change with the source_base that read returns. Nothing was \
             written."
        );
        assert!(plain_refusal(
            "source_base_ticket_invalid: this source_base ticket was not copied exactly.",
            mutate
        )
        .unwrap()
        .starts_with("That source_base ticket was not copied exactly."));
        // A plan the commit path discarded is its reason in one sentence. A
        // name that opens the reason is written as the graph spells it.
        let discarded = |reason: &str| {
            format!(
                "{reason}\nrepository authority did not move, and the 1 failed operation(s) \
                 have been cleared from transaction 6d8a4a52-0000-4000-8000-000000000001: stage \
                 corrected operations on it and commit again, or begin a new transaction with \
                 kin_transaction_begin\ndropped: create 7"
            )
        };
        assert_eq!(
            plain_refusal(
                &discarded("helper already exists in the anchor's source unit"),
                mutate
            )
            .unwrap(),
            "Nothing was written: helper already exists in the anchor's source unit."
        );
        assert_eq!(
            plain_refusal(
                &discarded("a method is created beside a method of its class"),
                "kin_transaction_commit"
            )
            .unwrap(),
            "Nothing was written: a method is created beside a method of its class. Its staged \
             operations were cleared: stage the corrected ones on the same transaction and \
             commit again."
        );
        // The commit-time conflict record.
        let conflict = crate::source_base::source_base_conflict(
            "6d8a4a52-0000-4000-8000-000000000001",
            "the exact body for entity 7 differs from the caller's source read",
        );
        let said = plain_refusal(&conflict, mutate).unwrap();
        assert!(
            said.starts_with(
                "Nothing was written. An entity is not as it was read: the exact body for \
                 entity 7 differs from the caller's source read. Read it again with source, \
                 then send your change again with the source_base that read returns."
            ),
            "{said}"
        );
        assert!(
            said.contains("still staged in transaction 6d8a4a52"),
            "{said}"
        );
        assert!(!said.contains("schema"), "{said}");
        // A commit refusal drops its JSON echo and keeps its reasons and remedy.
        let refusal = crate::session::CommitRefusal::new(
            crate::session::CommitRefusalCode::NotCommittable,
            "6d8a4a52-0000-4000-8000-000000000001",
            vec!["operation 0 names a file".to_string()],
        );
        let said = plain_refusal(&refusal.render(), mutate).unwrap();
        assert!(
            said.starts_with("Nothing was written: operation 0 names a file. Fix or remove"),
            "{said}"
        );
        assert!(!said.contains("kin.mcp.commit_refusal"), "{said}");
    }

    /// A failure this module cannot place before publication keeps its own
    /// words: it may have been written, and saying otherwise would be false.
    #[test]
    fn a_failure_that_may_follow_publication_is_never_told_as_nothing_written() {
        for failure in [
            "derived daemon graph does not match repository authority after transaction 7",
            "exact MCP repository commit returned an error; receipt recovery also failed: gone",
            "repository authority advanced beyond MCP receipt generation 9; reopen the daemon \
             before finalizing transaction 7",
            "source_base_required: send the base.",
        ] {
            assert_eq!(plain_refusal(failure, "kin_mutate"), None, "{failure}");
        }
        let recovery = json!({"schema": "kin.mutate.recovery.v1", "published": true,
            "code": "publication_completed_daemon_recovery_required"})
        .to_string();
        assert_eq!(plain_refusal(&recovery, "kin_mutate"), None);
    }

    /// A write's reply as an agent on the default profile reads it: plain
    /// words, then compact text.
    fn agent_text(tool: &str, reply: &str, refused: bool) -> String {
        use crate::server::{present_for_client, McpServerConfig};
        use crate::types::JsonRpcResponse;
        let agent = McpServerConfig {
            plain_replies: true,
            agent_belt: true,
            ..Default::default()
        };
        let request = json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":tool,"arguments":{"operations":[]}}})
        .to_string();
        let mut result = json!({"content":[{"type":"text","text":reply}]});
        if refused {
            result["isError"] = json!(true);
        }
        let response = JsonRpcResponse::success(Some(json!(1)), result);
        let mut result = present_for_client(response, &request, &agent)
            .result
            .unwrap();
        let handles = crate::session_handles::SessionHandles::default();
        crate::text_reply::render_result(&mut result, "kin_mutate", &handles);
        result["content"][0]["text"].as_str().unwrap().to_string()
    }

    #[test]
    fn an_agent_reads_a_write_as_a_sentence_and_the_entities_it_touched() {
        let mut machine = receipt(
            json!([
                {"entity_id": "00000000-0000-4000-8000-000000000011",
                    "name": "Flask.make_response", "kind": "method", "module": "flask.app",
                    "change": CHANGED},
                {"entity_id": "00000000-0000-4000-8000-000000000012",
                    "name": "Flask.added", "kind": "method", "module": "flask.app",
                    "change": CREATED},
            ]),
            1,
        );
        machine.insert(
            "publication_accounting".into(),
            json!({"schema": "kin.publication.v1", "bytes": 4096}),
        );
        let machine_text = serde_json::to_string_pretty(&Value::Object(machine.clone())).unwrap();
        let text = agent_text("mutate", &machine_text, false);
        // What the same commit read as before: every field of its receipt
        // and no entity, the way any reply with no wording of its own reads.
        let mut fields = machine;
        for key in [
            ENTITY_CHANGES_KEY,
            UNITS_WRITTEN_KEY,
            REPOSITORY_BASE_SENT_KEY,
        ] {
            fields.remove(key);
        }
        let before = agent_text(
            "kin_transaction_begin",
            &Value::Object(fields).to_string(),
            false,
        );
        println!(
            "{before}\n-- before, {} bytes\n{text}\n-- after, {} bytes",
            before.len(),
            text.len()
        );
        assert!(before.starts_with("this is the whole answer."), "{before}");
        assert!(!before.contains("Flask"), "{before}");
        assert!(text.len() * 2 < before.len(), "{before}");
        assert_eq!(
            text,
            "Changed flask.app :: Flask.make_response, created flask.app :: Flask.added; 1 \
             source unit written; the graph now holds the change."
        );

        // A refusal is the sentence, and nothing of its record.
        let refused = agent_text(
            "mutate",
            "helper already exists in the anchor's source unit\nrepository authority did not \
             move, and the 1 failed operation(s) have been cleared from transaction \
             6d8a4a52-0000-4000-8000-000000000001: stage corrected operations on it and commit \
             again, or begin a new transaction with kin_transaction_begin\ndropped: create 7",
            true,
        );
        println!("{refused}");
        assert_eq!(
            refused,
            "Nothing was written: helper already exists in the anchor's source unit."
        );
        let stale = agent_text(
            "mutate",
            "source_base_changed: App.first is not the source this ticket was read from. Its \
             body changed since that read, so an edit written against it could overwrite work \
             it never saw; read App.first again with get_entity_source and send the \
             source_base it returns. Nothing was written.",
            true,
        );
        assert_eq!(
            stale,
            "That ticket is from before App.first last changed. Read App.first again with \
             source and resend your change with the source_base that read returns. Nothing was \
             written."
        );
        // A failure that may follow publication keeps its own words.
        let after = "derived daemon graph does not match repository authority after \
                     transaction 7";
        assert_eq!(agent_text("mutate", after, true), after);

        // A plan discarded for a reason that names storage is still a write
        // that changed nothing, and is said as one, never as a diagnostic
        // that wants an operator.
        let physical = agent_text(
            "mutate",
            "derive exact semantics for src/flask/app.py: index error\nrepository authority did \
             not move, and the 1 failed operation(s) have been cleared from transaction \
             6d8a4a52-0000-4000-8000-000000000001: stage corrected operations on it and commit \
             again, or begin a new transaction with kin_transaction_begin\ndropped: update 7",
            true,
        );
        assert_eq!(
            physical,
            "Nothing was written: Kin refused this change while planning it, for a reason that \
             names where code is stored and is kept in the daemon's log; sending it again \
             unchanged will be refused again, so report this refusal."
        );
    }

    /// A write to one of two declarations under one name says which, by id.
    #[test]
    fn a_write_to_a_name_declared_twice_names_the_declaration_by_its_id() {
        let machine = receipt(
            json!([
                {"entity_id": "0e1d0e9e-ca73-5384-a5c0-152483859f52", "name": "Gauge.level",
                    "kind": "method", "module": "pkg.gauge", "change": CHANGED,
                    "name_shared": true},
                {"entity_id": "00000000-0000-4000-8000-000000000012", "name": "Gauge.reset",
                    "kind": "method", "module": "pkg.gauge", "change": CHANGED},
            ]),
            1,
        );
        let plain = plain_receipt(&machine).unwrap();
        assert_eq!(
            plain[crate::envelope::ENVELOPE_KEY]["meaning"],
            "Changed pkg.gauge :: Gauge.level #0e1d0e9e and pkg.gauge :: Gauge.reset; 1 source \
             unit written; the graph now holds the change."
        );
    }

    /// A refusal as a connection hands it on: finalized into its envelope
    /// the way every tool result is before it is presented.
    fn enveloped(refusal: &str) -> String {
        let finalized = crate::envelope::finalize(
            crate::types::ToolCallResult::error(refusal),
            crate::envelope::Envelope::daemon(),
            "kin_mutate",
        );
        match finalized.content.first() {
            Some(crate::types::ContentBlock::Text { text }) => text.clone(),
            None => panic!("a finalized refusal keeps its text"),
        }
    }

    /// The refusal reaches the presentation inside the connection's envelope,
    /// prose as a `message` and a record with `_kin` beside its fields. Read
    /// bare, it printed as `message:` rows over the commit path's bookkeeping.
    #[test]
    fn a_refusal_inside_its_envelope_reads_as_the_same_sentence() {
        let discarded = "helper already exists in the anchor's source unit\nrepository authority \
                         did not move, and the 1 failed operation(s) have been cleared from \
                         transaction 6d8a4a52-0000-4000-8000-000000000001: stage corrected \
                         operations on it and commit again, or begin a new transaction with \
                         kin_transaction_begin\ndropped: create 7";
        let wrapped = enveloped(discarded);
        assert!(
            wrapped.contains("\"message\"") && wrapped.contains("\"_kin\""),
            "the fixture must be the enveloped form: {wrapped}"
        );
        assert_eq!(
            agent_text("mutate", &wrapped, true),
            "Nothing was written: helper already exists in the anchor's source unit."
        );
        let stale = enveloped(
            "source_base_changed: App.first is not the source this ticket was read from. Its \
             body changed since that read; read App.first again with get_entity_source and send \
             the source_base it returns. Nothing was written.",
        );
        assert_eq!(
            agent_text("mutate", &stale, true),
            "That ticket is from before App.first last changed. Read App.first again with \
             source and resend your change with the source_base that read returns. Nothing was \
             written."
        );
        let conflict = enveloped(&crate::source_base::source_base_conflict(
            "6d8a4a52-0000-4000-8000-000000000001",
            "the exact body for entity 7 differs from the caller's source read",
        ));
        let said = agent_text("mutate", &conflict, true);
        assert!(
            said.starts_with("Nothing was written. An entity is not as it was read:"),
            "{said}"
        );
        assert!(
            !said.contains("message:") && !said.contains("_kin"),
            "{said}"
        );
        // A failure that may follow publication keeps its own words there too.
        let after = enveloped("derived daemon graph does not match repository authority");
        let said = agent_text("mutate", &after, true);
        assert!(
            said.contains("derived daemon graph does not match") && !said.contains("Nothing was"),
            "{said}"
        );
    }

    /// The whole route a stdio connection runs for an agent on the text
    /// format: the call is dispatched, its result finalized into the
    /// envelope, presented in plain words and written as text. A write
    /// refused on that route reads as one sentence that says what to do.
    #[tokio::test]
    async fn a_refused_write_reads_as_a_sentence_on_the_text_route() {
        use crate::server::{present_for_client, process_message, McpServerConfig};
        let config = McpServerConfig {
            plain_replies: true,
            agent_belt: true,
            reply_format: crate::text_reply::ReplyFormat::Text,
            session_handles: Some(std::sync::Arc::new(
                crate::session_handles::SessionHandles::default(),
            )),
            session_authority_mode: crate::server::SessionAuthorityMode::OfflineFallback,
            ..Default::default()
        };
        let store = kin_db::InMemoryGraph::default();
        let sessions = crate::SessionRegistry::empty_for_test();
        sessions.register("agent-session", "an agent");
        let call = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{
            "name":"mutate","arguments":{"session_id":"agent-session","operations":[{
                "verb":"patch","target":"00000000-0000-4000-8000-000000000011",
                "payload":{"EntitySourcePatch":{
                    "source_base":{"ticket":"sb2.this-was-not-copied-exactly"},
                    "edits":[{"old_text":"return 1","new_text":"return 2"}]}},
                "description":"change what it returns"}]}}})
        .to_string();
        let response = process_message(&call, &store, &config, &sessions)
            .await
            .expect("a tools/call is answered");
        let machine = serde_json::to_string(&response.result).unwrap();
        let response = present_for_client(response, &call, &config);
        let text = response.result.expect("a refusal is a result")["content"][0]["text"]
            .as_str()
            .expect("a text block")
            .to_string();
        println!("{machine}\n-- machine\n{text}\n-- text");
        assert_eq!(
            text,
            "That source_base ticket was not copied exactly. Send the source_base a source read \
             returned, unchanged, with that entity's id as the target, or read the entity again \
             with source and send what it returns. Nothing was written."
        );
    }
}
