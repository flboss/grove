use grove_types::{Diagnostic, Label, LabelStyle, Severity, Span};

#[rustfmt::skip]
#[derive(Debug, Clone)]
pub enum SchemaParseError {
}

impl From<SchemaParseError> for Diagnostic {
    fn from(err: SchemaParseError) -> Self {
        use SchemaParseError::*;

        let (severity, code, message, labels, notes, help) = match err {};

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
