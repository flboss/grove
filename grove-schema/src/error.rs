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
    ExpectedRootColon { span: Span, name: String },
    ExpectedRootStructName { span: Span },
    ExpectedRootUnderlyingTable { span: Span },
    ExpectedRootSemicolon { span: Span },

    ExpectedStructName { span: Span },
    ExpectedStructLBrace { span: Span, name: String },
    ExpectedStructFieldName { span: Span, struct_name: String },
    ExpectedStructFieldColon { span: Span, field: String, struct_name: String },
    ExpectedType { span: Span },
    ExpectedAtColumn { span: Span },
    ExpectedListLAngle { span: Span },
    ExpectedTupleLAngle { span: Span },
    ExpectedListRAngle { span: Span },
    ExpectedTupleCommaOrRAngle { span: Span },
    ExpectedTupleCommaOrRParen { span: Span },
    ExpectedStructCommaOrRBrace { span: Span, struct_name: String },
    UnclosedStruct { span: Span },
    DuplicateStruct { span: Span, name: String, previous: Span },
    DuplicateStructField {
        span: Span,
        struct_name: String,
        field: String,
        previous: Span,
    },
    ExpectedColumn { span: Span },
    ExpectedColumnCommaOrRParen { span: Span },
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
            ExpectedRootColon { span, name } => error_simple(
                "SP0013",
                format!("expected `:` after root name `{name}`"),
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
            ExpectedStructName { span } => error_simple(
                "SP0018",
                "expected a struct name",
                span,
                "expected an identifier",
            ),
            ExpectedStructLBrace { span, name } => error_simple(
                "SP0019",
                format!("expected `{{` after struct name `{name}`"),
                span,
                "expected `{`",
            ),
            ExpectedStructFieldName { span, struct_name } => error_simple(
                "SP0020",
                format!("expected a field name in struct `{struct_name}`"),
                span,
                "expected an identifier",
            ),
            ExpectedStructFieldColon {
                span,
                field,
                struct_name,
            } => error_simple(
                "SP0021",
                format!("expected `:` after field `{field}` in struct `{struct_name}`"),
                span,
                "expected `:`",
            ),
            ExpectedType { span } => {
                error_simple("SP0022", "expected a type", span, "expected a type")
            }
            ExpectedAtColumn { span } => error_simple(
                "SP0023",
                "expected one or more column names after `@`",
                span,
                "expected an identifier or `(`",
            ),
            ExpectedListLAngle { span } => {
                error_simple("SP0024", "expected `<` after `List`", span, "expected `<`")
            }
            ExpectedTupleLAngle { span } => {
                error_simple("SP0025", "expected `<` after `Tuple`", span, "expected `<`")
            }
            ExpectedListRAngle { span } => error_simple(
                "SP0026",
                "expected `>` to close the type arguments",
                span,
                "expected `>`",
            ),
            ExpectedTupleCommaOrRAngle { span } => error_simple(
                "SP0027",
                "expected `,` or `>` after tuple element",
                span,
                "expected `,` or `>`",
            ),
            ExpectedTupleCommaOrRParen { span } => error_simple(
                "SP0028",
                "expected `,` or `)` after tuple element",
                span,
                "expected `,` or `)`",
            ),
            ExpectedStructCommaOrRBrace { span, struct_name } => error_simple(
                "SP0029",
                format!("expected `,` or `}}` after field in struct `{struct_name}`"),
                span,
                "expected `,` or `}`",
            ),
            DuplicateStructField {
                span,
                struct_name,
                field,
                previous,
            } => error_simple(
                "SP0030",
                format!("duplicate field `{field}` in struct `{struct_name}`"),
                span,
                "duplicate field",
            )
            .with_label(Label {
                span: previous,
                message: "first defined here".into(),
                style: LabelStyle::Secondary,
            }),
            UnclosedStruct { span } => error_simple(
                "SP0031",
                "unclosed `struct` block",
                span,
                "missing closing `}`",
            ),
            DuplicateStruct {
                span,
                name,
                previous,
            } => error_simple(
                "SP0032",
                format!("duplicate struct `{name}`"),
                span,
                "redundant struct",
            )
            .with_label(Label {
                span: previous,
                message: "first defined here".into(),
                style: LabelStyle::Secondary,
            }),
            ExpectedColumn { span } => error_simple(
                "SP0033",
                "expected a column name",
                span,
                "expected an identifier",
            ),
            ExpectedColumnCommaOrRParen { span } => error_simple(
                "SP0034",
                "expected `,` or `)` after column",
                span,
                "expected `,` or `)`",
            ),
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
