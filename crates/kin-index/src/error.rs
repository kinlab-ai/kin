// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

use thiserror::Error;

#[derive(Debug, Error)]
pub enum IndexError {
    #[error("parse error: {0}")]
    Parse(#[from] kin_parser::ParseError),

    #[error("blob store error: {0}")]
    Blob(#[from] kin_blobs::BlobError),

    #[error("graph store error: {0}")]
    Graph(String),

    #[error("file I/O error: {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },

    #[error("source bytes for {path} have digest {actual}, not supplied blob digest {expected}")]
    SourceDigestMismatch {
        path: String,
        expected: kin_blobs::Hash256,
        actual: kin_blobs::Hash256,
    },

    #[error("unsupported file: {0}")]
    UnsupportedFile(String),

    #[error("invalid historical semantic input: {0}")]
    InvalidHistoricalSemantics(String),

    #[error("watcher error: {0}")]
    Watcher(String),

    #[error(transparent)]
    IncompleteRepositoryScan(#[from] crate::repository::IncompleteRepositoryScan),

    /// A manifest a unit's name depends on is held by the tree and cannot be
    /// read, so the unit is not parsed rather than named without it.
    #[error(transparent)]
    UnreadableManifest(#[from] kin_parser::project_manifest::UnreadableManifest),
}

impl IndexError {
    pub fn io(path: impl Into<String>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}

pub type Result<T> = std::result::Result<T, IndexError>;
