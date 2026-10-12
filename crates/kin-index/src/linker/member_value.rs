// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Bound members a method reads as values through its own instance.
//!
//! An adapter that reads `Consumer(process_event=self._process_event)` names
//! the enclosing class's member as `Class.member` on a `References` relation
//! tagged [`RelationSyntacticRole::PythonSelfMemberValue`]. Passing a bound
//! method is a use of it and not a call, so the edge is a reference, and it
//! says nothing about who later invokes the value or on which instance.
//!
//! The destination is the method the class declares, else the one its
//! hierarchy supplies. A member no callable declaration answers for, such as
//! instance data, binds nothing, and nothing falls back to a same-named
//! member elsewhere. When a subclass overrides the method, the instance's
//! runtime class decides which body the value is bound to, so the edge keeps
//! the self-dispatch qualification a `self.m()` call to the same method gets.

use kin_parser::{ExtractedRelation, RelationSyntacticRole};

/// Evidence rule on the site record of a member value reference.
pub const SELF_MEMBER_VALUE_RULE: &str = "python_self_member_value_v1";

/// Whether a raw relation is a member read as a value through the instance.
pub(crate) fn is_self_member_value(rel: &ExtractedRelation) -> bool {
    rel.site.as_ref().and_then(|site| site.syntactic_role)
        == Some(RelationSyntacticRole::PythonSelfMemberValue)
}
