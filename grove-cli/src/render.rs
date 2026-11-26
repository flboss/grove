use grove_exec::value::Val;
use grove_query::QueryType;
use grove_schema::ValidatedSchema;

use crate::args::Format;
use crate::error::{CliError, CliResult};
use crate::render::inline::compact;
use crate::render::layout::render_auto;
use crate::render::table::{TableError, build_table, render_table};
use crate::render::vertical::block_lines;

mod inline;
mod layout;
mod options;
mod table;
mod text;
mod theme;
mod vertical;

pub(crate) use options::RenderOptions;
pub(crate) use theme::paint;

pub(crate) fn render(
    value: &mut Val,
    ty: Option<&QueryType>,
    schema: &ValidatedSchema,
    options: &RenderOptions,
) -> CliResult<String> {
    if options.hide_refs && filter_refs(value) {
        return Ok(String::new());
    };
    let ctx = RenderCtx { schema, options };

    match options.format {
        Format::Raw => Ok(value.to_string()),
        Format::Inline => Ok(compact(&ctx, value, 0).styled),
        Format::Vertical => Ok(block_lines(&ctx, value, 0).join("\n")),
        Format::Table => match build_table(&ctx, value, ty) {
            Ok(data) => Ok(render_table(&ctx, data)),
            Err(TableError::NotTabular) => Err(CliError::runtime(
                "output format `table` requires a list of records or tuples; use `vertical` or `auto`",
            )),
            Err(TableError::TooWide) => Err(CliError::runtime(format!(
                "table does not fit in {} columns; use `vertical`, reduce `--max-cell-width`, or increase `--width`",
                options.width
            ))),
        },
        Format::Auto => render_auto(&ctx, value, ty),
    }
}

fn filter_refs(value: &mut Val) -> bool {
    match value {
        Val::Ref { .. } => true,
        Val::Some(inner) => filter_refs(inner),
        Val::List(items) => {
            items.items.retain_mut(|item| !filter_refs(item));
            false
        }
        Val::Tuple(items) => {
            items.retain_mut(|item| !filter_refs(item));
            false
        }
        Val::Record(fields) => {
            fields.fields.retain_mut(|(_, value)| !filter_refs(value));
            false
        }
        _ => false,
    }
}

pub(super) struct RenderCtx<'a> {
    pub(super) schema: &'a ValidatedSchema,
    pub(super) options: &'a RenderOptions,
}

#[cfg(test)]
mod tests;
