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
    MutationOnOptional { got: String, span: Span },
    MutationTargetNotTraceable { got: String, span: Span },
    InsertOnRecord { got: String, span: Span },
    InsertTargetNotCollection { got: String, span: Span },
    InsertOnNonOwningCollection { field: String, struct_name: String, span: Span },
    RowRefNotAllowed { span: Span },
    RowRefSourceNotTraceable { got: String, span: Span },
    RowRefOperationNotAllowed { operation: String, span: Span },
    UpdateOwningBackref { field: String, struct_name: String, span: Span },
    InsertOwningBackref { field: String, struct_name: String, span: Span },
    RowRefTargetMismatch { field: String, struct_name: String, expected: String, got: String, span: Span },
    RowRefRequired { field: String, struct_name: String, expected: String, got: String, span: Span },
    StructFieldNotInSchema { field: String, struct_name: String, span: Span },
    InsertMissingRequiredField { field: String, struct_name: String, span: Span },
    StructFieldTypeMismatch { field: String, struct_name: String, expected: String, got: String, span: Span },
    DuplicateStructField { name: String, span: Span },
    UnexpectedType { expected: String, got: String, span: Span },
    TupleAccessOnNonTuple { base_ty: String, span: Span },
    TupleIndexOutOfRange { index: usize, len: usize, span: Span },
    UnknownEnum { name: String, span: Span },
    UnknownEnumVariant { enum_name: String, variant: String, span: Span },
    OptionalDepthMismatch { depth: usize, ty: String, span: Span },
    UnknownTypeName { name: String, span: Span },
    AmbiguousCast { from: String, to: String, span: Span },
}

impl From<TypeError> for Diagnostic {
    fn from(err: TypeError) -> Self {
        match err {
            TypeError::UnknownIdentifier { name, span } => Diagnostic::error(
                "QT0001",
                format!("unknown identifier `{name}`"),
                span,
                "not found in scope",
            ),
            TypeError::UnknownField {
                field,
                base_ty,
                span,
            } => Diagnostic::error(
                "QT0002",
                format!("unknown field `{field}` on `{base_ty}`"),
                span,
                "no such field",
            ),
            TypeError::UnknownMethod {
                method,
                base_ty,
                span,
            } => Diagnostic::error(
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
            } => Diagnostic::error(
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
            } => Diagnostic::error(
                "QT0005",
                format!("`{method}` argument type mismatch: expected {expected}, got {got}"),
                span,
                "type mismatch",
            ),
            TypeError::AmbiguousType { span } => Diagnostic::error(
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
            } => Diagnostic::error(
                "QT0007",
                format!("invalid binary operation: cannot apply `{op}` to `{left}` and `{right}`"),
                span,
                "type mismatch",
            ),
            TypeError::UnaryOpTypeMismatch { op, operand, span } => Diagnostic::error(
                "QT0008",
                format!("invalid unary operation: cannot apply `{op}` to `{operand}`"),
                span,
                "type mismatch",
            ),
            TypeError::InRequiresTuple { span } => Diagnostic::error(
                "QT0009",
                "`in` requires a tuple on the right-hand side",
                span,
                "expected tuple",
            ),
            TypeError::IfConditionNotBool { span } => Diagnostic::error(
                "QT0011",
                "`if` condition must be `Bool`",
                span,
                "expected Bool",
            ),
            TypeError::IfBranchTypeMismatch {
                expected,
                got,
                span,
            } => Diagnostic::error(
                "QT0012",
                format!("if branch type mismatch: expected `{expected}`, got `{got}`"),
                span,
                "type mismatch",
            ),
            TypeError::InvalidCast { from, to, span } => Diagnostic::error(
                "QT0013",
                format!("invalid cast: `{from}` cannot be cast to `{to}`"),
                span,
                "invalid cast",
            ),
            TypeError::ArrayElementTypeMismatch {
                expected,
                got,
                span,
            } => Diagnostic::error(
                "QT0014",
                format!("array element type mismatch: expected `{expected}`, got `{got}`"),
                span,
                "type mismatch",
            ),
            TypeError::DuplicateProjectionField {
                name,
                span,
                previous,
            } => Diagnostic::error(
                "QT0015",
                format!("duplicate projection field name `{name}`"),
                span,
                "duplicate field",
            )
            .with_secondary_label(previous, "first defined here"),
            TypeError::UnnamedComputedField { span } => Diagnostic::error(
                "QT0016",
                "computed projection field requires an explicit alias",
                span,
                "missing alias",
            ),
            TypeError::ProjectionBaseNotRecord { got, span } => Diagnostic::error(
                "QT0017",
                format!("projection requires a Record or List<Record>, got `{got}`"),
                span,
                "expected record",
            ),
            TypeError::EmptyProjection { span } => Diagnostic::error(
                "QT0018",
                "empty projection `{}` is not allowed",
                span,
                "empty projection",
            )
            .with_help("omit the projection to keep all fields"),
            TypeError::MutationOnProjection { span } => Diagnostic::error(
                "QT0019",
                "cannot mutate a projection",
                span,
                "mutation on projection",
            )
            .with_note("only materialized collections can be mutated"),
            TypeError::MutationOnNonList { got, span } => Diagnostic::error(
                "QT0020",
                format!("mutation base type incompatible, got `{got}`"),
                span,
                "expected a record or list of records",
            ),
            TypeError::MutationOnOptional { got, span } => Diagnostic::error(
                "QT0035",
                format!("cannot mutate optional target `{got}` without optional chaining"),
                span,
                "optional mutation target",
            )
            .with_help("use `?.` on the mutation call or unwrap the target first"),
            TypeError::MutationTargetNotTraceable { got, span } => Diagnostic::error(
                "QT0036",
                format!("mutation target `{got}` cannot be traced to storage data"),
                span,
                "untraceable mutation target",
            )
            .with_note(
                "targets must come directly from storage, \
                not from `let`, partly or fully constructed values",
            ),
            TypeError::InsertOnRecord { got, span } => Diagnostic::error(
                "QT0037",
                format!("insert target must be a list of records, got `{got}`"),
                span,
                "expected list of records",
            ),
            TypeError::InsertTargetNotCollection { got, span } => Diagnostic::error(
                "QT0038",
                format!("insert target `{got}` does not identify a single mutable collection"),
                span,
                "ambiguous insert collection",
            )
            .with_note("`flatten`, `extend`, and `map` do not preserve a single insert collection"),
            TypeError::InsertOnNonOwningCollection {
                field,
                struct_name,
                span,
            } => Diagnostic::error(
                "QT0039",
                format!("cannot insert through non-owning collection `{struct_name}.{field}`"),
                span,
                "non-owning insert target",
            )
            .with_note("inserting non-owning references is not implemented"),
            TypeError::RowRefNotAllowed { span } => Diagnostic::error(
                "QT0040",
                "row references are only allowed inside mutation arguments",
                span,
                "row reference not allowed here",
            ),
            TypeError::RowRefSourceNotTraceable { got, span } => Diagnostic::error(
                "QT0041",
                format!("row reference source `{got}` cannot be traced to storage data"),
                span,
                "untraceable row reference source",
            )
            .with_note(
                "row references must point at existing record in storage, \
                not at `let`, partly or fully constructed values",
            ),
            TypeError::RowRefOperationNotAllowed { operation, span } => Diagnostic::error(
                "QT0042",
                format!("operation `{operation}` is not allowed on row references"),
                span,
                "row references are opaque",
            )
            .with_help("only shape-preserving list and optional operations are supported"),
            TypeError::UpdateOwningBackref {
                field,
                struct_name,
                span,
            } => Diagnostic::error(
                "QT0043",
                format!(
                    "cannot mutate non-owning field `{struct_name}.{field}` on an owning relation"
                ),
                span,
                "non-owning field of an owning relation",
            )
            .with_note("the owning side manages this back-reference"),
            TypeError::InsertOwningBackref {
                field,
                struct_name,
                span,
            } => Diagnostic::error(
                "QT0044",
                format!(
                    "cannot supply non-owning back-reference `{struct_name}.{field}` in an insert"
                ),
                span,
                "non-owning field of an owning relation",
            )
            .with_note("the parent assigns this back-reference on insertion"),
            TypeError::RowRefTargetMismatch {
                field,
                struct_name,
                expected,
                got,
                span,
            } => Diagnostic::error(
                "QT0045",
                format!(
                    "row reference for field `{field}` in struct `{struct_name}` \
                    targets the wrong struct"
                ),
                span,
                format!("expected {expected}, got {got}"),
            ),
            TypeError::RowRefRequired {
                field,
                struct_name,
                expected,
                got,
                span,
            } => Diagnostic::error(
                "QT0046",
                format!("field `{field}` in struct `{struct_name}` requires a row reference"),
                span,
                format!("expected {expected}, got {got}"),
            ),
            TypeError::StructFieldNotInSchema {
                field,
                struct_name,
                span,
            } => Diagnostic::error(
                "QT0022",
                format!("field `{field}` does not exist in struct `{struct_name}`"),
                span,
                "unknown field",
            ),
            TypeError::InsertMissingRequiredField {
                field,
                struct_name,
                span,
            } => Diagnostic::error(
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
            } => Diagnostic::error(
                "QT0024",
                format!("type mismatch for field `{field}` in struct `{struct_name}`"),
                span,
                format!("expected `{expected}`, got `{got}`"),
            ),
            TypeError::DuplicateStructField { name, span } => Diagnostic::error(
                "QT0025",
                format!("duplicate field name `{name}` in struct literal"),
                span,
                "duplicate field",
            ),
            TypeError::UnexpectedType {
                expected,
                got,
                span,
            } => Diagnostic::error(
                "QT0027",
                format!("type mismatch: expected {expected}, found {got}"),
                span,
                format!("expected {expected}"),
            ),
            TypeError::TupleAccessOnNonTuple { base_ty, span } => Diagnostic::error(
                "QT0028",
                format!("tuple index access on `{base_ty}`"),
                span,
                "only tuples support `.N` access",
            ),
            TypeError::TupleIndexOutOfRange { index, len, span } => Diagnostic::error(
                "QT0029",
                format!("tuple index `.{index}` out of range for tuple of length {len}"),
                span,
                format!("valid indices are .0 through .{}", len - 1),
            ),
            TypeError::UnknownEnum { name, span } => Diagnostic::error(
                "QT0030",
                format!("unknown enum `{name}`"),
                span,
                "no such enum",
            ),
            TypeError::UnknownEnumVariant {
                enum_name,
                variant,
                span,
            } => Diagnostic::error(
                "QT0031",
                format!("enum `{enum_name}` has no variant `{variant}`"),
                span,
                "no such variant",
            ),
            TypeError::OptionalDepthMismatch { depth, ty, span } => Diagnostic::error(
                "QT0032",
                format!("optional chaining depth {depth} exceeds the optional layers in `{ty}`"),
                span,
                format!("needs {depth} optional layer(s)"),
            ),
            TypeError::UnknownTypeName { name, span } => Diagnostic::error(
                "QT0033",
                format!("unknown type `{name}`"),
                span,
                "no such type",
            ),
            TypeError::AmbiguousCast { from, to, span } => Diagnostic::error(
                "QT0034",
                format!("cannot disambiguate `{from}` as `{to}`"),
                span,
                "ambiguous cast",
            )
            .with_help(
                "the target type cannot be reconciled with information \
                    already known about the value's type",
            ),
        }
    }
}
