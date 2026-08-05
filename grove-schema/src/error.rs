use grove_types::{DiagStr, Diagnostic, Label, LabelStyle, Severity, Span};

#[rustfmt::skip] // preserve one-line-per-variant formatting
#[derive(Debug, Clone)]
pub enum SchemaParseError {
    UnexpectedChar { span: Span, ch: char },
    UnclosedString { span: Span },
    UnclosedBacktickIdent { span: Span },

    ExpectedTopLevelStmt { span: Span },
    DuplicateConfigBlock { span: Span, previous: Span },
    DuplicateRoot { span: Span, name: String, previous: Span },

    ExpectedConfigLBrace { span: Span },
    ExpectedConfigKey { span: Span },
    UnknownConfigKey { span: Span, key: String },
    ExpectedConfigEquals { span: Span, key: String },
    ExpectedConfigValue { span: Span, key: String },
    InvalidConfigValue { span: Span, key: String, value: String, valid: String },
    DuplicateConfigKey { span: Span, key: String, previous: Span },
    ExpectedConfigCommaOrRBrace { span: Span, key: String },
    UnclosedConfig { span: Span },

    ExpectedRootName { span: Span },
    ExpectedRootColon { span: Span },
    ExpectedRootStructName { span: Span },
    ExpectedRootUnderlyingTable { span: Span },
    ExpectedRootSemicolon { span: Span },
}

impl From<SchemaParseError> for Diagnostic {
    fn from(err: SchemaParseError) -> Self {
        use SchemaParseError::*;

        match err {
            UnexpectedChar { span, ch } => error_simple(
                "SL0001",
                format!("unexpected character `{ch}`"),
                span,
                "unexpected character",
            ),
            UnclosedString { span } => error_simple(
                "SL0002",
                "unclosed string literal",
                span,
                "missing closing `\"`",
            ),
            UnclosedBacktickIdent { span } => error_simple(
                "SL0003",
                "unclosed backtick identifier",
                span,
                "missing closing '`'",
            ),
            ExpectedTopLevelStmt { span } => error_simple(
                "SP0001",
                "expected a top-level statement",
                span,
                "expected one of `config`, `root`, `struct`, or `rel`",
            ),
            ExpectedConfigLBrace { span } => error_simple(
                "SP0002",
                "expected `{` after `config`",
                span,
                "expected `{`",
            ),
            ExpectedConfigKey { span } => error_simple(
                "SP0003",
                "expected a config key",
                span,
                "expected an identifier",
            ),
            UnknownConfigKey { span, key } => error_simple(
                "SP0004",
                format!("unknown config key `{key}`"),
                span,
                "unknown config key",
            )
            .with_help("valid keys are `int_arithmetic`, `float_checks`, and `dec_arithmetic`"),
            ExpectedConfigEquals { span, key } => error_simple(
                "SP0005",
                format!("expected `=` after config key `{key}`"),
                span,
                "expected `=`",
            ),
            ExpectedConfigValue { span, key } => error_simple(
                "SP0006",
                format!("expected a value for config key `{key}`"),
                span,
                "expected a value",
            ),
            InvalidConfigValue {
                span,
                key,
                value,
                valid,
            } => error_simple(
                "SP0007",
                format!("invalid value `{value}` for config key `{key}`"),
                span,
                "invalid value",
            )
            .with_help(format!("expected one of {valid}")),
            DuplicateConfigKey {
                span,
                key,
                previous,
            } => error_simple(
                "SP0008",
                format!("duplicate config key `{key}`"),
                span,
                "duplicate key",
            )
            .with_label(Label {
                span: previous,
                message: "first defined here".into(),
                style: LabelStyle::Secondary,
            }),
            ExpectedConfigCommaOrRBrace { span, key } => error_simple(
                "SP0009",
                format!("expected `,` or `}}` after config key `{key}`"),
                span,
                "expected `,` or `}`",
            ),
            UnclosedConfig { span } => error_simple(
                "SP0010",
                "unclosed `config` block",
                span,
                "missing closing `}`",
            ),
            DuplicateConfigBlock { span, previous } => error_simple(
                "SP0011",
                "duplicate `config` block",
                span,
                "redundant `config` block",
            )
            .with_label(Label {
                span: previous,
                message: "first `config` block here".into(),
                style: LabelStyle::Secondary,
            }),
            ExpectedRootName { span } => error_simple(
                "SP0012",
                "expected a root name",
                span,
                "expected an identifier",
            ),
            ExpectedRootColon { span } => error_simple(
                "SP0013",
                "expected `:` after root name",
                span,
                "expected `:`",
            ),
            ExpectedRootStructName { span } => error_simple(
                "SP0014",
                "expected a struct type after `:`",
                span,
                "expected a struct name",
            ),
            ExpectedRootUnderlyingTable { span } => error_simple(
                "SP0015",
                "expected an underlying table name after `@`",
                span,
                "expected an identifier",
            ),
            ExpectedRootSemicolon { span } => error_simple(
                "SP0016",
                "expected `;` to end root statement",
                span,
                "expected `;`",
            ),
            DuplicateRoot {
                span,
                name,
                previous,
            } => error_simple(
                "SP0017",
                format!("duplicate root `{name}`"),
                span,
                "redundant root",
            )
            .with_label(Label {
                span: previous,
                message: "first defined here".into(),
                style: LabelStyle::Secondary,
            }),
        }
    }
}

fn error_simple(
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
