use chrono::{Datelike, NaiveDate};
use grove_types::{Diagnostic, Span};

#[rustfmt::skip] // preserve one-line-per-variant formatting
#[derive(Debug, Clone)]
pub enum QueryLexError {
    UnexpectedChar { span: Span, ch: char },
    UnclosedString { span: Span },
    UnclosedBacktickIdent { span: Span },
    UnclosedBlockComment { span: Span },
    UnclosedBytesLiteral { span: Span },
    BytesHexInvalidDigit { span: Span, ch: char },
    BytesHexOddDigitCount { span: Span },
    BytesBase64InvalidChar { span: Span, ch: char },
    BytesBase64InvalidPadding { span: Span },
    BytesBase64NonZeroTrailingBits { span: Span },

    HexEscapeInvalid { span: Span },
    HexEscapeNonAscii { span: Span, value: u8 },
    UnknownEscape { span: Span, ch: char },
    UnicodeEscapeExpectedLBrace { span: Span },
    UnicodeEscapeEmpty { span: Span },
    UnicodeEscapeInvalidDigit { span: Span, ch: char },
    UnicodeEscapeTooManyDigits { span: Span },
    UnicodeEscapeInvalidScalarValue { span: Span, value: u32 },

    UnexpectedPipe { span: Span },

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
            UnexpectedChar { span, ch } => Diagnostic::error(
                "QL0001",
                format!("unexpected character `{ch}`"),
                span,
                "unexpected character",
            ),
            UnclosedString { span } => Diagnostic::error(
                "QL0002",
                "unclosed string literal",
                span,
                "missing closing `\"`",
            ),
            UnclosedBacktickIdent { span } => Diagnostic::error(
                "QL0003",
                "unclosed backtick identifier",
                span,
                "missing closing '`'",
            ),
            UnclosedBlockComment { span } => Diagnostic::error(
                "QL0004",
                "unclosed block comment",
                span,
                "missing closing `*/`",
            ),
            UnclosedBytesLiteral { span } => Diagnostic::error(
                "QL0040",
                "unclosed bytes literal",
                span,
                "missing closing `\"`",
            ),
            BytesHexInvalidDigit { span, ch } => Diagnostic::error(
                "QL0041",
                format!("invalid hex digit `{ch}` in bytes literal"),
                span,
                "expected hex digit",
            )
            .with_help(
                "valid hex digits are `0-9`, `a-f`, or `A-F`; \
                    spaces, tabs, and `_` are allowed as separators",
            ),
            BytesHexOddDigitCount { span } => Diagnostic::error(
                "QL0042",
                "hex bytes literal has an odd number of digits",
                span,
                "expected even number of hex digits",
            ),
            BytesBase64InvalidChar { span, ch } => Diagnostic::error(
                "QL0043",
                format!("invalid base64 character `{ch}` in bytes literal"),
                span,
                "expected standard base64 character",
            )
            .with_help("valid base64 digits are `A-Z`, `a-z`, `0-9`, `+`, and `/`"),
            BytesBase64InvalidPadding { span } => Diagnostic::error(
                "QL0044",
                "invalid base64 padding in bytes literal",
                span,
                "invalid padding",
            )
            .with_help(
                "padding must complete the final 3-byte group: \
                `==` for 2 digits, `=` for 3 digits, no padding for 4 digits",
            ),
            BytesBase64NonZeroTrailingBits { span } => Diagnostic::error(
                "QL0045",
                "base64 bytes literal has nonzero unused trailing bits",
                span,
                "non-canonical base64 encoding",
            ),
            HexEscapeInvalid { span } => Diagnostic::error(
                "QL0005",
                "invalid `\\x` escape: expected two hexadecimal digits",
                span,
                "expected two hex digits",
            ),
            HexEscapeNonAscii { span, value } => Diagnostic::error(
                "QL0006",
                format!("invalid ASCII character `\\x{value:02X}`"),
                span,
                "non-ASCII hex escape",
            )
            .with_help("use `\\u{...}` for non-ASCII characters"),
            UnknownEscape { span, ch } => Diagnostic::error(
                "QL0007",
                format!("unknown escape sequence `\\{ch}`"),
                span,
                "unknown escape",
            ),
            UnicodeEscapeExpectedLBrace { span } => Diagnostic::error(
                "QL0008",
                "expected `{` after `\\u` escape sequence",
                span,
                "expected `{`",
            )
            .with_help("unicode escape sequences require brace-wrapped content: `\\u{...}`"),
            UnicodeEscapeEmpty { span } => Diagnostic::error(
                "QL0009",
                "empty `\\u{...}` escape",
                span,
                "expected a hex digit",
            ),
            UnicodeEscapeInvalidDigit { span, ch } => Diagnostic::error(
                "QL0010",
                format!("invalid character `{ch}` in `\\u{{...}}` escape"),
                span,
                "expected hex digit",
            ),
            UnicodeEscapeTooManyDigits { span } => Diagnostic::error(
                "QL0011",
                "`\\u{{...}}` escape has more than six hex digits",
                span,
                "too many digits",
            ),
            UnicodeEscapeInvalidScalarValue { span, value } => Diagnostic::error(
                "QL0012",
                format!("`\\u{{{value:X}}}` is not a valid Unicode scalar value"),
                span,
                "invalid scalar value",
            ),
            UnexpectedPipe { span } => {
                Diagnostic::error("QL0014", "unexpected `|`", span, "unexpected `|`")
                    .with_help("use `||` for logical OR")
            }
            IncompleteExponent { span } => Diagnostic::error(
                "QL0017",
                "incomplete exponent in numeric literal",
                span,
                "expected digits after `e`/`E`",
            ),
            IntLiteralOverflow { span } => Diagnostic::error(
                "QL0018",
                "integer literal is too large",
                span,
                "out of range",
            )
            .with_help(
                "the valid range `Int` values is -9223372036854775808..=9223372036854775807",
            ),
            DecLiteralOverflow { span } => Diagnostic::error(
                "QL0019",
                "decimal literal is out of range",
                span,
                "out of range",
            )
            .with_help("`Dec` supports up to 28 significant digits"),
            UnexpectedDotInNumber { span } => Diagnostic::error(
                "QL0020",
                "unexpected `.` in numeric literal",
                span,
                "unexpected `.`",
            ),
            DecimalRounded { span } => Diagnostic::warning(
                "QL0021",
                "decimal literal has more than 28 significant digits",
                span,
                "precision lost (rounded)",
            ),
            InstantExpected { span } => Diagnostic::error(
                "QL0022",
                "expected an instant literal after `@`",
                span,
                "expected `now`, `today`, `unix`, or a date",
            ),
            InstantNameInvalid { span, name } => Diagnostic::error(
                "QL0023",
                format!("invalid instant name `{name}`"),
                span,
                "invalid name",
            )
            .with_help("valid names are `now`, `today`, and `unix`"),
            InstantYearInvalid { span } => Diagnostic::error(
                "QL0024",
                "invalid year in instant literal",
                span,
                "expected at least 4 digits and valid range",
            )
            .with_note(format!(
                "any year from {} to {} is valid",
                NaiveDate::MIN.year(),
                NaiveDate::MAX.year()
            )),
            InstantMonthInvalid { span } => Diagnostic::error(
                "QL0025",
                "invalid month in instant literal",
                span,
                "expected 1-12",
            ),
            InstantDayInvalid { span } => Diagnostic::error(
                "QL0026",
                "invalid day in instant literal",
                span,
                "invalid day",
            ),
            InstantHourInvalid { span } => Diagnostic::error(
                "QL0027",
                "invalid hour in instant literal",
                span,
                "expected 0-23",
            ),
            InstantMinuteInvalid { span } => Diagnostic::error(
                "QL0028",
                "invalid minute in instant literal",
                span,
                "expected 0-59",
            ),
            InstantSecondInvalid { span } => Diagnostic::error(
                "QL0029",
                "invalid second in instant literal",
                span,
                "expected 0-59",
            ),
            InstantTimeColonExpected { span } => Diagnostic::error(
                "QL0030",
                "expected `:` between hour and minute",
                span,
                "expected `:`",
            ),
            InstantUnixSecondsMissing { span } => Diagnostic::error(
                "QL0031",
                "expected seconds after `@unix_`",
                span,
                "expected digits",
            ),
            InstantUnixOverflow { span } => Diagnostic::error(
                "QL0032",
                "`@unix_` seconds are out of range",
                span,
                "out of range",
            ),
            InstantSuffixInvalid { span } => Diagnostic::error(
                "QL0033",
                "unexpected characters after instant literal",
                span,
                "unexpected suffix",
            ),
            FractionRounded { span } => Diagnostic::warning(
                "QL0034",
                "fractional seconds exceed nanosecond precision",
                span,
                "precision lost (rounded)",
            ),
            DurationExpected { span } => Diagnostic::error(
                "QL0035",
                "expected a duration literal after `#`",
                span,
                "expected `<digits><unit>`",
            ),
            DurationUnitMissing { span } => Diagnostic::error(
                "QL0036",
                "expected a duration unit after the number",
                span,
                "expected duration unit",
            )
            .with_help("valid units are: `y`, `w`, `d`, `h`, `m`, and `s`"),
            DurationUnitDuplicate { span, unit } => Diagnostic::error(
                "QL0037",
                format!("duration unit `{unit}` appears more than once"),
                span,
                "duplicate unit",
            ),
            DurationFractionOnNonSecond { span } => Diagnostic::error(
                "QL0038",
                "fractional not supported by duration component",
                span,
                "fractional unit",
            )
            .with_note("only the seconds component may be fractional"),
            DurationOverflow { span } => Diagnostic::error(
                "QL0039",
                "duration literal is out of range",
                span,
                "out of range",
            ),
        }
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
