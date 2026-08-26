use grove_types::{DiagStr, Diagnostic, Label, LabelStyle, Severity, Span};

#[rustfmt::skip] // preserve one-line-per-variant formatting
#[derive(Debug, Clone)]
pub enum QueryLexError {
    UnexpectedChar { span: Span, ch: char },
    UnclosedString { span: Span },
    UnclosedBacktickIdent { span: Span },
    UnclosedBlockComment { span: Span },

    HexEscapeInvalid { span: Span },
    HexEscapeNonAscii { span: Span, value: u8 },
    UnknownEscape { span: Span, ch: char },
    UnicodeEscapeExpectedLBrace { span: Span },
    UnicodeEscapeEmpty { span: Span },
    UnicodeEscapeInvalidDigit { span: Span, ch: char },
    UnicodeEscapeTooManyDigits { span: Span },
    UnicodeEscapeInvalidScalarValue { span: Span, value: u32 },

    UnexpectedAmpersand { span: Span },
    UnexpectedPipe { span: Span },
    UnexpectedQuestion { span: Span },
    UnexpectedColon { span: Span },

    IncompleteExponent { span: Span },
    IntLiteralOverflow { span: Span },
    DecLiteralOverflow { span: Span },
    UnexpectedDotInNumber { span: Span },
    DecimalRounded { span: Span },

    InstantExpected { span: Span },
    InstantNameInvalid { span: Span, name: String },
    InstantYearInvalid { span: Span },
    InstantMonthInvalid { span: Span },
    InstantDayInvalid { span: Span },
    InstantHourInvalid { span: Span },
    InstantMinuteInvalid { span: Span },
    InstantSecondInvalid { span: Span },
    InstantTimeColonExpected { span: Span },
    InstantUnixSecondsMissing { span: Span },
    InstantUnixOverflow { span: Span },
    InstantSuffixInvalid { span: Span },
    FractionRounded { span: Span },

    DurationExpected { span: Span },
    DurationUnitMissing { span: Span },
    DurationUnitDuplicate { span: Span, unit: char },
    DurationFractionOnNonSecond { span: Span },
    DurationOverflow { span: Span },
}

impl From<QueryLexError> for Diagnostic {
    fn from(err: QueryLexError) -> Self {
        use QueryLexError::*;

        match err {
            UnexpectedChar { span, ch } => error_simple(
                "QL0001",
                format!("unexpected character `{ch}`"),
                span,
                "unexpected character",
            ),
            UnclosedString { span } => error_simple(
                "QL0002",
                "unclosed string literal",
                span,
                "missing closing `\"`",
            ),
            UnclosedBacktickIdent { span } => error_simple(
                "QL0003",
                "unclosed backtick identifier",
                span,
                "missing closing '`'",
            ),
            UnclosedBlockComment { span } => error_simple(
                "QL0004",
                "unclosed block comment",
                span,
                "missing closing `*/`",
            ),
            HexEscapeInvalid { span } => error_simple(
                "QL0005",
                "invalid `\\x` escape: expected two hexadecimal digits",
                span,
                "expected two hex digits",
            ),
            HexEscapeNonAscii { span, value } => error_simple(
                "QL0006",
                format!("invalid ASCII character `\\x{value:02X}`"),
                span,
                "non-ASCII hex escape",
            )
            .with_help("use `\\u{...}` for non-ASCII characters"),
            UnknownEscape { span, ch } => error_simple(
                "QL0007",
                format!("unknown escape sequence `\\{ch}`"),
                span,
                "unknown escape",
            ),
            UnicodeEscapeExpectedLBrace { span } => error_simple(
                "QL0008",
                "expected `{` after `\\u` escape sequence",
                span,
                "expected `{`",
            )
            .with_help("unicode escape sequences require brace-wrapped content: `\\u{...}`"),
            UnicodeEscapeEmpty { span } => error_simple(
                "QL0009",
                "empty `\\u{...}` escape",
                span,
                "expected a hex digit",
            ),
            UnicodeEscapeInvalidDigit { span, ch } => error_simple(
                "QL0010",
                format!("invalid character `{ch}` in `\\u{{...}}` escape"),
                span,
                "expected hex digit",
            ),
            UnicodeEscapeTooManyDigits { span } => error_simple(
                "QL0011",
                "`\\u{{...}}` escape has more than six hex digits",
                span,
                "too many digits",
            ),
            UnicodeEscapeInvalidScalarValue { span, value } => error_simple(
                "QL0012",
                format!("`\\u{{{value:X}}}` is not a valid Unicode scalar value"),
                span,
                "invalid scalar value",
            ),
            UnexpectedAmpersand { span } => {
                error_simple("QL0013", "unexpected `&`", span, "unexpected `&`")
                    .with_help("use `&&` for logical AND")
            }
            UnexpectedPipe { span } => {
                error_simple("QL0014", "unexpected `|`", span, "unexpected `|`")
                    .with_help("use `||` for logical OR")
            }
            UnexpectedQuestion { span } => {
                error_simple("QL0015", "unexpected `?`", span, "unexpected `?`")
                    .with_help("use `?.` for optional chaining")
            }
            UnexpectedColon { span } => {
                error_simple("QL0016", "unexpected `:`", span, "unexpected `:`")
                    .with_help("use `::` for type paths")
            }
            IncompleteExponent { span } => error_simple(
                "QL0017",
                "incomplete exponent in numeric literal",
                span,
                "expected digits after `e`/`E`",
            ),
            IntLiteralOverflow { span } => error_simple(
                "QL0018",
                "integer literal is too large",
                span,
                "out of range",
            )
            .with_help(
                "the valid range `Int` values is -9223372036854775808..=9223372036854775807",
            ),
            DecLiteralOverflow { span } => error_simple(
                "QL0019",
                "decimal literal is out of range",
                span,
                "out of range",
            )
            .with_help("`Dec` supports up to 28 significant digits"),
            UnexpectedDotInNumber { span } => error_simple(
                "QL0020",
                "unexpected `.` in numeric literal",
                span,
                "unexpected `.`",
            ),
            DecimalRounded { span } => warning_simple(
                "QL0021",
                "decimal literal has more than 28 significant digits",
                span,
                "precision lost (rounded)",
            ),
            InstantExpected { span } => error_simple(
                "QL0022",
                "expected an instant literal after `@`",
                span,
                "expected `now`, `today`, `unix`, or a date",
            ),
            InstantNameInvalid { span, name } => error_simple(
                "QL0023",
                format!("invalid instant name `{name}`"),
                span,
                "invalid name",
            )
            .with_help("valid names are `now`, `today`, and `unix`"),
            InstantYearInvalid { span } => error_simple(
                "QL0024",
                "invalid year in instant literal",
                span,
                "expected at least 4 digits",
            ),
            InstantMonthInvalid { span } => error_simple(
                "QL0025",
                "invalid month in instant literal",
                span,
                "expected 1-12",
            ),
            InstantDayInvalid { span } => error_simple(
                "QL0026",
                "invalid day in instant literal",
                span,
                "invalid day",
            ),
            InstantHourInvalid { span } => error_simple(
                "QL0027",
                "invalid hour in instant literal",
                span,
                "expected 0-23",
            ),
            InstantMinuteInvalid { span } => error_simple(
                "QL0028",
                "invalid minute in instant literal",
                span,
                "expected 0-59",
            ),
            InstantSecondInvalid { span } => error_simple(
                "QL0029",
                "invalid second in instant literal",
                span,
                "expected 0-59",
            ),
            InstantTimeColonExpected { span } => error_simple(
                "QL0030",
                "expected `:` between hour and minute",
                span,
                "expected `:`",
            ),
            InstantUnixSecondsMissing { span } => error_simple(
                "QL0031",
                "expected seconds after `@unix_`",
                span,
                "expected digits",
            ),
            InstantUnixOverflow { span } => error_simple(
                "QL0032",
                "`@unix_` seconds are out of range",
                span,
                "out of range",
            ),
            InstantSuffixInvalid { span } => error_simple(
                "QL0033",
                "unexpected characters after instant literal",
                span,
                "unexpected suffix",
            ),
            FractionRounded { span } => warning_simple(
                "QL0034",
                "fractional seconds exceed nanosecond precision",
                span,
                "precision lost (rounded)",
            ),
            DurationExpected { span } => error_simple(
                "QL0035",
                "expected a duration literal after `#`",
                span,
                "expected `<digits><unit>`",
            ),
            DurationUnitMissing { span } => error_simple(
                "QL0036",
                "expected a duration unit after the number",
                span,
                "expected duration unit",
            )
            .with_help("valid units are: `y`, `w`, `d`, `h`, `m`, and `s`"),
            DurationUnitDuplicate { span, unit } => error_simple(
                "QL0037",
                format!("duration unit `{unit}` appears more than once"),
                span,
                "duplicate unit",
            ),
            DurationFractionOnNonSecond { span } => error_simple(
                "QL0038",
                "fractional not supported by duration component",
                span,
                "fractional unit",
            )
            .with_note("only the seconds component may be fractional"),
            DurationOverflow { span } => error_simple(
                "QL0039",
                "duration literal is out of range",
                span,
                "out of range",
            ),
        }
    }
}

pub fn error_simple(
    code: &'static str,
    message: impl Into<DiagStr>,
    span: Span,
    label: impl Into<DiagStr>,
) -> Diagnostic {
    Diagnostic {
        severity: Severity::Error,
        code: code.into(),
        message: message.into(),
        labels: vec![Label {
            span,
            message: label.into(),
            style: LabelStyle::Primary,
        }],
        notes: Vec::new(),
        help: Vec::new(),
    }
}

pub fn warning_simple(
    code: &'static str,
    message: impl Into<DiagStr>,
    span: Span,
    label: impl Into<DiagStr>,
) -> Diagnostic {
    Diagnostic {
        severity: Severity::Warning,
        code: code.into(),
        message: message.into(),
        labels: vec![Label {
            span,
            message: label.into(),
            style: LabelStyle::Primary,
        }],
        notes: Vec::new(),
        help: Vec::new(),
    }
}

#[rustfmt::skip] // preserve one-line-per-variant formatting
#[derive(Debug, Clone)]
pub enum QueryParseError {
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
    ExpectedCastTypeName { span: Span },
    ProjectionItemAliasRequired { span: Span },
}

impl From<QueryParseError> for Diagnostic {
    fn from(err: QueryParseError) -> Self {
        use QueryParseError::*;

        match err {
            ExpectedExpr { span } => error_simple(
                "QP0001",
                "expected an expression",
                span,
                "expected an expression",
            ),
            TrailingInput { span } => error_simple(
                "QP0002",
                "unexpected input after the result expression",
                span,
                "unexpected input",
            ),
            TupleCommaOrRParenExpected { span } => {
                error_simple("QP0003", "unclosed tuple", span, "expected `,` or `)`")
            }
            ArrayCommaOrRBracketExpected { span } => {
                error_simple("QP0005", "unclosed array", span, "expected `,` or `]`")
            }
            SomeLParenExpected { span } => {
                error_simple("QP0006", "expected `(` after `some`", span, "expected `(`")
            }
            SomeRParenExpected { span } => {
                error_simple("QP0007", "unclosed `some(...)`", span, "expected `)`")
            }
            TypeConstColonExpected { span } => {
                error_simple("QP0008", "incomplete type constant", span, "expected `::`")
                    .with_note("numeric types support the constants `MIN` and `MAX`")
            }
            TypeConstNameExpected { span } => error_simple(
                "QP0009",
                "expected a valid type constant",
                span,
                "expected an identifier",
            )
            .with_help("numeric types support the constants `MIN` and `MAX`"),
            IfLBraceExpected { span } => error_simple(
                "QP0010",
                "expected `{` after `if` condition",
                span,
                "expected `{`",
            ),
            IfRBraceExpected { span } => {
                error_simple("QP0011", "unclosed `if` block", span, "expected `}`")
            }
            MissingElse { span } => error_simple(
                "QP0012",
                "`if` expression requires an `else` branch",
                span,
                "missing `else`",
            ),
            ElseLBraceExpected { span } => {
                error_simple("QP0013", "expected `{` after `else`", span, "expected `{`")
            }
            StructFieldNameExpected { span } => error_simple(
                "QP0014",
                "expected a field name in struct literal",
                span,
                "expected an identifier",
            ),
            StructEqualsExpected { span, field } => error_simple(
                "QP0015",
                format!("expected `=` after struct field `{field}`"),
                span,
                "expected `=`",
            ),
            StructCommaOrRBraceExpected { span } => error_simple(
                "QP0016",
                "unclosed struct literal",
                span,
                "expected `,` or `}`",
            ),
            ExpectedIdentAfterDot { span } => error_simple(
                "QP0017",
                "expected an identifier after `.` or `?.`",
                span,
                "expected a method or field name",
            ),
            UnclosedMethodArgs { span } => {
                error_simple("QP0018", "unclosed method arguments", span, "expected `)`")
            }
            ExpectedFilterRBracket { span } => {
                error_simple("QP0019", "unclosed filter", span, "expected `]`")
            }
            ExpectedProjectionRBrace { span } => {
                error_simple("QP0020", "unclosed projection", span, "expected `}`")
            }
            ExpectedCastTypeName { span } => error_simple(
                "QP0021",
                "expected a type name after `as`",
                span,
                "expected a type name",
            )
            .with_help("valid cast targets are `Int`, `Float`, and `Dec`"),
            ProjectionItemAliasRequired { span } => error_simple(
                "QP0022",
                "expected explicit alias for complex projection item",
                span,
                "expected a plain field path or an explicit alias",
            )
            .with_help("add an alias to name the projection item: `alias = ...`"),
        }
    }
}
