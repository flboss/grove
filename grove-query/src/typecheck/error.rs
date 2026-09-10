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
    ProjectionBaseNotRecord { got: String, span: Span },
    EmptyProjection { span: Span },
    MutationOnProjection { span: Span },
    MutationOnNonList { got: String, span: Span },
    InsertOnNonRoot { got: String, span: Span },
    StructFieldNotInSchema { field: String, struct_name: String, span: Span },
    InsertMissingRequiredField { field: String, struct_name: String, span: Span },
    StructFieldTypeMismatch { field: String, struct_name: String, expected: String, got: String, span: Span },
    DuplicateStructField { name: String, span: Span },
    RefFieldInMutation { field: String, struct_name: String, span: Span },
    UnexpectedType { expected: String, got: String, span: Span },
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
            TypeError::ProjectionBaseNotRecord { got, span } => error_simple(
                "QT0017",
                format!("projection requires a Record or List<Record>, got `{got}`"),
                span,
                "expected record",
            ),
            TypeError::EmptyProjection { span } => error_simple(
                "QT0018",
                "empty projection `{}` is not allowed",
                span,
                "empty projection",
            )
            .with_help("omit the projection to keep all fields"),
            TypeError::MutationOnProjection { span } => error_simple(
                "QT0019",
                "cannot mutate a projection",
                span,
                "mutation on projection",
            )
            .with_note("only materialized collections can be mutated"),
            TypeError::MutationOnNonList { got, span } => error_simple(
                "QT0020",
                format!("mutation base type incompatible, got `{got}`"),
                span,
                "expected list of records",
            ),
            TypeError::InsertOnNonRoot { got, span } => error_simple(
                "QT0021",
                format!("insert base must be a root collection, got `{got}`"),
                span,
                "expected root collection",
            ),
            TypeError::StructFieldNotInSchema {
                field,
                struct_name,
                span,
            } => error_simple(
                "QT0022",
                format!("field `{field}` does not exist in struct `{struct_name}`"),
                span,
                "unknown field",
            ),
            TypeError::InsertMissingRequiredField {
                field,
                struct_name,
                span,
            } => error_simple(
                "QT0023",
                format!("missing required field `{field}` in struct `{struct_name}`"),
                span,
                "missing required field",
            ),
            TypeError::StructFieldTypeMismatch {
                field,
                struct_name,
                expected,
                got,
                span,
            } => error_simple(
                "QT0024",
                format!("type mismatch for field `{field}` in struct `{struct_name}`"),
                span,
                format!("expected `{expected}`, got `{got}`"),
            ),
            TypeError::DuplicateStructField { name, span } => error_simple(
                "QT0025",
                format!("duplicate field name `{name}` in struct literal"),
                span,
                "duplicate field",
            ),
            TypeError::RefFieldInMutation {
                field,
                struct_name,
                span,
            } => error_simple(
                "QT0026",
                format!("field `{field}` in struct `{struct_name}` not supported in mutation"),
                span,
                "ref field not allowed",
            )
            .with_note(format!(
                "`{field}` is a non-owning reference and cannot be provided in a mutation"
            )),
            TypeError::UnexpectedType {
                expected,
                got,
                span,
            } => error_simple(
                "QT0027",
                format!("type mismatch: expected {expected}, found {got}"),
                span,
                format!("expected {expected}"),
            ),
        }
    }
}
