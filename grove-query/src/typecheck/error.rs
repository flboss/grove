use crate::error::error_simple;
use grove_types::{Diagnostic, Span};

#[derive(Debug, Clone)]
pub enum TypeError {
    UnknownIdentifier { name: String, span: Span },
    AmbiguousType { span: Span },
}

impl From<TypeError> for Diagnostic {
    fn from(err: TypeError) -> Self {
        match err {
            TypeError::UnknownIdentifier { name, span } => error_simple(
                "QT0001",
                format!("unknown identifier `{name}`"),
                span,
                "not found in scope",
            ),
            TypeError::AmbiguousType { span } => error_simple(
                "QT0002",
                "ambiguous type: cannot infer type without context",
                span,
                "type is ambiguous",
            ),
        }
    }
}
