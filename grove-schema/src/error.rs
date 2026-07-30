use grove_types::{Diagnostic, Label, LabelStyle, Severity, Span};

#[rustfmt::skip] // preserve one-line-per-variant formatting
#[derive(Debug, Clone)]
pub enum SchemaParseError {
    UnexpectedChar { span: Span, ch: char },
    UnclosedString { span: Span },
    UnclosedBacktickIdent { span: Span },
}

impl From<SchemaParseError> for Diagnostic {
    fn from(err: SchemaParseError) -> Self {
        use SchemaParseError::*;

        let (severity, code, message, labels, notes, help) = match err {
            UnexpectedChar { span, ch } => (
                Severity::Error,
                "SL0001".into(),
                format!("unexpected character `{ch}`").into(),
                vec![Label {
                    span,
                    message: "unexpected character".into(),
                    style: LabelStyle::Primary,
                }],
                vec![],
                vec![],
            ),
            UnclosedString { span } => (
                Severity::Error,
                "SL0002".into(),
                "unclosed string literal".into(),
                vec![Label {
                    span,
                    message: "missing closing `\"`".into(),
                    style: LabelStyle::Primary,
                }],
                vec![],
                vec![],
            ),
            UnclosedBacktickIdent { span } => (
                Severity::Error,
                "SL0003".into(),
                "unclosed backtick identifier".into(),
                vec![Label {
                    span,
                    message: "missing closing '`'".into(),
                    style: LabelStyle::Primary,
                }],
                vec![],
                vec![],
            ),
        };

        Diagnostic {
            severity,
            code,
            message,
            labels,
            notes,
            help,
        }
    }
}
