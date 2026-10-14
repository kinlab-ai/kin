// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

pub mod agent_belt;
pub(crate) mod answer_account;
pub mod artifact_entity;
pub mod ask;
pub mod budget;
pub mod call_sites;
pub mod caller_arrival;
pub mod command_shape;
pub mod connection_session;
pub mod container_shape;
pub mod daemon_delegate;
pub mod declaration_coverage;
pub mod edge_coverage;
pub mod entity_address;
pub mod entity_drafts;
pub mod entity_lifecycle;
pub mod entity_lines;
pub mod envelope;
pub mod error;
pub mod file_coordinate_query;
pub mod first_contact;
pub mod graph_position;
pub mod handlers;
pub mod impact_pages;
pub mod impact_sections;
pub mod input_contract;
pub(crate) mod lean_reply;
pub mod locate_pages;
pub mod member_placement;
pub mod module_code;
mod module_name_query;
pub mod negative;
pub mod outside_graph;
pub mod plain_reply;
pub mod query_reply;
pub mod query_tokens;
pub mod receiver_binding;
pub mod reference_pages;
pub mod remediation;
pub mod replies;
pub(crate) mod reply_budget;
pub mod request_schema;
pub mod requests;
pub mod review_flags;
pub mod routed;
#[cfg(test)]
pub(crate) mod schema_check;
#[doc(hidden)]
pub mod semantic_owners;
pub mod semantic_surface;
pub mod server;
pub mod session;
pub mod session_handles;
pub mod session_idle_floor;
pub mod site_expression;
pub mod source_base;
pub mod source_derivation;
pub mod source_pages;
pub mod source_ticket;
pub mod source_unit;
pub mod startup_binding;
pub mod syntax_gate;
pub mod text_reply;
pub(crate) mod tool_invocation;
pub mod tools;
pub(crate) mod trace_compact;
pub mod trace_named_scope;
pub(crate) mod trace_page_reply;
#[cfg(test)]
mod trace_page_reply_tests;
pub mod trace_pages;
pub mod trace_reach;
pub mod trace_scope;
pub mod types;
pub mod undecided_sites;
pub mod verbs;
pub mod verdict;
pub mod working_copy;
pub mod write_reply;

pub use agent_belt::{
    apply_belt_defaults, canonicalize_tool_name, compact_for_agent_default,
    AGENT_DEFAULT_DESCRIPTION_BUDGET, AGENT_DEFAULT_PROFILE_DESCRIPTION_BUDGET,
    DECLARATION_FILTER_ALIAS, DECLARATION_FILTER_CANONICAL,
};
pub use budget::{
    is_budgeted as is_budgeted_tool, BudgetAccounting, ResponseBudget, RESPONSE_DEFAULT_MAX_CHARS,
};
pub use call_sites::{
    publish_current_proof_contexts, published_current_proof_contexts, CALL_SITES_KEY,
};
pub use daemon_delegate::note_startup_repository;
pub use daemon_delegate::{readiness_budget, DAEMON_PATIENCE_ENV};
pub use edge_coverage::EDGE_COVERAGE_KEY;
pub use envelope::{
    annotate as annotate_with_envelope, finalize as finalize_with_envelope,
    finalize_bounded as finalize_with_envelope_bounded, Envelope, ENVELOPE_KEY, ENVELOPE_VERSION,
};
pub use error::{McpError, Result};
pub use handlers::LocalRepositoryAuthorityBinding;
pub use negative::NEGATIVE_KEY;
pub use outside_graph::OUTSIDE_GRAPH_KEY;
pub use server::{
    present_flagged_for_client, present_for_client, process_daemon_message, process_message,
    run_stdio, run_stdio_daemon, BoundRepo, McpServerConfig, RepoBinder, SessionAuthorityMode,
    WorkspaceBinding,
};
pub use session::{
    AssistantSession, CommitRefusal, CommitRefusalCode, CoordinationEnforcementMode,
    CoordinationSurfaceCoverage, CoordinationWritePreflight, IntentRegistrationAttempt,
    McpMutationOperation, McpMutationPayload, McpTransaction, SessionRegistry,
};
pub use startup_binding::{StartupBindingState, StartupDaemonBinding, StartupProgress};
pub use tools::{
    agent_ask_tool_names, agent_default_tool_names, agent_query_tool_names,
    agent_routed_tool_names, agent_search_tool_names, benchmark_tool_names,
    context_bench_tool_names, name_set as tool_name_set, served_tools_list, tool_definitions,
    AGENT_QUERY_LIST_CEILING_BYTES, AGENT_SEARCH_LIST_CEILING_BYTES,
};
pub use types::{
    ContentBlock, JsonRpcRequest, JsonRpcResponse, ToolCallParams, ToolCallResult, ToolDefinition,
};
pub use verdict::{disagreements as verdict_disagreements, Verdict, VERDICT_KEY};
pub use working_copy::{HostEntryReading, WorkingCopyProbe, WorkingCopySource, WorkingCopySurface};

pub mod status_pages;
