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
        }
    }
}
