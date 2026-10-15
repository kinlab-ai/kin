// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! What one Kin release says to a daemon of another release when it replaces
//! it.
//!
//! One Kin serves a repository at a time. A client of a newer release does not
//! work beside an older daemon: it asks that daemon to make way, waits for it
//! to go, and starts its own. This module is the wire contract both sides read:
//! the `/retire` body that carries a replacement, the stable codes a daemon
//! names what it is waiting on by, which of those a replacement waits for, and
//! the sentence a client of the replaced release is given.

use super::ProcessIdentity;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// The release that replaces a daemon, as a person reads it, for example
/// `Kin 0.8.7 (0123abc)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplacementClaim {
    pub successor: String,
}

/// The body of `POST /retire`.
///
/// A bare process incarnation is the request every daemon with `/retire` takes.
/// The envelope adds the replacement. A daemon that predates the envelope
/// cannot read it as an incarnation and answers 422, and the client then asks
/// the plain way.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RetirementRequest {
    Replacement {
        identity: ProcessIdentity,
        replacement: ReplacementClaim,
    },
    Plain(ProcessIdentity),
}

impl RetirementRequest {
    /// The incarnation the caller attributed, and the replacement when there
    /// is one.
    pub fn into_parts(self) -> (ProcessIdentity, Option<ReplacementClaim>) {
        match self {
            Self::Replacement {
                identity,
                replacement,
            } => (identity, Some(replacement)),
            Self::Plain(identity) => (identity, None),
        }
    }
}

/// The answer to `POST /retire`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetirementReply {
    #[serde(default)]
    pub retiring: bool,
    /// What the daemon is still waiting on, as sentences. Every daemon with
    /// `/retire` sends these.
    #[serde(default)]
    pub blocked_by: Vec<String>,
    /// The same, as codes from [`blocker`]. Absent from a daemon that predates
    /// replacement, whose sentences [`legacy_blocker_code`] reads instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blockers: Option<Vec<String>>,
    /// Whether the daemon took the request as a replacement.
    #[serde(default)]
    pub replacing: bool,
    /// The release it is making way for, when it is replacing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub successor: Option<String>,
}

/// The stable codes a daemon names what keeps it running by.
pub mod blocker {
    pub const PREPARED_PUBLICATION: &str = "prepared_publication";
    pub const REQUEST_IN_FLIGHT: &str = "request_in_flight";
    pub const FIRST_SCAN: &str = "first_scan";
    pub const RECONCILIATION: &str = "reconciliation";
    pub const EMBEDDING: &str = "embedding";
    pub const EMBEDDING_HELD_FOR_MEMORY: &str = "embedding_held_for_memory";
    pub const ENRICHMENT: &str = "enrichment";
    pub const EVENT_SUBSCRIBER: &str = "event_subscriber";
    pub const ATTACHED_SESSION: &str = "attached_session";
    pub const SWEEP_WAITER: &str = "sweep_waiter";
}

/// How a replacement treats one thing a daemon is waiting on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplacementWait {
    /// Work an interruption would lose rather than delay: a write or a request
    /// in flight, a publication being prepared, and the first scan, which does
    /// not resume. Always waited for.
    Always,
    /// A background pass a successor redoes: reconciliation, embedding and
    /// language-server enrichment. Waited for up to
    /// [`REPLACEMENT_BACKGROUND_GRACE`] from the replacement request.
    Bounded,
    /// A client of the release being replaced. It is told to restart and never
    /// holds a replacement up.
    Never,
}

/// How a replacement treats the blocker named `code`. A code this build does
/// not know is something it cannot judge, so it is always waited for.
pub fn replacement_wait(code: &str) -> ReplacementWait {
    match code {
        blocker::RECONCILIATION
        | blocker::EMBEDDING
        | blocker::EMBEDDING_HELD_FOR_MEMORY
        | blocker::ENRICHMENT => ReplacementWait::Bounded,
        blocker::EVENT_SUBSCRIBER | blocker::ATTACHED_SESSION | blocker::SWEEP_WAITER => {
            ReplacementWait::Never
        }
        _ => ReplacementWait::Always,
    }
}

/// How long a replacement waits for background passes before it proceeds.
///
/// Short on purpose. The successor runs every one of these passes again from
/// what is on disk, and a replacement held for a whole embedding backlog is
/// two releases serving one repository for that long.
pub const REPLACEMENT_BACKGROUND_GRACE: Duration = Duration::from_secs(15);

/// The code a replacing daemon answers new requests with, under HTTP 503.
pub const DAEMON_REPLACING: &str = "daemon_replacing";

/// What a client of the replaced release is told.
pub fn replaced_message(successor: &str) -> String {
    format!("Kin was replaced by {successor}; restart this client")
}

/// The body of a replacing daemon's 503.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplacingRefusal {
    pub error: String,
    pub code: String,
    pub successor: String,
}

impl ReplacingRefusal {
    pub fn new(successor: &str) -> Self {
        Self {
            error: replaced_message(successor),
            code: DAEMON_REPLACING.to_string(),
            successor: successor.to_string(),
        }
    }
}

/// Longest successor name a daemon keeps, in characters.
pub const SUCCESSOR_LABEL_MAX_CHARS: usize = 120;

/// A successor name fit to repeat to every client: control characters
/// dropped, bounded in length, and never empty.
pub fn bounded_successor_label(raw: &str) -> String {
    let label: String = raw
        .chars()
        .filter(|character| !character.is_control())
        .take(SUCCESSOR_LABEL_MAX_CHARS)
        .collect();
    let label = label.trim();
    if label.is_empty() {
        "a newer Kin".to_string()
    } else {
        label.to_string()
    }
}

/// The code for a sentence a daemon from 0.8.0 to 0.8.6 sent in `blocked_by`,
/// before daemons sent codes, or `None` for a sentence this build does not
/// know, which a replacement treats as something to wait for.
///
/// Frozen copies of what those releases said, byte for byte, and never the
/// current daemon's own sentences: those may change, and these must keep
/// reading the releases already installed.
pub fn legacy_blocker_code(sentence: &str) -> Option<&'static str> {
    Some(match sentence {
        "a publication is being prepared" => blocker::PREPARED_PUBLICATION,
        "a request is in flight" => blocker::REQUEST_IN_FLIGHT,
        "the first scan of the repository is still running" => blocker::FIRST_SCAN,
        "reconciliation is running" => blocker::RECONCILIATION,
        "embedding is running or queued" => blocker::EMBEDDING,
        "embedding is owed and waiting for memory to free, for a bounded window" => {
            blocker::EMBEDDING_HELD_FOR_MEMORY
        }
        "language-server enrichment is running" => blocker::ENRICHMENT,
        "a client is subscribed to its events" => blocker::EVENT_SUBSCRIBER,
        "a client session is attached" => blocker::ATTACHED_SESSION,
        "a command is waiting to read a language-server sweep's result" => blocker::SWEEP_WAITER,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_incarnation_and_the_envelope_both_read_and_the_envelope_is_no_incarnation() {
        let identity = super::super::current_process_identity().unwrap();
        let plain: RetirementRequest =
            serde_json::from_value(serde_json::to_value(&identity).unwrap()).unwrap();
        assert_eq!(plain.into_parts(), (identity.clone(), None));

        let envelope = serde_json::to_value(RetirementRequest::Replacement {
            identity: identity.clone(),
            replacement: ReplacementClaim {
                successor: "Kin 9.9.9 (0123abc)".to_string(),
            },
        })
        .unwrap();
        let read: RetirementRequest = serde_json::from_value(envelope.clone()).unwrap();
        assert_eq!(
            read.into_parts(),
            (
                identity,
                Some(ReplacementClaim {
                    successor: "Kin 9.9.9 (0123abc)".to_string()
                })
            )
        );
        // A daemon that predates the envelope reads its body as a bare
        // incarnation. It must refuse it rather than read a default one.
        assert!(serde_json::from_value::<ProcessIdentity>(envelope).is_err());
    }

    #[test]
    fn writes_and_the_first_scan_hold_a_replacement_and_clients_never_do() {
        for code in [
            blocker::PREPARED_PUBLICATION,
            blocker::REQUEST_IN_FLIGHT,
            blocker::FIRST_SCAN,
            "a_code_from_a_later_release",
        ] {
            assert_eq!(replacement_wait(code), ReplacementWait::Always, "{code}");
        }
        for code in [
            blocker::RECONCILIATION,
            blocker::EMBEDDING,
            blocker::EMBEDDING_HELD_FOR_MEMORY,
            blocker::ENRICHMENT,
        ] {
            assert_eq!(replacement_wait(code), ReplacementWait::Bounded, "{code}");
        }
        for code in [
            blocker::EVENT_SUBSCRIBER,
            blocker::ATTACHED_SESSION,
            blocker::SWEEP_WAITER,
        ] {
            assert_eq!(replacement_wait(code), ReplacementWait::Never, "{code}");
        }
    }

    #[test]
    fn a_legacy_sentence_reads_as_its_code_and_an_unknown_one_reads_as_none() {
        assert_eq!(
            legacy_blocker_code("a client session is attached"),
            Some(blocker::ATTACHED_SESSION)
        );
        assert_eq!(
            legacy_blocker_code("a command is waiting to read a language-server sweep's result"),
            Some(blocker::SWEEP_WAITER)
        );
        assert_eq!(legacy_blocker_code("a client session is attached."), None);
        assert_eq!(legacy_blocker_code(""), None);
    }

    #[test]
    fn a_successor_label_is_bounded_and_never_empty() {
        assert_eq!(bounded_successor_label("Kin 0.8.7\n\u{7}"), "Kin 0.8.7");
        assert_eq!(bounded_successor_label("  \r\n"), "a newer Kin");
        assert_eq!(
            bounded_successor_label(&"x".repeat(1000)).chars().count(),
            SUCCESSOR_LABEL_MAX_CHARS
        );
        assert_eq!(
            ReplacingRefusal::new("Kin 0.8.7").error,
            "Kin was replaced by Kin 0.8.7; restart this client"
        );
    }
}
