use crate::error::error_simple;
use grove_types::{Diagnostic, Label, LabelStyle, Span};

#[rustfmt::skip]
#[derive(Debug, Clone)]
pub enum TypeError {
    UnknownIdentifier { name: String, span: Span },
    UnknownField { field: String, base_ty: String, span: Span },
    UnknownMethod { method: String, base_ty: String, span: Span },
    WrongArgCount { method: String, expected: usize, got: usize, span: Span },
    ArgTypeMismatch { method: String, expected: String, got: String, span: Span },
    AmbiguousType { span: Span },
    BinaryOpTypeMismatch { left: String, op: String, right: String, span: Span },
    UnaryOpTypeMismatch { op: String, operand: String, span: Span },
    InRequiresTuple { span: Span },
    IfConditionNotBool { span: Span },
    IfBranchTypeMismatch { expected: String, got: String, span: Span },
    InvalidCast { from: String, to: String, span: Span },
    ArrayElementTypeMismatch { expected: String, got: String, span: Span },
    DuplicateProjectionField { name: String, span: Span, previous: Span},
    UnnamedComputedField { span: Span },
    ProjectionBaseNotRecord { found: String, span: Span },
    EmptyProjection { span: Span },
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
            TypeError::BinaryOpTypeMismatch {
                left,
                op,
                right,
                span,
            } => error_simple(
                "QT0007",
                format!("invalid binary operation: cannot apply `{op}` to `{left}` and `{right}`"),
                span,
                "type mismatch",
            ),
            TypeError::UnaryOpTypeMismatch { op, operand, span } => error_simple(
                "QT0008",
                format!("invalid unary operation: cannot apply `{op}` to `{operand}`"),
                span,
                "type mismatch",
            ),
            TypeError::InRequiresTuple { span } => error_simple(
                "QT0009",
                "`in` requires a tuple on the right-hand side",
                span,
                "expected tuple",
            ),
            TypeError::IfConditionNotBool { span } => error_simple(
                "QT0011",
                "`if` condition must be `Bool`",
                span,
                "expected Bool",
            ),
            TypeError::IfBranchTypeMismatch {
                expected,
                got,
                span,
            } => error_simple(
                "QT0012",
                format!("if branch type mismatch: expected `{expected}`, got `{got}`"),
                span,
                "type mismatch",
            ),
            TypeError::InvalidCast { from, to, span } => error_simple(
                "QT0013",
                format!("invalid cast: `{from}` cannot be cast to `{to}`"),
                span,
                "invalid cast",
            ),
            TypeError::ArrayElementTypeMismatch {
                expected,
                got,
                span,
            } => error_simple(
                "QT0014",
                format!("array element type mismatch: expected `{expected}`, got `{got}`"),
                span,
                "type mismatch",
            ),
            TypeError::DuplicateProjectionField {
                name,
                span,
                previous,
            } => error_simple(
                "QT0015",
                format!("duplicate projection field name `{name}`"),
                span,
                "duplicate field",
            )
            .with_label(Label {
                span: previous,
                message: "first defined here".into(),
                style: LabelStyle::Secondary,
            }),
            TypeError::UnnamedComputedField { span } => error_simple(
                "QT0016",
                "computed projection field requires an explicit alias",
                span,
                "missing alias",
            ),
            TypeError::ProjectionBaseNotRecord { found, span } => error_simple(
                "QT0017",
                format!("projection requires a Record or List<Record>, got `{found}`"),
                span,
                "expected record",
            ),
            TypeError::EmptyProjection { span } => error_simple(
                "QT0018",
                "empty projection `{}` is not allowed",
                span,
                "empty projection",
            )
            .with_help("omit the projection to select all fields"),
        }
    }
}
