use std::borrow::Cow;
use std::rc::Rc;

use grove_exec::value::{Scalar, Val};
use grove_query::{QueryType, RecordSource};
use grove_schema::ValidatedSchema;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::args::{Format, OutputArgs};
use crate::error::{CliError, CliResult};

const ANSI_RESET: &str = "\x1b[0m";
const ANSI_BOLD: &str = "1";
const ANSI_DIM: &str = "2";
const ANSI_FG_GREEN: &str = "32";
const ANSI_FG_YELLOW: &str = "33";
const ANSI_FG_BLUE: &str = "34";
const ANSI_FG_MAGENTA: &str = "35";
const ANSI_FG_CYAN: &str = "36";

const TRIPLE_DOTS: &str = "…";
const BOX_HORIZONTAL: &str = "─";
const BOX_VERTICAL: &str = "│";
const BOX_TOP_LEFT: &str = "┌";
const BOX_TOP_CENTER: &str = "┬";
const BOX_TOP_RIGHT: &str = "┐";
const BOX_MIDDLE_LEFT: &str = "├";
const BOX_MIDDLE_CENTER: &str = "┼";
const BOX_MIDDLE_RIGHT: &str = "┤";
const BOX_BOTTOM_LEFT: &str = "└";
const BOX_BOTTOM_CENTER: &str = "┴";
const BOX_BOTTOM_RIGHT: &str = "┘";

const MIDDLE_DOT: &str = "·";
const ARC_TOP: &str = "╭";
const ARC_BOTTOM: &str = "╰";
const VERTICAL_DASHED: &str = "┊";

const BRANCH_LAST_NESTED: &str = "└─┐";
const BRANCH_MIDDLE_NESTED: &str = "├─┐";
const BRANCH_LAST: &str = "└─";
const BRANCH_MIDDLE: &str = "├─";
const RECORD_ABBREVIATION: &str = "{…}";

pub(crate) struct RenderOptions {
    pub(crate) format: Format,
    pub(crate) color: bool,
    pub(crate) width: usize,
    pub(crate) max_rows: usize,
    pub(crate) max_depth: usize,
    pub(crate) max_cell_width: usize,
    pub(crate) headers: bool,
    pub(crate) hide_refs: bool,
}

impl RenderOptions {
    pub(crate) fn from_args(args: &OutputArgs, color: bool) -> Self {
        let width = args
            .width
            .or_else(terminal_width)
            .or_else(columns_env)
            .unwrap_or(80)
            .max(20);
        let (max_depth, max_cell_width) = if args.full {
            (0, 0)
        } else {
            (args.max_depth, args.max_cell_width)
        };

        RenderOptions {
            format: args.format,
            color,
            width,
            max_rows: args.max_rows,
            max_depth,
            max_cell_width,
            headers: !args.no_headers,
            hide_refs: args.hide_refs,
        }
    }
}

fn columns_env() -> Option<usize> {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|value| value.parse().ok())
}

#[cfg(unix)]
fn terminal_width() -> Option<usize> {
    use std::os::fd::AsRawFd;

    // SAFETY: winsize is plain data that ioctl initializes on success
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: the stdout descriptor is valid for the call and ioctl writes only size
    let result = unsafe { libc::ioctl(std::io::stdout().as_raw_fd(), libc::TIOCGWINSZ, &mut size) };
    (result == 0 && size.ws_col > 0).then_some(size.ws_col as usize)
}

#[cfg(not(unix))]
fn terminal_width() -> Option<usize> {
    None
}

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

fn render_auto(ctx: &RenderCtx<'_>, value: &Val, ty: Option<&QueryType>) -> CliResult<String> {
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

struct RenderCtx<'a> {
    schema: &'a ValidatedSchema,
    options: &'a RenderOptions,
}

#[derive(Clone, Default)]
struct Styled {
    styled: String,
    width: usize,
}

impl Styled {
    fn painted<'a>(text: impl Into<Cow<'a, str>>, style: &str, color: bool) -> Self {
        let text = text.into();
        let width = display_width(&text);
        let styled = paint(text, Some(style), color).into_owned();
        Styled { styled, width }
    }
}

fn compact(ctx: &RenderCtx<'_>, value: &Val, depth: usize) -> Styled {
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

fn block_lines(ctx: &RenderCtx<'_>, value: &Val, depth: usize) -> Vec<String> {
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

fn cell_styled(ctx: &RenderCtx<'_>, value: &Val, depth: usize) -> Styled {
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

fn expandable(value: &Val) -> bool {
    match value {
        Val::Record(fields) => !fields.is_empty(),
        Val::Tuple(items) => !items.is_empty(),
        Val::List(items) => !items.is_empty(),
        Val::Some(inner) => expandable(inner),
        _ => false,
    }
}

fn can_expand(ctx: &RenderCtx<'_>, depth: usize) -> bool {
    ctx.options.max_depth == 0 || depth < ctx.options.max_depth
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

#[derive(Clone, Copy)]
enum Align {
    Left,
    Right,
}

struct TableCell {
    content: Styled,
    align: Align,
}

impl TableCell {
    fn empty() -> Self {
        TableCell {
            content: Styled::default(),
            align: Align::Left,
        }
    }
}

struct TableData {
    headers: Vec<String>,
    rows: Vec<Vec<TableCell>>,
    widths: Vec<usize>,
    note: Option<String>,
}

enum TableError {
    NotTabular,
    TooWide,
}

fn build_table(
    ctx: &RenderCtx<'_>,
    value: &Val,
    ty: Option<&QueryType>,
) -> Result<TableData, TableError> {
    let Val::List(items) = value else {
        return Err(TableError::NotTabular);
    };

    if items.is_empty() {
        return empty_table(ctx, ty);
    }

    match &items[0] {
        Val::Record(fields) => {
            let headers: Vec<String> = fields.iter().map(|(name, _)| name.to_string()).collect();
            let (shown, note) = cap_items(items, ctx.options.max_rows, "row");
            let mut rows = Vec::with_capacity(shown.len());
            for item in shown {
                let Val::Record(fields) = item else {
                    return Err(TableError::NotTabular);
                };
                rows.push(
                    headers
                        .iter()
                        .map(|header| {
                            fields
                                .iter()
                                .find(|(name, _)| name.as_ref() == header)
                                .map(|(_, value)| table_cell(ctx, value))
                                .unwrap_or_else(TableCell::empty)
                        })
                        .collect(),
                );
            }
            finish_table(ctx, headers, rows, note)
        }
        Val::Tuple(first_items) => {
            let headers: Vec<String> = (0..first_items.len())
                .map(|index| format!(".{index}"))
                .collect();
            let (shown, note) = cap_items(items, ctx.options.max_rows, "row");
            let mut rows = Vec::with_capacity(shown.len());
            for item in shown {
                let Val::Tuple(values) = item else {
                    return Err(TableError::NotTabular);
                };
                rows.push(
                    (0..headers.len())
                        .into_iter()
                        .map(|index| {
                            values
                                .get(index)
                                .map(|value| table_cell(ctx, value))
                                .unwrap_or_else(TableCell::empty)
                        })
                        .collect(),
                );
            }
            finish_table(ctx, headers, rows, note)
        }
        _ => Err(TableError::NotTabular),
    }
}

fn empty_table(ctx: &RenderCtx<'_>, ty: Option<&QueryType>) -> Result<TableData, TableError> {
    let Some(QueryType::List(inner)) = ty else {
        return Err(TableError::NotTabular);
    };

    let headers = match inner.as_ref() {
        QueryType::Record(source) => {
            record_field_names(ctx.schema, source).ok_or(TableError::NotTabular)?
        }
        QueryType::Tuple(items) => (0..items.len()).map(|index| format!(".{index}")).collect(),
        _ => return Err(TableError::NotTabular),
    };

    finish_table(ctx, headers, Vec::new(), None)
}

fn finish_table(
    ctx: &RenderCtx<'_>,
    headers: Vec<String>,
    rows: Vec<Vec<TableCell>>,
    note: Option<String>,
) -> Result<TableData, TableError> {
    if headers.is_empty() {
        return Err(TableError::NotTabular);
    }
    if headers.len() > 12 {
        return Err(TableError::TooWide);
    }

    let headers: Vec<String> = headers
        .into_iter()
        .map(|header| truncate_display(header, ctx.options.max_cell_width).into_owned())
        .collect();
    let mut widths: Vec<usize> = headers.iter().map(|header| display_width(header)).collect();
    for row in &rows {
        for (index, cell) in row.iter().enumerate() {
            if let Some(width) = widths.get_mut(index) {
                *width = (*width).max(cell.content.width);
            }
        }
    }

    let total = widths.iter().sum::<usize>() + headers.len() * 3 + 1;
    if total > ctx.options.width {
        return Err(TableError::TooWide);
    }

    Ok(TableData {
        headers,
        rows,
        widths,
        note,
    })
}

fn table_cell(ctx: &RenderCtx<'_>, value: &Val) -> TableCell {
    TableCell {
        content: cell_styled(ctx, value, 0),
        align: if is_numeric(value) {
            Align::Right
        } else {
            Align::Left
        },
    }
}

fn render_table(ctx: &RenderCtx<'_>, data: TableData) -> String {
    let mut lines = vec![border_line(
        BOX_TOP_LEFT,
        BOX_TOP_CENTER,
        BOX_TOP_RIGHT,
        BOX_HORIZONTAL,
        &data.widths,
        ctx.options.color,
    )];

    if ctx.options.headers {
        let header_cells: Vec<TableCell> = data
            .headers
            .into_iter()
            .map(|header| TableCell {
                content: Styled::painted(header, ANSI_BOLD, ctx.options.color),
                align: Align::Left,
            })
            .collect();
        lines.push(table_row(
            &header_cells,
            BOX_VERTICAL,
            &data.widths,
            ctx.options.color,
        ));
        lines.push(border_line(
            BOX_MIDDLE_LEFT,
            BOX_MIDDLE_CENTER,
            BOX_MIDDLE_RIGHT,
            BOX_HORIZONTAL,
            &data.widths,
            ctx.options.color,
        ));
    }

    for row in &data.rows {
        lines.push(table_row(
            row,
            BOX_VERTICAL,
            &data.widths,
            ctx.options.color,
        ));
    }
    if let Some(note) = &data.note {
        lines.push(note_row(note, &data.widths, ctx.options.color));
    }
    lines.push(border_line(
        BOX_BOTTOM_LEFT,
        BOX_BOTTOM_CENTER,
        BOX_BOTTOM_RIGHT,
        BOX_HORIZONTAL,
        &data.widths,
        ctx.options.color,
    ));

    lines.join("\n")
}

fn border_line(
    left: &str,
    middle: &str,
    right: &str,
    connector: &str,
    widths: &[usize],
    color: bool,
) -> String {
    let mut line = String::with_capacity(widths.iter().sum::<usize>() + widths.len() * 3 + 1);
    line.push_str(left);
    for (index, width) in widths.iter().enumerate() {
        if index > 0 {
            line.push_str(middle);
        }
        for _ in 0..width + 2 {
            line.push_str(connector);
        }
    }
    line.push_str(right);
    paint(line, Some(ANSI_DIM), color).into_owned()
}

fn table_row(cells: &[TableCell], separator: &str, widths: &[usize], color: bool) -> String {
    let border = paint(separator, Some(ANSI_DIM), color);
    let empty = Styled::default();
    let mut row = border.clone().into_owned();
    for (index, width) in widths.iter().enumerate() {
        let cell = cells.get(index);
        let content = cell.map(|cell| &cell.content).unwrap_or(&empty);
        let align = cell.map(|cell| cell.align).unwrap_or(Align::Left);
        row.push(' ');
        row.push_str(&pad_styled(content, *width, align));
        row.push(' ');
        row.push_str(&border);
    }
    row
}

fn note_row(note: &str, widths: &[usize], color: bool) -> String {
    let total = widths.iter().sum::<usize>() + widths.len() * 3 + 1;
    let text_area = total.saturating_sub(4);
    let text = if text_area == 0 {
        "".into()
    } else {
        truncate_display(note, text_area)
    };
    let padding = text_area.saturating_sub(display_width(&text));
    let border = paint(BOX_VERTICAL, Some(ANSI_DIM), color);
    format!(
        "{border} {}{} {border}",
        paint(text, Some(ANSI_DIM), color),
        " ".repeat(padding)
    )
}

fn pad_styled(content: &Styled, width: usize, align: Align) -> String {
    let padding = width.saturating_sub(content.width);
    let mut out = String::with_capacity(padding + content.styled.len());
    match align {
        Align::Left => {
            out.push_str(&content.styled);
            out.extend(std::iter::repeat_n(' ', padding));
        }
        Align::Right => {
            out.extend(std::iter::repeat_n(' ', padding));
            out.push_str(&content.styled);
        }
    }
    out
}

fn pad<'a>(text: impl Into<Cow<'a, str>>, width: usize, align: Align) -> String {
    let text = text.into();
    let padding = width.saturating_sub(display_width(&text));
    match align {
        Align::Left => {
            let mut text = text.into_owned();
            text.extend(std::iter::repeat_n(' ', padding));
            text
        }
        Align::Right => {
            let mut spaces = String::with_capacity(padding + text.len());
            spaces.extend(std::iter::repeat_n(' ', padding));
            spaces.push_str(&text);
            spaces
        }
    }
}

fn cap_items<'a>(items: &'a [Val], max_rows: usize, noun: &str) -> (&'a [Val], Option<String>) {
    if max_rows > 0 && items.len() > max_rows {
        let remaining = items.len() - max_rows;
        let plural = if remaining == 1 { "" } else { "s" };
        (
            &items[..max_rows],
            Some(format!("{TRIPLE_DOTS} {remaining} more {noun}{plural}")),
        )
    } else {
        (items, None)
    }
}

fn record_field_names(schema: &ValidatedSchema, source: &RecordSource) -> Option<Vec<String>> {
    match source {
        RecordSource::Projection(fields) => {
            Some(fields.iter().map(|field| field.name.clone()).collect())
        }
        RecordSource::Schema(id) => {
            let struct_ = schema.structs.get(id.index())?;
            Some(
                struct_
                    .fields
                    .iter()
                    .map(|field| field.name().to_string())
                    .collect(),
            )
        }
    }
}

fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

fn truncate_display<'a>(text: impl Into<Cow<'a, str>>, max_width: usize) -> Cow<'a, str> {
    let text = text.into();
    if max_width == 0 || display_width(&text) <= max_width {
        return text;
    }
    if max_width == 1 {
        return Cow::Borrowed(TRIPLE_DOTS);
    }

    let mut out = String::new();
    let mut width = 0;
    for ch in text.chars() {
        let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + char_width > max_width - 1 {
            break;
        }
        out.push(ch);
        width += char_width;
    }
    out.push_str(TRIPLE_DOTS);
    Cow::Owned(out)
}

fn truncate_styled(styled: Styled, max_width: usize) -> Styled {
    if max_width == 0 || styled.width <= max_width {
        return styled;
    }

    let has_ansi = styled.styled.contains('\x1b');
    if max_width == 1 {
        return Styled {
            styled: if has_ansi {
                format!("{TRIPLE_DOTS}{ANSI_RESET}")
            } else {
                TRIPLE_DOTS.to_string()
            },
            width: 1,
        };
    }

    let mut out = String::new();
    let mut width = 0;
    let mut index = 0;
    let bytes = styled.styled.as_bytes();
    while index < bytes.len() {
        if bytes[index] == 0x1b && bytes.get(index + 1) == Some(&b'[') {
            let start = index;
            index += 2;
            while index < bytes.len() && bytes[index - 1] != b'm' {
                index += 1;
            }
            out.push_str(&styled.styled[start..index]);
            continue;
        }

        let ch = styled.styled[index..].chars().next().expect("index < len");
        let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + char_width > max_width - 1 {
            break;
        }
        out.push(ch);
        width += char_width;
        index += ch.len_utf8();
    }

    out.push_str(TRIPLE_DOTS);
    width += 1;
    if has_ansi {
        out.push_str(ANSI_RESET);
    }
    Styled { styled: out, width }
}

pub(crate) fn paint<'a>(
    text: impl Into<Cow<'a, str>>,
    style: Option<&str>,
    color: bool,
) -> Cow<'a, str> {
    let text = text.into();
    match (color, style) {
        (true, Some(style)) => Cow::Owned(format!("\x1b[{style}m{text}{ANSI_RESET}")),
        _ => text,
    }
}

fn scalar_style(scalar: &Scalar) -> &'static str {
    match scalar {
        Scalar::Int(_) | Scalar::Float(_) | Scalar::Dec(_) => ANSI_FG_CYAN,
        Scalar::String(_) => ANSI_FG_GREEN,
        Scalar::Bool(_) => ANSI_FG_YELLOW,
        Scalar::Bytes(_) | Scalar::Instant(_) | Scalar::Duration(_) => ANSI_FG_MAGENTA,
    }
}

fn is_numeric(value: &Val) -> bool {
    match value {
        Val::Scalar(Scalar::Int(_) | Scalar::Float(_) | Scalar::Dec(_) | Scalar::Duration(_)) => {
            true
        }
        Val::Some(inner) => is_numeric(inner),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use grove_exec::value::Key;
    use grove_schema::validated::{FieldId, ScalarType, StructId};

    fn schema() -> ValidatedSchema {
        let source = r#"
            root users: User;
            struct User {
                name: String,
                age: Int,
                active: Bool,
            }
            enum Role { Admin => "admin", User => "user" }
        "#;
        let (schema, diagnostics) = grove_schema::parse_schema(source);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let (schema, diagnostics) = grove_schema::validate(schema.unwrap());
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        schema.unwrap()
    }

    fn options(format: Format) -> RenderOptions {
        RenderOptions {
            format,
            color: false,
            width: 80,
            max_rows: 0,
            max_depth: 6,
            max_cell_width: 40,
            headers: true,
            hide_refs: false,
        }
    }

    fn int(value: i64) -> Val {
        Val::Scalar(Scalar::Int(value))
    }

    fn string(value: &str) -> Val {
        Val::Scalar(Scalar::String(value.to_string()))
    }

    fn user(name: &str, age: i64, active: bool) -> Val {
        Val::record(vec![
            (Rc::from("name"), string(name)),
            (Rc::from("age"), int(age)),
            (Rc::from("active"), Val::Scalar(Scalar::Bool(active))),
        ])
    }

    fn record_ty() -> QueryType {
        QueryType::Record(RecordSource::Schema(StructId::new(0)))
    }

    fn list_record_ty() -> QueryType {
        QueryType::List(Box::new(record_ty()))
    }

    fn render_value(mut value: Val, ty: &QueryType, options: &RenderOptions) -> String {
        render(&mut value, Some(ty), &schema(), options).expect("render value")
    }

    #[test]
    fn scalar_and_optional_auto_inline() {
        let int_ty = QueryType::Scalar(ScalarType::Int);
        let optional_int = QueryType::Optional(Box::new(int_ty.clone()));

        assert_eq!(render_value(int(42), &int_ty, &options(Format::Auto)), "42");
        assert_eq!(
            render_value(Val::None, &optional_int, &options(Format::Auto)),
            "none"
        );
        assert_eq!(
            render_value(
                Val::Some(Box::new(int(3))),
                &optional_int,
                &options(Format::Auto)
            ),
            "some(3)"
        );
    }

    #[test]
    fn scalar_list_auto_inline() {
        let value = Val::list(vec![int(1), int(2), int(3)]);
        let ty = QueryType::List(Box::new(QueryType::Scalar(ScalarType::Int)));
        assert_eq!(
            render_value(value, &ty, &options(Format::Auto)),
            "[1, 2, 3]"
        );
    }

    #[test]
    fn tuple_auto_inline() {
        let value = Val::Tuple(vec![int(1), string("a")]);
        let ty = QueryType::Tuple(vec![
            QueryType::Scalar(ScalarType::Int),
            QueryType::Scalar(ScalarType::String),
        ]);
        assert_eq!(
            render_value(value, &ty, &options(Format::Auto)),
            "(1, \"a\")"
        );
    }

    #[test]
    fn record_auto_vertical() {
        let value = user("Alice", 30, true);
        let output = render_value(value, &record_ty(), &options(Format::Auto));
        assert_eq!(output, "╭ name  : \"Alice\"\n┊ age   : 30\n╰ active: true");
    }

    #[test]
    fn single_field_record_dot() {
        let value = Val::record(vec![(Rc::from("name"), string("Alice"))]);
        let output = render_value(value, &record_ty(), &options(Format::Vertical));
        assert_eq!(output, "· name: \"Alice\"");
    }

    #[test]
    fn tuple_vertical_rails() {
        let value = Val::Tuple(vec![int(1), string("a"), Val::Scalar(Scalar::Bool(true))]);
        let ty = QueryType::Tuple(vec![
            QueryType::Scalar(ScalarType::Int),
            QueryType::Scalar(ScalarType::String),
            QueryType::Scalar(ScalarType::Bool),
        ]);
        let output = render_value(value, &ty, &options(Format::Vertical));
        assert_eq!(output, "╭ .0: 1\n┊ .1: \"a\"\n╰ .2: true");
    }

    #[test]
    fn some_record_vertical() {
        let value = Val::Some(Box::new(user("Alice", 30, true)));
        let output = render_value(value, &record_ty(), &options(Format::Vertical));
        assert_eq!(
            output,
            "some\n  ╭ name  : \"Alice\"\n  ┊ age   : 30\n  ╰ active: true"
        );
    }

    #[test]
    fn record_field_some_vertical() {
        let value = Val::record(vec![(
            Rc::from("profile"),
            Val::Some(Box::new(user("Alice", 30, true))),
        )]);
        let output = render_value(value, &record_ty(), &options(Format::Vertical));
        assert_eq!(
            output,
            "· profile: some\n    ╭ name  : \"Alice\"\n    ┊ age   : 30\n    ╰ active: true"
        );
    }

    #[test]
    fn some_scalar_compact_vertical() {
        let value = Val::Some(Box::new(int(3)));
        let output = render_value(value, &record_ty(), &options(Format::Vertical));
        assert_eq!(output, "some(3)");
    }

    #[test]
    fn record_nested_vertical_rails() {
        let value = Val::list(vec![user("Alice", 30, true), user("Bob", 9, false)]);
        let output = render_value(value, &list_record_ty(), &options(Format::Vertical));
        assert!(output.contains("├─ ╭ name"), "{output}");
        assert!(output.contains("│  ┊ age"), "{output}");
        assert!(output.contains("│  ╰ active"), "{output}");
        assert!(output.contains("└─ ╭ name"), "{output}");
        assert!(output.contains("   ┊ age"), "{output}");
        assert!(output.contains("   ╰ active"), "{output}");
    }

    #[test]
    fn record_list_auto_table() {
        let value = Val::list(vec![user("Alice", 30, true), user("Bob", 9, false)]);
        let output = render_value(value, &list_record_ty(), &options(Format::Auto));
        assert!(output.contains("┌"), "{output}");
        assert!(output.contains("name"), "{output}");
        assert!(output.contains("Alice"), "{output}");
        assert!(output.contains("Bob"), "{output}");
        assert!(!output.contains(":"), "{output}");
    }

    #[test]
    fn empty_record_list_type_headers() {
        let output = render_value(
            Val::list(Vec::new()),
            &list_record_ty(),
            &options(Format::Auto),
        );
        assert!(output.contains("┌"), "{output}");
        assert!(output.contains("name"), "{output}");
        assert!(output.contains("age"), "{output}");
    }

    #[test]
    fn wide_table_auto_vertical_fallback() {
        let value = Val::list(vec![user("Alice", 30, true), user("Bob", 9, false)]);
        let mut options = options(Format::Auto);
        options.width = 20;
        let output = render_value(value, &list_record_ty(), &options);
        assert!(!output.contains("┌"), "{output}");
        assert!(output.contains("name  : \"Alice\""), "{output}");
    }

    #[test]
    fn table_row_cap() {
        let value = Val::list(vec![
            user("Alice", 30, true),
            user("Bob", 9, false),
            user("Cara", 41, true),
        ]);
        let mut options = options(Format::Auto);
        options.max_rows = 2;
        let output = render_value(value, &list_record_ty(), &options);
        assert!(output.contains("1 more row"), "{output}");
        assert!(!output.contains("Cara"), "{output}");
        let note_line = output
            .lines()
            .find(|line| line.contains("1 more row"))
            .expect("note line");
        assert!(note_line.starts_with('│'), "{note_line}");
        assert!(note_line.ends_with('│'), "{note_line}");
    }

    #[test]
    fn scalar_list_row_cap() {
        let value = Val::list(vec![int(1), int(2), int(3)]);
        let ty = QueryType::List(Box::new(QueryType::Scalar(ScalarType::Int)));
        let mut options = options(Format::Auto);
        options.max_rows = 2;
        let output = render_value(value, &ty, &options);
        assert!(output.contains("├─ 1"), "{output}");
        assert!(!output.contains("├─ 3"), "{output}");
        assert!(output.contains("└─ … 1 more item"), "{output}");
    }

    #[test]
    fn table_format_non_tabular_rejected() {
        let int_ty = QueryType::Scalar(ScalarType::Int);
        assert!(
            render(
                &mut int(1),
                Some(&int_ty),
                &schema(),
                &options(Format::Table)
            )
            .is_err()
        );
    }

    #[test]
    fn color_style_scalar() {
        let int_ty = QueryType::Scalar(ScalarType::Int);
        let mut options = options(Format::Auto);
        options.color = true;
        let output = render_value(int(7), &int_ty, &options);
        assert!(output.contains("\x1b[36m7\x1b[0m"), "{output}");
    }

    #[test]
    fn table_cells_keep_part_styles() {
        let value = Val::list(vec![Val::Tuple(vec![Val::Some(Box::new(int(3)))])]);
        let ty = QueryType::List(Box::new(QueryType::Tuple(vec![QueryType::Optional(
            Box::new(QueryType::Scalar(ScalarType::Int)),
        )])));
        let mut options = options(Format::Table);
        options.color = true;
        let output = render_value(value, &ty, &options);
        assert!(output.contains("some(\x1b[36m3\x1b[0m)"), "{output}");
    }

    #[test]
    fn hide_refs_omits_fields_and_list_items() {
        let schema = schema();
        let reference = Val::Ref {
            field: FieldId::new(StructId::new(0), 0),
            parent: Key(7),
        };
        let mut record = Val::record(vec![
            (Rc::from("name"), string("Alice")),
            (Rc::from("link"), reference.clone()),
        ]);
        let mut opts = options(Format::Vertical);
        opts.hide_refs = true;
        let output = render(&mut record, None, &schema, &opts).unwrap();
        assert!(output.contains("name"), "{output}");
        assert!(!output.contains("link"), "{output}");
        assert!(!output.contains("ref("), "{output}");

        let mut list = Val::list(vec![reference, int(1)]);
        let mut opts = options(Format::Auto);
        opts.hide_refs = true;
        let output = render(&mut list, None, &schema, &opts).unwrap();
        assert_eq!(output, "[1]");
    }

    #[test]
    fn hide_refs_removes_table_columns() {
        let schema = schema();
        let reference = Val::Ref {
            field: FieldId::new(StructId::new(0), 0),
            parent: Key(7),
        };
        let mut value = Val::list(vec![Val::record(vec![
            (Rc::from("name"), string("Alice")),
            (Rc::from("link"), reference),
        ])]);
        let mut opts = options(Format::Table);
        opts.hide_refs = true;
        let output = render(&mut value, None, &schema, &opts).unwrap();
        assert!(output.contains("name"), "{output}");
        assert!(!output.contains("link"), "{output}");
    }

    #[test]
    fn ref_fallback_without_schema_metadata() {
        let mut value = Val::Ref {
            field: FieldId::new(StructId::new(0), 0),
            parent: Key(7),
        };
        let output = render(&mut value, None, &schema(), &options(Format::Auto)).unwrap();
        assert!(output.contains("ref("), "{output}");
    }

    #[test]
    fn nested_list_auto_vertical_indentation() {
        let value = Val::list(vec![
            Val::list(vec![int(1), int(2)]),
            Val::list(vec![int(3)]),
        ]);
        let ty = QueryType::List(Box::new(QueryType::List(Box::new(QueryType::Scalar(
            ScalarType::Int,
        )))));
        let output = render_value(value, &ty, &options(Format::Vertical));
        assert!(output.contains("├─┐"), "{output}");
        assert!(output.contains("│ ├─ 1"), "{output}");
        assert!(output.contains("  └─ 3"), "{output}");
    }
}
