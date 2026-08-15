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
