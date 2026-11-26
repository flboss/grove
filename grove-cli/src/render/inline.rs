use grove_exec::value::Val;

use crate::render::RenderCtx;
use crate::render::text::{Styled, display_width, truncate_styled};
use crate::render::theme::{
    ANSI_BOLD, ANSI_DIM, ANSI_FG_BLUE, RECORD_ABBREVIATION, TRIPLE_DOTS, paint, scalar_style,
};

pub(super) fn compact(ctx: &RenderCtx<'_>, value: &Val, depth: usize) -> Styled {
    if ctx.options.max_depth > 0
        && depth >= ctx.options.max_depth
        && matches!(value, Val::Record(_) | Val::List(_) | Val::Tuple(_))
    {
        return Styled::painted(TRIPLE_DOTS, ANSI_DIM, ctx.options.color);
    }

    match value {
        Val::None => Styled::painted("none", ANSI_DIM, ctx.options.color),
        Val::Some(inner) => {
            let inner = compact(ctx, inner, depth);
            Styled {
                styled: format!("some({})", inner.styled),
                width: 5 + inner.width + 1,
            }
        }
        Val::Scalar(scalar) => {
            let text = scalar.to_string();
            Styled::painted(text, scalar_style(scalar), ctx.options.color)
        }
        Val::Enum(id, variant) => {
            let text = ctx
                .schema
                .enums
                .get(id.index())
                .and_then(|enum_def| {
                    enum_def
                        .variants
                        .get(*variant)
                        .map(|variant_def| format!("{}::{}", enum_def.name, variant_def.name))
                })
                .unwrap_or_else(|| format!("enum({}:{})", id.index(), variant));
            Styled::painted(text, ANSI_FG_BLUE, ctx.options.color)
        }
        Val::List(items) => {
            let mut styled = String::from("[");
            let mut width = 1;
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    styled.push_str(", ");
                    width += 2;
                }
                let item = compact(ctx, item, depth + 1);
                styled.push_str(&item.styled);
                width += item.width;
            }
            styled.push(']');
            width += 1;
            Styled { styled, width }
        }
        Val::Tuple(items) => {
            let mut styled = String::from("(");
            let mut width = 1;
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    styled.push_str(", ");
                    width += 2;
                }
                let item = compact(ctx, item, depth + 1);
                styled.push_str(&item.styled);
                width += item.width;
            }
            styled.push(')');
            width += 1;
            Styled { styled, width }
        }
        Val::Record(fields) => {
            let mut styled = String::from("{");
            let mut width = 1;
            for (index, (name, value)) in fields.iter().enumerate() {
                if index > 0 {
                    styled.push_str(", ");
                    width += 2;
                }
                styled.push_str(&paint(name.as_ref(), Some(ANSI_BOLD), ctx.options.color));
                width += display_width(name);
                styled.push_str(": ");
                width += 2;
                let value = compact(ctx, value, depth + 1);
                styled.push_str(&value.styled);
                width += value.width;
            }
            styled.push('}');
            width += 1;
            Styled { styled, width }
        }
        Val::Ref { .. } => {
            let text = value.formatter(ctx.schema).to_string();
            Styled::painted(text, ANSI_DIM, ctx.options.color)
        }
        Val::RowRef(_) => {
            let text = value.formatter(ctx.schema).to_string();
            Styled::painted(text, ANSI_DIM, ctx.options.color)
        }
    }
}

pub(super) fn cell_styled(ctx: &RenderCtx<'_>, value: &Val, depth: usize) -> Styled {
    let compact = compact(ctx, value, depth);
    let cap = ctx.options.max_cell_width;
    if cap == 0 || compact.width <= cap {
        return compact;
    }

    let abbreviated = match value {
        Val::List(items) if !items.is_empty() => Styled::painted(
            format!("[{} items]", items.len()),
            ANSI_DIM,
            ctx.options.color,
        ),
        Val::Record(fields) if !fields.is_empty() => {
            Styled::painted(RECORD_ABBREVIATION, ANSI_DIM, ctx.options.color)
        }
        _ => return truncate_styled(compact, cap),
    };

    if abbreviated.width <= cap {
        abbreviated
    } else {
        truncate_styled(abbreviated, cap)
    }
}
