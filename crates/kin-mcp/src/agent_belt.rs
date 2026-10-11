// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Short tool descriptions and trimmed input schemas for the `agent-default`
//! profile.
//!
//! The registered descriptions in [`crate::tools`] are written for a reader with
//! room to read them. Measured on 2026-09-02, the `agent-default` profile's
//! `tools/list` is 82,262 bytes over 20 tools: 47,739 characters of description
//! and 30,456 bytes of input schema, roughly 20,500 tokens spent before the
//! model has asked anything. `semantic_locate` alone is 5,375 characters and
//! advertises thirteen input properties; `trace_data_flow` is 6,823. A 7.5B model
//! steered by that belt spent its budget on the wrong tools.
//!
//! So `agent-default` gets a second, shorter form of each description, and a
//! keep-list naming the input properties an agent actually needs. The `full`
//! profile is untouched and still serves every word, and so are the `benchmark`
//! and `context-bench` profiles, whose payload bytes are part of a citable
//! result and must not move because a description was rewritten.
//!
//! Measured the same way afterwards, with the acceptance exception below in
//! place, the served profile is 33,464 bytes over 21 tools: 6,188 characters of
//! description and 27,276 bytes of input schema. The descriptions are where the
//! win is, 6,188 against 47,739. The schemas keep most of their bytes on
//! purpose, because a property the shipped proofs or the acceptance suite grade
//! has to stay advertised; [`schema_keep_lists`] names that exception and the
//! checks that own it.
//!
//! Halved again on 2026-09-22. An eager client re-sends the whole served list on
//! every request, and in the corrected rerun pilot the `agent-query` list was
//! about 3,500 tokens a request, nearly all of it parameter prose. Every served
//! description on `agent-default` and `agent-query`, tool and parameter, nested
//! transaction contract included, is now at most half its bytes at 97c719c8d,
//! and no served parameter was dropped: 3,235 bytes of tool description became
//! 1,512 and 11,333 of schema description became 4,388 on `agent-default`.
//! `every_served_description_is_at_most_half_its_2026_09_22_base` holds each
//! one against the measured base.
//!
//! Cut again on 2026-09-28, this time the schemas. At bfb744592 a bare
//! `kin mcp start` served `agent-default` as 47,538 bytes of `tools/list` over
//! 22 tools, and 29,866 of them were `kin_transaction_stage` and
//! `kin_transaction_commit`: each carried the whole registered operation
//! contract, six branches with the entity source base schema restated in four
//! of them. An eager client resends that on every turn, about 12,000 prompt
//! tokens before the question, and on a local 27B model prefill time grows with
//! it. Now the operation shape is served once, on `kin_mutate`, in the form a
//! caller needs for the common path; stage and commit say they take the same
//! operations. The forms a caller needs only when it gets them wrong live in
//! the refusal: every malformed operation is answered with
//! [`crate::session::ACCEPTED_OPERATION_SHAPES`], which spells out each one.
//! Property prose that restated a name, an enum or a bound went, and so did
//! four properties no agent needs to see: two deprecated `*_file` pins, the
//! deprecated `files` input, and a one-value enum. No tool left the profile, and
//! a hidden property is still accepted. [`crate::tools::
//! AGENT_DEFAULT_LIST_CEILING_BYTES`] holds the result.
//!
//! Served as verbs since the same release. The belt's 22 tools became the 13
//! plain verbs in [`crate::verbs`], named the way the `kin` CLI names the same
//! capability, and the choices that were a choice between tools became options:
//! `locate` takes a `mode`, `trace` a `to`, and `transaction` an `action`.
//! `exec` runs the toolchain; the retired `session` is listed nowhere.
//! Every table below is keyed on the verb a belt serves, except
//! [`apply_belt_defaults`] and [`BUDGET_TOOLS`], which run after a call is
//! resolved and so are keyed on the registered tool the verb routed to. The
//! listing went from 14,719 bytes to 10,392, with `refs` no longer
//! advertising the answer-only toggle the agent profiles never take.
//!
//! Every continuation a reply hands back is served since 2026-09-29. `refs`,
//! `trace` and `impact` answered with a `next_cursor` their served schemas
//! could not take, and `history` with a `next_offset` its closed schema could
//! not; they now serve `cursor` and `offset` as `locate` and `status` did, and
//! property prose that only restated a name paid for them inside the same
//! ceilings. `every_continuation_is_callable_through_the_served_schema` holds it.
//!
//! Cut again on 2026-10-03, inside the schemas the verbs serve. Prose that a
//! field's name, its served default or the tool's own description already says
//! is withheld on the agent profiles and kept on `full`
//! ([`crate::request_schema::Field::no_agent_doc`]), and an agent schema serves
//! a default only when it is a boolean that is on, or one this belt injects.
//! The 11 definitions went from 7,978 bytes to 7,229, and no property, type,
//! enum value, bound, required list or argument name moved.
//! [`crate::tools::AGENT_DEFAULT_DEFINITION_CEILING_BYTES`] holds the result and
//! names what holds it there.
//!
//! Cut again on 2026-10-04, measured in the served model's own tokens: 2,428
//! a turn became 2,351 over the same 11 definitions. A clause a reply already
//! teaches went (the `cursor` continuation), and so did the length bound on a
//! value a caller copies back from a reply rather than chooses
//! ([`crate::request_schema::Field::from_reply`]). Every bound, enum, pairing
//! and required list a caller's own choice can break stays served, and the
//! server holds every call to all of them as before.
//!
//! Every short form says in a few words what the tool answers. Where one verb
//! answers a question another is easy to mistake for, it names the other.
//!
//! Trimming a schema hides a property; it does not remove it. No tool in this
//! profile sets `additionalProperties: false`, and the handlers read arguments
//! by name, so a caller that knows a withheld property can still pass it and the
//! `full` profile still advertises every one.
//!
//! # Adding a tool to `agent-default`
//!
//! If you added a name to [`crate::tools::agent_default_tool_names`] and
//! `kin-mcp` went red on `every_agent_default_tool_has_a_short_description`,
//! that is this module asking for two entries, not a defect:
//!
//! 1. [`short_descriptions`]: one short sentence saying what the tool answers,
//!    under [`AGENT_DEFAULT_DESCRIPTION_BUDGET`] characters. Where another tool
//!    is easy to confuse with yours, name it, and add the pointer back in its
//!    entry. Short property descriptions go in [`tool_property_descriptions`],
//!    under [`AGENT_DEFAULT_PROPERTY_DESCRIPTION_BUDGET`].
//! 2. [`schema_keep_lists`]: the input properties that change WHICH entities
//!    come back. Leave out the ones that only reshape the response, since the
//!    profile picks those itself.
//!
//! The guard is deliberate. Without it a tool joins the belt carrying its full
//! registered description, which is the several-thousand-character form this
//! module exists to replace, and nothing says so.

use std::collections::{BTreeMap, HashMap};

use crate::types::ToolsListResult;

/// The most characters one `agent-default` description may carry.
///
/// 90 is half the 180 that held until 2026-09-22, which was itself down from
/// the 265 that bought two sentences. It is a budget rather than a target:
/// several tools here are shorter, and none should need more, because a
/// description longer than this is documentation, and documentation belongs in
/// the `full` profile where a reader has the room for it. An eager client
/// re-sends every description on every request, so a sentence here is paid for
/// once per turn rather than once per session.
/// `no_agent_default_description_exceeds_its_budget` below fails on any tool
/// that exceeds it.
pub const AGENT_DEFAULT_DESCRIPTION_BUDGET: usize = 90;

/// The whole profile's description budget.
///
/// Half of the 3,235 characters the served descriptions cost at 97c719c8d, and
/// a small fraction of the 47,739 the long forms cost. Held as a total as well
/// as a per-tool cap because twenty tools each sitting just under the per-tool
/// budget would be a profile that had learned nothing.
pub const AGENT_DEFAULT_PROFILE_DESCRIPTION_BUDGET: usize = 1_617;

/// The honest name for the declaration filter, accepted on a call and never
/// served.
///
/// `semantic_search` does not search semantically. Its own registered
/// description opens "Find code declarations in the semantic graph by name,
/// kind, or language" and its arguments are `query`, `kind` and `language`: it
/// is a filter over declarations, and it ignores the query for ranking.
/// `semantic_locate` is the tool that ranks by meaning. A model reads a name
/// before it reads five thousand characters of prose, and these two names are
/// the wrong way round.
///
/// That reading still holds, and it was not enough to move the served name.
/// `agent-default` advertised the filter under this name for four landings, and
/// two proofs that run only on `main` went red on every one of them: the
/// install proof throws `MCP tools/list omitted semantic_search`
/// (`.github/workflows/install-proof.yml`) and the Windows npm proof asserts
/// `toolNames.includes('semantic_search')`
/// (`scripts/prove-windows-npm-first-run.mjs`). Renaming a public MCP tool on
/// the default profile is a product decision that has to move those proofs, the
/// docs and the acceptance suite in one deliberate change, rather than ride
/// along inside a payload compaction.
///
/// So the name lives on as an accepted input only. [`canonical_tool_name`] maps
/// it back at each dispatch entry, so a caller that learned it during those four
/// landings still reaches the same handler, while `tools/list` advertises the
/// registered name on every profile.
/// `agent_default_serves_every_name_the_shipped_proofs_assert` is what holds
/// that per pull request.
pub const DECLARATION_FILTER_ALIAS: &str = "find_declarations";

/// The registered name [`DECLARATION_FILTER_ALIAS`] stands in for.
pub const DECLARATION_FILTER_CANONICAL: &str = "semantic_search";

/// Map an accepted tool name back to the name everything internal is keyed on.
///
/// One function, called once per call at the point the name is parsed, so the
/// profile filter, the dispatcher, the response-budget shape, the
/// negative-evidence spec and the envelope all see the registered name and none
/// of them has to learn the alias. Every other name is returned unchanged, so
/// this is safe to call unconditionally and on every profile: a caller that
/// still sends `semantic_search` reaches the same handler it always did.
pub fn canonical_tool_name(name: &str) -> &str {
    if name == DECLARATION_FILTER_ALIAS {
        DECLARATION_FILTER_CANONICAL
    } else {
        name
    }
}

/// The in-place form, for a dispatcher that holds the name it parsed.
///
/// Separate from [`canonical_tool_name`] rather than spelled out at each call
/// site, because the obvious spelling does not compile: the returned `&str`
/// borrows the string being assigned to, so binding it before the assignment
/// holds a shared borrow across a mutable one. This does the comparison and
/// leaves a name that is already canonical untouched, so the common path
/// allocates nothing.
pub fn canonicalize_tool_name(name: &mut String) {
    if name == DECLARATION_FILTER_ALIAS {
        name.clear();
        name.push_str(DECLARATION_FILTER_CANONICAL);
    }
}

/// Ask, on this belt's agents' behalf, for the answer shape and the size a
/// small model can afford.
///
/// Called after [`crate::verbs::resolve`], so `name` is the registered tool a
/// verb routed to, and a call by a verb and one by the name it replaced get the
/// same defaults.
///
/// Compact responses keep entity identities and signatures within the initial
/// tool budget. Full responses retain entity detail and explanations; neither
/// MCP shape exposes the operator's file catalog. The shared result type can
/// still decode a full MCP answer with its absent legacy roll-up defaulted empty.
///
/// Only when the caller named no `surface` of its own. An agent that asks for
/// `full` gets full, which is what makes this a default rather than an override,
/// and every insertion below follows the same rule.
///
/// The rest is the response size. The registered ceiling of 45,000 characters is
/// right for a client with room and wrong for this belt: it is roughly 10,500
/// tokens, so two default answers exhaust a 24,000-token run. Each budget tool
/// therefore gets [`agent_default_response_max_chars`] for its own name, which
/// is [`AGENT_DEFAULT_RESPONSE_MAX_CHARS`] for a tool that answers with a list
/// and [`AGENT_CHAIN_RESPONSE_MAX_CHARS`] for the two that answer with a chain.
/// The walker gets the shape rather than the bodies, and the context pack gets a
/// token budget that fits inside the answer the list rule sizes. Every one of
/// these is advertised on the served schema by [`apply_belt_schema_defaults`],
/// so a caller reading `tools/list` sees the number the belt actually sends and
/// can raise it with the same `max_chars` it always could.
pub fn apply_belt_defaults(name: &str, arguments: &mut HashMap<String, serde_json::Value>) {
    if name == "find_references" {
        let explain = arguments
            .get("explain")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
            || arguments
                .get("compact")
                .and_then(serde_json::Value::as_bool)
                == Some(false);
        arguments
            .entry(crate::budget::ANSWER_ONLY_PARAM.to_string())
            .or_insert_with(|| serde_json::Value::Bool(!explain));
    }
    if name == "semantic_locate" {
        arguments
            .entry("surface".to_string())
            .or_insert_with(|| serde_json::Value::String("compact".to_string()));
    }

    // Shape before bodies on the walker. Not inserted when the caller named
    // either spelling, because `compact` is the alias for `include_body: false`
    // and overriding a caller who asked for one of them would be the belt
    // answering a question it was not asked.
    if name == "trace_data_flow"
        && !arguments.contains_key("include_body")
        && !arguments.contains_key("compact")
    {
        arguments.insert("include_body".to_string(), serde_json::Value::Bool(false));
    }

    if name == "get_context_pack" {
        arguments
            .entry("token_budget".to_string())
            .or_insert_with(|| serde_json::json!(AGENT_DEFAULT_CONTEXT_PACK_TOKEN_BUDGET));
    }

    // Default only a fresh ranking. A cursor already carries its page width;
    // injecting a limit here would override it and silently widen the next page.
    if name == "semantic_locate" && !arguments.contains_key("cursor") {
        arguments
            .entry("limit".to_string())
            .or_insert_with(|| serde_json::json!(AGENT_DEFAULT_LOCATE_PAGE));
    }

    // The response ceiling, on every belt tool that has one. Both spellings are
    // checked before inserting, because `ResponseBudget::from_arguments` takes
    // the FIRST of `max_chars` then `max_response_chars` that is present: an
    // unconditional insert would silently outrank a caller who had passed
    // `max_response_chars` and hand them a ceiling they never asked for.
    if BUDGET_TOOLS.contains(&name)
        && !arguments.contains_key("max_chars")
        && !arguments.contains_key("max_response_chars")
    {
        arguments.insert(
            "max_chars".to_string(),
            serde_json::json!(agent_default_response_max_chars(name)),
        );
    }
}

/// The shape toggles the agent profiles never take, on the tools whose answer
/// they shape.
///
/// Measured in the 0.8.3 benchmark: an agent that had the whole answer from
/// its first reference call passed `answer_only: false`, which the served
/// schema then described, doubled the reply to 18.9 KB, and spent five more
/// turns verifying. An agent on these profiles is served the answer-only
/// shape, always, and the toggles are no longer advertised to it; the `full`
/// profile keeps them.
const SHAPE_TOGGLES: [&str; 3] = [crate::budget::ANSWER_ONLY_PARAM, "explain", "compact"];

/// The registered tools whose answer [`SHAPE_TOGGLES`] would reshape: the
/// references, the walk and the route, and the context pack.
const SHAPED_TOOLS: [&str; 4] = [
    "find_references",
    "trace_data_flow",
    crate::handlers::path::TOOL_NAME,
    "get_context_pack",
];

/// Take out of one agent-profile call what these profiles never honor, and
/// say what was taken in one line, or `None` when nothing was.
///
/// `tool` is the registered tool the call resolves to, so a call by a verb and
/// one by the name it replaced are read alike, and a caller that learned a
/// toggle on another profile still gets an answer, the answer-only one, with a
/// sentence saying why.
///
/// A literal lookup is held to the list ceiling this profile advertises,
/// [`AGENT_DEFAULT_RESPONSE_MAX_CHARS`]. Its answers are lexical evidence and
/// always read inconclusive, and in the same benchmark an agent spent three of
/// them, one at 60,000 characters, re-verifying an answer `refs` had already
/// certified.
///
/// The cap earns no sentence. The line this returns is served as prose after
/// the answer, and after a literal reply it left the reply's text no longer
/// one JSON object. Nothing the sentence said is lost with it: the served
/// `max_chars` default is this ceiling, and a literal page cut by it says, in
/// its own JSON, how many matches it lists and hands back the `next_cursor`
/// that reaches the rest.
pub fn agent_profile_overrides(
    tool: &str,
    arguments: &mut HashMap<String, serde_json::Value>,
) -> Option<String> {
    let mut sentences = Vec::new();
    if SHAPED_TOOLS.contains(&tool) {
        let ignored: Vec<&str> = SHAPE_TOGGLES
            .iter()
            .copied()
            .filter(|toggle| arguments.remove(*toggle).is_some())
            .collect();
        if !ignored.is_empty() {
            sentences.push(format!(
                "{} is not taken on this profile, which always gives the short answer.",
                ignored.join(" and ")
            ));
        }
    }
    if tool == crate::handlers::lexical::TOOL_NAME {
        let cap = AGENT_DEFAULT_RESPONSE_MAX_CHARS;
        for key in [
            crate::request_schema::OUTPUT_BUDGET,
            "max_chars",
            "max_response_chars",
        ] {
            if arguments
                .get(key)
                .and_then(serde_json::Value::as_u64)
                .is_some_and(|asked| asked > cap)
            {
                arguments.insert(key.to_string(), serde_json::json!(cap));
            }
        }
    }
    (!sentences.is_empty()).then(|| sentences.join(" "))
}

/// The short description for each tool a belt profile serves, by served name.
///
/// Each query verb's form leads with the question it answers, in the words a
/// question arrives in, then says what comes back. Measured on 2026-10-01 with
/// a local model on tuned tasks, a surface whose descriptions led with the
/// question cut the model's generated tokens by a third at the same exact
/// answers. `refs` says it lists overriding methods because "what has to change
/// with this method's signature" is a question it answers.
///
/// Keyed on the verb a belt serves. The old names the verbs replaced are never
/// served on a belt, so they carry no short form. Where one verb answers a
/// question another is easy to mistake for, it names the other in backticks,
/// which is how a served description points at a tool.
fn short_descriptions() -> BTreeMap<&'static str, &'static str> {
    BTreeMap::from([
        (
            crate::ask::TOOL_NAME,
            "Plain question about X: callers, reach, change impact, uses, declarations, code.",
        ),
        (
            crate::verbs::LOCATE,
            "Find matches",
        ),
        (
            crate::verbs::REFS,
            "Uses X? References/overrides. Paths: `trace`.",
        ),
        (
            crate::verbs::CONTEXT,
            "Around X? Code/routes by name/question.",
        ),
        (crate::verbs::SOURCE, "X's code? Exact, by entity_id."),
        (
            crate::verbs::NEIGHBORS,
            "X's deps/dependents? Paths `trace`.",
        ),
        (
            crate::verbs::IMPACT,
            "X changes? Callers/overrides first.",
        ),
        (
            crate::verbs::STATUS,
            "Graph health; not needed to trust a reply. last_settled_selected_graph is older.",
        ),
        // The one fact an agent cannot discover for itself: that the list it was
        // handed is partial on purpose, and how to reach the rest. Every other
        // short form here describes a capability; this one describes the surface.
        (
            crate::tool_invocation::TOOL_NAME,
            "Run a discovered read-only tool with its name and input object. Mutations require a direct tool; normal authorization applies.",
        ),
        (
            crate::handlers::tool_search::TOOL_NAME,
            "This profile does not serve every tool in the registry. Find tools by describing \
             the job. Each match includes its full \
             schema and invocation routes. Discovery does not change the tool list; use \
             kin_tool_call for matches marked callable_via_dispatcher, or a connection serving \
             the tool directly. Normal authorization applies. Omit `need` to list every tool.",
        ),
        (
            crate::verbs::HISTORY,
            "Who changed X, when, approved?",
        ),
        (
            crate::verbs::MUTATE,
            "Commit entity changes atomically, in one call.",
        ),
        (
            crate::verbs::TRANSACTION,
            "Plumbing for writes; `mutate` with transaction_id stages.",
        ),
        // A callers walk answers with every caller, the callers of callers
        // included, so the description says one call is enough: an agent that
        // read "call chain/reach" planned to trace "recursively until I reach
        // a fixpoint". The twenty characters this adds come off `details` and
        // `scope` on the same schema.
        (
            crate::verbs::TRACE,
            "X's reach? Direct+indirect callers in 1 call. to: route; scope: module.",
        ),
    ])
}

/// The input properties `agent-default` advertises for each tool, by registered
/// name. A tool absent from this table keeps its full schema.
///
/// Two rules decided every list, and one exception overrides the second.
///
/// A property that changes WHICH entities come back stays. A property that only
/// reshapes the response (`explain`, `snippet_alias`, `pipeline`) goes, because
/// the profile picks those defaults itself and every one of them is another
/// line the model reads before it can ask its question.
///
/// The exception: a response-shaping property the shipped proofs or the
/// acceptance suite assert stays anyway, because those checks read the served
/// schema as the contract an agent discovers the knob from. Trimming
/// `include_body` and `compact` off `trace_data_flow` and `max_chars` off every
/// tool that registered one took three acceptance findings red for four
/// landings, and an agent that cannot see the knob cannot ask for bodies or
/// bound a response. Three checks own that contract:
/// `scripts/acceptance/magic_repro.py` `check_6`, which requires `include_body`
/// or `compact` on `trace_data_flow`; `check_14` arm 3, which requires the
/// literal `last_settled_selected_graph` in the served `kin_graph_status`
/// description; and `scripts/acceptance/response_budget_elisions.py` `check_2`,
/// which grades every advertised `max_chars` or `max_response_chars` and
/// reports UNREADABLE when it finds none. Do not trim these again without
/// moving those checks in the same change. What those checks need is one knob
/// per tool: `include_body` or `compact`, and `max_chars` or
/// `max_response_chars`, so each tool keeps one spelling of each.
///
/// Two more rules since 2026-09-28. A deprecated input goes, since the served
/// form's job is the way to call a tool now, and a caller still sending one is
/// answered as before. So does an alias whose canonical spelling is served
/// beside it, and a one-value enum, which offers no choice to make.
fn schema_keep_lists() -> BTreeMap<&'static str, &'static [&'static str]> {
    // The verbs are not here: each is served from its typed request
    // ([`crate::requests`]), which names the fields an agent is served as
    // `Visibility::Agent`. What remains are the discovery tools, which have no
    // typed request.
    BTreeMap::from([
        // The question alone, as the surface the sealed comparison measured
        // served it: its words set the test filter and the package limit.
        // `include_tests`, `scope`, `max_output_bytes` and `explain` stay
        // accepted, unadvertised, and `full` advertises every one.
        (crate::ask::TOOL_NAME, &["question"] as &[&str]),
        // Both, written out rather than left absent. The tool registers exactly
        // these two, so this trims nothing today; it is here because the belt's
        // rule is that a served tool declares which properties survive, and a
        // property added later would otherwise join the belt unreviewed.
        (
            crate::handlers::tool_search::TOOL_NAME,
            &["need", "limit"] as &[&str],
        ),
        (
            crate::tool_invocation::TOOL_NAME,
            &["tool", "arguments"] as &[&str],
        ),
    ])
}

/// The response ceiling `agent-default` asks for on its agents' behalf, for
/// every budgeted tool whose answer is a LIST.
///
/// Derived from one rule rather than taste: an agent must be able to make at
/// least six tool calls at default answer size inside a 24,000-token run and
/// still have room to answer. That puts one answer at about 2,800 tokens, and at
/// the 4.28 bytes per token measured on this profile's own JSON against
/// `google/gemma-4-e4b` that is about 12,000 characters.
///
/// The registered default is 45,000, which is [`crate::budget::
/// RESPONSE_DEFAULT_MAX_CHARS`] and right for a client with room. On this belt
/// it is catastrophic for the case the belt exists for: 45,000 characters is
/// roughly 10,500 tokens, so TWO default answers exhaust a 24,000-token run
/// before the model has reasoned about either. That is read from source rather
/// than guessed, because `apply_belt_defaults` shaped only `semantic_locate`'s
/// `surface` and every other tool fell through to the registered default.
///
/// Advertised AND injected, deliberately the same number. The acceptance suite
/// treats the served schema as the contract an agent reads, so a belt that
/// injected 12,000 while advertising 45,000 would be lying in the one place a
/// caller looks. Both halves move together, and `full` keeps 45,000.
///
/// A list cut at this number loses its tail and keeps its answer: the rows a
/// reader most wants lead it, and the count beside them says how many were
/// dropped. [`AGENT_CHAIN_RESPONSE_MAX_CHARS`] is the number for the two tools
/// where that is not true.
pub const AGENT_DEFAULT_RESPONSE_MAX_CHARS: u64 = 12_000;

/// The response ceiling `agent-default` asks for on the two tools whose answer
/// is a CHAIN rather than a list.
///
/// The number is the agent's own per-result limit. `kin agent run` cuts one tool
/// result to `ContextWindow::default_result_ceiling()`, an eighth of the window
/// at `kin_agent::context::BYTES_PER_TOKEN`, and on the 65,536-token window the
/// 2026-09-15 trace measurement ran under that is 24,576 bytes. It is the
/// largest ceiling a server can ask for and still know the whole answer reaches
/// the model, because past it the harness cuts and Kin discloses nothing about a
/// cut it did not make. kin-agent is not a dependency of this crate and cannot
/// be, so the number is written here rather than imported, with its derivation
/// beside it.
///
/// Why these two tools and not the whole belt. The rule above sizes an answer so
/// six of them fit a 24,000-token run, and for a ranked list that trade is
/// right: the top of the list IS the answer. A chain is not a list. Cutting it
/// removes the far end, which is the end the question was about, and a walk that
/// never reaches the callee a caller asked about has spent its tokens and
/// answered nothing. #76 measured that on `cli/cli`: at the trace tool's wider
/// per-step default the rendered answer is 16,327 characters, the 12,000 ceiling
/// cut the chain, and an untargeted `trace_data_flow` still did not reach
/// `httpRequest`. `get_context_pack` assembles the same shape, a focal plus the
/// routes and dependencies around it, and sheds from the same far end.
///
/// The cost is stated rather than hidden: two chain answers now fill a
/// 24,000-token run where four fit before. That is the trade the decision makes,
/// and it is made for the tools where a cut answer is not a shorter answer but a
/// wrong one. Every other budgeted tool keeps 12,000.
pub const AGENT_CHAIN_RESPONSE_MAX_CHARS: u64 = 24_576;

/// The belt tools that get [`AGENT_CHAIN_RESPONSE_MAX_CHARS`].
///
/// A list rather than a predicate on the tool's shape, because "is this answer a
/// chain" is not a property the registry carries and inferring it would make the
/// ceiling move when an unrelated field moved.
///
/// Each chain is named twice: by the registered tool the belt injects the
/// ceiling for, and by the verb that advertises it. `trace_path` is a chain as
/// well, the hops between two ends, and since `trace` serves both the walk and
/// the route under one advertised ceiling, both of its tools are held to it.
///
/// A reference answer is a list, and it is here all the same. A page is cut
/// by its machine form, and the text an agent reads is about a quarter of
/// that. Asked about a name three declarations share, the list ceiling held
/// one of the three sections, 3,032 bytes of text, and put the other two,
/// 4,226 bytes, behind a cursor: the answer was never too large for its
/// reader, only for the form it was measured in. At this ceiling the three
/// fit one page. It stays here until a page is cut by the text a reader gets.
const CHAIN_RESPONSE_TOOLS: [&str; 7] = [
    "trace_data_flow",
    crate::handlers::path::TOOL_NAME,
    "get_context_pack",
    "find_references",
    crate::verbs::TRACE,
    crate::verbs::CONTEXT,
    crate::verbs::REFS,
];

/// The response ceiling this belt asks for on behalf of `tool`, a registered
/// name or the verb that serves it.
///
/// One function so the injected number and the advertised number cannot come
/// apart: `apply_belt_defaults` and `belt_schema_defaults` both read it, and
/// `no_agent_default_response_budget_is_advertised_above_the_cap` grades them
/// against each other through it.
pub fn agent_default_response_max_chars(tool: &str) -> u64 {
    if CHAIN_RESPONSE_TOOLS.contains(&tool) {
        AGENT_CHAIN_RESPONSE_MAX_CHARS
    } else {
        AGENT_DEFAULT_RESPONSE_MAX_CHARS
    }
}

/// The ranked entities `semantic_locate` returns per page on `agent-default`.
///
/// The response cap above bounds what the SERVER builds. This bounds what a
/// CLIENT is handed, which is a different cut and the one that was binding.
/// Measured on the demo's React and VS Code stores on 2026-09-02
/// (`scratchpad/reports/demo-rerun.md`), every `semantic_locate` in all three
/// agentic runs came back cut at the client's own 1,500-token per-result
/// budget, which is 5,250 characters at that harness's 3.5 characters per
/// token. Compaction had made the payload three to five times smaller and it
/// still did not fit, so the cut landed every time and the model re-issued the
/// same query three times per run rather than reading a whole answer.
///
/// A client cut is not a cut Kin can disclose: the server returned inside its
/// own ceiling and the harness truncated afterwards, so none of the remediation
/// text in [`crate::budget`] ever reached the model. The only lever Kin holds is
/// to return a page that fits.
///
/// 12 is counted, not inferred. Read-only against the demo's own daemons on
/// 2026-09-02, with the demo's own binary and environment so no second daemon
/// was started, running the logged command and varying only the page:
///
/// | query | 24 | 13 | 12 | 11 |
/// |---|---|---|---|---|
/// | type character in editor | 6,471 | 4,624 | 4,343 | 4,065 |
/// | handle keyboard input character insertion | 8,732 | 5,281 | 5,029 | 4,779 |
/// | editor handles key press event for character input | 7,425 | 4,305 | 4,038 | 3,769 |
/// | component asking for an update (react) | 7,669 | 4,743 | 4,464 | 4,128 |
///
/// The page-24 column reproduces `demo-rerun.md` byte for byte, which is the
/// control that says this is the same surface it measured. At 13 the densest
/// query is 5,281 bytes and misses the window by 31; at 12 every query fits,
/// the worst with 221 characters to spare. So 12 is the largest page that fits
/// them all.
///
/// An earlier inference from the report's density line put this at 13. It was
/// off by one, which is what counting is for.
///
/// A page is not a cap on what the caller can reach. The answer carries
/// `total_ranked` and `next_cursor`, so an agent that wants more asks for the
/// next page instead of re-asking the same question, which is the behaviour this
/// number exists to buy.
pub const AGENT_DEFAULT_LOCATE_PAGE: u64 = 12;

/// The whole rendered context reply's estimated token budget on the agent belt.
///
/// Code, repository observations and qualification metadata all count. A real
/// short focal with its dependency neighborhood needs over 3,300 tokens, so
/// 4,000 keeps that answer intact with room for a longer body. The byte ceiling
/// applies independently, and explicit token budgets override this default.
pub const AGENT_DEFAULT_CONTEXT_PACK_TOKEN_BUDGET: u64 = 4_000;

/// The belt tools whose registered schema advertises a response budget.
///
/// Held as a list because [`apply_belt_defaults`] runs on every call and
/// building the registry there to rediscover eight names would cost more than
/// it saves. `the_budget_tool_list_matches_the_registry` fails if the registry
/// and this list ever disagree, so it cannot go stale quietly.
const BUDGET_TOOLS: [&str; 10] = [
    "semantic_locate",
    DECLARATION_FILTER_CANONICAL,
    "find_references",
    "trace_data_flow",
    crate::handlers::path::TOOL_NAME,
    "graph_neighborhood",
    "impact_analysis",
    "get_context_pack",
    "kin_graph_status",
    crate::handlers::lexical::TOOL_NAME,
];

/// The properties whose advertised `default` this belt rewrites, by tool.
///
/// A number the profile injects has to be the number the profile advertises, or
/// the served schema stops being the contract the acceptance suite grades it as.
fn belt_schema_defaults() -> BTreeMap<(&'static str, &'static str), serde_json::Value> {
    let mut defaults: BTreeMap<(&'static str, &'static str), serde_json::Value> = BTreeMap::new();
    // Keyed on the verb that serves each budgeted tool. The tools one verb
    // folds share one ceiling, so the verb advertises the number every one of
    // them is sent.
    // Under the one served budget name. The tools one verb folds share one
    // ceiling, so the verb advertises the number every one of them is sent,
    // whichever spelling its handler reads it under.
    for tool in BUDGET_TOOLS {
        let served = crate::verbs::verb_for(tool).unwrap_or(tool);
        defaults.insert(
            (served, crate::request_schema::OUTPUT_BUDGET),
            serde_json::json!(agent_default_response_max_chars(served)),
        );
    }
    // Shape first. The tool's own belt description tells a model to pass false
    // when it wants the shape of a chain, and then the registered default handed
    // it bodies anyway.
    defaults.insert(
        (crate::verbs::TRACE, "include_body"),
        serde_json::Value::Bool(false),
    );
    defaults.insert(
        (crate::verbs::CONTEXT, "token_budget"),
        serde_json::json!(AGENT_DEFAULT_CONTEXT_PACK_TOKEN_BUDGET),
    );
    defaults
}

/// Properties whose registered `default` this belt does not advertise, because
/// no one number is what every call gets.
///
/// `locate` is a page of 12 when it reads a description, which is
/// [`AGENT_DEFAULT_LOCATE_PAGE`], and a name search or a literal lookup keeps
/// its registered 20: a name search does not page, so a smaller default there
/// would cut an answer a caller could not continue. A served default of either
/// number would be wrong for the other modes, so none is served.
const BELT_UNSTATED_DEFAULTS: [(&str, &str); 1] = [(crate::verbs::LOCATE, "limit")];

/// Rewrite one tool's advertised property defaults for `agent-default`.
///
/// Only a property the schema already carries is touched, so this can never
/// invent a knob, and only its `default` moves: `minimum` and `maximum` are the
/// server's real limits and stay as registered, which is also what keeps
/// `response_budget_elisions.py` `check_2` satisfied, since it requires
/// `minimum < default <= maximum`.
pub(crate) fn apply_belt_schema_defaults(tool: &str, schema: &mut serde_json::Value) {
    let defaults = belt_schema_defaults();
    let Some(properties) = schema
        .get_mut("properties")
        .and_then(|value| value.as_object_mut())
    else {
        return;
    };
    for (name, property) in properties.iter_mut() {
        if BELT_UNSTATED_DEFAULTS.contains(&(tool, name.as_str())) {
            if let Some(property) = property.as_object_mut() {
                property.remove("default");
            }
            continue;
        }
        let (Some(value), Some(property)) = (
            defaults.get(&(tool, name.as_str())),
            property.as_object_mut(),
        ) else {
            continue;
        };
        property.insert("default".to_string(), value.clone());
    }
}

/// The most characters one `agent-default` PROPERTY description may carry.
///
/// The tool descriptions were the visible half of the belt's cost and 1353 cut
/// them from 47,739 characters to 6,188. Measured on 2026-09-02 against
/// `google/gemma-4-e4b`, the model the demo actually runs, that left the served
/// list at 7,747 tokens of which 6,380 are input schema and only 1,367 are
/// description. The schemas are where the budget goes now, and inside them the
/// bytes are property prose: thirteen property descriptions ran past 200
/// characters, and one of them, `max_chars`, carried the same 649 characters on
/// seven different tools.
///
/// 45 is half the 90 that held until 2026-09-22, and one short clause. A
/// property description exists to tell a model what to pass, and the shape,
/// bounds and default are already machine-readable beside it in `type`, `enum`,
/// `minimum`, `maximum` and `default`, so prose that restates them is paid for
/// twice, and on an eager client it is paid for again on every request.
/// `no_agent_default_property_description_exceeds_its_budget` fails on any
/// top-level property over it.
pub const AGENT_DEFAULT_PROPERTY_DESCRIPTION_BUDGET: usize = 45;

/// The short form of a property whose name, type, enum, bounds and default
/// already say everything a caller needs: no description at all.
///
/// A clause that restates `enum: ["calls", "callers", "both"]` as "`calls`,
/// `callers` or `both`", or `limit` as "Max rows.", is paid for on every
/// request and tells a model nothing the schema beside it did not.
/// [`shorten_description`] removes the description when handed this.
const SAID_BY_THE_SCHEMA: &str = "";

/// Short property descriptions that read the same on every tool carrying them.
///
/// Keyed by property name alone, because these properties mean one thing across
/// the belt and the long forms said that one thing seven times.
/// [`tool_property_descriptions`] overrides this per tool where the meaning
/// genuinely differs.
fn shared_property_descriptions() -> BTreeMap<&'static str, &'static str> {
    BTreeMap::from([
        // The name, the bounds and the default say it. Whether a tool cuts toward
        // the budget or refuses a reply that cannot fit is something the reply
        // says when it happens: a cut is reported under `elisions` and
        // `response_over_budget`, and a pack that cannot fit is refused with the
        // reason. Measured in the 0.8.2 head-to-head, the model passed
        // `max_chars` on 24 of its 42 `find_references` calls, so it reads the
        // knob without the clause.
        ("max_chars", SAID_BY_THE_SCHEMA),
        ("max_response_chars", SAID_BY_THE_SCHEMA),
        ("cursor", "`next_cursor` from the prior page."),
        ("offset", "`next_offset` from the prior page."),
        ("session_id", "Owning session UUID."),
        ("entity_id", "UUID"),
        ("transaction_id", "UUID"),
        ("source_change_id", "Change id; default is head."),
    ])
}

/// Short property descriptions for one tool's property, overriding
/// [`shared_property_descriptions`] where a name means different things.
///
/// Keyed by `(tool, property)`. `direction` is the reason this table exists: it
/// walks callees on `trace_data_flow` and dependencies on `graph_neighborhood`,
/// and one clause cannot honestly say both.
fn tool_property_descriptions() -> BTreeMap<(&'static str, &'static str), &'static str> {
    // The verbs' short property descriptions live on their typed requests
    // ([`crate::requests`]), beside the type, bounds and default each one
    // describes. What remains are the discovery tools, which have none.
    BTreeMap::from([
        ((crate::ask::TOOL_NAME, "question"), SAID_BY_THE_SCHEMA),
        (
            (crate::handlers::tool_search::TOOL_NAME, "need"),
            "The job in plain language.",
        ),
        (
            (crate::handlers::tool_search::TOOL_NAME, "limit"),
            "Matches returned as full definitions; see `matched_names`.",
        ),
    ])
}

/// Replace `property`'s description with `short` when that is actually shorter.
///
/// Only when it is shorter. A "short form" table keyed by property name meets
/// the same name on tools whose registered description is already one clause,
/// and replacing those made three tools BIGGER when this table grew:
/// `kin_transaction_begin` by 41 bytes, `kin_session_end` and
/// `kin_session_heartbeat` by 64 each. A trim that can lengthen its subject is
/// not a trim, and the arithmetic hid it because the total still fell.
///
/// [`SAID_BY_THE_SCHEMA`] removes the description outright, since an empty
/// string would still cost its key.
fn shorten_description(property: &mut serde_json::Value, short: &str) {
    let Some(property) = property.as_object_mut() else {
        return;
    };
    let Some(existing) = property.get("description").and_then(|value| value.as_str()) else {
        return;
    };
    if short == SAID_BY_THE_SCHEMA {
        property.remove("description");
    } else if short.len() < existing.len() {
        property.insert(
            "description".to_string(),
            serde_json::Value::String(short.to_string()),
        );
    }
}

/// Replace one tool's top-level property descriptions with their short forms.
fn shorten_property_descriptions(tool: &str, schema: &mut serde_json::Value) {
    let shared = shared_property_descriptions();
    let per_tool = tool_property_descriptions();
    let Some(properties) = schema
        .get_mut("properties")
        .and_then(|value| value.as_object_mut())
    else {
        return;
    };
    for (name, property) in properties.iter_mut() {
        let short = per_tool
            .get(&(tool, name.as_str()))
            .or_else(|| shared.get(name.as_str()));
        if let Some(short) = short {
            shorten_description(property, short);
        }
    }
}

/// Rewrite one served tool list for the `agent-default` profile: short tool
/// descriptions, trimmed schemas, and short property descriptions inside the
/// schemas that survive. Tool names are left exactly as registered, because two
/// proofs that run only on `main` read `tools/list` and assert a name
/// literally.
///
/// A tool with no short form keeps its registered description, so adding a tool
/// to the profile is never silently a tool with no description. The
/// `every_agent_default_tool_has_a_short_description` test is what stops that
/// from being a quiet gap.
pub fn compact_for_agent_default(list: &mut ToolsListResult) {
    let descriptions = short_descriptions();
    let keeps = schema_keep_lists();
    for tool in &mut list.tools {
        if let Some(short) = descriptions.get(tool.name.as_str()) {
            tool.description = (*short).to_string();
        }
        // A verb is served its typed request's agent form, which is already the
        // cut form: the fields an agent needs with their short descriptions.
        // The default injection still runs, because a budget this profile
        // advertises has to be the budget it injects whatever produced the
        // schema.
        if let Some(schema) = crate::requests::served_schema(
            &tool.name,
            crate::request_schema::Profile::Agent,
            &|_| None,
        ) {
            tool.input_schema = schema;
        } else {
            if let Some(keep) = keeps.get(tool.name.as_str()) {
                trim_schema(&mut tool.input_schema, keep);
            }
            shorten_property_descriptions(&tool.name, &mut tool.input_schema);
        }
        apply_belt_schema_defaults(&tool.name, &mut tool.input_schema);
    }
    // No name is rewritten here, so the name order `tools::tool_definitions`
    // built survives untouched. A client caches the prompt it builds from
    // `tools/list`, and an order this function moved would miss that cache.
}

/// Keep only the named properties, and narrow `required` to what survives.
///
/// A required property that the trim removed would leave a schema demanding a
/// field it does not describe, which is a schema no client can satisfy.
fn trim_schema(schema: &mut serde_json::Value, keep: &[&str]) {
    let Some(object) = schema.as_object_mut() else {
        return;
    };
    if let Some(serde_json::Value::Object(properties)) = object.get_mut("properties") {
        properties.retain(|name, _| keep.contains(&name.as_str()));
    }
    if let Some(serde_json::Value::Array(required)) = object.get_mut("required") {
        required.retain(|name| name.as_str().is_some_and(|n| keep.contains(&n)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// Every tool the profile serves needs a short form, or the profile carries
    /// a 6,823-character description it was supposed to have replaced.
    #[test]
    fn every_agent_default_tool_has_a_short_description() {
        let descriptions = short_descriptions();
        let missing: Vec<&str> = crate::tools::agent_default_tool_names()
            .iter()
            .copied()
            .filter(|name| !descriptions.contains_key(name))
            .collect();
        assert!(
            missing.is_empty(),
            "these agent-default tools have no short description: {missing:?}; add an entry for \
             each in short_descriptions() and schema_keep_lists() in \
             crates/kin-mcp/src/agent_belt.rs, at most {AGENT_DEFAULT_DESCRIPTION_BUDGET} \
             characters per description. Without one the tool joins the belt carrying its full \
             registered description, which is the several-thousand-character form this module \
             exists to replace."
        );
    }

    /// Every tool the search profile serves needs one too, for the same reason
    /// and with more at stake: that profile serves five tools, so one carrying
    /// its several-thousand-character registered form would be most of the list.
    #[test]
    fn every_agent_search_tool_has_a_short_description() {
        let descriptions = short_descriptions();
        let missing: Vec<&str> = crate::tools::agent_search_tool_names()
            .iter()
            .copied()
            .filter(|name| !descriptions.contains_key(name))
            .collect();
        assert!(
            missing.is_empty(),
            "these agent-search tools have no short description: {missing:?}; add an entry for \
             each in short_descriptions() and schema_keep_lists() in \
             crates/kin-mcp/src/agent_belt.rs, at most {AGENT_DEFAULT_DESCRIPTION_BUDGET} \
             characters per description."
        );
    }

    /// The agent profiles serve `refs` in the answer-only shape, always: the
    /// toggle is not advertised there, and a call that sends it, or `explain`
    /// or `compact`, is answered answer-only with one line saying so. The
    /// `full` profile keeps the toggle and its detailed default.
    #[test]
    fn agent_profiles_always_answer_references_in_the_answer_only_shape() {
        for profile in [
            crate::tools::agent_default_tool_names(),
            crate::tools::agent_query_tool_names(),
        ] {
            let served =
                crate::tools::served_tools_list(Some(&crate::tools::name_set(profile)), true);
            let refs = served
                .tools
                .iter()
                .find(|tool| tool.name == crate::verbs::REFS)
                .unwrap();
            for toggle in SHAPE_TOGGLES {
                assert!(
                    refs.input_schema["properties"].get(toggle).is_none(),
                    "refs advertises {toggle} on an agent profile"
                );
            }
        }
        let full = crate::tools::served_tools_list(None, false);
        let tool = full
            .tools
            .iter()
            .find(|tool| tool.name == crate::verbs::REFS)
            .unwrap();
        assert_eq!(
            tool.input_schema["properties"]["answer_only"]["default"],
            false
        );
        for (tool, arguments) in [
            ("find_references", serde_json::json!({"answer_only": false})),
            ("find_references", serde_json::json!({"explain": true})),
            ("find_references", serde_json::json!({"compact": false})),
            (
                "find_references",
                serde_json::json!({"answer_only": false, "explain": true, "compact": false}),
            ),
            ("trace_data_flow", serde_json::json!({"compact": false})),
            (
                crate::handlers::path::TOOL_NAME,
                serde_json::json!({"explain": true}),
            ),
            ("get_context_pack", serde_json::json!({"compact": true})),
        ] {
            let mut args: HashMap<String, serde_json::Value> =
                serde_json::from_value(arguments.clone()).unwrap();
            let note = agent_profile_overrides(tool, &mut args)
                .unwrap_or_else(|| panic!("{tool} {arguments} earned no note"));
            assert!(note.contains("always gives the short answer"), "{note}");
            assert!(!note.contains('\n'), "one line: {note}");
            for toggle in SHAPE_TOGGLES {
                assert!(!args.contains_key(toggle), "{tool} kept {toggle}");
            }
            apply_belt_defaults(tool, &mut args);
            if tool == "find_references" {
                assert!(
                    crate::budget::ResponseBudget::from_arguments(&args).answer_only,
                    "{arguments}"
                );
            }
        }
        // A call that sends none of them is left alone and earns no note.
        let mut plain = HashMap::from([("query".to_string(), serde_json::json!("parse"))]);
        assert_eq!(agent_profile_overrides("find_references", &mut plain), None);
        apply_belt_defaults("find_references", &mut plain);
        assert!(crate::budget::ResponseBudget::from_arguments(&plain).answer_only);
        // A tool with no answer-only shape keeps what it was sent.
        let mut other = HashMap::from([("compact".to_string(), serde_json::json!(false))]);
        assert_eq!(agent_profile_overrides("semantic_locate", &mut other), None);
        assert!(other.contains_key("compact"));
    }

    /// A literal lookup answers inside this profile's list ceiling, whatever
    /// the call asked for. The cap adds no prose after the answer, so the
    /// reply stays one JSON object;
    /// `a_capped_literal_reply_is_one_json_object` holds that end to end.
    #[test]
    fn agent_profiles_cap_a_literal_lookup_at_the_profile_ceiling() {
        let tool = crate::handlers::lexical::TOOL_NAME;
        let mut asked = HashMap::from([
            ("literal".to_string(), serde_json::json!(".pop(")),
            ("max_chars".to_string(), serde_json::json!(60_000)),
        ]);
        assert_eq!(agent_profile_overrides(tool, &mut asked), None);
        assert_eq!(
            asked["max_chars"],
            serde_json::json!(AGENT_DEFAULT_RESPONSE_MAX_CHARS)
        );
        let mut spelled =
            HashMap::from([("max_response_chars".to_string(), serde_json::json!(30_000))]);
        assert_eq!(agent_profile_overrides(tool, &mut spelled), None);
        assert_eq!(
            spelled["max_response_chars"],
            serde_json::json!(AGENT_DEFAULT_RESPONSE_MAX_CHARS)
        );
        // Under the ceiling is left as asked, and the other modes are not capped.
        let mut small = HashMap::from([("max_chars".to_string(), serde_json::json!(4_000))]);
        assert_eq!(agent_profile_overrides(tool, &mut small), None);
        assert_eq!(small["max_chars"], serde_json::json!(4_000));
        let mut ranked = HashMap::from([("max_chars".to_string(), serde_json::json!(60_000))]);
        assert_eq!(
            agent_profile_overrides("semantic_locate", &mut ranked),
            None
        );
        // Default listing stays compact; the mode enum names the choices,
        // while full descriptions and each text answer explain evidence scope.
        let served = served_agent_default();
        let locate = served
            .tools
            .iter()
            .find(|tool| tool.name == crate::verbs::LOCATE)
            .unwrap();
        assert_eq!(locate.description, "Find matches");
        assert_eq!(
            locate.input_schema["properties"]["mode"]["enum"],
            serde_json::json!(["auto", "name", "description", "text"])
        );
    }

    /// The legacy stored-field lookup on an agent profile is answered in one text block that
    /// parses as one JSON object, also when the call asked for more than the
    /// profile's ceiling and was capped. Checked on the plain reply an agent is
    /// served, and on the machine reply, whose envelope shows the ceiling the
    /// lookup was answered under.
    #[tokio::test]
    async fn a_capped_literal_reply_is_one_json_object() {
        use kin_parser::LanguageAdapter;
        // More stored-field matches than one page lists, so the reply also
        // carries a continuation. Call the legacy handler directly: this
        // fixture holds fields, not the blobs a fresh text search reads.
        let file = kin_model::FilePathId::new("retry.go");
        let mut source = String::from("package retry\n");
        for index in 0..60 {
            source.push_str(&format!(
                "func RetryWithBackoff{index}(attempt int) int {{ return backoffSeconds(attempt + {index}); }}\n"
            ));
        }
        let adapter = kin_parser::GoAdapter;
        let tree = adapter
            .parse(source.as_bytes())
            .expect("the fixture parses");
        let parsed = adapter
            .extract(&tree, source.as_bytes(), &file)
            .expect("the fixture extracts");
        let store = kin_db::InMemoryGraph::new();
        for entity in parsed.entities {
            let entity = entity.into_entity_with_source(
                kin_model::LanguageId::Go,
                &file,
                Some(source.as_bytes()),
            );
            kin_model::graph::EntityStore::upsert_entity(&store, &entity).expect("upsert");
        }
        store.flush_text_index().expect("flush the text index");
        let sessions = crate::session::SessionRegistry::new();
        let request = serde_json::json!({
            "jsonrpc": "2.0", "id": 7, "method": "tools/call",
            "params": {
                "name": crate::handlers::lexical::TOOL_NAME,
                "arguments": {"literal": "backoffSeconds", "max_chars": 60_000},
            }
        })
        .to_string();

        for plain_replies in [true, false] {
            let config = crate::server::McpServerConfig {
                allowed_tools: Some(crate::tools::name_set(
                    crate::tools::agent_default_tool_names(),
                )),
                agent_belt: true,
                plain_replies,
                session_authority_mode: crate::server::SessionAuthorityMode::OfflineFallback,
                ..crate::server::McpServerConfig::default()
            };
            let response = crate::server::process_message(&request, &store, &config, &sessions)
                .await
                .expect("a tools/call is answered");
            let response = crate::server::present_for_client(response, &request, &config);
            let result = response.result.expect("a result, not a protocol error");
            let blocks = result["content"].as_array().expect("content blocks");
            assert_eq!(
                blocks.len(),
                1,
                "plain_replies={plain_replies}: the answer is one block and nothing follows it: \
                 {blocks:?}"
            );
            let text = blocks[0]["text"].as_str().expect("a text block");
            let reply: serde_json::Value = serde_json::from_str(text).unwrap_or_else(|error| {
                panic!("plain_replies={plain_replies}: the reply is not JSON ({error}): {text}")
            });
            assert!(reply.is_object(), "{text}");
            assert_ne!(result["isError"], true, "{text}");
            assert!(
                reply["next_cursor"].is_string(),
                "plain_replies={plain_replies}: the rest is reachable from the reply itself: {text}"
            );
            if plain_replies {
                // The plain envelope says how to reach the rest, in its own
                // words, inside the JSON.
                assert!(reply["_kin"].to_string().contains("next_cursor"), "{text}");
            } else {
                assert_eq!(
                    reply["_kin"]["response"]["max_chars"],
                    serde_json::json!(AGENT_DEFAULT_RESPONSE_MAX_CHARS),
                    "the lookup was answered under the profile's ceiling, not the 60,000 asked: \
                     {text}"
                );
            }
        }
    }

    /// Every registered tool whose answer can hand back a continuation: the
    /// tool, the reply field that carries the continuation, and the input that
    /// takes it back. Named once, here, with where each continuation is minted.
    ///
    /// `list_file_entities` is in `crate::budget`'s `PAGED_TOOLS` as well. It is
    /// left out because it is retired from the registry
    /// ([`crate::tools::retired_file_operation`]), so no profile serves it.
    const CONTINUATIONS: &[(&str, &str, &str)] = &[
        // `crate::budget`'s `PAGED_TOOLS`: a ranked page, minted by
        // `semantic_locate` itself.
        ("semantic_locate", "next_cursor", "cursor"),
        // `PAGED_TOOLS`: a literal page, minted in `crate::handlers::lexical`.
        (crate::handlers::lexical::TOOL_NAME, "next_cursor", "cursor"),
        // `PAGED_TOOLS`: older changes, minted in `crate::handlers::review`.
        ("entity_history", "next_offset", "offset"),
        // A frozen reference answer: `crate::reference_pages` over
        // `crate::trace_pages`.
        ("find_references", "next_cursor", "cursor"),
        // A frozen walk: `crate::trace_pages`.
        ("trace_data_flow", "next_cursor", "cursor"),
        // A frozen impact answer: `crate::impact_pages`.
        (crate::impact_pages::TOOL, "next_cursor", "cursor"),
        // The status and enrichment pages: `crate::status_pages`.
        ("kin_graph_status", "next_cursor", "cursor"),
        // Older changes: `crate::handlers::provenance`.
        ("kin_provenance_query", "next_offset", "offset"),
        // Draft pages: `crate::entity_drafts`, whose `next_cursor` is passed
        // back as `after`.
        ("kin_draft_list", "next_cursor", "after"),
    ];

    /// The continuations a served tool can hand back: its own, or, for a verb,
    /// those of every registered tool it answers through.
    fn continuations_of(served: &str) -> Vec<(&'static str, &'static str, &'static str)> {
        let handlers = crate::verbs::handlers_of(served);
        CONTINUATIONS
            .iter()
            .copied()
            .filter(|(tool, _, _)| *tool == served || handlers.contains(tool))
            .collect()
    }

    /// Every continuation Kin hands out is callable through the schema the
    /// profile serves.
    ///
    /// A reference answer on the agent profiles said "call again with cursor
    /// set to next_cursor" while the served `refs` schema carried no `cursor`,
    /// and `trace`, `impact` and `history` had the same gap. A client that
    /// holds a model to the advertised schema could not follow the reply, and
    /// `history`'s schema is closed, so there no client could.
    ///
    /// Walks every agent profile: the three named listings, and both routed
    /// surfaces, whose one tool describes each command with the schema the
    /// belt serves. `agent-search` also reaches tools through `kin_tool_call`,
    /// which takes whatever `kin_tool_search` returned, and that is the
    /// registered schema, which the first half of this test holds.
    #[test]
    fn every_continuation_is_callable_through_the_served_schema() {
        let takes = |schema: &serde_json::Value, input: &str| {
            schema
                .get("properties")
                .and_then(|properties| properties.get(input))
                .is_some()
        };

        // The table is the registry's: every row names a registered tool that
        // takes its input, and every registered tool that takes a paging input
        // has a row, so a paged tool added later cannot miss this sweep.
        let registry = crate::tools::tool_definitions();
        for (tool, _, input) in CONTINUATIONS {
            let definition = registry
                .tools
                .iter()
                .find(|definition| definition.name == *tool)
                .unwrap_or_else(|| panic!("{tool} is in CONTINUATIONS and is not registered"));
            assert!(
                takes(&definition.input_schema, input),
                "{tool} is in CONTINUATIONS and does not register {input}"
            );
        }
        for tool in &registry.tools {
            if crate::verbs::is_verb(&tool.name) {
                continue;
            }
            for input in ["cursor", "offset", "after"] {
                if takes(&tool.input_schema, input) {
                    assert!(
                        CONTINUATIONS
                            .iter()
                            .any(|(name, _, taken)| *name == tool.name && *taken == input),
                        "{} registers {input}; add the continuation it takes to CONTINUATIONS",
                        tool.name
                    );
                }
            }
        }

        let mut missing: Vec<String> = Vec::new();
        let mut checked: BTreeSet<(&str, String, &str)> = BTreeSet::new();
        for (profile, names) in [
            ("agent-default", crate::tools::agent_default_tool_names()),
            ("agent-query", crate::tools::agent_query_tool_names()),
            ("agent-search", crate::tools::agent_search_tool_names()),
        ] {
            let served =
                crate::tools::served_tools_list(Some(&crate::tools::name_set(names)), true);
            for tool in &served.tools {
                for (handler, field, input) in continuations_of(&tool.name) {
                    checked.insert((profile, tool.name.clone(), input));
                    if !takes(&tool.input_schema, input) {
                        missing.push(format!(
                            "{profile}/{}: {handler} hands back {field}, and the served schema \
                             has no {input}",
                            tool.name
                        ));
                    }
                }
            }
        }
        for surface in [
            crate::routed::RoutedSurface::WITH_WRITES,
            crate::routed::RoutedSurface::READ_ONLY,
        ] {
            for verb in crate::verbs::VERBS {
                let continuations = continuations_of(verb);
                if continuations.is_empty() {
                    continue;
                }
                let mut call = crate::types::ToolCallParams {
                    name: crate::routed::TOOL_NAME.to_string(),
                    arguments: HashMap::from([
                        ("command".to_string(), serde_json::json!("describe")),
                        ("args".to_string(), serde_json::json!({"command": verb})),
                    ]),
                };
                let crate::routed::Routing::Answer(answer) =
                    crate::routed::route(&mut call, Some(surface))
                else {
                    panic!("{}: describe {verb} was not answered", surface.profile());
                };
                let crate::types::ContentBlock::Text { text } = &answer.content[0];
                let described: serde_json::Value =
                    serde_json::from_str(text).expect("describe answers with JSON");
                let schemas: Vec<&serde_json::Value> = match described.get("variants") {
                    Some(variants) => variants
                        .as_array()
                        .expect("variants")
                        .iter()
                        .map(|variant| &variant["args_schema"])
                        .collect(),
                    None => vec![&described["args_schema"]],
                };
                for (handler, field, input) in continuations {
                    checked.insert((surface.profile(), verb.to_string(), input));
                    for schema in &schemas {
                        if !takes(schema, input) {
                            missing.push(format!(
                                "{}/{verb}: {handler} hands back {field}, and describe serves no \
                                 {input}",
                                surface.profile()
                            ));
                        }
                    }
                }
            }
        }
        assert!(
            missing.is_empty(),
            "a reply hands back a continuation its served schema cannot take: {missing:#?}; add \
             the input to that verb's entry in schema_keep_lists()"
        );

        // And the continuation reaches the tool that minted it: routing the
        // verb call, and the belt's own changes to it, keep the input as sent.
        let routed: &[(&str, serde_json::Value, &str, &str)] = &[
            (
                crate::verbs::LOCATE,
                serde_json::json!({"query": "where retries back off", "cursor": "c"}),
                "semantic_locate",
                "cursor",
            ),
            (
                crate::verbs::REFS,
                serde_json::json!({"query": "parse_config", "cursor": "c"}),
                "find_references",
                "cursor",
            ),
            (
                crate::verbs::TRACE,
                serde_json::json!({"focal": "parse_config", "cursor": "c"}),
                "trace_data_flow",
                "cursor",
            ),
            (
                crate::verbs::IMPACT,
                serde_json::json!({"entity_ids": ["e"], "cursor": "c"}),
                crate::impact_pages::TOOL,
                "cursor",
            ),
            (
                crate::verbs::STATUS,
                serde_json::json!({"cursor": "c"}),
                "kin_graph_status",
                "cursor",
            ),
            (
                crate::verbs::HISTORY,
                serde_json::json!({"entity_id": "e", "offset": 20}),
                "kin_provenance_query",
                "offset",
            ),
        ];
        for (verb, arguments, handler, input) in routed {
            let mut name = (*verb).to_string();
            let mut arguments: HashMap<String, serde_json::Value> =
                serde_json::from_value(arguments.clone()).expect("an argument object");
            let sent = arguments[*input].clone();
            crate::verbs::resolve(&mut name, &mut arguments).expect("a valid verb call");
            assert_eq!(name, *handler, "{verb} routes to the tool that pages");
            agent_profile_overrides(&name, &mut arguments);
            apply_belt_defaults(&name, &mut arguments);
            assert_eq!(
                arguments.get(*input),
                Some(&sent),
                "{verb} lost its {input}"
            );
        }
        // The routing table covers every continuation `agent-default` serves,
        // so it cannot go stale while the sweep above grows.
        let default_pairs: BTreeSet<(String, &str)> = checked
            .iter()
            .filter(|(profile, _, _)| *profile == "agent-default")
            .map(|(_, verb, input)| (verb.clone(), *input))
            .collect();
        let routed_pairs: BTreeSet<(String, &str)> = routed
            .iter()
            .map(|(verb, _, _, input)| ((*verb).to_string(), *input))
            .collect();
        assert_eq!(default_pairs, routed_pairs);
        // Anti-vacuity: the sweep read every paged verb on every profile that
        // serves it.
        assert!(
            checked.len() >= 6 * 2 + 3 + 6 * 2,
            "the sweep checked only {} served continuations: {checked:?}",
            checked.len()
        );
    }

    /// And nothing in the table that no belt profile serves, which would be a
    /// short form nobody reads and a name nobody notices going stale.
    #[test]
    fn the_short_description_table_has_no_orphans() {
        let served: std::collections::HashSet<&str> = crate::tools::agent_default_tool_names()
            .iter()
            .copied()
            .chain(crate::tools::agent_search_tool_names().iter().copied())
            .collect();
        let orphans: Vec<&str> = short_descriptions()
            .keys()
            .copied()
            .filter(|name| !served.contains(name))
            .collect();
        assert!(
            orphans.is_empty(),
            "short_descriptions() in crates/kin-mcp/src/agent_belt.rs has entries for tools no \
             belt profile serves: {orphans:?}; remove them, or put the names back in \
             agent_default_tool_names() or agent_search_tool_names() if the removal was \
             accidental"
        );
    }

    /// The tool list exactly as `tools/list` serves it for `agent-default`.
    fn served_agent_default() -> ToolsListResult {
        crate::tools::served_tools_list(
            Some(&crate::tools::name_set(
                crate::tools::agent_default_tool_names(),
            )),
            true,
        )
    }

    /// The budget this whole file exists to hold. Measured on 2026-09-02 the
    /// served profile was 47,739 characters of description, the seven retrieval
    /// tools averaging 3,586 each and `trace_data_flow` alone at 6,823.
    #[test]
    fn no_agent_default_description_exceeds_its_budget() {
        let over: Vec<(String, usize)> = served_agent_default()
            .tools
            .into_iter()
            .map(|tool| (tool.name, tool.description.len()))
            .filter(|(_, len)| *len > AGENT_DEFAULT_DESCRIPTION_BUDGET)
            .collect();
        assert!(
            over.is_empty(),
            "over the {AGENT_DEFAULT_DESCRIPTION_BUDGET}-character per-tool budget: {over:?}"
        );
    }

    /// And the total, so twenty tools each sitting just under the per-tool cap
    /// cannot pass for a profile that learned something.
    #[test]
    fn the_agent_default_profile_stays_under_its_total_budget() {
        let total: usize = served_agent_default()
            .tools
            .iter()
            .map(|tool| tool.description.len())
            .sum();
        assert!(
            total <= AGENT_DEFAULT_PROFILE_DESCRIPTION_BUDGET,
            "agent-default descriptions total {total}, over the \
             {AGENT_DEFAULT_PROFILE_DESCRIPTION_BUDGET} budget"
        );
        // The control: the same tools' registered descriptions must be far
        // larger, or this test is passing against a surface that was already
        // short and the compaction is doing nothing.
        let served: std::collections::HashSet<&str> = crate::tools::agent_default_tool_names()
            .iter()
            .copied()
            .collect();
        let registered: usize = crate::tools::tool_definitions()
            .tools
            .iter()
            .filter(|tool| served.contains(tool.name.as_str()))
            .map(|tool| tool.description.len())
            .sum();
        assert!(
            registered > total * 3,
            "the long forms must be much larger than the short ones: \
             registered {registered}, served {total}"
        );
    }

    /// The `full` profile must keep every word. The short forms are a serving
    /// concern and must not have edited the registry.
    #[test]
    fn the_full_profile_keeps_the_long_descriptions() {
        let full = crate::tools::served_tools_list(None, false);
        let locate = full
            .tools
            .iter()
            .find(|t| t.name == crate::verbs::LOCATE)
            .expect("full serves locate");
        let registry = crate::tools::surface_definitions();
        let registered = registry
            .tools
            .iter()
            .find(|t| t.name == crate::verbs::LOCATE)
            .unwrap();
        assert_eq!(
            locate.description, registered.description,
            "full serves the complete current registry description"
        );
        let short = short_descriptions();
        assert!(locate.description.len() > short[crate::verbs::LOCATE].len());
        let full = crate::tools::tool_definitions();
        assert!(
            full.tools
                .iter()
                .any(|t| t.name == DECLARATION_FILTER_CANONICAL),
            "the registry must still hold the tool under its registered name"
        );
        assert!(
            !full
                .tools
                .iter()
                .any(|t| t.name == DECLARATION_FILTER_ALIAS),
            "the alias is accepted on a call, never registered and never served"
        );
    }

    /// `agent-default` serves the verbs and nothing else, and a call by any
    /// name a verb replaced, the declaration filter's old served name included,
    /// still reaches the handler it always did.
    #[test]
    fn agent_default_serves_the_verbs_and_accepts_every_old_name() {
        let served: BTreeSet<String> = served_agent_default()
            .tools
            .into_iter()
            .map(|tool| tool.name)
            .collect();
        let verbs: BTreeSet<String> = crate::verbs::VERBS
            .iter()
            .filter(|verb| **verb != crate::verbs::SESSION)
            .map(|verb| verb.to_string())
            .collect();
        assert_eq!(
            served, verbs,
            "agent-default serves the public working verbs; writes own their session"
        );
        for (alias, verb) in crate::verbs::ALIASES {
            assert!(!served.contains(alias), "agent-default serves {alias}");
            assert_eq!(
                crate::verbs::allows(
                    &crate::tools::name_set(crate::tools::agent_default_tool_names()),
                    alias
                ),
                verb != crate::verbs::SESSION,
                "only explicit lifecycle is withheld from agent-default: {alias} -> {verb}"
            );
            let mut name = alias.to_string();
            // The retired session verb still routes by its action, so it is
            // called in a form that names one.
            let mut arguments: HashMap<String, serde_json::Value> =
                if alias == crate::verbs::SESSION {
                    HashMap::from([
                        ("action".to_string(), serde_json::json!("end")),
                        ("session_id".to_string(), serde_json::json!("s")),
                    ])
                } else {
                    HashMap::new()
                };
            crate::verbs::resolve(&mut name, &mut arguments).expect("an old name resolves");
            if alias == crate::verbs::SESSION {
                assert_eq!(
                    name, "kin_session_end",
                    "the retired session verb still routes"
                );
            } else {
                assert_eq!(name, alias, "an old name reaches its own handler");
            }
        }
        assert_eq!(
            canonical_tool_name(DECLARATION_FILTER_ALIAS),
            DECLARATION_FILTER_CANONICAL
        );
        let mut name = DECLARATION_FILTER_ALIAS.to_string();
        crate::verbs::resolve(&mut name, &mut HashMap::new()).expect("the alias resolves");
        assert_eq!(name, DECLARATION_FILTER_CANONICAL);
        // And nothing else is rewritten by the name-only canonicalizer.
        for name in ["semantic_locate", "find_references", "get_entity_source"] {
            assert_eq!(canonical_tool_name(name), name);
        }
    }

    /// Every tool name the shipped proofs assert by name must be served under
    /// exactly that name on `agent-default`, and no tool may be served under a
    /// name the registry does not carry.
    ///
    /// Two proofs read `tools/list` and assert a tool name literally.
    /// `.github/workflows/install-proof.yml`, in its "Graph query and MCP
    /// tool-call proof" step, throws `MCP tools/list omitted locate` and then
    /// calls that tool twice through `tools/call`.
    /// `scripts/prove-windows-npm-first-run.mjs` asserts
    /// `toolNames.includes('locate')` for both npm entrypoints. Both jobs are
    /// `skipped` on a pull request and graded only on `main`'s push run, so
    /// before this test nothing per-PR could see a served name move: the change
    /// that introduced this module served `semantic_search` as
    /// `find_declarations`, and both proofs stayed red for four landings. The
    /// move to verbs moved both proofs with it.
    ///
    /// The second assertion is the general form of that class. A served name
    /// the registry does not carry is a rename by another route, whatever tool
    /// it lands on, and it fails here rather than on `main` a landing later. If
    /// you added a name to one of those two files, add it to
    /// `PROOF_ASSERTED_NAMES` as well. If this test is in your way because you
    /// meant to move a served name, that move takes those two files,
    /// `docs/mcp-tools.md` and the acceptance suite with it, in one change.
    #[test]
    fn agent_default_serves_every_name_the_shipped_proofs_assert() {
        // Read out of the two files named above. Both assert `locate` and no
        // other tool name.
        const PROOF_ASSERTED_NAMES: &[&str] = &[crate::verbs::LOCATE];

        let served: Vec<String> = served_agent_default()
            .tools
            .into_iter()
            .map(|tool| tool.name)
            .collect();
        for name in PROOF_ASSERTED_NAMES {
            assert!(
                served.iter().any(|candidate| candidate == name),
                "the shipped proofs call {name} by name and agent-default serves {served:?}"
            );
        }

        let registered: Vec<String> = crate::tools::tool_definitions()
            .tools
            .into_iter()
            .map(|tool| tool.name)
            .collect();
        let unregistered: Vec<&String> = served
            .iter()
            .filter(|name| !registered.contains(*name))
            .collect();
        assert!(
            unregistered.is_empty(),
            "agent-default serves {unregistered:?} under a name the registry does not carry, \
             which is a public rename no per-PR check but this one can see"
        );
    }

    /// No served property description may run past its budget.
    ///
    /// Property prose was 10,153 bytes of the served schemas outside the two
    /// exempt transaction tools, and one clause, `max_chars`, carried the same
    /// 649 characters on seven tools. The shape, bounds and default of a
    /// property are already machine-readable beside its description, so prose
    /// that restates them is paid for on every `tools/list` a small model reads.
    ///
    /// Top-level properties on every served tool. Prose nested inside a served
    /// schema, such as `kin_mutate`'s payload forms, is held by
    /// `every_served_description_is_at_most_half_its_2026_09_22_base` instead,
    /// because a form stated inside a payload needs more than one clause.
    ///
    /// Nothing in the acceptance suite reads a property description, checked
    /// rather than assumed: `magic_repro.py` `check_6` tests for the presence of
    /// the `include_body` or `compact` KEY, `check_14` arm 3 reads the TOOL
    /// description, and `response_budget_elisions.py` `grade_advertised_budget`
    /// reads only `maximum`, `default` and `minimum`. So this budget binds
    /// without moving a check.
    #[test]
    fn no_agent_default_property_description_exceeds_its_budget() {
        let over: Vec<(String, String, usize)> = served_agent_default()
            .tools
            .into_iter()
            .flat_map(|tool| {
                let properties = tool
                    .input_schema
                    .get("properties")
                    .and_then(|value| value.as_object())
                    .cloned()
                    .unwrap_or_default();
                properties
                    .into_iter()
                    .filter_map(|(name, property)| {
                        let length = property.get("description")?.as_str()?.chars().count();
                        (length > AGENT_DEFAULT_PROPERTY_DESCRIPTION_BUDGET).then_some((
                            tool.name.clone(),
                            name,
                            length,
                        ))
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        assert!(
            over.is_empty(),
            "over the {AGENT_DEFAULT_PROPERTY_DESCRIPTION_BUDGET}-character property budget: \
             {over:?}; give each one a single clause in shared_property_descriptions or \
             tool_property_descriptions"
        );
    }

    /// Every description `agent-default` served at 97c719c8d, measured on
    /// 2026-09-22 from the listing a client receives: the tool, a JSON pointer
    /// into its input schema to the node carrying the description (empty for the
    /// tool's own description), and the bytes it carried. `agent-query` serves a
    /// subset of these, byte for byte the same, so this is its base as well.
    const BASE_2026_09_22: &[(&str, &str, usize)] = &[
        ("find_references", "", 148),
        ("find_references", "/properties/answer_only", 88),
        ("find_references", "/properties/entity_id", 49),
        ("find_references", "/properties/max_chars", 55),
        ("find_references", "/properties/query", 83),
        ("find_references", "/properties/relation_kinds", 62),
        ("get_context_pack", "", 173),
        ("get_context_pack", "/properties/depth", 26),
        ("get_context_pack", "/properties/entities", 80),
        ("get_context_pack", "/properties/entity_id", 17),
        ("get_context_pack", "/properties/max_chars", 55),
        ("get_context_pack", "/properties/question", 80),
        ("get_context_pack", "/properties/token_budget", 36),
        ("get_entity_source", "", 79),
        ("get_entity_source", "/properties/entity_id", 11),
        ("graph_neighborhood", "", 150),
        ("graph_neighborhood", "/properties/depth", 15),
        ("graph_neighborhood", "/properties/direction", 80),
        ("graph_neighborhood", "/properties/entity_id", 11),
        ("graph_neighborhood", "/properties/limit", 35),
        ("graph_neighborhood", "/properties/max_chars", 55),
        ("impact_analysis", "", 140),
        ("impact_analysis", "/properties/base", 29),
        ("impact_analysis", "/properties/change_ids", 45),
        ("impact_analysis", "/properties/entity_ids", 34),
        ("impact_analysis", "/properties/files", 85),
        ("impact_analysis", "/properties/head", 29),
        ("impact_analysis", "/properties/max_chars", 55),
        ("kin_artifact_list", "", 135),
        ("kin_artifact_list", "/properties/source_change_id", 62),
        ("kin_graph_status", "", 174),
        ("kin_mutate", "", 141),
        ("kin_mutate", "/properties/operations", 39),
        (
            "kin_mutate",
            "/properties/operations/items/properties/body",
            92,
        ),
        (
            "kin_mutate",
            "/properties/operations/items/properties/description",
            48,
        ),
        (
            "kin_mutate",
            "/properties/operations/items/properties/target",
            89,
        ),
        (
            "kin_mutate",
            "/properties/operations/items/properties/verb",
            72,
        ),
        ("kin_mutate", "/properties/summary", 79),
        ("kin_provenance_query", "", 140),
        ("kin_provenance_query", "/properties/entity_id", 35),
        ("kin_provenance_query", "/properties/limit", 33),
        ("kin_session_end", "", 104),
        ("kin_session_end", "/properties/session_id", 12),
        ("kin_session_heartbeat", "", 105),
        ("kin_session_heartbeat", "/properties/session_id", 12),
        ("kin_session_start", "", 123),
        ("kin_session_start", "/properties/capabilities", 18),
        ("kin_session_start", "/properties/client_name", 26),
        ("kin_session_start", "/properties/cwd", 30),
        ("kin_session_start", "/properties/pid", 37),
        ("kin_session_start", "/properties/session_id", 76),
        ("kin_session_start", "/properties/transport", 41),
        ("kin_session_start", "/properties/vendor", 56),
        ("kin_transaction_abort", "", 146),
        ("kin_transaction_abort", "/properties/session_id", 76),
        ("kin_transaction_abort", "/properties/transaction_id", 16),
        ("kin_transaction_begin", "", 134),
        ("kin_transaction_begin", "/properties/scope", 42),
        ("kin_transaction_begin", "/properties/session_id", 35),
        ("kin_transaction_commit", "", 144),
        ("kin_transaction_commit", "/properties/message", 232),
        ("kin_transaction_commit", "/properties/operations", 77),
        // The unguarded body edit that was branch 0 is retired. Its body, verb and
        // description prose now ride the guarded body edit, branch 1, so those rows
        // follow it there; its name-or-UUID target has no successor, and neither
        // does the structured branch's body.
        (
            "kin_transaction_commit",
            "/properties/operations/items/oneOf/1/properties/body",
            125,
        ),
        (
            "kin_transaction_commit",
            "/properties/operations/items/oneOf/1/properties/description",
            42,
        ),
        (
            "kin_transaction_commit",
            "/properties/operations/items/oneOf/1/properties/verb",
            33,
        ),
        (
            "kin_transaction_commit",
            "/properties/operations/items/oneOf/1/properties/target",
            37,
        ),
        (
            "kin_transaction_commit",
            "/properties/operations/items/oneOf/2/properties/description",
            42,
        ),
        (
            "kin_transaction_commit",
            "/properties/operations/items/oneOf/2/properties/payload",
            132,
        ),
        (
            "kin_transaction_commit",
            "/properties/operations/items/oneOf/2/properties/target",
            88,
        ),
        (
            "kin_transaction_commit",
            "/properties/operations/items/oneOf/2/properties/verb",
            33,
        ),
        ("kin_transaction_commit", "/properties/session_id", 126),
        ("kin_transaction_commit", "/properties/transaction_id", 16),
        ("kin_transaction_stage", "", 165),
        ("kin_transaction_stage", "/properties/operations", 37),
        // The unguarded body edit that was branch 0 is retired. Its body, verb and
        // description prose now ride the guarded body edit, branch 1, so those rows
        // follow it there; its name-or-UUID target has no successor, and neither
        // does the structured branch's body.
        (
            "kin_transaction_stage",
            "/properties/operations/items/oneOf/1/properties/body",
            125,
        ),
        (
            "kin_transaction_stage",
            "/properties/operations/items/oneOf/1/properties/description",
            42,
        ),
        (
            "kin_transaction_stage",
            "/properties/operations/items/oneOf/1/properties/verb",
            33,
        ),
        (
            "kin_transaction_stage",
            "/properties/operations/items/oneOf/1/properties/target",
            37,
        ),
        (
            "kin_transaction_stage",
            "/properties/operations/items/oneOf/2/properties/description",
            42,
        ),
        (
            "kin_transaction_stage",
            "/properties/operations/items/oneOf/2/properties/payload",
            132,
        ),
        (
            "kin_transaction_stage",
            "/properties/operations/items/oneOf/2/properties/target",
            88,
        ),
        (
            "kin_transaction_stage",
            "/properties/operations/items/oneOf/2/properties/verb",
            33,
        ),
        ("kin_transaction_stage", "/properties/session_id", 126),
        ("kin_transaction_stage", "/properties/transaction_id", 16),
        ("lexical_lookup", "", 173),
        ("lexical_lookup", "/properties/cursor", 87),
        ("lexical_lookup", "/properties/kind", 82),
        ("lexical_lookup", "/properties/limit", 47),
        ("lexical_lookup", "/properties/literal", 82),
        ("lexical_lookup", "/properties/max_chars", 55),
        ("list_file_entities", "", 148),
        ("list_file_entities", "/properties/cursor", 72),
        ("list_file_entities", "/properties/page_size", 82),
        ("list_file_entities", "/properties/path", 87),
        ("semantic_locate", "", 150),
        ("semantic_locate", "/properties/cursor", 72),
        ("semantic_locate", "/properties/granularity", 62),
        ("semantic_locate", "/properties/include_tests", 79),
        ("semantic_locate", "/properties/limit", 77),
        ("semantic_locate", "/properties/max_chars", 55),
        ("semantic_locate", "/properties/query", 85),
        ("semantic_search", "", 157),
        ("semantic_search", "/properties/kind", 84),
        ("semantic_search", "/properties/language", 40),
        ("semantic_search", "/properties/limit", 21),
        ("semantic_search", "/properties/max_chars", 55),
        ("semantic_search", "/properties/query", 26),
        ("trace_data_flow", "", 143),
        ("trace_data_flow", "/properties/compact", 77),
        ("trace_data_flow", "/properties/depth", 63),
        ("trace_data_flow", "/properties/direction", 77),
        ("trace_data_flow", "/properties/focal", 60),
        ("trace_data_flow", "/properties/include_body", 83),
        ("trace_data_flow", "/properties/limit_per_step", 71),
        ("trace_data_flow", "/properties/max_chars", 55),
        ("trace_data_flow", "/properties/max_response_chars", 53),
        ("trace_data_flow", "/properties/target", 85),
        ("trace_path", "", 152),
        ("trace_path", "/properties/direction", 84),
        ("trace_path", "/properties/from", 88),
        ("trace_path", "/properties/from_file", 83),
        ("trace_path", "/properties/limit", 76),
        ("trace_path", "/properties/max_chars", 55),
        ("trace_path", "/properties/max_depth", 84),
        ("trace_path", "/properties/to", 30),
        ("trace_path", "/properties/to_file", 83),
    ];

    /// Why a base description is no longer served, since 2026-09-28.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Withdrawal {
        /// The property is still served. Its name, type, enum, bounds and
        /// default say what the prose said, so it is served with none.
        SaidBySchema,
        /// The property is no longer advertised on this profile: deprecated,
        /// an alias served beside its canonical spelling, a one-value enum, or
        /// an optional field the handler never needs. The handler still reads
        /// it by name.
        Hidden,
        /// A pointer inside the transaction tools' nested operation contract.
        /// Stage and commit take `kin_mutate`'s operations, which are served
        /// once, on `kin_mutate`.
        SharedWithMutate,
        /// The property is still served, and the served tool description
        /// already says what its prose said, so it is served with none.
        SaidByTool,
        /// The property is still served, and the forms its prose named are
        /// what the refusal of a call that needs one lists: a focal name
        /// several entities carry is refused with every candidate's
        /// `qualified_name`. Withdrawn on 2026-10-04 to pay for `trace`'s
        /// `scope` inside the same ceilings.
        SaidByRefusal,
        /// The property is still served, and every reply that hands back the
        /// value it takes says how to send it: a paged reply ends "call again
        /// with cursor set to next_cursor". Withdrawn on 2026-10-04 with the
        /// lean-schema cut.
        SaidByReply,
    }

    /// Base rows the 2026-09-28 cut withdrew on purpose, by tool and pointer
    /// prefix, each with the rule that took it, and the rows the same rule
    /// took on 2026-09-29 to pay for the continuation parameters. Every row is
    /// checked to be really withdrawn, so this cannot quietly excuse a
    /// description that is still served long.
    const WITHDRAWN_2026_09_28: &[(&str, &str, Withdrawal)] = &[
        // 2026-09-29: the served context description says what a question does
        // beside named focals.
        (
            "get_context_pack",
            "/properties/question",
            Withdrawal::SaidBySchema,
        ),
        // 2026-09-29: the served enum names the three reference classes.
        (
            "find_references",
            "/properties/relation_kinds",
            Withdrawal::SaidBySchema,
        ),
        // 2026-09-29: prose that only restated the property's name, withdrawn
        // so `refs`, `trace` and `impact` could serve `cursor` and `history`
        // `offset` inside both listing ceilings.
        (
            "get_context_pack",
            "/properties/depth",
            Withdrawal::SaidBySchema,
        ),
        (
            "graph_neighborhood",
            "/properties/depth",
            Withdrawal::SaidBySchema,
        ),
        (
            "impact_analysis",
            "/properties/change_ids",
            Withdrawal::SaidBySchema,
        ),
        (
            "impact_analysis",
            "/properties/entity_ids",
            Withdrawal::SaidBySchema,
        ),
        (
            "semantic_locate",
            "/properties/include_tests",
            Withdrawal::SaidBySchema,
        ),
        (
            "semantic_search",
            "/properties/language",
            Withdrawal::SaidBySchema,
        ),
        (
            "trace_data_flow",
            "/properties/depth",
            Withdrawal::SaidBySchema,
        ),
        (
            "trace_data_flow",
            "/properties/limit_per_step",
            Withdrawal::SaidBySchema,
        ),
        ("trace_path", "/properties/to", Withdrawal::SaidBySchema),
        (
            "find_references",
            "/properties/max_chars",
            Withdrawal::SaidBySchema,
        ),
        (
            "get_context_pack",
            "/properties/max_chars",
            Withdrawal::SaidBySchema,
        ),
        (
            "get_context_pack",
            "/properties/token_budget",
            Withdrawal::SaidBySchema,
        ),
        (
            "graph_neighborhood",
            "/properties/limit",
            Withdrawal::SaidBySchema,
        ),
        (
            "graph_neighborhood",
            "/properties/max_chars",
            Withdrawal::SaidBySchema,
        ),
        ("impact_analysis", "/properties/files", Withdrawal::Hidden),
        (
            "impact_analysis",
            "/properties/max_chars",
            Withdrawal::SaidBySchema,
        ),
        (
            "kin_mutate",
            "/properties/operations/items/properties/description",
            Withdrawal::SaidBySchema,
        ),
        (
            "kin_mutate",
            "/properties/operations/items/properties/verb",
            Withdrawal::SaidBySchema,
        ),
        (
            "kin_provenance_query",
            "/properties/limit",
            Withdrawal::SaidBySchema,
        ),
        (
            "kin_session_start",
            "/properties/capabilities",
            Withdrawal::SaidBySchema,
        ),
        (
            "kin_session_start",
            "/properties/client_name",
            Withdrawal::SaidBySchema,
        ),
        (
            "kin_session_start",
            "/properties/cwd",
            Withdrawal::SaidBySchema,
        ),
        ("kin_session_start", "/properties/pid", Withdrawal::Hidden),
        (
            "kin_session_start",
            "/properties/session_id",
            Withdrawal::Hidden,
        ),
        (
            "kin_session_start",
            "/properties/transport",
            Withdrawal::Hidden,
        ),
        (
            "kin_transaction_abort",
            "/properties/session_id",
            Withdrawal::Hidden,
        ),
        (
            "kin_transaction_commit",
            "/properties/operations/items/",
            Withdrawal::SharedWithMutate,
        ),
        (
            "kin_transaction_commit",
            "/properties/session_id",
            Withdrawal::Hidden,
        ),
        (
            "kin_transaction_commit",
            "/properties/transaction_id",
            Withdrawal::SaidBySchema,
        ),
        (
            "kin_transaction_stage",
            "/properties/operations/items/",
            Withdrawal::SharedWithMutate,
        ),
        (
            "kin_transaction_stage",
            "/properties/session_id",
            Withdrawal::Hidden,
        ),
        (
            "kin_transaction_stage",
            "/properties/transaction_id",
            Withdrawal::SaidBySchema,
        ),
        (
            "lexical_lookup",
            "/properties/limit",
            Withdrawal::SaidBySchema,
        ),
        (
            "lexical_lookup",
            "/properties/max_chars",
            Withdrawal::SaidBySchema,
        ),
        (
            "semantic_locate",
            "/properties/granularity",
            Withdrawal::Hidden,
        ),
        (
            "semantic_locate",
            "/properties/limit",
            Withdrawal::SaidBySchema,
        ),
        (
            "semantic_locate",
            "/properties/max_chars",
            Withdrawal::SaidBySchema,
        ),
        (
            "semantic_search",
            "/properties/limit",
            Withdrawal::SaidBySchema,
        ),
        (
            "semantic_search",
            "/properties/max_chars",
            Withdrawal::SaidBySchema,
        ),
        ("trace_data_flow", "/properties/compact", Withdrawal::Hidden),
        (
            "trace_data_flow",
            "/properties/direction",
            Withdrawal::SaidBySchema,
        ),
        // Since 2026-09-29 both walk spellings are aliases of the one served
        // budget name, which carries no prose.
        (
            "trace_data_flow",
            "/properties/max_chars",
            Withdrawal::SaidBySchema,
        ),
        (
            "trace_data_flow",
            "/properties/max_response_chars",
            Withdrawal::SaidBySchema,
        ),
        (
            "trace_path",
            "/properties/direction",
            Withdrawal::SaidBySchema,
        ),
        ("trace_path", "/properties/from_file", Withdrawal::Hidden),
        (
            "trace_path",
            "/properties/max_chars",
            Withdrawal::SaidBySchema,
        ),
        ("trace_path", "/properties/to_file", Withdrawal::Hidden),
        // 2026-10-03: every agent profile re-sends its definitions on every
        // model turn, so prose the served tool description, name or default
        // already carries went. `locate` says it finds by name or by what code
        // does; `mutate` that it commits atomically; `trace` serves
        // `include_body` with its false default; a commit's `message` and
        // `summary` and a `transaction_id` say themselves.
        (
            "semantic_locate",
            "/properties/query",
            Withdrawal::SaidByTool,
        ),
        (
            "semantic_search",
            "/properties/query",
            Withdrawal::SaidByTool,
        ),
        (
            "lexical_lookup",
            "/properties/literal",
            Withdrawal::SaidByTool,
        ),
        (
            "kin_mutate",
            "/properties/operations",
            Withdrawal::SaidByTool,
        ),
        (
            "kin_mutate",
            "/properties/summary",
            Withdrawal::SaidBySchema,
        ),
        (
            "trace_data_flow",
            "/properties/include_body",
            Withdrawal::SaidBySchema,
        ),
        (
            "kin_transaction_commit",
            "/properties/message",
            Withdrawal::SaidBySchema,
        ),
        (
            "kin_transaction_abort",
            "/properties/transaction_id",
            Withdrawal::SaidBySchema,
        ),
        // 2026-10-04: `trace` serves `scope`, the module a walk stays inside,
        // and pays for it here. The tool description names it, and the forms
        // a focal takes are listed by the refusal of a name that needs one.
        (
            "trace_data_flow",
            "/properties/focal",
            Withdrawal::SaidByRefusal,
        ),
        ("trace_path", "/properties/from", Withdrawal::SaidByRefusal),
        // 2026-10-04: a paged reply says how to continue it, so `locate`
        // serves `cursor` with no prose, as `trace` already did.
        (
            "semantic_locate",
            "/properties/cursor",
            Withdrawal::SaidByReply,
        ),
        (
            "lexical_lookup",
            "/properties/cursor",
            Withdrawal::SaidByReply,
        ),
    ];

    /// The withdrawal that covers one base row, if any, with its index.
    fn withdrawal(tool: &str, pointer: &str) -> Option<(usize, Withdrawal)> {
        WITHDRAWN_2026_09_28
            .iter()
            .enumerate()
            .find(|(_, (name, prefix, rule))| {
                *name == tool
                    && if *rule == Withdrawal::SharedWithMutate {
                        pointer.starts_with(prefix)
                    } else {
                        pointer == *prefix
                    }
            })
            .map(|(index, (_, _, rule))| (index, *rule))
    }

    /// Whether a JSON pointer into a served schema resolves.
    fn resolves(schema: &serde_json::Value, pointer: &str) -> bool {
        pointer.is_empty() || schema.pointer(pointer).is_some()
    }

    /// Every description in one served tool: its own, keyed on the empty
    /// pointer, and every `description` string anywhere in its input schema,
    /// keyed on the JSON pointer to the node that carries it.
    fn served_descriptions(tool: &crate::types::ToolDefinition) -> BTreeMap<String, usize> {
        fn walk(pointer: &str, value: &serde_json::Value, out: &mut BTreeMap<String, usize>) {
            match value {
                serde_json::Value::Object(map) => {
                    for (key, child) in map {
                        match child.as_str() {
                            Some(text) if key == "description" => {
                                out.insert(pointer.to_string(), text.len());
                            }
                            _ => walk(&format!("{pointer}/{key}"), child, out),
                        }
                    }
                }
                serde_json::Value::Array(items) => {
                    for (index, child) in items.iter().enumerate() {
                        walk(&format!("{pointer}/{index}"), child, out);
                    }
                }
                _ => {}
            }
        }
        let mut out = BTreeMap::from([(String::new(), tool.description.len())]);
        walk("", &tool.input_schema, &mut out);
        out
    }

    /// Where a base row is served now: the verb that replaced its tool, and the
    /// pointer that carries the same field there. The literal lookup's
    /// `literal` is `locate`'s `query`, and the route tool's `from` is `trace`'s
    /// `focal`; every other field keeps its name.
    fn served_as(tool: &'static str, pointer: &'static str) -> (&'static str, String) {
        let verb = crate::verbs::verb_for(tool).unwrap_or(tool);
        let pointer = match (tool, pointer) {
            ("lexical_lookup", "/properties/literal") => "/properties/query",
            ("trace_path", "/properties/from") => "/properties/focal",
            // One served name for every reply budget.
            (_, "/properties/max_chars" | "/properties/max_response_chars") => {
                "/properties/max_output_bytes"
            }
            _ => pointer,
        };
        (verb, pointer.to_string())
    }

    /// Base rows the move to verbs withdrew on purpose. The verb reads each of
    /// these fields from the call and does not serve it: the route tool's hop
    /// and route bounds are read from `depth` or left at their defaults, its
    /// budget is served under the walk's spelling, and the walk's `target` is
    /// what `to` replaced.
    const WITHDRAWN_FOR_VERBS: &[(&str, &str)] = &[
        ("trace_path", "/properties/limit"),
        ("trace_path", "/properties/max_depth"),
        // The agent profiles always answer in the answer-only shape, so the
        // toggle is not served to them.
        ("find_references", "/properties/answer_only"),
        ("trace_data_flow", "/properties/target"),
        // An agent profile stages through `mutate` with a `transaction_id`, so
        // the operations are served once, there, and `transaction` begins,
        // commits and aborts. The full profile still serves them on both.
        ("kin_transaction_stage", "/properties/operations"),
        ("kin_transaction_commit", "/properties/operations"),
        // A write opens its connection's session itself, so an agent profile
        // serves `exec` as the toolchain run and no profile lists the fields
        // that start, name or keep a session. Hidden aliases still take them.
        ("kin_session_start", "/properties/capabilities"),
        ("kin_session_start", "/properties/client_name"),
        ("kin_session_start", "/properties/cwd"),
        ("kin_session_start", "/properties/vendor"),
        ("kin_session_start", "/properties/session_id"),
        ("kin_session_heartbeat", "/properties/session_id"),
        ("kin_session_end", "/properties/session_id"),
        ("kin_transaction_begin", "/properties/session_id"),
        ("kin_transaction_stage", "/properties/session_id"),
        ("kin_transaction_commit", "/properties/session_id"),
        ("kin_transaction_abort", "/properties/session_id"),
    ];

    /// Every description `agent-default` and `agent-query` serve is at most half
    /// its bytes at 97c719c8d, and every parameter that carried one is still
    /// served, except where [`WITHDRAWN_2026_09_28`] or [`WITHDRAWN_FOR_VERBS`]
    /// names the rule that took it.
    ///
    /// The whole served list rides every request an eager client sends, and in
    /// the corrected rerun pilot the `agent-query` list alone was about 3,500
    /// tokens a request against about 1,100 for the raw arm's four tools, nearly
    /// all of it parameter prose. Read per description rather than as a total,
    /// because a total halves while one long clause survives untouched, and the
    /// pointer has to resolve, so a parameter trimmed off the served schema
    /// fails here as a dropped parameter rather than passing as a short one.
    ///
    /// The base was measured on the tools the verbs replaced. Each row is read
    /// where its field is served now, by [`served_as`], and a description that
    /// several base rows fold into, such as `session`'s own, which replaced
    /// three tools' descriptions, is held to half their total.
    ///
    /// A withdrawal is graded too: a property withdrawn only for its prose must
    /// still be served, and a property withdrawn outright must be gone unless
    /// another folded tool's row still serves it, so the tables cannot hide a
    /// dropped parameter.
    #[test]
    fn every_served_description_is_at_most_half_its_2026_09_22_base() {
        let mut problems: Vec<String> = Vec::new();
        let mut graded = 0usize;
        let mut withdrawals_used = std::collections::BTreeSet::new();
        let mut verb_withdrawals_used = std::collections::BTreeSet::new();
        let retired = |tool: &str| {
            crate::tools::retired_file_operation(tool).is_some()
                || crate::verbs::verb_for(tool) == Some(crate::verbs::SESSION)
        };
        // Whether a served pointer is also where a base row nothing withdrew
        // is read, so a field one folded tool dropped and another keeps is
        // graded on the row that keeps it.
        let kept_by_another = |tool: &str, verb: &str, pointer: &str| {
            BASE_2026_09_22.iter().any(|(other, at, _)| {
                *other != tool
                    && withdrawal(other, at).is_none()
                    && !WITHDRAWN_FOR_VERBS.contains(&(*other, *at))
                    && served_as(other, at) == (verb, pointer.to_string())
            })
        };
        for names in [
            crate::tools::agent_default_tool_names(),
            crate::tools::agent_query_tool_names(),
        ] {
            let served =
                crate::tools::served_tools_list(Some(&crate::tools::name_set(names)), true);
            let by_name: BTreeMap<&str, (BTreeMap<String, usize>, &serde_json::Value)> = served
                .tools
                .iter()
                .map(|tool| {
                    (
                        tool.name.as_str(),
                        (served_descriptions(tool), &tool.input_schema),
                    )
                })
                .collect();
            // The bases that fold into each served pointer, summed.
            let mut folded: BTreeMap<(&str, String), usize> = BTreeMap::new();
            for (tool, pointer, base) in BASE_2026_09_22 {
                if retired(tool) {
                    continue;
                }
                let (verb, served_pointer) = served_as(tool, pointer);
                if !names.contains(&verb) {
                    continue;
                }
                graded += 1;
                let Some((descriptions, schema)) = by_name.get(verb) else {
                    problems.push(format!("{verb} is no longer served"));
                    continue;
                };
                if let Some(index) = WITHDRAWN_FOR_VERBS
                    .iter()
                    .position(|row| *row == (*tool, *pointer))
                {
                    verb_withdrawals_used.insert(index);
                    if let Some((earlier, _)) = withdrawal(tool, pointer) {
                        withdrawals_used.insert(earlier);
                    }
                    if resolves(schema, &served_pointer)
                        && !kept_by_another(tool, verb, &served_pointer)
                    {
                        problems.push(format!(
                            "{tool}{pointer} is withdrawn for the verbs and {verb}{served_pointer} \
                             is still served"
                        ));
                    }
                    continue;
                }
                if let Some((index, rule)) = withdrawal(tool, pointer) {
                    withdrawals_used.insert(index);
                    let elsewhere = kept_by_another(tool, verb, &served_pointer);
                    if let Some(now) = descriptions.get(&served_pointer) {
                        if !elsewhere && !served_pointer.is_empty() {
                            problems.push(format!(
                                "{tool}{pointer} is withdrawn as {rule:?} and {verb}{served_pointer} \
                                 still serves a {now}-byte description"
                            ));
                        }
                    }
                    if matches!(
                        rule,
                        Withdrawal::SaidBySchema
                            | Withdrawal::SaidByTool
                            | Withdrawal::SaidByRefusal
                            | Withdrawal::SaidByReply
                    ) && !resolves(schema, &served_pointer)
                    {
                        problems.push(format!(
                            "{tool}{pointer} lost only its prose by rule and the property itself \
                             is no longer served"
                        ));
                    }
                    if rule == Withdrawal::Hidden && resolves(schema, &served_pointer) && !elsewhere
                    {
                        problems.push(format!(
                            "{tool}{pointer} is withdrawn as {rule:?} and is still served"
                        ));
                    }
                    continue;
                }
                *folded.entry((verb, served_pointer)).or_default() += base;
            }
            for ((verb, pointer), base) in &folded {
                let (descriptions, _) = &by_name[verb];
                match descriptions.get(pointer) {
                    None => problems.push(format!(
                        "{verb}{pointer} carried {base} bytes of description across the tools it \
                         replaced and is no longer served with one, so a parameter was dropped or \
                         its prose deleted"
                    )),
                    Some(now) if now * 2 > *base => problems.push(format!(
                        "{verb}{pointer} is {now} bytes, over half its {base}-byte base"
                    )),
                    Some(_) => {}
                }
            }
        }
        assert!(problems.is_empty(), "{problems:#?}");
        let graded_on = |names: &[&str]| {
            BASE_2026_09_22
                .iter()
                .filter(|(tool, pointer, _)| {
                    !retired(tool) && names.contains(&served_as(tool, pointer).0)
                })
                .count()
        };
        assert_eq!(
            graded,
            graded_on(crate::tools::agent_default_tool_names())
                + graded_on(crate::tools::agent_query_tool_names()),
            "the sweep did not grade every base description on both profiles"
        );
        assert_eq!(
            graded_on(crate::tools::agent_default_tool_names()),
            BASE_2026_09_22
                .iter()
                .filter(|(tool, _, _)| !retired(tool))
                .count(),
            "a base row names a tool whose verb agent-default does not serve"
        );
        // A withdrawal no base row reaches is a stale excuse.
        let unused: Vec<&(&str, &str, Withdrawal)> = WITHDRAWN_2026_09_28
            .iter()
            .enumerate()
            .filter(|(index, (tool, _, _))| !retired(tool) && !withdrawals_used.contains(index))
            .map(|(_, row)| row)
            .collect();
        assert!(
            unused.is_empty(),
            "these withdrawals match no base row: {unused:?}"
        );
        assert_eq!(
            verb_withdrawals_used.len(),
            WITHDRAWN_FOR_VERBS
                .iter()
                .filter(|(tool, _)| !retired(tool))
                .count(),
            "a withdrawal for the verbs matches no base row"
        );
    }

    /// The control for the budget above: the registered schemas must still carry
    /// the long forms, or the profile is passing because the registry lost them.
    #[test]
    fn the_full_profile_keeps_the_long_property_descriptions() {
        let full = crate::tools::served_tools_list(None, false);
        let locate = full
            .tools
            .iter()
            .find(|tool| tool.name == crate::verbs::LOCATE)
            .expect("full serves locate");
        let budget = locate.input_schema["properties"][crate::request_schema::OUTPUT_BUDGET]
            ["description"]
            .as_str()
            .expect("the reply budget carries a description");
        assert!(
            budget.chars().count() > AGENT_DEFAULT_PROPERTY_DESCRIPTION_BUDGET,
            "the full profile's budget prose was shortened too: {} chars",
            budget.chars().count()
        );
    }

    /// Every advertised response budget on `agent-default` is the one its own
    /// tool is capped at, and the number advertised is the number injected.
    ///
    /// The registered ceiling is 45,000 characters, about 10,500 gemma-4-e4b
    /// tokens, so two default answers exhaust the 24,000-token run this belt
    /// exists to fit. The list cap is derived from one rule: six calls at
    /// default size inside that run with room left to answer. The two chain
    /// tools are capped at the agent's own per-result limit instead, for the
    /// reason on [`AGENT_CHAIN_RESPONSE_MAX_CHARS`].
    ///
    /// Read per tool rather than against one number, because a single ceiling is
    /// exactly what this stopped being. Asserted as EQUALITY rather than "at or
    /// under", which is stronger than the check it replaced: a tool advertising
    /// less than its cap used to pass, and would now be a silent second policy.
    ///
    /// Both halves are asserted together on purpose. A belt that injected the
    /// cap while advertising 45,000 would pass any check that reads only one
    /// side, and the served schema is what an agent reads before it decides
    /// whether to narrow its own request. `minimum` and `maximum` are left as
    /// registered, which is also what keeps
    /// `response_budget_elisions.py` `check_2` satisfied, since it requires
    /// `minimum < default <= maximum`.
    #[test]
    fn no_agent_default_response_budget_is_advertised_above_the_cap() {
        let served = served_agent_default();
        let mut problems: Vec<String> = Vec::new();
        let mut checked = 0usize;
        for tool in &served.tools {
            let properties = tool
                .input_schema
                .get("properties")
                .and_then(|value| value.as_object());
            let Some(properties) = properties else {
                continue;
            };
            let cap = agent_default_response_max_chars(&tool.name);
            // A verb whose `max_output_bytes` is not a reply budget, the
            // toolchain run's bytes per stream, is not graded here.
            let reply_budget = !crate::requests::is_typed(&tool.name)
                || crate::requests::handler_budget_name(&tool.name, &HashMap::new()).is_some();
            for key in [crate::request_schema::OUTPUT_BUDGET]
                .into_iter()
                .filter(|_| reply_budget)
            {
                let Some(property) = properties.get(key) else {
                    continue;
                };
                checked += 1;
                let advertised = property.get("default").and_then(|value| value.as_u64());
                match advertised {
                    None => problems.push(format!("{}.{key} advertises no default", tool.name)),
                    Some(value) if value != cap => problems.push(format!(
                        "{}.{key} advertises {value}, and this tool's cap is {cap}",
                        tool.name
                    )),
                    Some(_) => {}
                }
                // The number advertised has to be the number the belt sends, to
                // every registered tool the verb routes to.
                let handlers = if crate::verbs::is_verb(&tool.name) {
                    crate::verbs::handlers_of(&tool.name)
                } else {
                    vec![tool.name.as_str()]
                };
                for handler in handlers {
                    let mut arguments = HashMap::new();
                    apply_belt_defaults(handler, &mut arguments);
                    let injected = arguments.get("max_chars").and_then(|value| value.as_u64());
                    if injected != Some(cap) {
                        problems.push(format!(
                            "{} advertises a budget and the belt injects {injected:?} into \
                             {handler} against a cap of {cap}",
                            tool.name
                        ));
                    }
                }
            }
        }
        assert!(
            problems.is_empty(),
            "agent-default response budgets disagree with their caps: {problems:#?}"
        );
        // The sweep has to be reaching the schemas, or an empty problem list is
        // a fixture that found nothing rather than a belt that is right.
        assert!(
            checked >= 7,
            "the sweep found only {checked} budget properties on agent-default, so it is not \
             reaching the served schemas"
        );
        // The split is the point, and a table that collapsed back to one number
        // would satisfy every assertion above.
        for chain in [
            "trace_data_flow",
            crate::handlers::path::TOOL_NAME,
            "get_context_pack",
            crate::verbs::TRACE,
            crate::verbs::CONTEXT,
            // A reference answer's sections for a shared name fit one page
            // at this ceiling and not at the list's.
            "find_references",
            crate::verbs::REFS,
        ] {
            assert_eq!(
                agent_default_response_max_chars(chain),
                AGENT_CHAIN_RESPONSE_MAX_CHARS,
                "{chain}"
            );
        }
        for list in [
            "semantic_locate",
            "impact_analysis",
            crate::verbs::LOCATE,
            crate::verbs::IMPACT,
        ] {
            assert_eq!(
                agent_default_response_max_chars(list),
                AGENT_DEFAULT_RESPONSE_MAX_CHARS,
                "{list}"
            );
        }
        assert_ne!(
            AGENT_CHAIN_RESPONSE_MAX_CHARS,
            AGENT_DEFAULT_RESPONSE_MAX_CHARS
        );
    }

    /// A description page stays at or under the cap, and `locate` never
    /// advertises a page size one of its modes does not get.
    ///
    /// This is the client-side cut rather than the server-side one. Every
    /// `semantic_locate` in all three agentic runs on the React and VS Code
    /// stores came back cut at the harness's own 1,500-token per-result budget,
    /// and the model answered by re-issuing the same query rather than paging.
    /// A client cut is invisible to Kin, so the page has to fit before it is
    /// sent.
    ///
    /// `locate` reads a description onto that page and keeps the registered 20
    /// for a name search, which does not page, and for a literal lookup. No one
    /// number is what every call gets, so the served `limit` states none, and
    /// the belt still sends the description page it always sent.
    #[test]
    fn the_served_locate_page_stays_under_the_cap() {
        let served = served_agent_default();
        let locate = served
            .tools
            .iter()
            .find(|tool| tool.name == crate::verbs::LOCATE)
            .expect("agent-default serves locate");
        let limit = &locate.input_schema["properties"]["limit"];
        assert!(limit.is_object(), "locate must serve limit: {locate:?}");
        assert!(
            limit.get("default").is_none(),
            "locate serves a limit default one of its modes does not get: {limit}"
        );

        let mut arguments = HashMap::new();
        apply_belt_defaults("semantic_locate", &mut arguments);
        assert_eq!(
            arguments.get("limit").and_then(|value| value.as_u64()),
            Some(AGENT_DEFAULT_LOCATE_PAGE),
            "the belt must send the description page it always sent"
        );
        for other in ["semantic_search", crate::handlers::lexical::TOOL_NAME] {
            let mut arguments = HashMap::new();
            apply_belt_defaults(other, &mut arguments);
            assert!(!arguments.contains_key("limit"), "{other}");
        }

        // The control: the registry must still carry the larger default, so this
        // is the profile choosing a page rather than the registry having lost
        // one.
        let registered = crate::tools::tool_definitions();
        let full = registered
            .tools
            .iter()
            .find(|tool| tool.name == crate::verbs::LOCATE)
            .expect("locate is registered");
        let registered_limit = full.input_schema["properties"]["limit"]["default"].as_u64();
        assert!(
            registered_limit.is_some_and(|value| value > AGENT_DEFAULT_LOCATE_PAGE),
            "the full profile's locate page was shrunk too: {registered_limit:?}"
        );
    }

    #[test]
    fn locate_cursor_keeps_its_width_unless_the_caller_overrides_it() {
        let mut inherited = HashMap::from([("cursor".into(), serde_json::json!("held"))]);
        apply_belt_defaults("semantic_locate", &mut inherited);
        assert!(!inherited.contains_key("limit"));
        assert!(!inherited.contains_key("page_size"));
        for key in ["limit", "page_size"] {
            let mut explicit = HashMap::from([
                ("cursor".into(), serde_json::json!("held")),
                (key.into(), serde_json::json!(3)),
            ]);
            apply_belt_defaults("semantic_locate", &mut explicit);
            assert_eq!(explicit[key], serde_json::json!(3));
            if key == "page_size" {
                assert!(!explicit.contains_key("limit"));
            }
        }
    }

    /// The belt must never outrank a caller who named a budget itself.
    ///
    /// `ResponseBudget::from_arguments` takes the FIRST of `max_chars` then
    /// `max_response_chars` that is present, so an unconditional insert of
    /// `max_chars` would silently override a caller who had passed
    /// `max_response_chars` and answer under a ceiling they never asked for.
    #[test]
    fn the_belt_never_overrides_a_budget_the_caller_named() {
        for key in ["max_chars", "max_response_chars"] {
            let mut arguments = HashMap::from([(key.to_string(), serde_json::json!(58_000u64))]);
            apply_belt_defaults("trace_data_flow", &mut arguments);
            assert_eq!(
                arguments.get(key).and_then(|value| value.as_u64()),
                Some(58_000),
                "the belt moved a {key} the caller named"
            );
            assert!(
                !(key == "max_response_chars" && arguments.contains_key("max_chars")),
                "the belt added max_chars beside a caller's max_response_chars, which \
                 from_arguments would then prefer"
            );
        }
        // Same rule on the walker's shape.
        for key in ["include_body", "compact"] {
            let mut arguments = HashMap::from([(key.to_string(), serde_json::json!(true))]);
            apply_belt_defaults("trace_data_flow", &mut arguments);
            assert_eq!(
                arguments.get(key).and_then(|value| value.as_bool()),
                Some(true),
                "the belt moved a {key} the caller named"
            );
        }
        // And a caller that names nothing gets the belt's shape.
        let mut bare = HashMap::new();
        apply_belt_defaults("trace_data_flow", &mut bare);
        assert_eq!(
            bare.get("include_body").and_then(|value| value.as_bool()),
            Some(false),
            "the walker's default shape on this belt is the chain, not the bodies"
        );
    }

    /// [`BUDGET_TOOLS`] must name exactly the registered tools the belt's verbs
    /// route to that the REGISTRY gives a budget property, or the belt caps a
    /// subset and nothing says so.
    #[test]
    fn the_budget_tool_list_matches_the_registry() {
        let full = crate::tools::tool_definitions();
        let belt: std::collections::HashSet<&str> = crate::tools::agent_default_tool_names()
            .iter()
            .flat_map(|name| crate::verbs::handlers_of(name))
            .collect();
        let mut registered: Vec<String> = full
            .tools
            .into_iter()
            .filter(|tool| belt.contains(tool.name.as_str()))
            .filter(|tool| {
                tool.input_schema
                    .get("properties")
                    .and_then(|value| value.as_object())
                    .is_some_and(|properties| {
                        properties.contains_key("max_chars")
                            || properties.contains_key("max_response_chars")
                    })
            })
            .map(|tool| tool.name)
            .collect();
        registered.sort();
        let mut listed: Vec<String> = BUDGET_TOOLS.iter().map(|name| name.to_string()).collect();
        listed.sort();
        assert_eq!(
            listed, registered,
            "BUDGET_TOOLS and the registry disagree about which belt tools carry a budget"
        );
    }

    /// The knobs the shipped acceptance checks read out of the served surface.
    ///
    /// `agent-default` trims response-shaping properties, and three acceptance
    /// checks read exactly those properties as the contract an agent discovers
    /// a knob from. Trimming them took `Product Acceptance` red on `main` for
    /// four landings, as `magic:6`, `magic:14` and `response_budget:2`, while
    /// every pull request stayed green, because that job is `skipped` on a pull
    /// request and graded only on `main`'s push run.
    ///
    /// The three checks, so the next person can find them.
    /// `scripts/acceptance/magic_repro.py` `check_6` requires `include_body` or
    /// `compact` on `trace`. Its `check_14` arm 3 requires the literal
    /// `last_settled_selected_graph` in the served `status` description. `scripts/acceptance/response_budget_elisions.py` `check_2`
    /// grades every advertised `max_chars` or `max_response_chars` and reports
    /// UNREADABLE when it finds none, so the served profile has to advertise a
    /// budget wherever the registry does.
    ///
    /// Every arm collects rather than panics, so one trimmed knob reports itself
    /// and the other two arms still run.
    #[test]
    fn agent_default_serves_every_knob_the_shipped_checks_assert() {
        const BUDGET_KEYS: [&str; 3] = [
            crate::request_schema::OUTPUT_BUDGET,
            "max_chars",
            "max_response_chars",
        ];
        let served = served_agent_default();
        let full = crate::tools::served_tools_list(None, false);
        let properties = |list: &ToolsListResult, name: &str| -> Vec<String> {
            list.tools
                .iter()
                .find(|tool| tool.name == name)
                .and_then(|tool| tool.input_schema.get("properties"))
                .and_then(|value| value.as_object())
                .map(|object| object.keys().cloned().collect())
                .unwrap_or_default()
        };
        let has_budget =
            |keys: &[String]| keys.iter().any(|key| BUDGET_KEYS.contains(&key.as_str()));
        let mut problems: Vec<String> = Vec::new();

        // magic_repro.py check_6.
        let trace = properties(&served, crate::verbs::TRACE);
        if !trace
            .iter()
            .any(|key| key == "include_body" || key == "compact")
        {
            problems.push(format!(
                "trace advertises neither include_body nor compact, which \
                 magic_repro.py check_6 requires: {trace:?}"
            ));
        }

        // response_budget_elisions.py check_2, graded against the registry's own set.
        for tool in &served.tools {
            if !has_budget(&properties(&full, &tool.name)) {
                continue;
            }
            let keys = properties(&served, &tool.name);
            if !has_budget(&keys) {
                problems.push(format!(
                    "{} registers a budget parameter and agent-default advertises none, so \
                     response_budget_elisions.py check_2 grades a smaller set than it did: \
                     {keys:?}",
                    tool.name
                ));
            }
        }

        // magic_repro.py check_14, arm 3.
        let status = served
            .tools
            .iter()
            .find(|tool| tool.name == crate::verbs::STATUS)
            .map(|tool| tool.description.as_str())
            .unwrap_or_default();
        if !status.contains("last_settled_selected_graph") {
            problems.push(
                "the status short description does not carry \
                 last_settled_selected_graph, which magic_repro.py check_14 arm 3 requires"
                    .to_string(),
            );
        }

        assert!(
            problems.is_empty(),
            "agent-default trimmed a knob a shipped acceptance check reads: {problems:#?}"
        );
    }

    /// Every remediation or invocation alternative the agent-facing response
    /// names must be present in the schema that same agent receives.
    ///
    /// The first-contact run was told to widen `limit_per_step`, but the belt
    /// had removed that property. It also left `question` and `entities` in
    /// `get_context_pack`'s input alternatives after removing both property
    /// definitions, producing a schema that described only one of its three
    /// valid ways to call the tool.
    #[test]
    fn agent_default_advertises_every_answer_recovery_and_input_alternative() {
        let served = served_agent_default();
        let properties = |name: &str| {
            served
                .tools
                .iter()
                .find(|tool| tool.name == name)
                .unwrap_or_else(|| panic!("agent-default does not serve {name}"))
                .input_schema["properties"]
                .as_object()
                .unwrap_or_else(|| panic!("{name} has no property map"))
        };

        let trace = properties(crate::verbs::TRACE);
        assert!(
            trace.contains_key("limit_per_step"),
            "trace remediation tells this profile to widen limit_per_step, so its served schema \
             must advertise the knob: {:?}",
            trace.keys().collect::<Vec<_>>()
        );

        let context = properties(crate::verbs::CONTEXT);
        for alternative in ["entity_id", "entities", "question"] {
            assert!(
                context.contains_key(alternative),
                "context names `{alternative}` as a valid input alternative but the \
                 served property map omits it: {:?}",
                context.keys().collect::<Vec<_>>()
            );
        }
    }

    /// Every property name a schema requires, directly or inside an `anyOf`,
    /// `oneOf` or `allOf` branch. Combinator branches only: a nested object's
    /// own `required` names ITS properties, not the tool's.
    fn required_property_names(schema: &serde_json::Value) -> Vec<(&str, bool)> {
        let mut names = Vec::new();
        if let Some(required) = schema.get("required").and_then(|value| value.as_array()) {
            names.extend(
                required
                    .iter()
                    .filter_map(|value| value.as_str())
                    .map(|name| (name, false)),
            );
        }
        for combinator in ["anyOf", "oneOf", "allOf"] {
            let Some(branches) = schema.get(combinator).and_then(|value| value.as_array()) else {
                continue;
            };
            for branch in branches {
                names.extend(
                    required_property_names(branch)
                        .into_iter()
                        .map(|(name, _)| (name, true)),
                );
            }
        }
        names
    }

    /// Every backtick-quoted identifier in `text`. Multi-word phrases and
    /// quoted values are not identifiers and are left alone.
    fn backticked_identifiers(text: &str) -> Vec<&str> {
        text.split('`')
            .skip(1)
            .step_by(2)
            .filter(|token| {
                !token.is_empty()
                    && token
                        .chars()
                        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
                    && token.starts_with(|ch: char| ch.is_ascii_alphabetic() || ch == '_')
            })
            .collect()
    }

    /// A short property description may not introduce an identifier the tool's
    /// own registered contract never uses.
    ///
    /// This belt shortens a vetted description; it does not get to write a new
    /// claim. The `limit_per_step` short form in this bundle first shipped
    /// naming `truncated_steps`, a response field that exists nowhere in Kin,
    /// while the real disclosure is `clipped_steps` carrying `fanout_truncated`
    /// and `fanout_dropped`. To the agent reading it, an invented field name is
    /// indistinguishable from a real one, and the remediation it anchors is
    /// unfollowable.
    ///
    /// The haystack is the tool's WHOLE registered definition, description and
    /// schema together, so an enum value and a response field both count as
    /// vetted. Scoped to the per-tool table, which is where a description can
    /// name a fact about one tool; the shared table's entries are written to be
    /// true of every tool that takes the property.
    #[test]
    fn no_short_property_description_names_an_identifier_the_tool_never_publishes() {
        let registered = crate::tools::tool_definitions();
        let mut checked = 0usize;
        let mut unknown: Vec<String> = Vec::new();

        for ((tool, property), short) in tool_property_descriptions() {
            let definition = registered
                .tools
                .iter()
                .find(|candidate| candidate.name == tool)
                .unwrap_or_else(|| {
                    panic!("tool_property_descriptions() names unregistered tool {tool}")
                });
            let contract = serde_json::to_string(&definition.input_schema)
                .expect("a registered schema serializes");
            for token in backticked_identifiers(short) {
                checked += 1;
                if contract.contains(token) || definition.description.contains(token) {
                    continue;
                }
                unknown.push(format!("{tool}.{property} names `{token}`"));
            }
        }
        // The verbs' short property descriptions live on their typed requests,
        // and are graded against every registered tool the verb answers through.
        for verb in crate::requests::TYPED_TOOLS {
            let contract: String = crate::verbs::handlers_of(verb)
                .into_iter()
                .chain(std::iter::once(verb))
                .filter_map(|name| registered.tools.iter().find(|tool| tool.name == name))
                .map(|tool| {
                    format!(
                        "{} {}",
                        tool.description,
                        serde_json::to_string(&tool.input_schema).expect("a schema serializes")
                    )
                })
                .collect();
            for field in crate::requests::declared_fields(verb).expect("a typed verb") {
                let texts = std::iter::once(field.doc).chain(field.agent_doc);
                for token in texts.flat_map(backticked_identifiers) {
                    checked += 1;
                    if !contract.contains(token) {
                        unknown.push(format!("{verb}.{} names `{token}`", field.name));
                    }
                }
            }
        }

        assert!(
            unknown.is_empty(),
            "these short property descriptions name identifiers absent from the tool's own \
             registered description and schema, so an agent is told to read a field that does \
             not exist: {unknown:?}"
        );
        // Five is every backticked identifier the table carried after the
        // 2026-09-28 cut, which removed the enum restatements that held most of
        // them.
        assert!(
            checked >= 5,
            "the sweep read {checked} backticked identifiers out of the per-tool table, so it is \
             not reaching the text it grades"
        );
    }

    /// A served schema must not require a property it does not define.
    ///
    /// [`trim_schema`] filters `properties` and the top-level `required` against
    /// the keep list and touches nothing else, so a constraint nested in a
    /// combinator keeps naming a property the trim removed. `get_context_pack`
    /// shipped exactly that on all three belt profiles: its `anyOf` offers
    /// `entity_id`, `entities` and `question`, and the keep list defined only
    /// the first, so two of the three documented ways to call the tool were
    /// required by the schema and absent from it.
    ///
    /// A sweep rather than those two names, because the next keep-list edit that
    /// drops a constrained property is the same defect, and a test naming the
    /// properties of the last one stays green through it.
    #[test]
    fn no_belt_profile_serves_a_schema_that_requires_a_property_it_does_not_define() {
        let mut names_read = 0usize;
        let mut branch_names_read = 0usize;
        let mut missing: Vec<String> = Vec::new();

        for (profile, names) in [
            ("agent-default", crate::tools::agent_default_tool_names()),
            ("agent-query", crate::tools::agent_query_tool_names()),
            ("agent-search", crate::tools::agent_search_tool_names()),
        ] {
            let allowed: std::collections::HashSet<String> =
                names.iter().map(|name| (*name).to_string()).collect();
            for tool in crate::tools::served_tools_list(Some(&allowed), true).tools {
                let defined: std::collections::HashSet<&str> = tool.input_schema["properties"]
                    .as_object()
                    .map(|properties| properties.keys().map(String::as_str).collect())
                    .unwrap_or_default();
                // The "at least one of" rules the server enforces in place of a
                // top-level `anyOf` are read as branch names, because that is
                // what they were and the same defect lives in them.
                let alternatives = crate::input_contract::alternatives(&tool.name)
                    .iter()
                    .flat_map(|set| set.iter().map(|name| (*name, true)));
                for (required, from_branch) in required_property_names(&tool.input_schema)
                    .into_iter()
                    .chain(alternatives)
                {
                    names_read += 1;
                    if from_branch {
                        branch_names_read += 1;
                    }
                    if !defined.contains(required) {
                        missing.push(format!("{profile}/{}: `{required}`", tool.name));
                    }
                }
            }
        }

        assert!(
            missing.is_empty(),
            "these served schemas require a property their own `properties` map does not define, \
             so the profile advertises a way to call the tool it never describes: {missing:?}; add \
             the property to that tool's entry in schema_keep_lists()"
        );
        // Anti-vacuity. A top-level `required` cannot fail this, because
        // `trim_schema` filters that list too, so the sweep is only grading the
        // class while it reads combinator branches. Reading none means the
        // schemas moved out from under it, not that they are clean.
        assert!(
            branch_names_read >= 3,
            "the sweep read {names_read} required names across three profiles and only \
             {branch_names_read} of them from an anyOf/oneOf/allOf branch or a server-side \
             alternative, which is the only \
             place this defect can live"
        );
    }

    /// The in-place form the dispatchers call must agree with the borrowing one,
    /// or a call under the alias reaches a different place than a test asserts.
    #[test]
    fn the_two_canonicalizers_agree() {
        for name in [
            DECLARATION_FILTER_ALIAS,
            DECLARATION_FILTER_CANONICAL,
            "semantic_locate",
            "trace_data_flow",
            "",
        ] {
            let mut owned = name.to_string();
            canonicalize_tool_name(&mut owned);
            assert_eq!(owned, canonical_tool_name(name), "disagreement on {name:?}");
        }
    }

    /// The served list stays in name order.
    #[test]
    fn the_served_list_stays_sorted() {
        let names: Vec<String> = served_agent_default()
            .tools
            .into_iter()
            .map(|tool| tool.name)
            .collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(
            names, sorted,
            "a client caches the prompt built from this order"
        );
    }

    /// The schemas shrink too. Descriptions were 47,739 characters of the 82,262
    /// the profile costs; the input schemas were the other 30,456.
    #[test]
    fn the_trimmed_schemas_drop_the_response_shaping_properties() {
        let served = served_agent_default();
        let locate = served
            .tools
            .iter()
            .find(|t| t.name == crate::verbs::LOCATE)
            .expect("served");
        let properties = locate.input_schema["properties"]
            .as_object()
            .expect("object");
        assert!(
            properties.contains_key("query"),
            "the question must survive"
        );
        // `max_chars` is the exception the keep-list doc names. It shapes the
        // response like the four below, and it stays advertised because
        // response_budget_elisions.py check_2 grades the advertised budget and
        // reports UNREADABLE when no tool carries one. An agent that cannot see
        // it cannot bound a response either.
        assert!(
            properties.contains_key(crate::request_schema::OUTPUT_BUDGET),
            "the reply budget is graded off the served schema and must stay advertised"
        );
        for shaping in ["compact", "explain", "snippet_alias", "pipeline"] {
            assert!(
                !properties.contains_key(shaping),
                "{shaping} shapes the response and should not be advertised here"
            );
        }
        // The control: the registered schema still has them all, so the profile
        // is hiding the four rather than the registry having lost them.
        let registered = crate::tools::tool_definitions();
        let full = registered
            .tools
            .iter()
            .find(|t| t.name == crate::verbs::LOCATE)
            .expect("registered");
        for shaping in [
            crate::request_schema::OUTPUT_BUDGET,
            "compact",
            "explain",
            "snippet_alias",
            "pipeline",
        ] {
            assert!(
                full.input_schema["properties"]
                    .as_object()
                    .expect("object")
                    .contains_key(shaping),
                "the full profile lost {shaping}"
            );
        }
    }

    /// A trim must never leave a schema requiring a property it no longer
    /// describes, which is a schema no client can satisfy.
    #[test]
    fn no_trimmed_schema_requires_a_property_it_dropped() {
        for tool in served_agent_default().tools {
            let Some(required) = tool.input_schema.get("required").and_then(|r| r.as_array())
            else {
                continue;
            };
            let properties = tool.input_schema["properties"]
                .as_object()
                .unwrap_or_else(|| panic!("{} has no properties", tool.name));
            for name in required {
                let name = name.as_str().expect("required names are strings");
                assert!(
                    properties.contains_key(name),
                    "{} requires '{name}' but no longer describes it",
                    tool.name
                );
            }
        }
    }

    /// Every short form must say what comes back, not only what the tool is
    /// for, and where one verb answers a question another is easy to mistake
    /// for, it points there.
    ///
    /// The two confusable pairs this guarded, the name filter and the ranking
    /// locate, and the one-endpoint walk and the two-endpoint route, are one
    /// verb each now, so the choice is an option the description names rather
    /// than a second tool.
    #[test]
    fn a_verb_points_a_question_it_does_not_answer_at_the_verb_that_does() {
        let descriptions = short_descriptions();
        let locate = descriptions[crate::verbs::LOCATE];
        assert_eq!(locate, "Find matches");
        // The chain question has one answer, and the belt must send it there.
        for hop in [crate::verbs::REFS, crate::verbs::NEIGHBORS] {
            assert!(
                descriptions[hop].contains("`trace`"),
                "{hop} must point a chain question at `trace`"
            );
        }
        let trace = descriptions[crate::verbs::TRACE];
        assert!(
            trace.contains("Direct+indirect callers in 1 call") && trace.contains("to: route"),
            "trace must say one call returns every caller, the indirect ones included, and \
             that to asks for the route: {trace}"
        );
        assert!(
            descriptions[crate::verbs::TRANSACTION].contains("`mutate`"),
            "transaction must name the one-call write"
        );
    }

    /// The transaction tool, which is plumbing for writing and
    /// can answer no question about code.
    ///
    /// Written out rather than derived from the difference between the two
    /// belts, for the reason
    /// `agent_query_serves_the_query_half_of_the_agent_belt` gives: a name that
    /// joins the write half should cost somebody a decision here.
    /// `the_lifecycle_set_is_the_write_half_of_the_belt` holds the two in step.
    ///
    /// `session` was the other until a write opened its connection's own
    /// session; it is retired and listed nowhere.
    const LIFECYCLE_TOOLS: [&str; 1] = [crate::verbs::TRANSACTION];

    /// `mutate` is the third write verb and is deliberately not lifecycle. It
    /// is the write ACTION rather than the plumbing around one, an agent asking
    /// how to change something should find it, and it names entity changes on
    /// purpose. `init` is the fourth and is not lifecycle either: it sets a
    /// folder up, and the answer that says a folder has no repository is what
    /// sends an agent to it. `exec` carries the toolchain run, so an agent
    /// asking to build, test or run the project should find
    /// it, and
    /// `a_code_question_never_ranks_a_lifecycle_tool_over_the_tool_that_answers_it`
    /// holds it to the same vocabulary rule the plumbing keeps.
    #[test]
    fn the_lifecycle_set_is_the_write_half_of_the_belt() {
        let query: BTreeSet<&str> = crate::tools::agent_query_tool_names()
            .iter()
            .copied()
            .collect();
        let write_half: BTreeSet<&str> = crate::tools::agent_default_tool_names()
            .iter()
            .copied()
            .filter(|name| {
                !query.contains(name)
                    && *name != crate::verbs::MUTATE
                    && *name != crate::verbs::INIT
                    && *name != crate::verbs::EXEC
            })
            .collect();
        assert_eq!(
            write_half,
            LIFECYCLE_TOOLS.into_iter().collect::<BTreeSet<&str>>(),
            "the belt's write half moved; the vocabulary guards below are keyed on it"
        );
    }

    /// Words a locate, reference, context or trace question arrives in.
    ///
    /// A lifecycle description that carries one of these competes with the tool
    /// that answers the question, in a client where the model never sees the
    /// schemas and learns the surface by searching it.
    const QUERY_VOCABULARY: [&str; 24] = [
        "behavior",
        "behaviour",
        "caller",
        "callers",
        "code",
        "context",
        "declaration",
        "depends",
        "entities",
        "entity",
        "find",
        "graph",
        "impact",
        "locate",
        "neighborhood",
        "query",
        "read",
        "reads",
        "reference",
        "references",
        "search",
        "semantic",
        "symbol",
        "trace",
    ];

    /// No lifecycle description may carry the vocabulary of a code question.
    ///
    /// Measured on a third-party client rather than reasoned about. Grok never
    /// sends MCP tool schemas to the model: the model learns a tool by calling
    /// Grok's own `search_tool` with a query, which ranks over tool names and
    /// descriptions. On a pure locate question the ranked top eight came back
    /// holding `kin__kin_transaction_commit` and `kin__kin_session_end`, while
    /// `get_context_pack`, `find_references`, `trace_path` and
    /// `trace_data_flow` did not make the cut. Two of eight slots went to tools
    /// that cannot answer any question, and four that can were never offered.
    ///
    /// The tools stay. A third-party agent's write path is begin, stage, commit,
    /// and deleting the plumbing would take the write path with it. What changes
    /// is that each of them now opens by saying it is plumbing for writing, and
    /// carries no word a code question is phrased in. Since the verbs, the
    /// plumbing is `transaction` with actions; the session is the connection's.
    #[test]
    fn no_lifecycle_description_carries_the_vocabulary_of_a_code_question() {
        let descriptions = short_descriptions();
        for tool in LIFECYCLE_TOOLS {
            let description = descriptions
                .get(tool)
                .unwrap_or_else(|| panic!("{tool} has no short description"));
            let words: BTreeSet<String> = tokens(description);
            let carried: Vec<&str> = QUERY_VOCABULARY
                .into_iter()
                .filter(|word| words.contains(*word))
                .collect();
            assert!(
                carried.is_empty(),
                "{tool}'s short description carries query vocabulary {carried:?}, so a ranker \
                 that scores it against a code question will offer it: {description:?}"
            );
            // And the other half of the rule: it has to SAY what it is, or a
            // ranker has nothing to score a write question against.
            assert!(
                words.contains("plumbing") && words.contains("writes"),
                "{tool}'s short description must name itself as plumbing for writes: \
                 {description:?}"
            );
        }
    }

    /// Lowercase alphanumeric words, which is what a lexical ranker sees.
    fn tokens(text: &str) -> BTreeSet<String> {
        text.split(|character: char| !character.is_ascii_alphanumeric())
            .filter(|word| !word.is_empty())
            .map(str::to_ascii_lowercase)
            .collect()
    }

    /// Words that carry no information about WHICH tool answers a question.
    ///
    /// `kin` is here with the grammar. Every tool on this server is a Kin tool,
    /// so the term separates none of them from each other, and half the
    /// registered names happen to carry it as a prefix while the retrieval tools
    /// do not. Leaving it in would score `kin_session_end` above
    /// `semantic_locate` on the word "kin" alone, which is a fact about the
    /// naming convention rather than about either tool.
    const RANKING_STOPWORDS: [&str; 48] = [
        "a", "about", "after", "all", "an", "and", "are", "as", "at", "be", "before", "by", "can",
        "do", "does", "every", "for", "from", "has", "have", "i", "if", "in", "into", "is", "it",
        "its", "kin", "me", "my", "not", "of", "on", "one", "onto", "or", "out", "so", "that",
        "the", "this", "to", "up", "was", "what", "which", "will", "with",
    ];

    /// How many distinct terms of `question` a tool's name and description
    /// carry, which is the signal a lexical ranker orders candidates by.
    ///
    /// A four-character shared prefix counts as a match, so "references" scores
    /// against "reference" and "entities" against "entity". A real ranker stems
    /// or embeds; this is the crudest form of the same idea, and it is enough to
    /// hold the property that matters.
    fn overlap(question: &str, name: &str, description: &str) -> usize {
        let stop: BTreeSet<&str> = RANKING_STOPWORDS.into_iter().collect();
        let surface = tokens(&format!("{name} {description}"));
        tokens(question)
            .iter()
            .filter(|term| !stop.contains(term.as_str()))
            .filter(|term| {
                surface.iter().any(|token| {
                    token == *term
                        || (term.len() >= 4
                            && token.len() >= 4
                            && (token.starts_with(term.as_str())
                                || term.starts_with(token.as_str())))
                })
            })
            .count()
    }

    /// A code question must never rank a lifecycle tool over the tool that
    /// answers it, and a write question must still reach the lifecycle tools.
    ///
    /// This is the regression guard for the ranked-discovery defect. It models a
    /// lexical ranker over the served name and description, which is what a
    /// client that hides schemas gives a model to choose from. The control at
    /// the end stops the guard being satisfied by emptying the lifecycle
    /// descriptions out.
    #[test]
    fn a_code_question_never_ranks_a_lifecycle_tool_over_the_tool_that_answers_it() {
        let descriptions = short_descriptions();
        let score = |question: &str, tool: &str| -> usize {
            overlap(
                question,
                tool,
                descriptions
                    .get(tool)
                    .unwrap_or_else(|| panic!("{tool} has no short description")),
            )
        };

        // The first probe is the query the Grok run actually sent.
        let code_questions: [(&str, &[&str]); 6] = [
            (
                "kin semantic locate function behavior exit code",
                &[crate::verbs::LOCATE],
            ),
            (
                "find every caller and reference of this function",
                &[crate::verbs::REFS],
            ),
            (
                "give me the context around this entity before I change it",
                &[crate::verbs::CONTEXT],
            ),
            (
                "trace the data flow from this function through the call chain",
                &[crate::verbs::TRACE],
            ),
            (
                "what breaks if I change this entity and what does it affect",
                &[crate::verbs::IMPACT],
            ),
            (
                "how does one function reach another through the code",
                &[crate::verbs::TRACE],
            ),
        ];

        // Only served lifecycle tools participate in ranking. Operator
        // execution and initialization are absent from the agent surface.
        let noise_tools = LIFECYCLE_TOOLS;
        for retired in [crate::verbs::EXEC, crate::verbs::INIT] {
            assert!(!descriptions.contains_key(retired));
            assert!(crate::tools::retired_file_operation(retired).is_some());
        }
        for (question, answering) in code_questions {
            let noise = noise_tools
                .iter()
                .copied()
                .map(|tool| (tool, score(question, tool)))
                .max_by_key(|(_, points)| *points)
                .expect("the lifecycle set is not empty");
            assert_eq!(
                noise.1, 0,
                "{:?} scores {} on the code question {question:?}, so a ranker can offer it \
                 in place of a tool that answers: {:?}",
                noise.0, noise.1, descriptions[noise.0]
            );
            for tool in answering {
                let points = score(question, tool);
                assert!(
                    points > noise.1,
                    "{tool} scores {points} on {question:?} against {} for {:?}, so the tool \
                     that answers the question is not the one a ranker offers",
                    noise.1,
                    noise.0
                );
            }
        }

        // The control. A write question has to reach the plumbing, or the guard
        // above would pass on blank strings.
        let write_questions: [(&str, &str); 2] = [
            ("commit the staged transaction", crate::verbs::TRANSACTION),
            (
                "stage an update onto the open transaction",
                crate::verbs::TRANSACTION,
            ),
        ];
        let query_tools: Vec<&str> = crate::tools::agent_query_tool_names().to_vec();
        for (question, answering) in write_questions {
            let points = score(question, answering);
            let best_query = query_tools
                .iter()
                .map(|tool| (*tool, score(question, tool)))
                .max_by_key(|(_, points)| *points)
                .expect("the query profile is not empty");
            assert!(
                points > best_query.1,
                "{answering} scores {points} on the write question {question:?} against {} for \
                 {:?}, so the write path is not what a ranker offers",
                best_query.1,
                best_query.0
            );
        }
    }

    /// The one string a client is guaranteed to hand the model names tools the
    /// model can actually reach, and stays inside its budget.
    ///
    /// The server `instructions` are separate from the profile, and a client may
    /// deliver one without the other. Grok delivers exactly this string and
    /// never the schemas: the model sees it as a synthetic reminder on turn one
    /// and has to search by name for anything else. So a name in here that the
    /// served profile does not carry sends a model looking for a tool that is
    /// not there, which is the same wasted round trip the ranked-discovery
    /// defect above cost, arriving through a different door.
    #[test]
    fn the_server_instructions_name_only_tools_the_profile_serves() {
        // The byte budget is a compile-time assertion beside the string
        // itself, so it is not restated here. This test is about what the
        // bytes SAY.
        let instructions = crate::server::SERVER_INSTRUCTIONS;

        assert!(instructions.contains("What calls X? refs on X, relation_kinds=[\"calls\"]."));
        let tools = crate::tools::tool_definitions();
        let refs = tools.tools.iter().find(|tool| tool.name == "refs").unwrap();
        let filter = &refs.input_schema["properties"]["relation_kinds"];
        assert_eq!(filter["type"], "array");
        assert_eq!(filter["items"]["type"], "string");
        assert!(filter["description"].as_str().unwrap().contains("calls"));

        // The tools it must name: the ones its routing sends a model's
        // questions to. A model that never reads a schema learns the surface
        // from these names.
        let named = crate::server::NAMED_PROCEDURE_TOOLS;
        let default: BTreeSet<&str> = crate::tools::agent_default_tool_names()
            .iter()
            .copied()
            .collect();
        let query: BTreeSet<&str> = crate::tools::agent_query_tool_names()
            .iter()
            .copied()
            .collect();
        for tool in named {
            assert!(
                instructions.contains(tool),
                "the instructions no longer name {tool}, so a client that hides schemas gives \
                 the model no way to find it"
            );
            assert!(
                default.contains(tool) && query.contains(tool),
                "the instructions name {tool}, which agent-default or agent-query does not \
                 serve"
            );
        }

        // No lifecycle tool is named. The write path is reachable through the
        // served list and through tool search; spending this budget on plumbing
        // is what the ranked-discovery defect cost in the first place.
        for tool in LIFECYCLE_TOOLS {
            assert!(
                !instructions.contains(tool),
                "the instructions name {tool}, which answers no question and costs the budget \
                 a query tool's line needs"
            );
        }
        // And no name a verb replaced: a model that searches for one finds a
        // tool no listing serves.
        for (alias, _) in crate::verbs::ALIASES {
            assert!(
                !instructions.contains(alias),
                "the instructions name {alias}, which no verb-speaking profile lists"
            );
        }

        // The one instruction that only matters in a client that hides the
        // schemas, and the one every measured run needed and did not get.
        assert!(
            instructions.contains("search for \"kin\" first, to find Kin's tools"),
            "the instructions must tell a model whose client lists tools by search to search \
             for this server to find its tools: {instructions:?}"
        );
        // And the verdict sentence a reader has to act on survives the cut, in
        // the plain words an agent profile answers in.
        assert!(
            instructions.contains("_kin.verdict")
                && instructions.contains(crate::plain_reply::COMPLETE_EXCEPT)
                && instructions.contains("partial"),
            "the verdict contract keeps its one sentence: {instructions:?}"
        );
    }

    /// Every instruction string but the citable one routes by question first,
    /// then defines each verdict as the action it permits, with the tool names
    /// adapted to the surface a profile serves and no em dash in any of them.
    ///
    /// The two lines measured to license unbounded deliberation are gone: a
    /// model quoted "Never promote a gap to proof" as its reason to keep
    /// re-checking a reply that had already answered, and every reply's own
    /// verdict carries its limits.
    #[test]
    fn the_instructions_route_by_question_then_define_each_verdict_as_an_action() {
        let routes = |instructions: &str| -> Vec<String> {
            instructions
                .lines()
                .filter(|line| line.starts_with("- ") && line.contains('?'))
                .map(str::to_string)
                .collect()
        };
        let named = crate::server::SERVER_INSTRUCTIONS;
        let search = crate::server::SEARCH_SERVER_INSTRUCTIONS;
        let routed = crate::server::ROUTED_SERVER_INSTRUCTIONS;
        let routed_query = crate::server::ROUTED_QUERY_SERVER_INSTRUCTIONS;
        assert_eq!(
            routes(named),
            vec![
                "- What calls X? refs on X, relation_kinds=[\"calls\"].",
                "- Overrides? refs, view=\"overrides\", entity_id=X UUID.",
                "- What must change if X changes? impact on X: callers and overrides.",
                "- What can end up calling X? trace on X, direction callers, depth 8. If the question limits where callers live, pass scope.",
                "- Is X used anywhere? refs on X, no relation_kinds.",
                "- Where is X? locate. Pass returned UUIDs as entity_id, semantic names as query.",
                "- Why is a caller's receiver typed? refs on X, proof_for_caller=caller UUID.",
            ]
        );
        // The reach line says when to pass scope wherever trace is routed to
        // by name and the string has room for it.
        for instructions in [named, search] {
            assert!(
                instructions.contains("If the question limits where callers live, pass scope."),
                "{instructions}"
            );
        }
        let routed_routes = vec![
            "- What calls X? kin refs on X, relation_kinds=[\"calls\"].",
            "- Overrides? kin refs, view=\"overrides\", entity_id=X UUID.",
            "- What must change if X changes? kin impact on X: callers and overrides.",
            "- What can end up calling X? kin trace on X, direction callers, depth 8.",
            "- Is X used anywhere? kin refs on X, no relation_kinds.",
            "- Where is X? kin locate. Pass returned UUIDs as entity_id, semantic names as query.",
        ];
        assert_eq!(routes(routed), routed_routes);
        assert_eq!(routes(routed_query), routed_routes);
        assert_eq!(routes(search).len(), 2, "{search}");
        for instructions in [named, search, routed, routed_query] {
            assert!(!instructions.contains('\u{2014}'), "{instructions}");
            assert!(
                instructions.contains("search for \"kin\" first, to find Kin's tools"),
                "{instructions}"
            );
            // The routing leads; the verdict actions follow it.
            let routing = instructions.find("\n- ").expect(instructions);
            let verdicts = instructions.find("_kin.verdict").expect(instructions);
            assert!(routing < verdicts, "{instructions}");
            for principle in [
                "Kin's indexed code graph includes tests.",
                "Pass returned UUIDs as entity_id, semantic names as query",
                "Explore entities and relationships",
                "Read _kin.verdict:",
                "- complete: checked against the whole graph; answer, don't re-read code. \
                 A whole body or outline is not all relations.",
                "- complete except the sites listed below, or indirect uses: rows are checked; \
                 check only listed places that could apply, then answer.",
                "- partial: answer from the proven rows and name the gap, unless the reply \
                 lists a concrete next step.",
                "Kin's current graph outranks memory of other versions of this code",
                "Answer once in the requested format",
            ] {
                assert!(
                    instructions.contains(principle),
                    "{principle}: {instructions}"
                );
            }
            for retired in [
                "Never promote a gap to proof",
                "Preserve partial, stale, unproven, unresolved and runtime-name limits",
                "answer now from the reply; do not re-read its rows",
            ] {
                assert!(!instructions.contains(retired), "{retired}: {instructions}");
            }
            assert!(
                instructions.len() <= crate::server::SERVER_INSTRUCTIONS_BUDGET,
                "{} bytes: {instructions}",
                instructions.len()
            );
        }
        // The historical profile keeps its original procedure, but retired file
        // catalog guidance is removed from newly built servers.
        assert!(routes(crate::server::LEGACY_SERVER_INSTRUCTIONS).is_empty());
        assert_eq!(crate::server::LEGACY_SERVER_INSTRUCTIONS.len(), 1106);

        // Every command a routed wording names is one its routed tool takes,
        // and the sentence about the rest is true of that surface.
        for (instructions, surface) in [
            (routed, crate::routed::RoutedSurface::WITH_WRITES),
            (routed_query, crate::routed::RoutedSurface::READ_ONLY),
        ] {
            let commands = crate::routed::command_names(surface);
            for command in [
                "locate", "refs", "impact", "trace", "context", "source", "describe", "call",
            ] {
                assert!(
                    instructions.contains(&format!("kin {command}")),
                    "{command}: {instructions}"
                );
                assert!(
                    commands.contains(&command),
                    "the routed tool takes no {command}"
                );
            }
        }
        assert!(routed.contains("every other Kin tool"));
        assert!(routed_query.contains("every other read-only Kin tool"));

        // The tool-search wording names only what that profile serves, and the
        // one tool it reaches through kin_tool_call is read-only.
        let served: BTreeSet<&str> = crate::tools::agent_search_tool_names()
            .iter()
            .copied()
            .collect();
        for tool in [
            crate::verbs::LOCATE,
            crate::verbs::CONTEXT,
            crate::verbs::TRACE,
            "kin_tool_search",
            "kin_tool_call",
        ] {
            assert!(search.contains(tool), "{tool}: {search}");
            assert!(served.contains(tool), "agent-search does not serve {tool}");
        }
        for withheld in ["refs", "neighbors", "impact"] {
            assert!(!search.contains(withheld), "{withheld}: {search}");
        }
        for (alias, _) in crate::verbs::ALIASES {
            assert!(!search.contains(alias), "{alias}: {search}");
        }
        let registry = crate::tools::tool_definitions();
        assert!(registry
            .tools
            .iter()
            .any(|tool| tool.name == crate::verbs::SOURCE && tool.annotations.read_only_hint));
    }

    /// Each profile is served the wording written for what it serves.
    #[test]
    fn each_profile_is_served_its_own_instructions() {
        use crate::server::{instructions_for, McpServerConfig};
        let named = |names: Option<&[&str]>| McpServerConfig {
            allowed_tools: names.map(crate::tools::name_set),
            agent_belt: true,
            ..McpServerConfig::default()
        };
        let routed = |surface: crate::routed::RoutedSurface| McpServerConfig {
            allowed_tools: Some(crate::tools::name_set(
                crate::tools::agent_routed_tool_names(),
            )),
            agent_belt: true,
            routed: Some(surface),
            ..McpServerConfig::default()
        };
        let citable = |names: &[&str]| McpServerConfig {
            allowed_tools: Some(crate::tools::name_set(names)),
            citable: true,
            ..McpServerConfig::default()
        };
        for (config, expected) in [
            (named(None), crate::server::SERVER_INSTRUCTIONS),
            (
                named(Some(crate::tools::agent_default_tool_names())),
                crate::server::SERVER_INSTRUCTIONS,
            ),
            (
                named(Some(crate::tools::agent_query_tool_names())),
                crate::server::ASK_SERVER_INSTRUCTIONS,
            ),
            (
                named(Some(crate::tools::agent_search_tool_names())),
                crate::server::SEARCH_SERVER_INSTRUCTIONS,
            ),
            (
                named(Some(crate::tools::agent_ask_tool_names())),
                crate::server::ASK_ONLY_SERVER_INSTRUCTIONS,
            ),
            (
                routed(crate::routed::RoutedSurface::WITH_WRITES),
                crate::server::ROUTED_SERVER_INSTRUCTIONS,
            ),
            (
                routed(crate::routed::RoutedSurface::READ_ONLY),
                crate::server::ROUTED_QUERY_SERVER_INSTRUCTIONS,
            ),
            (
                citable(crate::tools::benchmark_tool_names()),
                crate::server::LEGACY_SERVER_INSTRUCTIONS,
            ),
            (
                citable(crate::tools::context_bench_tool_names()),
                crate::server::LEGACY_SERVER_INSTRUCTIONS,
            ),
        ] {
            assert_eq!(
                instructions_for(&config),
                expected,
                "{:?}",
                config.allowed_tools
            );
        }
    }

    /// The belt asks for compact on behalf of its agents, and only when the
    /// caller said nothing. This is the half of the compact surface that
    /// actually reaches an agent, since the wire default is the shared schema.
    #[test]
    fn the_belt_asks_for_the_compact_locate_shape() {
        let mut args: HashMap<String, serde_json::Value> = HashMap::new();
        args.insert("query".into(), serde_json::Value::String("q".into()));
        apply_belt_defaults("semantic_locate", &mut args);
        assert_eq!(
            args.get("surface").and_then(|v| v.as_str()),
            Some("compact"),
            "the belt must ask for the small payload"
        );
        // The query is untouched, so this is an addition rather than a rewrite.
        assert_eq!(args.get("query").and_then(|v| v.as_str()), Some("q"));
    }

    /// A caller that named a surface keeps it. That is what makes the belt's
    /// choice a default rather than an override, and it is the assertion that
    /// fails if someone reaches for `insert` instead of `entry().or_insert`.
    #[test]
    fn the_belt_never_overrides_a_surface_the_caller_named() {
        for named in ["full", "compact"] {
            let mut args: HashMap<String, serde_json::Value> = HashMap::new();
            args.insert("surface".into(), serde_json::Value::String(named.into()));
            apply_belt_defaults("semantic_locate", &mut args);
            assert_eq!(
                args.get("surface").and_then(|v| v.as_str()),
                Some(named),
                "the caller's own surface must survive the belt"
            );
        }
    }

    /// Every argument the belt inserts is one the tool's registered schema
    /// advertises, with one named exception, and a tool the belt has no default
    /// for is left exactly as the caller sent it.
    ///
    /// This replaces a form that asserted the belt touched `semantic_locate`
    /// alone. That held while `surface` was the only injection and stopped being
    /// the point once the response ceiling moved onto every budget tool. The
    /// durable invariant is the one that test's own comment named: a default
    /// that leaked onto a tool would be sending an argument its handler never
    /// advertised. That is what this asserts, against the registry rather than
    /// against a list of names that has to be remembered.
    ///
    /// The exception is `semantic_locate`'s `surface`, which the registry
    /// advertises on no profile. The compact shape is opt-in on the wire by
    /// design, the handler reads the argument by name, and no tool in this
    /// profile sets `additionalProperties: false`. It is named here so the
    /// exception stays a decision on the record rather than a hole in the test.
    #[test]
    fn the_belt_only_injects_arguments_the_tool_advertises() {
        const UNADVERTISED_BY_DESIGN: [(&str, &str); 1] = [("semantic_locate", "surface")];
        let full = crate::tools::tool_definitions();
        let mut untouched = 0usize;
        for tool in &full.tools {
            let mut arguments: HashMap<String, serde_json::Value> = HashMap::new();
            apply_belt_defaults(&tool.name, &mut arguments);
            if arguments.is_empty() {
                untouched += 1;
                continue;
            }
            let properties = tool
                .input_schema
                .get("properties")
                .and_then(|value| value.as_object());
            for key in arguments.keys() {
                if UNADVERTISED_BY_DESIGN.contains(&(tool.name.as_str(), key.as_str())) {
                    continue;
                }
                assert!(
                    properties.is_some_and(|properties| properties.contains_key(key)),
                    "the belt injected {key} into {}, whose registered schema does not \
                     advertise it",
                    tool.name
                );
            }
        }
        // The control. Without it the loop above is satisfied by a belt that
        // injects into everything, since every assertion would then be about a
        // tool that has defaults rather than about one that must not.
        assert!(
            untouched > 0,
            "every registered tool received a belt default, so this test proved nothing"
        );
    }

    /// A typed agent schema may never require a field no handler behind the
    /// verb requires, or a call the server would accept is refused before it
    /// is sent. Every served field is a declared field by construction
    /// ([`crate::requests`]); this holds the other half.
    #[test]
    fn every_typed_agent_schema_requires_only_what_a_handler_requires() {
        let registered = crate::tools::tool_definitions();
        for verb in crate::requests::TYPED_TOOLS {
            let schema = crate::requests::served_schema(
                verb,
                crate::request_schema::Profile::Agent,
                &|_| None,
            )
            .expect("a typed verb");
            let required: Vec<&str> = schema["required"]
                .as_array()
                .map(|names| names.iter().filter_map(|name| name.as_str()).collect())
                .unwrap_or_default();
            for property in required {
                assert!(
                    schema["properties"].get(property).is_some(),
                    "{verb} requires '{property}' but does not describe it"
                );
                let handler_requires = crate::verbs::handlers_of(verb).iter().any(|tool| {
                    registered
                        .tools
                        .iter()
                        .find(|definition| definition.name == *tool)
                        .and_then(|definition| definition.input_schema["required"].as_array())
                        .is_some_and(|names| names.iter().any(|name| name == property))
                });
                assert!(
                    handler_requires,
                    "{verb} requires '{property}', which no tool behind it requires"
                );
            }
        }
    }

    #[test]
    fn every_keep_list_names_real_properties() {
        let registered = crate::tools::tool_definitions();
        for (name, keep) in schema_keep_lists() {
            let tool = registered
                .tools
                .iter()
                .find(|t| t.name == name)
                .unwrap_or_else(|| {
                    panic!(
                        "schema_keep_lists() in crates/kin-mcp/src/agent_belt.rs names '{name}', \
                         which tool_definitions() does not register; fix the spelling or drop \
                         the entry"
                    )
                });
            let properties = tool.input_schema["properties"]
                .as_object()
                .unwrap_or_else(|| panic!("{name} has no properties object"));
            for property in keep {
                assert!(
                    properties.contains_key(*property),
                    "{name}'s keep-list names '{property}', which it does not have"
                );
            }
        }
    }

    /// A whole-body update on the belt carries its `EntitySourceBase`. The served
    /// `kin_mutate` requires a payload on every operation and admits `EntitySourceBase`
    /// beside the anchored and lifecycle payloads, and the operation that shape describes
    /// is one the handler admits. The handler refuses `EntitySourceBase` with any verb but
    /// update or modify, or without a full body, and refuses the update without its base.
    #[test]
    fn the_belt_admits_a_guarded_whole_body_update() {
        let mutate = crate::requests::served_schema(
            crate::verbs::MUTATE,
            crate::request_schema::Profile::Agent,
            &|_| None,
        )
        .expect("mutate is typed");
        let item = &mutate["properties"]["operations"]["items"];
        let payload = &item["properties"]["payload"];
        let declared = payload["properties"]
            .as_object()
            .expect("the payload declares its properties");
        assert_eq!(declared["EntitySourceBase"]["type"], "object");
        // Every payload the handler admits on the agent path is named, and no
        // other key is, so a client that validates refuses a misspelled one.
        assert_eq!(
            declared.keys().map(String::as_str).collect::<BTreeSet<_>>(),
            BTreeSet::from([
                "EntityContent",
                "EntityCreate",
                "EntityRemove",
                "EntitySourceBase",
                "EntitySourcePatch",
                "UnitImports"
            ])
        );
        assert_eq!(payload["additionalProperties"], false);
        assert!(item["properties"]["verb"]["enum"]
            .as_array()
            .expect("the verb is an enum")
            .iter()
            .any(|verb| verb == "update"));
        let said = payload["description"].as_str().unwrap_or_default();
        assert!(
            !said.contains("no outer body") && said.contains("the source tool"),
            "the payload description must say where source_base comes from and must not \
             forbid the body a guarded update carries: {said}"
        );
        let base_said = declared["EntitySourceBase"]["description"]
            .as_str()
            .unwrap_or_default();
        assert!(
            base_said.contains("update") && base_said.contains("body"),
            "EntitySourceBase must say it rides an update with the body: {base_said}"
        );

        let base = crate::source_base::EntitySourceBase {
            schema: crate::source_base::SourceBaseSchema::V1,
            context: crate::source_base::SourceBaseContext {
                repository_id: "belt-test".into(),
                workspace_id: uuid::Uuid::new_v4().to_string(),
                workspace_generation: 1,
                workspace_head_hash: "a".repeat(64),
                workspace_tree_hash: "b".repeat(64),
            },
            entity_id: kin_model::EntityId::new(),
            artifact_id: kin_model::ArtifactId::new(),
            source_blob_hash: "c".repeat(64),
            start_byte: 0,
            end_byte: 12,
            body_hash: "d".repeat(64),
        };
        let target = base.entity_id.to_string();
        let operation = |verb: &str, body: Option<&str>| {
            let mut operation = serde_json::json!({
                "verb": verb,
                "target": target,
                "payload": { "EntitySourceBase": base },
                "description": "rewrite value",
            });
            if let Some(body) = body {
                operation["body"] = serde_json::json!(body);
            }
            operation
        };
        let admitted = |operation: serde_json::Value| -> Result<(), String> {
            let operations =
                crate::session::parse_staged_operations(&serde_json::json!([operation]))?;
            crate::session::validate_semantic_operations(&operations)
        };

        // The served shape describes the update: the one payload key is declared, and
        // every required operation field is present.
        let update = operation("update", Some("pub fn value() -> u8 {\n    2\n}"));
        let sent = update["payload"]
            .as_object()
            .expect("the payload is an object");
        assert_eq!(sent.len(), 1, "the served description says one key");
        assert!(sent.keys().all(|key| declared.contains_key(key)));
        for field in item["required"].as_array().unwrap() {
            assert!(update.get(field.as_str().unwrap()).is_some(), "{field}");
        }
        assert!(
            item["required"]
                .as_array()
                .unwrap()
                .iter()
                .any(|field| field == "payload"),
            "every belt operation names the version it changes"
        );
        admitted(update).expect("update, body and EntitySourceBase are admitted");

        // The handler's own refusals stand.
        for verb in ["patch", "create", "remove"] {
            assert!(
                admitted(operation(verb, Some("pub fn value() -> u8 {\n    2\n}"))).is_err(),
                "EntitySourceBase with verb {verb} must stay refused"
            );
        }
        assert!(
            admitted(operation("update", None)).is_err(),
            "an update with no body"
        );
        assert!(
            admitted(operation("update", Some("  "))).is_err(),
            "an update with a blank body"
        );

        // The same update without its base is refused, and says what fixes it.
        let unguarded = serde_json::json!({
            "verb": "update",
            "target": target,
            "body": "pub fn value() -> u8 {\n    2\n}",
            "description": "rewrite value",
        });
        let refusal = admitted(unguarded).expect_err("an unguarded update is refused");
        assert!(
            refusal.starts_with("source_base_required:") && refusal.contains("EntitySourceBase"),
            "{refusal}"
        );
    }

    /// Stage and commit take exactly `mutate`'s operations, so the belt serves
    /// that shape once, on `mutate`, and has `transaction` point at it. This
    /// holds the three halves of that claim: the registry builds all three item
    /// schemas from one function, every belt profile serving `transaction` also
    /// serves `mutate`, and the served form says where the shape is.
    #[test]
    fn stage_and_commit_take_the_operation_shape_mutate_serves() {
        let registry = crate::tools::tool_definitions();
        let items = |name: &str| {
            registry
                .tools
                .iter()
                .find(|tool| tool.name == name)
                .unwrap_or_else(|| panic!("{name} is registered"))
                .input_schema["properties"]["operations"]["items"]
                .clone()
        };
        let mutate_items = items(crate::verbs::MUTATE);
        assert!(mutate_items["oneOf"].is_array(), "{mutate_items}");
        for name in [
            "kin_mutate",
            "kin_transaction_stage",
            "kin_transaction_commit",
            crate::verbs::TRANSACTION,
        ] {
            assert_eq!(
                items(name),
                mutate_items,
                "{name} no longer takes mutate's operations, so pointing at them is wrong"
            );
        }

        for names in [
            crate::tools::agent_default_tool_names(),
            crate::tools::agent_query_tool_names(),
            crate::tools::agent_search_tool_names(),
        ] {
            let served =
                crate::tools::served_tools_list(Some(&crate::tools::name_set(names)), true);
            let by_name: BTreeMap<&str, &crate::types::ToolDefinition> = served
                .tools
                .iter()
                .map(|tool| (tool.name.as_str(), tool))
                .collect();
            // On an agent profile the operations are served once, on `mutate`,
            // which stages them into an open transaction when it is given a
            // `transaction_id`; `transaction` begins, commits and aborts.
            if let Some(tool) = by_name.get(crate::verbs::TRANSACTION) {
                let mutate = by_name.get(crate::verbs::MUTATE).unwrap_or_else(|| {
                    panic!("transaction stages through mutate on a profile that does not serve it")
                });
                assert!(
                    tool.input_schema["properties"].get("operations").is_none(),
                    "transaction serves a second copy of the operations: {}",
                    tool.input_schema
                );
                assert!(
                    mutate.input_schema["properties"]
                        .get("transaction_id")
                        .is_some(),
                    "mutate must take the transaction_id it stages into"
                );
            }
            if let Some(mutate) = by_name.get(crate::verbs::MUTATE) {
                let item = &mutate.input_schema["properties"]["operations"]["items"];
                let patch = &item["properties"]["payload"]["properties"]["EntitySourcePatch"];
                assert_eq!(
                    patch["required"],
                    serde_json::json!(["source_base", "edits"]),
                    "the anchored patch, the common write, is served as structure: {patch}"
                );
                assert_eq!(
                    item["properties"]["verb"]["enum"],
                    serde_json::json!(["patch", "update", "create", "remove", "replace"])
                );
            }
        }
    }

    /// The forms the belt serves as a clause rather than a schema are spelled out
    /// in the refusal a caller gets when it sends one wrong, so the caller that
    /// needs them pays for them once, and the agent that never creates or
    /// removes a declaration never pays for them at all.
    #[test]
    fn a_malformed_operation_is_answered_with_every_form() {
        for form in [
            "EntitySourcePatch",
            "EntitySourceBase",
            "EntityCreate",
            "UnitImports",
            "EntityRemove",
            "repository_base",
            "source_base",
            "placement",
            "\"role\"",
            "\"package\"",
            "\"kind\"",
        ] {
            assert!(
                crate::session::ACCEPTED_OPERATION_SHAPES.contains(form),
                "the refusal text no longer spells out {form}, which the belt's kin_mutate \
                 serves only as a clause"
            );
        }
        for malformed in [
            serde_json::json!([{
                "verb": "create",
                "target": "NewThing",
                "payload": {"EntityCreate": {"body": "func NewThing() {}"}},
                "description": "add it"
            }]),
            serde_json::json!([{
                "verb": "patch",
                "target": "x",
                "payload": {"EntitySourcePatch": {"edits": []}},
                "description": "d",
                "unexpected": true
            }]),
            serde_json::json!(["not an object"]),
        ] {
            let arguments = HashMap::from([("operations".to_string(), malformed.clone())]);
            let refusal = crate::handlers::sessions::checked_mutate_operations(&arguments)
                .expect_err("a malformed operation is refused");
            let text = serde_json::to_string(&refusal).expect("a tool result serializes");
            assert!(
                text.contains("UnitImports") && text.contains("placement"),
                "{malformed}: the refusal must carry every form: {text}"
            );
        }
    }
}
