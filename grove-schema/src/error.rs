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
    ExpectedColumns { span: Span },
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

    ExpectedListViaTable { span: Span },
    ExpectedListViaLBracket { span: Span },
    ExpectedListViaKeyCol { span: Span },
    ExpectedListViaComma { span: Span },
    ExpectedListViaRBracket { span: Span },
    ExpectedListForVia { span: Span },

    ExpectedRelEndpointStruct { span: Span },
    ExpectedRelEndpointDot { span: Span },
    ExpectedRelEndpointField { span: Span },
    ExpectedRelArrow { span: Span },
    ExpectedRelSemicolon { span: Span },
    ExpectedRelLParen { span: Span },
    ExpectedRelLinkTable { span: Span },
    ExpectedRelLinkDot { span: Span },
    ExpectedRelLinkColumn { span: Span },
    ExpectedRelLinkArrow { span: Span },
    ExpectedRelPKTable { span: Span },
    ExpectedRelPKDot { span: Span },
    ExpectedRelPKColumn { span: Span },
    ExpectedRelRParen { span: Span },
    ExpectedRelViaTable { span: Span },
    ExpectedRelViaLBracket { span: Span },
    ExpectedRelViaColumn { span: Span },
    ExpectedRelViaArrow { span: Span },
    ExpectedRelViaTargetTable { span: Span },
    ExpectedRelViaTargetDot { span: Span },
    ExpectedRelViaTargetColumn { span: Span },
    ExpectedRelViaCommaOrRBracket { span: Span },
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
            ExpectedColumns { span } => error_simple(
                "SP0023",
                "expected one or more underlying column names",
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
                "expected an underlying column name",
                span,
                "expected an identifier",
            ),
            ExpectedColumnCommaOrRParen { span } => error_simple(
                "SP0034",
                "expected `,` or `)` after column",
                span,
                "expected `,` or `)`",
            ),
            ExpectedListViaTable { span } => error_simple(
                "SP0035",
                "expected a table name after `via`",
                span,
                "expected an identifier",
            ),
            ExpectedListViaLBracket { span } => error_simple(
                "SP0036",
                "expected `[` after via table",
                span,
                "expected `[`",
            ),
            ExpectedListViaKeyCol { span } => error_simple(
                "SP0037",
                "expected a key column in `via` clause",
                span,
                "expected an identifier",
            ),
            ExpectedListViaComma { span } => error_simple(
                "SP0038",
                "expected `,` after key column in `via` clause",
                span,
                "expected `,`",
            ),
            ExpectedListViaRBracket { span } => error_simple(
                "SP0039",
                "expected `]` to close the `via` clause",
                span,
                "expected `]`",
            ),
            ExpectedListForVia { span } => error_simple(
                "SP0040",
                "cannot attach a `via` clause to type",
                span,
                "expected a `List` type",
            ),
            ExpectedRelEndpointStruct { span } => error_simple(
                "SP0041",
                "expected a struct name for the relation endpoint",
                span,
                "expected an identifier",
            ),
            ExpectedRelEndpointDot { span } => error_simple(
                "SP0042",
                "expected `.` after the relation endpoint struct",
                span,
                "expected `.`",
            ),
            ExpectedRelEndpointField { span } => error_simple(
                "SP0043",
                "expected a field name for the relation endpoint",
                span,
                "expected an identifier",
            ),
            ExpectedRelArrow { span } => error_simple(
                "SP0044",
                "expected a relation arrow",
                span,
                "expected one of `<->`, `<<->`, `<->>`, or `<<->>`",
            ),
            ExpectedRelSemicolon { span } => error_simple(
                "SP0045",
                "expected `;` to end the rel statement",
                span,
                "expected `;`",
            ),
            ExpectedRelLParen { span } => error_simple(
                "SP0046",
                "expected `(` to start the foreign-key mapping",
                span,
                "expected `(`",
            ),
            ExpectedRelLinkTable { span } => error_simple(
                "SP0047",
                "expected a table name for the child-side foreign key",
                span,
                "expected an identifier",
            ),
            ExpectedRelLinkDot { span } => error_simple(
                "SP0048",
                "expected `.` after the child-side foreign key table",
                span,
                "expected `.`",
            ),
            ExpectedRelLinkColumn { span } => error_simple(
                "SP0049",
                "expected a column name for the child-side foreign key",
                span,
                "expected an identifier",
            ),
            ExpectedRelLinkArrow { span } => error_simple(
                "SP0050",
                "expected `->` between the foreign key and the target column",
                span,
                "expected `->`",
            ),
            ExpectedRelPKTable { span } => error_simple(
                "SP0051",
                "expected a table name for the target primary key",
                span,
                "expected an identifier",
            ),
            ExpectedRelPKDot { span } => error_simple(
                "SP0052",
                "expected `.` after the target primary key table",
                span,
                "expected `.`",
            ),
            ExpectedRelPKColumn { span } => error_simple(
                "SP0053",
                "expected a column name for the target primary key",
                span,
                "expected an identifier",
            ),
            ExpectedRelRParen { span } => error_simple(
                "SP0054",
                "expected `)` to close the foreign-key mapping",
                span,
                "expected `)`",
            ),
            ExpectedRelViaTable { span } => error_simple(
                "SP0055",
                "expected a join table name after `via`",
                span,
                "expected an identifier",
            ),
            ExpectedRelViaLBracket { span } => error_simple(
                "SP0056",
                "expected `[` after the join table name",
                span,
                "expected `[`",
            ),
            ExpectedRelViaColumn { span } => error_simple(
                "SP0057",
                "expected a join table column name",
                span,
                "expected an identifier",
            ),
            ExpectedRelViaArrow { span } => error_simple(
                "SP0058",
                "expected `->` after the join table column",
                span,
                "expected `->`",
            ),
            ExpectedRelViaTargetTable { span } => error_simple(
                "SP0059",
                "expected a table name after `->` in the join table mapping",
                span,
                "expected an identifier",
            ),
            ExpectedRelViaTargetDot { span } => error_simple(
                "SP0060",
                "expected `.` after the join target table",
                span,
                "expected `.`",
            ),
            ExpectedRelViaTargetColumn { span } => error_simple(
                "SP0061",
                "expected a column name after `->` in the join table mapping",
                span,
                "expected an identifier",
            ),
            ExpectedRelViaCommaOrRBracket { span } => error_simple(
                "SP0062",
                "expected `,` or `]` after a join table column mapping",
                span,
                "expected `,` or `]`",
            ),
        }
    }
}

pub(crate) fn error_simple(
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
