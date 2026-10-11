// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Definitions written inside a function body, and the record of the ones an
//! adapter still leaves out.
//!
//! A class declared inside a test function, a helper `def` inside another
//! `def`, and an object literal of methods bound inside a factory are real
//! declarations: code overrides through them, calls them, and reads them. An
//! adapter that walks only module and class scope gives them no entity, so an
//! override question about `SessionInterface.open_session` misses the test
//! double that overrides it, and every call written in the nested body is
//! charged to the function around it.
//!
//! Admitting them needs a name that cannot collide. The linker indexes a file's
//! entities by `(file, name)` and keeps one entity per key, so a nested `index`
//! must not share a name with a module-level `index`, nor with the `index`
//! another test in the same file declares. The rule here is CPython's
//! `__qualname__`: a definition nested in a function body whose entity is `F` is
//! named `F.<locals>.name`. `<locals>` is not an identifier in any language this
//! crate parses, so no declaration written at module or class scope can spell
//! it, and every adapter spelling it through [`local_scope_name`] keeps the
//! names identical across languages.
//!
//! What an adapter still does not admit is recorded rather than dropped, in a
//! [`NestedDefinitionCensus`]. A reader asked about a file can then say which
//! definitions its answer cannot see, instead of reading their absence as an
//! absence in the code.

use kin_model::EntityKind;
use serde::{Deserialize, Serialize};

/// The path segment that marks a name as local to the function before it.
pub const LOCAL_SCOPE_SEGMENT: &str = "<locals>";

/// The entity name of `name` defined in the body of the entity `enclosing`.
///
/// `local_scope_name("test_open_session", "MySessionInterface")` is
/// `test_open_session.<locals>.MySessionInterface`. Members of such a definition
/// keep each adapter's ordinary member rule on the qualified owner, so the
/// method `open_session` of that class is
/// `test_open_session.<locals>.MySessionInterface.open_session`, and the last
/// segment is still the name the source writes.
pub fn local_scope_name(enclosing: &str, name: &str) -> String {
    format!("{enclosing}.{LOCAL_SCOPE_SEGMENT}.{name}")
}

/// Whether an entity name was minted by [`local_scope_name`] anywhere along
/// its path, meaning the entity is declared inside some function body.
///
/// A reader uses this to keep a module-scope rule off a nested declaration:
/// a `test_*` closure inside a test is not itself a test the runner collects.
pub fn is_local_scope_name(name: &str) -> bool {
    name.contains(&format!(".{LOCAL_SCOPE_SEGMENT}."))
}

/// The nested definitions one file's adapter took a census of.
///
/// `ParseOutput::nested_definitions` is `None` when the adapter took no census,
/// which a reader must treat as unmeasured. `Some` with an empty list is a
/// measured file in which every nested definition was admitted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NestedDefinitionCensus {
    /// Every definition the adapter found in a nested position and did not
    /// admit as an entity, in source order.
    pub not_admitted: Vec<UnadmittedNestedDefinition>,
}

impl NestedDefinitionCensus {
    /// Record one definition the adapter found and did not admit.
    pub fn record(&mut self, definition: UnadmittedNestedDefinition) {
        self.not_admitted.push(definition);
    }
}

/// One definition an adapter found in a nested position and left without an
/// entity of its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnadmittedNestedDefinition {
    /// The name the definition would carry if it were admitted, built with
    /// [`local_scope_name`] where it sits in a function body, so two
    /// same-named closures in different functions stay distinguishable.
    pub name: String,
    /// What the definition declares.
    pub kind: EntityKind,
    /// The 0-based line the definition starts on, the convention
    /// `SourceSpan::start_line` uses.
    pub start_line: u32,
    /// The innermost admitted entity whose body holds the definition. Its
    /// calls and references stay attributed to that entity, which is where a
    /// reader has to look for them.
    pub enclosing: Option<String>,
    /// The base classes a class declares, as written. Empty for anything else.
    pub bases: Vec<String>,
}

/// The local-scope names one file has admitted, so a second definition that
/// would mint the same name is recorded instead of admitted.
///
/// Two definitions under one name break the linker's `(file, name)` index, and
/// the one it keeps is an accident of order. The first definition in source
/// order keeps the name; a later one is a census entry.
#[derive(Debug, Default)]
pub struct LocalScopeNames {
    claimed: std::collections::HashSet<String>,
}

impl LocalScopeNames {
    /// Claim `name` for one definition. `false` means another definition in
    /// the file already holds it.
    pub fn claim(&mut self, name: &str) -> bool {
        self.claimed.insert(name.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_local_scope_name_reads_like_a_python_qualname() {
        let class = local_scope_name("test_session_error_pops_context", "FailingSessionInterface");
        assert_eq!(
            class,
            "test_session_error_pops_context.<locals>.FailingSessionInterface"
        );
        assert_eq!(
            local_scope_name("C.m", "inner"),
            "C.m.<locals>.inner",
            "a definition nested in a method keeps the method's qualified name"
        );
        assert!(is_local_scope_name(&class));
        assert!(is_local_scope_name(&format!("{class}.open_session")));
    }

    #[test]
    fn a_module_or_member_name_is_not_a_local_scope_name() {
        for name in ["index", "Flask.route", "ns::f", "locals", "a.locals.b"] {
            assert!(!is_local_scope_name(name), "{name}");
        }
    }

    #[test]
    fn a_second_claim_on_one_name_is_refused() {
        let mut names = LocalScopeNames::default();
        assert!(names.claim("f.<locals>.g"));
        assert!(!names.claim("f.<locals>.g"));
        assert!(names.claim("h.<locals>.g"));
    }
}
