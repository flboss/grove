use grove_types::{Diagnostic, Span};

#[rustfmt::skip] // preserve one-line-per-variant formatting
#[derive(Debug, Clone)]
pub(crate) enum SchemaParseError {
    UnexpectedChar { span: Span, ch: char },
    UnclosedString { span: Span },
    UnclosedBacktickIdent { span: Span },
    IntLiteralOverflow { span: Span },

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
    ExpectedListRAngle { span: Span },
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

    ExpectedEnumName { span: Span },
    ExpectedEnumLBrace { span: Span, name: String },
    ExpectedEnumVariantName { span: Span, enum_name: String },
    ExpectedEnumArrow {
        span: Span,
        enum_name: String,
        variant: String,
    },
    ExpectedEnumValue {
        span: Span,
        enum_name: String,
        variant: String,
    },
    ExpectedEnumCommaOrRBrace { span: Span, enum_name: String },
    UnclosedEnum { span: Span },
    DuplicateEnum {
        span: Span,
        name: String,
        previous: Span,
    },
    DuplicateEnumVariant {
        span: Span,
        enum_name: String,
        variant: String,
        previous: Span,
    },
    DuplicateEnumValue {
        span: Span,
        enum_name: String,
        value: String,
        previous: Span,
    },
    MixedEnumValueTypes {
        span: Span,
        enum_name: String,
        expected: String,
        found: String,
    },
    DuplicateTypeName {
        span: Span,
        name: String,
        previous: Span,
    },
    EmptyEnum { span: Span, name: String },

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
            UnexpectedChar { span, ch } => Diagnostic::error(
                "SL0001",
                format!("unexpected character `{ch}`"),
                span,
                "unexpected character",
            ),
            UnclosedString { span } => Diagnostic::error(
                "SL0002",
                "unclosed string literal",
                span,
                "missing closing `\"`",
            ),
            UnclosedBacktickIdent { span } => Diagnostic::error(
                "SL0003",
                "unclosed backtick identifier",
                span,
                "missing closing '`'",
            ),
            IntLiteralOverflow { span } => Diagnostic::error(
                "SL0004",
                "integer literal is out of range for `i64`",
                span,
                "literal does not fit in `i64`",
            )
            .with_help(format!("valid values are {}..{}", i64::MIN, i64::MAX)),
            ExpectedTopLevelStmt { span } => Diagnostic::error(
                "SP0001",
                "expected a top-level statement",
                span,
                "expected one of `config`, `root`, `struct`, `enum`, or `rel`",
            ),
            ExpectedConfigLBrace { span } => Diagnostic::error(
                "SP0002",
                "expected `{` after `config`",
                span,
                "expected `{`",
            ),
            ExpectedConfigKey { span } => Diagnostic::error(
                "SP0003",
                "expected a config key",
                span,
                "expected an identifier",
            ),
            UnknownConfigKey { span, key } => Diagnostic::error(
                "SP0004",
                format!("unknown config key `{key}`"),
                span,
                "unknown config key",
            )
            .with_help("valid keys are `int_arithmetic`, `float_checks`, and `dec_arithmetic`"),
            ExpectedConfigEquals { span, key } => Diagnostic::error(
                "SP0005",
                format!("expected `=` after config key `{key}`"),
                span,
                "expected `=`",
            ),
            ExpectedConfigValue { span, key } => Diagnostic::error(
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
            } => Diagnostic::error(
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
            } => Diagnostic::error(
                "SP0008",
                format!("duplicate config key `{key}`"),
                span,
                "duplicate key",
            )
            .with_secondary_label(previous, "first declared here"),
            ExpectedConfigCommaOrRBrace { span, key } => Diagnostic::error(
                "SP0009",
                format!("expected `,` or `}}` after config key `{key}`"),
                span,
                "expected `,` or `}`",
            ),
            UnclosedConfig { span } => Diagnostic::error(
                "SP0010",
                "unclosed `config` block",
                span,
                "missing closing `}`",
            ),
            DuplicateConfigBlock { span, previous } => Diagnostic::error(
                "SP0011",
                "duplicate `config` block",
                span,
                "config already defined",
            )
            .with_secondary_label(previous, "first `config` block here"),
            ExpectedRootName { span } => Diagnostic::error(
                "SP0012",
                "expected a root name",
                span,
                "expected an identifier",
            ),
            ExpectedRootColon { span, name } => Diagnostic::error(
                "SP0013",
                format!("expected `:` after root name `{name}`"),
                span,
                "expected `:`",
            ),
            ExpectedRootStructName { span } => Diagnostic::error(
                "SP0014",
                "expected a struct type after `:`",
                span,
                "expected a struct name",
            ),
            ExpectedRootUnderlyingTable { span } => Diagnostic::error(
                "SP0015",
                "expected an underlying table name after `@`",
                span,
                "expected an identifier",
            ),
            ExpectedRootSemicolon { span } => Diagnostic::error(
                "SP0016",
                "expected `;` to end root statement",
                span,
                "expected `;`",
            ),
            DuplicateRoot {
                span,
                name,
                previous,
            } => Diagnostic::error(
                "SP0017",
                format!("duplicate root `{name}`"),
                span,
                "duplicate root",
            )
            .with_secondary_label(previous, "first declared here"),
            ExpectedStructName { span } => Diagnostic::error(
                "SP0018",
                "expected a struct name",
                span,
                "expected an identifier",
            ),
            ExpectedStructLBrace { span, name } => Diagnostic::error(
                "SP0019",
                format!("expected `{{` after struct name `{name}`"),
                span,
                "expected `{`",
            ),
            ExpectedStructFieldName { span, struct_name } => Diagnostic::error(
                "SP0020",
                format!("expected a field name in struct `{struct_name}`"),
                span,
                "expected an identifier",
            ),
            ExpectedStructFieldColon {
                span,
                field,
                struct_name,
            } => Diagnostic::error(
                "SP0021",
                format!("expected `:` after field `{field}` in struct `{struct_name}`"),
                span,
                "expected `:`",
            ),
            ExpectedType { span } => {
                Diagnostic::error("SP0022", "expected a type", span, "expected a type")
            }
            ExpectedColumns { span } => Diagnostic::error(
                "SP0023",
                "expected one or more underlying column names",
                span,
                "expected an identifier or `(`",
            ),
            ExpectedListLAngle { span } => {
                Diagnostic::error("SP0024", "expected `<` after `List`", span, "expected `<`")
            }
            ExpectedListRAngle { span } => Diagnostic::error(
                "SP0026",
                "expected `>` to close the type arguments",
                span,
                "expected `>`",
            ),
            ExpectedTupleCommaOrRParen { span } => Diagnostic::error(
                "SP0028",
                "expected `,` or `)` after tuple element",
                span,
                "expected `,` or `)`",
            ),
            ExpectedStructCommaOrRBrace { span, struct_name } => Diagnostic::error(
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
            } => Diagnostic::error(
                "SP0030",
                format!("duplicate field `{field}` in struct `{struct_name}`"),
                span,
                "duplicate field",
            )
            .with_secondary_label(previous, "first declared here"),
            UnclosedStruct { span } => Diagnostic::error(
                "SP0031",
                "unclosed `struct` block",
                span,
                "missing closing `}`",
            ),
            DuplicateStruct {
                span,
                name,
                previous,
            } => Diagnostic::error(
                "SP0032",
                format!("duplicate struct `{name}`"),
                span,
                "duplicate struct",
            )
            .with_secondary_label(previous, "first declared here"),
            ExpectedColumn { span } => Diagnostic::error(
                "SP0033",
                "expected an underlying column name",
                span,
                "expected an identifier",
            ),
            ExpectedColumnCommaOrRParen { span } => Diagnostic::error(
                "SP0034",
                "expected `,` or `)` after column",
                span,
                "expected `,` or `)`",
            ),
            ExpectedListViaTable { span } => Diagnostic::error(
                "SP0035",
                "expected a table name after `via`",
                span,
                "expected an identifier",
            ),
            ExpectedListViaLBracket { span } => Diagnostic::error(
                "SP0036",
                "expected `[` after via table",
                span,
                "expected `[`",
            ),
            ExpectedListViaKeyCol { span } => Diagnostic::error(
                "SP0037",
                "expected a key column in `via` clause",
                span,
                "expected an identifier",
            ),
            ExpectedListViaComma { span } => Diagnostic::error(
                "SP0038",
                "expected `,` after key column in `via` clause",
                span,
                "expected `,`",
            ),
            ExpectedListViaRBracket { span } => Diagnostic::error(
                "SP0039",
                "expected `]` to close the `via` clause",
                span,
                "expected `]`",
            ),
            ExpectedListForVia { span } => Diagnostic::error(
                "SP0040",
                "cannot attach a `via` clause to type",
                span,
                "expected a `List` type",
            ),
            ExpectedEnumName { span } => Diagnostic::error(
                "SP0063",
                "expected an enum name",
                span,
                "expected an identifier",
            ),
            ExpectedEnumLBrace { span, name } => Diagnostic::error(
                "SP0064",
                format!("expected `{{` after enum name `{name}`"),
                span,
                "expected `{`",
            ),
            ExpectedEnumVariantName { span, enum_name } => Diagnostic::error(
                "SP0065",
                format!("expected a variant name in enum `{enum_name}`"),
                span,
                "expected an identifier",
            ),
            ExpectedEnumArrow {
                span,
                enum_name,
                variant,
            } => Diagnostic::error(
                "SP0066",
                format!("expected `=>` after variant `{variant}` in enum `{enum_name}`"),
                span,
                "expected `=>`",
            ),
            ExpectedEnumValue {
                span,
                enum_name,
                variant,
            } => Diagnostic::error(
                "SP0067",
                format!(
                    "expected a string or integer value \
                    for variant `{variant}` in enum `{enum_name}`"
                ),
                span,
                "expected a string or integer literal",
            )
            .with_help("enum underlying values must be string or integer literals"),
            ExpectedEnumCommaOrRBrace { span, enum_name } => Diagnostic::error(
                "SP0068",
                format!("expected `,` or `}}` after variant in enum `{enum_name}`"),
                span,
                "expected `,` or `}`",
            ),
            UnclosedEnum { span } => Diagnostic::error(
                "SP0069",
                "unclosed `enum` block",
                span,
                "missing closing `}`",
            ),
            DuplicateEnum {
                span,
                name,
                previous,
            } => Diagnostic::error(
                "SP0073",
                format!("duplicate enum `{name}`"),
                span,
                "duplicate enum",
            )
            .with_secondary_label(previous, "first declared here"),
            DuplicateEnumVariant {
                span,
                enum_name,
                variant,
                previous,
            } => Diagnostic::error(
                "SP0070",
                format!("duplicate variant `{variant}` in enum `{enum_name}`"),
                span,
                "duplicate variant",
            )
            .with_secondary_label(previous, "first declared here"),
            DuplicateEnumValue {
                span,
                enum_name,
                value,
                previous,
            } => Diagnostic::error(
                "SP0071",
                format!("duplicate underlying value {value} in enum `{enum_name}`"),
                span,
                "duplicate value",
            )
            .with_secondary_label(previous, "first used here")
            .with_help("variants must have unique underlying values within each enum"),
            MixedEnumValueTypes {
                span,
                enum_name,
                expected,
                found,
            } => Diagnostic::error(
                "SP0072",
                format!("enum `{enum_name}` mixes underlying value types"),
                span,
                format!("this value is `{found}`"),
            )
            .with_note(format!("the first variant uses `{expected}` values"))
            .with_help("all variants in an enum must use either String or Int values"),
            DuplicateTypeName {
                span,
                name,
                previous,
            } => Diagnostic::error(
                "SP0074",
                format!("duplicate type name `{name}`"),
                span,
                "type name already declared",
            )
            .with_secondary_label(previous, "first declared here"),
            EmptyEnum { span, name } => Diagnostic::error(
                "SP0075",
                format!("enum `{name}` has no variants"),
                span,
                "empty enum",
            )
            .with_help("add at least one variant to the enum"),
            ExpectedRelEndpointStruct { span } => Diagnostic::error(
                "SP0041",
                "expected a struct name for the relation endpoint",
                span,
                "expected an identifier",
            ),
            ExpectedRelEndpointDot { span } => Diagnostic::error(
                "SP0042",
                "expected `.` after the relation endpoint struct",
                span,
                "expected `.`",
            ),
            ExpectedRelEndpointField { span } => Diagnostic::error(
                "SP0043",
                "expected a field name for the relation endpoint",
                span,
                "expected an identifier",
            ),
            ExpectedRelArrow { span } => Diagnostic::error(
                "SP0044",
                "expected a relation arrow",
                span,
                "expected one of `<->`, `<<->`, `<->>`, or `<<->>`",
            ),
            ExpectedRelSemicolon { span } => Diagnostic::error(
                "SP0045",
                "expected `;` to end the rel statement",
                span,
                "expected `;`",
            ),
            ExpectedRelLParen { span } => Diagnostic::error(
                "SP0046",
                "expected `(` to start the foreign-key mapping",
                span,
                "expected `(`",
            ),
            ExpectedRelLinkTable { span } => Diagnostic::error(
                "SP0047",
                "expected a table name for the child-side foreign key",
                span,
                "expected an identifier",
            ),
            ExpectedRelLinkDot { span } => Diagnostic::error(
                "SP0048",
                "expected `.` after the child-side foreign key table",
                span,
                "expected `.`",
            ),
            ExpectedRelLinkColumn { span } => Diagnostic::error(
                "SP0049",
                "expected a column name for the child-side foreign key",
                span,
                "expected an identifier",
            ),
            ExpectedRelLinkArrow { span } => Diagnostic::error(
                "SP0050",
                "expected `->` between the foreign key and the target column",
                span,
                "expected `->`",
            ),
            ExpectedRelPKTable { span } => Diagnostic::error(
                "SP0051",
                "expected a table name for the target primary key",
                span,
                "expected an identifier",
            ),
            ExpectedRelPKDot { span } => Diagnostic::error(
                "SP0052",
                "expected `.` after the target primary key table",
                span,
                "expected `.`",
            ),
            ExpectedRelPKColumn { span } => Diagnostic::error(
                "SP0053",
                "expected a column name for the target primary key",
                span,
                "expected an identifier",
            ),
            ExpectedRelRParen { span } => Diagnostic::error(
                "SP0054",
                "expected `)` to close the foreign-key mapping",
                span,
                "expected `)`",
            ),
            ExpectedRelViaTable { span } => Diagnostic::error(
                "SP0055",
                "expected a join table name after `via`",
                span,
                "expected an identifier",
            ),
            ExpectedRelViaLBracket { span } => Diagnostic::error(
                "SP0056",
                "expected `[` after the join table name",
                span,
                "expected `[`",
            ),
            ExpectedRelViaColumn { span } => Diagnostic::error(
                "SP0057",
                "expected a join table column name",
                span,
                "expected an identifier",
            ),
            ExpectedRelViaArrow { span } => Diagnostic::error(
                "SP0058",
                "expected `->` after the join table column",
                span,
                "expected `->`",
            ),
            ExpectedRelViaTargetTable { span } => Diagnostic::error(
                "SP0059",
                "expected a table name after `->` in the join table mapping",
                span,
                "expected an identifier",
            ),
            ExpectedRelViaTargetDot { span } => Diagnostic::error(
                "SP0060",
                "expected `.` after the join target table",
                span,
                "expected `.`",
            ),
            ExpectedRelViaTargetColumn { span } => Diagnostic::error(
                "SP0061",
                "expected a column name after `->` in the join table mapping",
                span,
                "expected an identifier",
            ),
            ExpectedRelViaCommaOrRBracket { span } => Diagnostic::error(
                "SP0062",
                "expected `,` or `]` after a join table column mapping",
                span,
                "expected `,` or `]`",
            ),
        }
    }
}
