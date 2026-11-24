use std::fmt;

use grove_types::Span;

#[derive(Debug, Clone, PartialEq)]
pub enum ExecError {
    /// A valid query failing at runtime on data
    Abort { message: String, span: Span },
    /// Backend or environment failure (SQLite errors, DB/shape mismatch)
    Storage { message: String },
    /// Broken compiler/executor invariant; not caused by user data or query
    Internal { message: String },
    /// Data violating its schema or a schema/storage contract
    Integrity { message: String },
    /// A feature that the executor does not yet implement
    Unsupported { message: String },
    /// Storage does not permit mutations
    ReadOnly { message: String },
}

impl fmt::Display for ExecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExecError::Abort { message, .. } => write!(f, "query aborted: {message}"),
            ExecError::Storage { message } => write!(f, "storage error: {message}"),
            ExecError::Internal { message } => write!(f, "internal error: {message}"),
            ExecError::Integrity { message } => write!(f, "data integrity error: {message}"),
            ExecError::Unsupported { message } => write!(f, "unsupported: {message}"),
            ExecError::ReadOnly { message } => write!(f, "read-only: {message}"),
        }
    }
}

impl std::error::Error for ExecError {}

impl ExecError {
    pub fn context(self, context: &str) -> Self {
        let prefixed = |message: String| format!("{context}: {message}");
        match self {
            ExecError::Abort { message, span } => ExecError::Abort {
                message: prefixed(message),
                span,
            },
            ExecError::Storage { message } => ExecError::Storage {
                message: prefixed(message),
            },
            ExecError::Internal { message } => ExecError::Internal {
                message: prefixed(message),
            },
            ExecError::Integrity { message } => ExecError::Integrity {
                message: prefixed(message),
            },
            ExecError::Unsupported { message } => ExecError::Unsupported {
                message: prefixed(message),
            },
            ExecError::ReadOnly { message } => ExecError::ReadOnly {
                message: prefixed(message),
            },
        }
    }
}
