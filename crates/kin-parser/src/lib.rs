// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Tree-sitter parsing and language adapters for Kin.
//!
//! This crate provides the `LanguageAdapter` trait and built-in adapters
//! for TypeScript, JavaScript, Python, Go, Java, Rust, C, C++, C#, Ruby, PHP, Swift,
//! Kotlin, and HCL/Terraform.

pub const PARSER_SCHEMA_EPOCH: &str = "parser-schema-2026-03-29-v1";

pub mod adapter;
pub mod binding_callability;
pub mod body_syntax;
pub mod call_owner;
pub mod code_placement;
pub mod declaration_census;
pub mod declaration_extent;
pub mod declaration_lifecycle;
pub mod declaration_only;
pub mod error;
pub mod extract;
pub mod file_evidence;
pub mod import_origin;
pub mod import_witness;
pub mod key_domain;
pub mod languages;
pub mod literal_members;
pub mod member_placement;
pub mod module_contains;
pub mod name_syntax;
pub mod nested;
pub mod project_manifest;
pub mod scope;
pub mod shallow;
pub mod site_syntax;
pub mod source_span;
pub mod syntax_reading;
pub mod todos;
pub mod unit_append;
pub mod unit_editor;
pub mod value_escape;
pub mod value_origins;

#[cfg(test)]
mod language_conformance_test;

/// Re-exported because this crate's public signatures name it: `LanguageAdapter::extract`
/// and [`parse_shallow_file`] both take a `&FilePathId`. Without the re-export a caller
/// must declare its own `kin-model` dependency purely to spell the argument type, and any
/// version skew between that dependency and the one this crate resolves produces two
/// distinct `FilePathId` types and a mismatched-types error at the call site. Reaching the
/// type through `kin_parser` makes that skew unrepresentable.
pub use kin_model::FilePathId;

pub use adapter::{EditHint, LanguageAdapter};
pub use error::{ParseError, Result};
pub use extract::{
    attach_file_context_metadata, attach_file_nested_definition_record,
    attach_file_reference_parse_counts, call_extraction_incomplete_marker,
    is_call_extraction_incomplete_marker, is_js_bare_package_specifier,
    is_scoped_call_extraction_incomplete_marker, scoped_call_extraction_incomplete_marker,
    separate_shared_identities, CallArgShape, ExtractedDerivedMember, ExtractedEntity,
    ExtractedRelation, ExtractedTest, ExtractedTestKind, FileImport, ImportedName, ParseOutput,
    RelationSite, RelationSyntacticRole, CALL_EXTRACTION_INCOMPLETE_MARKER_V1,
    COMMAND_EFFECT_CONTRACT_KEY, DECLARATION_LINE_KEY, FILE_IMPORT_CONTEXT_KEY,
    FILE_NESTED_DEFINITIONS_NOT_ADMITTED_KEY, FILE_PARSED_CALL_SITES_KEY,
    FILE_PARSED_EXTERNAL_MODULE_IMPORTS_KEY, FILE_PARSED_IMPORT_STATEMENTS_KEY,
    FILE_SURFACE_CONTEXT_KEY, NESTED_DEFINITIONS_NOT_ADMITTED_KEY, NESTED_DEFINITIONS_RECORD_CAP,
};
pub use file_evidence::{is_file_evidence_record, is_reserved_parser_record};
pub use languages::{
    attach_go_command_effect_contract_metadata, attach_go_package_metadata, is_python_builtin_name,
    language_ordinal, AdapterRegistry, CAdapter, CSharpAdapter, CppAdapter, GoAdapter, HclAdapter,
    JavaAdapter, JavaScriptAdapter, KotlinAdapter, PhpAdapter, PythonAdapter, RubyAdapter,
    RustAdapter, SwiftAdapter, TypeScriptAdapter, ALL_LANGUAGE_IDS,
    DYNAMIC_MEMBERS_DISCLOSURE_PREFIX, PYTHON_BUILTIN_NAMES,
};
pub use nested::{
    is_local_scope_name, local_scope_name, NestedDefinitionCensus, UnadmittedNestedDefinition,
};
pub use shallow::{
    extract_shallow, get_shallow_grammar, parse_shallow_file, ShallowDecl, ShallowDeclKind,
    ShallowFile, ShallowFingerprint, ShallowImport,
};
pub use source_span::validate_module_source_span;
pub use todos::{todos_in_text, todos_in_unparsed_text, ExtractedTodo};
