// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The `ask` front door: a question in plain words, answered in one reply.
//!
//! [`extract`] and [`plan`] read what a question says, without a repository:
//! the names it asks about, what it asks of them, the module it qualifies a
//! name with, and the package or test limits it sets. Paths stay unapplied
//! metadata, and a plan carries no binding or coverage proof.
//!
//! [`answer`] then runs the verbs that answer the plan, through
//! [`dispatch`], the same path a client's own calls take, and composes the
//! reply: what Kin ran and what it read the question as, what the question
//! asked for that the query did not apply, and each verb's answer, verdict
//! first.

mod compose;
mod definition;
pub mod dispatch;
mod extract;
pub mod prompt;
mod route;
mod sub_answer;

pub use compose::{answer, AskRequest, Composite, SubCalls, TOOL_NAME};
pub use definition::definition;
pub use extract::{extract, Extracted};
pub use route::{plan, DeclarationQuestion, Intent, Plan};
pub use sub_answer::{Declaration, Standing, SubAnswer};
