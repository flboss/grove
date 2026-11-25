use grove_types::{Diagnostic, Span};

#[rustfmt::skip] // preserve one-line-per-variant formatting
#[derive(Debug, Clone)]
pub(crate) enum QueryParseError {
    ExpectedExpr { span: Span },
    TrailingInput { span: Span },

    TupleCommaOrRParenExpected { span: Span },
    ArrayCommaOrRBracketExpected { span: Span },

    SomeLParenExpected { span: Span },
    SomeRParenExpected { span: Span },

    TypeConstColonExpected { span: Span },
    TypeConstNameExpected { span: Span },

    IfLBraceExpected { span: Span },
    IfRBraceExpected { span: Span },
    MissingElse { span: Span },
    ElseLBraceExpected { span: Span },

    StructFieldNameExpected { span: Span },
    StructEqualsExpected { span: Span, field: String },
    StructCommaOrRBraceExpected { span: Span },

    ExpectedIdentAfterDot { span: Span },
    UnclosedMethodArgs { span: Span },
    ExpectedFilterRBracket { span: Span },
    ExpectedProjectionRBrace { span: Span },
    ExpectedType { span: Span },
    EmptyTupleType { span: Span },
    EmptyRecordType { span: Span },
    ExpectedTypeFieldName { span: Span },
    ExpectedTypeFieldColon { span: Span },
    ExpectedTypeFieldCommaOrRBrace { span: Span },
    ExpectedListTypeLAngle { span: Span },
    ExpectedListTypeRAngle { span: Span },
    DuplicateTypeField {
        span: Span,
        field: String,
        previous: Span,
    },
    ExpectedEnumVariantName { span: Span },
    ProjectionItemAliasRequired { span: Span },
    ProjectionSpreadNotLast { span: Span },
    EmptyStatement { span: Span },
    DiscardedQuery { span: Span },
    ExpectedSemicolon { span: Span, after: &'static str },
    LetEqualsExpected { span: Span },
    MutationAsValue { span: Span },
    LetNameExpected { span: Span },
}

impl From<QueryParseError> for Diagnostic {
    fn from(err: QueryParseError) -> Self {
        use QueryParseError::*;

        match err {
            ExpectedExpr { span } => Diagnostic::error(
                "QP0001",
                "expected an expression",
                span,
                "expected an expression",
            ),
            TrailingInput { span } => Diagnostic::error(
                "QP0002",
                "unexpected input after the result expression",
                span,
                "unexpected input",
            ),
            TupleCommaOrRParenExpected { span } => {
                Diagnostic::error("QP0003", "unclosed tuple", span, "expected `,` or `)`")
            }
            ArrayCommaOrRBracketExpected { span } => {
                Diagnostic::error("QP0005", "unclosed array", span, "expected `,` or `]`")
            }
            SomeLParenExpected { span } => {
                Diagnostic::error("QP0006", "expected `(` after `some`", span, "expected `(`")
            }
            SomeRParenExpected { span } => {
                Diagnostic::error("QP0007", "unclosed `some(...)`", span, "expected `)`")
            }
            TypeConstColonExpected { span } => {
                Diagnostic::error("QP0008", "incomplete type constant", span, "expected `::`")
                    .with_note("numeric types support the constants `MIN` and `MAX`")
            }
            TypeConstNameExpected { span } => Diagnostic::error(
                "QP0009",
                "expected a valid type constant",
                span,
                "expected an identifier",
            )
            .with_help("numeric types support the constants `MIN` and `MAX`"),
            IfLBraceExpected { span } => Diagnostic::error(
                "QP0010",
                "expected `{` after `if` condition",
                span,
                "expected `{`",
            ),
            IfRBraceExpected { span } => {
                Diagnostic::error("QP0011", "unclosed `if` block", span, "expected `}`")
            }
            MissingElse { span } => Diagnostic::error(
                "QP0012",
                "`if` expression requires an `else` branch",
                span,
                "missing `else`",
            ),
            ElseLBraceExpected { span } => {
                Diagnostic::error("QP0013", "expected `{` after `else`", span, "expected `{`")
            }
            StructFieldNameExpected { span } => Diagnostic::error(
                "QP0014",
                "expected a field name in struct literal",
                span,
                "expected an identifier",
            ),
            StructEqualsExpected { span, field } => Diagnostic::error(
                "QP0015",
                format!("expected `=` after struct field `{field}`"),
                span,
                "expected `=`",
            ),
            StructCommaOrRBraceExpected { span } => Diagnostic::error(
                "QP0016",
                "unclosed struct literal",
                span,
                "expected `,` or `}`",
            ),
            ExpectedIdentAfterDot { span } => Diagnostic::error(
                "QP0017",
                "expected an identifier after `.` or `?.`",
                span,
                "expected a method or field name",
            ),
            UnclosedMethodArgs { span } => {
                Diagnostic::error("QP0018", "unclosed method arguments", span, "expected `)`")
            }
            ExpectedFilterRBracket { span } => {
                Diagnostic::error("QP0019", "unclosed filter", span, "expected `]`")
            }
            ExpectedProjectionRBrace { span } => {
                Diagnostic::error("QP0020", "unclosed projection", span, "expected `}`")
            }
            ExpectedType { span } => {
                Diagnostic::error("QP0021", "expected a type", span, "expected a type")
            }
            EmptyTupleType { span } => Diagnostic::error(
                "QP0031",
                "expected one or more tuple elements",
                span,
                "empty tuple",
            ),
            EmptyRecordType { span } => Diagnostic::error(
                "QP0032",
                "expected one or more record fields",
                span,
                "empty record",
            ),
            ExpectedTypeFieldName { span } => Diagnostic::error(
                "QP0033",
                "expected a field name in record type",
                span,
                "expected an identifier",
            ),
            ExpectedTypeFieldColon { span } => Diagnostic::error(
                "QP0034",
                "expected `:` after record type field name",
                span,
                "expected `:`",
            ),
            ExpectedTypeFieldCommaOrRBrace { span } => Diagnostic::error(
                "QP0035",
                "unclosed record type",
                span,
                "expected `,` or `}`",
            ),
            ExpectedListTypeLAngle { span } => {
                Diagnostic::error("QP0036", "expected `<` after `List`", span, "expected `<`")
            }
            ExpectedListTypeRAngle { span } => {
                Diagnostic::error("QP0037", "unclosed `List<...>` type", span, "expected `>`")
            }
            DuplicateTypeField {
                span,
                field,
                previous,
            } => Diagnostic::error(
                "QP0038",
                format!("duplicate field `{field}` in record type"),
                span,
                "duplicate field",
            )
            .with_secondary_label(previous, "first declared here"),
            ExpectedEnumVariantName { span } => Diagnostic::error(
                "QP0030",
                "expected an enum variant name after `::`",
                span,
                "expected an identifier",
            ),
            ProjectionItemAliasRequired { span } => Diagnostic::error(
                "QP0022",
                "expected explicit alias for complex projection item",
                span,
                "expected a plain field path or an explicit alias",
            )
            .with_help("add an alias to name the projection item: `alias = ...`"),
            ProjectionSpreadNotLast { span } => Diagnostic::error(
                "QP0040",
                "projection spread must be the last item",
                span,
                "spread is not last",
            )
            .with_help("move `...` to the end of the projection"),
            EmptyStatement { span } => {
                Diagnostic::error("QP0023", "empty statement", span, "expected an expression")
            }
            DiscardedQuery { span } => Diagnostic::warning(
                "QP0024",
                "discarded query has no effect",
                span,
                "query result is discarded",
            )
            .with_help("move this expression to the end of the file to return its result"),
            ExpectedSemicolon { span, after } => Diagnostic::error(
                "QP0039",
                format!("expected `;` after {after}"),
                span,
                "expected `;`",
            ),
            LetEqualsExpected { span } => Diagnostic::error(
                "QP0026",
                "expected `=` after `let` binding name",
                span,
                "expected `=`",
            ),
            MutationAsValue { span } => Diagnostic::error(
                "QP0028",
                "mutation cannot be used as a `let` value",
                span,
                "produces no value",
            )
            .with_help("only query expressions may be bound using `let`"),
            LetNameExpected { span } => Diagnostic::error(
                "QP0029",
                "expected a name after `let`",
                span,
                "expected an identifier",
            ),
        }
    }
}
