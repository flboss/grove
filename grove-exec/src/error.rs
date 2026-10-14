use std::fmt;

use grove_types::Span;

#[derive(Debug, Clone, PartialEq)]
pub enum ExecError {
    Abort { message: String, span: Span },
    Integrity { message: String },
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
