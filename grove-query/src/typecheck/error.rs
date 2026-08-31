use crate::error::error_simple;
use grove_types::{Diagnostic, Span};

#[rustfmt::skip]
#[derive(Debug, Clone)]
pub enum TypeError {
    UnknownIdentifier { name: String, span: Span },
    UnknownField { field: String, base_ty: String, span: Span },
    UnknownMethod { method: String, base_ty: String, span: Span },
    WrongArgCount { method: String, expected: usize, got: usize, span: Span },
    ArgTypeMismatch { method: String, expected: String, got: String, span: Span },
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
            TypeError::UnknownField {
                field,
                base_ty,
                span,
            } => error_simple(
                "QT0002",
                format!("unknown field `{field}` on `{base_ty}`"),
                span,
                "no such field",
            ),
            TypeError::UnknownMethod {
                method,
                base_ty,
                span,
            } => error_simple(
                "QT0003",
                format!("unknown method `{method}` on `{base_ty}`"),
                span,
                "no such method",
            ),
            TypeError::WrongArgCount {
                method,
                expected,
                got,
                span,
            } => error_simple(
                "QT0004",
                format!("`{method}` expects {expected} argument(s), got {got}"),
                span,
                "wrong number of arguments",
            ),
            TypeError::ArgTypeMismatch {
                method,
                expected,
                got,
                span,
            } => error_simple(
                "QT0005",
                format!("`{method}` argument type mismatch: expected {expected}, got {got}"),
                span,
                "type mismatch",
            ),
            TypeError::AmbiguousType { span } => error_simple(
                "QT0006",
                "ambiguous type: cannot infer type without context",
                span,
                "type is ambiguous",
            ),
        }
    }
}
