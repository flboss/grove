use grove_types::{Diagnostic, Label, LabelStyle, Span};

use crate::error::error_simple;

#[rustfmt::skip] // preserve one-line-per-variant formatting
#[derive(Debug, Clone)]
pub enum SchemaValidationError {
    UnknownStructRef {
        name: String,
        occurrences: Vec<Span>,
    },
    InvalidArrow { span: Span },
    TableMismatch {
        span: Span,
        name: String,
        tables: Vec<String>,
    },
    StructNoTable { span: Span, name: String },
    DuplicateTable {
        span: Span,
        name: String,
        previous: Span,
    },
    DuplicateColumn {
        table: String,
        name: String,
        span: Span,
        previous: Span,
    },
    NestedStructRef {
        span: Span,
        struct_name: String,
        field: String,
        nested: Span,
    },
    ViaOnStructList {
        span: Span,
        struct_name: String,
        field: String,
    },
    TupleArityMismatch {
        span: Span,
        columns: Span,
        struct_name: String,
        field: String,
        declared: usize,
        expected: usize,
    },
    ViaArityMismatch {
        span: Span,
        columns: Span,
        struct_name: String,
        field: String,
        declared: usize,
        expected: usize,
    },
    NoViaScalarList {
        span: Span,
        struct_name: String,
        field: String,
    },
    UnmatchedForwardRef {
        span: Span,
        struct_name: String,
        field: String,
    },
}

impl From<SchemaValidationError> for Diagnostic {
    fn from(err: SchemaValidationError) -> Self {
        use SchemaValidationError::*;

        match err {
            UnknownStructRef { name, occurrences } => {
                let (first, rest) = occurrences
                    .split_first()
                    .expect("an unknown struct reference always has at least one occurrence");
                let mut diag = error_simple(
                    "SV0001",
                    format!("unknown struct reference `{name}`"),
                    *first,
                    "unknown struct",
                );
                for span in rest {
                    diag = diag.with_label(Label {
                        span: *span,
                        message: "also referenced here".into(),
                        style: LabelStyle::Secondary,
                    });
                }
                diag
            }
            InvalidArrow { span } => error_simple(
                "SV0002",
                "invalid relation arrow `<->>`",
                span,
                "invalid arrow",
            )
            .with_note("a child holding a single foreign key cannot reference many parents")
            .with_help("use `<<->` for many children to one parent"),
            TableMismatch { span, name, tables } => error_simple(
                "SV0003",
                format!("struct `{name}` maps to multiple tables"),
                span,
                "conflicting table mappings",
            )
            .with_note(format!("candidate tables: {}", tables.join(", "))),
            StructNoTable { span, name } => error_simple(
                "SV0004",
                format!("struct `{name}` has no underlying table"),
                span,
                "no underlying table",
            )
            .with_help("add a `root` for this struct or a relation referencing it"),
            DuplicateTable {
                span,
                name,
                previous,
            } => error_simple(
                "SV0005",
                format!("duplicate table `{name}`"),
                span,
                "table already declared",
            )
            .with_label(Label {
                span: previous,
                message: "first declared here".into(),
                style: LabelStyle::Secondary,
            }),
            DuplicateColumn {
                table,
                name,
                span,
                previous,
            } => error_simple(
                "SV0006",
                format!("duplicate column `{table}.{name}`"),
                span,
                "column already declared",
            )
            .with_label(Label {
                span: previous,
                message: "first declared here".into(),
                style: LabelStyle::Secondary,
            }),
            NestedStructRef {
                span,
                struct_name,
                field,
                nested,
            } => error_simple(
                "SV0007",
                format!(
                    "field `{struct_name}.{field}` nests a struct reference inside a value type"
                ),
                span,
                "struct in a value position",
            )
            .with_label(Label {
                span: nested,
                message: "nested struct reference".into(),
                style: LabelStyle::Secondary,
            })
            .with_help(
                "struct references are only valid as a standalone `S`, `?S`, or `List<S>` field",
            ),
            ViaOnStructList {
                span,
                struct_name,
                field,
            } => error_simple(
                "SV0008",
                format!(
                    "field `{struct_name}.{field}` attaches a `via` storage table to a list of \
                     structs"
                ),
                span,
                "`via` on a struct list",
            )
            .with_help("struct collections are defined by `rel` statements, not a `via` clause"),
            TupleArityMismatch {
                span,
                columns,
                struct_name,
                field,
                declared,
                expected,
            } => error_simple(
                "SV0009",
                format!(
                    "field `{struct_name}.{field}` maps {declared} column(s) but its type needs \
                     {expected}"
                ),
                span,
                format!("type needs {expected} column(s)"),
            )
            .with_label(Label {
                span: columns,
                message: format!("{declared} column(s) provided").into(),
                style: LabelStyle::Secondary,
            }),
            ViaArityMismatch {
                span,
                columns,
                struct_name,
                field,
                declared,
                expected,
            } => error_simple(
                "SV0010",
                format!(
                    "field `{struct_name}.{field}` lists {declared} value column(s) in its \
                     storage table but the element type needs {expected}"
                ),
                span,
                format!("type needs {expected} column(s)"),
            )
            .with_label(Label {
                span: columns,
                message: format!("{declared} column(s) provided").into(),
                style: LabelStyle::Secondary,
            }),
            NoViaScalarList {
                span,
                struct_name,
                field,
            } => error_simple(
                "SV0011",
                format!(
                    "field `{struct_name}.{field}` is a list of values without a `via` storage \
                     table"
                ),
                span,
                "missing `via` storage",
            )
            .with_help("a list of scalar values must be backed by a `via` storage table"),
            UnmatchedForwardRef {
                span,
                struct_name,
                field,
            } => error_simple(
                "SV0012",
                format!(
                    "field `{struct_name}.{field}` is a struct reference that cannot be \
                     materialized"
                ),
                span,
                "unrepresentable reference",
            )
            .with_help("struct references must be matched by a `rel` forward reference"),
        }
    }
}
