use crate::error::error_simple;
use grove_types::{Diagnostic, Span};

#[rustfmt::skip]
#[derive(Debug, Clone)]
pub enum TypeError {
    UnknownIdentifier { name: String, span: Span },
    UnknownField { field: String, base_ty: String, span: Span },
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
            TypeError::AmbiguousType { span } => error_simple(
                "QT0003",
                "ambiguous type: cannot infer type without context",
                span,
                "type is ambiguous",
            ),
        }
    }
}
