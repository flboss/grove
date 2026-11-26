use std::rc::Rc;

use grove_exec::value::Val;

use crate::render::RenderCtx;
use crate::render::inline::cell_styled;
use crate::render::text::{Align, cap_items, display_width, pad};
use crate::render::theme::{
    ANSI_BOLD, ANSI_DIM, ARC_BOTTOM, ARC_TOP, BOX_VERTICAL, BRANCH_LAST, BRANCH_LAST_NESTED,
    BRANCH_MIDDLE, BRANCH_MIDDLE_NESTED, MIDDLE_DOT, VERTICAL_DASHED, paint,
};

pub(super) fn block_lines(ctx: &RenderCtx<'_>, value: &Val, depth: usize) -> Vec<String> {
    if can_expand(ctx, depth) {
        match value {
            Val::Record(fields) => return record_lines(ctx, fields, depth),
            Val::Tuple(items) => return tuple_lines(ctx, items, depth),
            Val::List(items) => return list_lines(ctx, items, depth),
            Val::Some(inner) if expandable(inner) && can_expand(ctx, depth + 1) => {
                return some_lines(ctx, inner, depth);
            }
            _ => {}
        }
    }
    vec![cell_styled(ctx, value, depth).styled]
}

fn record_lines(ctx: &RenderCtx<'_>, fields: &[(Rc<str>, Val)], depth: usize) -> Vec<String> {
    if fields.is_empty() {
        return vec!["{}".to_string()];
    }
    let entries: Vec<(String, &Val)> = fields
        .iter()
        .map(|(name, value)| (name.to_string(), value))
        .collect();
    named_lines(ctx, &entries, depth)
}

fn tuple_lines(ctx: &RenderCtx<'_>, items: &[Val], depth: usize) -> Vec<String> {
    if items.is_empty() {
        return vec!["()".to_string()];
    }
    let entries: Vec<(String, &Val)> = items
        .iter()
        .enumerate()
        .map(|(index, value)| (format!(".{index}"), value))
        .collect();
    named_lines(ctx, &entries, depth)
}

fn some_lines(ctx: &RenderCtx<'_>, inner: &Val, depth: usize) -> Vec<String> {
    let mut lines = vec!["some".to_string()];
    for mut line in block_lines(ctx, inner, depth + 1) {
        line.insert_str(0, "  ");
        lines.push(line);
    }
    lines
}

fn named_lines(ctx: &RenderCtx<'_>, entries: &[(String, &Val)], depth: usize) -> Vec<String> {
    let key_width = entries
        .iter()
        .map(|(key, _)| display_width(key))
        .max()
        .unwrap_or(0);
    let mut lines = Vec::new();

    for (index, (key, value)) in entries.iter().enumerate() {
        let is_last = index + 1 == entries.len();
        let marker = if entries.len() == 1 {
            MIDDLE_DOT
        } else if index == 0 {
            ARC_TOP
        } else if is_last {
            ARC_BOTTOM
        } else {
            VERTICAL_DASHED
        };
        let mut line = paint(marker, Some(ANSI_DIM), ctx.options.color).into_owned();
        line.push(' ');
        let key = pad(key, key_width, Align::Left);
        line.push_str(&paint(key, Some(ANSI_BOLD), ctx.options.color));
        if let Val::Some(inner) = value
            && expandable(inner)
            && can_expand(ctx, depth + 1)
        {
            line.push_str(": some");
            lines.push(line);
            let prefix = if is_last { " " } else { VERTICAL_DASHED };
            let mut prefix = paint(prefix, Some(ANSI_DIM), ctx.options.color).into_owned();
            prefix.push_str("   ");
            for mut inner_line in block_lines(ctx, inner, depth + 1) {
                inner_line.insert_str(0, &prefix);
                lines.push(inner_line);
            }
        } else if expandable(value) && can_expand(ctx, depth + 1) {
            line.push(':');
            lines.push(line);
            let prefix = if is_last { " " } else { VERTICAL_DASHED };
            let mut prefix = paint(prefix, Some(ANSI_DIM), ctx.options.color).into_owned();
            prefix.push(' ');
            for mut inner_line in block_lines(ctx, value, depth + 1) {
                inner_line.insert_str(0, &prefix);
                lines.push(inner_line);
            }
        } else {
            line.push_str(": ");
            line.push_str(&cell_styled(ctx, value, depth + 1).styled);
            lines.push(line);
        }
    }

    lines
}

fn list_lines(ctx: &RenderCtx<'_>, items: &[Val], depth: usize) -> Vec<String> {
    if items.is_empty() {
        return vec!["[]".to_string()];
    }

    let (items, note) = cap_items(items, ctx.options.max_rows, "item");
    let mut lines = Vec::new();

    for (index, item) in items.iter().enumerate() {
        let is_last = index + 1 == items.len() && note.is_none();

        if matches!(item, Val::List(inner) if !inner.is_empty()) && can_expand(ctx, depth + 1) {
            let branch = if is_last {
                BRANCH_LAST_NESTED
            } else {
                BRANCH_MIDDLE_NESTED
            };
            lines.push(paint(branch, Some(ANSI_DIM), ctx.options.color).into_owned());
            let continuation = if is_last { " " } else { BOX_VERTICAL };
            let mut continuation =
                paint(continuation, Some(ANSI_DIM), ctx.options.color).into_owned();
            continuation.push(' ');
            for mut line in block_lines(ctx, item, depth + 1) {
                line.insert_str(0, &continuation);
                lines.push(line);
            }
            continue;
        }

        let mut item_lines = if expandable(item) {
            block_lines(ctx, item, depth + 1)
        } else {
            vec![cell_styled(ctx, item, depth + 1).styled]
        };

        for (line_index, mut line) in item_lines.drain(..).enumerate() {
            let prefix = if line_index == 0 {
                if is_last { BRANCH_LAST } else { BRANCH_MIDDLE }
            } else if is_last {
                " "
            } else {
                BOX_VERTICAL
            };
            let mut prefix = paint(prefix, Some(ANSI_DIM), ctx.options.color).into_owned();
            prefix.push(' ');
            if line_index > 0 {
                prefix.push(' ');
            }
            line.insert_str(0, &prefix);
            lines.push(line);
        }
    }

    if let Some(note) = note {
        let mut prefix = paint(BRANCH_LAST, Some(ANSI_DIM), ctx.options.color).into_owned();
        prefix.push(' ');
        prefix.push_str(&paint(note, Some(ANSI_DIM), ctx.options.color));
        lines.push(prefix);
    }

    lines
}

pub(super) fn expandable(value: &Val) -> bool {
    match value {
        Val::Record(fields) => !fields.is_empty(),
        Val::Tuple(items) => !items.is_empty(),
        Val::List(items) => !items.is_empty(),
        Val::Some(inner) => expandable(inner),
        _ => false,
    }
}

pub(super) fn can_expand(ctx: &RenderCtx<'_>, depth: usize) -> bool {
    ctx.options.max_depth == 0 || depth < ctx.options.max_depth
}
