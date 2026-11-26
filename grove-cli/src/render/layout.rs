use grove_exec::value::Val;
use grove_query::QueryType;

use crate::error::CliResult;
use crate::render::RenderCtx;
use crate::render::inline::compact;
use crate::render::table::{build_table, render_table};
use crate::render::vertical::block_lines;

pub(super) fn render_auto(
    ctx: &RenderCtx<'_>,
    value: &Val,
    ty: Option<&QueryType>,
) -> CliResult<String> {
    match value {
        Val::List(items) => {
            if items.is_empty() {
                return match build_table(ctx, value, ty) {
                    Ok(data) => Ok(render_table(ctx, data)),
                    Err(_) => Ok("[]".to_string()),
                };
            }

            if is_record_list(items) || is_tuple_list(items) {
                return match build_table(ctx, value, ty) {
                    Ok(data) => Ok(render_table(ctx, data)),
                    Err(_) => Ok(block_lines(ctx, value, 0).join("\n")),
                };
            }

            let compact = compact(ctx, value, 0);
            let limit = if is_scalar_list(items) { 20 } else { 10 };
            let within_row_cap = ctx.options.max_rows == 0 || items.len() <= ctx.options.max_rows;
            if items.len() <= limit && within_row_cap && compact.width <= ctx.options.width {
                Ok(compact.styled)
            } else {
                Ok(block_lines(ctx, value, 0).join("\n"))
            }
        }
        Val::Record(_) => Ok(block_lines(ctx, value, 0).join("\n")),
        Val::Tuple(_) => {
            let compact = compact(ctx, value, 0);
            if compact.width <= ctx.options.width {
                Ok(compact.styled)
            } else {
                Ok(block_lines(ctx, value, 0).join("\n"))
            }
        }
        _ => Ok(compact(ctx, value, 0).styled),
    }
}

fn is_record_list(items: &[Val]) -> bool {
    items.iter().all(|item| matches!(item, Val::Record(_)))
}

fn is_tuple_list(items: &[Val]) -> bool {
    items.iter().all(|item| matches!(item, Val::Tuple(_)))
}

fn is_scalar_value(value: &Val) -> bool {
    match value {
        Val::Scalar(_) | Val::Enum(_, _) | Val::None => true,
        Val::Some(inner) => is_scalar_value(inner),
        _ => false,
    }
}

fn is_scalar_list(items: &[Val]) -> bool {
    items.iter().all(is_scalar_value)
}
