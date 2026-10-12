// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

use thiserror::Error;

/// Errors from the context pack builder.
#[derive(Debug, Error)]
pub enum ContextError {
    #[error("entity not found: {0}")]
    EntityNotFound(String),

    /// Raised only once the pack is down to one focal named by its kind, with
    /// nothing around it, and still measures over its budget. `actual` is
    /// what that smallest pack measures, which is a budget that builds it.
    #[error(
        "the smallest context pack for this request measures {actual} tokens, more than the \
         {budget}-token budget; ask for a budget of at least {actual} tokens"
    )]
    BudgetExceeded { actual: usize, budget: usize },

    #[error("graph error: {0}")]
    Graph(String),

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, ContextError>;
