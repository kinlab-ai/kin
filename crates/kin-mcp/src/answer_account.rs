// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Shared classification rules for the question one tool answer addresses.
//!
//! The machine verdict under `_kin.verdict` is response-wide: whether every
//! input agreed, and the codes of those that refused. This module interprets
//! which codes bear on an operation, which describe delivery, and which places
//! an answer lists or counts.
//!
//! Grouping retains its existing reason text so callers render the same groups.
//! [`account`] decides, once per answer, how it stands for its question: the
//! plain reply renders that standing as its word and the machine reply carries
//! it as `_kin.verdict.answer`. Reply-specific prose stays with the renderer,
//! which chooses its sentence by the [`Basis`] the account names and never
//! decides a word of its own.

use serde_json::{json, Map, Value};

/// What one tool call does, and so what its answer can be complete about.
///
/// Every tool's completion statement means what that operation did and
/// nothing else. A usage analysis (`refs`, `impact`, `trace`) is complete when
/// every place that could use its entity was checked, and only its answers say
/// what is left to check. A name search is complete when the declaration
/// search finished over its stated scope; a ranking when it delivered its
/// ranked results, which never claim to be every relevant entity; a source read
/// when it delivered the requested code at this revision; a history read when
/// it delivered the requested page of recorded history; a context when it
/// delivered the focal evidence; and a neighbourhood when it explored the hops
/// it was asked for. A verdict code that speaks to another operation's question
/// never reaches an answer to this one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Operation {
    /// Who uses an entity: every place is the question.
    Usage,
    /// A walk from an entity along its calls.
    Trace,
    /// The declarations a name selects.
    NameSearch,
    /// Adapter file-module identities recorded at one exact graph coordinate.
    Coordinate,
    /// Entities ranked against a description.
    Ranked,
    /// One entity's code.
    Source,
    /// One entity's recorded changes.
    History,
    /// The code an entity needs, around it.
    Context,
    /// The entities a few hops from one.
    Neighbors,
    /// A lookup of what Kin has recorded: text it holds, an override census,
    /// a stored receiver chain. Only a graph out of date with the code
    /// qualifies it.
    Lookup,
    /// Anything else, judged by its own verdict.
    Other,
}

impl Operation {
    pub(crate) fn of(tool: &str) -> Self {
        match tool {
            "find_references" | "impact_analysis" | "bulk_check_references" => Self::Usage,
            TRACE_TOOL | crate::handlers::path::TOOL_NAME => Self::Trace,
            "semantic_search" => Self::NameSearch,
            "semantic_locate" => Self::Ranked,
            SOURCE_TOOL | "get_entity_body" | "get_entity_sources" => Self::Source,
            HISTORY_TOOL => Self::History,
            "get_context_pack" => Self::Context,
            "graph_neighborhood" => Self::Neighbors,
            _ => Self::Other,
        }
    }

    /// Whether the answer is about every place an entity is used, which is the
    /// only question calls Kin could not match, values it could not follow and
    /// leftover sites bear on.
    pub(crate) fn analyses_usage(self) -> bool {
        matches!(self, Self::Usage | Self::Trace | Self::Other)
    }

    /// Whether the answer lists what surrounds an entity.
    pub(crate) fn is_listing(self) -> bool {
        matches!(self, Self::Context | Self::Neighbors)
    }

    /// Whether a code speaks to this operation's question. A code about who
    /// uses an entity does not qualify a read of its code, its history, a
    /// search for its name or what surrounds it; a code about the search index
    /// qualifies only a ranking, which says it as scope.
    pub(crate) fn bears_on(self, code: &str) -> bool {
        if self.analyses_usage() {
            return true;
        }
        // The daemon's own degraded signals are scope for every other answer:
        // what it paused is background work, and the answer says so.
        if code == "degraded" {
            return false;
        }
        if is_usage_code(code) || is_link_coverage_code(code) {
            return false;
        }
        if is_ranking_code(code) {
            return false;
        }
        match self {
            // A ranking claims only that these are its ranked results, which
            // a graph out of date with the code is the one thing to qualify.
            Self::Ranked | Self::Lookup => is_freshness_code(code),
            Self::NameSearch | Self::Coordinate | Self::Source | Self::History => {
                !matches!(code, "depth_zero" | "withheld_candidates")
            }
            _ => true,
        }
    }
}

/// Codes about who uses an entity: calls Kin could not match, values it could
/// not follow, leftover sites and the files that reach the entity.
pub(crate) fn is_usage_code(code: &str) -> bool {
    is_call_resolution_code(code)
        || code.starts_with("value_calls")
        || code.starts_with("escape")
        || code.starts_with("unkeyed")
        || code.starts_with("trace_spine")
        || code.starts_with("reference_enrichment")
        || code.starts_with("reference_sites")
        || matches!(
            code,
            "named_leftovers" | "withheld_candidates" | crate::verdict::MATCHED_BY_NAME
        )
}

/// Codes saying the graph is not level with the code it describes, which
/// qualify every answer drawn from it.
pub(crate) fn is_freshness_code(code: &str) -> bool {
    code.starts_with("graph_")
        || code.starts_with("tracked_changes")
        || code.starts_with("file_bytes")
        || code.starts_with("watcher")
        || matches!(code, "file_spans_stale" | "store_semantics_behind")
}

/// Codes about how completely the graph's links cover some code, which a
/// listing of links states as its scope.
pub(crate) fn is_link_coverage_code(code: &str) -> bool {
    code.starts_with("answer_coverage")
        || code.starts_with("edge_coverage")
        || code.starts_with("cross_file_edges")
}

/// Codes about what a ranking could rank on.
pub(crate) fn is_ranking_code(code: &str) -> bool {
    code.starts_with("coverage_")
        || code.starts_with("semantic_")
        || matches!(code, "relevance_floor_unmeasured" | "substrate_unknown")
}

/// The machine verdict a reply carries, read once.
pub(crate) struct MachineVerdict {
    pub(crate) state: Option<String>,
    pub(crate) remedy: Option<String>,
    pub(crate) codes: Vec<String>,
    pub(crate) safe_to_conclude_absent: bool,
    pub(crate) bounded: bool,
    pub(crate) has_more: bool,
    pub(crate) reference_rows_complete: Option<bool>,
    pub(crate) reference_rows_has_more: bool,
    pub(crate) reference_evidence_has_more: bool,
    /// Every answer row and every candidate site of the frozen answer is on
    /// this page or an earlier one, so later evidence pages hold only the
    /// coverage fields behind the verdict.
    pub(crate) reference_sites_delivered: bool,
    /// The daemon's own degraded signals that are switched on, such as
    /// `memory_pressure`. Empty when `degraded` came only from the reply's own
    /// size cuts, which are delivery.
    pub(crate) degraded_signals: Vec<String>,
    /// Why a language server for the answer's language could not start, as
    /// its `reference_enrichment_unusable` clause carries it, when the clause
    /// carries one.
    pub(crate) server_start_failure: Option<String>,
}

/// The reason a `reference_enrichment_unusable` clause carries, in the form
/// [`crate::negative::reference_readiness_gap`] writes it: `<code>: <reason>,
/// so reference enrichment for <language> is unavailable on this host`.
fn server_start_failure(factor: &str) -> Option<String> {
    factor.split("; ").find_map(|clause| {
        let text = clause.strip_prefix("reference_enrichment_unusable: ")?;
        let (reason, _) = text.rsplit_once(", so reference enrichment for ")?;
        let reason = reason.trim();
        (!reason.is_empty()).then(|| reason.to_string())
    })
}

impl MachineVerdict {
    pub(crate) fn read(envelope: &Value) -> Self {
        let verdict = envelope.get("verdict").unwrap_or(&Value::Null);
        let codes = verdict
            .get("limiting_factor")
            .and_then(Value::as_str)
            .map(|factor| {
                factor
                    .split("; ")
                    .map(|clause| {
                        clause
                            .split(|c: char| c == ':' || c.is_whitespace())
                            .next()
                            .unwrap_or_default()
                            .to_string()
                    })
                    .filter(|code| !code.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        let degraded_signals = envelope
            .get("degraded")
            .and_then(Value::as_object)
            .map(|signals| {
                signals
                    .iter()
                    .filter(|(_, on)| on.as_bool() == Some(true))
                    .map(|(signal, _)| signal.clone())
                    .collect()
            })
            .unwrap_or_default();
        Self {
            state: verdict
                .get("state")
                .and_then(Value::as_str)
                .map(str::to_string),
            remedy: verdict
                .get("remedy")
                .and_then(Value::as_str)
                .map(str::to_string),
            codes,
            safe_to_conclude_absent: verdict
                .get("safe_to_conclude_absent")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            bounded: envelope
                .pointer("/response/bounded")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            has_more: envelope
                .pointer("/page/has_more")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            reference_rows_complete: envelope
                .get("page")
                .filter(|page| page.get("kind").and_then(Value::as_str) == Some("references"))
                .and_then(|page| page.get("answer_rows_complete"))
                .and_then(Value::as_bool),
            reference_rows_has_more: envelope
                .pointer("/page/answer_rows_has_more")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            reference_evidence_has_more: envelope
                .pointer("/page/evidence_has_more")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            reference_sites_delivered: {
                let field = |key: &str| {
                    envelope
                        .pointer(&format!("/page/{key}"))
                        .and_then(Value::as_u64)
                };
                matches!(
                    (field("next_record"), field("total_answer_records"), field("total_candidates")),
                    (Some(next), Some(answers), Some(candidates)) if next >= answers + candidates
                )
            },
            degraded_signals,
            server_start_failure: verdict
                .get("limiting_factor")
                .and_then(Value::as_str)
                .and_then(server_start_failure),
        }
    }

    pub(crate) fn certified(&self) -> bool {
        self.state.as_deref() == Some("certified")
    }

    pub(crate) fn certified_except(&self) -> bool {
        self.state.as_deref() == Some(crate::verdict::CERTIFIED_EXCEPT)
    }

    /// Whether `degraded` says only that this reply was cut to fit its size.
    pub(crate) fn degraded_is_delivery(&self) -> bool {
        self.degraded_signals.is_empty()
    }

    /// Whether a code says how much of the answer this reply delivered.
    pub(crate) fn is_delivery(&self, code: &str) -> bool {
        is_delivery_code(code, self.bounded) || (code == "degraded" && self.degraded_is_delivery())
    }
}

/// How an answer stands for its question, once what this reply delivered is
/// told apart from what the answer holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Standing {
    /// Whole and proven.
    Complete,
    /// Whole except sites the reply lists or counts, each with its reason.
    CompleteExcept,
    /// Something that could change the answer is still open.
    Partial,
}

/// Whether a callers reach finished its reading: every row it recorded is on
/// this page, and no bound stopped the closure or the reading of a member's
/// unresolved calls. Only a reach that did not finish is missing rows.
pub(crate) fn reach_finished(reach: &Map<String, Value>) -> bool {
    crate::trace_reach::rows_delivered(reach)
        && reach.get("state").and_then(Value::as_str) != Some("bounded")
        && reach
            .get("bounds")
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty)
}

/// Whether a finished reach lists or counts a place that could still add a
/// caller: a frontier row, a folded observation, a holder of a member as a
/// value that is not closed, or a caller matched by name alone.
pub(crate) fn reach_lists_open_places(reach: &Map<String, Value>) -> bool {
    let count = |key: &str| reach.get(key).and_then(Value::as_u64).unwrap_or(0);
    let holders = reach
        .get("blind_spots")
        .and_then(|blind| blind.get("value_holders"));
    let holder = |key: &str| {
        holders
            .and_then(|holders| holders.get(key))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    count("frontier_total") > 0
        || reach
            .get(crate::trace_reach::FRONTIER_FOLDED_KEY)
            .and_then(|folded| folded.get("count"))
            .and_then(Value::as_u64)
            .unwrap_or(0)
            > 0
        || count(crate::trace_reach::NAME_ONLY_TOTAL_KEY) > 0
        || holder("rows") > 0
        || holder("unlisted") > 0
        || holders
            .and_then(|holders| holders.get("in_members"))
            .and_then(Value::as_array)
            .is_some_and(|members| !members.is_empty())
        || holders
            .and_then(|holders| holders.get("complete"))
            .and_then(Value::as_bool)
            == Some(false)
}

/// Whether a verdict code restates a place a finished reach already lists
/// or counts: a call in scope nobody resolved, a member the language can run
/// without a call that spells it, a use that holds one as a value, a lookup
/// by a built name, or a cut of the display chain. A code that says work is
/// still owed, or that the graph cannot speak for the code, is not one.
pub(crate) fn reach_itemizes(code: &str) -> bool {
    use kin_model::call_site_reading as reading;
    reading::RESIDUE_CODES.contains(&code)
        || reading::LISTED_SITE_CODES.contains(&code)
        || matches!(
            code,
            "method_call_resolution_incomplete" | "retrieval_degraded"
        )
}

/// Whether a code says how much of an answer this reply delivered rather than
/// whether the answer suffices: a cut the reply discloses, with the way to the
/// rest, is delivery, and `delivered` says it.
pub(crate) fn is_delivery_code(code: &str, bounded: bool) -> bool {
    // A page of a paged answer is partial only as delivery: its cursor
    // reaches the rest.
    matches!(
        code,
        "response_bounded"
            | "reference_page_partial"
            | "trace_page_partial"
            | "impact_page_partial"
            | "locate_page_partial"
    ) || (bounded
        && matches!(
            code,
            "answer_truncated" | "counts_are_a_floor" | "retrieval_degraded"
        ))
}

impl MachineVerdict {
    /// The codes still standing against `op`'s answer once delivery, a listed
    /// residue, and every code that speaks to another operation's question are
    /// set aside. A listing sets aside the calls around it, which it says as
    /// scope, and a degraded signal of the daemon's own, which it says too.
    pub(crate) fn open_codes(&self, op: Operation) -> Vec<&str> {
        self.codes
            .iter()
            .map(String::as_str)
            .filter(|code| {
                !crate::verdict::is_residue_code(code)
                    && !self.is_delivery(code)
                    && op.bears_on(code)
                    && !(op.is_listing()
                        && (is_call_resolution_code(code)
                            || is_link_coverage_code(code)
                            || *code == "degraded"))
            })
            .collect()
    }

    /// How the answer stands for `op`'s question.
    pub(crate) fn standing(&self, op: Operation) -> Standing {
        if self.certified() {
            return Standing::Complete;
        }
        let residue = |code: &String| crate::verdict::is_residue_code(code);
        // A residue listed for another operation's question is not this
        // answer's exception.
        let own_residue = self
            .codes
            .iter()
            .any(|code| residue(code) && op.bears_on(code));
        if self.certified_except() && (own_residue || op.analyses_usage()) {
            return Standing::CompleteExcept;
        }
        if self.state.is_none() {
            // No verdict was computed for this answer. A read that delivered
            // its page is whole as a read; anything else says nothing more
            // than it holds.
            return Standing::Complete;
        }
        if !self.open_codes(op).is_empty() {
            return Standing::Partial;
        }
        if own_residue {
            Standing::CompleteExcept
        } else {
            Standing::Complete
        }
    }
}

/// Whether the only codes that stand against the answer, delivery aside, are
/// counted unkeyed lookups, so nothing is left to check.
pub(crate) fn only_unkeyed(verdict: &MachineVerdict) -> bool {
    let codes: Vec<&str> = verdict
        .codes
        .iter()
        .map(String::as_str)
        .filter(|code| !verdict.is_delivery(code))
        .collect();
    !verdict.certified()
        && !codes.is_empty()
        && codes
            .iter()
            .all(|code| *code == kin_model::call_site_reading::UNKEYED_LOOKUPS_COUNTED)
}

/// Whether the only codes that stand against the answer, delivery aside, are
/// gaps it counts beside itself: lookups by a built name, and the folded
/// kinds ([`crate::verdict::FOLDED_CODES`]). Nothing is then left to check,
/// and the counts are said as scope; none of them is an absence.
pub(crate) fn only_counted(verdict: &MachineVerdict) -> bool {
    let codes: Vec<&str> = verdict
        .codes
        .iter()
        .map(String::as_str)
        .filter(|code| !verdict.is_delivery(code))
        .collect();
    !verdict.certified()
        && !codes.is_empty()
        && codes.iter().all(|code| {
            *code == kin_model::call_site_reading::UNKEYED_LOOKUPS_COUNTED
                || crate::verdict::FOLDED_CODES.contains(code)
        })
}

/// [`only_counted`] for a reference answer whose every named site was folded
/// into a count: the named sites its code says are listed are all counted,
/// and calls through a value are moot once the census ruled out every value
/// use (`values_ruled_out`), or once every value use it lists was settled as
/// adding no function to the answer (`values_settled`), which also accounts
/// for the residue the census listed.
pub(crate) fn only_counted_or_folded(
    verdict: &MachineVerdict,
    values_ruled_out: bool,
    values_settled: bool,
) -> bool {
    let codes: Vec<&str> = verdict
        .codes
        .iter()
        .map(String::as_str)
        .filter(|code| !verdict.is_delivery(code))
        .collect();
    !verdict.certified()
        && !codes.is_empty()
        && codes.iter().all(|code| {
            *code == kin_model::call_site_reading::UNKEYED_LOOKUPS_COUNTED
                || *code == kin_model::call_site_reading::NAMED_LEFTOVERS
                || ((values_ruled_out || values_settled)
                    && *code == kin_model::call_site_reading::VALUE_CALLS_UNMEASURED)
                || (values_settled && *code == kin_model::call_site_reading::ESCAPE_RESIDUE_LISTED)
                || crate::verdict::FOLDED_CODES.contains(code)
        })
}

/// Whether the escape census of a reference answer lists exactly `settled`
/// value uses and no other kind of place: no same-named binding, no call the
/// language makes without spelling the name, no file without a record, no
/// lookup that spells the name. Counted lookups by a built name and reads
/// counted apart are not uses. A read left unmatched that is not one of the
/// value uses is a listed site of its own, and the caller's `listed_open`
/// answers for it before this is asked.
pub(crate) fn census_lists_only_value_uses(machine: &Map<String, Value>, settled: u64) -> bool {
    let Some(residue) = machine
        .get(crate::handlers::entities::CALL_SITE_CANDIDATES_KEY)
        .and_then(|candidates| {
            candidates
                .pointer("/value_calls/census")
                .or_else(|| candidates.get("census"))
        })
        .and_then(|census| census.get("residue"))
        .and_then(Value::as_object)
    else {
        return false;
    };
    let listed = |key: &str| {
        residue.get(key).map_or(0, |list| {
            list.get("count")
                .and_then(Value::as_u64)
                .unwrap_or_else(|| {
                    list.get("sites")
                        .or(Some(list))
                        .and_then(Value::as_array)
                        .map_or(1, |sites| sites.len() as u64)
                })
        })
    };
    settled > 0
        && listed("values") == settled
        && [
            "bindings",
            "implicit_calls",
            "protocol",
            "unrecorded",
            "reflective",
        ]
        .iter()
        .all(|key| listed(key) == 0)
}

/// Whether the only thing keeping a reference answer from certifying is that
/// non-call mentions are not recorded: its open codes are
/// [`REFERENCE_COMPLETION_UNMEASURED`] and, at most, the counted lookups by a
/// built name that the scope line states.
pub(crate) fn only_non_call_unmeasured(verdict: &MachineVerdict) -> bool {
    verdict
        .codes
        .iter()
        .any(|code| code == REFERENCE_COMPLETION_UNMEASURED)
        && verdict.codes.iter().all(|code| {
            code == REFERENCE_COMPLETION_UNMEASURED || code == "unkeyed_lookups_counted"
        })
}

/// Whether the reply measured that nothing holds the entity as a value: the
/// census of places that pass or store it found none.
pub(crate) fn value_uses_ruled_out(machine: &Map<String, Value>) -> bool {
    machine
        .get(crate::handlers::entities::CALL_SITE_CANDIDATES_KEY)
        .and_then(|candidates| {
            candidates
                .pointer("/value_calls/census")
                .or_else(|| candidates.get("census"))
        })
        .and_then(|census| census.get("escape"))
        .and_then(Value::as_str)
        == Some("contained")
}

/// Whether a verdict code states how Kin resolved calls across the store. A
/// status reply counts those for the whole repository, which qualifies no
/// answer, so they are left out of its health report. Work Kin still owes is
/// reported from its counts instead.
pub(crate) fn is_call_resolution_code(code: &str) -> bool {
    use kin_model::call_site_reading as reading;
    reading::RESIDUE_CODES.contains(&code)
        || reading::LISTED_SITE_CODES.contains(&code)
        || code.starts_with("call_sites")
        || code.starts_with("caller_arrival")
        || code.starts_with("proof_context")
        || code.starts_with("enrichment")
        || matches!(
            code,
            "method_call_resolution_incomplete"
                | "binding_unproven"
                | "local_binding_unproven"
                | "reference_enrichment_unknown"
        )
}

/// How many places the walk could not follow, when every one of them is
/// listed in `reach.frontier` with its reason and none is withheld. Those are
/// the trace's sites listed below, as a reference answer's are; a frontier
/// with withheld places stays a limit.
pub(crate) fn trace_frontier_listed(machine: &Map<String, Value>) -> Option<u64> {
    let reach = machine.get("reach").or_else(|| {
        machine
            .get("readings")?
            .as_array()?
            .iter()
            .find(|reading| reading.get("key").and_then(Value::as_str) == Some("reach"))?
            .get("value")
    })?;
    let total = reach.get("frontier_total").and_then(Value::as_u64)?;
    let listed = reach
        .get("frontier")
        .and_then(Value::as_array)
        .map_or(0, Vec::len) as u64;
    let withheld = reach
        .get("frontier_withheld")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    (total > 0 && listed == total && withheld == 0).then_some(total)
}

/// The places a call through a value could take a reached entity from, when
/// the reach reading establishes that they are all of them and this reply
/// serves every one.
#[derive(Debug, Default)]
pub(crate) struct ValuePlaces {
    /// `also_referenced_by` rows that hold a member as a value.
    pub(crate) holders: Vec<String>,
    /// Members that hold a member as a value themselves.
    pub(crate) members: Vec<String>,
}

/// [`ValuePlaces`] for a reply, or `None` when the reading is not a complete
/// closure, a member has no census, or a place is not served; the value
/// qualification then stays open.
pub(crate) fn trace_value_places(machine: &Map<String, Value>) -> Option<ValuePlaces> {
    let reach = crate::trace_reach::answer_view(machine)?;
    let held = reach
        .get("blind_spots")?
        .get(crate::trace_reach::VALUE_HOLDERS_KEY)?;
    let uncensused = reach
        .get("blind_spots")
        .and_then(|blind| blind.pointer("/uncensused/members"))
        .and_then(Value::as_array)
        .is_some_and(|members| !members.is_empty());
    if held.get("complete").and_then(Value::as_bool) != Some(true) || uncensused {
        return None;
    }
    let rows = reach
        .get("also_referenced_by")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    let total = reach
        .get("also_referenced_total")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if rows.len() as u64 != total {
        return None;
    }
    let holding: Vec<&Value> = rows
        .iter()
        .filter(|row| {
            row.get("refs")
                .and_then(Value::as_array)
                .is_some_and(|refs| {
                    refs.iter()
                        .any(|reference| reference.get("value").is_some())
                })
        })
        .collect();
    if holding.len() as u64 != held.get("rows").and_then(Value::as_u64).unwrap_or(0) {
        return None;
    }
    let distinct = |names: &mut dyn Iterator<Item = &str>| {
        let mut out: Vec<String> = Vec::new();
        for name in names {
            if !out.iter().any(|seen| seen == name) {
                out.push(name.to_string());
            }
        }
        out
    };
    Some(ValuePlaces {
        holders: distinct(
            &mut holding
                .iter()
                .filter_map(|row| row.get("name").and_then(Value::as_str)),
        ),
        members: distinct(
            &mut held
                .get("in_members")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|row| row.get("name").and_then(Value::as_str)),
        ),
    })
}

/// Whether the reach reading still qualifies calls through values. A
/// counted site is conditioned on a reached entity's escape, not proof of
/// an in-scope caller. A zero site count cannot discharge an unmeasured
/// caller or a missing census, and a listed named frontier covers neither.
/// A complete reading that serves every place a member is held as a value
/// ([`trace_value_places`]) discharges it: those places are listed instead.
pub(crate) fn trace_value_qualification_open(
    verdict: &MachineVerdict,
    machine: &Map<String, Value>,
) -> bool {
    if trace_value_places(machine).is_some() {
        return false;
    }
    let value_gap = |reach: &Value| {
        reach.get("blind_spots").is_some_and(|blind| {
            blind
                .get("calls_through_values")
                .and_then(Value::as_u64)
                .is_some_and(|count| count > 0)
                || blind
                    .pointer("/uncensused/members")
                    .and_then(Value::as_array)
                    .is_some_and(|members| !members.is_empty())
        })
    };
    verdict.codes.iter()
        .any(|code| code == kin_model::call_site_reading::VALUE_CALLS_UNMEASURED)
        || machine.get("reach").is_some_and(value_gap)
        // The named-frontier decision also reads raw reach metadata. Keep
        // its qualifiers even when incomplete or conflicting readings cannot
        // establish a closure; none is being selected as a proof here.
        || machine.get("readings").and_then(Value::as_array).is_some_and(|readings| {
            readings.iter().any(|reading| {
                reading.get("key").and_then(Value::as_str) == Some("reach")
                    && reading.get("value").is_some_and(value_gap)
            })
        })
}

/// The verdict code a reference answer carries when it was asked about more
/// than calls and Kin measured only calls and values.
pub(crate) const REFERENCE_COMPLETION_UNMEASURED: &str = "reference_completion_unmeasured";

/// The registered tool `trace` walks a chain through.
pub(crate) const TRACE_TOOL: &str = "trace_data_flow";

/// The registered tool `source` reads an entity through.
pub(crate) const SOURCE_TOOL: &str = "get_entity_source";

/// The registered tool `history` answers through.
pub(crate) const HISTORY_TOOL: &str = "kin_provenance_query";

/// How the group of unresolved coverage gaps begins, listed or folded.
const GAP_LEAD: &str = "Unresolved coverage gaps: Kin could not establish the callee name, \
     caller text, or target-name scope";

/// Where the group of unresolved coverage gaps counts its retained entries by
/// the reason each was kept without an observed callee.
pub(crate) const GAP_REASONS_KEY: &str = "by_reason";

/// The site field naming the module a listed call's receiver is imported
/// from, as its file's import statement writes it.
pub(crate) const RECEIVER_IMPORTED_FROM_KEY: &str = "receiver_imported_from";

/// The site field saying no file of the repository supplies that module.
pub(crate) const RECEIVER_OUTSIDE_KEY: &str = "receiver_outside_repository";

/// Why a call or a read is counted rather than listed: it is made on a name
/// its file imports from a module no file of the repository supplies.
pub(crate) const RECEIVER_IMPORTED_FROM_OUTSIDE: &str = "receiver_imported_from_outside";

/// Why a call that passes the name on as an argument is counted rather than
/// listed: the function that makes it is already a row of the answer.
pub(crate) const PASSER_ALREADY_A_ROW: &str = "passer_already_a_row";

/// Why a call is counted rather than listed: its caller is already a row of
/// the answer.
pub(crate) const CALLER_ALREADY_A_ROW: &str = "caller_already_a_row";

/// Why a read that is no call is counted rather than listed: the value its
/// receiver holds is proved to take the member from a type no file of the
/// repository declares.
pub(crate) const MEMBER_OF_OUTSIDE_TYPE: &str = "member_of_outside_type";

/// Why a read is counted rather than listed: its receiver is an instance of
/// a class that shares no instance with the entity's owner, so the member it
/// reads is another hierarchy's. The reason is relative to the entity asked
/// about: it says nothing about where the member that is read is declared.
pub(crate) const RECEIVER_OF_UNRELATED_CLASS: &str = "receiver_of_unrelated_class";

/// Where a reference answer counts the places that spell the name and are
/// proved to add no function to the answer: `[{"why", "folded_entries",
/// "by_reason"}]`. Nothing in it is left to check, and nothing in it weakens
/// the verdict.
pub const SETTLED_KEY: &str = "settled";

/// The reasons that prove a counted place adds no function to the answer:
/// the function it sits in is already a row, or what it reaches is declared
/// outside the repository. Every other reason a place is counted under names
/// a way it could still reach the entity, and stays in `left_to_check`.
///
/// `receiver_outside_repository` on a call is not among them: it says the
/// receiver is what a library's method returned, which may be an object of
/// this repository. Nor is `receiver_literal_or_builtin`, which is read from
/// the receiver's text as an ordering hint and proves nothing.
///
/// `receiver_of_unrelated_class` is a proof about the entity asked about
/// only: the receiver's class and the entity's owner share no instance.
const PROOF_REASONS: [&str; 5] = [
    CALLER_ALREADY_A_ROW,
    PASSER_ALREADY_A_ROW,
    RECEIVER_IMPORTED_FROM_OUTSIDE,
    MEMBER_OF_OUTSIDE_TYPE,
    RECEIVER_OF_UNRELATED_CLASS,
];

/// The census reason for an attribute of a class Kin keeps.
pub(crate) const CLASS_ATTRIBUTE_REASON: &str = "class_member_not_admitted";

/// `text` with its first letter in capitals.
pub(crate) fn capitalized(text: &str) -> String {
    let mut characters = text.chars();
    match characters.next() {
        Some(first) => first.to_uppercase().chain(characters).collect(),
        None => String::new(),
    }
}

/// `a` or `an` before `word`.
pub(crate) fn article(word: &str) -> &'static str {
    if word
        .chars()
        .next()
        .is_some_and(|first| "AEIOUaeiou".contains(first))
    {
        "an"
    } else {
        "a"
    }
}

pub(crate) fn plural<'a>(count: u64, one: &'a str, many: &'a str) -> &'a str {
    if count == 1 {
        one
    } else {
        many
    }
}

/// An observed count with the noun naming its actual unit.
pub(crate) fn counted(count: u64, one: &str, many: &str) -> String {
    format!("{count} {}", plural(count, one, many))
}

/// The owner a method belongs to, from its qualified name.
pub(crate) fn owner_of(focal: &Value) -> Option<String> {
    let kind = focal.get("kind").and_then(Value::as_str)?;
    if !kind.eq_ignore_ascii_case("method") {
        return None;
    }
    let name = focal.get("name").and_then(Value::as_str)?;
    let (owner, _) = name.rsplit_once('.')?;
    Some(owner.rsplit('.').next().unwrap_or(owner).to_string())
}

/// The plain reason for a group of named leftovers in one state. `bare` says
/// the calls of the group are written on no object, which matters for a
/// member: no object's member is read by a bare name.
fn named_reason(state: &str, callee: &str, focal: &str, owner: Option<&str>, bare: bool) -> String {
    match state {
        "unresolved" => match owner {
            Some(owner) if bare => format!(
                "Calls of the bare name {callee}, made on no object. A bare name runs a member only where code binds the member to that name: an assignment, a destructuring or a re-export of `object.{callee}`. Check one only if the {callee} its caller's module binds could be {} {owner}'s.",
                article(owner)
            ),
            Some(owner) => format!(
                "Calls to a method named {callee} on an object Kin could not identify. Check one only if its object could be {} {owner}.",
                article(owner)
            ),
            None => format!(
                "Calls to something named {callee} that Kin could not match to a definition. Check one only if it could be {focal}."
            ),
        },
        "binding" => format!(
            "Calls through a variable named {callee}: a parameter, a local or an assigned name, not a declaration. One runs {focal} only if that variable was handed {focal} as a value, so check where its caller gets it."
        ),
        "owed_derivation" | "owed_enrichment" => format!(
            "Calls to {callee} that Kin has not finished checking yet. Ask again later, or check each one."
        ),
        "server_failed" => format!(
            "Calls to {callee} that the language server failed on. Check each one."
        ),
        "not_in_build" | "unproven_no_resolver" => format!(
            "Calls to {callee} that no language server could check. Check each one."
        ),
        "proof_context_stale" | "proof_context_unverified" => format!(
            "Calls to {callee} that were checked against an older build setup. Check each one."
        ),
        _ => format!("Calls to {callee} that Kin could not confirm. Check each one."),
    }
}

/// Where a left-to-check call site names the module that holds its caller.
pub(crate) const CALLER_MODULE_KEY: &str = "caller_module";

/// The plain reason for one census residue group.
fn residue_reason(key: &str, name: &str, focal: &str) -> String {
    match key {
        "values" => format!(
            "Places that pass or store something named {name} as a value instead of calling it. A later call through that value would reach it without saying its name. Check one only if the value could be {focal}."
        ),
        "bindings" => format!(
            "Places that assign something named {name} to another name. Check one only if it could be {focal}."
        ),
        "alias_sites" => format!("Places where recorded bindings give {focal} another name. Read references and their coverage verdict for calls through those names."),
        "implicit_calls" => format!(
            "Places the language calls {name} without spelling its name, such as an operator or a with or for statement."
        ),
        "reflective" => format!(
            "Code that looks a name up while it runs, where the name as written could be {name}. Check one only if the name it looks up could be {name}."
        ),
        "unrecorded" => "Some uses lack a complete semantic observation.".to_string(),
        "protocol" => format!(
            "Places the language itself can call {name} through a protocol, without code that says its name."
        ),
        kin_model::call_site_reading::MENTIONS_KEY => format!(
            "Places that read {name} without calling it, such as an import or a type use, where no recorded reference says what they read. Check one only if it could read {focal}."
        ),
        crate::call_sites::DISPATCH_DECORATORS_KEY => format!(
            "Decorators on {focal}. A call that says {name} runs what the decorator returned, which is how the rows and sites above reach it. Check one only if that decorator could keep {name} and call it some other way."
        ),
        _ => format!("Other places that could reach {focal}. Check each one."),
    }
}

/// One census residue site, plainly: where it is, the declaration it sits in
/// when a reader can read that one with `source`, and its code.
fn plain_residue_site(site: &Value) -> Value {
    let mut plain = Map::new();
    for key in [
        "inside",
        "inside_id",
        "in_module",
        "in_module_id",
        "line_in_entity",
    ] {
        if let Some(value) = site.get(key) {
            plain.insert(key.into(), value.clone());
        }
    }
    if let Some(code) = site.get("expr") {
        plain.insert("code".into(), code.clone());
    } else {
        // With no line to quote, say what the site is and the name it
        // spells, so it is never an empty entry.
        for key in ["kind", "name"] {
            if let Some(value) = site.get(key) {
                plain.insert(key.into(), value.clone());
            }
        }
        if let Some(detail) = site.get("detail") {
            plain.insert("code".into(), detail.clone());
        }
    }
    Value::Object(plain)
}

/// A census residue list, plainly: its sites, and when it is longer than the
/// sites served, its count and where the rest are.
fn plain_residue_list(list: &Value) -> (u64, Map<String, Value>) {
    let mut group = Map::new();
    let sites: Vec<Value> = list
        .get("sites")
        .and_then(Value::as_array)
        .map(|sites| sites.iter().map(plain_residue_site).collect())
        .unwrap_or_default();
    let count = list
        .get("count")
        .and_then(Value::as_u64)
        .unwrap_or(sites.len() as u64);
    if count > sites.len() as u64 {
        group.insert("count".into(), json!(count));
    }
    group.insert("sites".into(), json!(sites));
    (count, group)
}

/// How many outstanding local-name bindings the answer showed unrelated to
/// it, read from the binding scope its source observation carries under
/// either of the names the envelope uses for it.
pub(crate) fn unrelated_bindings(envelope: &Value) -> Option<u64> {
    ["source_derivation", "binding_coverage"]
        .iter()
        .find_map(|key| {
            envelope
                .get(*key)?
                .pointer("/binding_scope/unrelated_obligations")?
                .as_u64()
        })
        .filter(|count| *count > 0)
}

/// Collapse adjacent, identical projected coverage gaps whose code is not
/// available. `occurrences` counts retained entries on this page, not calls
/// or distinct source positions. Expanding it restores the projected order.
/// The caller keeps its original totals and frozen evidence cursor.
pub(crate) fn compact_uninspectable_sites(sites: Vec<Value>) -> (Vec<Value>, bool) {
    let mut runs: Vec<(Value, u64, bool)> = Vec::new();
    for site in sites {
        let compactable = site.is_object()
            && site.get("occurrences").is_none()
            && site
                .get("code")
                .map(|code| code.is_null() || code.as_str() == Some(""))
                .unwrap_or(true);
        match runs
            .last_mut()
            .filter(|(previous, _, eligible)| compactable && *eligible && *previous == site)
        {
            Some((_, count, _)) => *count += 1,
            None => runs.push((site, 1, compactable)),
        }
    }
    let mut compacted = false;
    let sites = runs
        .into_iter()
        .map(|(mut site, count, _)| {
            if count > 1 {
                site["occurrences"] = json!(count);
                compacted = true;
            }
            site
        })
        .collect();
    (sites, compacted)
}

/// What a reference answer leaves to check, as groups with a plain reason,
/// how many sites they hold together, and the unkeyed lookups it counts.
/// The candidate sites minus any that a proven row already answers: the same
/// caller, the same line and the same code. A call the resolver both proved
/// and kept as an unmatched candidate (React's reconciler calls a host-config
/// function both through its import and by its bare name) is answered, not
/// left to check.
pub(crate) fn without_proven_sites(candidates: &Value, rows: &[Value]) -> Value {
    let key = |caller: Option<&Value>, line: Option<&Value>, code: Option<&Value>| {
        (
            caller.and_then(Value::as_str).map(str::to_string),
            line.and_then(Value::as_u64),
            code.and_then(Value::as_str).map(str::to_string),
        )
    };
    let proven: std::collections::HashSet<_> = rows
        .iter()
        .filter(|row| {
            !matches!(
                row.get("resolution").and_then(Value::as_str),
                Some("name_only") | Some(crate::handlers::entities::NAME_UNIQUE_RESOLUTION)
            )
        })
        .flat_map(|row| {
            row.get("sites")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|site| {
                    key(
                        row.get("entity_id"),
                        site.get("line_in_entity"),
                        site.get("expr"),
                    )
                })
        })
        .filter(|(caller, _, code)| caller.is_some() && code.is_some())
        .collect();
    if proven.is_empty() {
        return candidates.clone();
    }
    let mut kept = candidates.clone();
    if let Some(named) = kept.get_mut("named").and_then(Value::as_array_mut) {
        named.retain(|site| {
            !proven.contains(&key(
                site.get("caller"),
                site.get("line_in_entity"),
                site.get("expr"),
            ))
        });
    }
    kept
}

/// Why one gap row was kept without an observed callee, as a short code: a
/// focal with no name a call could spell, else the reason its caller's text
/// could not be read, else its recorded reach reason, else that no callee name
/// was recorded. Codes, not prose, so a count by reason stays a few bytes.
fn gap_reason(row: &Value) -> String {
    let reason = row.get("reason").and_then(Value::as_str);
    let code = if reason == Some(kin_model::call_site_reading::REACH_NAME_UNKNOWN) {
        reason
    } else {
        row.get("expr_unavailable")
            .and_then(Value::as_str)
            .or(reason)
    };
    code.unwrap_or("callee_name_unknown").replace(' ', "_")
}

/// The group of unresolved coverage gaps as one count by reason, with the one
/// line that says they could still add callers and how to list them. It stays
/// among the places left to check, so the answer still reads as open: a fold
/// counts the gaps and never turns them into an absence.
fn fold_gap_group(group: &Value, focal_name: &str) -> Value {
    let reasons = group.get(GAP_REASONS_KEY).cloned().unwrap_or(json!({}));
    let entries: u64 = reasons
        .as_object()
        .map(|reasons| reasons.values().filter_map(Value::as_u64).sum())
        .unwrap_or(0);
    let mut folded = Map::new();
    folded.insert(
        "why".into(),
        json!(format!(
            "{GAP_LEAD} at {} on this page. They are not observed calls to {focal_name}; one \
             could add a caller only if the code Kin could not read there calls it. \
             {GAP_REASONS_KEY} counts them; to list them, repeat this request or its incoming \
             cursor with details:true.",
            counted(entries, "retained entry", "retained entries"),
        )),
    );
    folded.insert("retained_entries".into(), json!(entries));
    folded.insert(GAP_REASONS_KEY.into(), reasons);
    Value::Object(folded)
}

/// Whether a group of places left to check is a count rather than a list:
/// the folded coverage gaps and the folded settled sites, each counted by
/// reason. A fold never makes an answer partial and never closes it.
fn is_folded_group(group: &Value) -> bool {
    group.get("sites").is_none() && group.get(GAP_REASONS_KEY).is_some()
}

/// Why a named site whose callee spells the name is counted rather than
/// listed, when it is a kind of site that never held a missed caller: its
/// caller is already a proven row, or its receiver is a value of a type
/// outside the repository, a literal or builtin, or a name imported from a
/// module outside the repository.
fn settled_site_reason(
    site: &Value,
    proven: &std::collections::HashSet<&str>,
    member: bool,
) -> Option<&'static str> {
    if site
        .get("caller_id")
        .and_then(Value::as_str)
        .is_some_and(|caller| proven.contains(caller))
    {
        return Some(CALLER_ALREADY_A_ROW);
    }
    // A module some other project supplies holds its own members. It can
    // still hand out a value of a class declared here, so a method keeps the
    // site.
    if !member && site.get(RECEIVER_OUTSIDE_KEY).and_then(Value::as_bool) == Some(true) {
        return Some(RECEIVER_IMPORTED_FROM_OUTSIDE);
    }
    if site
        .pointer(&format!(
            "/{}/reason",
            crate::receiver_binding::RECEIVER_BINDING_MISSING_KEY
        ))
        .and_then(Value::as_str)
        == Some("method_of_external_type")
    {
        return Some("receiver_outside_repository");
    }
    if matches!(
        site.get("receiver_hint").and_then(Value::as_str),
        Some("literal" | "builtin")
    ) {
        return Some("receiver_literal_or_builtin");
    }
    None
}

/// Fold the named sites of `groups` whose kind never held a missed caller
/// into counts, by reason. A site counted under one of [`PROOF_REASONS`] adds
/// no function to the answer: those go to `settled` as one group. Every
/// other counted site stays in `groups` as one group, since its reason names
/// a way it could still reach the entity. Groups left without a site go;
/// every other group keeps its order. Returns how many entries were settled
/// and how many were counted and left open.
fn fold_settled_sites(
    groups: &mut Vec<Value>,
    settled: &mut Vec<Value>,
    proven: &std::collections::HashSet<&str>,
    focal_name: &str,
    member: bool,
) -> (u64, u64) {
    let mut by_reason: std::collections::BTreeMap<&'static str, u64> =
        std::collections::BTreeMap::new();
    groups.retain_mut(|group| {
        let Some(sites) = group.get_mut("sites").and_then(Value::as_array_mut) else {
            return true;
        };
        if sites.is_empty() {
            return true;
        }
        sites.retain(|site| match settled_site_reason(site, proven, member) {
            Some(reason) => {
                *by_reason.entry(reason).or_default() +=
                    site.get("occurrences").and_then(Value::as_u64).unwrap_or(1);
                false
            }
            None => true,
        });
        !sites.is_empty()
    });
    let (proofs, open): (
        std::collections::BTreeMap<&'static str, u64>,
        std::collections::BTreeMap<&'static str, u64>,
    ) = by_reason
        .into_iter()
        .partition(|(reason, _)| PROOF_REASONS.contains(reason));
    let proved: u64 = proofs.values().sum();
    if proved > 0 {
        let mut why = format!(
            "{} that {} {focal_name}, {}proved to add no function to this answer.",
            capitalized(&counted(proved, "call site", "call sites")),
            plural(proved, "spells", "spell"),
            if proved == 1 { "" } else { "each " },
        );
        if proofs.contains_key(CALLER_ALREADY_A_ROW) {
            why.push_str(&format!(
                " {CALLER_ALREADY_A_ROW}: the caller is already a row of this answer."
            ));
        }
        if proofs.contains_key(RECEIVER_IMPORTED_FROM_OUTSIDE) {
            why.push_str(&format!(
                " {RECEIVER_IMPORTED_FROM_OUTSIDE}: the call is made on a name its file imports \
                 from a module no file of this repository supplies, so it runs that module's \
                 own member."
            ));
        }
        settled.push(json!({
            "why": why,
            "folded_entries": proved,
            GAP_REASONS_KEY: proofs,
        }));
    }
    let counted_open: u64 = open.values().sum();
    if counted_open > 0 {
        groups.push(json!({
            "why": format!(
                "{} that {} {focal_name}, counted by reason rather than listed. \
                 receiver_outside_repository and receiver_literal_or_builtin: the call is made \
                 on a value of a type outside the repository, or on a literal or builtin, so it \
                 could reach {focal_name} only if that value were one of its own objects. \
                 To list them, repeat this request or its incoming cursor with details:true.",
                capitalized(&counted(counted_open, "call site", "call sites")),
                plural(counted_open, "spells", "spell"),
            ),
            "folded_entries": counted_open,
            GAP_REASONS_KEY: open,
        }));
    }
    (proved, counted_open)
}

/// The census's reads on names imported from outside the repository, as one
/// counted group: which read, on which name, imported from which module. They
/// are never sites to check, so the group lists none.
fn outside_reads_group(list: &Value) -> Option<(u64, Value)> {
    let mut total = 0u64;
    let mut parts: Vec<String> = Vec::new();
    for row in list.as_array()? {
        let text = |key: &str| row.get(key).and_then(Value::as_str);
        let (Some(receiver), Some(module), Some(name)) =
            (text("receiver"), text("module"), text("name"))
        else {
            continue;
        };
        let reads = row.get("reads").and_then(Value::as_u64).unwrap_or(1);
        total += reads;
        parts.push(format!(
            "{} of {receiver}.{name}, where its file imports {receiver} from the module {module}",
            counted(reads, "read", "reads"),
        ));
    }
    if total == 0 {
        return None;
    }
    let why = format!(
        "{}. No file of this repository supplies {}, so {} the outside module's own member, \
         not this one. Counted, not listed.",
        capitalized(&parts.join("; ")),
        plural(parts.len() as u64, "that module", "those modules"),
        plural(total, "it reads", "each reads"),
    );
    let mut reasons = Map::new();
    reasons.insert(RECEIVER_IMPORTED_FROM_OUTSIDE.into(), json!(total));
    Some((
        total,
        json!({"why": why, "folded_entries": total, GAP_REASONS_KEY: reasons}),
    ))
}

/// The census's reads, not calls, on a receiver whose value is proved to
/// take the member from a type outside the repository, as one counted group:
/// which read, on which name, from which type. They are never sites to
/// check, so the group lists none.
fn outside_values_group(list: &Value) -> Option<(u64, Value)> {
    let mut total = 0u64;
    let mut parts: Vec<String> = Vec::new();
    for row in list.as_array()? {
        let text = |key: &str| row.get(key).and_then(Value::as_str);
        let (Some(receiver), Some(owner), Some(name)) =
            (text("receiver"), text("owner"), text("name"))
        else {
            continue;
        };
        let reads = row.get("reads").and_then(Value::as_u64).unwrap_or(1);
        total += reads;
        parts.push(format!(
            "{} of {receiver}.{name}, where the value {receiver} holds takes {name} from {owner}",
            counted(reads, "read", "reads"),
        ));
    }
    if total == 0 {
        return None;
    }
    let why = format!(
        "{}. No file of this repository declares {}, so {} a member declared outside this \
         repository, not this one. Counted, not listed.",
        capitalized(&parts.join("; ")),
        plural(parts.len() as u64, "that type", "those types"),
        plural(total, "it reads", "each reads"),
    );
    let mut reasons = Map::new();
    reasons.insert(MEMBER_OF_OUTSIDE_TYPE.into(), json!(total));
    Some((
        total,
        json!({"why": why, "folded_entries": total, GAP_REASONS_KEY: reasons}),
    ))
}

/// The census's reads on a receiver whose class shares no instance with the
/// entity's owner, as one counted group: which read, off an instance of
/// which class. Each sentence is about the entity asked about, never about
/// where the member that is read is declared: a class of the repository may
/// well declare it. They are never sites to check, so the group lists none.
fn unrelated_receivers_group(list: &Value, owner: Option<&str>) -> Option<(u64, Value)> {
    let owner = owner?;
    let mut total = 0u64;
    let mut parts: Vec<String> = Vec::new();
    for row in list.as_array()? {
        let text = |key: &str| row.get(key).and_then(Value::as_str);
        let (Some(receiver), Some(class), Some(name)) =
            (text("receiver"), text("class"), text("name"))
        else {
            continue;
        };
        let reads = row.get("reads").and_then(Value::as_u64).unwrap_or(1);
        total += reads;
        parts.push(format!(
            "{} of {receiver}.{name}, where {receiver} is an instance of {class}",
            counted(reads, "read", "reads"),
        ));
    }
    if total == 0 {
        return None;
    }
    let why = format!(
        "{}. {owner} is not in {}: no class is, or derives from, both. So {} a member of that \
         other hierarchy, not this one. Counted, not listed.",
        capitalized(&parts.join("; ")),
        plural(
            parts.len() as u64,
            "that class's hierarchy",
            "those classes' hierarchies"
        ),
        plural(total, "it reads", "each reads"),
    );
    let mut reasons = Map::new();
    reasons.insert(RECEIVER_OF_UNRELATED_CLASS.into(), json!(total));
    Some((
        total,
        json!({"why": why, "folded_entries": total, GAP_REASONS_KEY: reasons}),
    ))
}

pub(crate) fn left_to_check(candidates: &Value, focal: &Value) -> Leftovers {
    let focal_name = focal
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("this entity");
    let short_name = focal_name.rsplit('.').next().unwrap_or(focal_name);
    let owner = owner_of(focal);
    let mut groups: Vec<Value> = Vec::new();
    let mut total = 0u64;

    // A retained site can be named only because its caller text is unknown.
    // That is a coverage gap, not an observed call spelling the focal name.
    // Sites whose code spells a name are grouped as they arrive, adjacent
    // rows in one state together, and come first: they are the ones that
    // answer the question. The gaps follow as one group in their own order,
    // since the same reason restated for every run of them was most of a
    // reply that found no caller at all.
    let mut named: Vec<((String, Option<String>, bool), Vec<Value>)> = Vec::new();
    let mut gap_sites: Vec<Value> = Vec::new();
    let mut gap_reasons: std::collections::BTreeMap<String, u64> =
        std::collections::BTreeMap::new();
    for row in candidates
        .get("named")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        let state = row["state"].as_str().unwrap_or("unresolved").to_string();
        let callee = row
            .get("callee")
            .and_then(Value::as_str)
            .filter(|name| !name.trim().is_empty())
            .map(str::to_string);
        let unknown_text = callee.is_none()
            || matches!(
                row.get("reason").and_then(Value::as_str),
                Some(
                    kin_model::call_site_reading::REACH_TEXT_UNKNOWN
                        | kin_model::call_site_reading::REACH_NAME_UNKNOWN
                )
            );
        let mut site = Map::new();
        if let Some(caller) = row.get("caller_name") {
            site.insert("caller".into(), caller.clone());
        }
        // The module that holds the caller: a caller's name alone does not
        // say where it sits, and a reader that names the caller in an answer
        // writes `module :: caller` the way every row is written.
        if let Some(module) = row
            .get("module")
            .filter(|module| module.as_str().is_some_and(|module| !module.is_empty()))
        {
            site.insert(CALLER_MODULE_KEY.into(), module.clone());
        }
        if let Some(id) = row.get("caller") {
            site.insert("caller_id".into(), id.clone());
        }
        if let Some(line) = row.get("line_in_entity") {
            site.insert("line_in_caller".into(), line.clone());
        }
        if let Some(code) = row.get("expr") {
            site.insert("code".into(), code.clone());
        }
        for key in [
            "reason",
            "state_reason",
            "expr_unavailable",
            "receiver",
            "receiver_hint",
            "occurrences",
            crate::receiver_binding::RECEIVER_BINDING_MISSING_KEY,
        ] {
            if let Some(value) = row.get(key).filter(|value| !value.is_null()) {
                site.insert(key.into(), value.clone());
            }
        }
        // What the receiver is imported from, in a reader's words.
        if let Some(import) = row.get(crate::site_expression::RECEIVER_IMPORT_KEY) {
            if let Some(module) = import.get("module").and_then(Value::as_str) {
                site.insert(RECEIVER_IMPORTED_FROM_KEY.into(), json!(module));
                if import.get("outside_repository").and_then(Value::as_bool) == Some(true) {
                    site.insert(RECEIVER_OUTSIDE_KEY.into(), json!(true));
                }
            }
        }
        if unknown_text {
            if let Some(callee) = &callee {
                site.insert("callee".into(), json!(callee));
            }
            let entries = row.get("occurrences").and_then(Value::as_u64).unwrap_or(1);
            *gap_reasons.entry(gap_reason(row)).or_default() += entries;
            gap_sites.push(Value::Object(site));
            continue;
        }
        // A call of a bare name is a different question for a member than a
        // call made on an object, so the two never share a group.
        let bare = owner.is_some() && row.get("bare_call").and_then(Value::as_bool) == Some(true);
        let key = (state, callee, bare);
        match named.last_mut().filter(|(existing, _)| *existing == key) {
            Some((_, sites)) => sites.push(Value::Object(site)),
            None => named.push((key, vec![Value::Object(site)])),
        }
    }
    for ((state, callee, bare), sites) in named {
        // A row the reply already collapsed counts every entry it stands for.
        total += sites
            .iter()
            .map(|site| site.get("occurrences").and_then(Value::as_u64).unwrap_or(1))
            .sum::<u64>();
        let why = named_reason(
            &state,
            callee.as_deref().expect("observed callee"),
            focal_name,
            owner.as_deref(),
            bare,
        );
        groups.push(json!({"why":why,"sites":sites}));
    }
    let gap_group = (!gap_sites.is_empty()).then(|| {
        let entries: u64 = gap_reasons.values().sum();
        total += entries;
        let why = format!("{GAP_LEAD} for these retained sites. They are not observed calls to {focal_name}. Check any shown code; entries without code remain coverage gaps. A caller_id alone does not prove a call or promise that source can inspect it.");
        let mut group = json!({"why":why,"sites":gap_sites});
        let (sites, compacted) = compact_uninspectable_sites(
            group["sites"].as_array().expect("projected sites").clone(),
        );
        if compacted {
            let compact = json!({
                "why":format!("{why} occurrences counts identical retained entries on this page, not observed calls."),
                "sites":sites,
            });
            // Presentation follows budgeting. Include the explanation in
            // the comparison so even a tiny group cannot grow here.
            if compact.to_string().len() < group.to_string().len() {
                group = compact;
            }
        }
        group[GAP_REASONS_KEY] = json!(gap_reasons);
        groups.push(group);
        groups.len() - 1
    });
    if let Some(withheld) = candidates
        .get("named_withheld")
        .and_then(Value::as_u64)
        .filter(|count| *count > 0)
    {
        total += withheld;
        groups.push(json!({
            "why": format!("{withheld} more retained candidate sites did not fit in this reply. Ask for fewer relation kinds, or page with cursor, to see them."),
        }));
    }

    // The decorators a method is declared under, when they are the only way
    // it could run without a call that says its name.
    if let Some(decorators) = candidates
        .get(crate::call_sites::DISPATCH_DECORATORS_KEY)
        .and_then(Value::as_array)
        .filter(|decorators| !decorators.is_empty())
    {
        total += decorators.len() as u64;
        groups.push(json!({
            "why": residue_reason(crate::call_sites::DISPATCH_DECORATORS_KEY, short_name, focal_name),
            "sites": decorators.iter().map(plain_residue_site).collect::<Vec<_>>(),
        }));
    }

    // The census residue: every place the focal could be reached without a
    // call that says its name.
    let census = candidates
        .pointer("/value_calls/census")
        .or_else(|| candidates.get("census"));
    let mut unkeyed = 0u64;
    let mut other_names = None;
    let mut passed: Vec<Value> = Vec::new();
    let mut outside_reads = 0u64;
    let mut settled: Vec<Value> = Vec::new();
    // Where each listed value use sits. A non-call read at the same place is
    // that use again, and is listed once.
    let mut value_places: std::collections::HashSet<String> = std::collections::HashSet::new();
    // A site's place is its line when the residue carries one, and otherwise
    // the declaration it sits in with its code, which is all a reader sees.
    let place = |site: &Value| -> Option<String> {
        if let (Some(file), Some(line)) = (
            site.get("file").and_then(Value::as_str),
            site.get("line").and_then(Value::as_u64),
        ) {
            return Some(format!("{file}:{line}"));
        }
        let plain = plain_residue_site(site);
        (plain.get("code").is_some()
            && (plain.get("inside_id").is_some() || plain.get("in_module_id").is_some()))
        .then(|| plain.to_string())
    };
    if let Some(census) = census {
        if let Some(residue) = census.get("residue").and_then(Value::as_object) {
            if let Some((reads, group)) = residue
                .get(kin_model::call_site_reading::OUTSIDE_RECEIVERS_KEY)
                .and_then(outside_reads_group)
            {
                outside_reads = reads;
                settled.push(group);
            }
            if let Some((reads, group)) = residue
                .get(kin_model::call_site_reading::OUTSIDE_VALUES_KEY)
                .and_then(outside_values_group)
            {
                outside_reads += reads;
                settled.push(group);
            }
            if let Some((reads, group)) = residue
                .get(kin_model::call_site_reading::UNRELATED_RECEIVERS_KEY)
                .and_then(|list| unrelated_receivers_group(list, owner.as_deref()))
            {
                outside_reads += reads;
                settled.push(group);
            }
            unkeyed = residue
                .get("unkeyed_lookups")
                .and_then(|list| {
                    list.get("count").and_then(Value::as_u64).or_else(|| {
                        list.get("sites")
                            .and_then(Value::as_array)
                            .map(|sites| sites.len() as u64)
                    })
                })
                .unwrap_or(0);
            if let Some(list) = residue.get("alias_sites") {
                let (_, mut group) = plain_residue_list(list);
                group.insert(
                    "note".into(),
                    json!(residue_reason("alias_sites", short_name, focal_name)),
                );
                other_names = Some(Value::Object(group));
            }
            for key in [
                "values",
                "bindings",
                "implicit_calls",
                "protocol",
                "unrecorded",
                "reflective",
                kin_model::call_site_reading::MENTIONS_KEY,
            ] {
                let Some(list) = residue.get(key) else {
                    continue;
                };
                // The non-call reads no recorded row carries, each a place
                // to read. The reads a row covers are in the answer already.
                let list = if key == kin_model::call_site_reading::MENTIONS_KEY {
                    match list.get("unmatched") {
                        Some(unmatched) => unmatched,
                        None => continue,
                    }
                } else if key == "protocol" {
                    match list.get("sites") {
                        Some(sites) => sites,
                        None => {
                            groups
                                .push(json!({"why": residue_reason(key, short_name, focal_name)}));
                            continue;
                        }
                    }
                } else {
                    list
                };
                // A read the values already list is the same place twice.
                let mentions_left;
                let list = if key == kin_model::call_site_reading::MENTIONS_KEY
                    && !value_places.is_empty()
                {
                    let sites = list
                        .get("sites")
                        .and_then(Value::as_array)
                        .map(Vec::as_slice)
                        .unwrap_or_default();
                    let others: Vec<&Value> = sites
                        .iter()
                        .filter(|site| !place(site).is_some_and(|at| value_places.contains(&at)))
                        .collect();
                    if others.len() == sites.len() {
                        list
                    } else {
                        let count = list
                            .get("count")
                            .and_then(Value::as_u64)
                            .unwrap_or(sites.len() as u64);
                        let mut rest = list.clone();
                        rest["count"] =
                            json!(count.saturating_sub((sites.len() - others.len()) as u64));
                        rest["sites"] = json!(others);
                        if rest["count"] == json!(0) {
                            continue;
                        }
                        mentions_left = rest;
                        &mentions_left
                    }
                } else {
                    list
                };
                // A value passed straight into a call goes to code that can
                // call it: those sites are served on their own. Only listed
                // sites have a known position; any counted beyond them stay
                // with the values.
                let values_left;
                let list = if key == "values" {
                    let sites = list
                        .get("sites")
                        .and_then(Value::as_array)
                        .map(Vec::as_slice)
                        .unwrap_or_default();
                    value_places.extend(sites.iter().filter_map(place));
                    let (arguments, others): (Vec<&Value>, Vec<&Value>) =
                        sites.iter().partition(|site| {
                            site.get("kind").and_then(Value::as_str) == Some("argument")
                        });
                    if arguments.is_empty() {
                        list
                    } else {
                        passed.extend(arguments.into_iter().map(|site| {
                            let mut plain = plain_residue_site(site);
                            plain["note"] = json!(crate::site_expression::passed_note(site));
                            plain
                        }));
                        let count = list
                            .get("count")
                            .and_then(Value::as_u64)
                            .unwrap_or(sites.len() as u64);
                        let mut rest = list.clone();
                        rest["sites"] = json!(others);
                        rest["count"] = json!(count - (sites.len() - others.len()) as u64);
                        if others.is_empty() && rest["count"] == json!(0) {
                            continue;
                        }
                        values_left = rest;
                        &values_left
                    }
                } else {
                    list
                };
                let (count, mut group) = plain_residue_list(list);
                total += count;
                group.insert(
                    "why".into(),
                    json!(residue_reason(key, short_name, focal_name)),
                );
                groups.push(Value::Object(group));
            }
        } else if census.get("escape").and_then(Value::as_str) == Some("unknown") {
            let calls = candidates
                .pointer("/value_calls/sites")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if calls > 0 {
                total += calls;
                groups.push(json!({
                    "why": format!("Kin could not check whether {focal_name} is ever stored or passed as a value, so any of {calls} calls through a variable could reach it. They are not listed one by one."),
                    "count": calls,
                }));
            }
        }
    }
    Leftovers {
        groups,
        total,
        unkeyed,
        other_names,
        passed,
        gap_group,
        outside_reads,
        settled,
    }
}

/// Whether a reference question asked about calls and nothing else.
pub(crate) fn calls_only_question(machine: &Map<String, Value>) -> bool {
    machine
        .get("relation_kinds")
        .and_then(Value::as_array)
        .is_some_and(|kinds| {
            !kinds.is_empty()
                && kinds.iter().all(|kind| {
                    kind.as_str()
                        .is_some_and(|kind| kind.eq_ignore_ascii_case("calls"))
                })
        })
}

/// Leave the override census out of a calls-only answer: the classes that
/// spell the method's base without an inheritance edge are counted in
/// `coverage`, never listed, and the census's own scope gap goes with them.
/// Coverage still reads incomplete, so the count is never an absence.
pub(crate) fn leave_out_override_census(overrides: &mut Value) {
    let Some(block) = overrides.as_object_mut() else {
        return;
    };
    if let Some(Value::Array(classes)) = block.remove("classes_spelling_base_without_edge") {
        if let Some(coverage) = block.get_mut("coverage").and_then(Value::as_object_mut) {
            coverage.insert(
                "classes_spelling_base_without_edge".into(),
                json!(classes.len()),
            );
        }
    }
    if let Some(gaps) = block.get_mut("gaps").and_then(Value::as_array_mut) {
        gaps.retain(|gap| gap["code"] != "repository_override_coverage_unmeasured");
        if gaps.is_empty() {
            block.remove("gaps");
        }
    }
}

/// What a reference answer leaves open, read from its candidates block.
#[derive(Default)]
pub(crate) struct Leftovers {
    /// Groups of sites to check, each with a plain reason.
    pub(crate) groups: Vec<Value>,
    /// How many sites the groups hold together.
    pub(crate) total: u64,
    /// Lookups by a name no record follows, counted rather than listed.
    pub(crate) unkeyed: u64,
    /// Where the focal is bound under another name. Calls by those names are
    /// already among the references, so nothing here is left to check.
    pub(crate) other_names: Option<Value>,
    /// Calls that pass the focal to other code as an argument, each with its
    /// code: the code they call may call it, which makes each one evidence of
    /// its own kind rather than a place it could merely be held.
    pub(crate) passed: Vec<Value>,
    /// Where in `groups` the one group of unresolved coverage gaps sits:
    /// retained sites whose callee name Kin could not establish, which by its
    /// own wording are not observed calls to the focal.
    pub(crate) gap_group: Option<usize>,
    /// Reads the census counted apart because they are made on a name
    /// imported from outside the repository, or on a value proved to take the
    /// member from a type outside it. `settled` holds their counted groups;
    /// `total` does not count them.
    pub(crate) outside_reads: u64,
    /// Counted groups of places proved to add no function to the answer (see
    /// [`SETTLED_KEY`]). Nothing here is left to check.
    pub(crate) settled: Vec<Value>,
}

/// Whether a name search's reply holds every declaration of the name, and
/// what could hold another when it does not.
///
/// Read from the reply an agent is served, which carries no count or list of
/// source units, and stricter than the verdict word. The rows are every
/// declaration Kin holds under the name only when the search says it asked
/// for the name as a member's own name too, with no filter narrowing it. The
/// census speaks for the declarations Kin does not hold, and only when every
/// source unit carries a current one, each was parsed whole, and every
/// matching declaration it found is listed. Nothing may be withheld from the
/// reply. One condition short, the reply states no closure and names what is
/// open.
pub(crate) struct NameClosure {
    /// The reply holds every declaration of the name.
    pub(crate) closed: bool,
    /// Declarations the census read across the repository's source.
    pub(crate) declarations_read: Option<u64>,
    /// Declarations in this reply whose own name is the query.
    pub(crate) named_exactly: u64,
    /// Declarations in this reply, held or not.
    pub(crate) in_reply: u64,
    /// Some source is held as bytes and was never parsed.
    pub(crate) unparsed: bool,
    /// Some source was parsed around an error region, and how many source
    /// units when the reading counts them.
    pub(crate) partly_parsed: bool,
    pub(crate) partly_parsed_count: Option<u64>,
    /// What the regions no parse could read hold of the name, when the search
    /// read them back: clear of it, spelling it, or not read.
    pub(crate) unread_regions: Option<String>,
    /// The modules whose unread regions spell the name, and how many source
    /// units do, listed or not.
    pub(crate) spelled_in: Vec<String>,
    pub(crate) spelled_in_count: u64,
    /// Partly parsed source units whose content could not be read back.
    pub(crate) not_read_back: u64,
    /// Some source carries no current record of its declarations.
    pub(crate) unmeasured: bool,
    /// Matching declarations Kin holds no entity for that the census counted
    /// and did not list.
    pub(crate) unlisted: u64,
}

impl NameClosure {
    pub(crate) fn read(
        machine: &Map<String, Value>,
        verdict: &MachineVerdict,
        census: &Value,
        rows: &[Value],
        total: u64,
    ) -> Self {
        use crate::declaration_coverage as coverage;
        let number = |key: &str| census.get(key).and_then(Value::as_u64);
        let said = |code: &str| verdict.codes.iter().any(|held| held == code);
        let asked = machine
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim();
        let named_so = |row: &Value| {
            row.get("name").and_then(Value::as_str).is_some_and(|name| {
                crate::handlers::entities::own_name(name).eq_ignore_ascii_case(asked)
            })
        };
        let listed: &[Value] = census
            .get("matching_unadmitted")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let unadmitted_total = number("matching_unadmitted_total").unwrap_or(listed.len() as u64);
        let unlisted = unadmitted_total.saturating_sub(listed.len() as u64);
        let state = census.get("state").and_then(Value::as_str);
        let unparsed = number("unparsed_files_total").is_some_and(|count| count > 0)
            || said(coverage::UNPARSED_FILES_LISTED_CODE);
        let partly_parsed = census.get(coverage::PARSED_KEY).and_then(Value::as_str)
            == Some(coverage::PARSED_IN_PART);
        let unmeasured =
            state == Some(coverage::STATE_UNMEASURED) || said(coverage::CENSUS_UNMEASURED_CODE);
        let unread_regions = census
            .get(coverage::UNREAD_REGIONS_KEY)
            .and_then(Value::as_str);
        // The census read every source unit whole, or the search read back
        // what no parse could read and the name is spelled nowhere in it. The
        // reading says so itself, since one from a build that did not record
        // how each unit was parsed cannot.
        let parsed = census.get(coverage::PARSED_KEY).and_then(Value::as_str);
        let nothing_unread = parsed == Some(coverage::PARSED_WHOLE)
            || (partly_parsed && unread_regions == Some(coverage::UNREAD_CLEAR));
        let census_whole = matches!(
            state,
            Some(coverage::STATE_COMPLETE | coverage::STATE_COMPLETE_EXCEPT)
        ) && nothing_unread;
        let spelled_in: Vec<String> = census
            .get(coverage::UNREAD_SPELLED_IN_KEY)
            .and_then(Value::as_array)
            .map(|rows| {
                rows.iter()
                    .filter_map(|row| row.get("module").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let verdict_whole = verdict.certified()
            || (verdict.certified_except()
                && verdict.codes.len() == 1
                && said(coverage::UNADMITTED_LISTED_CODE));
        let set = |value: Option<&Value>| value.and_then(Value::as_bool) == Some(true);
        let own_name_rows_complete =
            set(machine.get(crate::handlers::entities::OWN_NAME_ROWS_COMPLETE_KEY));
        let coverage_cut = set(machine
            .get(crate::edge_coverage::EDGE_COVERAGE_KEY)
            .and_then(|coverage| coverage.get("budget_exhausted")));
        let closed = own_name_rows_complete
            && verdict_whole
            && census_whole
            && !unparsed
            && !unmeasured
            && unlisted == 0
            && total == rows.len() as u64
            && !set(machine.get("truncated"))
            && !coverage_cut;
        Self {
            closed,
            declarations_read: number("declarations_seen"),
            named_exactly: (rows.iter().filter(|row| named_so(row)).count()
                + listed.iter().filter(|row| named_so(row)).count())
                as u64,
            in_reply: rows.len() as u64 + listed.len() as u64,
            unparsed,
            partly_parsed,
            partly_parsed_count: number(coverage::PARSED_IN_PART_COUNT_KEY),
            unread_regions: unread_regions.map(str::to_string),
            spelled_in_count: number(coverage::UNREAD_SPELLING_COUNT_KEY)
                .unwrap_or(spelled_in.len() as u64),
            spelled_in,
            not_read_back: number(coverage::UNREAD_NOT_READ_COUNT_KEY).unwrap_or(0),
            unmeasured,
            unlisted,
        }
    }
}

impl MachineVerdict {
    /// Whether a listing set aside calls around it Kin could not match.
    pub(crate) fn set_aside_calls(&self, op: Operation) -> bool {
        op.is_listing() && self.codes.iter().any(|code| is_call_resolution_code(code))
    }
}

/// The candidates left open or settled for one reference page.
pub(crate) struct ReferenceAccount {
    pub(crate) groups: Vec<Value>,
    pub(crate) left: u64,
    pub(crate) unkeyed: u64,
    pub(crate) other_names: Option<Value>,
    pub(crate) passed: Vec<Value>,
    pub(crate) settled: Vec<Value>,
    pub(crate) settled_entries: u64,
    pub(crate) folded_entries: u64,
    pub(crate) passed_by_rows: u64,
}

impl ReferenceAccount {
    pub(crate) fn read(
        machine: &Map<String, Value>,
        every_row: &[Value],
        rows: &[Value],
        focal: &Value,
        focal_name: &str,
        details: bool,
    ) -> Self {
        let Leftovers {
            mut groups,
            total: left,
            unkeyed,
            other_names,
            mut passed,
            gap_group,
            outside_reads,
            mut settled,
        } = machine
            .get(crate::handlers::entities::CALL_SITE_CANDIDATES_KEY)
            .map(|candidates| left_to_check(&without_proven_sites(candidates, every_row), focal))
            .unwrap_or_default();
        // A short reply counts the coverage gaps by reason instead of listing
        // them: one React answer that found no caller listed 302 groups of them,
        // 112 KB, and they were the same gaps on every reference question asked.
        // Details list them, under the same byte budget as every reply.
        //
        // A counted place is one of two kinds. `settled_entries` are proved to
        // add no function to the answer and take nothing from its verdict.
        // `folded_entries` could each still reach the entity in the one way its
        // reason names, and keep the answer "complete except" them.
        let mut settled_entries = outside_reads;
        let mut folded_entries = 0u64;
        // Value uses the census listed that were settled because the function
        // that passes the value on is already a row.
        let mut passed_by_rows = 0u64;
        let mut left = left;
        if let (false, Some(at)) = (details, gap_group) {
            groups[at] = fold_gap_group(&groups[at], focal_name);
            let entries = groups[at]["retained_entries"].as_u64().unwrap_or(0);
            folded_entries += entries;
            left = left.saturating_sub(entries);
        }
        // So does a site whose kind never held a missed caller. Details list it.
        if !details {
            let proven: std::collections::HashSet<&str> = rows
                .iter()
                .filter(|row| {
                    !matches!(
                        row.get("resolution").and_then(Value::as_str),
                        Some("name_only") | Some(crate::handlers::entities::NAME_UNIQUE_RESOLUTION)
                    )
                })
                .filter_map(|row| row.get("entity_id").and_then(Value::as_str))
                .collect();
            // A method or a field is reached through a value of its owner.
            let member = focal
                .get("kind")
                .and_then(Value::as_str)
                .is_some_and(|kind| {
                    kind.eq_ignore_ascii_case("method") || kind.eq_ignore_ascii_case("field")
                });
            let (proved, counted_open) =
                fold_settled_sites(&mut groups, &mut settled, &proven, focal_name, member);
            settled_entries += proved;
            folded_entries += counted_open;
            left = left.saturating_sub(proved + counted_open);
            // A value handed on by a function that is already a row adds no
            // function to the answer.
            let handed_on = passed.len();
            passed.retain(|site| {
                !site
                    .get("inside_id")
                    .and_then(Value::as_str)
                    .is_some_and(|passer| proven.contains(passer))
            });
            let by_rows = (handed_on - passed.len()) as u64;
            passed_by_rows = by_rows;
            if by_rows > 0 {
                settled_entries += by_rows;
                settled.push(json!({
                    "why": format!(
                        "{} that {} {focal_name} to other code as an argument, each made by a \
                     function that is already a row of this answer, so it adds no function to \
                     the answer. Counted, not listed.",
                        capitalized(&counted(by_rows, "call", "calls")),
                        plural(by_rows, "passes", "pass"),
                    ),
                    "folded_entries": by_rows,
                    GAP_REASONS_KEY: {PASSER_ALREADY_A_ROW: by_rows},
                }));
            }
        }
        Self {
            groups,
            left,
            unkeyed,
            other_names,
            passed,
            settled,
            settled_entries,
            folded_entries,
            passed_by_rows,
        }
    }
}

/// Whether the answer lists any name-only row, passed value or open group.
pub(crate) fn reference_lists_open_places(named: u64, passed_count: u64, groups: &[Value]) -> bool {
    named > 0 || passed_count > 0 || groups.iter().any(|group| !is_folded_group(group))
}

/// Whether the census rules value uses out or every listed use was settled.
pub(crate) fn reference_values_settled(
    machine: &Map<String, Value>,
    passed_count: u64,
    passed_by_rows: u64,
) -> (bool, bool) {
    let values_ruled_out = value_uses_ruled_out(machine);
    let values_settled = !values_ruled_out
        && passed_count == 0
        && census_lists_only_value_uses(machine, passed_by_rows);
    (values_ruled_out, values_settled)
}

/// Counts and closure facts behind a name-search reply.
pub(crate) struct NameSearchAccount {
    pub(crate) total: u64,
    pub(crate) not_held: Vec<Value>,
    pub(crate) folded_attributes: std::collections::BTreeMap<String, u64>,
    pub(crate) folded_total: u64,
    pub(crate) not_held_total: u64,
    /// How many declarations `not_held` lists. The reply may later say rows
    /// that read alike once, with a count, but they are still these many.
    pub(crate) not_held_listed: u64,
    pub(crate) closure: NameClosure,
    pub(crate) artifacts: u64,
}

/// Declarations that read alike once projected, the same name, kind, module,
/// enclosing entity and reason on different lines, are said once with how
/// many there are. Sixteen tests that each declare `validate` inside a
/// `constraint` object are one row with a count of sixteen, not sixteen
/// copies of one sentence. The order of first appearance is kept.
pub(crate) fn said_once(rows: Vec<Value>) -> Vec<Value> {
    let mut said: Vec<Value> = Vec::with_capacity(rows.len());
    let mut at: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for row in rows {
        let key = row.to_string();
        match at.get(&key) {
            Some(&index) => {
                let count = said[index]
                    .get("count")
                    .and_then(Value::as_u64)
                    .unwrap_or(1);
                said[index]["count"] = serde_json::json!(count + 1);
            }
            None => {
                at.insert(key, said.len());
                said.push(row);
            }
        }
    }
    said
}

impl NameSearchAccount {
    pub(crate) fn read(
        machine: &Map<String, Value>,
        verdict: &MachineVerdict,
        rows: &[Value],
        details: bool,
        project_not_held: fn(&Value) -> Value,
    ) -> Self {
        let total = machine
            .get("total_matches")
            .and_then(Value::as_u64)
            .unwrap_or(rows.len() as u64)
            .max(rows.len() as u64);
        let census = machine
            .get("edge_coverage")
            .and_then(|coverage| coverage.get("declarations"))
            .cloned()
            .unwrap_or(Value::Null);
        // An attribute of a class Kin keeps is a value that class holds, never a
        // declaration an answer names: a short reply counts those by class
        // rather than listing them, and details lists them.
        let mut folded_attributes: std::collections::BTreeMap<String, u64> =
            std::collections::BTreeMap::new();
        let not_held: Vec<Value> = census
            .get("matching_unadmitted")
            .and_then(Value::as_array)
            .map(|rows| {
                rows.iter()
                    .filter(|row| {
                        let attribute = row.get("reason").and_then(Value::as_str)
                            == Some(CLASS_ATTRIBUTE_REASON);
                        if attribute && !details {
                            let owner = row
                                .get("owner")
                                .and_then(Value::as_str)
                                .unwrap_or("a class")
                                .to_string();
                            *folded_attributes.entry(owner).or_default() += 1;
                        }
                        !attribute || details
                    })
                    .map(project_not_held)
                    .collect()
            })
            .unwrap_or_default();
        let not_held_listed = not_held.len() as u64;
        let folded_total: u64 = folded_attributes.values().sum();
        let not_held_total = census
            .get("matching_unadmitted_total")
            .and_then(Value::as_u64)
            .unwrap_or(not_held_listed + folded_total)
            .saturating_sub(folded_total);
        let closure = NameClosure::read(machine, verdict, &census, rows, total);

        // The artifacts among the matches: tracked things Kin does not parse,
        // each of which says on its row what Kin holds it as.
        let artifacts = machine
            .get(crate::artifact_entity::NAME_MATCHES_KEY)
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .min(total);
        Self {
            total,
            not_held,
            folded_attributes,
            folded_total,
            not_held_total,
            not_held_listed,
            closure,
            artifacts,
        }
    }
}

/// The override census and its ordered coverage gaps.
pub(crate) struct OverrideCoverage {
    pub(crate) coverage: Value,
    pub(crate) gaps: Vec<Value>,
}

impl OverrideCoverage {
    pub(crate) fn read(block: &Value) -> Self {
        let coverage = block.get("coverage").cloned().unwrap_or_else(|| json!({
            "scope": "recorded_overrides_and_implements",
            "recorded_closure": "unmeasured",
            "nested_definitions": block.get("nested_definitions").cloned().unwrap_or(json!("unmeasured")),
            "repository_completeness": "unmeasured",
        }));
        let mut gaps = Vec::new();
        if block["bounded"] == true || coverage["recorded_closure"] == "bounded" {
            gaps.push(json!({"code":"override_closure_bounded", "action":"report_scope_limit"}));
        }
        if coverage["nested_definitions"] == "unmeasured" {
            gaps.push(
                json!({"code":"nested_definition_census_unmeasured", "action":"report_scope_limit"}),
            );
        }
        if coverage["repository_completeness"] != "complete" {
            gaps.push(json!({"code":"repository_override_coverage_unmeasured", "action":"report_scope_limit"}));
        }
        Self { coverage, gaps }
    }
}

/// Facts read from the projected override block and its delivery page.
pub(crate) struct OverridePageAccount {
    pub(crate) count: usize,
    pub(crate) has_more: bool,
    pub(crate) census_complete: bool,
    pub(crate) checked: u64,
    pub(crate) incomplete: bool,
}

impl OverridePageAccount {
    pub(crate) fn read(block: &Value, machine: &Map<String, Value>, envelope: &Value) -> Self {
        let count = block["methods"].as_array().map_or(0, Vec::len);
        let has_more = machine.get("next_cursor").is_some_and(|v| !v.is_null())
            || envelope.pointer("/page/has_more").and_then(Value::as_bool) == Some(true);
        // Complete only when the repository census accounted for every recorded
        // subclass and class header and nothing else limits the block.
        let census_complete = !has_more
            && block
                .pointer("/coverage/repository_completeness")
                .and_then(Value::as_str)
                == Some("complete")
            && block
                .get("gaps")
                .and_then(Value::as_array)
                .is_none_or(Vec::is_empty);
        let checked = block
            .pointer("/coverage/recorded_subclasses_checked")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let incomplete = block
            .pointer("/coverage/repository_completeness")
            .and_then(Value::as_str)
            == Some("incomplete");
        Self {
            count,
            has_more,
            census_complete,
            checked,
            incomplete,
        }
    }
}

// ---------------------------------------------------------------------------
// The account of an answer
// ---------------------------------------------------------------------------

/// The verdict word an answer earns for the question its tool asked. The
/// plain reply's word is this, rendered; the machine reply carries it as
/// `_kin.verdict.answer.standing`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AnswerStanding {
    /// Whole and proven.
    Complete,
    /// Whole except places the reply lists or counts below, each with why.
    CompleteExceptListed,
    /// Whole except uses through a value or a name the reply does not list.
    CompleteExceptIndirect,
    /// Something that could change the answer is still open.
    Partial,
}

impl AnswerStanding {
    /// The machine reply's word for it.
    pub(crate) fn wire(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::CompleteExceptListed => "complete_except_listed",
            Self::CompleteExceptIndirect => "complete_except_indirect",
            Self::Partial => "partial",
        }
    }

    /// The plain reply's word for it.
    pub(crate) fn word(self) -> &'static str {
        match self {
            Self::Complete => crate::plain_reply::COMPLETE,
            Self::CompleteExceptListed => crate::plain_reply::COMPLETE_EXCEPT,
            Self::CompleteExceptIndirect => crate::plain_reply::COMPLETE_EXCEPT_INDIRECT,
            Self::Partial => crate::plain_reply::PARTIAL,
        }
    }

    /// How far it is from whole, for the answer made of sections, whose word
    /// is its least whole section's.
    fn distance(self) -> u8 {
        match self {
            Self::Complete => 0,
            Self::CompleteExceptListed => 1,
            Self::CompleteExceptIndirect => 2,
            Self::Partial => 3,
        }
    }
}

/// What decided an answer's standing, which is what the plain reply's sentence
/// says beside the word. It never decides the word itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Basis {
    /// The operation's own standing under the verdict, with nothing further
    /// to weigh.
    Verdict,
    /// A stored receiver chain, recorded whole or not.
    Receiver,
    /// An override census.
    Overrides,
    /// An answer made of one section per entity of a name.
    Sections,
    /// A page of a reference answer that carries rows without its focal.
    RefContinued,
    /// Every use is a row, and nothing is left to check.
    RefWhole,
    /// Places are listed to check: name-only rows, calls that pass the
    /// entity on, or groups of sites.
    RefListedOpen,
    /// Every place left is folded into a count by its reason.
    RefFolded,
    /// Every place that spelled the name and is not a row is settled.
    RefSettled,
    /// What is left is counted beside the answer and is no place to check.
    RefCounted,
    /// Uses through a value or a built name could add callers, and the
    /// reply does not list where.
    RefIndirect,
    /// Calls and values were measured; only non-call mentions were not.
    RefNonCallUnmeasured,
    /// Something that could add a use is still open.
    RefOpen,
    /// A name search that is open because of what the verdict says.
    NameOpen,
    /// A name search that could miss a declaration in source no parse read.
    NameUnread,
    /// No entity has the name; declarations Kin keeps no entity for do.
    NameNotHeldOnly,
    /// No declaration has the name.
    NameNothing,
    /// Declarations that have the name, with any Kin keeps no entity for.
    NameRows,
    /// A page of a callers walk past every reach row.
    WalkDisplayPage,
    /// A finished reach that lists or counts the places it could not follow.
    WalkReachListed,
    /// A finished reach with nothing it could not follow.
    WalkReachWhole,
    /// A walk whose only open places are its fully listed frontier.
    WalkFrontierListed,
    /// A closed reach whose only open places are the value holders it lists.
    WalkValuePlaces,
    /// A walk or listing that is whole.
    WalkWhole,
    /// A walk whose open places are counted beside it.
    WalkCounted,
    /// A usage answer whose entities could also be reached indirectly.
    WalkIndirect,
    /// An answer whose only exception speaks to another question.
    WalkScope,
    /// A walk or listing with something still open.
    WalkOpen,
    /// An outline whose member coverage could not be established.
    SourceMemberGap,
    /// Places whose text names an unparsed entity, matched by name.
    NamedIn,
}

/// How one answer stands for its question, decided once from the machine
/// reply. The plain reply renders it; the machine reply carries it.
#[derive(Debug, Clone)]
pub(crate) struct Account {
    pub(crate) standing: AnswerStanding,
    pub(crate) basis: Basis,
    /// Whether the answer carries a verdict word at all. A reply no verdict
    /// was computed for says only what it holds.
    pub(crate) judged: bool,
    /// The verdict codes still standing against this answer's question.
    pub(crate) open: Vec<String>,
}

impl Account {
    fn new(standing: AnswerStanding, basis: Basis, judged: bool, open: Vec<String>) -> Self {
        Self {
            standing,
            basis,
            judged,
            open,
        }
    }

    /// The plain word this answer is rendered with, or `None` when it carries
    /// none.
    pub(crate) fn word(&self) -> Option<&'static str> {
        self.judged.then(|| self.standing.word())
    }

    /// `_kin.verdict.answer` on a machine reply.
    pub(crate) fn to_value(&self) -> Value {
        let mut value = Map::new();
        value.insert("standing".into(), Value::from(self.standing.wire()));
        if !self.open.is_empty() {
            value.insert("open".into(), Value::from(self.open.clone()));
        }
        Value::Object(value)
    }
}

/// The account of one answer, read from its machine reply: `machine` without
/// its envelope, the envelope `_kin` it carried, and for a trace page the
/// complete readings it continues.
pub(crate) fn account(
    tool: &str,
    machine: &Map<String, Value>,
    envelope: &Value,
    semantic: Option<&Map<String, Value>>,
) -> Account {
    let verdict = MachineVerdict::read(envelope);
    let judged = verdict.state.is_some();
    if let Some(proof) = machine.get("receiver_proof") {
        let recorded = proof["status"] == "recorded" && proof["resolution"] != "name_only";
        let open: Vec<String> = verdict
            .open_codes(Operation::Lookup)
            .into_iter()
            .map(str::to_string)
            .collect();
        let standing = if recorded && open.is_empty() {
            AnswerStanding::Complete
        } else {
            AnswerStanding::Partial
        };
        return Account::new(standing, Basis::Receiver, true, open);
    }
    if tool == crate::plain_reply::STATUS_TOOL {
        return Account::new(AnswerStanding::Complete, Basis::Verdict, false, Vec::new());
    }
    // What names an unparsed entity in its text is matched by name, never a
    // resolved use, so such an answer is partial whether it lists places or
    // none.
    if Operation::of(tool) == Operation::Usage
        && crate::artifact_entity::mentions::is_named_in_reply(machine)
    {
        return Account::new(
            AnswerStanding::Partial,
            Basis::NamedIn,
            judged,
            open_of(&verdict, Operation::Usage),
        );
    }
    if crate::plain_reply::is_override_view(machine, envelope) {
        let open: Vec<String> = verdict
            .open_codes(Operation::Lookup)
            .into_iter()
            .map(str::to_string)
            .collect();
        let standing =
            if crate::plain_reply::override_census_complete(machine, envelope) && open.is_empty() {
                AnswerStanding::Complete
            } else {
                AnswerStanding::Partial
            };
        return Account::new(standing, Basis::Overrides, true, open);
    }
    if let Some(sections) = machine
        .get(crate::handlers::entities::AMBIGUOUS_CANDIDATES_KEY)
        .and_then(Value::as_array)
    {
        return sections_account(&verdict, sections, envelope);
    }
    if crate::plain_reply::is_reference_answer(machine, tool) {
        return reference_account(machine, envelope, envelope);
    }
    if tool == crate::handlers::lexical::TOOL_NAME && machine.contains_key("literal") {
        return text_account(&verdict, machine);
    }
    if machine.contains_key(crate::file_coordinate_query::KEY)
        && crate::file_coordinate_query::recognizes(tool, &Value::Object(machine.clone()))
    {
        let gap = matches!(
            machine[crate::file_coordinate_query::KEY]["state"].as_str(),
            Some("module_unrecorded" | "coordinate_unrecorded")
        );
        let partial = gap || verdict.standing(Operation::Coordinate) == Standing::Partial;
        return Account::new(
            if partial {
                AnswerStanding::Partial
            } else {
                AnswerStanding::Complete
            },
            Basis::Verdict,
            judged,
            open_of(&verdict, Operation::Coordinate),
        );
    }
    let op = Operation::of(tool);
    match op {
        Operation::NameSearch if machine.get("results").is_some_and(Value::is_array) => {
            name_account(&verdict, machine)
        }
        Operation::Ranked if machine.get("entities").is_some_and(Value::is_array) => {
            plain_or_partial(&verdict, op, Basis::Verdict)
        }
        Operation::Source if crate::plain_reply::is_source_reply(machine) => {
            if machine.get("semantic_gap").and_then(Value::as_str)
                == Some(crate::container_shape::MODULE_MEMBER_GAP)
            {
                Account::new(
                    AnswerStanding::Partial,
                    Basis::SourceMemberGap,
                    judged,
                    open_of(&verdict, op),
                )
            } else {
                plain_or_partial(&verdict, op, Basis::Verdict)
            }
        }
        // A history read always says how it stands: it is the recorded
        // changes, delivered or not.
        Operation::History if machine.get("changes").is_some_and(Value::is_array) => Account {
            judged: true,
            ..plain_or_partial(&verdict, op, Basis::Verdict)
        },
        _ => walk_account(tool, machine, envelope, op, semantic),
    }
}

/// The codes standing against `op`'s answer, as the account carries them.
fn open_of(verdict: &MachineVerdict, op: Operation) -> Vec<String> {
    verdict
        .open_codes(op)
        .into_iter()
        .map(str::to_string)
        .collect()
}

/// An answer that is whole unless something open speaks to its question:
/// a ranking, a read of an entity's code or history.
fn plain_or_partial(verdict: &MachineVerdict, op: Operation, basis: Basis) -> Account {
    let standing = match verdict.standing(op) {
        Standing::Partial => AnswerStanding::Partial,
        _ => AnswerStanding::Complete,
    };
    Account::new(
        standing,
        basis,
        verdict.state.is_some(),
        open_of(verdict, op),
    )
}

/// Whether a text search over held text could have missed the text: some
/// parsed module or unparsed entity could not be read back, an occurrence
/// sits in no entity, a module's spans or parse are not whole, a held member
/// has no address, or source authority was not available.
pub(crate) fn held_text_open(coverage: &Value) -> bool {
    let count = |key: &str| coverage.get(key).and_then(Value::as_u64).unwrap_or(0);
    [
        "modules_unread",
        "artifacts_unread",
        "occurrences_outside_entities",
        "modules_without_complete_spans",
        "modules_with_unverified_parse",
        "unaddressable",
    ]
    .iter()
    .any(|key| count(key) > 0)
        || coverage
            .get("authority_unavailable")
            .and_then(Value::as_bool)
            == Some(true)
}

/// A text search: whole over what it read, partial when held text could have
/// hidden it or the graph is not level with the code.
fn text_account(verdict: &MachineVerdict, machine: &Map<String, Value>) -> Account {
    let unread = machine
        .get(crate::handlers::lexical::HELD_TEXT_KEY)
        .and_then(Value::as_bool)
        == Some(true)
        && held_text_open(
            machine
                .get(crate::handlers::lexical::HELD_TEXT_COVERAGE_KEY)
                .unwrap_or(&Value::Null),
        );
    let open = open_of(verdict, Operation::Lookup);
    let standing = if unread || !open.is_empty() {
        AnswerStanding::Partial
    } else {
        AnswerStanding::Complete
    };
    Account::new(standing, Basis::Verdict, verdict.state.is_some(), open)
}

/// One answer per entity of a name: its word is its least whole section's,
/// each section graded as the reference answer it is. A section that carries
/// no verdict of its own is graded by the answer's, which then stands beside
/// the sections' words, as it does when there are no sections at all.
fn sections_account(verdict: &MachineVerdict, sections: &[Value], envelope: &Value) -> Account {
    let whole = match verdict.standing(Operation::Usage) {
        Standing::Complete => AnswerStanding::Complete,
        Standing::CompleteExcept if only_counted(verdict) => AnswerStanding::Complete,
        Standing::CompleteExcept => AnswerStanding::CompleteExceptListed,
        Standing::Partial => AnswerStanding::Partial,
    };
    let mut composed: Option<AnswerStanding> = None;
    let mut unjudged = sections.is_empty();
    let mut compose = |standing: AnswerStanding| {
        composed = Some(match composed {
            Some(held) if held.distance() >= standing.distance() => held,
            _ => standing,
        });
    };
    for section in sections {
        let Value::Object(section) = section else {
            continue;
        };
        let mut section = section.clone();
        let section_envelope = section
            .remove(crate::envelope::ENVELOPE_KEY)
            .unwrap_or(Value::Null);
        if section_envelope.get("verdict").is_none() {
            unjudged = true;
            continue;
        }
        if let Some(qualified) = section.get("qualified_name").cloned() {
            if let Some(focal) = section
                .get_mut("focal_entity")
                .and_then(Value::as_object_mut)
            {
                focal.entry("qualified_name").or_insert(qualified);
            }
        }
        for key in crate::handlers::entities::SECTION_LABEL_KEYS {
            section.remove(key);
        }
        compose(reference_account(&section, &section_envelope, envelope).standing);
    }
    if unjudged {
        compose(whole);
    }
    Account::new(
        composed.unwrap_or(whole),
        Basis::Sections,
        verdict.state.is_some(),
        open_of(verdict, Operation::Usage),
    )
}

/// Whether an open indirect use stands against a reference answer: calls
/// through a value nobody measured, or values that escape, unless every value
/// use was ruled out or settled by the rows.
fn indirect_use_open(
    verdict: &MachineVerdict,
    values_ruled_out: bool,
    values_settled: bool,
) -> bool {
    use kin_model::call_site_reading as reading;
    let said = |code: &str| verdict.codes.iter().any(|held| held == code);
    (said(reading::VALUE_CALLS_UNMEASURED) && !(values_ruled_out || values_settled))
        || (said(reading::ESCAPE_RESIDUE_LISTED) && !values_settled)
}

/// A reference answer, graded on what it holds once its places are counted
/// the way a short reply counts them: the word never depends on how much
/// detail a reply chose to show.
pub(crate) fn reference_account(
    machine: &Map<String, Value>,
    envelope: &Value,
    scope: &Value,
) -> Account {
    let verdict = MachineVerdict::read(envelope);
    let held = ReferenceRows::read(machine, envelope, scope);
    let places = ReferenceAccount::read(
        machine,
        &held.every_row,
        &held.rows,
        &held.focal,
        &held.focal_name(),
        false,
    );
    let named = held.by_name_count();
    let passed = places.passed.len() as u64;
    let listed_open = reference_lists_open_places(named, passed, &places.groups);
    let (values_ruled_out, values_settled) =
        reference_values_settled(machine, passed, places.passed_by_rows);
    let standing = verdict.standing(Operation::Usage);
    let counted_or_folded = only_counted_or_folded(&verdict, values_ruled_out, values_settled);
    use AnswerStanding as A;
    let (standing, basis) = match standing {
        // Sites a sweep could not decide are never part of a whole answer:
        // listed, the answer is complete except them; not listed, partial.
        _ if undecided_unlisted(&verdict, machine) => (A::Partial, Basis::RefOpen),
        _ if held.full_focal_missing => (
            match standing {
                Standing::Complete | Standing::CompleteExcept if listed_open => {
                    A::CompleteExceptListed
                }
                Standing::Complete => A::Complete,
                Standing::CompleteExcept => A::CompleteExceptListed,
                Standing::Partial => A::Partial,
            },
            Basis::RefContinued,
        ),
        Standing::Complete if !listed_open || verdict.certified() => (A::Complete, Basis::RefWhole),
        Standing::Complete | Standing::CompleteExcept if listed_open => {
            (A::CompleteExceptListed, Basis::RefListedOpen)
        }
        Standing::Complete | Standing::CompleteExcept
            if places.folded_entries > 0 && counted_or_folded =>
        {
            (A::CompleteExceptListed, Basis::RefFolded)
        }
        Standing::Complete | Standing::CompleteExcept
            if places.settled_entries > 0 && counted_or_folded =>
        {
            (A::Complete, Basis::RefSettled)
        }
        Standing::Complete | Standing::CompleteExcept if only_counted(&verdict) => {
            (A::Complete, Basis::RefCounted)
        }
        // Uses through a value or a built name the reply does not list.
        Standing::Complete | Standing::CompleteExcept
            if indirect_use_open(&verdict, values_ruled_out, values_settled) =>
        {
            (A::CompleteExceptIndirect, Basis::RefIndirect)
        }
        // Nothing indirect is open. What is left is counted below, or is a
        // code whose places this reply already settled.
        Standing::Complete | Standing::CompleteExcept if places.folded_entries > 0 => {
            (A::CompleteExceptListed, Basis::RefFolded)
        }
        Standing::Complete | Standing::CompleteExcept => (A::Complete, Basis::RefCounted),
        Standing::Partial if only_non_call_unmeasured(&verdict) => {
            (A::Partial, Basis::RefNonCallUnmeasured)
        }
        Standing::Partial => (A::Partial, Basis::RefOpen),
    };
    Account::new(
        standing,
        basis,
        verdict.state.is_some(),
        open_of(&verdict, Operation::Usage),
    )
}

/// A name search, graded as the closure work reads its census: whole when
/// every place a declaration could be was read, partial when source no parse
/// read could hold one, and complete except the declarations it lists or
/// counts when Kin keeps no entity for some.
fn name_account(verdict: &MachineVerdict, machine: &Map<String, Value>) -> Account {
    use crate::declaration_coverage as coverage;
    let op = Operation::NameSearch;
    let judged = verdict.state.is_some();
    let open = open_of(verdict, op);
    if verdict.standing(op) == Standing::Partial {
        return Account::new(AnswerStanding::Partial, Basis::NameOpen, judged, open);
    }
    let census = machine
        .get("edge_coverage")
        .and_then(|coverage| coverage.get("declarations"))
        .cloned()
        .unwrap_or(Value::Null);
    let number = |key: &str| census.get(key).and_then(Value::as_u64);
    // Source held as bytes and never parsed, or parsed around an error and
    // either not read back or read back spelling the name: a declaration of
    // the name could be there, and the search cannot say it is not.
    let unparsed = number("unparsed_files_total").is_some_and(|count| count > 0)
        || verdict
            .codes
            .iter()
            .any(|code| code == coverage::UNPARSED_FILES_LISTED_CODE);
    let partly_parsed =
        census.get(coverage::PARSED_KEY).and_then(Value::as_str) == Some(coverage::PARSED_IN_PART);
    let unread_clear = census
        .get(coverage::UNREAD_REGIONS_KEY)
        .and_then(Value::as_str)
        == Some(coverage::UNREAD_CLEAR);
    if unparsed || (partly_parsed && !unread_clear) {
        return Account::new(AnswerStanding::Partial, Basis::NameUnread, judged, open);
    }
    let rows = machine
        .get("results")
        .and_then(Value::as_array)
        .map_or(0, Vec::len) as u64;
    let total = machine
        .get("total_matches")
        .and_then(Value::as_u64)
        .unwrap_or(rows)
        .max(rows);
    let not_held = number("matching_unadmitted_total").unwrap_or_else(|| {
        census
            .get("matching_unadmitted")
            .and_then(Value::as_array)
            .map_or(0, Vec::len) as u64
    });
    let (standing, basis) = match (total, not_held) {
        (0, 0) => (AnswerStanding::Complete, Basis::NameNothing),
        (0, _) => (AnswerStanding::CompleteExceptListed, Basis::NameNotHeldOnly),
        (_, 0) => (AnswerStanding::Complete, Basis::NameRows),
        _ => (AnswerStanding::CompleteExceptListed, Basis::NameRows),
    };
    Account::new(standing, basis, judged, open)
}

/// A walk along calls, an impact answer, a context or a neighbourhood:
/// graded on the walk's own limits and on what its reach reading lists.
fn walk_account(
    tool: &str,
    machine: &Map<String, Value>,
    envelope: &Value,
    op: Operation,
    semantic: Option<&Map<String, Value>>,
) -> Account {
    let mut verdict = MachineVerdict::read(envelope);
    let judged = verdict.state.is_some();
    let mut machine = machine.clone();
    if tool == TRACE_TOOL {
        if let Some(question) = envelope.get("trace_question").and_then(Value::as_object) {
            for key in ["direction", "depth", "limit_per_step", "focal_name"] {
                if let Some(value) = question.get(key) {
                    machine.entry(key).or_insert_with(|| value.clone());
                }
            }
        }
    }
    let mut normalized = (tool == TRACE_TOOL).then(|| semantic.unwrap_or(&machine).clone());
    let reach = normalized
        .as_ref()
        .and_then(crate::trace_reach::answer_view);
    if let (Some(reach), Some(normalized)) = (&reach, normalized.as_mut()) {
        normalized.insert("reach".into(), Value::Object(reach.clone()));
        set_aside_reach_itemized(&mut verdict, reach);
    }
    let walked = normalized.as_ref().unwrap_or(&machine);
    let limits = if tool == TRACE_TOOL {
        crate::plain_reply::trace_limits(walked)
    } else {
        Vec::new()
    };
    let focal_missing = op == Operation::Context
        && crate::plain_reply::context_focal_body_missing(&machine).is_some();
    let value_qualification_open =
        tool == TRACE_TOOL && trace_value_qualification_open(&verdict, walked);
    // The walk's only limit is its frontier, listed place by place.
    let frontier_listed = (tool == TRACE_TOOL && limits.len() == 1 && !focal_missing)
        .then(|| trace_frontier_listed(walked))
        .flatten()
        .filter(|_| {
            reach_frontier_total(walked) > 0
                && !verdict.certified()
                && verdict.open_codes(op).is_empty()
                && !value_qualification_open
        });
    let reach_settled = (tool == TRACE_TOOL && !focal_missing)
        .then_some(reach.as_ref())
        .flatten()
        .filter(|reach| reach_finished(reach) && verdict.open_codes(op).is_empty())
        .map(reach_lists_open_places);
    let limited = frontier_listed.is_none() && reach_settled.is_none() && !limits.is_empty();
    let standing = if limited || focal_missing {
        Standing::Partial
    } else {
        match reach_settled {
            Some(true) => Standing::CompleteExcept,
            Some(false) => Standing::Complete,
            None => verdict.standing(op),
        }
    };
    let value_places = (tool == TRACE_TOOL)
        .then(|| trace_value_places(walked))
        .flatten()
        .filter(|places| !(places.holders.is_empty() && places.members.is_empty()));
    let closed_but_values = value_places.is_some()
        && !limited
        && !focal_missing
        && standing != Standing::Partial
        && reach
            .as_ref()
            .and_then(|reach| reach.get("state"))
            .and_then(Value::as_str)
            == Some("closed");
    let listed = !crate::plain_reply::is_empty_answer(&machine);
    let display_page =
        tool == TRACE_TOOL && crate::plain_reply::is_display_page(&machine, envelope);
    use AnswerStanding as A;
    let (standing, basis) = match standing {
        _ if display_page => (A::Partial, Basis::WalkDisplayPage),
        _ if reach_settled == Some(true) && frontier_listed.is_none() && !closed_but_values => {
            (A::CompleteExceptListed, Basis::WalkReachListed)
        }
        _ if reach_settled == Some(false) => (A::Complete, Basis::WalkReachWhole),
        _ if frontier_listed.is_some() => (A::CompleteExceptListed, Basis::WalkFrontierListed),
        _ if closed_but_values => (A::CompleteExceptListed, Basis::WalkValuePlaces),
        Standing::Complete => (A::Complete, Basis::WalkWhole),
        Standing::CompleteExcept if only_counted(&verdict) => (A::Complete, Basis::WalkCounted),
        Standing::CompleteExcept if op.analyses_usage() && listed => {
            (A::CompleteExceptIndirect, Basis::WalkIndirect)
        }
        Standing::CompleteExcept => (A::Complete, Basis::WalkScope),
        Standing::Partial => (A::Partial, Basis::WalkOpen),
    };
    // Entities that reach it only through a call matched by a bare name are
    // listed apart from the members, so a whole answer is complete except
    // them.
    let by_name = !display_page
        && reach
            .as_ref()
            .and_then(|reach| reach.get(crate::trace_reach::NAME_ONLY_TOTAL_KEY))
            .and_then(Value::as_u64)
            .is_some_and(|count| count > 0);
    let standing = if by_name && standing == A::Complete {
        A::CompleteExceptListed
    } else {
        standing
    };
    // A display page grades nothing: the first page's verdict stands.
    Account::new(
        standing,
        basis,
        judged && !display_page,
        open_of(&verdict, op),
    )
}

/// How many places a walk's reach reading could not follow.
fn reach_frontier_total(machine: &Map<String, Value>) -> u64 {
    machine
        .get("reach")
        .or_else(|| {
            machine
                .get("readings")?
                .as_array()?
                .iter()
                .find(|reading| reading.get("key").and_then(Value::as_str) == Some("reach"))?
                .get("value")
        })
        .and_then(|reach| reach.get("frontier_total"))
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

/// The codes a walk's reach reading sets aside: caps on the display chain,
/// which do not bound the closure, cuts of rows that arrived, and codes that
/// restate a place a finished reach already lists or counts.
pub(crate) fn set_aside_reach_itemized(verdict: &mut MachineVerdict, reach: &Map<String, Value>) {
    let delivered = crate::trace_reach::rows_delivered(reach);
    let finished = reach_finished(reach);
    verdict.codes.retain(|code| {
        let chain_cap = matches!(
            code.as_str(),
            "trace_spine_clipped" | "trace_walk_truncated" | "trace_walk_degraded"
        );
        let delivered_cut =
            delivered && matches!(code.as_str(), "answer_truncated" | "counts_are_a_floor");
        let itemized = finished && reach_itemizes(code);
        !(chain_cap || delivered_cut || itemized)
    });
}

/// The rows a reference answer holds, read the same way by its account and
/// by the plain reply: the entity it is about, every row, and the rows that
/// are not a module that only imports it.
pub(crate) struct ReferenceRows {
    pub(crate) focal: Value,
    pub(crate) full_focal_missing: bool,
    pub(crate) every_row: Vec<Value>,
    pub(crate) module_rows: Vec<Value>,
    pub(crate) rows: Vec<Value>,
    /// Every use the answer holds, modules that only import it included.
    pub(crate) total: u64,
}

impl ReferenceRows {
    pub(crate) fn read(machine: &Map<String, Value>, envelope: &Value, scope: &Value) -> Self {
        let full_focal = machine.get("focal_entity").cloned().unwrap_or(Value::Null);
        let full_focal_missing = full_focal.is_null();
        // A frozen label qualifies method/name evidence even when the complete
        // focal is a later reading. It does not change this page's delivery state.
        let focal = if full_focal_missing {
            envelope
                .get("reference_focal")
                .or_else(|| scope.get("reference_focal"))
                .cloned()
                .unwrap_or(Value::Null)
        } else {
            full_focal
        };
        let every_row: Vec<Value> = machine
            .get("references")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let total = machine
            .get("total_upstream")
            .and_then(Value::as_u64)
            .unwrap_or(every_row.len() as u64);
        // A module that imports or re-exports the entity is listed apart from
        // the functions that use it and counted apart on the answer line. Only
        // when this page holds every row, so the two counts are of the whole
        // answer, and never for a module, whose importers are its answer.
        let focal_is_module = focal
            .get("kind")
            .and_then(Value::as_str)
            .is_some_and(|kind| kind.eq_ignore_ascii_case("module"));
        let (module_rows, rows): (Vec<Value>, Vec<Value>) =
            if !focal_is_module && every_row.len() as u64 == total {
                every_row
                    .iter()
                    .cloned()
                    .partition(crate::plain_reply::is_module_level_import)
            } else {
                (Vec::new(), every_row.clone())
            };
        Self {
            focal,
            full_focal_missing,
            every_row,
            module_rows,
            rows,
            total,
        }
    }

    pub(crate) fn focal_name(&self) -> String {
        self.focal
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("this entity")
            .to_string()
    }

    /// Rows matched by the name alone, which a reply lists apart.
    pub(crate) fn by_name_count(&self) -> u64 {
        self.rows.iter().filter(|row| is_name_only_row(row)).count() as u64
    }
}

/// Whether a reference row was matched by the name alone.
pub(crate) fn is_name_only_row(row: &Value) -> bool {
    matches!(
        row.get("resolution").and_then(Value::as_str),
        Some("name_only") | Some(crate::handlers::entities::NAME_UNIQUE_RESOLUTION)
    )
}

/// The verdict code for reference sites a sweep could not decide after
/// repeated passes over unchanged code, when the answer lists each one.
pub(crate) const UNDECIDED_SITES_LISTED: &str = "reference_sites_undecided_listed";

/// The verdict code for such sites when the answer can only count them.
pub(crate) const UNDECIDED_SITES: &str = "reference_sites_undecided";

/// Where a reference answer reports the sites a sweep could not decide:
/// `{"count": N, "sites": [...]}` beside its rows.
pub(crate) const UNDECIDED_SITES_POINTER: &str = "/undecided_reference_sites";

/// Whether the verdict names listed undecided sites that this reply does not
/// list whole: none listed, or fewer than it counted. Such an answer cannot be
/// complete except them, so it is partial.
fn undecided_unlisted(verdict: &MachineVerdict, machine: &Map<String, Value>) -> bool {
    if !verdict
        .codes
        .iter()
        .any(|code| code == UNDECIDED_SITES_LISTED)
    {
        return false;
    }
    let block = Value::Object(machine.clone());
    let Some(undecided) = block.pointer(UNDECIDED_SITES_POINTER) else {
        return true;
    };
    let listed = undecided
        .get("sites")
        .and_then(Value::as_array)
        .map_or(0, Vec::len) as u64;
    let count = undecided
        .get("count")
        .and_then(Value::as_u64)
        .unwrap_or(listed);
    listed == 0 || listed < count
}

/// Keys under which a plain reply lists or counts the places an answer is
/// complete except.
const LISTING_KEYS: [&str; 8] = [
    "undecided_reference_sites",
    crate::plain_reply::LEFT_TO_CHECK_KEY,
    crate::plain_reply::NAME_ONLY_KEY,
    crate::plain_reply::PASSED_TO_CALLS_KEY,
    "not_held",
    "not_held_attributes",
    "answers",
    "reach",
];

/// What is wrong with a plain reply for the account it was rendered from:
/// a word that is not the account's, a "listed below" word over a reply that
/// lists nothing, or an "indirect uses" word with no indirect use open.
/// Empty when the reply renders its account faithfully.
pub(crate) fn breaches(account: &Account, plain: &Value) -> Vec<String> {
    let mut found = Vec::new();
    let word = plain.pointer("/_kin/verdict").and_then(Value::as_str);
    if word != account.word() {
        found.push(format!(
            "the reply's word {word:?} is not the account's {:?}",
            account.word()
        ));
    }
    if word == Some(crate::plain_reply::COMPLETE_EXCEPT)
        && account.basis != Basis::RefContinued
        && !LISTING_KEYS.iter().any(|key| plain.get(*key).is_some())
    {
        found.push("the reply says complete except the sites listed below and lists none".into());
    }
    if word == Some(crate::plain_reply::COMPLETE_EXCEPT_INDIRECT)
        && !matches!(account.basis, Basis::RefIndirect | Basis::WalkIndirect)
    {
        found.push("the reply says complete except indirect uses with none open".into());
    }
    found
}

/// Attach `_kin.verdict.answer` to a machine reply that carries a verdict and
/// is delivered whole, computed from the same reply the plain renderer reads.
/// Whether a page's accounting says it holds its whole answer: delivery is
/// complete, nothing more follows, and each count it returned is the total.
fn holds_whole_answer(page: &Value) -> bool {
    let Some(page) = page.as_object() else {
        return false;
    };
    let mut returned = page
        .iter()
        .filter_map(|(key, value)| Some((key.strip_prefix("returned_")?, value)))
        .peekable();
    page.get("complete").and_then(Value::as_bool) == Some(true)
        && page.get("has_more").and_then(Value::as_bool) != Some(true)
        && returned.peek().is_some()
        && returned.all(|(counted, value)| page.get(&format!("total_{counted}")) == Some(value))
}

pub(crate) fn attach_answer(tool: &str, payload: &mut Value) {
    let Some(object) = payload.as_object() else {
        return;
    };
    let Some(envelope) = object.get(crate::envelope::ENVELOPE_KEY) else {
        return;
    };
    if envelope.get("verdict").and_then(Value::as_object).is_none() {
        return;
    }
    // A paged answer's verdict is about the whole frozen answer while the
    // page holds part of it; its account is read page by page. One page that
    // returned every row and step it counts is the whole answer.
    let paged = envelope
        .get("page")
        .is_some_and(|page| !holds_whole_answer(page))
        || ["next_cursor", "answer_next_cursor", "evidence_next_cursor"]
            .iter()
            .any(|key| object.get(*key).is_some_and(|cursor| !cursor.is_null()));
    if paged {
        return;
    }
    let envelope = envelope.clone();
    let mut machine = object.clone();
    machine.remove(crate::envelope::ENVELOPE_KEY);
    // A reply that is no answer, such as a health report, is graded by none.
    let account = account(tool, &machine, &envelope, None);
    if !account.judged {
        return;
    }
    let answer = account.to_value();
    if let Some(verdict) = payload
        .pointer_mut("/_kin/verdict")
        .and_then(Value::as_object_mut)
    {
        verdict.insert("answer".into(), answer);
    }
}
