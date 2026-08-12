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
        }
    }
}
