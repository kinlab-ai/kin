// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The machine reply each verb answers with, as a typed shell, and the
//! `outputSchema` served from it.
//!
//! A reply is built by its handler, and each one carries fields no other does,
//! so a shell here names only what a caller can rely on: the `_kin` envelope,
//! the continuation handle, and each verb's primary collections and counts,
//! with the type every producer gives them. Every other field is the reply's
//! own and passes through, since no shell sets `additionalProperties: false`.
//! Every field but `_kin` may be null, because the producers write `null` for
//! an empty continuation or an absent reading.
//!
//! The schemas are served only where they can be kept, see
//! [`crate::server::serves_structured_output`]: to a client that negotiated a
//! protocol version with structured tool output, on a connection whose text
//! content is this same machine JSON. A client that shows its model only the
//! structured content (measured in Claude Code, Codex and VS Code) would
//! otherwise read JSON where an agent profile serves plain words.

use serde_json::{Map, Value};

use crate::request_schema::{Profile, TypedRequest};
use crate::verbs::{
    CONTEXT, EXEC, HISTORY, IMPACT, INIT, LOCATE, MUTATE, NEIGHBORS, REFS, SESSION, SOURCE, STATUS,
    TRACE, TRANSACTION,
};

/// Declare a reply shell: the fields every reply carries, and the verb's own.
macro_rules! reply {
    ($(#[$meta:meta])* $name:ident for $tool:path, { $($field:ident : $ty:ty),* $(,)? }) => {
        crate::typed_request! {
            $(#[$meta])*
            pub struct $name for $tool, example serde_json::json!({}), rules vec![], {
                _kin: Map<String, Value> { doc("Freshness, coverage and the one verdict.") },
                next_cursor: String { doc("Send alone as cursor for the next page."), nullable() },
                negative: Map<String, Value> { nullable() },
                edge_coverage: Map<String, Value> { nullable() },
                $( $field: $ty { nullable() }, )*
            }
        }
    };
}

reply!(LocateReply for LOCATE, {
    results: Vec<Value>,
    hits: Vec<Value>,
    total_matches: u64,
    total_matching: u64,
    total_ranked: u64,
});

reply!(RefsReply for REFS, {
    references: Vec<Value>,
    candidates: Vec<Value>,
    total_upstream: u64,
});

reply!(TraceReply for TRACE, {
    chain: Vec<Value>,
    routes: Vec<Value>,
    total_steps: u64,
    routes_total: u64,
    found: bool,
});

reply!(ContextReply for CONTEXT, {});

reply!(SourceReply for SOURCE, {});

reply!(NeighborsReply for NEIGHBORS, {
    entities: Vec<Value>,
    relations: Vec<Value>,
    entity_count: u64,
    relation_count: u64,
});

reply!(ImpactReply for IMPACT, {
    changed_ids: Vec<Value>,
    affected_callers: Vec<Value>,
    affected_overrides: Vec<Value>,
    affected_dependents: Vec<Value>,
    affected_tests: Vec<Value>,
    entity_impacts: Vec<Value>,
});

reply!(StatusReply for STATUS, {});

reply!(HistoryReply for HISTORY, {
    changes: Vec<Value>,
    change_count: u64,
    offset: u64,
    next_offset: u64,
    returned: u64,
});

reply!(InitReply for INIT, {});

reply!(MutateReply for MUTATE, {});

reply!(SessionReply for SESSION, {});

reply!(ExecReply for EXEC, {});

reply!(TransactionReply for TRANSACTION, {});

/// The `outputSchema` served for `tool`, or `None` for a tool with no typed
/// reply.
pub fn output_schema(tool: &str) -> Option<Value> {
    let none = |_: &str| None;
    Some(match tool {
        LOCATE => LocateReply::schema(Profile::Full, &none),
        REFS => RefsReply::schema(Profile::Full, &none),
        TRACE => TraceReply::schema(Profile::Full, &none),
        CONTEXT => ContextReply::schema(Profile::Full, &none),
        SOURCE => SourceReply::schema(Profile::Full, &none),
        NEIGHBORS => NeighborsReply::schema(Profile::Full, &none),
        IMPACT => ImpactReply::schema(Profile::Full, &none),
        STATUS => StatusReply::schema(Profile::Full, &none),
        HISTORY => HistoryReply::schema(Profile::Full, &none),
        INIT => InitReply::schema(Profile::Full, &none),
        MUTATE => MutateReply::schema(Profile::Full, &none),
        SESSION => SessionReply::schema(Profile::Full, &none),
        EXEC => ExecReply::schema(Profile::Full, &none),
        TRANSACTION => TransactionReply::schema(Profile::Full, &none),
        _ => return None,
    })
}

/// Decode one machine reply through `tool`'s typed shell, as the server does
/// before serving it as structured content. An error names the field whose
/// type the shell does not give it.
pub fn decode(tool: &str, reply: &Value) -> Result<(), String> {
    fn through<R: TypedRequest>(reply: &Value) -> Result<(), String> {
        let arguments = serde_json::from_value(reply.clone())
            .map_err(|_| "a reply is a JSON object".to_string())?;
        crate::request_schema::check_fields::<R>(&arguments).map(|_| ())
    }
    match tool {
        LOCATE => through::<LocateReply>(reply),
        REFS => through::<RefsReply>(reply),
        TRACE => through::<TraceReply>(reply),
        CONTEXT => through::<ContextReply>(reply),
        SOURCE => through::<SourceReply>(reply),
        NEIGHBORS => through::<NeighborsReply>(reply),
        IMPACT => through::<ImpactReply>(reply),
        STATUS => through::<StatusReply>(reply),
        HISTORY => through::<HistoryReply>(reply),
        INIT => through::<InitReply>(reply),
        MUTATE => through::<MutateReply>(reply),
        SESSION => through::<SessionReply>(reply),
        EXEC => through::<ExecReply>(reply),
        TRANSACTION => through::<TransactionReply>(reply),
        _ => Ok(()),
    }
}
