use grove_exec::value::Val;
use grove_schema::validated::ValidatedSchema;

use crate::args::Format;
use crate::error::{CliError, CliResult};

pub fn render(value: &Val, schema: &ValidatedSchema, format: Format) -> CliResult<String> {
    match format {
        Format::Auto | Format::Inline => Ok(value.formatter(schema).to_string()),
        Format::Raw => Ok(value.to_string()),
        Format::Table | Format::Vertical => Err(CliError::runtime(format!(
            "output format `{}` is not yet implemented",
            format.name()
        ))),
    }
}
