// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Argument rules a served tool schema cannot carry, checked by the server.
//!
//! A tool's `inputSchema` used to say "at least one of these fields" with a
//! top-level `anyOf`, and `kin_mutate` said "a keyed call names its session"
//! with a top-level `allOf` holding an `if`/`then`. Model provider tool APIs
//! refuse a schema whose top level is a combinator, and a client that loads
//! tools for one of them drops such a tool without a word: Claude Code listed
//! 19 of the 22 tools `agent-default` served, missing `semantic_locate`,
//! `get_context_pack` and `lexical_lookup`, while Kin's own instructions sent
//! the model to `semantic_locate` first.
//!
//! So every served schema is a plain `type: object` with its properties, one
//! schema for every client, and the rules the combinators carried live here:
//!
//! * [`alternatives`] is each tool's "at least one of" rule, the `required`
//!   sets of the `anyOf` it replaced. [`refusal`] enforces it on every named
//!   call before anything runs, and the routed tool reads the same table, so a
//!   routed refusal and a named one say the same thing.
//! * The keyed `kin_mutate` rule, a `request_id` needing a `session_id`, only
//!   `scope: repository` and only the durable fields, is enforced where it
//!   always was: the MCP mutate adapter refuses a keyed call with no session,
//!   and the daemon's durable route refuses any other field or scope.
//!
//! A field counts as supplied when it is present and not null, which is how
//! the routed tool always read these rules. None of the properties involved
//! accepts null, so a null the old schema refused by type is still refused.
//! Which field wins when several are supplied is the handler's business and is
//! unchanged: this checks that one is there, never which one is used.

use std::collections::HashMap;

use serde_json::{json, Value};

use crate::types::ToolCallResult;

/// One tool's "at least one of" rule: the call must supply every field of at
/// least one set.
struct Alternatives {
    tool: &'static str,
    sets: &'static [&'static [&'static str]],
    /// One call that works, as JSON text of the tool's arguments.
    example: &'static str,
}

const ALTERNATIVES: &[Alternatives] = &[
    Alternatives {
        tool: "semantic_locate",
        sets: &[&["query"], &["cursor"]],
        example: r#"{"query":"where failed requests are retried"}"#,
    },
    Alternatives {
        tool: "get_context_pack",
        sets: &[&["entity_id"], &["entities"], &["question"]],
        example: r#"{"question":"how are failed requests retried"}"#,
    },
    Alternatives {
        tool: crate::handlers::lexical::TOOL_NAME,
        sets: &[&["literal"], &["cursor"]],
        example: r#"{"literal":"retry_after"}"#,
    },
    Alternatives {
        tool: "kin_review_create",
        sets: &[
            &["base", "head"],
            &["scope_type", "entity_ids"],
            &["scopes"],
        ],
        example: r#"{"title":"Review the retry change","base":"<base change id>","head":"<head change id>"}"#,
    },
    Alternatives {
        tool: "kin_review_assign",
        sets: &[&["reviewer"], &["reviewers"]],
        example: r#"{"review_id":"<review id>","reviewer":"reviewer@example.com"}"#,
    },
];

/// The same rules on the verbs that serve those tools, for the routed tool,
/// which validates a command's args against the verb before the verb is
/// resolved. A named verb call is resolved first and checked as the tool it
/// became, and [`crate::verbs::resolve`] refuses a locate with neither field
/// in the verb's own words.
const VERB_ALTERNATIVES: &[(&str, &[&[&str]])] = &[
    (crate::verbs::LOCATE, &[&["query"], &["cursor"]]),
    (
        crate::verbs::CONTEXT,
        &[&["entity_id"], &["entities"], &["question"]],
    ),
];

/// `tool`'s "at least one of" rule: the field sets of which a call must supply
/// one in full. Empty for a tool with no such rule.
pub fn alternatives(tool: &str) -> &'static [&'static [&'static str]] {
    ALTERNATIVES
        .iter()
        .find(|rule| rule.tool == tool)
        .map(|rule| rule.sets)
        .or_else(|| {
            VERB_ALTERNATIVES
                .iter()
                .find(|(verb, _)| *verb == tool)
                .map(|(_, sets)| *sets)
        })
        .unwrap_or(&[])
}

/// Every tool with an "at least one of" rule.
pub fn tools_with_alternatives() -> impl Iterator<Item = &'static str> {
    ALTERNATIVES.iter().map(|rule| rule.tool)
}

/// Whether `arguments` supply `field`: present and not null.
pub fn supplied(arguments: &HashMap<String, Value>, field: &str) -> bool {
    arguments.get(field).is_some_and(|value| !value.is_null())
}

/// Whether `arguments` satisfy `tool`'s "at least one of" rule, if it has one.
pub fn satisfies_alternatives(tool: &str, arguments: &HashMap<String, Value>) -> bool {
    let sets = alternatives(tool);
    sets.is_empty()
        || sets
            .iter()
            .any(|set| set.iter().all(|field| supplied(arguments, field)))
}

/// The rule in words: `query or cursor`, `base and head or scopes`.
pub fn spell(sets: &[&[&str]]) -> String {
    sets.iter()
        .map(|set| set.join(" and "))
        .collect::<Vec<_>>()
        .join(" or ")
}

/// The refusal a named call gets when it breaks `tool`'s "at least one of"
/// rule, or `None` when it keeps it. Structured the way a named refusal is:
/// what is missing, and one call that works.
pub fn refusal(tool: &str, arguments: &HashMap<String, Value>) -> Option<ToolCallResult> {
    if let Some(message) = semantic_selector_refusal(tool, arguments) {
        return Some(ToolCallResult::error(
            json!({
                "error": "unsupported_entity_selector",
                "message": message,
            })
            .to_string(),
        ));
    }
    let rule = ALTERNATIVES.iter().find(|rule| rule.tool == tool)?;
    if satisfies_alternatives(tool, arguments) {
        return None;
    }
    let example: Value = serde_json::from_str(rule.example).unwrap_or_else(|_| json!({}));
    let value = json!({
        "error": "missing_arguments",
        "message": format!(
            "{tool} needs {}, and this call supplied none of them, so nothing ran.",
            spell(rule.sets)
        ),
        "needs_one_of": rule.sets,
        "example": {"name": tool, "arguments": example},
    });
    Some(ToolCallResult::error(
        serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string()),
    ))
}

/// The same public selector boundary for MCP transports that validate their
/// own bounded request before opening repository authority.
pub fn semantic_selector_refusal(
    tool: &str,
    arguments: &HashMap<String, Value>,
) -> Option<&'static str> {
    if invalid_public_source_base(tool, arguments) {
        return Some("Copy the unchanged source_base ticket returned for this entity. This edit precondition is not supported; nothing ran.");
    }
    if nonentity_granularity(tool, arguments) {
        return Some(LOCATE_GRANULARITY_REFUSAL);
    }
    nonsemantic_selector(tool, arguments).then_some(
        "Select semantic entities by name or returned UUID. This selector is not supported; nothing ran.",
    )
}

/// Check only documented source-precondition slots before private expansion.
/// Body text, imports, descriptions and unrelated payload data are not selectors.
fn invalid_public_source_base(tool: &str, arguments: &HashMap<String, Value>) -> bool {
    let invalid = |base: &Value| {
        !crate::source_ticket::is_ticket(base)
            || base
                .get("ticket")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
    };
    if tool == "kin_draft_create" {
        return arguments.get("original_source_base").is_some_and(invalid);
    }
    if !matches!(
        tool,
        "kin_mutate"
            | "kin_mutate_durable_v1"
            | "kin_transaction_stage"
            | "kin_transaction_commit"
            | crate::verbs::MUTATE
            | crate::verbs::TRANSACTION
    ) {
        return false;
    }
    arguments
        .get("operations")
        .and_then(Value::as_array)
        .is_some_and(|operations| {
            operations.iter().any(|operation| {
                let Some(payload) = operation.get("payload").and_then(Value::as_object) else {
                    return false;
                };
                payload.get("EntitySourceBase").is_some_and(invalid)
                    || [
                        "EntitySourcePatch",
                        "EntityCreate",
                        "EntityRemove",
                        "EntityContent",
                    ]
                    .iter()
                    .any(|variant| {
                        payload
                            .get(*variant)
                            .and_then(|value| value.get("source_base"))
                            .is_some_and(invalid)
                    })
            })
        })
}

/// The refusal for a locate that asks for anything but entities. It names
/// the one granularity locate ranks, so a caller knows what to ask instead.
const LOCATE_GRANULARITY_REFUSAL: &str = "semantic_locate accepts only entity granularity. Select semantic entities by name or returned UUID; nothing ran.";

/// A locate asking to rank something other than semantic entities.
fn nonentity_granularity(tool: &str, arguments: &HashMap<String, Value>) -> bool {
    matches!(tool, "semantic_locate" | crate::verbs::LOCATE)
        && arguments
            .get("granularity")
            .and_then(Value::as_str)
            .is_some_and(|value| value != "entity")
}

/// Checked after alias expansion on both local and forwarded MCP calls.
/// Hiding a compatibility parameter in the listing does not retire it.
fn nonsemantic_selector(tool: &str, arguments: &HashMap<String, Value>) -> bool {
    // Only authority selectors are examined. Nested source, mutation bodies,
    // import literals and opaque source_base tickets remain exact program data.
    if PHYSICAL_SELECTORS
        .iter()
        .any(|key| supplied(arguments, key))
    {
        return true;
    }
    if nonsemantic_scope(tool, arguments) {
        return true;
    }
    // These are internal resolutions/observations, supplied by the daemon
    // after it resolves the public request. A client must not inject them.
    if tool == "get_context_pack"
        && ["question_focals", "coverage"]
            .iter()
            .any(|key| supplied(arguments, key))
    {
        return true;
    }
    // Status dependencies select RepoPath values, not graph relationships.
    if matches!(tool, "kin_graph_status" | crate::verbs::STATUS)
        && supplied(arguments, "dependencies")
    {
        return true;
    }
    if nonentity_granularity(tool, arguments) {
        return true;
    }
    // A literal is program text, not a repository coordinate. Its handler
    // already operates on admitted entity bodies rather than the filesystem.
    if tool == crate::handlers::lexical::TOOL_NAME {
        return false;
    }
    let coordinate = |value: &Value| {
        value
            .as_str()
            .is_some_and(crate::file_coordinate_query::looks_like_coordinate)
    };
    // The route and context resolvers interpret a nonempty @ suffix as a
    // projection pin, even when it is a bare basename with no slash.
    let pinned = |value: &Value| {
        value.as_str().is_some_and(|value| {
            value
                .rsplit_once('@')
                .is_some_and(|(name, pin)| !name.trim().is_empty() && !pin.trim().is_empty())
        })
    };
    let location_pin = match tool {
        "trace_data_flow" | "trace_path" | crate::verbs::TRACE => ["focal", "from", "to", "target"]
            .iter()
            .any(|key| arguments.get(*key).is_some_and(pinned)),
        "get_context_pack" | crate::verbs::CONTEXT => arguments
            .get("entities")
            .and_then(Value::as_array)
            .is_some_and(|values| values.iter().any(pinned)),
        _ => false,
    };
    location_pin
        || [
            "query",
            "question",
            "entity_id",
            "focal",
            "from",
            "to",
            "target",
            "proof_for_caller",
        ]
        .iter()
        .any(|key| arguments.get(*key).is_some_and(coordinate))
        || ["entities", "entity_ids", "queries"].iter().any(|key| {
            arguments
                .get(*key)
                .and_then(Value::as_array)
                .is_some_and(|values| values.iter().any(coordinate))
        })
}

/// Known top-level physical authority fields. Never recursively inspect code
/// payloads or provenance tickets, whose byte offsets are opaque authority.
const PHYSICAL_SELECTORS: &[&str] = &[
    "file",
    "files",
    "file_path",
    "path",
    "paths",
    "from_file",
    "to_file",
    "cwd",
    "directory",
    "root",
    "repo_path",
    "start_line",
    "end_line",
    "line",
];

fn semantic_scope(value: &Value, annotation: bool) -> bool {
    let Some(value) = value.as_str() else {
        return false;
    };
    if uuid::Uuid::parse_str(value).is_ok() {
        return true;
    }
    let Some((kind, id)) = value.split_once(':') else {
        return false;
    };
    match kind {
        "entity" | "contract" | "external_reference" => uuid::Uuid::parse_str(id).is_ok(),
        "work" if annotation => uuid::Uuid::parse_str(id).is_ok(),
        "change" => kin_model::Hash256::from_hex(id).is_ok(),
        _ => false,
    }
}

/// Scope-bearing tools have distinct public grammars. Validate those exact
/// fields, not similarly named labels such as a transaction's free-text scope.
fn nonsemantic_scope(tool: &str, arguments: &HashMap<String, Value>) -> bool {
    if matches!(tool, "kin_register_intent" | "kin_check_traffic") {
        return arguments.get("scopes").is_some_and(|value| {
            value.as_array().is_none_or(|scopes| {
                scopes.iter().any(|scope| {
                    scope.as_object().is_none_or(|scope| {
                        scope.len() != 1
                            || !scope.iter().all(|(kind, value)| {
                                matches!(kind.as_str(), "Entity" | "Contract")
                                    && value.as_str().is_some_and(|id| {
                                        uuid::Uuid::parse_str(id).is_ok()
                                            || (kind == "Entity"
                                                && id
                                                    .strip_prefix("external_reference:")
                                                    .is_some_and(|id| {
                                                        uuid::Uuid::parse_str(id).is_ok()
                                                    }))
                                    })
                            })
                    })
                })
            })
        });
    }
    let annotation = matches!(tool, "kin_annotation_add" | "kin_annotation_list");
    let fields: &[&str] = match tool {
        "kin_work_create" | "kin_work_link" | "kin_work_implement" => &["scopes"],
        "kin_work_list" | "kin_review_note_add" | "kin_review_discuss" => &["scope"],
        "kin_review_create" => &["scopes", "entity_ids"],
        "kin_annotation_add" | "kin_annotation_list" => &["scopes", "targets"],
        _ => &[],
    };
    fields.iter().any(|field| {
        arguments.get(*field).is_some_and(|value| {
            if value.is_null() {
                return false;
            }
            if *field == "scope" {
                return !semantic_scope(value, annotation);
            }
            value.as_array().is_none_or(|values| {
                values
                    .iter()
                    .any(|value| {
                        !semantic_scope(value, annotation)
                            // The review's legacy entity_ids parser recognizes
                            // only entity/contract scopes and bare UUIDs. A
                            // change: token there would fall back to Artifact.
                            || (*field == "entity_ids" && value.as_str().is_some_and(|value| value.starts_with("change:")))
                    })
            })
        })
    }) || (tool == "kin_review_create"
        && arguments
            .get("scope_type")
            .and_then(Value::as_str)
            .is_some_and(|kind| !matches!(kind, "entity" | "module" | "work-item")))
}

/// The nine query capabilities, including every registered compatibility name.
fn semantic_query_verb(tool: &str) -> Option<&str> {
    let verb = crate::verbs::verb_for(tool).unwrap_or(tool);
    matches!(
        verb,
        crate::verbs::LOCATE
            | crate::verbs::REFS
            | crate::verbs::TRACE
            | crate::verbs::CONTEXT
            | crate::verbs::SOURCE
            | crate::verbs::NEIGHBORS
            | crate::verbs::IMPACT
            | crate::verbs::STATUS
            | crate::verbs::HISTORY
    )
    .then_some(verb)
}

/// Public query vocabulary is shared by named, discovered and routed tools.
/// Registered handler schemas retain their types and limits, but old operator
/// descriptions and retired selectors are not an agent contract.
pub(crate) fn semantic_query_definition(tool: &mut crate::types::ToolDefinition) {
    if let Some(properties) = tool
        .input_schema
        .get_mut("properties")
        .and_then(Value::as_object_mut)
    {
        for key in PHYSICAL_SELECTORS {
            properties.remove(*key);
        }
    }
    if let Some(required) = tool
        .input_schema
        .get_mut("required")
        .and_then(Value::as_array_mut)
    {
        required.retain(|key| {
            key.as_str()
                .is_none_or(|key| !PHYSICAL_SELECTORS.contains(&key))
        });
    }
    // These are authored tool descriptions, never reply data or user source.
    for (old, new) in [
        ("derives the unit's file and owns its package clause and import block", "owns the unit's package clause and import block"),
        ("File-level create, replace, delete and rename operations are refused. Source-tree conversion and materialization are separate from semantic agent work.", "Only targeted semantic operations are admitted."),
        ("admitted; whole-file creation, replacement, deletion and relocation are refused.", "admitted."),
        ("The reply then adds staged_operation_files and carried_pending_files beside modified_files, and the change message names the count and a sample of what was carried. Neither key appears when nothing was carried.", "The receipt records this publication without attributing prior pending content to the current request."),
        ("entity/artifact intent conflicts", "applicable recorded intent conflicts"),
        ("through the retired session verb's start action", "through session's start action"),
        ("commits with identical durability, so a success naming modified_files means the body reached the file,", "commits with identical durability,"),
        (", and modified_files", ""),
        (", and modified_files.", "."),
        (", repository_generation, and modified_files,", ", repository_generation,"),
        (", and it declares the same carried_pending_files split the original answer did", ""),
        (
            "kind, language, file path, line range, signature",
            "kind, language, signature",
        ),
        (
            "name, kind, file path, and the matched relation kinds",
            "name, kind, and the matched relation kinds",
        ),
        ("file paths (resolved to their entities), or ", ""),
        ("four ways", "three ways"),
        ("entity_ids, file paths, or", "entity_ids, or"),
        (
            "unparsed files, missing spans",
            "missing semantic coverage, missing source authority",
        ),
        (
            "name, kind, file, and signature",
            "name, kind, and signature",
        ),
        ("whole-repo or file-scoped sweep", "whole-repository sweep"),
        ("entities, contracts, or artifacts", "entities or contracts"),
        (
            "entities, contracts, artifacts, changes, or work items",
            "entities, contracts, changes, or work items",
        ),
        ("entities, contracts, artifacts", "entities or contracts"),
        ("graph scopes rather than file lines", "graph scopes"),
        ("entities, not just files", "entities"),
        ("entity or file (and line)", "entity"),
        ("entity or file/line", "entity"),
        (
            "never the repository's test files",
            "not the presence of test-role entities",
        ),
    ] {
        tool.description = tool.description.replace(old, new);
    }
    if tool.name == "dead_code" {
        tool.description = "Find functions, methods and classes with no proven incoming relationships, up to limit. Read the returned verdict and coverage before interpreting a candidate as removable. find_dead_code_seeded narrows by a semantic search concept; bulk_check_references classifies known entity UUIDs.".into();
    }
    if matches!(
        tool.name.as_str(),
        "kin_register_intent" | "kin_check_traffic"
    ) {
        tool.input_schema["properties"]["scopes"]["items"] = json!({
            "type": "object",
            "properties": {
                "Entity": {"type": "string", "format": "uuid"},
                "Contract": {"type": "string", "format": "uuid"}
            },
            "minProperties": 1, "maxProperties": 1, "additionalProperties": false
        });
    }
    if let Some(property) = tool.input_schema.pointer_mut("/properties/compact") {
        if tool.name == "bulk_check_references" {
            property["description"] = json!("Return entity_id, has_references and reference_count; false also includes name, kind and matched relationship kinds.");
        }
    }
    let Some(verb) = semantic_query_verb(&tool.name) else {
        return;
    };
    tool.description = match tool.name.as_str() {
        "semantic_search" => "Find declarations by semantic name, kind or language. A module-qualified name is resolved from recorded modules; matching declarations retain distinct UUIDs. Read the verdict before treating an empty result as absence.",
        "semantic_locate" => "Find semantic entities by a description of their behavior. Return ranked identities and bounded code excerpts. Read exact code with `source`. Follow next_cursor to continue the same answer; read the verdict for coverage gaps.",
        crate::handlers::lexical::TOOL_NAME => "Find exact text in stored entity names, signatures, documentation, body previews and import context. Pass the literal itself. A text match is not a resolved relationship or proof of absence. Use refs or trace for relationships. Follow next_cursor for remaining matches.",
        "trace_path" => "Find shortest graph routes between two entities selected by exact name, qualified name or returned UUID. Each hop names an entity and its relationship to the next. A class can expand to its recorded members. Read the verdict and scope before concluding no route exists.",
        _ => match verb {
            crate::verbs::LOCATE => "Find semantic entities by name or behavior. Auto mode treats identifiers and qualified names as names, other text as a description. Literal mode finds exact text in stored entity fields, not resolved relationships. Read a module UUID with `source` for its recorded member inventory. Follow next_cursor and read the verdict for coverage gaps.",
            crate::verbs::REFS => "Find references to an entity, or its overriding methods. Pass a returned UUID as entity_id or a name as query. Qualified names or UUIDs distinguish same-named entities. Overrides requires entity_id. For counts across known UUIDs use bulk_check_references. Read each section's scope and verdict; follow next_cursor for remaining rows.",
            crate::verbs::TRACE => "Follow the recorded call relationships of an entity selected by name, qualified name or UUID. With to, return routes between the two entities. Include bodies only when their code is needed. Read the verdict and traversal bounds; follow next_cursor to continue a walk.",
            crate::verbs::CONTEXT => "Read bounded code and relationships around an entity, several named entities or a question. Names and questions resolve through the live graph. Use returned UUIDs to distinguish same-named entities. Exact focal code appears in focal_entity.body. Modules return recorded members; missing semantic coverage remains a gap.",
            crate::verbs::SOURCE => "Read one entity's exact code by UUID or unambiguous semantic name. Modules return recorded member identities, never an aggregate body. Large classes and objects return their header and member identities unless full_body is requested within its bound. Read the member needed by its UUID.",
            crate::verbs::NEIGHBORS => "Read an entity's recorded dependencies, dependents or both, addressed by UUID. The reply preserves entity identities and relationships within the requested traversal bounds. Use trace for routes between entities.",
            crate::verbs::IMPACT => "Find the effect of changing entity UUIDs, recorded change IDs or a base/head change range. Direct callers and overrides answer what must adapt; broader dependents have their own scope. Read section verdicts and follow next_cursor for remaining rows.",
            crate::verbs::STATUS => "Read the selected graph's entity and relationship counts, freshness and unresolved semantic coverage. Each query already carries its own verdict; status is not required before trusting one. Follow next_cursor to continue the same observation.",
            crate::verbs::HISTORY => "Read an entity's recorded changes, latest change, approvals and recent audit activity. Address it by UUID. Use next_offset to continue older changes. An empty recent audit list does not establish that no change occurred.",
            _ => unreachable!(),
        },
    }.into();
    // Replace only the registry fields that advertised physical selectors or
    // metadata. Other guidance, including continuation and proof semantics,
    // keeps its registered wording. Source and literal arguments are data.
    if let Some(properties) = tool
        .input_schema
        .get_mut("properties")
        .and_then(Value::as_object_mut)
    {
        for key in [
            "file",
            "files",
            "file_path",
            "path",
            "paths",
            "from_file",
            "to_file",
        ] {
            properties.remove(key);
        }
        if verb == crate::verbs::STATUS {
            properties.remove("dependencies");
        }
        for (key, property) in properties {
            let description = match (verb, key.as_str()) {
                (crate::verbs::LOCATE, "query") if tool.name == crate::verbs::LOCATE => Some("Semantic name or description; literal mode takes exact text."),
                (crate::verbs::LOCATE, "kind") if tool.name == "semantic_search" => Some(crate::handlers::common::SEMANTIC_SEARCH_KIND_GUIDANCE),
                (crate::verbs::LOCATE, "kind") => Some("Semantic declaration kind. test selects the test role; command selects a CLI entry."),
                (crate::verbs::LOCATE, "include_tests") => Some("Include test-role entities in the requested scope. Any withheld role coverage remains explicit in the verdict."),
                (crate::verbs::LOCATE, "granularity") => Some("Rank semantic entities."),
                (crate::verbs::LOCATE, "compact") if tool.name == "semantic_search" => Some("Return identities and signatures; false also includes documentation summaries."),
                (crate::verbs::CONTEXT, "entities") => Some("Semantic names or returned UUIDs. Use a UUID to distinguish same-named entities. The pack prioritizes relationships between focals, then each focal's neighborhood, within token_budget."),
                (crate::verbs::REFS, "query") => Some("Exact semantic name or recorded qualified name. A name shared by distinct entities returns a section per entity with its own scope and verdict. Pass a returned entity_id to select one entity exactly."),
                (crate::verbs::REFS, "include_tests") => Some("Include callers whose current stored role is Test. True by default. False excludes only those callers from reference answers and coverage; other roles remain. Cursor-only calls inherit this scope. False cannot be combined with proof_for_caller or view=overrides."),
                (crate::verbs::STATUS, "cursor") => Some("Returned next_cursor continues the same selected-graph observation."),
                (crate::verbs::STATUS, "max_chars" | "max_output_bytes") => Some("Hard encoded reply byte ceiling, including metadata. Follow next_cursor for the remaining observation."),
                _ => None,
            };
            if let (Some(description), Some(object)) = (description, property.as_object_mut()) {
                object.insert("description".into(), json!(description));
            }
        }
    }
}

/// The JSON-schema keywords a provider tool API refuses at a schema's top
/// level.
pub const TOP_LEVEL_COMBINATORS: [&str; 7] =
    ["oneOf", "anyOf", "allOf", "not", "if", "then", "else"];

/// The combinators at the top of `schema`, which no served schema may carry.
pub fn top_level_combinators(schema: &Value) -> Vec<&'static str> {
    TOP_LEVEL_COMBINATORS
        .into_iter()
        .filter(|keyword| schema.get(*keyword).is_some())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(value: Value) -> HashMap<String, Value> {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn all_tools_refuse_top_level_authority_selectors_without_reading_program_data() {
        for tool in crate::tools::tool_definitions().tools {
            for field in PHYSICAL_SELECTORS {
                let arguments = args(json!({(*field): "sensitive/location"}));
                assert!(
                    semantic_selector_refusal(&tool.name, &arguments).is_some(),
                    "{}: {field}",
                    tool.name
                );
                assert!(
                    tool.input_schema["properties"].get(*field).is_none(),
                    "{} advertises {field}",
                    tool.name
                );
            }
        }
        let literal = args(json!({"literal": "src/core.rs"}));
        assert!(semantic_selector_refusal("lexical_lookup", &literal).is_none());
        let operation = args(
            json!({"operations": [{"verb":"patch", "target":"semantic-id", "payload":{"EntitySourcePatch":{"source_base":{"ticket":"opaque-ticket"},"edits":[{"old_text":"import './old.js'", "new_text":"import './new.js'"}]}}}]}),
        );
        let original = operation.clone();
        assert!(semantic_selector_refusal("kin_mutate", &operation).is_none());
        assert_eq!(operation, original);
        assert!(semantic_selector_refusal(
            "kin_review_note_add",
            &args(json!({"body":"Discuss src/core.rs and import './core.js'"}))
        )
        .is_none());
    }

    #[test]
    fn public_source_preconditions_accept_only_tickets_before_private_expansion() {
        let expanded = json!({"schema":"kin.entity.source_base.v1","artifact_id":"private", "start_byte":3, "end_byte":9});
        let ticket = json!({"ticket":"read-ticket"});
        for tool in [
            "kin_mutate",
            "kin_mutate_durable_v1",
            "kin_transaction_stage",
            "kin_transaction_commit",
            "mutate",
            "transaction",
        ] {
            for variant in [
                "EntitySourceBase",
                "EntitySourcePatch",
                "EntityCreate",
                "EntityRemove",
            ] {
                for invalid in [
                    expanded.clone(),
                    json!({"ticket":"read-ticket","start_byte":3}),
                    json!({"ticket":7}),
                    json!({"ticket":""}),
                ] {
                    let base = if variant == "EntitySourceBase" {
                        invalid
                    } else {
                        json!({"source_base":invalid})
                    };
                    assert!(
                        semantic_selector_refusal(
                            tool,
                            &args(json!({"operations":[{"payload":{variant:base}}]}))
                        )
                        .is_some(),
                        "{tool}: {variant}"
                    );
                }
                let base = if variant == "EntitySourceBase" {
                    ticket.clone()
                } else {
                    json!({"source_base":ticket})
                };
                let arguments = args(
                    json!({"operations":[{"body":"source_base = {'path': 'literal.py'}", "payload":{variant:base}}]}),
                );
                let original = arguments.clone();
                assert!(
                    semantic_selector_refusal(tool, &arguments).is_none(),
                    "{tool}: {variant}"
                );
                assert_eq!(arguments, original);
            }
        }
        assert!(semantic_selector_refusal(
            "kin_draft_create",
            &args(json!({"original_source_base":expanded}))
        )
        .is_some());
        assert!(semantic_selector_refusal(
            "kin_draft_create",
            &args(json!({"original_source_base":ticket,"original_body":"import './exact.py'"}))
        )
        .is_none());
        // Unit-addressed creation has no entity source base yet.
        assert!(semantic_selector_refusal("kin_mutate", &args(json!({"operations":[{"payload":{"EntityCreate":{"repository_base":"opaque", "unit":{"package":"semantic.name"}, "body":"source_base = {'path':'exact.py'}"}}}]}))).is_none());
    }

    #[test]
    fn external_semantic_handles_reach_the_graph_aware_scope_diagnosis() {
        let address = "external_reference:123e4567-e89b-12d3-a456-426614174000";
        for (tool, field) in [
            ("kin_work_create", "scopes"),
            ("kin_work_link", "scopes"),
            ("kin_work_implement", "scopes"),
            ("kin_annotation_list", "targets"),
            ("kin_annotation_add", "targets"),
            ("kin_review_create", "scopes"),
        ] {
            assert!(semantic_selector_refusal(tool, &args(json!({field:[address]}))).is_none());
            for malformed in [
                "external_reference:src/core.rs",
                "external_reference:not-a-uuid",
            ] {
                assert!(
                    semantic_selector_refusal(tool, &args(json!({field:[malformed]}))).is_some()
                );
            }
        }
        for tool in ["kin_work_list", "kin_review_note_add", "kin_review_discuss"] {
            assert!(semantic_selector_refusal(tool, &args(json!({"scope":address}))).is_none());
        }
        for tool in ["kin_register_intent", "kin_check_traffic"] {
            assert!(
                semantic_selector_refusal(tool, &args(json!({"scopes":[{"Entity":address}]})))
                    .is_none()
            );
            assert!(semantic_selector_refusal(
                tool,
                &args(json!({"scopes":[{"Entity":"external_reference:src/core.rs"}]}))
            )
            .is_some());
        }
    }

    #[test]
    fn collaboration_scopes_admit_only_their_semantic_grammar() {
        let id = "123e4567-e89b-12d3-a456-426614174000";
        for (tool, field) in [
            ("kin_work_create", "scopes"),
            ("kin_work_link", "scopes"),
            ("kin_work_implement", "scopes"),
            ("kin_annotation_add", "targets"),
            ("kin_annotation_list", "scopes"),
            ("kin_review_create", "entity_ids"),
        ] {
            for physical in ["artifact:src/a.rs", "file:a.rs", "src/a.rs", "a.rs"] {
                assert!(
                    semantic_selector_refusal(tool, &args(json!({field:[physical]}))).is_some(),
                    "{tool}: {physical}"
                );
            }
            assert!(
                semantic_selector_refusal(
                    tool,
                    &args(json!({field:[id,format!("entity:{id}"),format!("contract:{id}")]}))
                )
                .is_none(),
                "{tool}"
            );
        }
        for tool in ["kin_work_list", "kin_review_note_add", "kin_review_discuss"] {
            assert!(
                semantic_selector_refusal(tool, &args(json!({"scope":"artifact:a.rs"}))).is_some()
            );
            assert!(semantic_selector_refusal(
                tool,
                &args(json!({"scope":format!("entity:{id}")}))
            )
            .is_none());
        }
        for tool in ["kin_register_intent", "kin_check_traffic"] {
            for scopes in [
                json!([{"Artifact":"a.rs"}]),
                json!([{"Entity":id,"Artifact":"a.rs"}]),
                json!([{"Entity":"a.rs"}]),
            ] {
                assert!(semantic_selector_refusal(tool, &args(json!({"scopes":scopes}))).is_some());
            }
            assert!(semantic_selector_refusal(
                tool,
                &args(json!({"scopes":[{"Entity":id},{"Contract":id}]}))
            )
            .is_none());
        }
        assert!(semantic_selector_refusal(
            "kin_review_create",
            &args(json!({"entity_ids":[format!("change:{}", "a".repeat(64))]}))
        )
        .is_some());
        // A transaction scope is a label; it is not parsed as a work selector.
        assert!(semantic_selector_refusal(
            "kin_transaction_begin",
            &args(json!({"scope":"repair import './core.js'"}))
        )
        .is_none());
    }

    #[test]
    fn operator_tools_are_absent_and_legacy_dispatch_refuses() {
        for name in [
            "init",
            "kin_init",
            "exec",
            "kin_session_exec",
            "kin_todo_import",
        ] {
            assert!(crate::tools::retired_file_operation(name).is_some());
            assert!(!crate::tools::tool_definitions()
                .tools
                .iter()
                .any(|tool| tool.name == name));
            assert!(!crate::tools::surface_definitions()
                .tools
                .iter()
                .any(|tool| tool.name == name));
            let mut call = name.to_string();
            assert!(crate::verbs::resolve(&mut call, &mut args(json!({}))).is_err());
        }
        for arguments in [
            json!({"action":"exec","argv":["cat","secret"]}),
            json!({"argv":["cat","secret"]}),
        ] {
            assert!(
                crate::verbs::resolve(&mut "session".to_string(), &mut args(arguments)).is_err()
            );
        }
        let mut call = "session".to_string();
        let mut arguments = args(json!({"vendor":"client", "client_name":"semantic client"}));
        crate::verbs::resolve(&mut call, &mut arguments).unwrap();
        assert_eq!(call, "kin_session_start");
        assert!(!arguments.contains_key("cwd"));
    }

    #[test]
    fn semantic_queries_refuse_coordinates_and_hidden_location_selectors() {
        for (tool, arguments) in [
            ("semantic_search", json!({"query":"src/core.rs"})),
            ("semantic_locate", json!({"query":"./core.rs"})),
            ("find_references", json!({"query":"./core.rs"})),
            ("get_entity_source", json!({"entity_id":"./core.rs"})),
            ("graph_neighborhood", json!({"entity_id":"./core.rs"})),
            ("kin_provenance_query", json!({"entity_id":"./core.rs"})),
            ("get_context_pack", json!({"entities":["src/core.rs"]})),
            ("get_context_pack", json!({"question":"./core.rs"})),
            (
                "trace_data_flow",
                json!({"focal":"run", "from_file":"src/core.rs"}),
            ),
            (
                "trace_path",
                json!({"from":"run", "to":"finish", "to_file":"src/core.rs"}),
            ),
            ("impact_analysis", json!({"files":["src/core.rs"]})),
            ("kin_graph_status", json!({"dependencies":["src/core.rs"]})),
            ("kin_graph_status", json!({"dependencies":[]})),
            ("trace_path", json!({"from":"run@core.rs:9", "to":"finish"})),
            (
                "trace_data_flow",
                json!({"focal":"run", "target":"src/core.rs"}),
            ),
            ("get_context_pack", json!({"entities":["run@core.rs"]})),
            (
                "get_context_pack",
                json!({"entity_id":"held", "question_focals":[]}),
            ),
            (
                "get_context_pack",
                json!({"entity_id":"held", "coverage":"complete"}),
            ),
            (
                "semantic_locate",
                json!({"query":"run", "granularity":"file"}),
            ),
            (
                "lexical_lookup",
                json!({"literal":"src/core.rs", "file":"src/core.rs"}),
            ),
        ] {
            let refused = refusal(tool, &args(arguments)).expect(tool);
            let text = serde_json::to_string(&refused).unwrap();
            assert!(text.contains("unsupported_entity_selector"));
            assert!(!text.contains("core.rs"));
        }
        // A locate asking for another granularity is told the one it accepts.
        for tool in ["semantic_locate", crate::verbs::LOCATE] {
            let refused =
                refusal(tool, &args(json!({"query":"run", "granularity":"file"}))).expect(tool);
            let text = serde_json::to_string(&refused).unwrap();
            assert!(text.contains("unsupported_entity_selector"), "{text}");
            assert!(
                text.contains("semantic_locate accepts only entity granularity"),
                "{text}"
            );
        }
        for (tool, arguments) in [
            ("semantic_search", json!({"query":"core.run"})),
            ("get_context_pack", json!({"entities":["run"]})),
            ("trace_path", json!({"from":"core.run", "to":"finish"})),
            ("lexical_lookup", json!({"literal":"src/core.rs"})),
            (
                "lexical_lookup",
                json!({"literal":"import x from './core.js'"}),
            ),
            (
                "get_context_pack",
                json!({"entities":["Widget#Class", "cached.<locals>.decorator"]}),
            ),
            (
                "semantic_locate",
                json!({"query":"where imports like './core.js' are resolved"}),
            ),
            (
                "kin_graph_status",
                json!({"cursor":"opaque", "max_chars":4000}),
            ),
        ] {
            assert!(refusal(tool, &args(arguments)).is_none(), "{tool}");
        }
    }

    #[test]
    fn semantic_query_registry_and_profiles_use_semantic_vocabulary() {
        let registry = crate::tools::tool_definitions();
        let profiles = [
            (None, false),
            (
                Some(crate::tools::name_set(
                    crate::tools::agent_query_tool_names(),
                )),
                true,
            ),
            (
                Some(crate::tools::name_set(crate::tools::benchmark_tool_names())),
                false,
            ),
        ];
        let lists = profiles
            .into_iter()
            .map(|(allowed, belt)| crate::tools::served_tools_list(allowed.as_ref(), belt))
            .chain(std::iter::once(registry));
        for list in lists {
            for tool in list
                .tools
                .iter()
                .filter(|tool| semantic_query_verb(&tool.name).is_some())
            {
                let properties = tool.input_schema["properties"].as_object().unwrap();
                for retired in [
                    "file",
                    "files",
                    "file_path",
                    "path",
                    "paths",
                    "from_file",
                    "to_file",
                ] {
                    assert!(
                        !properties.contains_key(retired),
                        "{}: {retired}",
                        tool.name
                    );
                }
                if semantic_query_verb(&tool.name) == Some(crate::verbs::STATUS) {
                    assert!(!properties.contains_key("dependencies"));
                }
                let text = serde_json::to_string(&json!({
                    "description": tool.description, "schema": tool.input_schema,
                }))
                .unwrap()
                .to_ascii_lowercase();
                for word in text.split(|ch: char| !ch.is_alphanumeric() && ch != '_') {
                    assert!(
                        ![
                            "file",
                            "files",
                            "file_path",
                            "filesystem",
                            "directory",
                            "coordinate"
                        ]
                        .contains(&word),
                        "{} still advertises {word}",
                        tool.name
                    );
                }
            }
        }
    }

    #[test]
    fn alias_discovery_and_routing_reach_the_same_selector_refusal() {
        let calls = [
            json!({"name":"find_declarations","arguments":{"query":"src/core.rs"}}),
            json!({"name":"semantic_search","arguments":{"query":"src/core.rs"}}),
            json!({"name":"locate","arguments":{"query":"src/core.rs"}}),
            json!({"name":"kin_tool_call","arguments":{"tool":"locate","arguments":{"query":"src/core.rs"}}}),
            json!({"name":"kin","arguments":{"command":"locate","args":{"query":"src/core.rs"}}}),
        ];
        for value in calls {
            let mut call: crate::types::ToolCallParams = serde_json::from_value(value).unwrap();
            let routed =
                crate::routed::route(&mut call, Some(crate::routed::RoutedSurface::READ_ONLY));
            assert!(!matches!(routed, crate::routed::Routing::Answer(_)));
            crate::tool_invocation::expand(&mut call, None).unwrap();
            crate::verbs::resolve(&mut call.name, &mut call.arguments).unwrap();
            assert!(
                semantic_selector_refusal(&call.name, &call.arguments).is_some(),
                "{}",
                call.name
            );
        }
        let mut literal = args(json!({"query":"src/core.rs", "mode":"literal"}));
        let mut tool = crate::verbs::LOCATE.to_string();
        crate::verbs::resolve(&mut tool, &mut literal).unwrap();
        assert!(semantic_selector_refusal(&tool, &literal).is_none());
        assert_eq!(literal["literal"], "src/core.rs");
    }

    #[test]
    fn every_example_keeps_its_own_rule() {
        for rule in ALTERNATIVES {
            let example: HashMap<String, Value> = serde_json::from_str(rule.example).unwrap();
            assert!(
                satisfies_alternatives(rule.tool, &example),
                "{}'s example breaks its rule: {}",
                rule.tool,
                rule.example
            );
            let registered = crate::tools::tool_definitions();
            let schema = &registered
                .tools
                .iter()
                .find(|tool| tool.name == rule.tool)
                .unwrap_or_else(|| panic!("{} is not registered", rule.tool))
                .input_schema;
            for field in rule.sets.iter().flat_map(|set| set.iter()) {
                assert!(
                    schema["properties"].get(*field).is_some(),
                    "{}'s rule names {field}, which its schema does not define",
                    rule.tool
                );
            }
            for field in example.keys() {
                assert!(
                    schema["properties"].get(field).is_some(),
                    "{}'s example passes {field}, which its schema does not define",
                    rule.tool
                );
            }
        }
    }

    /// The five schemas as served at cf32b66b3, reduced to what a validator
    /// reads for these rules: `type`, `required`, each property's `type`, and
    /// the top-level combinator. `kin_mutate` rides along for the record; its
    /// keyed rule is proved where it is enforced.
    const BEFORE: &str = include_str!("testdata/combinator_schemas_cf32b66b3.json");

    /// A JSON-schema validator for exactly the keywords the old schemas used.
    /// Anything else in a schema is a failure of this test, not a pass.
    fn valid(schema: &Value, instance: &Value) -> bool {
        let object = schema.as_object().expect("a schema is an object");
        for (keyword, rule) in object {
            let ok = match keyword.as_str() {
                "type" => match rule.as_str().unwrap() {
                    "object" => instance.is_object(),
                    "string" => instance.is_string(),
                    "array" => instance.is_array(),
                    "integer" => instance.is_i64() || instance.is_u64(),
                    "boolean" => instance.is_boolean(),
                    other => panic!("unexpected type {other}"),
                },
                "required" => rule
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|name| instance.get(name.as_str().unwrap()).is_some()),
                "properties" => rule.as_object().unwrap().iter().all(|(name, property)| {
                    instance
                        .get(name)
                        .is_none_or(|value| valid(property, value))
                }),
                "anyOf" => rule
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|branch| valid(branch, instance)),
                "allOf" => rule
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|branch| valid(branch, instance)),
                "if" => {
                    let then = object.get("then").cloned().unwrap_or(json!({}));
                    !valid(rule, instance) || valid(&then, instance)
                }
                "then" => true,
                "const" => instance == rule,
                "propertyNames" => instance
                    .as_object()
                    .is_none_or(|fields| fields.keys().all(|name| valid(rule, &json!(name)))),
                "enum" => rule.as_array().unwrap().contains(instance),
                other => panic!("the evaluator does not read {other}"),
            };
            if !ok {
                return false;
            }
        }
        true
    }

    /// The old schema without its top-level combinators: what is served now.
    fn flattened(schema: &Value) -> Value {
        let mut flat = schema.clone();
        let object = flat.as_object_mut().unwrap();
        for keyword in TOP_LEVEL_COMBINATORS {
            object.remove(keyword);
        }
        flat
    }

    /// Every input over each rule's fields, each absent, a well-typed value or
    /// null, alone and beside an unrelated field and the tool's other required
    /// fields, is accepted by the served schema and the server's rule exactly
    /// when the old schema accepted it.
    #[test]
    fn the_server_rule_accepts_exactly_what_the_old_schema_accepted() {
        let before: std::collections::BTreeMap<String, Value> =
            serde_json::from_str(BEFORE).unwrap();
        let mut graded = 0usize;
        for tool in tools_with_alternatives() {
            let old = &before[tool];
            assert!(
                old.get("anyOf").is_some(),
                "{tool} carried no anyOf at cf32b66b3"
            );
            let now = crate::tools::tool_definitions()
                .tools
                .into_iter()
                .find(|candidate| candidate.name == tool)
                .unwrap()
                .input_schema;
            assert!(
                top_level_combinators(&now).is_empty(),
                "{tool} still serves a combinator"
            );
            let fields: Vec<&str> = {
                let mut fields: Vec<&str> = alternatives(tool)
                    .iter()
                    .flat_map(|set| set.iter().copied())
                    .collect();
                fields.sort_unstable();
                fields.dedup();
                fields
            };
            let typed = |field: &str| match old["properties"][field]["type"].as_str() {
                Some("array") => json!(["x"]),
                Some("string") => json!("x"),
                other => panic!("{tool}.{field} has type {other:?}"),
            };
            // Each field takes one of three states: absent, typed, null.
            for mask in 0..3usize.pow(fields.len() as u32) {
                let mut code = mask;
                let mut instance = serde_json::Map::new();
                for field in &fields {
                    match code % 3 {
                        0 => {}
                        1 => {
                            instance.insert(field.to_string(), typed(field));
                        }
                        _ => {
                            instance.insert(field.to_string(), Value::Null);
                        }
                    }
                    code /= 3;
                }
                for extra in [false, true] {
                    let mut instance = instance.clone();
                    for required in old["required"].as_array().into_iter().flatten() {
                        let name = required.as_str().unwrap();
                        instance.insert(name.to_string(), json!("x"));
                    }
                    if extra {
                        instance.insert("limit".to_string(), json!(3));
                    }
                    let instance = Value::Object(instance);
                    let arguments: HashMap<String, Value> =
                        serde_json::from_value(instance.clone()).unwrap();
                    let was = valid(old, &instance);
                    let is = valid(&flattened(old), &instance)
                        && satisfies_alternatives(tool, &arguments);
                    assert_eq!(was, is, "{tool} {instance}: before {was}, now {is}");
                    graded += 1;
                }
            }
        }
        assert!(graded >= 200, "graded only {graded} inputs");
    }

    /// No tool on any profile serves a schema that opens with a combinator.
    /// Each profile's listing is built by the function `tools/list` writes it
    /// with. `kin-cli` holds the same over every profile token it accepts.
    #[test]
    fn no_profile_serves_a_top_level_combinator() {
        let names = |list: &'static [&'static str]| Some(crate::tools::name_set(list));
        let profiles: Vec<(&str, crate::server::McpServerConfig)> = vec![
            (
                "agent-default",
                crate::tools::agent_default_tool_names(),
                true,
                false,
            ),
            (
                "agent-query",
                crate::tools::agent_query_tool_names(),
                true,
                true,
            ),
            (
                "agent-search",
                crate::tools::agent_search_tool_names(),
                true,
                true,
            ),
            (
                "benchmark",
                crate::tools::benchmark_tool_names(),
                false,
                false,
            ),
            (
                "context-bench",
                crate::tools::context_bench_tool_names(),
                false,
                false,
            ),
        ]
        .into_iter()
        .map(|(profile, list, belt, numbered)| {
            (
                profile,
                crate::server::McpServerConfig {
                    allowed_tools: names(list),
                    agent_belt: belt,
                    number_entity_lines: numbered,
                    ..Default::default()
                },
            )
        })
        .chain([
            ("full", crate::server::McpServerConfig::default()),
            (
                "agent-routed",
                crate::server::McpServerConfig {
                    routed: Some(crate::routed::RoutedSurface::WITH_WRITES),
                    ..Default::default()
                },
            ),
            (
                "agent-routed-query",
                crate::server::McpServerConfig {
                    routed: Some(crate::routed::RoutedSurface::READ_ONLY),
                    ..Default::default()
                },
            ),
        ])
        .collect();
        let mut served = 0usize;
        let mut offenders = Vec::new();
        for (profile, config) in &profiles {
            for tool in crate::server::served_tools_for(config).tools {
                served += 1;
                let found = top_level_combinators(&tool.input_schema);
                if !found.is_empty() || tool.input_schema["type"] != "object" {
                    offenders.push(format!("{profile}/{}: {found:?}", tool.name));
                }
            }
        }
        assert!(offenders.is_empty(), "{offenders:#?}");
        assert!(served > 100, "the sweep served only {served} tools");
    }

    #[test]
    fn a_refusal_names_what_is_missing_and_one_call_that_works() {
        let refused = refusal("semantic_locate", &args(json!({"limit": 5}))).unwrap();
        assert_eq!(refused.is_error, Some(true));
        let crate::types::ContentBlock::Text { text } = &refused.content[0];
        let answer: Value = serde_json::from_str(text).unwrap();
        assert!(answer["message"]
            .as_str()
            .unwrap()
            .contains("needs query or cursor"));
        assert_eq!(answer["example"]["name"], "semantic_locate");
        assert_eq!(
            answer["example"]["arguments"]["query"],
            "where failed requests are retried"
        );
        assert!(refusal("semantic_locate", &args(json!({"cursor": "c"}))).is_none());
        // A null field supplies nothing.
        assert!(refusal("semantic_locate", &args(json!({"query": null}))).is_some());
        // A tool with no rule is never refused here.
        assert!(refusal("trace_path", &args(json!({}))).is_none());
        assert_eq!(
            spell(alternatives("kin_review_create")),
            "base and head or scope_type and entity_ids or scopes"
        );
    }
}
