use std::fmt;

use grove_types::Span;

#[derive(Debug, Clone, PartialEq)]
pub enum ExecError {
    /// A valid query failing at runtime on data
    Abort { message: String, span: Span },
    /// Upstream correctness issue: data violating its schema, or a broken
    /// pipeline invariant
    Integrity { message: String },
    /// Backend or environment failure (SQLite errors, DB/shape mismatch).
    Storage { message: String },
}

impl fmt::Display for ExecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExecError::Abort { message, .. } => write!(f, "query aborted: {message}"),
            ExecError::Integrity { message } => write!(f, "data integrity error: {message}"),
            ExecError::Storage { message } => write!(f, "storage error: {message}"),
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
            ExecError::Integrity { message } => ExecError::Integrity {
                message: prefixed(message),
            },
            ExecError::Storage { message } => ExecError::Storage {
                message: prefixed(message),
            },
        }
    }
}
